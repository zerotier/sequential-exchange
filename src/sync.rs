use std::{
    ops::{Deref, DerefMut},
    sync::{
        mpsc::{channel, Receiver, Sender},
        Condvar, Mutex, MutexGuard,
    },
    time::Instant,
};

use crate::{Packet, PumpError, RecvOkRaw, SeqEx, SeqNo, TransportLayer, DEFAULT_INITIAL_SEQ_NO, DEFAULT_RESEND_INTERVAL_MS, DEFAULT_WINDOW_CAP};

pub struct SeqExSync<SendData, RecvData, const CAP: usize = DEFAULT_WINDOW_CAP> {
    seq_ex: Mutex<(SeqEx<SendData, RecvData, CAP>, usize)>,
    send_block: Condvar,
    lock: Condvar,
}

pub struct ReplyGuard<'a, TL: TransportLayer<SendData>, SendData, RecvData, const CAP: usize = DEFAULT_WINDOW_CAP> {
    seq: &'a SeqExSync<SendData, RecvData, CAP>,
    app: Option<TL>,
    reply_no: SeqNo,
    locked: bool,
}
impl<'a, TL: TransportLayer<SendData>, SendData, RecvData, const CAP: usize> ReplyGuard<'a, TL, SendData, RecvData, CAP> {
    /// If you need to reply more than once, say to fragment a large file, then include in your
    /// first reply some identifier, and then `send` all fragments with the same included identifier.
    /// The identifier will tell the remote peer which packets contain fragments of the file,
    /// and since each fragment will be received in order it will be trivial for them to reconstruct
    /// the original file.
    pub fn reply(self, packet_data: SendData) {
        self.reply_with(|_, _| packet_data)
    }
    pub fn reply_with(mut self, packet_data: impl FnOnce(SeqNo, SeqNo) -> SendData) {
        let mut app = None;
        core::mem::swap(&mut app, &mut self.app);
        let mut seq = self.seq.lock();
        let seq_no = seq.seq_no();
        seq.reply_raw(app.unwrap(), self.reply_no, self.locked, packet_data(seq_no, self.reply_no));
        core::mem::forget(self);
    }
}
impl<'a, TL: TransportLayer<SendData>, SendData, RecvData, const CAP: usize> Drop for ReplyGuard<'a, TL, SendData, RecvData, CAP> {
    fn drop(&mut self) {
        if let Some(app) = self.app.as_mut() {
            let mut seq = self.seq.lock();
            if let Some(p) = seq.ack_raw_and_direct(self.reply_no, self.locked) {
                app.send(p)
            }
        }
    }
}
impl<'a, TL: TransportLayer<SendData>, SendData, RecvData, const CAP: usize> std::fmt::Debug for ReplyGuard<'a, TL, SendData, RecvData, CAP> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ReplyGuard").field("reply_no", &self.reply_no).finish()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    /// The packet is out-of-sequence. It was either received too soon or too late and so it would be
    /// invalid to process it right now. No action needs to be taken by the caller.
    OutOfSequence,
    /// The Send Window is currently full. The received packet cannot be processed right now because
    /// it could cause the send window to overflow. No action needs to be taken by the caller.
    WindowIsFull,
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
crate::impl_recvok!(RecvOk, &'a SeqExSync<SendData, RecvData, CAP>);

pub struct ReplyIter<'a, TL: TransportLayer<SendData>, P: Into<RecvData>, SendData, RecvData, const CAP: usize = DEFAULT_WINDOW_CAP> {
    seq: Option<&'a SeqExSync<SendData, RecvData, CAP>>,
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
            lock: Condvar::default(),
        }
    }

    pub fn receive<TL: TransportLayer<SendData>, P: Into<RecvData>>(
        &self,
        app: TL,
        mut packet: Packet<P>,
    ) -> Result<RecvOk<'_, TL, P, SendData, RecvData, CAP>, Error> {
        let mut seq = self.seq_ex.lock().unwrap();
        loop {
            match seq.0.receive_raw(app.clone(), packet) {
                Ok(r) => {
                    if seq.1 > 0 {
                        self.send_block.notify_one();
                    }
                    return Ok(RecvOk::from_raw(self, app, r));
                }
                Err(crate::Error::OutOfSequence) => return Err(Error::OutOfSequence),
                Err(crate::Error::WindowIsFull(_)) => return Err(Error::WindowIsFull),
                Err(crate::Error::WindowIsLocked(p)) => {
                    seq = self.lock.wait(seq).unwrap();
                    packet = p;
                }
            }
        }
    }
    pub fn pump<TL: TransportLayer<SendData>>(&self, app: TL) -> Result<RecvOk<'_, TL, RecvData, SendData, RecvData, CAP>, Error> {
        let mut seq = self.seq_ex.lock().unwrap();
        loop {
            match seq.0.pump_raw() {
                Ok(r) => {
                    if seq.1 > 0 {
                        self.send_block.notify_one();
                    }
                    return Ok(RecvOk::from_raw(self, app, r));
                }
                Err(PumpError::OutOfSequence) => return Err(Error::OutOfSequence),
                Err(PumpError::WindowIsFull) => return Err(Error::WindowIsFull),
                Err(PumpError::WindowIsLocked) => {
                    seq = self.lock.wait(seq).unwrap();
                }
            }
        }
    }
    pub fn receive_all<TL: TransportLayer<SendData>, P: Into<RecvData>>(
        &self,
        app: TL,
        packet: Packet<P>,
    ) -> ReplyIter<'_, TL, P, SendData, RecvData, CAP> {
        if let Ok(r) = self.receive(app.clone(), packet) {
            ReplyIter { seq: Some(self), app, first: Some(r) }
        } else {
            ReplyIter { seq: None, app, first: None }
        }
    }
    pub fn try_send<TL: TransportLayer<SendData>>(&self, app: TL, packet_data: SendData) -> Result<(), SendData> {
        self.try_send_with(app, |_| packet_data)
    }
    pub fn send_with<TL: TransportLayer<SendData>>(&self, app: TL, mut packet_data: impl FnMut(SeqNo) -> SendData) {
        let mut seq = self.seq_ex.lock().unwrap();
        while let Err(()) = seq.0.try_send_with(app.clone(), &mut packet_data) {
            seq.1 += 1;
            seq = self.send_block.wait(seq).unwrap();
            seq.1 -= 1;
        }
    }
    pub fn send<TL: TransportLayer<SendData>>(&self, app: TL, mut packet_data: SendData) {
        let mut seq = self.seq_ex.lock().unwrap();
        while let Err(p) = seq.0.try_send(app.clone(), packet_data) {
            packet_data = p;
            seq.1 += 1;
            seq = self.send_block.wait(seq).unwrap();
            seq.1 -= 1;
        }
    }
    pub fn try_send_with<TL: TransportLayer<SendData>>(&self, app: TL, packet_data: impl FnOnce(SeqNo) -> SendData) -> Result<(), SendData> {
        let mut seq = self.lock();
        let seq_no = seq.seq_no();
        seq.try_send(app, packet_data(seq_no))
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
        } else if let Some(origin) = self.seq {
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
