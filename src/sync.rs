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
    Error, SeqEx, SeqNo, TransportLayer, DEFAULT_INITIAL_SEQ_NO, DEFAULT_WINDOW_CAP, DEFAULT_RESEND_INTERVAL_MS,
};

pub struct SeqExSync<TL: TransportLayer, const CAP: usize = DEFAULT_WINDOW_CAP> {
    seq_ex: Mutex<SeqEx<TL, CAP>>,
    /// The mutex above is always held when this value changes, hence it is safe to mutate.
    /// We don't pack this as a component of the mutex to avoid having to reimplement MutexGuard.
    wait_count: UnsafeCell<usize>,
    send_block: Condvar,
}

pub struct ReplyGuard<'a, TL: TransportLayer, const CAP: usize = DEFAULT_WINDOW_CAP>(&'a SeqExSync<TL, CAP>, TL, SeqNo);
impl<'a, TL: TransportLayer, const CAP: usize> ReplyGuard<'a, TL, CAP> {
    /// If you need to reply more than once, say to fragment a large file, then include in your
    /// first reply some identifier, and then `send` all fragments with the same included identifier.
    /// The identifier will tell the remote peer which packets contain fragments of the file,
    /// and since each fragment will be received in order it will be trivial for them to reconstruct
    /// the original file.
    pub fn reply(self, packet_data: TL::SendData) {
        let mut seq = self.0.seq_ex.lock().unwrap();
        seq.reply_raw(self.1.clone(), self.2, packet_data);
        core::mem::forget(self);
    }
}
impl<'a, TL: TransportLayer, const CAP: usize> Drop for ReplyGuard<'a, TL, CAP> {
    fn drop(&mut self) {
        let mut seq = self.0.seq_ex.lock().unwrap();
        seq.ack_raw(self.1.clone(), self.2);
    }
}

pub struct RecvSuccess<'a, TL: TransportLayer, P: Into<TL::RecvData>, const CAP: usize = DEFAULT_WINDOW_CAP> {
    pub guard: ReplyGuard<'a, TL, CAP>,
    pub packet: P,
    pub send_data: Option<TL::SendData>,
}

impl<TL: TransportLayer, const CAP: usize> SeqExSync<TL, CAP> {
    pub fn new(retry_interval: i64, initial_seq_no: SeqNo) -> Self {
        Self {
            seq_ex: Mutex::new(SeqEx::<TL, CAP>::new(retry_interval, initial_seq_no)),
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
    ) -> Result<RecvSuccess<'_, TL, P, CAP>, Error> {
        let mut seq = self.lock();
        let ret = seq.receive_raw(app.clone(), seq_no, reply_no, packet);
        // TODO: double check blocking.
        self.unblock(ret.is_ok());
        ret.map(|(reply_no, packet, send_data)| RecvSuccess { guard: ReplyGuard(self, app, reply_no), packet, send_data })
    }
    pub fn pump(&self, app: TL) -> Result<RecvSuccess<'_, TL, TL::RecvData, CAP>, Error> {
        let mut seq = self.lock();
        let ret = seq.pump_raw();
        self.unblock(ret.is_ok());
        ret.map(|(reply_no, packet, send_data)| RecvSuccess { guard: ReplyGuard(self, app, reply_no), packet, send_data })
    }
    pub fn receive_iter(
        &self,
        app: TL,
        seq_no: SeqNo,
        reply_no: Option<SeqNo>,
        packet: TL::RecvData,
    ) -> ReplyIter<'_, TL, CAP> {
        if let Ok(g) = self.receive(app.clone(), seq_no, reply_no, packet) {
            ReplyIter { origin: Some(self), app, first: Some(g) }
        } else {
            ReplyIter { origin: None, app, first: None }
        }
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

    pub fn receive_ack(&self, reply_no: SeqNo) -> Result<TL::SendData, Error> {
        let ret = self.lock().receive_ack(reply_no);
        self.unblock(ret.is_ok());
        ret
    }
    pub fn service(&self, app: TL) -> i64 {
        self.lock().service(app)
    }

    pub fn lock(&self) -> MutexGuard<SeqEx<TL, CAP>> {
        self.seq_ex.lock().unwrap()
    }
}
impl<TL: TransportLayer> Default for SeqExSync<TL> {
    fn default() -> Self {
        Self::new(DEFAULT_RESEND_INTERVAL_MS, DEFAULT_INITIAL_SEQ_NO)
    }
}

pub struct ReplyIter<'a, TL: TransportLayer, const CAP: usize = DEFAULT_WINDOW_CAP> {
    origin: Option<&'a SeqExSync<TL, CAP>>,
    app: TL,
    first: Option<RecvSuccess<'a, TL, TL::RecvData, CAP>>
}

impl<'a, TL: TransportLayer> Iterator for ReplyIter<'a, TL> {
    type Item = RecvSuccess<'a, TL, TL::RecvData>;
    fn next(&mut self) -> Option<Self::Item> {
        if let Some(g) = self.first.take() {
            Some(g)
        } else if let Some(origin) = self.origin {
            origin.pump(self.app.clone()).ok()
        } else {
            None
        }
    }
}

#[derive(Clone)]
pub enum PacketType<Payload: Clone> {
    Payload {
        seq_no: SeqNo,
        reply_no: Option<SeqNo>,
        payload: Payload,
    },
    Ack {
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

    fn time(&mut self) -> i64 {
        self.time.elapsed().as_millis() as i64
    }

    fn send(&mut self, seq_no: SeqNo, reply_no: Option<SeqNo>, payload: &Payload) {
        let _ = self.channel.send(PacketType::Payload { seq_no, reply_no, payload: payload.clone() });
    }
    fn send_ack(&mut self, reply_no: SeqNo) {
        let _ = self.channel.send(PacketType::Ack { reply_no });
    }
}
