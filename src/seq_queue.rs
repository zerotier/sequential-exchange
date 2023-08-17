//! The reference implementation of the **Sequential Exchange Protocol**, or SEP.
//!
//! SEP is a peer-to-peer transport protocol that guarantees packets of data will always be received
//! in the same order they were sent. In addition, it also guarantees the sequential consistency of
//! stateful exchanges between the two communicating peers.
//!
//! A "stateful exchange" is defined here as a sequence of packets, where the first packet
//! initiates the exchange, and all subsequent packets are replies to the previous packet in the
//! exchange.
//!
//! SEP guarantees both peers will agree upon which packets are members of which exchanges,
//! and it guarantees each packet is received by each peer in sequential order.
//!
//! SEP is a tiny, dead simple protocol and we have implemented it here in less than 500 lines of code.
//!
//! ## Why not TCP?
//!
//! TCP only guarantees packets will be received in the same order they were sent.
//! It has no inherent concept of "replying to a packet" and as such it cannot guarantee both sides
//! of a conversation have the same view of any stateful exchanges that take place.
//!
//! TCP is also much higher overhead. It requires a 1.5 RTT handshake to begin any connection,
//! it has a larger amount of metadata that must be transported with packets, and it has quite a few
//! features that slow down runtime regardless of whether or not they are used.
//! A lot of this overhead owes to TCPs sizeable complexity.
//!
//! That being said SEP does lack many of TCP's additional features, such as a dynamic resend timer,
//! keep-alives, and fragmentation. This can be both a pro and a con, as it means there is a
//! lot of efficiency to be gained if these features are not needed or are implemented at a
//! different protocol layer.
//!
//! Neither SEP nor TCP are cryptographically secure.
//!
//! ## Examples
//!

use crate::TransportLayer;

/// A 32-bit sequence number. Packets transported with SEP are expected to contain at least one
/// sequence number, and sometimes two.
/// All packets will either have a seq_no, a reply_no, or both.
pub type SeqNo = u32;

/// The resend interval for a default instance of SeqEx.
pub const DEFAULT_RESEND_INTERVAL_MS: i64 = 200;
/// The initial sequence number for a default instance of SeqEx.
pub const DEFAULT_INITIAL_SEQ_NO: SeqNo = 1;

pub const DEFAULT_WINDOW_CAP: usize = 64;

pub struct SeqEx<SendData, RecvData, const CAP: usize = DEFAULT_WINDOW_CAP> {
    /// The interval at which packets will be resent if they have not yet been acknowledged by the
    /// remote peer.
    /// It can be statically or dynamically set, it is up to the user to decide.
    pub resend_interval: i64,
    pub next_service_timestamp: i64,
    next_send_seq_no: SeqNo,
    pre_recv_seq_no: SeqNo,
    /// This could be made more efficient by changing to SoA format.
    send_window: [Option<SendEntry<SendData>>; CAP],
    recv_window: [Option<RecvEntry<RecvData>>; CAP],
    /// The size of this array determines the maximum number of received packets that the application
    /// may attempt to process concurrently before new received packets start being dropped.
    concurrent_replies: [SeqNo; CAP],
    /// The total number of concurrent replies being processed. When a packet is received, a reply
    /// number is issued for that packet. That reply number reserves resources for itself, so that
    /// when `reply_raw` or `ack_raw` are called with it, they are guaranteed not to fail.
    /// To accomplish this we must track all issued reply numbers.
    concurrent_replies_total: usize,
}

struct RecvEntry<RecvData> {
    seq_no: SeqNo,
    reply_no: Option<SeqNo>,
    data: RecvData,
}

struct SendEntry<SendData> {
    seq_no: SeqNo,
    reply_no: Option<SeqNo>,
    next_resend_time: i64,
    data: SendData,
}

