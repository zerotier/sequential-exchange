use std::{
    ops::{Deref, DerefMut},
    sync::{
        mpsc::{channel, Receiver, Sender},
        Condvar, Mutex, MutexGuard,
    },
    time::Instant,
};

use crate::{Error, Packet, RecvOkRaw, SeqEx, SeqNo, TransportLayer, DEFAULT_INITIAL_SEQ_NO, DEFAULT_RESEND_INTERVAL_MS, DEFAULT_WINDOW_CAP};

pub struct SeqExSync<SendData, RecvData, const CAP: usize = DEFAULT_WINDOW_CAP> {
    seq_ex: Mutex<(SeqEx<SendData, RecvData, CAP>, usize)>,
    send_block: Condvar,
}

pub struct ReplyGuard<'a, TL: TransportLayer<SendData>, SendData, RecvData, const CAP: usize = DEFAULT_WINDOW_CAP> {
    seq: &'a SeqExSync<SendData, RecvData, CAP>,
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
        let mut seq = self.seq.lock();
        seq.reply_raw(self.app.clone(), self.reply_no, packet_data);
        core::mem::forget(self);
    }
    pub fn reply_with(self, packet_data: impl FnOnce(SeqNo, SeqNo) -> SendData) {
        let mut seq = self.seq.lock();
        let seq_no = seq.seq_no();
        seq.reply_raw(self.app.clone(), self.reply_no, packet_data(seq_no, self.reply_no));
        core::mem::forget(self);
    }
}
impl<'a, TL: TransportLayer<SendData>, SendData, RecvData, const CAP: usize> Drop for ReplyGuard<'a, TL, SendData, RecvData, CAP> {
    fn drop(&mut self) {
        let mut seq = self.seq.lock();
        seq.ack_raw(self.app.clone(), self.reply_no);
    }
}
impl<'a, TL: TransportLayer<SendData>, SendData, RecvData, const CAP: usize> std::fmt::Debug for ReplyGuard<'a, TL, SendData, RecvData, CAP> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ReplyGuard").field("reply_no", &self.reply_no).finish()
    }
}

