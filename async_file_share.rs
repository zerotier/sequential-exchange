use std::{
    collections::HashMap,
    fs::{read_dir, File},
    io::{Read, Write},
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
    ConfirmFileSize { file: File },
    ConfirmDownload,
    FileDownload,
    ReadDir { download_missing: bool },
    DirContents,
}

#[derive(Serialize, Deserialize)]
enum Packet<'a> {
    RequestFile { filename: &'a str },
    ConfirmFileSize { filesize: u64 },
    ConfirmDownload { fileid: u64 },
    FileDownload { fileid: u64, file_chunk: &'a [u8] },
    FileDownloadComplete { fileid: u64 },
    ReadDir,
    DirContents { filenames: Vec<&'a str> },
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
    ($peer:expr, $trans:expr, $send_data:expr, $packet: expr) => {
        $peer.seqex.send_with($trans, |seq_no| ($send_data, create_payload(seq_no, &$packet)))
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
    downloads_in_progress: HashMap<u64, File>,
    home_dir: PathBuf,
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

fn process(peer: &Arc<Peer>, transport: &Transport, guard: ReplyGuard<'_>, payload: Packet<'_>, data: Option<SendData>) -> Option<()> {
    match (payload, data) {
        (Packet::RequestFile { filename }, None) => {
            let file = File::open(peer.home_dir.join(filename)).ok()?;
            let metadata = file.metadata().ok()?;
            let filesize = metadata.len();
            reply!(
                guard,
                SendData::ConfirmFileSize { file },
                Packet::ConfirmFileSize { filesize }
            );
        }
        (Packet::ConfirmFileSize { filesize }, Some(SendData::RequestFile { filename })) => {
            let path = peer.home_dir.join(filename);
            if filesize > DOWNLOAD_LIMIT {
                return None;
            }
            let file = File::create(&path).ok()?;
            let fileid = OsRng.next_u64();
            peer.downloads_in_progress.insert(fileid, file);
            reply!(guard, SendData::ConfirmDownload, Packet::ConfirmDownload { fileid });
        }
        (Packet::ConfirmDownload { fileid }, Some(SendData::ConfirmFileSize { mut file })) => {
            let peer = peer.clone();
            let transport = transport.clone();
            thread::spawn(move || {
                const BUFFERED_CHUNKS: usize = 10;
                let mut buffer = [0u8; BUFFERED_CHUNKS * FILE_CHUNK_SIZE];
                while let Ok(n) = file.read(&mut buffer) {
                    if n == 0 {
                        break;
                    }
                    let mut i = 0;
                    while i < n {
                        let j = n.min(i + FILE_CHUNK_SIZE);
                        send!(
                            peer,
                            &transport,
                            SendData::FileDownload,
                            Packet::FileDownload { fileid, file_chunk: &buffer[i..j] }
                        );
                        i = j;
                    }
                }
                send!(
                    peer,
                    &transport,
                    SendData::FileDownload,
                    Packet::FileDownloadComplete { fileid }
                );
            });
        }
        (Packet::FileDownload { fileid, file_chunk }, None) => {
            let mut file = peer.downloads_in_progress.get(&fileid)?;
            let result = file.write_all(file_chunk);
        }
        (Packet::FileDownloadComplete { fileid }, None) => {

        }
        (Packet::ReadDir, None) => {
            let dir = read_dir(&peer.home_dir).ok()?;
            let mut filenames = Vec::new();
            for entry in dir {
                if let Ok(entry) = entry {
                    if entry.file_type().map_or(false, |f| f.is_file()) {
                        if let Ok(filename) = entry.file_name().into_string() {
                            filenames.push(filename)
                        }
                    }
                } else {
                    return None;
                }
            }
            let filenames: Vec<&str> = filenames.iter().map(|f| f.as_str()).collect();
            reply!(guard, SendData::DirContents, Packet::DirContents { filenames });
        }
        (Packet::DirContents { filenames }, Some(SendData::ReadDir { download_missing: true })) => {
            drop(guard);
            for filename in filenames {}
        }
        _ => {}
    }
    Some(())
}

fn receive(peer: &Arc<Peer>, transport: &Transport, receiver: &Receiver<Vec<u8>>) -> Option<()> {
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
                peer.seqex.receive_all(transport, seq_no, None, packet)
            }
            PACKET_TYPE_REPLY => {
                let seq_no = SeqNo::from_be_bytes(packet.get(1..5)?.try_into().ok()?);
                let reply_no = SeqNo::from_be_bytes(packet.get(5..9)?.try_into().ok()?);
                peer.seqex.receive_all(transport, seq_no, Some(reply_no), packet)
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
                process(peer, transport, guard, parsed_packet, send_data.map(|d| d.0));
            }
        }
    }
    Some(())
}

fn main() {
    let alice_root = std::path::Path::new("examples").join("alice_home");
    let bob_root = std::path::Path::new("examples").join("bob_home");
    let (s, r) = channel::<i32>();
    thread::spawn(move || {
        s.send(0);
    });
}

#[test]
fn test() {
    main()
}
