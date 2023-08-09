use std::sync::mpsc::Receiver;

use seq_ex::sync::{MpscTransport, PacketType, ReplyGuard, SeqExSync};

#[derive(Clone, Debug)]
enum Packet {
    Hello,
    Space,
    World,
    Exclamation,
}
use Packet::*;

fn process(guard: ReplyGuard<'_, &MpscTransport<Packet>>, recv_packet: Packet, send_packet: Option<Packet>) {
    match (recv_packet, send_packet) {
        (Hello, None) => {
            print!("Hello");
            guard.reply(Space);
        }
        (Space, Some(Hello)) => {
            print!(" ");
            guard.reply(World);
        }
        (World, Some(Space)) => {
            print!("World");
            guard.reply(Exclamation);
        }
        (Exclamation, Some(World)) => {
            print!("!");
        }
        (a, None) => {
            print!("Unsolicited packet received: {:?}", a);
        }
        (a, Some(b)) => {
            print!("Incorrect reply received: {:?}, was a reply to: {:?}", a, b);
        }
    }
}

fn receive<'a>(recv: &Receiver<PacketType<Packet>>, seq: &SeqExSync<&'a MpscTransport<Packet>>, transport: &'a MpscTransport<Packet>) {
    let do_pump = match recv.recv().unwrap() {
        PacketType::Ack { reply_no } => {
            seq.receive_ack(reply_no);
            return;
        }
        PacketType::EmptyReply { reply_no } => {
            let result = seq.receive_empty_reply(reply_no);
            if let Some(Exclamation) = result {
                // Our Hello World exchange ends right here.
                print!("\n");
            }
            result.is_some()
        }
        PacketType::Data { seq_no, reply_no, payload } => {
            if let Ok((guard, recv_packet, send_packet)) = seq.receive(transport, seq_no, reply_no, payload) {
                process(guard, recv_packet, send_packet);
                true
            } else {
                false
            }
        }
    };
    if do_pump {
        while let Ok((guard, recv_packet, send_packet)) = seq.pump(transport) {
            process(guard, recv_packet, send_packet);
        }
    }
}

fn main() {
    let (transport1, recv2) = MpscTransport::new();
    let (transport2, recv1) = MpscTransport::new();
    let seq1 = SeqExSync::default();
    let seq2 = SeqExSync::default();

    // We begin a "Hello World" exchange right here.
    seq1.send(&transport1, Packet::Hello);

    receive(&recv2, &seq2, &transport2);
    receive(&recv1, &seq1, &transport1);
    receive(&recv2, &seq2, &transport2);
    receive(&recv1, &seq1, &transport1);
    receive(&recv2, &seq2, &transport2);
}
