use std::{
    sync::{
        mpsc::{channel, Receiver, Sender},
        Condvar, Mutex, MutexGuard,
    },
    time::{Duration, Instant},
};

use crate::{no_std::RecvOkRaw, Packet, SeqNo, TransportLayer, DEFAULT_INITIAL_SEQ_NO, DEFAULT_RESEND_INTERVAL_MS, DEFAULT_WINDOW_CAP};

pub use crate::no_std::TryError;

/// The core thread-safe datastructure which manages the SEQEX protocol.
///
/// `SendData` is some collection of data chosen by the user to define the contents, or payload, of
/// a packet. It can also be made to contain additional associated data that is not sent to the peer,
/// but instead is retained by SEQEX to locally track the current state of some exchange with the
/// remote peer.
/// Commonly uses include placing an enum within `SendData` that defines an asynchronous state machine.
/// The kind of state machine Rust would automatically generate to implement async-await code.
///
/// Any instance of `SendData` passed into a `SeqEx` method can only be dropped if the
/// entire `SeqEx` instance is dropped, otherwise they will eventually be returned by some future
/// call into a `SeqEx` method.
///
/// `RecvData` is another collection of data chosen by the user to define a payload of data that was
/// just received from the remote peer. Often this is just set to `Vec<u8>`, but it can also be
/// set to some other data-owning type. `RecvData` can also be made to contain packet metadata such
/// as what IP address and port the payload of data was received over.
pub struct SeqEx<SendData, RecvData, const CAP: usize = DEFAULT_WINDOW_CAP> {
    inner: Mutex<SeqExInner<SendData, RecvData, CAP>>,
    wait_on_recv: Condvar,
    wait_on_reply: Condvar,
}
struct SeqExInner<SendData, RecvData, const CAP: usize> {
    seq: crate::no_std::SeqEx<SendData, RecvData, CAP>,
    recv_waiters: usize,
    reply_sender_waiters: bool,
    reply_receiver_waiters: bool,
    closed: bool,
}
/// A guard type that allows Rust's borrow-checker to guarantee that the invariants of the SEQEX
/// protocol are maintained.
///
/// Every time a data-containing packet is received by SEQEX, it must be replied to by either an ack,
/// or a reply packet, but never both. This guard is created when data-containing packet is received,
/// and when it is dropped it will send an ack. However this guard contains the function `reply`.
/// This function consumes the guard and will cause a reply packet to be sent instead of an ack.
///
/// In addition, if the data-containing packet was also a SeqCst packet, then this guard will act
/// like a `MutexGuard`. All other received SeqCst packets will be blocked until this guard is
/// dropped.
/// This creates a critical section for SeqCst packets that guarantees they are processed in the
/// exact same order they were sent.
///
/// SeqCst packets almost always require a critical section to be processed correctly, otherwise
/// their in-order guarantee would be rendered pointless because of CPU scheduling non-determinism.
pub struct ReplyGuard<'a, TL: TransportLayer<SendData>, SendData, RecvData, const CAP: usize = DEFAULT_WINDOW_CAP> {
    seq: &'a SeqEx<SendData, RecvData, CAP>,
    tl: TL,
    reply_no: SeqNo,
    is_holding_lock: bool,
}

/// A lock guard for maintaining the critical section for processing SeqCst packets. SeqCst packets
/// can only enter this critical section in the order they were sent by the remote peer.
pub struct SeqCstGuard<'a, SendData, RecvData, const CAP: usize = DEFAULT_WINDOW_CAP> {
    seq: &'a SeqEx<SendData, RecvData, CAP>,
}

