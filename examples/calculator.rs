use std::{
    sync::{
        mpsc::{channel, Receiver, Sender},
        Mutex,
    },
    time::Duration, thread,
};

use seq_ex::{ReplyGuard, SeqEx, SeqNo};
use std::time::Instant;

#[derive(Clone)]
enum RawPacket {
    Ack(SeqNo),
    EmptyReply(SeqNo),
    Send(SeqNo, SendPacket),
}

#[derive(Clone)]
enum SendPacket {
    Add(f64),
    Sub(f64),
    Mul(f64),
    Div(f64),
    Mod(f64),
}

fn drop_packet() -> bool {
    static RNG: Mutex<u32> = Mutex::new(12);
    let mut rng = RNG.lock().unwrap();
    *rng ^= *rng << 13;
    *rng ^= *rng >> 17;
    *rng ^= *rng << 5;
    *rng & 3 == 0
}

struct Transport {
    channel: Sender<RawPacket>,
    time: Instant,
    value: Mutex<f64>,
}

impl seq_ex::TransportLayer for &Transport {
    type RecvData = SendPacket;
    type RecvDataRef<'a> = &'a SendPacket;
    type RecvReturn = ();

    type SendData = RawPacket;

    fn time(&self) -> i64 {
        self.time.elapsed().as_millis() as i64
    }

    fn send(&self, data: &Self::SendData) {
        if drop_packet() {
            let _ = self.channel.send(data.clone());
        }
    }
    fn send_ack(&self, reply_no: SeqNo) {
        if drop_packet() {
            let _ = self.channel.send(RawPacket::Ack(reply_no));
        }
    }
    fn send_empty_reply(&self, reply_no: SeqNo) {
        if drop_packet() {
            let _ = self.channel.send(RawPacket::EmptyReply(reply_no));
        }
    }

    fn deserialize<'a>(data: &'a Self::RecvData) -> Self::RecvDataRef<'a> {
        data
    }
    fn process(&self, _: ReplyGuard<'_, Self>, recv_packet: &SendPacket, _: Option<Self::SendData>) -> Self::RecvReturn {
        let mut value = self.value.lock().unwrap();
        use SendPacket::*;
        match recv_packet {
            Add(n) => *value = *value + n,
            Sub(n) => *value = *value - n,
            Mul(n) => *value = *value * n,
            Div(n) => *value = *value / n,
            Mod(n) => *value = *value % n,
        }
    }
}

fn receive<'a>(recv: &Receiver<RawPacket>, seq: &mut SeqEx<&'a Transport>, transport: &'a Transport) {
    match recv.try_recv() {
        Ok(RawPacket::Ack(reply_no)) => seq.receive_ack(reply_no),
        Ok(RawPacket::EmptyReply(reply_no)) => {
            seq.receive_empty_reply(reply_no);
            while let Ok(()) = seq.pump(transport) {}
        }
        Ok(RawPacket::Send(seq_no, packet)) => match seq.receive(transport, seq_no, None, packet) {
            Ok(()) => {
                while let Ok(()) = seq.pump(transport) {}
            }
            Err(_) => {}
        }
        _ => {}
    }
}
fn main() {
    let (send1, recv1) = channel();
    let (send2, recv2) = channel();
    let transport1 = Transport { channel: send2, time: Instant::now(), value: Mutex::new(0.0) };
    let transport2 = Transport { channel: send1, time: Instant::now(), value: Mutex::new(0.0) };
    let mut seq1 = SeqEx::new(5, 1);
    let mut seq2 = SeqEx::new(5, 1);

    let mut value = 0.0;
    assert!(seq1.send(&transport1, RawPacket::Send(seq1.seq_no(), SendPacket::Add(1.0))));
    value += 1.0;
    assert!(seq1.send(&transport1, RawPacket::Send(seq1.seq_no(), SendPacket::Sub(2.0))));
    value -= 2.0;
    assert!(seq1.send(&transport1, RawPacket::Send(seq1.seq_no(), SendPacket::Mul(3.0))));
    value *= 3.0;
    assert!(seq1.send(&transport1, RawPacket::Send(seq1.seq_no(), SendPacket::Div(4.0))));
    value /= 4.0;
    assert!(seq1.send(&transport1, RawPacket::Send(seq1.seq_no(), SendPacket::Mod(5.0))));
    value %= 5.0;

    for _ in 0..30 {
        receive(&recv1, &mut seq1, &transport1);
        receive(&recv2, &mut seq2, &transport2);
        thread::sleep(Duration::from_millis(5));
        seq1.service(&transport1);
        seq2.service(&transport2);
    }
    let remote_value = transport2.value.lock().unwrap();
    assert_eq!(value, remote_value.clone());
}
