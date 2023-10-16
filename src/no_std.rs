pub use crate::single_thread::*;

use crate::{
    result::{TryError, TryRecvError},
    transport_layer::SeqNo,
    Packet, DEFAULT_INITIAL_SEQ_NO, DEFAULT_RESEND_INTERVAL_MS, DEFAULT_WINDOW_CAP,
};
use Packet::*;

#[derive(Debug)]
pub struct SeqEx<SendData, RecvData, const CAP: usize = DEFAULT_WINDOW_CAP> {
    /// The interval at which packets will be resent if they have not yet been acknowledged by the
    /// remote peer.
    /// It can be statically or dynamically set.
    pub resend_interval: i64,
    pub next_service_timestamp: i64,
    next_send_seq_no: SeqNo,
    next_recv_seq_no: SeqNo,
    /// This could be made more efficient by changing to SoA format.
    send_window: [Option<SendEntry<SendData>>; CAP],
    recv_window: [RecvEntry<RecvData>; CAP],
    /// The size of this array determines the maximum number of received packets that the application
    /// may attempt to process concurrently before new received packets start being dropped.
    concurrent_replies: [SeqNo; CAP],
    /// The total number of concurrent replies being processed. When a packet is received, a reply
    /// number is issued for that packet. That reply number reserves resources for itself, so that
    /// when `reply_raw` or `ack_raw` are called with it, they are guaranteed not to fail.
    /// To accomplish this we must track all issued reply numbers.
    concurrent_replies_total: usize,
    is_locked: bool,
}

#[derive(Debug)]
enum RecvEntry<RecvData> {
    Occupied {
        seq_no: SeqNo,
        reply_no: Option<SeqNo>,
        seq_cst: bool,
        data: RecvData,
    },
    Unlocked {
        seq_no: SeqNo,
    },
    Empty,
}

#[derive(Debug)]
struct SendEntry<SendData> {
    seq_no: SeqNo,
    reply_no: Option<SeqNo>,
    seq_cst: bool,
    next_resend_time: i64,
    data: SendData,
}
impl<SendData> SendEntry<SendData> {
    fn to_packet(&self) -> Packet<&SendData> {
        let mut p = if let Some(reply_no) = self.reply_no {
            Reply(self.seq_no, reply_no, &self.data)
        } else {
            Payload(self.seq_no, &self.data)
        };
        p.set_seq_cst(self.seq_cst);
        p
    }
}

