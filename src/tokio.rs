use std::sync::{Mutex, MutexGuard};
use tokio::{
    sync::{mpsc, oneshot, Notify},
    time,
};

use crate::{
    error::{TryError, TryRecvError},
    no_std::RecvOkRaw,
    Packet, SeqNo, TransportLayer, DEFAULT_INITIAL_SEQ_NO, DEFAULT_RESEND_INTERVAL_MS, DEFAULT_WINDOW_CAP,
};

type Sender<SendData, RecvData> = (oneshot::Sender<Option<(SeqNo, bool, RecvData)>>, SendData);
type Receiver<RecvData> = (oneshot::Sender<(SeqNo, bool, RecvData)>, RecvData);

pub struct SeqEx<SendData, RecvData, const CAP: usize = DEFAULT_WINDOW_CAP> {
    inner: Mutex<SeqExInner<SendData, RecvData, CAP>>,
    wait_on_recv: Notify,
    wait_on_reply: Notify,
    update_queue: mpsc::Sender<i64>,
}

struct SeqExInner<SendData, RecvData, const CAP: usize> {
    seq: crate::no_std::SeqEx<Sender<SendData, RecvData>, Receiver<RecvData>, CAP>,
    recv_waiters: usize,
    reply_waiters: bool,
}

pub struct ReplyGuard<'a, TL: TokioLayer<SendData = SendData>, SendData, RecvData, const CAP: usize = DEFAULT_WINDOW_CAP> {
    seq: &'a SeqEx<SendData, RecvData, CAP>,
    tl: TL,
    reply_no: SeqNo,
    is_holding_lock: bool,
}

pub struct SeqCstGuard<'a, SendData, RecvData, const CAP: usize = DEFAULT_WINDOW_CAP> {
    seq: &'a SeqEx<SendData, RecvData, CAP>,
}

impl<'a, TL: TokioLayer<SendData = SendData>, SendData, RecvData, const CAP: usize> ReplyGuard<'a, TL, SendData, RecvData, CAP> {
    /// Returns a reference to the `TransportLayer` instance this guard was created with.
    pub fn get_tl(&self) -> &TL {
        &self.tl
    }
    /// Returns a mutable reference to the `TransportLayer` instance this guard was created with.
    pub fn get_tl_mut(&mut self) -> &mut TL {
        &mut self.tl
    }
    /// Returns whether or nor this reply guard is currently holding the SeqCst lock,
    /// preventing other SeqCst packets from being processed.
    ///
    /// When this returns `true`, it means the current thread is within the critical section for
    /// processing SeqCst packets. SeqCst packets can only enter this critical section in the same
    /// order they were sent.
    pub fn is_seq_cst(&self) -> bool {
        self.is_holding_lock
    }

    fn try_reply_with_inner(&mut self, tl: TL, seq_cst: bool, packet_data: impl FnOnce(SeqNo, SeqNo) -> Sender<SendData, RecvData>) -> Option<i64> {
        let mut inner = self.seq.inner.lock().unwrap();
        let seq_no = inner.seq.seq_no();

        let pre_ts = inner.seq.next_service_timestamp;
        inner
            .seq
            .reply_raw(tl, self.reply_no, self.is_holding_lock, seq_cst, packet_data(seq_no, self.reply_no));
        let ret = (pre_ts != inner.seq.next_service_timestamp).then_some(inner.seq.next_service_timestamp);

        self.seq.notify_reply(inner);
        ret
    }
    /// If you need to reply more than once, say to fragment a large file, then include in your
    /// first reply some identifier, and then `send` all fragments with the same included identifier.
    /// The identifier will tell the remote peer which packets contain fragments of the file,
    /// and since each fragment will be received in order it will be trivial for them to reconstruct
    /// the original file.
    pub async fn reply(self, seq_cst: bool, packet_data: SendData) -> Result<(ReplyGuard<'a, TL, SendData, RecvData, CAP>, RecvData), AsyncError> {
        self.reply_with(seq_cst, |_, _| packet_data).await
    }
    pub async fn reply_with(
        mut self,
        seq_cst: bool,
        packet_data: impl FnOnce(SeqNo, SeqNo) -> SendData,
    ) -> Result<(ReplyGuard<'a, TL, SendData, RecvData, CAP>, RecvData), AsyncError> {
        let tl = self.tl;
        let seq = self.seq;
        let (tx, rx) = oneshot::channel();

        let update_ts = self.try_reply_with_inner(tl, seq_cst, |s, r| (tx, packet_data(s, r)));
        core::mem::forget(self);

        if let Some(update_ts) = update_ts {
            let _ = seq.update_queue.send(update_ts).await;
        }
        let (reply_no, seq_cst, recv_data) = rx.await.map_err(|_| AsyncError::SeqExClosed)?.ok_or(AsyncError::EndOfExchange)?;
        Ok((Self::new(seq, tl, reply_no, seq_cst), recv_data))
    }

