use std::{
    collections::HashMap,
    ops::Deref,
    sync::{
        mpsc::{channel, Receiver, Sender},
        Arc, RwLock,
    },
    thread,
    time::{Duration, Instant},
};

use rand_core::{OsRng, RngCore};
use seq_ex::{
    sync::{PacketType, RecvSuccess, ReplyGuard, SeqExSync},
    SeqNo, TransportLayer,
};
use serde::{Deserialize, Serialize};

const FILE_CHUNK_SIZE: usize = 1000;
#[derive(Clone, Debug, Serialize, Deserialize)]
enum Packet {
    RequestFile { filename: String },
    ConfirmRequestFile { filesize: u64 },
    FileDownload { filename: String, file_chunk: Vec<u8> },
}

#[derive(Clone)]
struct Transport {
    sender: Sender<Vec<u8>>,
    time: Instant,
}
struct Peer {
    filesystem: Arc<RwLock<HashMap<String, Vec<u8>>>>,
    transport: Transport,
    seqex: Arc<SeqExSync<Packet, Packet>>,
    receiver: Receiver<Vec<u8>>,
}

impl TransportLayer for &Transport {
    type SendData = Packet;

    fn time(&mut self) -> i64 {
        self.time.elapsed().as_millis() as i64
    }

    fn send(&mut self, seq_no: SeqNo, reply_no: Option<SeqNo>, payload: &Packet) {
        let p = PacketType::Payload(seq_no, reply_no, payload.clone());
        if let Ok(p) = serde_json::to_vec(&p) {
            let _ = self.sender.send(p);
        }
    }

    fn send_ack(&mut self, reply_no: SeqNo) {
        let p = PacketType::<Packet>::Ack(reply_no);
        if let Ok(p) = serde_json::to_vec(&p) {
            let _ = self.sender.send(p);
        }
    }
}

fn drop_packet() -> bool {
    OsRng.next_u32() >= (u32::MAX / 4 * 3)
}

fn process(peer: &Peer, guard: ReplyGuard<'_, &Transport, Packet, Packet>, recv_packet: Packet, sent_packet: Option<Packet>) {
    match (recv_packet, sent_packet) {
        (Packet::RequestFile { filename }, None) => {
            let filesystem = peer.filesystem.clone();
            let transport = peer.transport.clone();
            let seqex = peer.seqex.clone();
            if let Some(file) = filesystem.read().unwrap().get(&filename) {
                guard.reply(Packet::ConfirmRequestFile { filesize: file.len() as u64 });
            }
            thread::spawn(move || {
                let filesystem = filesystem.read().unwrap();
                // NOTE: in a real application you need to explicitly handle the situation where the
                // file is missing.
                if let Some(file) = filesystem.get(&filename) {
                    let mut i = 0;
                    while i < file.len() {
                        let j = file.len().min(i + FILE_CHUNK_SIZE);
                        seqex.send(
                            &transport,
                            Packet::FileDownload { filename: filename.clone(), file_chunk: file[i..j].to_vec() },
                        );
                        i = j;
                    }
                }
            });
        }
        (Packet::ConfirmRequestFile { filesize }, Some(Packet::RequestFile { filename })) => {
            let mut filesystem = peer.filesystem.write().unwrap();
            let file = Vec::with_capacity(filesize as usize);
            filesystem.insert(filename, file);
        }
        (Packet::FileDownload { filename, file_chunk }, None) => {
            let mut filesystem = peer.filesystem.write().unwrap();
            if let Some(file) = filesystem.get_mut(&filename) {
                if file.len() + file_chunk.len() <= file.capacity() {
                    file.extend(&file_chunk);
                }
            }
        }
        _ => {
            assert!(false);
        }
    }
}

fn receive(peer: &Peer) {
    while let Ok(packet) = peer.receiver.try_recv() {
        if drop_packet() {
            continue;
        }
        let parsed_packet = serde_json::from_slice::<PacketType<Packet>>(&packet);
        match parsed_packet {
            Ok(PacketType::Ack(reply_no)) => {
                let _ = peer.seqex.receive_ack(reply_no);
            }
            Ok(PacketType::Payload(seq_no, reply_no, payload)) => {
                for RecvSuccess { guard, packet, send_data } in peer.seqex.receive_all(&peer.transport, seq_no, reply_no, payload) {
                    process(peer, guard, packet, send_data);
                }
            }
            _ => {}
        }
    }
}

fn main() {
    let mut filesystem2 = HashMap::new();
    let mut file = Vec::from([0u8; 1 << 16]);
    OsRng.fill_bytes(&mut file);
    filesystem2.insert("File1".to_string(), file);
    let mut file = Vec::from([0u8; 1 << 18]);
    OsRng.fill_bytes(&mut file);
    filesystem2.insert("File2".to_string(), file);
    let mut file = Vec::from([0u8; 1 << 20]);
    OsRng.fill_bytes(&mut file);
    filesystem2.insert("File3".to_string(), file);

    let (send1, recv2) = channel();
    let (send2, recv1) = channel();

    let peer1 = Peer {
        filesystem: Arc::new(RwLock::new(HashMap::new())),
        seqex: Arc::new(SeqExSync::new(5, 1)),
        transport: Transport { time: Instant::now(), sender: send1 },
        receiver: recv1,
    };
    let peer2 = Peer {
        filesystem: Arc::new(RwLock::new(filesystem2)),
        seqex: Arc::new(SeqExSync::new(5, 1)),
        transport: Transport { time: Instant::now(), sender: send2 },
        receiver: recv2,
    };

    peer1.seqex.send(&peer1.transport, Packet::RequestFile { filename: "File1".to_string() });
    peer1.seqex.send(&peer1.transport, Packet::RequestFile { filename: "File3".to_string() });
    peer1.seqex.send(&peer1.transport, Packet::RequestFile { filename: "File2".to_string() });

    for _ in 0..300 {
        receive(&peer1);
        receive(&peer2);
        thread::sleep(Duration::from_millis(1));
        peer1.seqex.service(&peer1.transport);
        peer2.seqex.service(&peer2.transport);
    }

    assert_eq!(peer1.filesystem.read().unwrap().deref(), peer2.filesystem.read().unwrap().deref());
}