/// The error type for when a packet has been received, but for whatever reason could not be
/// immediately processed.
#[derive(Debug, Clone)]
pub enum Error {
    /// The packet is out-of-sequence. It was either received too soon or too late and so it would be
    /// invalid to process it right now. No action needs to be taken by the caller.
    OutOfSequence,
    /// The Send Window is currently full. The received packet cannot be processed right now because
    /// it could cause the send window to overflow. No action needs to be taken by the caller.
    WindowIsFull,
}

/// An iterator over all packets in the send window. It will iterate over all packets currently
/// being sent to the remote peer.
/// These packets are awaiting a reply from the remote peer.
pub struct Iter<'a, SendData>(core::slice::Iter<'a, Option<SendEntry<SendData>>>);
/// A mutable iterator over all packets in the send window.
///
/// The user is able to mutate the contents of the packet being sent to the remote peer, as well as
/// any local metadata associated with the packet.
///
/// Take note that if the packet itself is modified, SeqEx provides no guarantees about which
/// version of the packet will have been received by the remote peer. The local peer cannot be sure
/// if the remote peer will see the modified packet. For this reason it is not recommended to modify
/// the packet.
pub struct IterMut<'a, SendData>(core::slice::IterMut<'a, Option<SendEntry<SendData>>>);

impl<SendData, RecvData, const CAP: usize> SeqEx<SendData, RecvData, CAP> {
    /// Creates a new instance of `SeqEx` for a new remote peer.
    /// An instance of `SeqEx` expects to communicate with only exactly one other remote instance
    /// of `SeqEx`.
    ///
    /// `retry_interval` is the initial value of the `retry_interval` field of `SeqEx`. It defines
    /// how long `SeqEx` will wait until resending unacknowledged packets. It can be changed later.
    ///
    /// `initial_seq_no` is the first sequence number that this instance of `SeqEx` will use. It must be
    /// exactly the same as the `initial_seq_no` of the remote instance of `SeqEx`. It can just be 1.
    pub fn new(retry_interval: i64, initial_seq_no: SeqNo) -> Self {
        Self {
            resend_interval: retry_interval,
            next_service_timestamp: i64::MAX,
            next_send_seq_no: initial_seq_no,
            pre_recv_seq_no: initial_seq_no.wrapping_sub(1),
            recv_window: core::array::from_fn(|_| None),
            send_window: core::array::from_fn(|_| None),
            concurrent_replies: core::array::from_fn(|_| 0),
            concurrent_replies_total: 0,
        }
    }
    fn send_window_slot_mut(&mut self, seq_no: SeqNo) -> &mut Option<SendEntry<SendData>> {
        &mut self.send_window[seq_no as usize % self.send_window.len()]
    }
    fn send_window_slot(&self, seq_no: SeqNo) -> &Option<SendEntry<SendData>> {
        &self.send_window[seq_no as usize % self.send_window.len()]
    }

