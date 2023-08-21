use std::sync::Mutex;
use tokio::{
    sync::{mpsc, oneshot, Notify},
    time,
};

use crate::{Packet, PumpError, RecvOkRaw, SeqEx, SeqNo, TransportLayer, DEFAULT_INITIAL_SEQ_NO, DEFAULT_RESEND_INTERVAL_MS, DEFAULT_WINDOW_CAP};

type SendData<Payload> = (oneshot::Sender<Option<(SeqNo, bool, Payload)>>, Payload);

pub struct SeqExTokio<Payload, const CAP: usize = DEFAULT_WINDOW_CAP> {
    seq_ex: Mutex<(SeqEx<SendData<Payload>, Payload, CAP>, usize, bool)>,
    send_block: Notify,
    reply_block: Notify,
    update_queue: mpsc::Sender<i64>,
}

pub struct ReplyGuard<'a, TL: TokioLayer<Payload = Payload>, Payload, const CAP: usize = DEFAULT_WINDOW_CAP> {
    seq: &'a SeqExTokio<Payload, CAP>,
    app: Option<TL>,
    reply_no: SeqNo,
    is_holding_lock: bool,
}
impl<'a, TL: TokioLayer<Payload = Payload>, Payload, const CAP: usize> ReplyGuard<'a, TL, Payload, CAP> {
    fn new(seq: &'a SeqExTokio<Payload, CAP>, app: TL, reply_no: SeqNo, is_holding_lock: bool) -> Self {
        ReplyGuard { seq, app: Some(app), reply_no, is_holding_lock }
    }
    fn try_reply_with_inner(&mut self, app: TL, seq_cst: bool, packet_data: impl FnOnce(SeqNo, SeqNo) -> SendData<Payload>) -> Option<i64> {
        let mut seq = self.seq.seq_ex.lock().unwrap();
        let seq_no = seq.0.seq_no();

        let pre_ts = seq.0.next_service_timestamp;
        seq.0
            .reply_raw(app, self.reply_no, self.is_holding_lock, seq_cst, packet_data(seq_no, self.reply_no));
        let ret = (pre_ts != seq.0.next_service_timestamp).then_some(seq.0.next_service_timestamp);
        if seq.2 {
            seq.2 = false;
            drop(seq);
            self.seq.reply_block.notify_waiters();
        }
        ret
    }
    ///// If you need to reply more than once, say to fragment a large file, then include in your
    ///// first reply some identifier, and then `send` all fragments with the same included identifier.
    ///// The identifier will tell the remote peer which packets contain fragments of the file,
    ///// and since each fragment will be received in order it will be trivial for them to reconstruct
    ///// the original file.
    pub async fn reply(self, seq_cst: bool, packet_data: Payload) -> Result<(ReplyGuard<'a, TL, Payload, CAP>, Payload), AsyncError> {
        self.reply_with(seq_cst, |_, _| packet_data).await
    }
    pub async fn reply_with(
        mut self,
        seq_cst: bool,
        packet_data: impl FnOnce(SeqNo, SeqNo) -> Payload,
    ) -> Result<(ReplyGuard<'a, TL, Payload, CAP>, Payload), AsyncError> {
        let app = self.app.take().unwrap();
        let (tx, rx) = oneshot::channel();

        let update_ts = self.try_reply_with_inner(app.clone(), seq_cst, |s, r| (tx, packet_data(s, r)));

        if let Some(update_ts) = update_ts {
            let _ = self.seq.update_queue.send(update_ts).await;
        }
        let (reply_no, seq_cst, recv_data) = rx.await.map_err(|_| AsyncError::SeqExClosed)?.ok_or(AsyncError::ReceivedAck)?;
        Ok((Self::new(self.seq, app, reply_no, seq_cst), recv_data))
    }

