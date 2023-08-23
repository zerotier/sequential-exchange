use std::{
    sync::{
        mpsc::{channel, Receiver, Sender},
        Condvar, Mutex, MutexGuard,
    },
    time::Instant,
};

use crate::{
    Packet, TryError, RecvError, RecvOkRaw, SeqEx, SeqNo, TransportLayer, DEFAULT_INITIAL_SEQ_NO, DEFAULT_RESEND_INTERVAL_MS, DEFAULT_WINDOW_CAP,
};

pub struct SeqExSync<SendData, RecvData, const CAP: usize = DEFAULT_WINDOW_CAP> {
    inner: Mutex<SeqExInner<SendData, RecvData, CAP>>,
    wait_on_recv: Condvar,
    wait_on_reply_sender: Condvar,
    wait_on_reply_receiver: Condvar,
}
struct SeqExInner<SendData, RecvData, const CAP: usize> {
    seq: SeqEx<SendData, RecvData, CAP>,
    recv_waiters: usize,
    reply_sender_waiters: bool,
    reply_receiver_waiters: bool,
}

pub struct ReplyGuard<'a, TL: TransportLayer<SendData>, SendData, RecvData, const CAP: usize = DEFAULT_WINDOW_CAP> {
    seq: &'a SeqExSync<SendData, RecvData, CAP>,
    app: Option<TL>,
    reply_no: SeqNo,
    is_holding_lock: bool,
}
impl<'a, TL: TransportLayer<SendData>, SendData, RecvData, const CAP: usize> ReplyGuard<'a, TL, SendData, RecvData, CAP> {
    fn reply_with(mut self, seq_cst: bool, packet_data: impl FnOnce(SeqNo, SeqNo) -> SendData) {
        let app = self.app.take().unwrap();
        let mut inner = self.seq.inner.lock().unwrap();
        let seq_no = inner.seq.seq_no();
        inner.seq
            .reply_raw(app, self.reply_no, self.is_holding_lock, seq_cst, packet_data(seq_no, self.reply_no));
        self.seq.notify_reply(inner);
    }
    /// If you need to reply more than once, say to fragment a large file, then include in your
    /// first reply some identifier, and then `send` all fragments with the same included identifier.
    /// The identifier will tell the remote peer which packets contain fragments of the file,
    /// and since each fragment will be received in order it will be trivial for them to reconstruct
    /// the original file.
    pub fn reply(self, seq_cst: bool, packet_data: SendData) {
        self.reply_with(seq_cst, |_, _| packet_data)
    }

    pub fn to_components(mut self) -> (TL, SeqNo, bool) {
        (self.app.take().unwrap(), self.reply_no, self.is_holding_lock)
    }
    pub unsafe fn from_components(seq: &'a SeqExSync<SendData, RecvData, CAP>, app: TL, reply_no: SeqNo, is_holding_lock: bool) -> Self {
        ReplyGuard { seq, app: Some(app), reply_no, is_holding_lock }
    }
}
impl<'a, TL: TransportLayer<SendData>, SendData, RecvData, const CAP: usize> Drop for ReplyGuard<'a, TL, SendData, RecvData, CAP> {
    fn drop(&mut self) {
        if let Some(app) = self.app.take() {
            let mut inner = self.seq.inner.lock().unwrap();
            inner.seq.ack_raw(app, self.reply_no, self.is_holding_lock);
            self.seq.notify_reply(inner);
        }
    }
}
impl<'a, TL: TransportLayer<SendData>, SendData, RecvData, const CAP: usize> std::fmt::Debug for ReplyGuard<'a, TL, SendData, RecvData, CAP> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ReplyGuard")
            .field("reply_no", &self.reply_no)
            .field("is_holding_lock", &self.is_holding_lock)
            .finish()
    }
}