    /// Returns whether or not the send window is full.
    /// If the send window is full calls to `SeqEx::send` will always fail.
    pub fn is_full(&self) -> bool {
        // We claim that the window is full one entry before it is actually full for the sake of
        // making it always possible for both peers to process at least one reply at all times.
        self.is_full_inner(true)
    }
    /// Returns the next sequence number to be attached to the next sent packet.
    /// This should be called before `SeqEx::send`, and the return value should be
    /// included in some way with the `packet_data` parameter passed to `SeqEx::send`.
    ///
    /// When `packet_data` is sent to the remote peer, the receiver should be able to quickly read
    /// the sequence number off of it.
    #[inline]
    pub fn seq_no(&self) -> SeqNo {
        self.next_send_seq_no
    }
    /// Sends the given packet to the remote peer and adds it to the send window.
    ///
    /// If `Ok` is returned then the packet was successfully sent.
    ///
    /// If the return value is `Err` the queue is full and the packet will not be sent.
    /// The caller must either cancel sending, abort the connection, or wait until a call to
    /// `receive`, `receive_ack` or `pump` returns `Ok` and try again.
    ///
    /// `packet_data` should contain both the packet to be sent as well as any local metadata the
    /// caller wants to store with the packet. This metadata allows the exchange to be stateful.
    /// `packet_data` must contain the latest sequence number returned by `seq_no()`
    /// There should always be a call to `SeqEx::seq_no` preceding every call to `send`.
    ///
    /// `current_time` should be a timestamp of the current time, using whatever units of time the
    /// user would like. However this choice of units must be consistent with the units of the
    /// `retry_interval`. `current_time` does not have to be monotonically increasing.
    #[must_use = "The queue might be full causing the packet to not be sent"]
    pub fn try_send(&mut self, mut app: impl TransportLayer<SendData>, packet_data: SendData) -> Result<(), SendData> {
        if self.is_full() {
            return Err(packet_data);
        }
        let seq_no = self.next_send_seq_no;
        self.next_send_seq_no = self.next_send_seq_no.wrapping_add(1);

        let current_time = app.time();
        let next_resend_time = current_time + self.resend_interval;
        if self.next_service_timestamp > next_resend_time {
            self.next_service_timestamp = next_resend_time;
            app.update_service_time(next_resend_time, current_time);
        }
        let slot = self.send_window_slot_mut(seq_no);
        debug_assert!(slot.is_none());
        let entry = slot.insert(SendEntry { seq_no, reply_no: None, next_resend_time, data: packet_data });

        app.send(entry.seq_no, entry.reply_no, &entry.data);
        Ok(())
    }

    /// If this returns `Ok` then `try_send` might succeed on next call.
    pub fn receive_raw<P: Into<RecvData>>(
        &mut self,
        mut app: impl TransportLayer<SendData>,
        seq_no: SeqNo,
        reply_no: Option<SeqNo>,
        packet: P,
    ) -> Result<(SeqNo, P, Option<SendData>), Error> {
        // We only want to accept packets with sequence numbers in the range:
        // `self.pre_recv_seq_no < seq_no <= self.pre_recv_seq_no + self.recv_window.len()`.
        // To check that range we compute `seq_no - (self.pre_recv_seq_no + 1)` and check
        // if the number wrapped below 0, or if it is above `self.recv_window.len()`.
        let normalized_seq_no = seq_no.wrapping_sub(self.pre_recv_seq_no).wrapping_sub(1);
        let is_below_range = normalized_seq_no > SeqNo::MAX / 2;
        let is_above_range = !is_below_range && normalized_seq_no >= self.recv_window.len() as u32;
        let is_next = normalized_seq_no == 0;
        if is_below_range {
            // If it is below the range, that means the packet has been received twice.
            // For every received packet we either send a normal reply or send an ack.
            // If the application sent a normal reply in response to this packet previously, and
            // that reply has not been acknowledged, then arg `seq_no` will be in the send window,
            // and if the application is still deciding what to reply with, it will be in the
            // concurrent replies array.
            // If either are the case then we know we will eventually send a reply to the packet.
            // If neither are the case then we must send an ack so the remote peer can stop
            // resending the packet.
            for entry in self.send_window.iter().flatten() {
                if entry.reply_no == Some(seq_no) {
                    return Err(Error::OutOfSequence);
                }
            }
            for i in 0..self.concurrent_replies_total {
                if self.concurrent_replies[i] == seq_no {
                    return Err(Error::OutOfSequence);
                }
            }
            app.send_ack(seq_no);
            return Err(Error::OutOfSequence);
        } else if is_above_range {
            return Err(Error::OutOfSequence);
        }
        // If the send window is full we cannot safely process received packets,
        // because there would be no way to reply.
        // We can only process this packet if processing it would make space in the send window.
        let is_full = self.is_full_inner(false);

        let i = seq_no as usize % self.recv_window.len();
        if let Some(pre) = self.recv_window[i].as_mut() {
            if seq_no == pre.seq_no {
                if is_next && !is_full {
                    self.recv_window[i] = None;
                } else {
                    return if is_full {
                        Err(Error::WindowIsFull)
                    } else {
                        Err(Error::OutOfSequence)
                    };
                }
            } else {
                // This is currently unreachable due to the range check.
                // `self.pre_recv_seq_no < seq_no <= self.pre_recv_seq_no + self.recv_window.len()`.
                return Err(Error::OutOfSequence);
            }
        }
        if is_next && !is_full {
            self.pre_recv_seq_no = seq_no;
            self.concurrent_replies[self.concurrent_replies_total] = seq_no;
            self.concurrent_replies_total += 1;
            let data = reply_no.and_then(|r| self.take_send(r));
            Ok((seq_no, packet, data))
        } else {
            self.recv_window[i] = Some(RecvEntry { seq_no, reply_no, data: packet.into() });
            if is_full {
                Err(Error::WindowIsFull)
            } else {
                Err(Error::OutOfSequence)
            }
        }
    }
    /// If this returns `Ok` then `try_send` might succeed on next call.
    pub fn receive_ack(&mut self, reply_no: SeqNo) -> Result<SendData, Error> {
        self.take_send(reply_no).ok_or(Error::OutOfSequence)
    }

