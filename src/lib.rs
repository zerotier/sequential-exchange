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
//! keep-alives, and expiration handling. This can be both a pro and a con, as it means there is a
//! lot of efficiency to be gained if these features are not needed or are implemented at a
//! different protocol layer.
//!
//! Neither SEP nor TCP are cryptographically secure.
//!
//! ## Examples
//!
#![no_std]
#![forbid(unsafe_code)]
//#![warn(missing_docs, missing_debug_implementations, rust_2018_idioms)]

pub type SeqNum = u32;

pub trait TransportLayer: Sized {
    type RecvData;
    type RecvDataRef<'a>;
    type RecvReturn;

    type SendData;

    fn send(&self, data: &Self::SendData);
    fn send_ack(&self, reply_num: SeqNum);
    fn send_empty_reply(&self, reply_num: SeqNum);

    fn deserialize<'a>(data: &'a Self::RecvData) -> Self::RecvDataRef<'a>;
    fn process(&self, reply_cx: ReplyGuard<'_, Self>, recv_packet: Self::RecvDataRef<'_>, send_data: Option<Self::SendData>) -> Self::RecvReturn;
}

pub trait IntoRecvData<TL: TransportLayer>: Into<TL::RecvData> {
    fn as_ref(&self) -> TL::RecvDataRef<'_>;
}
impl<TL: TransportLayer> IntoRecvData<TL> for TL::RecvData {
    fn as_ref(&self) -> TL::RecvDataRef<'_> {
        TL::deserialize(self)
    }
}

pub struct SeqEx<TL: TransportLayer, const SLEN: usize = 64, const RLEN: usize = 32> {
    pub retry_interval: i64,
    next_send_seq_num: SeqNum,
    pre_recv_seq_num: SeqNum,
    send_window: [Option<SendEntry<TL>>; SLEN],
    recv_window: [Option<RecvEntry<TL>>; RLEN],
}

struct RecvEntry<TL: TransportLayer> {
    seq_num: SeqNum,
    reply_num: Option<SeqNum>,
    data: TL::RecvData,
}

struct SendEntry<TL: TransportLayer> {
    seq_num: SeqNum,
    reply_num: Option<SeqNum>,
    next_resent_time: i64,
    data: TL::SendData,
}

pub enum Error {
    OutOfSequence,
    QueueIsFull,
}
/// Whenever a packet is received, it must be replied to.
/// This Guard object guarantees that this is the case.
/// If it is dropped without calling `reply` an empty reply will be sent to the remote peer.
pub struct ReplyGuard<'a, TL: TransportLayer> {
    app: Option<&'a TL>,
    seq_queue: &'a mut SeqEx<TL>,
    reply_num: SeqNum,
}

pub struct Iter<'a, TL: TransportLayer>(core::slice::Iter<'a, Option<SendEntry<TL>>>);
pub struct IterMut<'a, TL: TransportLayer>(core::slice::IterMut<'a, Option<SendEntry<TL>>>);

impl<TL: TransportLayer> SeqEx<TL> {
    pub fn new(retry_interval: i64, initial_seq_num: SeqNum) -> Self {
        Self {
            retry_interval,
            next_send_seq_num: initial_seq_num,
            pre_recv_seq_num: initial_seq_num.wrapping_sub(1),
            recv_window: core::array::from_fn(|_| None),
            send_window: core::array::from_fn(|_| None),
        }
    }

    pub fn is_full(&self) -> bool {
        // We claim that the window is full one entry before it is actually full for the sake of
        // making it always possible for both peers to process at least one reply at all times.
        let next_i = self.next_send_seq_num as usize;
        self.send_window[next_i % self.send_window.len()].is_some() || self.send_window[(next_i + 1) % self.send_window.len()].is_some()
    }

    pub fn seq_num(&self) -> SeqNum {
        self.next_send_seq_num
    }
    /// If the return value is `false` the queue is full and the packet will not be sent.
    /// The caller must either cancel sending, abort the connection, or wait until a call to
    /// `receive` or `receive_empty_reply` returns `Some` and try again.
    ///
    /// If true is returned then the packet was successfully sent.
    ///
    /// `packet_data` must contain the latest sequence number returned by `seq_num()`
    /// There should always be a call to `SeqQueue::seq_num()` preceding every call to `send_seq`.
    #[must_use = "The queue might be full causing the packet to not be sent"]
    pub fn send_seq(&mut self, app: TL, packet_data: TL::SendData, current_time: i64) -> bool {
        if self.is_full() {
            return false;
        }
        let seq_num = self.next_send_seq_num;
        self.next_send_seq_num = self.next_send_seq_num.wrapping_add(1);

        let next_resent_time = current_time + self.retry_interval;
        let entry = self.send_window[seq_num as usize % self.send_window.len()].insert(SendEntry {
            seq_num,
            reply_num: None,
            next_resent_time,
            data: packet_data,
        });

        app.send(&entry.data);
        true
    }