#[derive(Clone, Debug)]
pub enum RecvOkRaw<SendData, RecvData> {
    Payload {
        reply_no: SeqNo,
        seq_cst: bool,
        recv_data: RecvData,
    },
    Reply {
        reply_no: SeqNo,
        seq_cst: bool,
        recv_data: RecvData,
        send_data: SendData,
    },
    Ack {
        send_data: SendData,
    },
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

#[derive(Clone, Debug)]
pub struct ServiceIter {
    seq_no: SeqNo,
    next_time: i64,
}

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
        debug_assert!(CAP > 1);
        Self {
            resend_interval: retry_interval,
            next_service_timestamp: i64::MAX,
            next_send_seq_no: initial_seq_no,
            next_recv_seq_no: initial_seq_no,
            recv_window: core::array::from_fn(|_| RecvEntry::Empty),
            send_window: core::array::from_fn(|_| None),
            concurrent_replies: core::array::from_fn(|_| 0),
            concurrent_replies_total: 0,
            is_locked: false,
        }
    }
    #[inline]
    fn send_window_slot_mut(&mut self, seq_no: SeqNo) -> &mut Option<SendEntry<SendData>> {
        &mut self.send_window[seq_no as usize % self.send_window.len()]
    }
    #[inline]
    fn is_full_inner(&self, reply_no: Option<SeqNo>) -> Result<(), TryError> {
        if self.concurrent_replies_total >= self.concurrent_replies.len() - 2 {
            return Err(TryError::WaitingForReply);
        }
        let reply_idx = reply_no.map_or(self.send_window.len(), |r| r as usize % self.send_window.len());
        for i in 0..1 + self.concurrent_replies_total as u32 {
            let idx = self.next_send_seq_no.wrapping_add(i) as usize % self.send_window.len();
            if self.send_window[idx].is_some() && reply_idx != idx {
                return if i == 0 {
                    Err(TryError::WaitingForRecv)
                } else {
                    Err(TryError::WaitingForReply)
                };
            }
        }
        Ok(())
    }

    #[inline]
    fn take_send(&mut self, reply_no: SeqNo) -> Option<SendData> {
        let slot = self.send_window_slot_mut(reply_no);
        if slot.as_ref().map_or(false, |e| e.seq_no == reply_no) {
            slot.take().map(|e| e.data)
        } else {
            None
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
    ///
    /// Can mutate `next_service_timestamp`.
    pub fn try_send_direct(&mut self, current_time: i64, seq_cst: bool, packet_data: SendData) -> Result<Packet<&SendData>, (TryError, SendData)> {
        let mut tmp = Some(packet_data);
        self.try_send_direct_with(current_time, seq_cst, |_| tmp.take().unwrap())
            .map_err(|e| e.0)
            .map_err(|e| (e, tmp.unwrap()))
    }
    /// Can mutate `next_service_timestamp`.
    pub fn try_send_direct_with<F: FnOnce(SeqNo) -> SendData>(
        &mut self,
        current_time: i64,
        seq_cst: bool,
        packet_data: F,
    ) -> Result<Packet<&SendData>, (TryError, F)> {
        if let Err(e) = self.is_full_inner(None) {
            return Err((e, packet_data));
        }

        let seq_no = self.next_send_seq_no;
        self.next_send_seq_no = self.next_send_seq_no.wrapping_add(1);

        let next_resend_time = current_time + self.resend_interval;
        if self.next_service_timestamp > next_resend_time {
            self.next_service_timestamp = next_resend_time;
        }
        let slot = self.send_window_slot_mut(seq_no);
        debug_assert!(slot.is_none());
        let entry = slot.insert(SendEntry {
            seq_no,
            reply_no: None,
            seq_cst,
            next_resend_time,
            data: packet_data(seq_no),
        });

        Ok(entry.to_packet())
    }

    fn fast_forward(&mut self) -> bool {
        loop {
            let next_seq_no = self.next_recv_seq_no;
            let i = next_seq_no as usize % self.recv_window.len();
            match &self.recv_window[i] {
                RecvEntry::Unlocked { seq_no } => {
                    debug_assert_eq!(*seq_no, next_seq_no);
                    self.recv_window[i] = RecvEntry::Empty;
                    self.next_recv_seq_no = next_seq_no.wrapping_add(1);
                }
                RecvEntry::Occupied { .. } => return true,
                RecvEntry::Empty => return false,
            }
        }
    }

    /// If this returns `Ok` then `try_send` might succeed on next call.
    pub fn receive_raw_and_direct<P: Into<RecvData>>(&mut self, packet: Packet<P>) -> Result<(RecvOkRaw<SendData, P>, bool), TryRecvError> {
        let seq_cst = packet.is_seq_cst();
        let (seq_no, reply_no, recv_data) = match packet {
            Payload(seq_no, recv_data) | SeqCstPayload(seq_no, recv_data) => (seq_no, None, recv_data),
            Reply(seq_no, reply_no, recv_data) | SeqCstReply(seq_no, reply_no, recv_data) => (seq_no, Some(reply_no), recv_data),
            Ack(reply_no) => {
                return self
                    .take_send(reply_no)
                    .map(|send_data| (RecvOkRaw::Ack { send_data }, false))
                    .ok_or(TryRecvError::DroppedDuplicate)
            }
        };
        // We only want to accept packets with sequence numbers in the range:
        // `self.next_recv_seq_no <= seq_no < self.next_recv_seq_no + self.recv_window.len()`.
        // To check that range we compute `seq_no - self.next_recv_seq_no` and check
        // if the number wrapped below 0, or if it is above `self.recv_window.len()`.
        let normalized_seq_no = seq_no.wrapping_sub(self.next_recv_seq_no);
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
                    return Err(TryRecvError::DroppedDuplicate);
                }
            }
            for i in 0..self.concurrent_replies_total {
                if self.concurrent_replies[i] == seq_no {
                    return Err(TryRecvError::DroppedDuplicate);
                }
            }
            return Err(TryRecvError::DroppedDuplicateResendAck(seq_no));
        } else if is_above_range {
            return Err(TryRecvError::DroppedTooEarly);
        }

        // Check whether or not we've already received this packet
        let i = seq_no as usize % self.recv_window.len();
        let is_duplicate = if let RecvEntry::Occupied { seq_no: pre_seq_no, .. } | RecvEntry::Unlocked { seq_no: pre_seq_no } = &self.recv_window[i] {
            // Due to the range check these should always be equal.
            // `self.pre_recv_seq_no < seq_no <= self.pre_recv_seq_no + self.recv_window.len()`.
            debug_assert_eq!(seq_no, *pre_seq_no);
            true
        } else {
            false
        };
        // If the send window is full we cannot safely process received packets,
        // because there would be no way to reply.
        // We can only process this packet if processing it would make space in the send window.
        let (wait_recv, wait_reply) = match self.is_full_inner(reply_no) {
            Ok(()) => (seq_cst && !is_next, seq_cst && self.is_locked),
            Err(TryError::WaitingForRecv) => (true, false),
            Err(TryError::WaitingForReply) => (false, true),
        };
        if wait_recv || wait_reply {
            if !is_duplicate {
                self.recv_window[i] = RecvEntry::Occupied { seq_no, reply_no, seq_cst, data: recv_data.into() }
            }
            return if wait_recv {
                Err(TryRecvError::WaitingForRecv)
            } else {
                Err(TryRecvError::WaitingForReply)
            };
        }

        let do_pump = if is_next {
            self.recv_window[i] = RecvEntry::Empty;
            self.next_recv_seq_no = seq_no.wrapping_add(1);
            self.fast_forward()
        } else {
            debug_assert!(!seq_cst);
            self.recv_window[i] = RecvEntry::Unlocked { seq_no };
            false
        };
        self.concurrent_replies[self.concurrent_replies_total] = seq_no;
        self.concurrent_replies_total += 1;
        if seq_cst {
            debug_assert!(!self.is_locked);
            self.is_locked = true;
        }
        if let Some(send_data) = reply_no.and_then(|r| self.take_send(r)) {
            Ok((RecvOkRaw::Reply { reply_no: seq_no, recv_data, send_data, seq_cst }, do_pump))
        } else {
            Ok((RecvOkRaw::Payload { reply_no: seq_no, recv_data, seq_cst }, do_pump))
        }
    }
    /// If this returns `Ok` then `try_send` might succeed on next call.
    pub fn try_pump_raw(&mut self) -> Result<(RecvOkRaw<SendData, RecvData>, bool), TryError> {
        let next_seq_no = self.next_recv_seq_no;
        let i = next_seq_no as usize % self.recv_window.len();
        if let RecvEntry::Occupied { seq_no, reply_no, seq_cst, .. } = &self.recv_window[i] {
            debug_assert_eq!(*seq_no, next_seq_no);
            // We cannot safely reserve a reply no if the window is full.
            self.is_full_inner(*reply_no)?;

            if !*seq_cst || !self.is_locked {
                let mut entry = RecvEntry::Empty;
                core::mem::swap(&mut entry, &mut self.recv_window[i]);
                if let RecvEntry::Occupied { seq_no, reply_no, seq_cst, data } = entry {
                    self.next_recv_seq_no = next_seq_no.wrapping_add(1);
                    let do_pump = self.fast_forward();
                    self.concurrent_replies[self.concurrent_replies_total] = seq_no;
                    self.concurrent_replies_total += 1;
                    if seq_cst {
                        debug_assert!(!self.is_locked);
                        self.is_locked = true;
                    }
                    let ret = if let Some(send_data) = reply_no.and_then(|r| self.take_send(r)) {
                        RecvOkRaw::Reply { reply_no: seq_no, seq_cst, recv_data: data, send_data }
                    } else {
                        RecvOkRaw::Payload { reply_no: seq_no, seq_cst, recv_data: data }
                    };
                    return Ok((ret, do_pump));
                } else {
                    unreachable!();
                }
            } else {
                return Err(TryError::WaitingForReply);
            }
        }
        Err(TryError::WaitingForRecv)
    }
    /// This function must be passed a reply number given by `receive_raw` or `pump_raw`, otherwise
    /// it will do nothing. This reply number can only be used to reply once.
    ///
    /// If you need to reply more than once, say to fragment a large file, then include in your
    /// first reply some identifier, and then `send` all fragments with the same included identifier.
    /// The identifier will tell the remote peer which packets contain fragments of the file,
    /// and since each fragment will be received in order it will be trivial for them to reconstruct
    /// the original file.
    ///
    /// Can mutate `next_service_timestamp`.
    /// If `unlock` is true and the return value is `Some` pump may return new values.
    #[must_use]
    pub fn reply_raw_and_direct(
        &mut self,
        current_time: i64,
        reply_no: SeqNo,
        unlock: bool,
        seq_cst: bool,
        packet_data: SendData,
    ) -> Option<Packet<&SendData>> {
        if self.remove_reservation(reply_no) {
            let seq_no = self.next_send_seq_no;
            self.next_send_seq_no = self.next_send_seq_no.wrapping_add(1);

            let next_resend_time = current_time + self.resend_interval;
            if self.next_service_timestamp > next_resend_time {
                self.next_service_timestamp = next_resend_time;
            }
            if unlock {
                debug_assert!(self.is_locked, "The window must be locked to attempt to unlock: double unlock detected.");
                self.is_locked = false;
            }
            let slot = self.send_window_slot_mut(seq_no);
            debug_assert!(slot.is_none());
            let entry = slot.insert(SendEntry {
                seq_no,
                reply_no: Some(reply_no),
                seq_cst,
                next_resend_time,
                data: packet_data,
            });

            Some(entry.to_packet())
        } else {
            None
        }
    }
    /// If `unlock` is true and the return value is `Some` pump may return new values.
    #[must_use]
    pub fn ack_raw_and_direct(&mut self, reply_no: SeqNo, unlock: bool) -> Option<Packet<&SendData>> {
        if self.remove_reservation(reply_no) {
            if unlock {
                debug_assert!(self.is_locked, "The window must be locked to attempt to unlock: double unlock detected.");
                self.is_locked = false;
            }
            Some(Ack(reply_no))
        } else {
            None
        }
    }

    /// Can mutate `next_service_timestamp`.
    pub fn service_direct(&mut self, current_time: i64, iter: &mut Option<ServiceIter>) -> Option<Packet<&SendData>> {
        if self.next_service_timestamp <= current_time {
            let iter = iter.get_or_insert(ServiceIter {
                seq_no: self.next_send_seq_no.wrapping_sub(self.send_window.len() as u32),
                next_time: i64::MAX,
            });
            while iter.seq_no != self.next_send_seq_no {
                let idx = iter.seq_no as usize % self.send_window.len();
                iter.seq_no = iter.seq_no.wrapping_add(1);
                if let Some(entry) = &mut self.send_window[idx] {
                    if entry.next_resend_time <= current_time {
                        entry.next_resend_time = current_time + self.resend_interval;
                        iter.next_time = iter.next_time.min(entry.next_resend_time);

                        return Some(self.send_window[idx].as_ref().unwrap().to_packet());
                    } else {
                        iter.next_time = iter.next_time.min(entry.next_resend_time);
                    }
                }
            }
            self.next_service_timestamp = iter.next_time;
        }
        None
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