    fn consume_lock(self, inner: MutexGuard<'a, SeqExInner<SendData, RecvData, CAP>>) -> Option<SeqCstGuard<'a, SendData, RecvData, CAP>> {
        let ret = if self.is_holding_lock {
            Some(SeqCstGuard { seq: self.seq })
        } else {
            self.seq.notify_reply(inner);
            None
        };
        core::mem::forget(self);
        ret
    }
    /// Cause this reply guard to immediately send an ack to the remote peer,
    /// consuming it and returning a new guard. This `SeqCstGuard` prevents SeqCst packets from
    /// being processed by SeqEx until it is dropped.
    ///
    /// If this guard was not for a SeqCst packet (or the user called `unlock` previously),
    /// then it will return `None` instead of a `SeqCstGuard`.
    ///
    /// This allows advanced users to de-couple locking the critical section for SeqCst packets
    /// from sending acks to received packets.
    /// In cases where the critical section has to be maintained for a long period of time this can
    /// save bandwidth that would otherwise be wasted on resends.
    /// It is not recommended to use this function except for this purpose.
    ///
    /// Casual users are recommended to simply `drop` this reply guard instead of calling this function.
    pub fn ack(self) -> Option<SeqCstGuard<'a, SendData, RecvData, CAP>> {
        let mut inner = self.seq.inner.lock().unwrap();
        inner.seq.ack_raw(self.tl, self.reply_no, false);
        self.consume_lock(inner)
    }
    /// If this guard is holding the SeqCst lock, and preventing other SeqCst packets from
    /// being processed, then this function will force it to drop this lock.
    /// If the next SeqCst packet is waiting in the receive window, then this will unblock a thread
    /// blocked on `SeqEx::receive` or `SeqEx::pump` to process it.
    ///
    /// Returns `true` if this guard was holding the SeqCst lock,
    /// similar to function `ReplyGuard::is_seq_cst`.
    ///
    /// This allows advanced users to de-couple locking the critical section for SeqCst packets
    /// from sending acks to received packets.
    /// In cases where a critical section is no longer needed, this function can allow more than one
    /// thread to process SeqCst packets in parallel, improving performance.
    /// It is not recommended to use this function except for this purpose.
    ///
    /// Casual users are recommended to simply `drop` this reply guard instead of calling this function.
    pub fn unlock(&mut self) -> bool {
        if self.is_holding_lock {
            self.is_holding_lock = false;
            let mut inner = self.seq.inner.lock().unwrap();
            inner.seq.unlock_raw();
            self.seq.notify_reply(inner);
            true
        } else {
            false
        }
    }