    pub fn receive(
        &mut self,
        app: TL,
        seq_num: SeqNum,
        reply_num: Option<SeqNum>,
        packet: impl IntoRecvData<TL>,
    ) -> Result<TL::RecvReturn, Error> {
        // We only want to accept packets with seq_nums in the range:
        // `self.pre_recv_seq_num < seq_num <= self.pre_recv_seq_num + self.recv_window.len()`.
        // To check that range we compute `seq_num - (self.pre_recv_seq_num + 1)` and check
        // if the number wrapped below 0, or if it is above `self.recv_window.len()`.
        let normalized_seq_num = seq_num.wrapping_sub(self.pre_recv_seq_num).wrapping_sub(1);
        let is_below_range = normalized_seq_num > SeqNum::MAX / 2;
        let is_above_range = !is_below_range && normalized_seq_num >= self.recv_window.len() as u32;
        let is_next = normalized_seq_num == 0;
        if is_below_range {
            // Check whether or not we are already replying to this packet.
            for entry in self.send_window.iter() {
                if entry.as_ref().map_or(false, |e| e.reply_num == Some(seq_num)) {
                    return Err(Error::OutOfSequence);
                }
            }
            app.send_empty_reply(seq_num);
            return Err(Error::OutOfSequence);
        } else if is_above_range {
            return Err(Error::OutOfSequence);
        }
        // If the send window is full we cannot safely process received packets,
        // because there would be no way to reply.
        // We can only process this packet if processing it would make space in the send window.
        let next_i = self.next_send_seq_num as usize % self.send_window.len();
        let would_be_full = self.send_window[next_i].as_ref().map_or(false, |e| Some(e.seq_num) != reply_num);
        let i = seq_num as usize % self.recv_window.len();
        if let Some(pre) = self.recv_window[i].as_mut() {
            if seq_num == pre.seq_num {
                if is_next && !would_be_full {
                    self.recv_window[i] = None;
                } else {
                    app.send_ack(seq_num);
                    return if would_be_full {
                        Err(Error::QueueIsFull)
                    } else {
                        Err(Error::OutOfSequence)
                    };
                }
            } else {
                return Err(Error::OutOfSequence);
            }
        }
        if is_next && !would_be_full {
            self.pre_recv_seq_num = seq_num;
            let data = reply_num.and_then(|r| self.take_send(r));
            Ok(app.process(ReplyGuard { app: Some(&app), seq_queue: self, reply_num: seq_num }, packet.as_ref(), data))
        } else {
            self.recv_window[i] = Some(RecvEntry { seq_num, reply_num, data: packet.into() });
            if let Some(reply_num) = reply_num {
                self.receive_ack(reply_num);
            }
            app.send_ack(seq_num);
            if would_be_full {
                Err(Error::QueueIsFull)
            } else {
                Err(Error::OutOfSequence)
            }
        }
    }

    fn take_send(&mut self, reply_num: SeqNum) -> Option<TL::SendData> {
        let i = reply_num as usize % self.send_window.len();
        if self.send_window[i].as_ref().map_or(false, |e| e.seq_num == reply_num) {
            self.send_window[i].take().map(|e| e.data)
        } else {
            None
        }
    }
    pub fn pump(&mut self, app: TL) -> Result<TL::RecvReturn, Error> {
        let next_seq_num = self.pre_recv_seq_num.wrapping_add(1);
        let i = next_seq_num as usize % self.recv_window.len();

        if self.recv_window[i].as_ref().map_or(false, |pre| pre.seq_num == next_seq_num) {
            let next_i = self.next_send_seq_num as usize % self.send_window.len();
            if self.send_window[next_i].is_some() {
                return Err(Error::QueueIsFull);
            }
            let entry = self.recv_window[i].take().unwrap();
            self.pre_recv_seq_num = next_seq_num;
            let data = entry.reply_num.and_then(|r| self.take_send(r));
            Ok(app.process(
                ReplyGuard { app: Some(&app), seq_queue: self, reply_num: entry.seq_num },
                TL::deserialize(&entry.data),
                data,
            ))
        } else {
            Err(Error::OutOfSequence)
        }
    }

    pub fn receive_ack(&mut self, reply_num: SeqNum) {
        let i = reply_num as usize % self.send_window.len();
        if let Some(entry) = self.send_window[i].as_mut() {
            if entry.seq_num == reply_num {
                entry.next_resent_time = i64::MAX;
            }
        }
    }
    pub fn receive_empty_reply(&mut self, reply_num: SeqNum) -> Option<TL::SendData> {
        let i = reply_num as usize % self.send_window.len();
        if self.send_window[i].as_ref().map_or(false, |e| e.seq_num == reply_num) {
            let entry = self.send_window[i].take().unwrap();
            Some(entry.data)
        } else {
            None
        }
    }

    pub fn service(&mut self, app: TL, current_time: i64) -> i64 {
        let next_interval = current_time + self.retry_interval;
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
    pub fn seq_num(&self) -> SeqNum {
        self.seq_queue.next_send_seq_num
    }
    pub fn reply_num(&self) -> SeqNum {
        self.reply_num
    }
    pub fn reply(mut self, packet_data: TL::SendData, current_time: i64) {
        if let Some(app) = self.app {
            let seq_queue = &mut self.seq_queue;
            let seq_num = seq_queue.next_send_seq_num;
            seq_queue.next_send_seq_num = seq_queue.next_send_seq_num.wrapping_add(1);

            let i = seq_num as usize % seq_queue.send_window.len();
            let next_resent_time = current_time + seq_queue.retry_interval;
            let entry = seq_queue.send_window[i].insert(SendEntry {
                seq_num,
                reply_num: Some(self.reply_num),
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
            app.send_empty_reply(self.reply_num);
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
