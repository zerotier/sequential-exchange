use crate::{Packet, RecvOkRaw, SeqEx, SeqNo, TransportLayer, TryError, TryRecvError, DEFAULT_WINDOW_CAP};

pub struct ReplyGuard<'a, TL: TransportLayer<SendData>, SendData, RecvData, const CAP: usize = DEFAULT_WINDOW_CAP> {
    seq: &'a mut SeqEx<SendData, RecvData, CAP>,
    app: Option<TL>,
    reply_no: SeqNo,
    is_holding_lock: bool,
}
impl<'a, TL: TransportLayer<SendData>, SendData, RecvData, const CAP: usize> ReplyGuard<'a, TL, SendData, RecvData, CAP> {
    /// If you need to reply more than once, say to fragment a large file, then include in your
    /// first reply some identifier, and then `send` all fragments with the same included identifier.
    /// The identifier will tell the remote peer which packets contain fragments of the file,
    /// and since each fragment will be received in order it will be trivial for them to reconstruct
    /// the original file.
    pub fn reply(self, seq_cst: bool, packet_data: SendData) {
        self.reply_with(seq_cst, |_, _| packet_data)
    }
    fn reply_with(mut self, seq_cst: bool, packet_data: impl FnOnce(SeqNo, SeqNo) -> SendData) {
        let app = self.app.take();
        let seq_no = self.seq.seq_no();
        self.seq.reply_raw(
            app.unwrap(),
            self.reply_no,
            self.is_holding_lock,
            seq_cst,
            packet_data(seq_no, self.reply_no),
        );
    }

    pub fn to_components(mut self) -> (TL, SeqNo, bool) {
        (self.app.take().unwrap(), self.reply_no, self.is_holding_lock)
    }
    pub unsafe fn from_components(seq: &'a mut SeqEx<SendData, RecvData, CAP>, app: TL, reply_no: SeqNo, is_holding_lock: bool) -> Self {
        ReplyGuard { seq, app: Some(app), reply_no, is_holding_lock }
    }
}
impl<'a, TL: TransportLayer<SendData>, SendData, RecvData, const CAP: usize> Drop for ReplyGuard<'a, TL, SendData, RecvData, CAP> {
    fn drop(&mut self) {
        if let Some(app) = &mut self.app {
            if let Some(p) = self.seq.ack_raw_and_direct(self.reply_no, self.is_holding_lock) {
                app.send(p)
            }
        }
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecvError {
    DroppedTooEarly,
    DroppedDuplicate,
    WaitingForRecv,
    WaitingForReply,
}
#[cfg(feature = "std")]
impl std::fmt::Display for RecvError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RecvError::DroppedTooEarly => write!(f, "packet arrived too early"),
            RecvError::DroppedDuplicate => write!(f, "packet was a duplicate"),
            RecvError::WaitingForRecv => write!(f, "can't process until another packet is received"),
            RecvError::WaitingForReply => write!(f, "can't process until a reply is finished"),
        }
    }
}
#[cfg(feature = "std")]
impl std::error::Error for RecvError {}

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
            fn from_raw(seq: $seq_ex, app: TL, value: RecvOkRaw<SendData, RecvData>) -> Self {
                match value {
                    RecvOkRaw::Payload { reply_no, seq_cst, recv_data } => Self::Payload {
                        reply_guard: ReplyGuard { seq, app: Some(app), reply_no, is_holding_lock: seq_cst },
                        recv_data,
                    },
                    RecvOkRaw::Reply { reply_no, seq_cst, recv_data, send_data } => Self::Reply {
                        reply_guard: ReplyGuard { seq, app: Some(app), reply_no, is_holding_lock: seq_cst },
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
    /// Can mutate `next_service_timestamp`.
    pub fn try_send(&mut self, mut app: impl TransportLayer<SendData>, seq_cst: bool, packet_data: SendData) -> Result<(), (TryError, SendData)> {
        match self.try_send_direct(app.time(), seq_cst, packet_data) {
            Ok(p) => {
                app.send(p);
                Ok(())
            }
            Err(e) => Err(e),
        }
    }
    /// Can mutate `next_service_timestamp`.
    pub fn try_send_with<F: FnOnce(SeqNo) -> SendData>(
        &mut self,
        mut app: impl TransportLayer<SendData>,
        seq_cst: bool,
        packet_data: F,
    ) -> Result<(), (TryError, F)> {
        match self.try_send_direct_with(app.time(), seq_cst, packet_data) {
            Ok(p) => {
                app.send(p);
                Ok(())
            }
            Err(e) => Err(e),
        }
    }
    /// If this returns `Ok` then `try_send` might succeed on next call.
    pub fn receive_raw<P: Into<RecvData>>(
        &mut self,
        mut app: impl TransportLayer<SendData>,
        packet: Packet<P>,
    ) -> Result<(RecvOkRaw<SendData, P>, bool), RecvError> {
        match self.receive_raw_and_direct(packet) {
            Ok(a) => Ok(a),
            Err(TryRecvError::DroppedDuplicateResendAck(reply_no)) => {
                app.send(Packet::Ack(reply_no));
                Err(RecvError::DroppedDuplicate)
            }
            Err(TryRecvError::DroppedTooEarly) => Err(RecvError::DroppedTooEarly),
            Err(TryRecvError::DroppedDuplicate) => Err(RecvError::DroppedDuplicate),
            Err(TryRecvError::WaitingForRecv) => Err(RecvError::WaitingForRecv),
            Err(TryRecvError::WaitingForReply) => Err(RecvError::WaitingForReply),
        }
    }
    /// Can mutate `next_service_timestamp`.
    /// If `unlock` is true and the return value is true pump may return new values.
    ///
    /// Only returns false if the reply number was incorrect or used twice.
    pub fn reply_raw(&mut self, mut app: impl TransportLayer<SendData>, reply_no: SeqNo, unlock: bool, seq_cst: bool, packet_data: SendData) -> bool {
        if let Some(p) = self.reply_raw_and_direct(app.time(), reply_no, unlock, seq_cst, packet_data) {
            app.send(p);
            true
        } else {
            false
        }
    }
    /// If `unlock` is true and the return value is true pump may return new values.
    pub fn ack_raw(&mut self, mut app: impl TransportLayer<SendData>, reply_no: SeqNo, unlock: bool) -> bool {
        if let Some(p) = self.ack_raw_and_direct(reply_no, unlock) {
            app.send(p);
            true
        } else {
            false
        }
    }
    /// Can mutate `next_service_timestamp`.
    pub fn service(&mut self, mut app: impl TransportLayer<SendData>) -> i64 {
        let current_time = app.time();
        let mut iter = None;
        while let Some(p) = self.service_direct(current_time, &mut iter) {
            app.send(p)
        }
        self.resend_interval.min(self.next_service_timestamp - current_time)
    }
    pub fn receive<TL: TransportLayer<SendData>>(
        &mut self,
        app: TL,
        packet: Packet<RecvData>,
    ) -> Result<(RecvOk<'_, TL, SendData, RecvData, CAP>, bool), RecvError> {
        self.receive_raw(app.clone(), packet)
            .map(|(r, do_pump)| (RecvOk::from_raw(self, app, r), do_pump))
    }
    pub fn try_pump<TL: TransportLayer<SendData>>(&mut self, app: TL) -> Result<(RecvOk<'_, TL, SendData, RecvData, CAP>, bool), TryError> {
        self.try_pump_raw().map(|(r, do_pump)| (RecvOk::from_raw(self, app, r), do_pump))
    }
}