    fn is_full_inner(&self, reserve_one: bool) -> bool {
        if self.concurrent_replies_total >= self.concurrent_replies.len() {
            return true;
        }
        for i in 0..self.concurrent_replies_total as u32 + 1 + reserve_one as u32 {
            let slot = self.send_window_slot(self.next_send_seq_no.wrapping_add(i));
            if slot.is_some() {
                return true;
            }
        }
        false
    }

    fn take_send(&mut self, reply_no: SeqNo) -> Option<SendData> {
        let slot = self.send_window_slot_mut(reply_no);
        if slot.as_ref().map_or(false, |e| e.seq_no == reply_no) {
            slot.take().map(|e| e.data)
        } else {
            None
        }
    }
    /// If this returns `Ok` then `try_send` might succeed on next call.
    pub fn pump_raw(&mut self) -> Result<(SeqNo, RecvData, Option<SendData>), Error> {
        let next_seq_no = self.pre_recv_seq_no.wrapping_add(1);
        let i = next_seq_no as usize % self.recv_window.len();

        if self.recv_window[i].as_ref().map_or(false, |pre| pre.seq_no == next_seq_no) {
            if self.is_full_inner(false) {
                return Err(Error::WindowIsFull);
            }

            let entry = self.recv_window[i].take().unwrap();
            self.pre_recv_seq_no = next_seq_no;
            self.concurrent_replies[self.concurrent_replies_total] = entry.seq_no;
            self.concurrent_replies_total += 1;
            let data = entry.reply_no.and_then(|r| self.take_send(r));
            Ok((entry.seq_no, entry.data, data))
        } else {
            Err(Error::OutOfSequence)
        }
    }

    /// This function must be passed a reply number given by `receive_raw` or `pump_raw`, otherwise
    /// it will do nothing. This reply number can only be used to reply once.
    ///
    /// If you need to reply more than once, say to fragment a large file, then include in your
    /// first reply some identifier, and then `send` all fragments with the same included identifier.
    /// The identifier will tell the remote peer which packets contain fragments of the file,
    /// and since each fragment will be received in order it will be trivial for them to reconstruct
    /// the original file.
    pub fn reply_raw(&mut self, mut app: impl TransportLayer<SendData>, reply_no: SeqNo, packet_data: SendData) {
        if self.remove_reservation(reply_no) {
            let seq_no = self.next_send_seq_no;
            self.next_send_seq_no = self.next_send_seq_no.wrapping_add(1);

            let current_time = app.time();
            let next_resend_time = current_time + self.resend_interval;
            if self.next_service_timestamp > next_resend_time {
                self.next_service_timestamp = next_resend_time;
                app.update_service_time(next_resend_time, current_time);
            }
            let slot = self.send_window_slot_mut(seq_no);
            debug_assert!(slot.is_none());
            let entry = slot.insert(SendEntry {
                seq_no,
                reply_no: Some(reply_no),
                next_resend_time,
                data: packet_data,
            });

            app.send(entry.seq_no, entry.reply_no, &entry.data);
        }
    }
    pub fn ack_raw(&mut self, mut app: impl TransportLayer<SendData>, reply_no: SeqNo) {
        if self.remove_reservation(reply_no) {
            // Acks are only sent once. There is code in `receive_raw` to handle resending
            // an ack in the event that the first one here was dropped by the network.
            app.send_ack(reply_no);
        }
    }
    fn remove_reservation(&mut self, reply_no: SeqNo) -> bool {
        for i in 0..self.concurrent_replies_total {
            if self.concurrent_replies[i] == reply_no {
                // swap remove
                self.concurrent_replies_total -= 1;
                self.concurrent_replies[i] = self.concurrent_replies[self.concurrent_replies_total];
                return true;
            }
        }
        false
    }

