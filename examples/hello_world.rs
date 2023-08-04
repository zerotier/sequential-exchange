use std::sync::mpsc::Receiver;

use seq_ex::sync::{MpscTransport, PacketType};
use seq_ex::{ReplyGuard, SeqEx};

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

fn receive<'a>(recv: &Receiver<PacketType<Packet>>, seq: &mut SeqEx<&'a MpscTransport<Packet>>, transport: &'a MpscTransport<Packet>) {
    let do_pump = {
        let result = match recv.recv().unwrap() {
            PacketType::Ack { reply_no } => {
                seq.receive_ack(reply_no);
                return;
            }
            PacketType::EmptyReply { reply_no } => {
                if let Some(Exclamation) = seq.receive_empty_reply(reply_no) {
                    // Our Hello World exchange ends right here.
                    print!("\n");
                }
                return;
            }
            PacketType::Data { seq_no, reply_no, payload } => seq.receive(transport, seq_no, reply_no, payload),
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
    let (transport1, recv2) = MpscTransport::new();
    let (transport2, recv1) = MpscTransport::new();
    let mut seq1 = SeqEx::default();
    let mut seq2 = SeqEx::default();

    // We begin a "Hello World" exchange right here.
    assert!(seq1.try_send(&transport1, Packet::Hello).is_ok());

    receive(&recv2, &mut seq2, &transport2);
    receive(&recv1, &mut seq1, &transport1);
    receive(&recv2, &mut seq2, &transport2);
    receive(&recv1, &mut seq1, &transport1);
    receive(&recv2, &mut seq2, &transport2);
}