impl<'a, TL: TransportLayer<SendData>, SendData, RecvData, const CAP: usize> ReplyGuard<'a, TL, SendData, RecvData, CAP> {
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
    /// Similar to `ReplyGuard::reply`, except the provided function is called to produce the data
    /// to write to the send window.
    ///
    /// This function receives as its first argument the packet sequence number, and as its second
    /// number the packet reply number. All calls to `TransportLayer::send` involving this packet
    /// will receive the exact same sequence and reply number.
    ///
    /// Returns the timestamp of when `service_scheduled` should be called next, only if it has decrease.
    /// This can be safely ignored if `service_scheduled` is not being used.
    pub fn reply_with(self, seq_cst: bool, f: impl FnOnce(SeqNo, SeqNo) -> SendData) -> Option<i64> {
        let mut inner = self.seq.inner.lock().unwrap();
        if inner.closed {
            return None;
        }
        let pre_nst = inner.seq.next_service_timestamp;
        let seq_no = inner.seq.seq_no();
        inner
            .seq
            .reply_raw(self.tl, self.reply_no, self.is_holding_lock, seq_cst, f(seq_no, self.reply_no));
        let nst = inner.seq.next_service_timestamp;
        self.seq.notify_reply(inner);
        core::mem::forget(self);
        (pre_nst > nst).then_some(nst)
    }
    /// Consume this reply guard to add `packet_data` to the send window and immediately send it
    /// as a reply to the remote peer. Similar to `SeqEx::send`, except the remote peer will be
    /// explicitly informed that this packet is indeed a reply to a packet they sent.
    ///
    /// Returns the timestamp of when `service_scheduled` should be called next, only if it has decrease.
    /// This can be safely ignored if `service_scheduled` is not being used.
    ///
    /// If you need to reply more than once, say to fragment a large file, then include in your
    /// first reply some identifier, and then `send` all fragments with the same included identifier.
    /// The identifier will tell the remote peer which packets contain fragments of the file,
    /// and since each fragment will be received in order it will be trivial for them to reconstruct
    /// the original file.
    pub fn reply(self, seq_cst: bool, packet_data: SendData) -> Option<i64> {
        self.reply_with(seq_cst, |_, _| packet_data)
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
        if !inner.closed {
            inner.seq.ack_raw(self.tl, self.reply_no, false);
        }
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
    /// Send a reply to the remote peer that was created by the provided function.
    /// Similar to `ReplyGuard::reply_with`, except this will not drop the SeqCst lock if
    /// this guard is holding it.
    ///
    /// See the documentation for `ReplyGuard::reply_stay_locked` for more information.
    pub fn reply_with_stay_locked(
        self,
        seq_cst: bool,
        f: impl FnOnce(SeqNo, SeqNo) -> SendData,
    ) -> (Option<SeqCstGuard<'a, SendData, RecvData, CAP>>, Option<i64>) {
        let mut inner = self.seq.inner.lock().unwrap();
        if inner.closed {
            return (self.consume_lock(inner), None);
        }
        let pre_nst = inner.seq.next_service_timestamp;
        let seq_no = inner.seq.seq_no();
        inner.seq.reply_raw(self.tl, self.reply_no, false, seq_cst, f(seq_no, self.reply_no));
        let nst = inner.seq.next_service_timestamp;

        (self.consume_lock(inner), (pre_nst > nst).then_some(nst))
    }
    /// Send a reply to the remote peer, similar to `ReplyGuard::reply`, except without dropping the
    /// SeqCst lock.
    ///
    /// If this guard was holding the SeqCst lock, then it will return a `SeqCstGuard`, which will
    /// continue to hold the lock until it is dropped. If this guard was not holding the lock, it
    /// will return `None` instead.
    ///
    /// This allows advanced users to de-couple locking the critical section for SeqCst packets
    /// from sending acks to received packets.
    /// In cases where the critical section has to be maintained for a long period of time this can
    /// save bandwidth that would otherwise be wasted on resends.
    /// It is not recommended to use this function except for this purpose.
    ///
    /// Casual users are recommended to use `ReplyGuard::reply` instead of calling this function.
    ///
    /// The second return value is again the timestamp of when `service_scheduled` should be called
    /// next, only if it has decrease.
    pub fn reply_stay_locked(self, seq_cst: bool, packet_data: SendData) -> (Option<SeqCstGuard<'a, SendData, RecvData, CAP>>, Option<i64>) {
        self.reply_with_stay_locked(seq_cst, |_, _| packet_data)
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
impl<'a, TL: TransportLayer<SendData>, SendData, RecvData, const CAP: usize> Drop for ReplyGuard<'a, TL, SendData, RecvData, CAP> {
    fn drop(&mut self) {
        let mut inner = self.seq.inner.lock().unwrap();
        if !inner.closed {
            inner.seq.ack_raw(self.tl, self.reply_no, self.is_holding_lock);
        }
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
impl<'a, TL: TransportLayer<SendData>, SendData, RecvData, const CAP: usize> std::fmt::Debug for ReplyGuard<'a, TL, SendData, RecvData, CAP> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ReplyGuard")
            .field("reply_no", &self.reply_no)
            .field("is_holding_lock", &self.is_holding_lock)
            .finish()
    }
}

/// When a packet received through `SeqEx` is ready to be processed,
/// it is returned as an instance of this enum.
///
/// This enum specifies what kind of packet was received, and any additional data associated with the packet.
pub enum RecvOk<'a, TL: TransportLayer<SendData>, SendData, RecvData, const CAP: usize = DEFAULT_WINDOW_CAP> {
    /// The received packet is a payload.
    Payload {
        /// This type of packet can be optionally replied to, otherwise we need to send an ack.
        /// This guard instance guarantees exactly one of those happens.
        /// See the documentation of `ReplyGuard` for more information.
        reply_guard: ReplyGuard<'a, TL, SendData, RecvData, CAP>,
        /// The data associated with the packet we just received from the remote peer.
        /// It was moved from the receive window to this enum.
        recv_data: RecvData,
    },
    /// The received packet is a reply to some packet we sent previously.
    Reply {
        /// This type of packet can be optionally replied to, otherwise we need to send an ack.
        /// This guard instance guarantees exactly one of those happens.
        /// See the documentation of `ReplyGuard` for more information.
        reply_guard: ReplyGuard<'a, TL, SendData, RecvData, CAP>,
        /// The data associated with the packet we just received from the remote peer.
        /// It was moved from the receive window to this enum.
        recv_data: RecvData,
        /// The data associated with a packet we sent to the remote peer.
        /// The received packet is a reply to that packet.
        /// As a result, this data was moved from the `SeqEx` send window to this enum.
        send_data: SendData,
    },
    /// The received packet is an acknowledgment of some packet we sent previously.
    Ack {
        /// The data associated with a packet we sent to the remote peer.
        /// The received packet was an ack of this packet.
        /// As a result, this data was moved from the `SeqEx` send window to this enum.
        send_data: SendData,
    },
}
crate::no_std::impl_recvok!(RecvOk, &'a SeqEx<SendData, RecvData, CAP>);

/// An iterator which will automatically call either `SeqEx::pump` or `SeqEx::try_pump` to go
/// through all packets that can currently be processed.
pub struct RecvIter<'a, TL: TransportLayer<SendData>, SendData, RecvData, const CAP: usize = DEFAULT_WINDOW_CAP> {
    seq: Option<&'a SeqEx<SendData, RecvData, CAP>>,
    tl: TL,
    first: Option<RecvOk<'a, TL, SendData, RecvData, CAP>>,
    blocking: bool,
}

macro_rules! send {
    ($self:ident, $tl:ident, $seq_cst:ident, $data:ident, $try_send:tt) => {{
        let mut inner = $self.inner.lock().unwrap();
        if inner.closed {
            return Err(ClosedError);
        }
        let mut pre_nst = inner.seq.next_service_timestamp;
        while let Err((e, d)) = inner.seq.$try_send($tl, $seq_cst, $data) {
            $data = d;
            match e {
                TryError::WaitingForRecv => {
                    inner.recv_waiters += 1;
                    inner = $self.wait_on_recv.wait(inner).unwrap();
                    inner.recv_waiters -= 1;
                    pre_nst = inner.seq.next_service_timestamp;
                }
                TryError::WaitingForReply => {
                    inner.reply_sender_waiters = true;
                    inner = $self.wait_on_reply.wait(inner).unwrap();
                    pre_nst = inner.seq.next_service_timestamp;
                }
            }
            if inner.closed {
                return Err(ClosedError);
            }
        }
        let nst = inner.seq.next_service_timestamp;
        Ok((pre_nst > nst).then_some(nst))
    }};
    ($self:ident, $tl:ident, $seq_cst:ident, $data:ident, $try_send:tt, $timeout:ident) => {{
        let mut inner = $self.inner.lock().unwrap();
        if inner.closed {
            return Err((TrySendError::Closed, $data));
        }
        let mut pre_nst = inner.seq.next_service_timestamp;
        let mut start_time = $tl.time();
        let end_time = start_time.saturating_add($timeout);
        while let Err((e, p)) = inner.seq.$try_send($tl, $seq_cst, $data) {
            $data = p;
            match e {
                TryError::WaitingForRecv => {
                    inner.recv_waiters += 1;
                    (inner, _) = $self
                        .wait_on_recv
                        .wait_timeout(inner, Duration::from_millis((end_time - start_time) as u64))
                        .unwrap();
                    start_time = $tl.time();
                    inner.recv_waiters -= 1;
                    pre_nst = inner.seq.next_service_timestamp;
                    if end_time <= start_time {
                        return Err((TrySendError::WaitingForRecv, $data));
                    }
                }
                TryError::WaitingForReply => {
                    inner.reply_sender_waiters = true;
                    (inner, _) = $self
                        .wait_on_reply
                        .wait_timeout(inner, Duration::from_millis((end_time - start_time) as u64))
                        .unwrap();
                    start_time = $tl.time();
                    pre_nst = inner.seq.next_service_timestamp;
                    if end_time <= start_time {
                        return Err((TrySendError::WaitingForReply, $data));
                    }
                }
            }
            if inner.closed {
                return Err((TrySendError::Closed, $data));
            }
        }
        let nst = inner.seq.next_service_timestamp;
        Ok((pre_nst > nst).then_some(nst))
    }};
}

impl<SendData, RecvData, const CAP: usize> SeqEx<SendData, RecvData, CAP> {
    /// Creates a new instance of SeqEx.
    ///
    /// `retry_interval` sets the interval at which SEQEX will resend unacknowledged packets.
    ///
    /// `initial_seq_no` sets the initial sequence number for this SEQEX session. This number
    /// **must** be synchronized with the remote peer.
    /// Setting `initial_seq_no` to a static value is the easiest way to synchronize it with the
    /// emote peer, but this is only recommended if running SEQEX on top of an encrypted tunnel.
    /// Otherwise it should be randomized.
    pub fn new(retry_interval: i64, initial_seq_no: SeqNo) -> Self {
        Self {
            inner: Mutex::new(SeqExInner {
                seq: crate::no_std::SeqEx::new(retry_interval, initial_seq_no),
                recv_waiters: 0,
                reply_sender_waiters: false,
                reply_receiver_waiters: false,
                closed: false,
            }),
            wait_on_recv: Condvar::default(),
            wait_on_reply: Condvar::default(),
        }
    }
    fn notify_reply(&self, mut inner: MutexGuard<'_, SeqExInner<SendData, RecvData, CAP>>) {
        if inner.reply_sender_waiters || inner.reply_receiver_waiters {
            inner.reply_sender_waiters = false;
            drop(inner);
            self.wait_on_reply.notify_all();
        }
    }
    fn notify_recv(&self, inner: MutexGuard<'_, SeqExInner<SendData, RecvData, CAP>>) {
        if inner.recv_waiters > 0 {
            drop(inner);
            self.wait_on_recv.notify_one();
        }
    }
    /// A SEQEX packet was just received and deserialized from the transport layer.
    /// Passing it to this function officially writes it to the receive window so that it can be
    /// eventually processed with the SEQEX transport guarantees.
    ///
    /// This function can only block to lock an internal mutex.
    ///
    /// If this function returns `Ok`, the contained boolean is true if `SeqEx::pump` should also
    /// be called, because there are packets ready to be processed in the receive window.
    pub fn try_receive<TL: TransportLayer<SendData>>(
        &self,
        tl: TL,
        packet: Packet<RecvData>,
    ) -> Result<(RecvOk<'_, TL, SendData, RecvData, CAP>, bool), TryRecvError> {
        let mut inner = self.inner.lock().unwrap();
        if inner.closed {
            return Err(TryRecvError::Closed);
        }
        match inner.seq.try_receive_raw(tl, packet) {
            Ok((r, do_pump)) => {
                self.notify_recv(inner);
                Ok((RecvOk::from_raw(self, tl, r), do_pump))
            }
            Err(e) => Err(match e {
                crate::no_std::TryRecvError::DroppedTooEarly
                | crate::no_std::TryRecvError::DroppedDuplicate
                | crate::no_std::TryRecvError::WaitingForRecv => TryRecvError::OutOfSequence,
                crate::no_std::TryRecvError::WaitingForReply => TryRecvError::WaitingForReply,
            }),
        }
    }
    /// A SEQEX packet was just received and deserialized from the transport layer.
    /// Passing it to this function officially writes it to the receive window so that it can be
    /// eventually processed with the SEQEX transport guarantees.
    ///
    /// This function may block to preserve lossless or in-order transport.
    /// In particular this function will block to guarantee SeqCst packets are processed in the same
    /// order that they were sent. See `ReplyGuard` for more information.
    ///
    /// If we are waiting to receive packets from the remote peer, this function will not block
    /// and instead return `None`.
    ///
    /// This function returns an option, where the first contained value is received data that
    /// the caller can now process, and the second value is a boolean specifying if there is yet
    /// more received data to be processed. This boolean is true if `SeqEx::pump` should be called,
    /// because there are more packets ready to be processed in the receive window.
    pub fn receive<TL: TransportLayer<SendData>>(
        &self,
        tl: TL,
        packet: Packet<RecvData>,
    ) -> Result<(RecvOk<'_, TL, SendData, RecvData, CAP>, bool), RecvError> {
        match self.try_receive(tl, packet) {
            Ok(r) => Ok(r),
            Err(e) => match e {
                TryRecvError::WaitingForReply => self.pump(tl).ok_or(RecvError::OutOfSequence),
                TryRecvError::OutOfSequence => Err(RecvError::OutOfSequence),
                TryRecvError::Closed => Err(RecvError::Closed),
            },
        }
    }
    /// Non-blocking variant of `SeqEx::pump`.
    /// See the documentation for `SeqEx::pump` for more information.
    ///
    /// NOTE: This function can block for a short period of time to lock an internal mutex.
    pub fn try_pump<TL: TransportLayer<SendData>>(&self, tl: TL) -> Result<(RecvOk<'_, TL, SendData, RecvData, CAP>, bool), TryError> {
        let mut inner = self.inner.lock().unwrap();
        match inner.seq.try_pump_raw() {
            Ok((r, do_pump)) => {
                self.notify_recv(inner);
                Ok((RecvOk::from_raw(self, tl, r), do_pump))
            }
            Err(e) => Err(e),
        }
    }
    /// Check if there are any packets in the receive window that can be processed.
    ///
    /// This function may block to preserve lossless or in-order transport.
    /// It will never block if the receive window is empty (a.k.a. there is no data to receive).
    ///
    /// If this function returns `Some`, the contained boolean is true if `SeqEx::pump` should be
    /// called another time, because there are still packets ready to be processed in the receive window.
    ///
    /// It is recommended to assign the job of pumping `SeqEx` to a different thread so packets can
    /// be processed in parallel.
    pub fn pump<TL: TransportLayer<SendData>>(&self, tl: TL) -> Option<(RecvOk<'_, TL, SendData, RecvData, CAP>, bool)> {
        let mut inner = self.inner.lock().unwrap();
        // Enforce that only one thread may wait to pump at a time.
        if inner.reply_receiver_waiters {
            return None;
        }
        loop {
            match inner.seq.try_pump_raw() {
                Ok((r, do_pump)) => {
                    self.notify_recv(inner);
                    return Some((RecvOk::from_raw(self, tl, r), do_pump));
                }
                Err(TryError::WaitingForRecv) => return None,
                Err(TryError::WaitingForReply) => {
                    inner.reply_receiver_waiters = true;
                    inner = self.wait_on_reply.wait(inner).unwrap();
                    inner.reply_receiver_waiters = false;
                }
            }
        }
    }
    /// Similar to `SeqEx::receive`, except this function will return an iterator which
    /// automatically calls `SeqEx::pump` as many times as needed to empty the receive window.
    pub fn receive_all<TL: TransportLayer<SendData>>(&self, tl: TL, packet: Packet<RecvData>) -> RecvIter<'_, TL, SendData, RecvData, CAP> {
        let ret = self.receive(tl, packet);
        if let Ok((first, do_pump)) = ret {
            RecvIter {
                seq: do_pump.then_some(self),
                tl,
                first: Some(first),
                blocking: true,
            }
        } else {
            RecvIter { seq: None, tl, first: None, blocking: true }
        }
    }
    /// Similar to `SeqEx::try_receive`, except this function will return an iterator which
    /// automatically calls `SeqEx::try_pump` as many times as it can before either the receive
    /// window is empty, or one of the calls would block.
    pub fn try_receive_all<TL: TransportLayer<SendData>>(&self, tl: TL, packet: Packet<RecvData>) -> RecvIter<'_, TL, SendData, RecvData, CAP> {
        let ret = self.try_receive(tl, packet);
        if let Ok((first, do_pump)) = ret {
            RecvIter {
                seq: do_pump.then_some(self),
                tl,
                first: Some(first),
                blocking: false,
            }
        } else {
            RecvIter { seq: None, tl, first: None, blocking: false }
        }
    }

