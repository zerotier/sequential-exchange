use crate::error::{RecvError, TryError, TryRecvError};
use crate::no_std::{RecvOkRaw, SeqEx};
use crate::{Packet, SeqNo, TransportLayer, DEFAULT_WINDOW_CAP};

pub struct ReplyGuard<'a, TL: TransportLayer<SendData>, SendData, RecvData, const CAP: usize = DEFAULT_WINDOW_CAP> {
    seq: &'a mut SeqEx<SendData, RecvData, CAP>,
    tl: TL,
    reply_no: SeqNo,
    is_holding_lock: bool,
}

pub struct SeqCstGuard<'a, SendData, RecvData, const CAP: usize = DEFAULT_WINDOW_CAP> {
    seq: &'a mut SeqEx<SendData, RecvData, CAP>,
}

impl<'a, TL: TransportLayer<SendData>, SendData, RecvData, const CAP: usize> ReplyGuard<'a, TL, SendData, RecvData, CAP> {
    pub fn get_seqex(&'a mut self) -> &'a mut SeqEx<SendData, RecvData, CAP> {
        self.seq
    }
    pub fn get_tl(&self) -> &TL {
        &self.tl
    }
    pub fn get_tl_mut(&mut self) -> &mut TL {
        &mut self.tl
    }
    pub fn is_seq_cst(&self) -> bool {
        self.is_holding_lock
    }
    /// If you need to reply more than once, say to fragment a large file, then include in your
    /// first reply some identifier, and then `send` all fragments with the same included identifier.
    /// The identifier will tell the remote peer which packets contain fragments of the file,
    /// and since each fragment will be received in order it will be trivial for them to reconstruct
    /// the original file.
    pub fn reply(self, seq_cst: bool, packet_data: SendData) {
        self.reply_with(seq_cst, |_, _| packet_data)
    }
    pub fn reply_with(self, seq_cst: bool, packet_data: impl FnOnce(SeqNo, SeqNo) -> SendData) {
        let seq_no = self.seq.seq_no();
        self.seq
            .reply_raw(self.tl, self.reply_no, self.is_holding_lock, seq_cst, packet_data(seq_no, self.reply_no));
        core::mem::forget(self);
    }

    fn consume_lock(self) -> Option<SeqCstGuard<'a, SendData, RecvData, CAP>> {
        let ret = if self.is_holding_lock {
            let seq = self.seq as *mut SeqEx<SendData, RecvData, CAP>;
            Some(SeqCstGuard { seq: unsafe { seq.as_mut().unwrap_unchecked() } })
        } else {
            None
        };
        core::mem::forget(self);
        ret
    }
    pub fn ack(self) -> Option<SeqCstGuard<'a, SendData, RecvData, CAP>> {
        self.seq.ack_raw(self.tl, self.reply_no, false);
        self.consume_lock()
    }
    pub fn unlock(&mut self) -> bool {
        if self.is_holding_lock {
            self.is_holding_lock = false;
            self.seq.unlock_raw();
            true
        } else {
            false
        }
    }
    pub fn reply_stay_locked(self, seq_cst: bool, packet_data: SendData) -> Option<SeqCstGuard<'a, SendData, RecvData, CAP>> {
        self.reply_with_stay_locked(seq_cst, |_, _| packet_data)
    }
    pub fn reply_with_stay_locked(
        self,
        seq_cst: bool,
        packet_data: impl FnOnce(SeqNo, SeqNo) -> SendData,
    ) -> Option<SeqCstGuard<'a, SendData, RecvData, CAP>> {
        let seq_no = self.seq.seq_no();
        self.seq
            .reply_raw(self.tl, self.reply_no, false, seq_cst, packet_data(seq_no, self.reply_no));
        self.consume_lock()
    }

    pub unsafe fn to_components(self) -> (SeqNo, bool) {
        let ret = (self.reply_no, self.is_holding_lock);
        core::mem::forget(self);
        ret
    }
    fn new(seq: &'a mut SeqEx<SendData, RecvData, CAP>, tl: TL, reply_no: SeqNo, is_holding_lock: bool) -> Self {
        ReplyGuard { seq, tl, reply_no, is_holding_lock }
    }
    pub unsafe fn from_components(seq: &'a mut SeqEx<SendData, RecvData, CAP>, tl: TL, reply_no: SeqNo, is_holding_lock: bool) -> Self {
        Self::new(seq, tl, reply_no, is_holding_lock)
    }
}
impl<'a, TL: TransportLayer<SendData>, SendData, RecvData, const CAP: usize> Drop for ReplyGuard<'a, TL, SendData, RecvData, CAP> {
    fn drop(&mut self) {
        self.seq.ack_raw(self.tl, self.reply_no, self.is_holding_lock);
    }
}
impl<'a, SendData, RecvData, const CAP: usize> Drop for SeqCstGuard<'a, SendData, RecvData, CAP> {
    fn drop(&mut self) {
        self.seq.unlock_raw();
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
macro_rules! impl_recvok {
    ($recv:tt, $seq_ex:ty) => {
        #[cfg(feature = "std")]
        impl<'a, TL: TransportLayer<SendData>, SendData: std::fmt::Debug, RecvData: std::fmt::Debug, const CAP: usize> std::fmt::Debug
            for $recv<'a, TL, SendData, RecvData, CAP>
        {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                match self {
                    Self::Payload { reply_guard, recv_data } => f
                        .debug_struct("Payload")
                        .field("reply_guard", reply_guard)
                        .field("recv_data", recv_data)
                        .finish(),
                    Self::Reply { reply_guard, recv_data, send_data } => f
                        .debug_struct("Reply")
                        .field("reply_guard", reply_guard)
                        .field("recv_data", recv_data)
                        .field("send_data", send_data)
                        .finish(),
                    Self::Ack { send_data } => f.debug_struct("Ack").field("send_data", send_data).finish(),
                }
            }
        }
        impl<'a, TL: TransportLayer<SendData>, SendData, RecvData, const CAP: usize> $recv<'a, TL, SendData, RecvData, CAP> {
            fn from_raw(seq: $seq_ex, tl: TL, value: RecvOkRaw<SendData, RecvData>) -> Self {
                match value {
                    RecvOkRaw::Payload { reply_no, seq_cst, recv_data } => Self::Payload {
                        reply_guard: ReplyGuard::new(seq, tl, reply_no, seq_cst),
                        recv_data,
                    },
                    RecvOkRaw::Reply { reply_no, seq_cst, recv_data, send_data } => Self::Reply {
                        reply_guard: ReplyGuard::new(seq, tl, reply_no, seq_cst),
                        recv_data,
                        send_data,
                    },
                    RecvOkRaw::Ack { send_data } => Self::Ack { send_data },
                }
            }
            pub fn consume(self) -> (Option<(ReplyGuard<'a, TL, SendData, RecvData, CAP>, RecvData)>, Option<SendData>) {
                match self {
                    Self::Payload { reply_guard, recv_data } => (Some((reply_guard, recv_data)), None),
                    Self::Reply { reply_guard, recv_data, send_data } => (Some((reply_guard, recv_data)), Some(send_data)),
                    Self::Ack { send_data } => (None, Some(send_data)),
                }
            }
            pub fn new(recv_data: Option<(ReplyGuard<'a, TL, SendData, RecvData, CAP>, RecvData)>, send_data: Option<SendData>) -> Option<Self> {
                match (recv_data, send_data) {
                    (Some((reply_guard, recv_data)), None) => Some(Self::Payload { reply_guard, recv_data }),
                    (Some((reply_guard, recv_data)), Some(send_data)) => Some(Self::Reply { reply_guard, recv_data, send_data }),
                    (None, Some(send_data)) => Some(Self::Ack { send_data }),
                    (None, None) => None,
                }
            }
        }
    };
}
impl_recvok!(RecvOk, &'a mut SeqEx<SendData, RecvData, CAP>);
pub(crate) use impl_recvok;

impl<SendData, RecvData, const CAP: usize> SeqEx<SendData, RecvData, CAP> {
    /// Can decrease `next_service_timestamp`.
    pub fn try_send(&mut self, mut tl: impl TransportLayer<SendData>, seq_cst: bool, packet_data: SendData) -> Result<(), (TryError, SendData)> {
        match self.try_send_direct(tl.time(), seq_cst, packet_data) {
            Ok(p) => {
                tl.send(p);
                Ok(())
            }
            Err(e) => Err(e),
        }
    }
    /// Can decrease `next_service_timestamp`.
    pub fn try_send_with<F: FnOnce(SeqNo) -> SendData>(
        &mut self,
        mut tl: impl TransportLayer<SendData>,
        seq_cst: bool,
        packet_data: F,
    ) -> Result<(), (TryError, F)> {
        match self.try_send_direct_with(tl.time(), seq_cst, packet_data) {
            Ok(p) => {
                tl.send(p);
                Ok(())
            }
            Err(e) => Err(e),
        }
    }
    /// If this returns `Ok` then `try_send` might succeed on next call.
    pub fn receive_raw<P: Into<RecvData>>(
        &mut self,
        mut tl: impl TransportLayer<SendData>,
        packet: Packet<P>,
    ) -> Result<(RecvOkRaw<SendData, P>, bool), RecvError> {
        match self.receive_raw_and_direct(packet) {
            Ok(a) => Ok(a),
            Err(TryRecvError::DroppedDuplicateResendAck(reply_no)) => {
                tl.send(Packet::Ack(reply_no));
                Err(RecvError::DroppedDuplicate)
            }
            Err(TryRecvError::DroppedTooEarly) => Err(RecvError::DroppedTooEarly),
            Err(TryRecvError::DroppedDuplicate) => Err(RecvError::DroppedDuplicate),
            Err(TryRecvError::WaitingForRecv) => Err(RecvError::WaitingForRecv),
            Err(TryRecvError::WaitingForReply) => Err(RecvError::WaitingForReply),
        }
    }
    /// Can decrease `next_service_timestamp`.
    /// If `unlock` is true and the return value is true pump may return new values.
    ///
    /// Only returns false if the reply number was incorrect or used twice.
    pub fn reply_raw(&mut self, mut tl: impl TransportLayer<SendData>, reply_no: SeqNo, unlock: bool, seq_cst: bool, packet_data: SendData) -> bool {
        if let Some(p) = self.reply_raw_and_direct(tl.time(), reply_no, unlock, seq_cst, packet_data) {
            tl.send(p);
            true
        } else {
            false
        }
    }
    /// If `unlock` is true and the return value is true pump may return new values.
    pub fn ack_raw(&mut self, mut tl: impl TransportLayer<SendData>, reply_no: SeqNo, unlock: bool) -> bool {
        if let Some(p) = self.ack_raw_and_direct(reply_no, unlock) {
            tl.send(p);
            true
        } else {
            false
        }
    }
    /// Can increase `next_service_timestamp`.
    pub fn service(&mut self, mut tl: impl TransportLayer<SendData>) -> i64 {
        let current_time = tl.time();
        let mut iter = None;
        while let Some(p) = self.service_direct(current_time, &mut iter) {
            tl.send(p)
        }
        self.resend_interval.min(self.next_service_timestamp - current_time)
    }
    pub fn receive<TL: TransportLayer<SendData>>(
        &mut self,
        tl: TL,
        packet: Packet<RecvData>,
    ) -> Result<(RecvOk<'_, TL, SendData, RecvData, CAP>, bool), RecvError> {
        self.receive_raw(tl, packet).map(|(r, do_pump)| (RecvOk::from_raw(self, tl, r), do_pump))
    }
    pub fn try_pump<TL: TransportLayer<SendData>>(&mut self, tl: TL) -> Result<(RecvOk<'_, TL, SendData, RecvData, CAP>, bool), TryError> {
        self.try_pump_raw().map(|(r, do_pump)| (RecvOk::from_raw(self, tl, r), do_pump))
    }
}
