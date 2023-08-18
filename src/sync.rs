use std::{
    ops::{Deref, DerefMut},
    sync::{
        mpsc::{channel, Receiver, Sender},
        Condvar, Mutex, MutexGuard,
    },
    time::Instant,
};

use crate::{Error, Packet, SeqEx, SeqNo, TransportLayer, DEFAULT_INITIAL_SEQ_NO, DEFAULT_RESEND_INTERVAL_MS, DEFAULT_WINDOW_CAP};

pub struct SeqExSync<SendData, RecvData, const CAP: usize = DEFAULT_WINDOW_CAP> {
    seq_ex: Mutex<(SeqEx<SendData, RecvData, CAP>, usize)>,
    send_block: Condvar,
}

pub struct ReplyGuard<'a, TL: TransportLayer<SendData>, SendData, RecvData, const CAP: usize = DEFAULT_WINDOW_CAP> {
    origin: &'a SeqExSync<SendData, RecvData, CAP>,
    app: TL,
    reply_no: SeqNo,
}
impl<'a, TL: TransportLayer<SendData>, SendData, RecvData, const CAP: usize> ReplyGuard<'a, TL, SendData, RecvData, CAP> {
    /// If you need to reply more than once, say to fragment a large file, then include in your
    /// first reply some identifier, and then `send` all fragments with the same included identifier.
    /// The identifier will tell the remote peer which packets contain fragments of the file,
    /// and since each fragment will be received in order it will be trivial for them to reconstruct
    /// the original file.
    pub fn reply(self, packet_data: SendData) {
        let mut seq = self.origin.lock();
        seq.reply_raw(self.app.clone(), self.reply_no, packet_data);
        core::mem::forget(self);
    }
    pub fn reply_with(self, packet_data: impl FnOnce(SeqNo, SeqNo) -> SendData) {
        let mut seq = self.origin.lock();
        let seq_no = seq.seq_no();
        seq.reply_raw(self.app.clone(), self.reply_no, packet_data(seq_no, self.reply_no));
        core::mem::forget(self);
    }
}
impl<'a, TL: TransportLayer<SendData>, SendData, RecvData, const CAP: usize> Drop for ReplyGuard<'a, TL, SendData, RecvData, CAP> {
    fn drop(&mut self) {
        let mut seq = self.origin.lock();
        seq.ack_raw(self.app.clone(), self.reply_no);
    }
}

pub struct RecvSuccess<'a, TL: TransportLayer<SendData>, P: Into<RecvData>, SendData, RecvData, const CAP: usize = DEFAULT_WINDOW_CAP> {
    pub guard: ReplyGuard<'a, TL, SendData, RecvData, CAP>,
    pub packet: P,
    pub send_data: Option<SendData>,
}

pub struct ReplyIter<'a, TL: TransportLayer<SendData>, SendData, RecvData, const CAP: usize = DEFAULT_WINDOW_CAP> {
    origin: Option<&'a SeqExSync<SendData, RecvData, CAP>>,
    app: TL,
    first: Option<RecvSuccess<'a, TL, RecvData, SendData, RecvData, CAP>>,
}

pub struct SeqExGuard<'a, SendData, RecvData, const CAP: usize>(MutexGuard<'a, (SeqEx<SendData, RecvData, CAP>, usize)>);
impl<'a, SendData, RecvData, const CAP: usize> Deref for SeqExGuard<'a, SendData, RecvData, CAP> {
    type Target = SeqEx<SendData, RecvData, CAP>;

    fn deref(&self) -> &Self::Target {
        &self.0 .0
    }
}
impl<'a, SendData, RecvData, const CAP: usize> DerefMut for SeqExGuard<'a, SendData, RecvData, CAP> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0 .0
    }
}

impl<SendData, RecvData, const CAP: usize> SeqExSync<SendData, RecvData, CAP> {
    pub fn new(retry_interval: i64, initial_seq_no: SeqNo) -> Self {
        Self {
            seq_ex: Mutex::new((SeqEx::new(retry_interval, initial_seq_no), 0)),
            send_block: Condvar::default(),
        }
    }

