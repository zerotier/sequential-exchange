use std::sync::mpsc::Receiver;

use seqex::{
    sync::{MpscTransport, RecvOk, SeqEx},
    Packet,
};

#[derive(Clone, Debug)]
enum Payload {
    Hello,
    Space,
    World,
    Exclamation,
}
use Payload::*;

fn receive(recv: &Receiver<Packet<Payload>>, seq: &SeqEx<Payload, Payload>, transport: &MpscTransport<Payload>) {
    let packet = recv.recv().unwrap();
    for recv_data in seq.receive_all(transport, packet) {
        match recv_data.consume() {
            (Some((guard, Hello)), None) => {
                print!("Hello");
                guard.reply(false, Space);
            }
            (Some((guard, Space)), Some(Hello)) => {
                print!(" ");
                guard.reply(false, World);
            }
            (Some((guard, World)), Some(Space)) => {
                print!("World");
                guard.reply(false, Exclamation);
            }
            (Some((_g, Exclamation)), Some(World)) => {
                print!("!");
            }
            (None, Some(Exclamation)) => {
                // Our Hello World exchange ends right here.
                print!("\n");
            }
            (Some(a), b) => {
                print!("Unsolicited packet received: {:?}", RecvOk::new(Some(a), b));
            }
            _ => {}
        }
    }
}

fn main() {
    let (transport1, recv2) = MpscTransport::new();
    let (transport2, recv1) = MpscTransport::new();
    let seq1 = SeqEx::default();
    let seq2 = SeqEx::default();

    // We begin a "Hello World" exchange right here.
    seq1.send(&transport1, false, Payload::Hello).unwrap();

    receive(&recv2, &seq2, &transport2);
    receive(&recv1, &seq1, &transport1);
    receive(&recv2, &seq2, &transport2);
    receive(&recv1, &seq1, &transport1);
    receive(&recv2, &seq2, &transport2);
}

#[test]
fn test() {
    main()
}
