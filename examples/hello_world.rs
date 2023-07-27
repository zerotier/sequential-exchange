use std::sync::mpsc::{channel, Receiver, Sender};

use seq_ex::{ReplyGuard, SeqEx, SeqNo};
use std::time::Instant;

#[derive(Clone)]
enum RawPacket {
    Ack(SeqNo),
    EmptyReply(SeqNo),
    Send(SeqNo, Packet),
    Reply(SeqNo, SeqNo, Packet),
}
#[derive(Clone)]
enum Packet {
    Hello,
    Space,
    World,
    Exclamation,
}

struct Transport {
    channel: Sender<RawPacket>,
    time: Instant,
}

impl seq_ex::TransportLayer for &Transport {
    type RecvData = Packet;
    type RecvDataRef<'a> = &'a Packet;
    type RecvReturn = ();

    type SendData = RawPacket;

    fn time(&self) -> i64 {
        self.time.elapsed().as_millis() as i64
    }

    fn send(&self, data: &Self::SendData) {
        let _ = self.channel.send(data.clone());
    }
    fn send_ack(&self, reply_no: SeqNo) {
        let _ = self.channel.send(RawPacket::Ack(reply_no));
    }
    fn send_empty_reply(&self, reply_no: SeqNo) {
        let _ = self.channel.send(RawPacket::EmptyReply(reply_no));
    }

    fn deserialize<'a>(data: &'a Self::RecvData) -> Self::RecvDataRef<'a> {
        data
    }
    fn process(&self, reply_cx: ReplyGuard<'_, Self>, recv_packet: &Packet, _: Option<Self::SendData>) -> Self::RecvReturn {
        use RawPacket::Reply;
        match recv_packet {
            Packet::Hello => {
                print!("Hello");
                reply_cx.reply_with(|seq_no, reply_no| Reply(seq_no, reply_no, Packet::Space));
            }
            Packet::Space => {
                print!(" ");
                reply_cx.reply_with(|seq_no, reply_no| Reply(seq_no, reply_no, Packet::World));
            }
            Packet::World => {
                print!("World");
                reply_cx.reply_with(|seq_no, reply_no| Reply(seq_no, reply_no, Packet::Exclamation));
            }
            Packet::Exclamation => {
                println!("!");
            }
        }
    }
}

fn receive<'a>(recv: &Receiver<RawPacket>, seq: &mut SeqEx<&'a Transport>, transport: &'a Transport) {
    match recv.recv().unwrap() {
        RawPacket::Ack(reply_no) => seq.receive_ack(reply_no),
        RawPacket::EmptyReply(reply_no) => {
            seq.receive_empty_reply(reply_no);
        }
        RawPacket::Send(seq_no, packet) => seq.receive(transport, seq_no, None, packet).unwrap(),
        RawPacket::Reply(seq_no, reply_no, packet) => seq.receive(transport, seq_no, Some(reply_no), packet).unwrap(),
    }
}
fn main() {
    let (send1, recv1) = channel();
    let (send2, recv2) = channel();
    let transport1 = Transport { channel: send2, time: Instant::now() };
    let transport2 = Transport { channel: send1, time: Instant::now() };
    let mut seq1 = SeqEx::new(100, 1);
    let mut seq2 = SeqEx::new(100, 1);

    assert!(seq1.send(&transport1, RawPacket::Send(seq1.seq_no(), Packet::Hello)));

    receive(&recv2, &mut seq2, &transport2);
    receive(&recv1, &mut seq1, &transport1);
    receive(&recv2, &mut seq2, &transport2);
    receive(&recv1, &mut seq1, &transport1);
}