    pub fn receive<TL: TransportLayer<SendData>, P: Into<RecvData>>(
        &self,
        app: TL,
        seq_no: SeqNo,
        reply_no: Option<SeqNo>,
        packet: P,
    ) -> Result<RecvSuccess<'_, TL, P, SendData, RecvData, CAP>, Error> {
        let mut seq = self.seq_ex.lock().unwrap();
        let ret = seq.0.receive_raw(app.clone(), seq_no, reply_no, packet);
        if seq.1 > 0 && ret.is_ok() {
            self.send_block.notify_one();
        }
        ret.map(|(reply_no, packet, send_data)| RecvSuccess {
            guard: ReplyGuard { origin: self, app, reply_no },
            packet,
            send_data,
        })
    }
    pub fn pump<TL: TransportLayer<SendData>>(&self, app: TL) -> Result<RecvSuccess<'_, TL, RecvData, SendData, RecvData, CAP>, Error> {
        let mut seq = self.seq_ex.lock().unwrap();
        let ret = seq.0.pump_raw();
        if seq.1 > 0 && ret.is_ok() {
            self.send_block.notify_one();
        }
        ret.map(|(reply_no, packet, send_data)| RecvSuccess {
            guard: ReplyGuard { origin: self, app, reply_no },
            packet,
            send_data,
        })
    }
    pub fn receive_all<TL: TransportLayer<SendData>>(
        &self,
        app: TL,
        seq_no: SeqNo,
        reply_no: Option<SeqNo>,
        packet: RecvData,
    ) -> ReplyIter<'_, TL, SendData, RecvData, CAP> {
        if let Ok(g) = self.receive(app.clone(), seq_no, reply_no, packet) {
            ReplyIter { origin: Some(self), app, first: Some(g) }
        } else {
            ReplyIter { origin: None, app, first: None }
        }
    }
    pub fn try_send<TL: TransportLayer<SendData>>(&self, app: TL, packet_data: SendData) -> Result<(), SendData> {
        let mut seq = self.lock();
        seq.try_send(app, packet_data)
    }
    fn send_inner<TL: TransportLayer<SendData>>(
        &self,
        mut seq: MutexGuard<'_, (SeqEx<SendData, RecvData, CAP>, usize)>,
        app: TL,
        mut packet_data: SendData,
    ) {
        while let Err(p) = seq.0.try_send(app.clone(), packet_data) {
            packet_data = p;
            seq.1 += 1;
            seq = self.send_block.wait(seq).unwrap();
            seq.1 -= 1;
        }
    }
    pub fn send<TL: TransportLayer<SendData>>(&self, app: TL, packet_data: SendData) {
        self.send_inner(self.seq_ex.lock().unwrap(), app, packet_data)
    }
    pub fn try_send_with<TL: TransportLayer<SendData>>(&self, app: TL, packet_data: impl FnOnce(SeqNo) -> SendData) -> Result<(), SendData> {
        let mut seq = self.lock();
        let seq_no = seq.seq_no();
        seq.try_send(app, packet_data(seq_no))
    }
    pub fn send_with<TL: TransportLayer<SendData>>(&self, app: TL, packet_data: impl FnOnce(SeqNo) -> SendData) {
        let seq = self.seq_ex.lock().unwrap();
        let seq_no = seq.0.seq_no();
        self.send_inner(seq, app, packet_data(seq_no))
    }

    pub fn receive_ack(&self, reply_no: SeqNo) -> Result<SendData, Error> {
        let mut seq = self.seq_ex.lock().unwrap();
        let ret = seq.0.receive_ack(reply_no);
        if seq.1 > 0 && ret.is_ok() {
            self.send_block.notify_one();
        }
        ret
    }
    pub fn service<TL: TransportLayer<SendData>>(&self, app: TL) -> i64 {
        self.lock().service(app)
    }

    pub fn lock(&self) -> SeqExGuard<'_, SendData, RecvData, CAP> {
        SeqExGuard(self.seq_ex.lock().unwrap())
    }
}
impl<SendData, RecvData, const CAP: usize> Default for SeqExSync<SendData, RecvData, CAP> {
    fn default() -> Self {
        Self::new(DEFAULT_RESEND_INTERVAL_MS, DEFAULT_INITIAL_SEQ_NO)
    }
}

impl<'a, TL: TransportLayer<SendData>, SendData, RecvData, const CAP: usize> Iterator for ReplyIter<'a, TL, SendData, RecvData, CAP> {
    type Item = RecvSuccess<'a, TL, RecvData, SendData, RecvData, CAP>;
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

#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[derive(Clone)]
pub enum PacketOwned<Payload: Clone> {
    Payload(SeqNo, Option<SeqNo>, Payload),
    Ack(SeqNo),
}
impl<'a, Payload: Clone> From<Packet<'a, Payload>> for PacketOwned<Payload> {
    fn from(value: Packet<'a, Payload>) -> Self {
        match value {
            Packet::Payload { seq_no, reply_no, data } => PacketOwned::Payload(seq_no, reply_no, data.clone()),
            Packet::Ack { reply_no } => PacketOwned::Ack(reply_no),
        }
    }
}

#[derive(Clone)]
pub struct MpscTransport<Payload: Clone> {
    pub channel: Sender<PacketOwned<Payload>>,
    pub time: Instant,
}
pub type MpscGuard<'a, Packet> = ReplyGuard<'a, &'a MpscTransport<Packet>, Packet, Packet>;
pub type MpscSeqEx<Packet> = SeqExSync<Packet, Packet>;

impl<Payload: Clone> MpscTransport<Payload> {
    pub fn new() -> (Self, Receiver<PacketOwned<Payload>>) {
        let (send, recv) = channel();
        (Self { channel: send, time: std::time::Instant::now() }, recv)
    }
    pub fn from_sender(send: Sender<PacketOwned<Payload>>) -> Self {
        Self { channel: send, time: std::time::Instant::now() }
    }
}
impl<Payload: Clone> TransportLayer<Payload> for &MpscTransport<Payload> {
    fn time(&mut self) -> i64 {
        self.time.elapsed().as_millis() as i64
    }

    fn send(&mut self, packet: Packet<'_, Payload>) {
        let _ = self.channel.send(PacketOwned::from(packet));
    }
}
