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
#![no_std]
#![forbid(unsafe_code)]
//#![warn(missing_docs, rust_2018_idioms)]

/// A 32-bit sequence number. Packets transported with SEP are expected to contain at least one
/// sequence number, and sometimes two.
/// All packets will either have a seq_no, a reply_no, or both.
pub type SeqNo = u32;

/// A trait for giving an instance of SeqEx access to the transport layer.
///
/// The implementor is free to choose how to define the generic types based on how they want to
/// manage memory.
/// It is possible through these generics to implement SeqEx to be no-alloc and zero-copy, but otherwise
/// a lot of them are most easily implemented as tuples of custom enums and Vec<u8>.
pub trait TransportLayer: Sized {
    type RecvData;
    type RecvDataRef<'a>;
    type RecvReturn;

    type SendData;

    fn send(&self, data: &Self::SendData);
    fn send_ack(&self, reply_no: SeqNo);
    fn send_empty_reply(&self, reply_no: SeqNo);

    fn deserialize<'a>(data: &'a Self::RecvData) -> Self::RecvDataRef<'a>;
    fn process(&self, reply_cx: ReplyGuard<'_, Self>, recv_packet: Self::RecvDataRef<'_>, send_data: Option<Self::SendData>) -> Self::RecvReturn;
}
/// A trait for abstracting the process of receiving a packet, it allows SeqEx to either immediately
/// process a reference to the packet, or take ownership of the packet so it can be processed later.
///
/// SeqEx has to take ownership of packets when they are received out-of-order, they are held in a
/// buffer until the time comes that they can be processed in order.
///
/// It is possible through this trait to avoid a copy, allocation or other expensive ownership
/// operation whenever a packet is received in order and can immediately be processed.
pub trait IntoRecvData<TL: TransportLayer>: Into<TL::RecvData> {
    /// Return some form of reference to the data that `process` expects to receive.
    /// This function can do anything from complex deserialization to a basic dereference.
    fn as_ref(&self) -> TL::RecvDataRef<'_>;
}
impl<TL: TransportLayer> IntoRecvData<TL> for TL::RecvData {
    fn as_ref(&self) -> TL::RecvDataRef<'_> {
        TL::deserialize(self)
    }
}
/// a
pub struct SeqEx<TL: TransportLayer, const SLEN: usize = 64, const RLEN: usize = 32> {
    /// The interval at which packets will be resent if they have not yet been acknowledged by the
    /// remote peer.
    /// It can be statically or dynamically set, it is up to the user to decide.
    pub resend_interval: i64,
    next_send_seq_no: SeqNo,
    pre_recv_seq_no: SeqNo,
    send_window: [Option<SendEntry<TL>>; SLEN],
    recv_window: [Option<RecvEntry<TL>>; RLEN],
}

struct RecvEntry<TL: TransportLayer> {
    seq_no: SeqNo,
    reply_no: Option<SeqNo>,
    data: TL::RecvData,
}

struct SendEntry<TL: TransportLayer> {
    seq_no: SeqNo,
    reply_no: Option<SeqNo>,
    next_resent_time: i64,
    data: TL::SendData,
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
/// Whenever a packet is received, it must be replied to.
/// This Guard object guarantees that this is the case.
/// If it is dropped without calling `reply` an empty reply will be sent to the remote peer.
pub struct ReplyGuard<'a, TL: TransportLayer> {
    app: Option<&'a TL>,
    seq_queue: &'a mut SeqEx<TL>,
    reply_no: SeqNo,
}

/// An iterator over all packets in the send window. It will iterate over all packets currently
/// being sent to the remote peer.
/// These packets are awaiting a reply from the remote peer.
pub struct Iter<'a, TL: TransportLayer>(core::slice::Iter<'a, Option<SendEntry<TL>>>);
/// A mutable iterator over all packets in the send window.
///
/// The user is able to mutate the contents of the packet being sent to the remote peer, as well as
/// any local metadata associated with the packet.
///
/// Take note that if the packet itself is modified, SeqEx provides no guarantees about which
/// version of the packet will have been received by the remote peer. The local peer cannot be sure
/// if the remote peer will see the modified packet. For this reason it is not recommended to modify
/// the packet.
pub struct IterMut<'a, TL: TransportLayer>(core::slice::IterMut<'a, Option<SendEntry<TL>>>);