    pub fn to_components(mut self) -> (TL, SeqNo, bool) {
        (self.app.take().unwrap(), self.reply_no, self.is_holding_lock)
    }
    pub unsafe fn from_components(seq: &'a SeqExTokio<Payload, CAP>, app: TL, reply_no: SeqNo, is_holding_lock: bool) -> Self {
        Self::new(seq, app, reply_no, is_holding_lock)
    }
}
impl<'a, TL: TokioLayer<Payload = Payload>, Payload, const CAP: usize> Drop for ReplyGuard<'a, TL, Payload, CAP> {
    fn drop(&mut self) {
        if let Some(app) = self.app.take() {
            let mut seq = self.seq.seq_ex.lock().unwrap();
            seq.0.ack_raw(app, self.reply_no, self.is_holding_lock);
            if seq.2 {
                seq.2 = false;
                drop(seq);
                self.seq.reply_block.notify_waiters();
            }
        }
    }
}
impl<'a, TL: TokioLayer<Payload = Payload>, Payload, const CAP: usize> std::fmt::Debug for ReplyGuard<'a, TL, Payload, CAP> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ReplyGuard")
            .field("reply_no", &self.reply_no)
            .field("is_holding_lock", &self.is_holding_lock)
            .finish()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AsyncRecvError {
    DroppedTooEarly,
    DroppedDuplicate,
    WaitingForRecv,
    WaitingForReply,
    AsyncReply,
}
impl std::fmt::Display for AsyncRecvError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AsyncRecvError::DroppedTooEarly => write!(f, "packet arrived too early"),
            AsyncRecvError::DroppedDuplicate => write!(f, "packet was a duplicate"),
            AsyncRecvError::WaitingForRecv => write!(f, "can't process until another packet is received"),
            AsyncRecvError::WaitingForReply => write!(f, "can't process until a reply is finished"),
            AsyncRecvError::AsyncReply => write!(f, "packet was an async reply"),
        }
    }
}
impl std::error::Error for AsyncRecvError {}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AsyncError {
    ReceivedAck,
    SeqExClosed,
}
impl std::fmt::Display for AsyncError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AsyncError::ReceivedAck => write!(f, "peer replied with an ack"),
            AsyncError::SeqExClosed => write!(f, "the window was closed before a reply could be received"),
        }
    }
}
impl std::error::Error for AsyncError {}

pub struct AsyncRecvIter<'a, TL: TokioLayer<Payload = Payload>, P: Into<Payload>, Payload, const CAP: usize = DEFAULT_WINDOW_CAP> {
    seq: Option<&'a SeqExTokio<Payload, CAP>>,
    app: TL,
    first: Option<(ReplyGuard<'a, TL, Payload, CAP>, P)>,
}
pub struct RecvIter<'a, TL: TokioLayer<Payload = Payload>, P: Into<Payload>, Payload, const CAP: usize = DEFAULT_WINDOW_CAP> {
    seq: Option<&'a SeqExTokio<Payload, CAP>>,
    app: TL,
    first: Option<(ReplyGuard<'a, TL, Payload, CAP>, P)>,
}

pub struct ServiceState {
    next_service_timestamp: i64,
    recv_service_update: mpsc::Receiver<i64>,
}

impl<Payload, const CAP: usize> SeqExTokio<Payload, CAP> {
    pub fn new(retry_interval: i64, initial_seq_no: SeqNo) -> (Self, ServiceState) {
        let (update_queue, recv_service_update) = mpsc::channel(8);
        (
            Self {
                seq_ex: Mutex::new((SeqEx::new(retry_interval, initial_seq_no), 0, false)),
                send_block: Notify::new(),
                reply_block: Notify::new(),
                update_queue,
            },
            ServiceState { next_service_timestamp: i64::MAX, recv_service_update },
        )
    }
    pub fn new_default() -> (Self, ServiceState) {
        Self::new(DEFAULT_RESEND_INTERVAL_MS, DEFAULT_INITIAL_SEQ_NO)
    }

