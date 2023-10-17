use std::{
    sync::{
        mpsc::{channel, Receiver, Sender},
        Condvar, Mutex, MutexGuard,
    },
    time::Instant,
};

use crate::{
    no_std::RecvOkRaw,
    error::{RecvError, TryError},
    Packet, SeqNo, TransportLayer, DEFAULT_INITIAL_SEQ_NO, DEFAULT_RESEND_INTERVAL_MS, DEFAULT_WINDOW_CAP,
};

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
    wait_on_reply_sender: Condvar,
    wait_on_reply_receiver: Condvar,
}
struct SeqExInner<SendData, RecvData, const CAP: usize> {
    seq: crate::no_std::SeqEx<SendData, RecvData, CAP>,
    recv_waiters: usize,
    reply_sender_waiters: bool,
    reply_receiver_waiters: bool,
}
/// A guard type that allows Rust's borrow-checker to guarantee that the invariants of the SEQEX
/// protocol are maintained.
///
/// Every time a data-containing packet is received by SEQEX, it must be replied to by either an ack,
/// or a reply pacjet, but never both. This guard is created when data-containing packet is received,
/// and when it is dropped it will send an ack. However this guard contains the function `reply`.
/// This function consumes the guard and will cause a reply packet to be sent instead of an ack.
///
/// In addition, if the data-containing packet was also a SeqCst packet. This guard acts like a
/// `MutexGuard`. All other received SeqCst packets will be blocked until this guard is dropped or
/// consumed.
///
/// SeqCst packets almost always require a critical section to be processed correctly, otherwise
/// their in-order guarantee would be rendered pointless because of CPU scheduling non-determinism.
/// Reply guard make sure these critical sections are exist by default.
pub struct ReplyGuard<'a, TL: TransportLayer<SendData>, SendData, RecvData, const CAP: usize = DEFAULT_WINDOW_CAP> {
    seq: &'a SeqEx<SendData, RecvData, CAP>,
    tl: TL,
    reply_no: SeqNo,
    is_holding_lock: bool,
    has_replied: bool,
}
impl<'a, TL: TransportLayer<SendData>, SendData, RecvData, const CAP: usize> ReplyGuard<'a, TL, SendData, RecvData, CAP> {
    /// Returns a reference to the `TransportLayer` instance this guard was created with.
    pub fn get_tl(&self) -> &TL {
        &self.tl
    }
    /// Returns a mutable reference to the `TransportLayer` instance this guard was created with.
    ///
    /// Keep in mind that this cannot be used to change how replies and acks are resent, since
    /// resends are handled with a separate instance of `TransportLayer` passed to `SeqEx::service`.
    pub fn get_tl_mut(&mut self) -> &mut TL {
        &mut self.tl
    }
    /// Returns whether or not `ack` has already been called on this reply guard instance.
    pub fn has_replied(&self) -> bool {
        self.has_replied
    }
    /// Returns whether or nor this reply guard is for a SeqCst packet, and therefore is
    /// holding a lock, preventing other SeqCst packets from being processed yet.
    pub fn is_seq_cst(&self) -> bool {
        self.is_holding_lock
    }
    /// Cause this reply guard to immediately send an ack to the remote peer,
    /// preventing it from being used to send a reply.
    ///
    /// This allows advanced users to decouple locking the critical section for SeqCst packets
    /// from sending acks to received packets. It is not recommended to use this function
    /// except for this purpose. `drop` this reply guard instead.
    pub fn ack(&mut self) {
        if !self.has_replied {
            self.has_replied = true;
            let mut inner = self.seq.inner.lock().unwrap();
            inner.seq.ack_raw(self.tl, self.reply_no, self.is_holding_lock);
        }
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
    ///
    /// # Panic
    /// This function will panic if `ack` has been called previously.
    pub fn reply_with(mut self, seq_cst: bool, packet_data: impl FnOnce(SeqNo, SeqNo) -> SendData) -> Option<i64> {
        assert!(!self.has_replied, "Cannot reply after an ack has been sent");
        self.has_replied = true;
        let mut inner = self.seq.inner.lock().unwrap();
        let pre_nst = inner.seq.next_service_timestamp;
        let seq_no = inner.seq.seq_no();
        inner
            .seq
            .reply_raw(self.tl, self.reply_no, self.is_holding_lock, seq_cst, packet_data(seq_no, self.reply_no));
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
    ///
    /// # Panic
    /// This function will panic if `ack` has been called previously.
    pub fn reply(self, seq_cst: bool, packet_data: SendData) -> Option<i64> {
        self.reply_with(seq_cst, |_, _| packet_data)
    }
    /// Break down a `ReplyGuard` into its primitive components, without causing it to send an ack
    /// or reply to the remote peer.
    ///
    /// The first return value is the packet reply number, and the second is the return value of
    /// `is_seq_cst`, which states whether or not this `ReplyGuard` is holding a lock.
    ///
    /// This can be used in combination with `from_components` to move a `ReplyGuard` to a different
    /// thread.
    ///
    /// # Safety
    /// This function must never be called on a `ReplyGuard` that has had `ack` called on it.
    ///
    /// The caller must guarantee that `ReplyGuard::from_components` is eventually called on
    /// the returned values.
    ///
    /// If these invariants are not maintained protocol deadlock is likely to occur, which will quickly
    /// be followed by lock-based deadlock.
    pub unsafe fn to_components(self) -> (SeqNo, bool) {
        debug_assert!(!self.has_replied, "Cannot break down a ReplyGuard after an ack has been sent");
        let ret = (self.reply_no, self.is_holding_lock);
        core::mem::forget(self);
        ret
    }
    fn new(seq: &'a SeqEx<SendData, RecvData, CAP>, tl: TL, reply_no: SeqNo, is_holding_lock: bool) -> Self {
        ReplyGuard { seq, tl, reply_no, is_holding_lock, has_replied: false }
    }
    /// Constructs a `ReplyGuard` object from the raw components returned by
    /// `ReplyGuard::to_components`.
    ///
    /// # Safety
    /// The caller must always pass values for `reply_no` and `is_holding_lock` that were
    /// returned by `ReplyGuard::to_components`. Otherwise undefined behavior will occur.
    pub unsafe fn from_components(seq: &'a SeqEx<SendData, RecvData, CAP>, tl: TL, reply_no: SeqNo, is_holding_lock: bool) -> Self {
        Self::new(seq, tl, reply_no, is_holding_lock)
    }
}
impl<'a, TL: TransportLayer<SendData>, SendData, RecvData, const CAP: usize> Drop for ReplyGuard<'a, TL, SendData, RecvData, CAP> {
    fn drop(&mut self) {
        let mut inner = self.seq.inner.lock().unwrap();
        if !self.has_replied {
            self.has_replied = true;
            inner.seq.ack_raw(self.tl, self.reply_no, self.is_holding_lock);
        }
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
            }),
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
    ) -> Result<(RecvOk<'_, TL, SendData, RecvData, CAP>, bool), RecvError> {
        let mut inner = self.inner.lock().unwrap();
        match inner.seq.receive_raw(tl, packet) {
            Ok((r, do_pump)) => {
                self.notify_recv(inner);
                Ok((RecvOk::from_raw(self, tl, r), do_pump))
            }
            Err(e) => Err(e),
        }
    }
    /// A SEQEX packet was just received and deserialized from the transport layer.
    /// Passing it to this function officially writes it to the receive window so that it can be
    /// eventually processed with the SEQEX transport guarantees.
    ///
    /// This function may block to preserve lossless or in-order transport.
    ///
    /// If this function returns `Some`, the contained boolean is true if `SeqEx::pump` should also
    /// be called, because there are packets ready to be processed in the receive window.
    pub fn receive<TL: TransportLayer<SendData>>(&self, tl: TL, packet: Packet<RecvData>) -> Option<(RecvOk<'_, TL, SendData, RecvData, CAP>, bool)> {
        let result = self.try_receive(tl, packet);
        if let Err(RecvError::WaitingForReply) = result {
            self.pump(tl)
        } else {
            result.ok()
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
    /// It will never block if the receive window is empty.
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
                    inner = self.wait_on_reply_receiver.wait(inner).unwrap();
                }
            }
        }
    }
    /// Similar to `SeqEx::receive`, except this function will return an iterator which
    /// automatically calls `SeqEx::pump` as many times as needed to empty the receive window.
    pub fn receive_all<TL: TransportLayer<SendData>>(&self, tl: TL, packet: Packet<RecvData>) -> RecvIter<'_, TL, SendData, RecvData, CAP> {
        let ret = self.receive(tl, packet);
        if let Some((first, do_pump)) = ret {
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
        packet_data: F,
    ) -> Result<Option<i64>, (TryError, F)> {
        let mut inner = self.inner.lock().unwrap();
        let pre_nst = inner.seq.next_service_timestamp;
        inner.seq.try_send_with(tl, seq_cst, packet_data).map(|()| {
            let nst = inner.seq.next_service_timestamp;
            (pre_nst > nst).then_some(nst)
        })
    }
    /// Non-blocking variant of `SeqEx::send`.
    /// See the documentation for `SeqEx::send` for more information.
    ///
    /// NOTE: This function can block for a short period of time to lock an internal mutex.
    ///
    /// A return value of `Ok` contains the timestamp of when `service_scheduled` should be called next,
    /// only if it has decrease. This can be safely ignored if `service_scheduled` is not being used.
    pub fn try_send<TL: TransportLayer<SendData>>(&self, tl: TL, seq_cst: bool, packet_data: SendData) -> Result<Option<i64>, (TryError, SendData)> {
        let mut inner = self.inner.lock().unwrap();
        let pre_nst = inner.seq.next_service_timestamp;
        inner.seq.try_send(tl, seq_cst, packet_data).map(|()| {
            let nst = inner.seq.next_service_timestamp;
            (pre_nst > nst).then_some(nst)
        })
    }

    /// Variant of `SeqEx::send` that allows for a provided function to write data to the send
    /// window.
    /// See the documentation for `SeqEx::send` for more information.
    ///
    /// This function receives the packet sequence number the packet will be assigned.
    /// All calls to `TransportLayer::send` involving this packet will receive this exact same
    /// sequence number.
    pub fn send_with<TL: TransportLayer<SendData>>(&self, tl: TL, seq_cst: bool, mut packet_data: impl FnOnce(SeqNo) -> SendData) -> Option<i64> {
        let mut inner = self.inner.lock().unwrap();
        let mut pre_nst = inner.seq.next_service_timestamp;
        while let Err((e, p)) = inner.seq.try_send_with(tl, seq_cst, packet_data) {
            packet_data = p;
            match e {
                TryError::WaitingForRecv => {
                    inner.recv_waiters += 1;
                    inner = self.wait_on_recv.wait(inner).unwrap();
                    pre_nst = inner.seq.next_service_timestamp;
                }
                TryError::WaitingForReply => {
                    inner.reply_sender_waiters = true;
                    inner = self.wait_on_reply_sender.wait(inner).unwrap();
                    pre_nst = inner.seq.next_service_timestamp;
                }
            }
        }
        let nst = inner.seq.next_service_timestamp;
        (pre_nst > nst).then_some(nst)
    }
    /// Add the given `packet_data` to the send window, to be sent immediately, and to be resent
    /// if the payload is not successfully received by the remote peer.
    ///
    /// If the either send or receive window is full, this function will block until they are not.
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
    pub fn send<TL: TransportLayer<SendData>>(&self, tl: TL, seq_cst: bool, mut packet_data: SendData) -> Option<i64> {
        let mut inner = self.inner.lock().unwrap();
        let mut pre_nst = inner.seq.next_service_timestamp;
        while let Err((e, p)) = inner.seq.try_send(tl, seq_cst, packet_data) {
            packet_data = p;
            match e {
                TryError::WaitingForRecv => {
                    inner.recv_waiters += 1;
                    inner = self.wait_on_recv.wait(inner).unwrap();
                    pre_nst = inner.seq.next_service_timestamp;
                }
                TryError::WaitingForReply => {
                    inner.reply_sender_waiters = true;
                    inner = self.wait_on_reply_sender.wait(inner).unwrap();
                    pre_nst = inner.seq.next_service_timestamp;
                }
            }
        }
        let nst = inner.seq.next_service_timestamp;
        (pre_nst > nst).then_some(nst)
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
    pub fn service_scheduled(&self, mut tl: impl TransportLayer<SendData>) -> i64 {
        let mut inner = self.inner.lock().unwrap();
        let current_time = tl.time();
        let mut iter = None;
        while let Some(p) = inner.seq.service_direct(current_time, &mut iter) {
            tl.send(p)
        }
        inner.seq.next_service_timestamp
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
