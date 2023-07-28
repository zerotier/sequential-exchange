use std::sync::mpsc::{channel, Receiver, Sender};

use seq_ex::{ReplyGuard, SeqEx, SeqNo};
use std::time::Instant;

#[derive(Clone, Debug)]
enum Packet {
    Ack(SeqNo),
    EmptyReply(SeqNo),
    Hello(SeqNo),
    Reply(SeqNo, SeqNo, ReplyPacket),
}

#[derive(Clone, Debug)]
enum ReplyPacket {
    Space,
    World,
    Exclamation,
}

struct Transport {
    channel: Sender<Packet>,
    time: Instant,
}

impl seq_ex::TransportLayer for &Transport {
    type RecvData = Packet;
    type SendData = Packet;

    fn time(&self) -> i64 {
        self.time.elapsed().as_millis() as i64
    }

    fn send(&self, data: &Self::SendData) {
        let _ = self.channel.send(data.clone());
    }
    fn send_ack(&self, reply_no: SeqNo) {
        let _ = self.channel.send(Packet::Ack(reply_no));
    }
    fn send_empty_reply(&self, reply_no: SeqNo) {
        let _ = self.channel.send(Packet::EmptyReply(reply_no));
    }
}

fn process(guard: ReplyGuard<'_, &Transport>, recv_packet: Packet, send_packet: Option<Packet>) {
    use Packet::*;
    use ReplyPacket::*;
    match (recv_packet, send_packet) {
        (Hello(_), None) => {
            print!("Hello");
            guard.reply_with(|seq_no, reply_no| Reply(seq_no, reply_no, Space));
        }
        (Reply(_, _, r), Some(p)) => match (r, p) {
            (Space, Hello(_)) => {
                print!(" ");
                guard.reply_with(|seq_no, reply_no| Reply(seq_no, reply_no, World));
            }
            (World, Reply(_, _, Space)) => {
                print!("World");
                guard.reply_with(|seq_no, reply_no| Reply(seq_no, reply_no, Exclamation));
            }
            (Exclamation, Reply(_, _, World)) => {
                println!("!");
            }
            (a, b) => {
                println!("Unsolicited reply received: {:?}, was a reply to: {:?}", a, b);
            }
        },
        (a, None) => {
            println!("Unsolicited packet received: {:?}", a);
        }
        (a, Some(b)) => {
            println!("Unsolicited reply received: {:?}, was a reply to: {:?}", a, b);
        }
    }
}

fn receive<'a>(recv: &Receiver<Packet>, seq: &mut SeqEx<&'a Transport>, transport: &'a Transport) {
    use Packet::*;
    let do_pump = {
        let result = match recv.recv().unwrap() {
            Ack(reply_no) => {
                seq.receive_ack(reply_no);
                return;
            }
            EmptyReply(reply_no) => {
                seq.receive_empty_reply(reply_no);
                return;
            }
            Hello(seq_no) => seq.receive(transport, seq_no, None, Hello(seq_no)),
            Reply(seq_no, reply_no, p) => seq.receive(transport, seq_no, Some(reply_no), Reply(seq_no, reply_no, p)),
        };
        if let Ok((guard, recv_packet, send_packet)) = result {
            process(guard, recv_packet, send_packet);
            true
        } else {
            false
        }
    };
    if do_pump {
        while let Ok((guard, recv_packet, send_packet)) = seq.pump(transport) {
            process(guard, recv_packet, send_packet);
        }
    }
}

fn main() {
    let (send1, recv1) = channel();
    let (send2, recv2) = channel();
    let transport1 = Transport { channel: send2, time: Instant::now() };
    let transport2 = Transport { channel: send1, time: Instant::now() };
    let mut seq1 = SeqEx::new(100, 1);
    let mut seq2 = SeqEx::new(100, 1);

    assert!(seq1.send_with(&transport1, |seq_no| Packet::Hello(seq_no)));

    receive(&recv2, &mut seq2, &transport2);
    receive(&recv1, &mut seq1, &transport1);
    receive(&recv2, &mut seq2, &transport2);
    receive(&recv1, &mut seq1, &transport1);
}
