use std::sync::mpsc::Receiver;

use seq_ex::sync::{MpscTransport, PacketType, RecvSuccess, MpscGuard, MpscSeqEx};

#[derive(Clone, Debug)]
enum Packet {
    Hello,
    Space,
    World,
    Exclamation,
}
use Packet::*;

fn process(guard: MpscGuard<'_, Packet>, recv_packet: Packet, send_packet: Option<Packet>) {
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

fn receive(recv: &Receiver<PacketType<Packet>>, seq: &MpscSeqEx<Packet>, transport: &MpscTransport<Packet>) {
    match recv.recv().unwrap() {
        PacketType::Ack ( reply_no ) => {
            let result = seq.receive_ack(reply_no);
            if let Ok(Exclamation) = result {
                // Our Hello World exchange ends right here.
                print!("\n");
            }
        }
        PacketType::Payload ( seq_no, reply_no, payload ) => {
            for RecvSuccess { guard, packet, send_data } in seq.receive_all(transport, seq_no, reply_no, payload) {
                process(guard, packet, send_data)
            }
        }
    }
}

fn main() {
    let (transport1, recv2) = MpscTransport::new();
    let (transport2, recv1) = MpscTransport::new();
    let seq1 = MpscSeqEx::default();
    let seq2 = MpscSeqEx::default();

    // We begin a "Hello World" exchange right here.
    seq1.send(&transport1, Packet::Hello);

    receive(&recv2, &seq2, &transport2);
    receive(&recv1, &seq1, &transport1);
    receive(&recv2, &seq2, &transport2);
    receive(&recv1, &seq1, &transport1);
    receive(&recv2, &seq2, &transport2);
}