impl<TL: TransportLayer> SeqEx<TL> {
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
            next_send_seq_no: initial_seq_no,
            pre_recv_seq_no: initial_seq_no.wrapping_sub(1),
            recv_window: core::array::from_fn(|_| None),
            send_window: core::array::from_fn(|_| None),
        }
    }

    /// Returns whether or not the send window is full.
    /// If the send window is full calls to `SeqEx::send` will always fail.
    pub fn is_full(&self) -> bool {
        // We claim that the window is full one entry before it is actually full for the sake of
        // making it always possible for both peers to process at least one reply at all times.
        let next_i = self.next_send_seq_no as usize;
        self.send_window[next_i % self.send_window.len()].is_some() || self.send_window[(next_i + 1) % self.send_window.len()].is_some()
    }
    /// Returns the next sequence number to be attached to the next sent packet.
    /// This should be called before `SeqEx::send`, and the return value should be
    /// included in some way with the `packet_data` parameter passed to `SeqEx::send`.
    ///
    /// When `packet_data` is sent to the remote peer, the receiver should be able to quickly read
    /// the sequence number off of it.
    pub fn seq_no(&self) -> SeqNo {
        self.next_send_seq_no
    }
    /// Sends the given packet to the remote peer and adds it to the send window.
    ///
    /// If the return value is `false` the queue is full and the packet will not be sent.
    /// The caller must either cancel sending, abort the connection, or wait until a call to
    /// `receive` or `receive_empty_reply` returns `Some` and try again.
    ///
    /// If true is returned then the packet was successfully sent.
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
    pub fn send(&mut self, app: TL, packet_data: TL::SendData, current_time: i64) -> bool {
        if self.is_full() {
            return false;
        }
        let seq_no = self.next_send_seq_no;
        self.next_send_seq_no = self.next_send_seq_no.wrapping_add(1);

        let next_resent_time = current_time + self.resend_interval;
        let entry = self.send_window[seq_no as usize % self.send_window.len()].insert(SendEntry {
            seq_no,
            reply_no: None,
            next_resent_time,
            data: packet_data,
        });

        app.send(&entry.data);
        true
    }

    pub fn receive(
        &mut self,
        app: TL,
        seq_no: SeqNo,
        reply_no: Option<SeqNo>,
        packet: impl IntoRecvData<TL>,
    ) -> Result<TL::RecvReturn, Error> {
        // We only want to accept packets with seq_nos in the range:
        // `self.pre_recv_seq_no < seq_no <= self.pre_recv_seq_no + self.recv_window.len()`.
        // To check that range we compute `seq_no - (self.pre_recv_seq_no + 1)` and check
        // if the number wrapped below 0, or if it is above `self.recv_window.len()`.
        let normalized_seq_no = seq_no.wrapping_sub(self.pre_recv_seq_no).wrapping_sub(1);
        let is_below_range = normalized_seq_no > SeqNo::MAX / 2;
        let is_above_range = !is_below_range && normalized_seq_no >= self.recv_window.len() as u32;
        let is_next = normalized_seq_no == 0;
        if is_below_range {
            // Check whether or not we are already replying to this packet.
            for entry in self.send_window.iter() {
                if entry.as_ref().map_or(false, |e| e.reply_no == Some(seq_no)) {
                    return Err(Error::OutOfSequence);
                }
            }
            app.send_empty_reply(seq_no);
            return Err(Error::OutOfSequence);
        } else if is_above_range {
            return Err(Error::OutOfSequence);
        }
        // If the send window is full we cannot safely process received packets,
        // because there would be no way to reply.
        // We can only process this packet if processing it would make space in the send window.
        let next_i = self.next_send_seq_no as usize % self.send_window.len();
        let would_be_full = self.send_window[next_i].as_ref().map_or(false, |e| Some(e.seq_no) != reply_no);
        let i = seq_no as usize % self.recv_window.len();
        if let Some(pre) = self.recv_window[i].as_mut() {
            if seq_no == pre.seq_no {
                if is_next && !would_be_full {
                    self.recv_window[i] = None;
                } else {
                    app.send_ack(seq_no);
                    return if would_be_full {
                        Err(Error::WindowIsFull)
                    } else {
                        Err(Error::OutOfSequence)
                    };
                }
            } else {
                // NOTE: I believe this return is currently unreachable.
                return Err(Error::OutOfSequence);
            }
        }
        if is_next && !would_be_full {
            self.pre_recv_seq_no = seq_no;
            let data = reply_no.and_then(|r| self.take_send(r));
            Ok(app.process(ReplyGuard { app: Some(&app), seq_queue: self, reply_no: seq_no }, packet.as_ref(), data))
        } else {
            self.recv_window[i] = Some(RecvEntry { seq_no, reply_no, data: packet.into() });
            if let Some(reply_no) = reply_no {
                self.receive_ack(reply_no);
            }
            app.send_ack(seq_no);
            if would_be_full {
                Err(Error::WindowIsFull)
            } else {
                Err(Error::OutOfSequence)
            }
        }
    }
    pub fn receive_ack(&mut self, reply_no: SeqNo) {
        let i = reply_no as usize % self.send_window.len();
        if let Some(entry) = self.send_window[i].as_mut() {
            if entry.seq_no == reply_no {
                entry.next_resent_time = i64::MAX;
            }
        }
    }
    pub fn receive_empty_reply(&mut self, reply_no: SeqNo) -> Option<TL::SendData> {
        let i = reply_no as usize % self.send_window.len();
        if self.send_window[i].as_ref().map_or(false, |e| e.seq_no == reply_no) {
            let entry = self.send_window[i].take().unwrap();
            Some(entry.data)
        } else {
            None
        }
    }

    fn take_send(&mut self, reply_no: SeqNo) -> Option<TL::SendData> {
        let i = reply_no as usize % self.send_window.len();
        if self.send_window[i].as_ref().map_or(false, |e| e.seq_no == reply_no) {
            self.send_window[i].take().map(|e| e.data)
        } else {
            None
        }
    }
    pub fn pump(&mut self, app: TL) -> Result<TL::RecvReturn, Error> {
        let next_seq_no = self.pre_recv_seq_no.wrapping_add(1);
        let i = next_seq_no as usize % self.recv_window.len();

        if self.recv_window[i].as_ref().map_or(false, |pre| pre.seq_no == next_seq_no) {
            let next_i = self.next_send_seq_no as usize % self.send_window.len();
            if self.send_window[next_i].is_some() {
                return Err(Error::WindowIsFull);
            }
            let entry = self.recv_window[i].take().unwrap();
            self.pre_recv_seq_no = next_seq_no;
            let data = entry.reply_no.and_then(|r| self.take_send(r));
            Ok(app.process(
                ReplyGuard { app: Some(&app), seq_queue: self, reply_no: entry.seq_no },
                TL::deserialize(&entry.data),
                data,
            ))
        } else {
            Err(Error::OutOfSequence)
        }
    }

    pub fn service(&mut self, app: TL, current_time: i64) -> i64 {
        let next_interval = current_time + self.resend_interval;
        let mut next_activity = next_interval;
        for item in self.send_window.iter_mut() {
            if let Some(entry) = item {
                if entry.next_resent_time <= current_time {
                    entry.next_resent_time = next_interval;
                    app.send(&entry.data);
                } else {
                    next_activity = next_activity.min(entry.next_resent_time);
                }
            }
        }
        next_activity - current_time
    }

    pub fn iter(&self) -> Iter<'_, TL> {
        Iter(self.send_window.iter())
    }
    pub fn iter_mut(&mut self) -> IterMut<'_, TL> {
        IterMut(self.send_window.iter_mut())
    }
}
impl<'a, TL: TransportLayer> IntoIterator for &'a SeqEx<TL> {
    type Item = &'a TL::SendData;
    type IntoIter = Iter<'a, TL>;

