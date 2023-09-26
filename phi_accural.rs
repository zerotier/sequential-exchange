use std::sync::mpsc::Receiver;

use seq_ex::sync::{MpscTransport, PacketType, RecvSuccess, ReplyGuard, SeqExSync};

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
            if let Some(Exclamation) = &result {
                // Our Hello World exchange ends right here.
                print!("\n");
            }
            result.is_some()
        }
        PacketType::Payload { seq_no, reply_no, payload } => {
            if let Ok(RecvSuccess { guard, packet, send_data }) = seq.receive(transport, seq_no, reply_no, payload) {
                process(guard, packet, send_data);
                true
            } else {
                false
            }
        }
    };
    if do_pump {
        while let Ok(RecvSuccess { guard, packet, send_data }) = seq.pump(transport) {
            process(guard, packet, send_data);
        }
    }
}

pub const WINDOW_SIZE: usize = 32;
pub const DEFAULT_ALLOWED_MISSES: f64 = 1.0;
pub const DEFAULT_ALLOWED_PROB: f64 = .01;

/// This version of phi accural takes into account the possibility of packets being dropped uniformly at random from the network, and computes an approximation of that cdf. We do not attempt to dynamically compute the loss rate (since packet loss is not uniform or independent irl), but instead require the user preprogram an `allowed_misses` parameter.
/// `allowed_misses` is an estimation of the number of phi accural packets that the user thinks could possibly be dropped in a row given that both the network and the remote peer are still alive.
///
/// The exact distribution we simulate is the probability that the peer is dead, given the amount of time since the last received phi accural packet, and given that `allowed_misses` number of phi accural packets have been or will be dropped from the network.
#[derive(Clone)]
pub struct PhiAccumulator {
    pub allowed_misses: f64,
    pub allowed_prob: f64,
    head_idx: usize,
    intervals: [f64; WINDOW_SIZE],
    last_time: i64,
    mean: f64,
    std: f64,
}

pub fn normal_cdf_apprx(position: f64, mean: f64, std: f64) -> f64 {

}

impl PhiAccumulator {
    pub fn new(allowed_prob_of_failure: f64, allowed_misses: f64, expected_first_interval: f64, current_time: i64) -> Self {
        PhiAccumulator {
            allowed_misses,
            allowed_prob: allowed_prob_of_failure,
            head_idx: 0,
            intervals: std::array::from_fn(|_| expected_first_interval),
            last_time: current_time,
            mean: expected_first_interval,
            std: 0.0,
        }
    }
    /// Returns false if it is likely that the remote peer is dead or unreachable.
    pub fn check(&self, current_time: i64) -> bool {
        let prob = normal_cdf_apprx((current_time - self.last_time) as f32, (self.allowed_misses + 1.0)*self.mean, self.std);
        prob > self.allowed_prob
    }
    /// Updates the internal state to acknowledge a just received phi accural packet.
    pub fn just_received_phi_packet(&mut self, current_time: i64) {
        let new_interval = (current_time - self.last_time) as f64;
        self.last_time = current_time;
        let idx = self.head_idx;
        self.head_idx += 1;
        // We compute a rolling mean, which is fast but suceptible to rounding errors. Hence f64.
        self.mean += (new_interval - self.intervals[idx])/WINDOW_SIZE as f64;
        self.intervals[idx] = new_interval;
        let mut std = 0.0;
        for x in self.intervals {
            let diff = (x - self.mean);
            std += diff*diff;
        }
        ///
        self.std = std.sqrt()/WINDOW_SIZE as f64;
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
