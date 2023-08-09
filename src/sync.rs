use std::cell::UnsafeCell;
use std::sync::Condvar;
use std::{
    sync::{
        mpsc::{channel, Receiver, Sender},
        Mutex, MutexGuard,
    },
    time::Instant,
};

use crate::{
    Error, SeqEx, SeqNo, TransportLayer, DEFAULT_INITIAL_SEQ_NO, DEFAULT_RECV_WINDOW_LEN, DEFAULT_RESEND_INTERVAL_MS, DEFAULT_SEND_WINDOW_LEN,
};

pub struct SeqExSync<TL: TransportLayer, const SLEN: usize = DEFAULT_SEND_WINDOW_LEN, const RLEN: usize = DEFAULT_RECV_WINDOW_LEN> {
    seq_ex: Mutex<SeqEx<TL, SLEN, RLEN>>,
    /// The mutex above is always held when this value changes, hence it is safe to mutate.
    /// We don't pack this as a component of the mutex to avoid having to reimplement MutexGuard.
    wait_count: UnsafeCell<usize>,
    send_block: Condvar,
}

pub struct ReplyGuard<'a, TL: TransportLayer>(&'a SeqExSync<TL>, TL, SeqNo);
impl<'a, TL: TransportLayer> ReplyGuard<'a, TL> {
    pub fn reply(self, packet_data: TL::SendData) {
        let mut seq = self.0.seq_ex.lock().unwrap();
        seq.reply_raw(self.1.clone(), self.2, packet_data);
        core::mem::forget(self);
    }
}
impl<'a, TL: TransportLayer> Drop for ReplyGuard<'a, TL> {
    fn drop(&mut self) {
        let mut seq = self.0.seq_ex.lock().unwrap();
        seq.reply_empty_raw(self.1.clone(), self.2);
    }
}

impl<TL: TransportLayer> SeqExSync<TL> {
    pub fn new(retry_interval: i64, initial_seq_no: SeqNo) -> Self {
        Self {
            seq_ex: Mutex::new(SeqEx::new(retry_interval, initial_seq_no)),
            wait_count: UnsafeCell::new(0),
            send_block: Condvar::default(),
        }
    }

    pub fn receive<P: Into<TL::RecvData>>(
        &self,
        app: TL,
        seq_no: SeqNo,
        reply_no: Option<SeqNo>,
        packet: P,
    ) -> Result<(ReplyGuard<'_, TL>, P, Option<TL::SendData>), Error> {
        let mut seq = self.lock();
        let ret = seq.receive_raw(app.clone(), seq_no, reply_no, packet);
        self.unblock(ret.is_ok());
        ret.map(|(reply_no, packet, data)| (ReplyGuard(self, app, reply_no), packet, data))
    }
    pub fn pump(&self, app: TL) -> Result<(ReplyGuard<'_, TL>, TL::RecvData, Option<TL::SendData>), Error> {
        let mut seq = self.lock();
        let ret = seq.pump_raw();
        self.unblock(ret.is_ok());
        ret.map(|(reply_no, packet, data)| (ReplyGuard(self, app, reply_no), packet, data))
    }
    #[inline]
    fn unblock(&self, is_ok: bool) {
        let has_waiting = unsafe { *self.wait_count.get() > 0 };
        if has_waiting && is_ok {
            self.send_block.notify_one();
        }
    }
    pub fn try_send(&self, app: TL, packet_data: TL::SendData) -> Result<(), TL::SendData> {
        let mut seq = self.lock();
        seq.try_send(app, packet_data)
    }
    pub fn send(&self, app: TL, mut packet_data: TL::SendData) {
        let mut seq = self.lock();
        while let Err(p) = seq.try_send(app.clone(), packet_data) {
            packet_data = p;
            unsafe { *self.wait_count.get() += 1 }
            seq = self.send_block.wait(seq).unwrap();
            unsafe { *self.wait_count.get() -= 1 }
        }
    }

    pub fn receive_ack(&self, reply_no: SeqNo) {
        self.lock().receive_ack(reply_no)
    }
    pub fn receive_empty_reply(&self, reply_no: SeqNo) -> Option<TL::SendData> {
        let ret = self.lock().receive_empty_reply(reply_no);
        if ret.is_some() {
            self.send_block.notify_one();
        }
        ret
    }
    pub fn service(&self, app: TL) -> i64 {
        self.lock().service(app)
    }

    pub fn lock(&self) -> MutexGuard<SeqEx<TL>> {
        self.seq_ex.lock().unwrap()
    }
}
impl<TL: TransportLayer> Default for SeqExSync<TL> {
    fn default() -> Self {
        Self::new(DEFAULT_RESEND_INTERVAL_MS, DEFAULT_INITIAL_SEQ_NO)
    }
}

#[derive(Clone)]
pub enum PacketType<Payload: Clone> {
    Data {
        seq_no: SeqNo,
        reply_no: Option<SeqNo>,
        payload: Payload,
    },
    Ack {
        reply_no: SeqNo,
    },
    EmptyReply {
        reply_no: SeqNo,
    },
}

#[derive(Clone)]
pub struct MpscTransport<Payload: Clone> {
    pub channel: Sender<PacketType<Payload>>,
    pub time: Instant,
}
impl<Payload: Clone> MpscTransport<Payload> {
    pub fn new() -> (Self, Receiver<PacketType<Payload>>) {
        let (send, recv) = channel();
        (Self { channel: send, time: std::time::Instant::now() }, recv)
    }
    pub fn from_sender(send: Sender<PacketType<Payload>>) -> Self {
        Self { channel: send, time: std::time::Instant::now() }
    }
}
impl<Payload: Clone> TransportLayer for &MpscTransport<Payload> {
    type RecvData = Payload;
    type SendData = Payload;

    fn time(&self) -> i64 {
        self.time.elapsed().as_millis() as i64
    }

    fn send(&self, seq_no: SeqNo, reply_no: Option<SeqNo>, payload: &Payload) {
        let _ = self.channel.send(PacketType::Data { seq_no, reply_no, payload: payload.clone() });
    }
    fn send_ack(&self, reply_no: SeqNo) {
        let _ = self.channel.send(PacketType::Ack { reply_no });
    }
    fn send_empty_reply(&self, reply_no: SeqNo) {
        let _ = self.channel.send(PacketType::EmptyReply { reply_no });
    }
}
