use std::{
    sync::{mpsc::Receiver, Mutex},
    thread,
    time::Duration,
};

use seq_ex::sync::{MpscTransport, PacketType, RecvSuccess, ReplyGuard, SeqExSync};

#[derive(Clone)]
enum Packet {
    Add(f32),
    Sub(f32),
    Mul(f32),
    Div(f32),
    Mod(f32),
}

fn drop_packet() -> bool {
    static RNG: Mutex<u32> = Mutex::new(43);
    let mut rng = RNG.lock().unwrap();
    *rng ^= *rng << 13;
    *rng ^= *rng >> 17;
    *rng ^= *rng << 5;
    *rng & 1 == 0
}

fn process(_: ReplyGuard<'_, &MpscTransport<Packet>>, recv_packet: Packet, _: Option<Packet>, value: &mut f32) {
    use Packet::*;
    match recv_packet {
        Add(n) => *value = *value + n,
        Sub(n) => *value = *value - n,
        Mul(n) => *value = *value * n,
        Div(n) => *value = *value / n,
        Mod(n) => *value = *value % n,
    }
}

fn receive<'a>(
    recv: &Receiver<PacketType<Packet>>,
    seq: &SeqExSync<&'a MpscTransport<Packet>>,
    transport: &'a MpscTransport<Packet>,
    value: &mut f32,
) {
    while let Ok(packet) = recv.try_recv() {
        if !drop_packet() {
            match packet {
                PacketType::Ack { reply_no } => {
                    let _ = seq.receive_ack(reply_no);
                }
                PacketType::Payload { seq_no, reply_no, payload } => {
                    for RecvSuccess { guard, packet, send_data } in seq.receive_iter(transport, seq_no, reply_no, payload) {
                        process(guard, packet, send_data, value);
                    }
                }
            }
        }
    }
}

fn main() {
    let (transport1, recv2) = MpscTransport::new();
    let (transport2, recv1) = MpscTransport::new();
    let seq1 = SeqExSync::new(5, 1);
    let seq2 = SeqExSync::new(5, 1);
    let mut value = 0.0;
    let mut remote_value = value;

    seq1.send(&transport1, Packet::Add(1.0));
    value += 1.0;
    seq1.send(&transport1, Packet::Sub(2.0));
    value -= 2.0;
    seq1.send(&transport1, Packet::Mul(3.0));
    value *= 3.0;
    seq1.send(&transport1, Packet::Div(4.0));
    value /= 4.0;
    seq1.send(&transport1, Packet::Mod(5.0));
    value %= 5.0;

    for _ in 0..30 {
        receive(&recv1, &seq1, &transport1, &mut value);
        receive(&recv2, &seq2, &transport2, &mut remote_value);
        thread::sleep(Duration::from_millis(5));
        seq1.service(&transport1);
        seq2.service(&transport2);
    }
    assert_eq!(value, remote_value);
}