pub enum RecvOk<'a, TL: TransportLayer<SendData>, SendData, RecvData, const CAP: usize = DEFAULT_WINDOW_CAP> {
    Payload {
        reply_guard: ReplyGuard<'a, TL, SendData, RecvData, CAP>,
        recv_data: RecvData,
    },
    Reply {
        reply_guard: ReplyGuard<'a, TL, SendData, RecvData, CAP>,
        recv_data: RecvData,
        send_data: SendData,
    },
    Ack {
        send_data: SendData,
    },
}
crate::impl_recvok!(RecvOk, &'a SeqExSync<SendData, RecvData, CAP>);

pub struct RecvIter<'a, TL: TransportLayer<SendData>, SendData, RecvData, const CAP: usize = DEFAULT_WINDOW_CAP> {
    seq: Option<&'a SeqExSync<SendData, RecvData, CAP>>,
    app: TL,
    first: Option<RecvOk<'a, TL, SendData, RecvData, CAP>>,
    blocking: bool,
}

impl<SendData, RecvData, const CAP: usize> SeqExSync<SendData, RecvData, CAP> {
    pub fn new(retry_interval: i64, initial_seq_no: SeqNo) -> Self {
        Self {
            inner: Mutex::new(SeqExInner { seq: SeqEx::new(retry_interval, initial_seq_no), recv_waiters: 0, reply_sender_waiters: false, reply_receiver_waiters: false }),
            wait_on_recv: Condvar::default(),
            wait_on_reply_receiver: Condvar::default(),
            wait_on_reply_sender: Condvar::default(),
        }
    }
    fn notify_reply(&self, mut inner: MutexGuard<'_, SeqExInner<SendData, RecvData, CAP>>) {
        if inner.reply_receiver_waiters {
            inner.reply_receiver_waiters = false;
            drop(inner);
            self.wait_on_reply_receiver.notify_all();
        } else if inner.reply_sender_waiters {
            inner.reply_sender_waiters = false;
            drop(inner);
            self.wait_on_reply_sender.notify_all();
        }
    }
    fn notify_recv(&self, mut inner: MutexGuard<'_, SeqExInner<SendData, RecvData, CAP>>) {
        if inner.recv_waiters > 0 {
            inner.recv_waiters -= 1;
            drop(inner);
            self.wait_on_recv.notify_one();
        }
    }

    pub fn try_receive<TL: TransportLayer<SendData>>(
        &self,
        app: TL,
        packet: Packet<RecvData>,
    ) -> Result<(RecvOk<'_, TL, SendData, RecvData, CAP>, bool), RecvError> {
        let mut inner = self.inner.lock().unwrap();
        match inner.seq.receive_raw(app.clone(), packet) {
            Ok((r, do_pump)) => {
                self.notify_recv(inner);
                Ok((RecvOk::from_raw(self, app, r), do_pump))
            }
            Err(e) => Err(e),
        }
    }
    pub fn receive<TL: TransportLayer<SendData>>(
        &self,
        app: TL,
        packet: Packet<RecvData>,
    ) -> Option<(RecvOk<'_, TL, SendData, RecvData, CAP>, bool)> {
        let result = self.try_receive(app.clone(), packet);
        if let Err(RecvError::WaitingForReply) = result {
            self.pump(app)
        } else {
            result.ok()
        }
    }
    pub fn try_pump<TL: TransportLayer<SendData>>(&self, app: TL) -> Result<(RecvOk<'_, TL, SendData, RecvData, CAP>, bool), TryError> {
        let mut inner = self.inner.lock().unwrap();
        match inner.seq.try_pump_raw() {
            Ok((r, do_pump)) => {
                self.notify_recv(inner);
                Ok((RecvOk::from_raw(self, app, r), do_pump))
            }
            Err(e) => Err(e),
        }
    }
    pub fn pump<TL: TransportLayer<SendData>>(&self, app: TL) -> Option<(RecvOk<'_, TL, SendData, RecvData, CAP>, bool)> {
        let mut inner = self.inner.lock().unwrap();
        // Enforce that only one thread may wait to pump at a time.
        if inner.reply_receiver_waiters {
            return None;
        }
        loop {
            match inner.seq.try_pump_raw() {
                Ok((r, do_pump)) => {
                    self.notify_recv(inner);
                    return Some((RecvOk::from_raw(self, app, r), do_pump));
                }
                Err(TryError::WaitingForRecv) => return None,
                Err(TryError::WaitingForReply) => {
                    inner.reply_receiver_waiters = true;
                    inner = self.wait_on_reply_receiver.wait(inner).unwrap();
                }
            }
        }
    }