    /// Break down a `ReplyGuard` into its primitive components, without causing it to send an ack
    /// or reply to the remote peer.
    ///
    /// The first return value is the packet reply number, and the second is the return value of
    /// `is_seq_cst`, which states whether or not this `ReplyGuard` is holding the SeqCst lock.
    ///
    /// This can be used in combination with `from_components` to move a `ReplyGuard` to a different
    /// thread.
    ///
    /// # Safety
    /// The caller must guarantee that `ReplyGuard::from_components` is eventually called on
    /// the returned values.
    ///
    /// If this does not happen the SEQEX protocol will enter a deadlocked state,
    /// which is likely to cause threads to permanently block.
    pub unsafe fn to_components(self) -> (SeqNo, bool) {
        let ret = (self.reply_no, self.is_holding_lock);
        core::mem::forget(self);
        ret
    }
    fn new(seq: &'a SeqEx<SendData, RecvData, CAP>, tl: TL, reply_no: SeqNo, is_holding_lock: bool) -> Self {
        ReplyGuard { seq, tl, reply_no, is_holding_lock }
    }
    /// Constructs a `ReplyGuard` object from the raw components returned by
    /// `ReplyGuard::to_components`.
    ///
    /// # Safety
    /// The caller must always pass values for `reply_no` and `is_holding_lock` that were
    /// originally returned by consuming a `ReplyGuard` instance with `to_components`.
    ///
    /// `seq` must be the exact same instance of `SeqEx` that issued the original `ReplyGuard`.
    ///
    /// Otherwise undefined behavior will occur.
    pub unsafe fn from_components(seq: &'a SeqEx<SendData, RecvData, CAP>, tl: TL, reply_no: SeqNo, is_holding_lock: bool) -> Self {
        Self::new(seq, tl, reply_no, is_holding_lock)
    }
}
impl<'a, TL: TokioLayer<SendData = SendData>, SendData, RecvData, const CAP: usize> Drop for ReplyGuard<'a, TL, SendData, RecvData, CAP> {
    fn drop(&mut self) {
        let mut inner = self.seq.inner.lock().unwrap();
        inner.seq.ack_raw(self.tl, self.reply_no, self.is_holding_lock);
        self.seq.notify_reply(inner);
    }
}
impl<'a, SendData, RecvData, const CAP: usize> Drop for SeqCstGuard<'a, SendData, RecvData, CAP> {
    fn drop(&mut self) {
        let mut inner = self.seq.inner.lock().unwrap();
        // It is guaranteed at this point for SeqEx to be locked because it was locked on guard creation.
        inner.seq.unlock_raw();
        self.seq.notify_reply(inner);
    }
}
impl<'a, TL: TokioLayer<SendData = SendData>, SendData, RecvData, const CAP: usize> std::fmt::Debug for ReplyGuard<'a, TL, SendData, RecvData, CAP> {
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
    AsyncReply,
    SeqExClosed,
}
impl std::fmt::Display for AsyncRecvError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AsyncRecvError::DroppedTooEarly => write!(f, "packet arrived too early"),
            AsyncRecvError::DroppedDuplicate => write!(f, "packet was a duplicate"),
            AsyncRecvError::AsyncReply => write!(f, "packet was an async reply"),
            AsyncRecvError::SeqExClosed => write!(f, "the instance of SeqEx was dropped"),
        }
    }
}
impl std::error::Error for AsyncRecvError {}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AsyncError {
    EndOfExchange,
    SeqExClosed,
}
impl std::fmt::Display for AsyncError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AsyncError::EndOfExchange => write!(f, "peer replied with an ack"),
            AsyncError::SeqExClosed => write!(f, "the window was closed before a reply could be received"),
        }
    }
}
impl std::error::Error for AsyncError {}

pub struct ServiceState {
    next_service_timestamp: i64,
    recv_service_update: mpsc::Receiver<i64>,
}

struct IntoOneshot<'a, RecvData>(RecvData, &'a mut Option<oneshot::Receiver<(SeqNo, bool, RecvData)>>);
impl<'a, RecvData> From<IntoOneshot<'a, RecvData>> for Receiver<RecvData> {
    fn from(value: IntoOneshot<'a, RecvData>) -> Self {
        let (rx, tx) = oneshot::channel();
        *value.1 = Some(tx);
        (rx, value.0)
    }
}

impl<SendData, RecvData, const CAP: usize> SeqEx<SendData, RecvData, CAP> {
    pub fn new(retry_interval: i64, initial_seq_no: SeqNo) -> (Self, ServiceState) {
        let (update_queue, recv_service_update) = mpsc::channel(8);
        (
            Self {
                inner: Mutex::new(SeqExInner {
                    seq: crate::no_std::SeqEx::new(retry_interval, initial_seq_no),
                    recv_waiters: 0,
                    reply_waiters: false,
                }),
                wait_on_recv: Notify::new(),
                wait_on_reply: Notify::new(),
                update_queue,
            },
            ServiceState { next_service_timestamp: i64::MAX, recv_service_update },
        )
    }
    pub fn new_default() -> (Self, ServiceState) {
        Self::new(DEFAULT_RESEND_INTERVAL_MS, DEFAULT_INITIAL_SEQ_NO)
    }