    /// Non-blocking variant of `SeqEx::send` that allows for a provided function to write data to
    /// the send window.
    /// See the documentation for `SeqEx::send` for more information.
    ///
    /// This function receives the packet sequence number the packet will be assigned.
    /// All calls to `TransportLayer::send` involving this packet will receive this exact same
    /// sequence number.
    ///
    /// NOTE: This function can block for a short period of time to lock an internal mutex.
    ///
    /// A return value of `Ok` contains the timestamp of when `service_scheduled` should be called next,
    /// only if it has decrease. This can be safely ignored if `service_scheduled` is not being used.
    pub fn try_send_with<TL: TransportLayer<SendData>, F: FnOnce(SeqNo) -> SendData>(
        &self,
        tl: TL,
        seq_cst: bool,
        f: F,
    ) -> Result<Option<i64>, (TrySendError, F)> {
        let mut inner = self.inner.lock().unwrap();
        if inner.closed {
            return Err((TrySendError::Closed, f));
        }
        let pre_nst = inner.seq.next_service_timestamp;
        inner
            .seq
            .try_send_with(tl, seq_cst, f)
            .map(|()| {
                let nst = inner.seq.next_service_timestamp;
                (pre_nst > nst).then_some(nst)
            })
            .map_err(|(e, b)| (e.into(), b))
    }
    /// Non-blocking variant of `SeqEx::send`.
    /// See the documentation for `SeqEx::send` for more information.
    ///
    /// NOTE: This function can block for a short period of time to lock an internal mutex.
    ///
    /// A return value of `Ok` contains the timestamp of when `service_scheduled` should be called next,
    /// only if it has decrease. This can be safely ignored if `service_scheduled` is not being used.
    pub fn try_send<TL: TransportLayer<SendData>>(
        &self,
        tl: TL,
        seq_cst: bool,
        packet_data: SendData,
    ) -> Result<Option<i64>, (TrySendError, SendData)> {
        let mut inner = self.inner.lock().unwrap();
        if inner.closed {
            return Err((TrySendError::Closed, packet_data));
        }
        let pre_nst = inner.seq.next_service_timestamp;
        inner
            .seq
            .try_send(tl, seq_cst, packet_data)
            .map(|()| {
                let nst = inner.seq.next_service_timestamp;
                (pre_nst > nst).then_some(nst)
            })
            .map_err(|(e, b)| (e.into(), b))
    }