    fn map_raw_or_reply<'a, TL: TokioLayer<Payload = Payload>, P: Into<Payload>>(
        &'a self,
        app: TL,
        seq: &mut SeqEx<SendData<Payload>, Payload, CAP>,
        ret: RecvOkRaw<SendData<Payload>, P>,
    ) -> Option<(ReplyGuard<'a, TL, Payload, CAP>, P)> {
        match ret {
            RecvOkRaw::Payload { reply_no, seq_cst, recv_data } => Some((ReplyGuard::new(self, app, reply_no, seq_cst), recv_data)),
            RecvOkRaw::Reply { reply_no, seq_cst, recv_data, send_data: (tx, _) } => {
                if tx.send(Some((reply_no, seq_cst, recv_data.into()))).is_err() {
                    // Send an ack if no one is receiving the reply on the other end.
                    // Could occur if the future holding the receiver is dropped.
                    seq.ack_raw(app, reply_no, seq_cst);
                }
                None
            }
            RecvOkRaw::Ack { send_data: (tx, _) } => {
                drop(tx);
                None
            }
        }
    }

    pub fn receive<TL: TokioLayer<Payload = Payload>, P: Into<Payload>>(
        &self,
        app: TL,
        packet: Packet<P>,
    ) -> Result<(ReplyGuard<'_, TL, Payload, CAP>, P), AsyncRecvError> {
        let mut seq = self.seq_ex.lock().unwrap();
        return match seq.0.receive_raw(app.clone(), packet) {
            Ok(ret) => {
                if seq.1 > 0 {
                    seq.1 -= 1;
                    self.send_block.notify_one();
                }
                self.map_raw_or_reply(app, &mut seq.0, ret).ok_or(AsyncRecvError::AsyncReply)
            }
            Err(crate::RecvError::DroppedTooEarly) => Err(AsyncRecvError::DroppedTooEarly),
            Err(crate::RecvError::DroppedDuplicate) => Err(AsyncRecvError::DroppedDuplicate),
            Err(crate::RecvError::WaitingForRecv) => Err(AsyncRecvError::WaitingForRecv),
            Err(crate::RecvError::WaitingForReply) => Err(AsyncRecvError::WaitingForReply),
        };
    }

    fn try_pump_inner<TL: TokioLayer<Payload = Payload>>(
        &self,
        app: TL,
        blocking: bool,
    ) -> Result<(ReplyGuard<'_, TL, Payload, CAP>, Payload), PumpError> {
        let mut seq = self.seq_ex.lock().unwrap();
        // Enforce that only one thread may pump at a time.
        loop {
            match seq.0.try_pump_raw() {
                Ok(ret) => {
                    if seq.1 > 0 {
                        seq.1 -= 1;
                        self.send_block.notify_one();
                    }
                    if let Some(ret) = self.map_raw_or_reply(app.clone(), &mut seq.0, ret) {
                        return Ok(ret);
                    }
                }
                Err(PumpError::WaitingForRecv) => return Err(PumpError::WaitingForRecv),
                Err(PumpError::WaitingForReply) => {
                    seq.2 |= blocking;
                    return Err(PumpError::WaitingForReply);
                }
            }
        }
    }
    pub fn try_pump<TL: TokioLayer<Payload = Payload>>(&self, app: TL) -> Result<(ReplyGuard<'_, TL, Payload, CAP>, Payload), PumpError> {
        self.try_pump_inner(app, false)
    }
    pub async fn pump<TL: TokioLayer<Payload = Payload>>(&self, app: TL) -> Option<(ReplyGuard<'_, TL, Payload, CAP>, Payload)> {
        loop {
            match self.try_pump_inner(app.clone(), true) {
                Ok(ret) => return Some(ret),
                Err(PumpError::WaitingForRecv) => return None,
                Err(PumpError::WaitingForReply) => {
                    self.reply_block.notified().await;
                }
            }
        }
    }
    pub fn try_receive_all<TL: TokioLayer<Payload = Payload>, P: Into<Payload>>(
        &self,
        app: TL,
        packet: Packet<P>,
    ) -> RecvIter<'_, TL, P, Payload, CAP> {
        match self.receive(app.clone(), packet) {
            Ok(ret) => RecvIter { seq: Some(self), app, first: Some(ret) },
            Err(AsyncRecvError::AsyncReply) => RecvIter { seq: Some(self), app, first: None },
            Err(_) => RecvIter { seq: None, app, first: None },
        }
    }
    pub fn receive_all<TL: TokioLayer<Payload = Payload>, P: Into<Payload>>(
        &self,
        app: TL,
        packet: Packet<P>,
    ) -> AsyncRecvIter<'_, TL, P, Payload, CAP> {
        match self.receive(app.clone(), packet) {
            Ok(ret) => AsyncRecvIter { seq: Some(self), app, first: Some(ret) },
            Err(AsyncRecvError::AsyncReply) => AsyncRecvIter { seq: Some(self), app, first: None },
            Err(_) => AsyncRecvIter { seq: None, app, first: None },
        }
    }

    fn try_send_with_inner<TL: TokioLayer<Payload = Payload>, F: FnOnce(SeqNo) -> SendData<Payload>>(
        &self,
        app: TL,
        blocking: bool,
        seq_cst: bool,
        packet_data: F,
    ) -> Result<Option<i64>, F> {
        let mut seq = self.seq_ex.lock().unwrap();
        let pre_ts = seq.0.next_service_timestamp;
        let result = seq.0.try_send_with(app, seq_cst, packet_data);
        if let Err(e) = result {
            seq.1 += blocking as usize;
            Err(e)
        } else {
            Ok((pre_ts != seq.0.next_service_timestamp).then_some(seq.0.next_service_timestamp))
        }
    }

    pub async fn send<TL: TokioLayer<Payload = Payload>>(
        &self,
        app: TL,
        seq_cst: bool,
        packet_data: Payload,
    ) -> Result<(ReplyGuard<'_, TL, Payload, CAP>, Payload), AsyncError> {
        self.send_with(app, seq_cst, |_| packet_data).await
    }
    pub async fn send_with<TL: TokioLayer<Payload = Payload>>(
        &self,
        app: TL,
        seq_cst: bool,
        packet_data: impl FnOnce(SeqNo) -> Payload,
    ) -> Result<(ReplyGuard<'_, TL, Payload, CAP>, Payload), AsyncError> {
        let (rx, tx) = oneshot::channel();
        let mut pf = |s| (rx, packet_data(s));
        loop {
            let ret = self.try_send_with_inner(app.clone(), true, seq_cst, pf);
            match ret {
                Ok(update) => {
                    if let Some(update) = update {
                        let _ = self.update_queue.send(update).await;
                    }
                    let (reply_no, locked, recv_data) = tx.await.map_err(|_| AsyncError::SeqExClosed)?.ok_or(AsyncError::ReceivedAck)?;
                    return Ok((ReplyGuard::new(self, app, reply_no, locked), recv_data));
                }
                Err(p) => {
                    pf = p;
                    self.send_block.notified().await;
                }
            }
        }
    }

    /// This function must be called with the same ServiceState instance returned upon creation of
    /// the given SeqExTokio instance.
    pub async fn service_task<TL: TokioLayer<Payload = Payload>>(&self, mut app: TL, state: &mut ServiceState) {
        let mut result = None;
        if state.next_service_timestamp < i64::MAX {
            let diff = state.next_service_timestamp - app.time();
            if diff > 0 {
                if let Ok(up) = time::timeout(time::Duration::from_millis(diff as u64), state.recv_service_update.recv()).await {
                    result = up
                }
            }
        } else {
            result = state.recv_service_update.recv().await
        };

        if let Some(up) = result {
            state.next_service_timestamp = state.next_service_timestamp.min(up);
        } else {
            let mut seq = self.seq_ex.lock().unwrap();
            seq.0.service(app.clone());
            state.next_service_timestamp = seq.0.next_service_timestamp;
        }
    }
}