pub enum RecvOk<'a, TL: TransportLayer<SendData>, P, SendData, RecvData, const CAP: usize = DEFAULT_WINDOW_CAP> {
    Payload {
        reply_guard: ReplyGuard<'a, TL, SendData, RecvData, CAP>,
        recv_data: P,
    },
    Reply {
        reply_guard: ReplyGuard<'a, TL, SendData, RecvData, CAP>,
        recv_data: P,
        send_data: SendData,
    },
    Ack {
        send_data: SendData,
    },
}
impl<'a, TL: TransportLayer<SendData>, P: std::fmt::Debug, SendData: std::fmt::Debug, RecvData, const CAP: usize> std::fmt::Debug
    for RecvOk<'a, TL, P, SendData, RecvData, CAP>
{
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Payload { reply_guard, recv_data } => f
                .debug_struct("Payload")
                .field("reply_guard", reply_guard)
                .field("recv_data", recv_data)
                .finish(),
            Self::Reply { reply_guard, recv_data, send_data } => f
                .debug_struct("Reply")
                .field("reply_guard", reply_guard)
                .field("recv_data", recv_data)
                .field("send_data", send_data)
                .finish(),
            Self::Ack { send_data } => f.debug_struct("Ack").field("send_data", send_data).finish(),
        }
    }
}
impl<'a, TL: TransportLayer<SendData>, P, SendData, RecvData, const CAP: usize> RecvOk<'a, TL, P, SendData, RecvData, CAP> {
    pub fn from_raw(seq: &'a SeqExSync<SendData, RecvData, CAP>, app: TL, value: RecvOkRaw<SendData, P>) -> Self {
        match value {
            RecvOkRaw::Payload { reply_no, recv_data } => RecvOk::Payload { reply_guard: ReplyGuard { seq, app, reply_no }, recv_data },
            RecvOkRaw::Reply { reply_no, recv_data, send_data } => RecvOk::Reply {
                reply_guard: ReplyGuard { seq, app, reply_no },
                recv_data,
                send_data,
            },
            RecvOkRaw::Ack { send_data } => RecvOk::Ack { send_data },
        }
    }
    pub fn consume(self) -> (Option<(ReplyGuard<'a, TL, SendData, RecvData, CAP>, P)>, Option<SendData>) {
        match self {
            RecvOk::Payload { reply_guard, recv_data } => (Some((reply_guard, recv_data)), None),
            RecvOk::Reply { reply_guard, recv_data, send_data } => (Some((reply_guard, recv_data)), Some(send_data)),
            RecvOk::Ack { send_data } => (None, Some(send_data)),
        }
    }
    pub fn new(recv_data: Option<(ReplyGuard<'a, TL, SendData, RecvData, CAP>, P)>, send_data: Option<SendData>) -> Option<Self> {
        match (recv_data, send_data) {
            (Some((reply_guard, recv_data)), None) => Some(RecvOk::Payload { reply_guard, recv_data }),
            (Some((reply_guard, recv_data)), Some(send_data)) => Some(RecvOk::Reply { reply_guard, recv_data, send_data }),
            (None, Some(send_data)) => Some(RecvOk::Ack { send_data }),
            (None, None) => None,
        }
    }
}
impl<'a, TL: TransportLayer<SendData>, P: Into<RecvData>, SendData, RecvData, const CAP: usize> RecvOk<'a, TL, P, SendData, RecvData, CAP> {
    pub fn into(self) -> RecvOk<'a, TL, RecvData, SendData, RecvData, CAP> {
        match self {
            RecvOk::Payload { reply_guard, recv_data } => RecvOk::Payload { reply_guard, recv_data: recv_data.into() },
            RecvOk::Reply { reply_guard, recv_data, send_data } => RecvOk::Reply { reply_guard, recv_data: recv_data.into(), send_data },
            RecvOk::Ack { send_data } => RecvOk::Ack { send_data },
        }
    }
}

pub struct ReplyIter<'a, TL: TransportLayer<SendData>, P: Into<RecvData>, SendData, RecvData, const CAP: usize = DEFAULT_WINDOW_CAP> {
    origin: Option<&'a SeqExSync<SendData, RecvData, CAP>>,
    app: TL,
    first: Option<RecvOk<'a, TL, P, SendData, RecvData, CAP>>,
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
        packet: Packet<P>,
    ) -> Result<RecvOk<'_, TL, P, SendData, RecvData, CAP>, Error> {
        let mut seq = self.seq_ex.lock().unwrap();
        let ret = seq.0.receive_raw(app.clone(), packet);
        if seq.1 > 0 && ret.is_ok() {
            self.send_block.notify_one();
        }
        ret.map(|r| RecvOk::from_raw(self, app, r))
    }
    pub fn pump<TL: TransportLayer<SendData>>(&self, app: TL) -> Result<RecvOk<'_, TL, RecvData, SendData, RecvData, CAP>, Error> {
        let mut seq = self.seq_ex.lock().unwrap();
        let ret = seq.0.pump_raw();
        if seq.1 > 0 && ret.is_ok() {
            self.send_block.notify_one();
        }
        ret.map(|r| RecvOk::from_raw(self, app, r))
    }
    pub fn receive_all<TL: TransportLayer<SendData>, P: Into<RecvData>>(
        &self,
        app: TL,
        packet: Packet<P>,
    ) -> ReplyIter<'_, TL, P, SendData, RecvData, CAP> {
        if let Ok(r) = self.receive(app.clone(), packet) {
            ReplyIter { origin: Some(self), app, first: Some(r) }
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

impl<'a, TL: TransportLayer<SendData>, P: Into<RecvData>, SendData, RecvData, const CAP: usize> ReplyIter<'a, TL, P, SendData, RecvData, CAP> {
    pub fn take_first(&mut self) -> Option<RecvOk<'a, TL, P, SendData, RecvData, CAP>> {
        self.first.take()
    }
}
impl<'a, TL: TransportLayer<SendData>, P: Into<RecvData>, SendData, RecvData, const CAP: usize> Iterator
    for ReplyIter<'a, TL, P, SendData, RecvData, CAP>
{
    type Item = RecvOk<'a, TL, RecvData, SendData, RecvData, CAP>;
    fn next(&mut self) -> Option<Self::Item> {
        if let Some(g) = self.first.take() {
            Some(g.into())
        } else if let Some(origin) = self.origin {
            origin.pump(self.app.clone()).ok()
        } else {
            None
        }
    }
}

#[derive(Clone, Debug)]
pub struct MpscTransport<Payload: Clone> {
    pub channel: Sender<Packet<Payload>>,
    pub time: Instant,
}
pub type MpscGuard<'a, Packet> = ReplyGuard<'a, &'a MpscTransport<Packet>, Packet, Packet>;
pub type MpscSeqEx<Packet> = SeqExSync<Packet, Packet>;

impl<Payload: Clone> MpscTransport<Payload> {
    pub fn new() -> (Self, Receiver<Packet<Payload>>) {
        let (send, recv) = channel();
        (Self { channel: send, time: std::time::Instant::now() }, recv)
    }
    pub fn from_sender(send: Sender<Packet<Payload>>) -> Self {
        Self { channel: send, time: std::time::Instant::now() }
    }
}
impl<Payload: Clone> TransportLayer<Payload> for &MpscTransport<Payload> {
    fn time(&mut self) -> i64 {
        self.time.elapsed().as_millis() as i64
    }

    fn send(&mut self, packet: Packet<&Payload>) {
        let _ = self.channel.send(packet.cloned());
    }
}