    /// Variant of `SeqEx::send` that allows for a provided function to write data to the send
    /// window.
    /// See the documentation for `SeqEx::send` for more information.
    ///
    /// This function receives the packet sequence number the packet will be assigned.
    /// All calls to `TransportLayer::send` involving this packet will receive this exact same
    /// sequence number.
    pub fn send_with<TL: TransportLayer<SendData>>(
        &self,
        tl: TL,
        seq_cst: bool,
        mut f: impl FnOnce(SeqNo) -> SendData,
    ) -> Result<Option<i64>, ClosedError> {
        send!(self, tl, seq_cst, f, try_send_with)
    }
    /// Add the given `packet_data` to the send window, to be sent immediately, and to be resent
    /// if the payload is not successfully received by the remote peer.
    ///
    /// If the either send or receive window is full, this function will block until they are not.
    /// The amount of time this blocks for can depend on how long it takes the remote peer to respond.
    ///
    /// This function can only return `Err(ClosedError)` if the function `SeqEx::close` has
    /// previously been called on this instance of `SeqEx`.
    ///
    /// By default this function guarantees lossless transport. When `seq_cst` is set to true, this
    /// function also guarantees in-order transport.
    ///
    /// Lossless transport guarantees that all payloads will be received by the remote peer exactly
    /// once, but not necessarily in the same order they were sent.
    ///
    /// In-order transport guarantees that all SeqCst payloads are received in the same order that
    /// they were sent.
    ///
    /// Returns the timestamp of when `service_scheduled` should be called next, only if it has decrease.
    /// This can be safely ignored if `service_scheduled` is not being used.
    pub fn send<TL: TransportLayer<SendData>>(&self, tl: TL, seq_cst: bool, mut packet_data: SendData) -> Result<Option<i64>, ClosedError> {
        send!(self, tl, seq_cst, packet_data, try_send)
    }
    /// Variant of `SeqEx::send` that will block at most around `timeout` milliseconds before unblocking.
    ///
    /// A return value of `Err(TrySendError::WaitingForRecv)` signals that this function timed-out
    /// while it was waiting to receive an acknowledgement from the remote peer.
    /// A return value of `Err(TrySendError::WaitingForReply)` signals that this function timed-out
    /// while it was waiting for a delinquent `ReplyGuard` to be dropped.
    /// It is guaranteed that, if this function times-out, at least `timeout` milliseconds have passed.
    /// So this function will not spuriously return.
    ///
    /// This and `send_with_timeout` are the only functions capable of calling `tl.time()`
    /// more than once. So `tl.time()` should be accurate and monotonically increasing.
    pub fn send_timeout<TL: TransportLayer<SendData>>(
        &self,
        mut tl: TL,
        seq_cst: bool,
        timeout: i64,
        mut packet_data: SendData,
    ) -> Result<Option<i64>, (TrySendError, SendData)> {
        send!(self, tl, seq_cst, packet_data, try_send, timeout)
    }
    /// Variant of `SeqEx::send_with` that will block at most around `timeout` milliseconds before unblocking.
    ///
    /// A return value of `Err(TrySendError::WaitingForRecv)` signals that this function timed out
    /// while it was waiting to receive an acknowledgement from the remote peer.
    /// A return value of `Err(TrySendError::WaitingForReply)` signals that this function timed out
    /// while it was waiting for a delinquent `ReplyGuard` to be dropped.
    /// It is guaranteed that, if this function times-out, at least `timeout` milliseconds have passed.
    /// So this function will not spuriously return.
    ///
    /// This and `send_timeout` are the only functions capable of calling `tl.time()`
    /// more than once. So `tl.time()` should be accurate and monotonically increasing.
    pub fn send_with_timeout<TL: TransportLayer<SendData>, F: FnOnce(SeqNo) -> SendData>(
        &self,
        mut tl: TL,
        seq_cst: bool,
        timeout: i64,
        mut f: F,
    ) -> Result<Option<i64>, (TrySendError, F)> {
        send!(self, tl, seq_cst, f, try_send_with, timeout)
    }
    /// Function which handles resending unacknowledged packets.
    /// It returns the duration of time in milliseconds that should be waited
    /// until calling this function again.
    ///
    /// This function should be called repeatedly in a loop, with the loop
    /// sleeping the amount of time specified before calling again.
    pub fn service<TL: TransportLayer<SendData>>(&self, tl: TL) -> i64 {
        self.inner.lock().unwrap().seq.service(tl)
    }
    /// A variant of `SeqEx::service` for advanced users.
    ///
    /// Instead of returning a duration of time, this function returns the exact timestamp at which
    /// this function should be called again, or i64::MAX if the send window is empty and nothing
    /// currently needs to be resent.
    ///
    /// This can be used in combination with the return values of `SeqEx::send` and
    /// `ReplyGuard::reply` to precisely schedule updates to `SeqEx`, allowing
    /// updated to occur much less often.
    ///
    /// However this function can be very difficult to use correctly as it both requires a means
    /// of dynamically scheduling calls, as well as discipline in correctly applying updates to
    /// that schedule whenever `SeqEx::send`, `ReplyGuard::reply` or any of their variants are called.
    ///
    /// It is recommended to just use `SeqEx::service`.
    pub fn service_scheduled(&self, mut tl: impl TransportLayer<SendData>) -> Result<i64, ClosedError> {
        let mut inner = self.inner.lock().unwrap();
        if inner.closed {
            return Err(ClosedError);
        }
        let current_time = tl.time();
        let mut iter = None;
        while let Some(p) = inner.seq.service_direct(current_time, &mut iter) {
            tl.send(p)
        }
        Ok(inner.seq.next_service_timestamp)
    }
    /// When this function is called, sending and receiving packets is immediately disabled for this
    /// instance of `SeqEx`. Nothing else will be disabled, it will still be possible to empty any
    /// packets left in the receive window with `pump`.
    ///
    /// Normally if the send window is full, threads that call into `send` or `send_with` will block
    /// until the remote peer acknowledges some of the packets in the send window.
    /// If the remote peer never sends an acknowledgement, then these threads will be
    /// permanently blocked, unless this function is called.
    ///
    /// This function causes all threads currently waiting for the remote peer to send an ack to
    /// unblock and return an error.
    /// Future calls to `send` or `send_with` will return an error instead of blocking.
    /// All future packets received from the remote peer will be dropped and an error will be returned.
    ///
    /// Calling this function can cause data loss due to packets being dropped from the send window,
    /// but it will maintain state synchronization with the remote peer up to the moment it was
    /// called.
    ///
    /// If you are using functions `send` or `send_with`,
    /// and are communicating with an unreliable peer, then threads permanently blocking on send
    /// is a possibility. To avoid this you must implement some system that will detect if the
    /// remote peer has disconnected and then call this function when.
    pub fn close(&self) {
        // It is not practical to implement this function as a called-on-drop RAII object.
        // Since SEQEX does not specify a protocol for closing a session, we have to allow the user
        // to call this function at any time for any reason.
        // RAII is good for when you need to guarantee some object is alive when a thread has access
        // to it, but this is the opposite of the semantics we want for closing a session.
        // When the user decides to close a session, this instance of `SeqEx` needs to immediately
        // close even while other threads have access to it.
        let mut inner = self.inner.lock().unwrap();
        inner.closed = true;
        self.notify_recv(inner);
    }
    /// Return whether or not `close` has previously been called on this instance of `SeqEx`.
    pub fn is_closed(&self) -> bool {
        let inner = self.inner.lock().unwrap();
        inner.closed
    }
}
impl<SendData, RecvData, const CAP: usize> Default for SeqEx<SendData, RecvData, CAP> {
    fn default() -> Self {
        Self::new(DEFAULT_RESEND_INTERVAL_MS, DEFAULT_INITIAL_SEQ_NO)
    }
}