    fn notify_reply(&self, mut inner: MutexGuard<'_, SeqExInner<SendData, RecvData, CAP>>) {
        if inner.reply_waiters {
            inner.reply_waiters = false;
            drop(inner);
            self.wait_on_reply.notify_waiters();
        }
    }
    fn receive_inner<TL: TokioLayer<SendData = SendData>>(
        &self,
        tl: TL,
        packet: Packet<IntoOneshot<'_, RecvData>>,
    ) -> Result<(ReplyGuard<'_, TL, SendData, RecvData, CAP>, RecvData), Option<AsyncRecvError>> {
        let mut inner = self.inner.lock().unwrap();
        return match inner.seq.try_receive_raw(tl, packet) {
            Ok((recv_data, do_pump)) => {
                // pump first, handle return value second.
                let mut total_recv = 1;
                if do_pump {
                    while let Ok((data, do_pump)) = inner.seq.try_pump_raw() {
                        total_recv += 1;
                        match data {
                            RecvOkRaw::Payload { reply_no, seq_cst, recv_data } => {
                                if recv_data.0.send((reply_no, seq_cst, recv_data.1)).is_err() {
                                    // Send an ack if no one is receiving the reply on the other end.
                                    // Could occur if the future holding the receiver is dropped.
                                    inner.seq.ack_raw(tl, reply_no, seq_cst);
                                }
                            }
                            RecvOkRaw::Reply { reply_no, seq_cst, recv_data, send_data } => {
                                if send_data.0.send(Some((reply_no, seq_cst, recv_data.1))).is_err() {
                                    inner.seq.ack_raw(tl, reply_no, seq_cst);
                                }
                            }
                            RecvOkRaw::Ack { send_data: (tx, _) } => {
                                let _ = tx.send(None);
                            }
                        }
                        if !do_pump {
                            break;
                        }
                    }
                }

                let ret = match recv_data {
                    RecvOkRaw::Payload { reply_no, seq_cst, recv_data } => Ok((ReplyGuard::new(self, tl, reply_no, seq_cst), recv_data.0)),
                    RecvOkRaw::Reply { reply_no, seq_cst, recv_data, send_data: (tx, _) } => {
                        if tx.send(Some((reply_no, seq_cst, recv_data.0))).is_err() {
                            // Send an ack if no one is receiving the reply on the other end.
                            // Could occur if the future holding the receiver is dropped.
                            inner.seq.ack_raw(tl, reply_no, seq_cst);
                        }
                        Err(Some(AsyncRecvError::AsyncReply))
                    }
                    RecvOkRaw::Ack { send_data: (tx, _) } => {
                        let _ = tx.send(None);
                        Err(Some(AsyncRecvError::AsyncReply))
                    }
                };
                if inner.recv_waiters > 0 {
                    let to_wake = inner.recv_waiters.min(total_recv);
                    inner.recv_waiters -= to_wake;
                    drop(inner);
                    for _ in 0..to_wake {
                        self.wait_on_recv.notify_one();
                    }
                }
                ret
            }
            Err(TryRecvError::DroppedTooEarly) => Err(Some(AsyncRecvError::DroppedTooEarly)),
            Err(TryRecvError::DroppedDuplicate) => Err(Some(AsyncRecvError::DroppedDuplicate)),
            Err(TryRecvError::WaitingForRecv) | Err(TryRecvError::WaitingForReply) => Err(None),
        };
    }

    pub async fn receive<TL: TokioLayer<SendData = SendData>>(
        &self,
        tl: TL,
        packet: Packet<RecvData>,
    ) -> Result<(ReplyGuard<'_, TL, SendData, RecvData, CAP>, RecvData), AsyncRecvError> {
        let mut tx = None;
        let packet = packet.map(|r| IntoOneshot(r, &mut tx));
        match self.receive_inner(tl, packet) {
            Ok(ret) => Ok(ret),
            Err(Some(e)) => Err(e),
            Err(None) => {
                if let Some(tx) = tx {
                    let (reply_no, is_holding_lock, data) = tx.await.map_err(|_| AsyncRecvError::SeqExClosed)?;
                    Ok((ReplyGuard::new(self, tl, reply_no, is_holding_lock), data))
                } else {
                    Err(AsyncRecvError::DroppedDuplicate)
                }
            }
        }
    }

