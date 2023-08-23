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
    sync::{RecvOk, SeqExSync},
    Packet, TransportLayer,
};
use serde::{Deserialize, Serialize};

const FILE_CHUNK_SIZE: usize = 1000;
#[derive(Clone, Debug, Serialize, Deserialize)]
enum Payload {
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
    seqex: Arc<SeqExSync<Payload, Payload>>,
    receiver: Receiver<Vec<u8>>,
}

impl TransportLayer<Payload> for &Transport {
    fn time(&mut self) -> i64 {
        self.time.elapsed().as_millis() as i64
    }

    fn send(&mut self, packet: Packet<&Payload>) {
        if let Ok(p) = serde_cbor::to_vec(&packet) {
            let _ = self.sender.send(p);
        }
    }
}

fn drop_packet() -> bool {
    OsRng.next_u32() >= (u32::MAX / 4 * 3)
}

fn process(peer: &Peer, recv_data: RecvOk<'_, &Transport, Payload, Payload>) {
    use Payload::*;
    match recv_data.consume() {
        (Some((guard, RequestFile { filename })), None) => {
            let filesystem = peer.filesystem.clone();
            let transport = peer.transport.clone();
            let seqex = peer.seqex.clone();
            if let Some(file) = filesystem.read().unwrap().get(&filename) {
                guard.reply(true, ConfirmRequestFile { filesize: file.len() as u64 });
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
                            true,
                            FileDownload { filename: filename.clone(), file_chunk: file[i..j].to_vec() },
                        );
                        i = j;
                    }
                }
            });
        }
        (Some((_g, ConfirmRequestFile { filesize })), Some(RequestFile { filename })) => {
            let mut filesystem = peer.filesystem.write().unwrap();
            let file = Vec::with_capacity(filesize as usize);
            filesystem.insert(filename, file);
        }
        (Some((_g, FileDownload { filename, file_chunk })), None) => {
            let mut filesystem = peer.filesystem.write().unwrap();
            if let Some(file) = filesystem.get_mut(&filename) {
                if file.len() + file_chunk.len() <= file.capacity() {
                    file.extend(&file_chunk);
                }
            }
        }
        (Some(a), b) => {
            print!("Unsolicited packet received: {:?}", RecvOk::new(Some(a), b));
        }
        _ => {}
    }
}

fn receive(peer: &Peer) {
    while let Ok(packet) = peer.receiver.try_recv() {
        if drop_packet() {
            continue;
        }
        if let Ok(parsed_packet) = serde_cbor::from_slice::<Packet<Payload>>(&packet) {
            for recv_data in peer.seqex.try_receive_all(&peer.transport, parsed_packet) {
                process(peer, recv_data);
            }
        }
    }
}

fn main() {
    let mut filesystem2 = HashMap::new();
    let mut file = vec![0; 1 << 16];
    OsRng.fill_bytes(&mut file);
    filesystem2.insert("File1".to_string(), file);
    let mut file = vec![0; 1 << 18];
    OsRng.fill_bytes(&mut file);
    filesystem2.insert("File2".to_string(), file);
    let mut file = vec![0; 1 << 20];
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

    let tl = &peer1.transport;
    peer1.seqex.send(tl, false, Payload::RequestFile { filename: "File1".to_string() });
    peer1.seqex.send(tl, false, Payload::RequestFile { filename: "File3".to_string() });
    peer1.seqex.send(tl, false, Payload::RequestFile { filename: "File2".to_string() });

    for _ in 0..400 {
        receive(&peer1);
        receive(&peer2);
        thread::sleep(Duration::from_millis(2));
        peer1.seqex.service(&peer1.transport);
        peer2.seqex.service(&peer2.transport);
    }

    assert_eq!(peer1.filesystem.read().unwrap().deref(), peer2.filesystem.read().unwrap().deref());
}

#[test]
fn test() {
    main()
}