impl<'a, TL: TransportLayer<SendData>, SendData, RecvData, const CAP: usize> Iterator for RecvIter<'a, TL, SendData, RecvData, CAP> {
    type Item = RecvOk<'a, TL, SendData, RecvData, CAP>;
    fn next(&mut self) -> Option<Self::Item> {
        if let Some(item) = self.first.take() {
            Some(item)
        } else if let Some(origin) = self.seq {
            let ret = if self.blocking {
                origin.pump(self.tl)
            } else {
                origin.try_pump(self.tl).ok()
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

/// An implementation of `TransportLayer` using std::sync::mpsc channels.
/// If you prefer channels instead of callbacks then you can use this.
///
/// If you choose to use this with `SeqEx`, one instance of `MpscTransport` should be created,
/// and only that instance and clones of that instance should be used with `SeqEx`
#[derive(Clone, Debug)]
pub struct MpscTransport<Payload: Clone> {
    /// The sender channel of this instance.
    pub channel: Sender<Packet<Payload>>,
    /// A std::time::Instant for providing answers to `time()` callbacks.
    pub time: Instant,
}

impl<Payload: Clone> MpscTransport<Payload> {
    /// Create a new instance of `MpscTransport`.
    pub fn new() -> (Self, Receiver<Packet<Payload>>) {
        let (send, recv) = channel();
        (Self { channel: send, time: std::time::Instant::now() }, recv)
    }
    /// Create a new instance of `MpscTransport` using the provided `sender`.
    pub fn from_sender(sender: Sender<Packet<Payload>>) -> Self {
        Self { channel: sender, time: std::time::Instant::now() }
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

/// These are the error types that can be returned by `SeqEx::try_receive`.
///
/// Some of these errors specify that SEQEX is waiting on some event to occur before it can proceed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TryRecvError {
    /// Either the receive window is full, or the received packet is SeqCst and cannot enter the
    /// critical section where it is processed. In either case there currently exists some reply
    /// guard that must be dropped or consumed before this packet can be processed.
    ///
    /// If the receive window was full, the packet was dropped.
    /// Otherwise if the packet is SeqCst, then the packet was saved to the receive window.
    WaitingForReply,
    /// The packet was received but cannot be processed yet to preserve losslessness or
    /// in-order transport. No action needs to be taken by the caller.
    OutOfSequence,
    /// This instance of `SeqEx` has been explicitly closed.
    /// It can no longer send or receive data.
    ///
    /// This error can only occur after `SeqEx::close` has been called.
    /// An instance of `SeqEx` will never close by itself, only the caller can close it.
    Closed,
}
/// These are the error types that can be returned by `SeqEx::receive`.
///
/// Some of these errors specify that SEQEX is waiting on some event to occur before it can proceed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecvError {
    /// The packet was received but cannot be processed yet to preserve losslessness or
    /// in-order transport. No action needs to be taken by the caller.
    OutOfSequence,
    /// This instance of `SeqEx` has been explicitly closed.
    /// It can no longer send or receive data.
    ///
    /// This error can only occur after `SeqEx::close` has been called.
    /// An instance of `SeqEx` will never close by itself, only the caller can close it.
    Closed,
}
/// A generic error that can be returned by a `try_send` or `send_timeout` function.
/// They specify what event must occur before a future call to `try_send` or `try_pump` can succeed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrySendError {
    /// The packet could not be sent at this time because the send window is full.
    /// Acknowledgements must be received from the remote peer to empty the send window.
    ///
    /// If this error is returned by a `send_timeout` function, then the function timed-out.
    WaitingForRecv,
    /// Some currently issued reply number must be returned to SEQEX to send either an Ack or a
    /// Reply. If using reply guards, then some currently existing reply guard must be dropped or
    /// consumed.
    /// Until this occurs the packet cannot be sent or processed.
    ///
    /// If this error is returned by a `send_timeout` function, then the function timed-out.
    WaitingForReply,
    /// This instance of `SeqEx` has been explicitly closed.
    /// It can no longer send or receive data.
    ///
    /// This error can only occur after `SeqEx::close` has been called.
    /// An instance of `SeqEx` will never close by itself, only the caller can close it.
    Closed,
}
/// This instance of `SeqEx` has been explicitly closed.
/// It can no longer send or receive data.
///
/// This error can only occur after `SeqEx::close` has been called.
/// An instance of `SeqEx` will never close by itself, only the caller can close it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClosedError;

impl std::fmt::Display for TryRecvError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::OutOfSequence => write!(f, "packet arrived but cannot yet be processed"),
            Self::WaitingForReply => write!(f, "can't process packet until a reply is finished"),
            Self::Closed => write!(f, "can't receive packet because the session was explicitly closed"),
        }
    }
}
impl std::error::Error for TryRecvError {}

impl std::fmt::Display for RecvError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let e: TryRecvError = (*self).into();
        e.fmt(f)
    }
}
impl std::error::Error for RecvError {}

#[cfg(feature = "std")]
impl std::fmt::Display for TrySendError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TrySendError::WaitingForRecv => write!(f, "can't send until another packet is received"),
            TrySendError::WaitingForReply => write!(f, "can't send until a reply is finished"),
            TrySendError::Closed => write!(f, "can't send because the session was explicitly closed"),
        }
    }
}
#[cfg(feature = "std")]
impl std::error::Error for TrySendError {}

impl std::fmt::Display for ClosedError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "can't send because the session was explicitly closed")
    }
}
impl std::error::Error for ClosedError {}

impl From<RecvError> for TryRecvError {
    fn from(value: RecvError) -> Self {
        match value {
            RecvError::OutOfSequence => TryRecvError::OutOfSequence,
            RecvError::Closed => TryRecvError::Closed,
        }
    }
}

impl From<TryError> for TrySendError {
    fn from(value: TryError) -> Self {
        match value {
            TryError::WaitingForRecv => TrySendError::WaitingForRecv,
            TryError::WaitingForReply => TrySendError::WaitingForReply,
        }
    }
}
