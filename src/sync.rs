use std::{
    sync::{
        mpsc::{channel, Receiver, Sender},
        Condvar, Mutex, MutexGuard,
    },
    time::Instant,
};

use crate::{
    no_std::RecvOkRaw,
    result::{RecvError, TryError},
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
    /// resends are handled with a separate instance of `TransportLayer`.
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
    /// # Panic
    /// This function will panic if `ack` has been called previously.
    pub fn reply_with(mut self, seq_cst: bool, packet_data: impl FnOnce(SeqNo, SeqNo) -> SendData) {
        assert!(!self.has_replied, "Cannot reply after an ack has been sent");
        self.has_replied = true;
        let mut inner = self.seq.inner.lock().unwrap();
        let seq_no = inner.seq.seq_no();
        inner
            .seq
            .reply_raw(self.tl, self.reply_no, self.is_holding_lock, seq_cst, packet_data(seq_no, self.reply_no));
        self.seq.notify_reply(inner);
        core::mem::forget(self);
    }
    /// Consume this reply guard to add `packet_data` to the send window and immediately send it
    /// as a reply to the remote peer. Similar to `SeqEx::send`, except the remote peer will be
    /// explicitly informed that this packet is indeed a reply to a packet they sent.
    ///
    /// If you need to reply more than once, say to fragment a large file, then include in your
    /// first reply some identifier, and then `send` all fragments with the same included identifier.
    /// The identifier will tell the remote peer which packets contain fragments of the file,
    /// and since each fragment will be received in order it will be trivial for them to reconstruct
    /// the original file.
    /// # Panic
    /// This function will panic if `ack` has been called previously.
    pub fn reply(self, seq_cst: bool, packet_data: SendData) {
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
crate::no_std::impl_recvok!(RecvOk, &'a SeqEx<SendData, RecvData, CAP>);

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
    ///
    pub fn receive<TL: TransportLayer<SendData>>(&self, tl: TL, packet: Packet<RecvData>) -> Option<(RecvOk<'_, TL, SendData, RecvData, CAP>, bool)> {
        let result = self.try_receive(tl, packet);
        if let Err(RecvError::WaitingForReply) = result {
            self.pump(tl)
        } else {
            result.ok()
        }
    }
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

    pub fn try_send_with<TL: TransportLayer<SendData>, F: FnOnce(SeqNo) -> SendData>(
        &self,
        tl: TL,
        seq_cst: bool,
        packet_data: F,
    ) -> Result<(), (TryError, F)> {
        let mut inner = self.inner.lock().unwrap();
        inner.seq.try_send_with(tl, seq_cst, packet_data)
    }
    pub fn try_send<TL: TransportLayer<SendData>>(&self, tl: TL, seq_cst: bool, packet_data: SendData) -> Result<(), (TryError, SendData)> {
        let mut inner = self.inner.lock().unwrap();
        inner.seq.try_send(tl, seq_cst, packet_data)
    }

    pub fn send_with<TL: TransportLayer<SendData>>(&self, tl: TL, seq_cst: bool, mut packet_data: impl FnOnce(SeqNo) -> SendData) {
        let mut inner = self.inner.lock().unwrap();
        while let Err((e, p)) = inner.seq.try_send_with(tl, seq_cst, packet_data) {
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
    pub fn send<TL: TransportLayer<SendData>>(&self, tl: TL, seq_cst: bool, mut packet_data: SendData) {
        let mut inner = self.inner.lock().unwrap();
        while let Err((e, p)) = inner.seq.try_send(tl, seq_cst, packet_data) {
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

    pub fn service<TL: TransportLayer<SendData>>(&self, tl: TL) -> i64 {
        self.inner.lock().unwrap().seq.service(tl)
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