    fn into_iter(self) -> Self::IntoIter {
        self.iter()
    }
}
impl<'a, TL: TransportLayer> IntoIterator for &'a mut SeqEx<TL> {
    type Item = &'a mut TL::SendData;
    type IntoIter = IterMut<'a, TL>;

    fn into_iter(self) -> Self::IntoIter {
        self.iter_mut()
    }
}

impl<'a, TL: TransportLayer> ReplyGuard<'a, TL> {
    pub fn seq_no(&self) -> SeqNo {
        self.seq_queue.next_send_seq_no
    }
    pub fn reply_no(&self) -> SeqNo {
        self.reply_no
    }
    pub fn reply(mut self, packet_data: TL::SendData, current_time: i64) {
        if let Some(app) = self.app {
            let seq_queue = &mut self.seq_queue;
            let seq_no = seq_queue.next_send_seq_no;
            seq_queue.next_send_seq_no = seq_queue.next_send_seq_no.wrapping_add(1);

            let i = seq_no as usize % seq_queue.send_window.len();
            let next_resent_time = current_time + seq_queue.resend_interval;
            let entry = seq_queue.send_window[i].insert(SendEntry {
                seq_no,
                reply_no: Some(self.reply_no),
                next_resent_time,
                data: packet_data,
            });

            app.send(&entry.data);
            self.app = None;
        }
    }
}
impl<'a, TL: TransportLayer> Drop for ReplyGuard<'a, TL> {
    fn drop(&mut self) {
        if let Some(app) = self.app {
            app.send_empty_reply(self.reply_no);
        }
    }
}

macro_rules! iterator {
    ($iter:ident, {$( $mut:tt )?}) => {
        impl<'a, TL: TransportLayer> Iterator for $iter<'a, TL> {
            type Item = &'a $($mut)? TL::SendData;
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
        impl<'a, TL: TransportLayer> DoubleEndedIterator for $iter<'a, TL> {
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