    pub fn receive_all<TL: TransportLayer<SendData>>(
        &self,
        app: TL,
        packet: Packet<RecvData>,
    ) -> RecvIter<'_, TL, SendData, RecvData, CAP> {
        let ret = self.receive(app.clone(), packet);
        if let Some((first, do_pump)) = ret {
            RecvIter { seq: do_pump.then_some(self), app, first: Some(first), blocking: true }
        } else {
            RecvIter { seq: None, app, first: None, blocking: true }
        }
    }
    pub fn try_receive_all<TL: TransportLayer<SendData>>(
        &self,
        app: TL,
        packet: Packet<RecvData>,
    ) -> RecvIter<'_, TL, SendData, RecvData, CAP> {
        let ret = self.try_receive(app.clone(), packet);
        if let Ok((first, do_pump)) = ret {
            RecvIter { seq: do_pump.then_some(self), app, first: Some(first), blocking: false }
        } else {
            RecvIter { seq: None, app, first: None, blocking: false }
        }
    }

    pub fn try_send_with<TL: TransportLayer<SendData>, F: FnOnce(SeqNo) -> SendData>(&self, app: TL, seq_cst: bool, packet_data: F) -> Result<(), (TryError, F)> {
        let mut inner = self.inner.lock().unwrap();
        inner.seq.try_send_with(app, seq_cst, packet_data)
    }
    pub fn try_send<TL: TransportLayer<SendData>>(&self, app: TL, seq_cst: bool, packet_data: SendData) -> Result<(), (TryError, SendData)> {
        let mut inner = self.inner.lock().unwrap();
        inner.seq.try_send(app, seq_cst, packet_data)
    }

    pub fn send_with<TL: TransportLayer<SendData>>(&self, app: TL, seq_cst: bool, mut packet_data: impl FnOnce(SeqNo) -> SendData) {
        let mut inner = self.inner.lock().unwrap();
        while let Err((e, p)) = inner.seq.try_send_with(app.clone(), seq_cst, packet_data) {
            packet_data = p;
            match e {
                TryError::WaitingForRecv => {
                    inner.recv_waiters += 1;
                    inner = self.wait_on_recv.wait(inner).unwrap();
                }
                TryError::WaitingForReply => {
                    inner.reply_sender_waiters = true;
                    inner = self.wait_on_reply_sender.wait(inner).unwrap();
                }
            }
        }
    }
    pub fn send<TL: TransportLayer<SendData>>(&self, app: TL, seq_cst: bool, mut packet_data: SendData) {
        let mut inner = self.inner.lock().unwrap();
        while let Err((e, p)) = inner.seq.try_send(app.clone(), seq_cst, packet_data) {
            packet_data = p;
            match e {
                TryError::WaitingForRecv => {
                    inner.recv_waiters += 1;
                    inner = self.wait_on_recv.wait(inner).unwrap();
                }
                TryError::WaitingForReply => {
                    inner.reply_sender_waiters = true;
                    inner = self.wait_on_reply_sender.wait(inner).unwrap();
                }
            }
        }
    }

    pub fn service<TL: TransportLayer<SendData>>(&self, app: TL) -> i64 {
        self.inner.lock().unwrap().seq.service(app)
    }
}
impl<SendData, RecvData, const CAP: usize> Default for SeqExSync<SendData, RecvData, CAP> {
    fn default() -> Self {
        Self::new(DEFAULT_RESEND_INTERVAL_MS, DEFAULT_INITIAL_SEQ_NO)
    }
}

impl<'a, TL: TransportLayer<SendData>, SendData, RecvData, const CAP: usize> Iterator
    for RecvIter<'a, TL, SendData, RecvData, CAP>
{
    type Item = RecvOk<'a, TL, SendData, RecvData, CAP>;
    fn next(&mut self) -> Option<Self::Item> {
        if let Some(item) = self.first.take() {
            Some(item)
        } else if let Some(origin) = self.seq {
            let ret = if self.blocking {
                origin.pump(self.app.clone())
            } else {
                origin.try_pump(self.app.clone()).ok()
            };
            if let Some((item, do_pump)) = ret {
                if !do_pump {
                    self.seq = None;
                }
                Some(item)
            } else {
                None
            }
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