    pub fn service(&mut self, mut app: impl TransportLayer<SendData>) -> i64 {
        let current_time = app.time();
        let real_interval = if self.next_service_timestamp <= current_time {
            let next_resend_time = current_time + self.resend_interval;
            let mut next_activity = i64::MAX;
            for entry in self.send_window.iter_mut().flatten() {
                if entry.next_resend_time <= current_time {
                    entry.next_resend_time = next_resend_time;
                    app.send(entry.seq_no, entry.reply_no, &entry.data);
                }
                next_activity = next_activity.min(entry.next_resend_time);
            }
            self.next_service_timestamp = next_activity;
            app.update_service_time(next_activity, current_time);
            next_activity - current_time
        } else {
            self.next_service_timestamp - current_time
        };
        self.resend_interval.min(real_interval)
    }

    pub fn iter(&self) -> Iter<'_, SendData> {
        Iter(self.send_window.iter())
    }
    pub fn iter_mut(&mut self) -> IterMut<'_, SendData> {
        IterMut(self.send_window.iter_mut())
    }
}
impl<SendData, RecvData, const CAP: usize> Default for SeqEx<SendData, RecvData, CAP> {
    fn default() -> Self {
        Self::new(DEFAULT_RESEND_INTERVAL_MS, DEFAULT_INITIAL_SEQ_NO)
    }
}
impl<'a, SendData, RecvData, const CAP: usize> IntoIterator for &'a SeqEx<SendData, RecvData, CAP> {
    type Item = &'a SendData;
    type IntoIter = Iter<'a, SendData>;

    fn into_iter(self) -> Self::IntoIter {
        self.iter()
    }
}
impl<'a, SendData, RecvData, const CAP: usize> IntoIterator for &'a mut SeqEx<SendData, RecvData, CAP> {
    type Item = &'a mut SendData;
    type IntoIter = IterMut<'a, SendData>;

    fn into_iter(self) -> Self::IntoIter {
        self.iter_mut()
    }
}

macro_rules! iterator {
    ($iter:ident, {$( $mut:tt )?}) => {
        impl<'a, SendData> Iterator for $iter<'a, SendData> {
            type Item = &'a $($mut)? SendData;
            fn next(&mut self) -> Option<Self::Item> {
                while let Some(entry) = self.0.next() {
                    if let Some(entry) = entry {
                        return Some(& $($mut)? entry.data)
                    }
                }
                None
            }

            fn size_hint(&self) -> (usize, Option<usize>) {
                (0, Some(self.0.len()))
            }
        }
        impl<'a, SendData> DoubleEndedIterator for $iter<'a, SendData> {
            fn next_back(&mut self) -> Option<Self::Item> {
                while let Some(entry) = self.0.next_back() {
                    if let Some(entry) = entry {
                        return Some(& $($mut)? entry.data)
                    }
                }
                None
            }
        }
    }
}
iterator!(Iter, {});
iterator!(IterMut, {mut});