    fn send_with_inner<TL: TokioLayer<SendData = SendData>, F: FnOnce(SeqNo) -> Sender<SendData, RecvData>>(
        &self,
        tl: TL,
        seq_cst: bool,
        packet_data: F,
    ) -> Result<Option<i64>, (TryError, F)> {
        let mut inner = self.inner.lock().unwrap();
        let pre_ts = inner.seq.next_service_timestamp;
        let result = inner.seq.try_send_with(tl, seq_cst, packet_data);
        match result {
            Err((TryError::WaitingForRecv, p)) => {
                inner.recv_waiters += 1;
                Err((TryError::WaitingForRecv, p))
            }
            Err((TryError::WaitingForReply, p)) => {
                inner.reply_waiters = true;
                Err((TryError::WaitingForReply, p))
            }
            Ok(()) => Ok((pre_ts != inner.seq.next_service_timestamp).then_some(inner.seq.next_service_timestamp)),
        }
    }

    pub async fn send<TL: TokioLayer<SendData = SendData>>(
        &self,
        tl: TL,
        seq_cst: bool,
        packet_data: SendData,
    ) -> Result<(ReplyGuard<'_, TL, SendData, RecvData, CAP>, RecvData), AsyncError> {
        self.send_with(tl, seq_cst, |_| packet_data).await
    }
    pub async fn send_with<TL: TokioLayer<SendData = SendData>>(
        &self,
        tl: TL,
        seq_cst: bool,
        packet_data: impl FnOnce(SeqNo) -> SendData,
    ) -> Result<(ReplyGuard<'_, TL, SendData, RecvData, CAP>, RecvData), AsyncError> {
        let (rx, tx) = oneshot::channel();
        let mut pf = |s| (rx, packet_data(s));
        loop {
            match self.send_with_inner(tl, seq_cst, pf) {
                Ok(update) => {
                    if let Some(update) = update {
                        let _ = self.update_queue.send(update).await;
                    }
                    let (reply_no, locked, recv_data) = tx.await.map_err(|_| AsyncError::SeqExClosed)?.ok_or(AsyncError::EndOfExchange)?;
                    return Ok((ReplyGuard::new(self, tl, reply_no, locked), recv_data));
                }
                Err((TryError::WaitingForRecv, p)) => {
                    pf = p;
                    self.wait_on_recv.notified().await;
                }
                Err((TryError::WaitingForReply, p)) => {
                    pf = p;
                    self.wait_on_reply.notified().await;
                }
            }
        }
    }

    /// This function must be called with the same ServiceState instance returned upon creation of
    /// the given SeqExTokio instance.
    pub async fn service_task<TL: TokioLayer<SendData = SendData>>(&self, mut tl: TL, state: &mut ServiceState) {
        let mut result = None;
        if state.next_service_timestamp < i64::MAX {
            let diff = state.next_service_timestamp - tl.time();
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
            let mut inner = self.inner.lock().unwrap();
            inner.seq.service(tl);
            state.next_service_timestamp = inner.seq.next_service_timestamp;
        }
    }
}

pub trait TokioLayer: Clone + Copy {
    type SendData;

    fn time(&mut self) -> i64;

    fn send(&mut self, packet: Packet<&Self::SendData>);
}

impl<TL: TokioLayer, RecvData> TransportLayer<Sender<TL::SendData, RecvData>> for TL {
    fn time(&mut self) -> i64 {
        self.time()
    }
    fn send(&mut self, packet: Packet<&Sender<TL::SendData, RecvData>>) {
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
    type SendData = Payload;

    fn time(&mut self) -> i64 {
        self.time.elapsed().as_millis() as i64
    }
    fn send(&mut self, packet: Packet<&Payload>) {
        let _ = self.channel.try_send(packet.cloned());
    }
}