impl<'a, TL: TokioLayer<Payload = Payload>, P: Into<Payload>, Payload, const CAP: usize> RecvIter<'a, TL, P, Payload, CAP> {
    pub fn take_first(&mut self) -> Option<(ReplyGuard<'a, TL, Payload, CAP>, P)> {
        self.first.take()
    }
}
impl<'a, TL: TokioLayer<Payload = Payload>, P: Into<Payload>, Payload, const CAP: usize> Iterator for RecvIter<'a, TL, P, Payload, CAP> {
    type Item = (ReplyGuard<'a, TL, Payload, CAP>, Payload);

    fn next(&mut self) -> Option<Self::Item> {
        if let Some(g) = self.first.take() {
            Some((g.0, g.1.into()))
        } else if let Some(seq) = self.seq {
            seq.try_pump(self.app.clone()).ok()
        } else {
            None
        }
    }
}
impl<'a, TL: TokioLayer<Payload = Payload>, P: Into<Payload>, Payload, const CAP: usize> AsyncRecvIter<'a, TL, P, Payload, CAP> {
    pub fn take_first(&mut self) -> Option<(ReplyGuard<'a, TL, Payload, CAP>, P)> {
        self.first.take()
    }

    pub async fn next(&mut self) -> Option<(ReplyGuard<'a, TL, Payload, CAP>, Payload)> {
        if let Some(g) = self.first.take() {
            Some((g.0, g.1.into()))
        } else if let Some(seq) = self.seq {
            seq.pump(self.app.clone()).await
        } else {
            None
        }
    }
}

pub trait TokioLayer: Clone {
    type Payload;

    fn time(&mut self) -> i64;

    fn send(&mut self, packet: Packet<&Self::Payload>);
}

impl<TL: TokioLayer> TransportLayer<SendData<TL::Payload>> for TL {
    fn time(&mut self) -> i64 {
        self.time()
    }
    fn send(&mut self, packet: Packet<&SendData<TL::Payload>>) {
        self.send(packet.map(|p| &p.1))
    }
}

#[derive(Clone, Debug)]
pub struct MpscTransport<Payload: Clone> {
    pub channel: mpsc::Sender<Packet<Payload>>,
    pub time: time::Instant,
}

impl<Payload: Clone> MpscTransport<Payload> {
    pub fn new(buffer: usize) -> (Self, mpsc::Receiver<Packet<Payload>>) {
        let (send, recv) = mpsc::channel(buffer);
        (Self { channel: send, time: time::Instant::now() }, recv)
    }
    pub fn from_sender(send: mpsc::Sender<Packet<Payload>>) -> Self {
        Self { channel: send, time: time::Instant::now() }
    }
}
impl<Payload: Clone> TokioLayer for &MpscTransport<Payload> {
    type Payload = Payload;

    fn time(&mut self) -> i64 {
        self.time.elapsed().as_millis() as i64
    }
    fn send(&mut self, packet: Packet<&Payload>) {
        let _ = self.channel.try_send(packet.cloned());
    }
}
