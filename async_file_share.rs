use std::{
    collections::HashMap,
    io::Read,
    ops::Deref,
    path::PathBuf,
    sync::{
        mpsc::{channel, Receiver, Sender},
        Arc, RwLock,
    },
    thread,
    time::{Duration, Instant},
};

use rand_core::{OsRng, RngCore};
use seq_ex::{sync::RecvSuccess, SeqNo, TransportLayer};
use serde::{Deserialize, Serialize};
use smol::fs::File;
use smol::prelude::*;

/// serde_cbor minimal format is both smaller and faster than default format.
/// The overhead is ~33% faster.
fn to_writer_minimal(value: &impl serde::Serialize, w: &mut impl serde_cbor::ser::Write) -> serde_cbor::Result<()> {
    value.serialize(&mut serde_cbor::Serializer::new(w).packed_format().legacy_enums())?;
    Ok(())
}

fn drop_packet() -> bool {
    OsRng.next_u32() >= (u32::MAX / 4 * 3)
}

const FILE_CHUNK_SIZE: usize = 1000;
const DOWNLOAD_LIMIT: u64 = 1000000;

#[derive(Debug)]
enum SendData {
    RequestFile { filename: String },
    ConfirmFileSize { filename: String, file: File },
    ConfirmDownload,
    FileDownload,
}

#[derive(Serialize, Deserialize)]
enum Packet<'a> {
    RequestFile { filename: &'a str },
    ConfirmFileSize { filesize: u64 },
    ConfirmDownload,
    FileDownload { filename: &'a str, file_chunk: &'a [u8] },
    FileDownloadComplete { filename: &'a str },
}

const PACKET_TYPE_PAYLOAD: u8 = 0;
const PACKET_TYPE_REPLY: u8 = 1;
const PACKET_TYPE_ACK: u8 = 2;

fn create_payload(seq_no: SeqNo, packet: &Packet<'_>) -> Vec<u8> {
    let mut p = vec![PACKET_TYPE_PAYLOAD];
    p.extend(&seq_no.to_be_bytes());
    to_writer_minimal(packet, &mut p);
    p
}
fn create_reply(seq_no: SeqNo, reply_no: SeqNo, packet: &Packet<'_>) -> Vec<u8> {
    let mut p = vec![PACKET_TYPE_REPLY];
    p.extend(&seq_no.to_be_bytes());
    p.extend(&reply_no.to_be_bytes());
    to_writer_minimal(packet, &mut p);
    p
}
macro_rules! reply {
    ($guard:expr, $send_data:expr, $packet: expr) => {
        $guard.reply_with(|seq_no, reply_no| ($send_data, create_reply(seq_no, reply_no, &$packet)))
    };
}
macro_rules! send {
    ($peer:expr, $send_data:expr, $packet: expr) => {
        $peer
            .seqex
            .send_with(&$peer.transport, |seq_no| ($send_data, create_payload(seq_no, &$packet)))
    };
}

type SeqEx = seq_ex::sync::SeqExSync<(SendData, Vec<u8>), Vec<u8>>;
type ReplyGuard<'a> = seq_ex::sync::ReplyGuard<'a, &'a Transport, (SendData, Vec<u8>), Vec<u8>>;

#[derive(Clone)]
struct Transport {
    sender: Sender<Vec<u8>>,
    time: Instant,
}

struct Peer {
    home_dir: PathBuf,
    transport: Transport,
    seqex: SeqEx,
}

impl TransportLayer<(SendData, Vec<u8>)> for &Transport {
    fn time(&mut self) -> i64 {
        self.time.elapsed().as_millis() as i64
    }

    fn send(&mut self, _: SeqNo, _: Option<SeqNo>, (_, packet): &(SendData, Vec<u8>)) {
        let _ = self.sender.send(packet.clone());
    }
    fn send_ack(&mut self, reply_no: SeqNo) {
        let mut p = vec![PACKET_TYPE_ACK];
        p.extend(&reply_no.to_be_bytes());
        self.sender.send(p);
    }
}

async fn process(peer: Arc<Peer>, guard: ReplyGuard<'_>, payload: Packet<'_>, data: Option<SendData>) -> Option<()> {
    match (payload, data) {
        (Packet::RequestFile { filename }, None) => {
            let file = File::open(peer.home_dir.join(filename)).await.ok()?;
            let metadata = file.metadata().await.ok()?;
            let filesize = metadata.len();
            reply!(
                guard,
                SendData::ConfirmFileSize { filename: filename.to_string(), file },
                Packet::ConfirmFileSize { filesize }
            );
        }
        (Packet::ConfirmFileSize { filesize }, Some(SendData::RequestFile { filename })) => {
            let path = peer.home_dir.join(filename);
            if filesize > DOWNLOAD_LIMIT {
                return None;
            }
            let file = File::open(&path).await;
            if file.is_ok() {
                return None;
            }
            reply!(guard, SendData::ConfirmDownload, Packet::ConfirmDownload);
        }
        (Packet::ConfirmDownload, Some(SendData::ConfirmFileSize { filename, mut file })) => {
            drop(guard);
            const BUFFERED_CHUNKS: usize = 10;
            let mut buffer = [0u8; BUFFERED_CHUNKS * FILE_CHUNK_SIZE];
            while let Ok(n) = file.read(&mut buffer).await {
                if n == 0 {
                    break;
                }
                let mut i = 0;
                while i < n {
                    let j = n.min(i + FILE_CHUNK_SIZE);
                    send!(
                        peer,
                        SendData::FileDownload,
                        Packet::FileDownload { filename: &filename, file_chunk: &buffer[i..j] }
                    );
                    i = j;
                }
            }
            send!(peer, SendData::FileDownload, Packet::FileDownloadComplete { filename: &filename });
        }
        _ => {}
    }
    Some(())
}

async fn receive(peer: &Arc<Peer>, receiver: &Receiver<Vec<u8>>) -> Option<()> {
    while let Ok(packet) = receiver.try_recv() {
        if drop_packet() {
            continue;
        }
        let iter = match *packet.get(0)? {
            PACKET_TYPE_ACK => {
                let reply_no = SeqNo::from_be_bytes(packet.get(1..5)?.try_into().ok()?);
                let _ = peer.seqex.receive_ack(reply_no);
                return None;
            }
            PACKET_TYPE_PAYLOAD => {
                let seq_no = SeqNo::from_be_bytes(packet.get(1..5)?.try_into().ok()?);
                peer.seqex.receive_all(&peer.transport, seq_no, None, packet)
            }
            PACKET_TYPE_REPLY => {
                let seq_no = SeqNo::from_be_bytes(packet.get(1..5)?.try_into().ok()?);
                let reply_no = SeqNo::from_be_bytes(packet.get(5..9)?.try_into().ok()?);
                peer.seqex.receive_all(&peer.transport, seq_no, Some(reply_no), packet)
            }
            _ => return None,
        };
        for RecvSuccess { guard, packet, send_data } in iter {
            let offset = match packet[0] {
                PACKET_TYPE_PAYLOAD => 5,
                PACKET_TYPE_REPLY => 9,
                _ => return None,
            };
            if let Ok(parsed_packet) = serde_cbor::from_slice::<Packet>(packet.get(offset..)?) {
                process(peer.clone(), guard, parsed_packet, send_data.map(|d| d.0)).await;
            }
        }
    }
    Some(())
}

fn main() {
    let alice_root = std::path::Path::new("examples").join("alice_home");
    let bob_root = std::path::Path::new("examples").join("bob_home");
}

#[test]
fn test() {
    main()
}
