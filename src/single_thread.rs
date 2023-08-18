use crate::{DirectError, Packet, PumpError, RecvOkRaw, SeqEx, SeqNo, TransportLayer, DEFAULT_WINDOW_CAP};

pub struct ReplyGuard<'a, TL: TransportLayer<SendData>, SendData, RecvData, const CAP: usize = DEFAULT_WINDOW_CAP> {
    seq: &'a mut SeqEx<SendData, RecvData, CAP>,
    app: Option<TL>,
    reply_no: SeqNo,
    locked: bool,
}
impl<'a, TL: TransportLayer<SendData>, SendData, RecvData, const CAP: usize> ReplyGuard<'a, TL, SendData, RecvData, CAP> {
    /// If you need to reply more than once, say to fragment a large file, then include in your
    /// first reply some identifier, and then `send` all fragments with the same included identifier.
    /// The identifier will tell the remote peer which packets contain fragments of the file,
    /// and since each fragment will be received in order it will be trivial for them to reconstruct
    /// the original file.
    pub fn reply(self, packet_data: SendData) {
        self.reply_inner(false, |_, _| packet_data)
    }
    pub fn reply_locked(self, packet_data: SendData) {
        self.reply_inner(true, |_, _| packet_data)
    }
    pub fn reply_with(self, packet_data: impl FnOnce(SeqNo, SeqNo) -> SendData) {
        self.reply_inner(false, packet_data)
    }
    pub fn reply_locked_with(self, packet_data: impl FnOnce(SeqNo, SeqNo) -> SendData) {
        self.reply_inner(true, packet_data)
    }
    fn reply_inner(mut self, locked: bool, packet_data: impl FnOnce(SeqNo, SeqNo) -> SendData) {
        let mut app = None;
        core::mem::swap(&mut app, &mut self.app);
        let seq_no = self.seq.seq_no();
        self.seq
            .reply_raw(app.unwrap(), self.reply_no, self.locked, locked, packet_data(seq_no, self.reply_no));
        core::mem::forget(self);
    }
}
impl<'a, TL: TransportLayer<SendData>, SendData, RecvData, const CAP: usize> Drop for ReplyGuard<'a, TL, SendData, RecvData, CAP> {
    fn drop(&mut self) {
        if let Some(app) = &mut self.app {
            if let Some(p) = self.seq.ack_raw_and_direct(self.reply_no, self.locked) {
                app.send(p)
            }
        }
    }
}
impl<'a, TL: TransportLayer<SendData>, SendData, RecvData, const CAP: usize> std::fmt::Debug for ReplyGuard<'a, TL, SendData, RecvData, CAP> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ReplyGuard").field("reply_no", &self.reply_no).finish()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error<RecvData> {
    /// The packet is out-of-sequence. It was either received too soon or too late and so it would be
    /// invalid to process it right now. No action needs to be taken by the caller.
    OutOfSequence,
    /// The Send Window is currently full. The received packet cannot be processed right now because
    /// it could cause the send window to overflow.
    WindowIsFull(Packet<RecvData>),
    WindowIsLocked(Packet<RecvData>),
}

pub enum RecvOk<'a, TL: TransportLayer<SendData>, P, SendData, RecvData, const CAP: usize = DEFAULT_WINDOW_CAP> {
    Payload {
        reply_guard: ReplyGuard<'a, TL, SendData, RecvData, CAP>,
        recv_data: P,
    },
    Reply {
        reply_guard: ReplyGuard<'a, TL, SendData, RecvData, CAP>,
        recv_data: P,
        send_data: SendData,
    },
    Ack {
        send_data: SendData,
    },
}
macro_rules! impl_recvok {
    ($recv:tt, $seq_ex:ty) => {
        #[cfg(feature = "std")]
        impl<'a, TL: TransportLayer<SendData>, P: std::fmt::Debug, SendData: std::fmt::Debug, RecvData, const CAP: usize> std::fmt::Debug
            for $recv<'a, TL, P, SendData, RecvData, CAP>
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
        impl<'a, TL: TransportLayer<SendData>, P, SendData, RecvData, const CAP: usize> $recv<'a, TL, P, SendData, RecvData, CAP> {
            pub fn from_raw(seq: $seq_ex, app: TL, value: RecvOkRaw<SendData, P>) -> Self {
                match value {
                    RecvOkRaw::Payload { reply_no, locked, recv_data } => Self::Payload {
                        reply_guard: ReplyGuard { seq, app: Some(app), reply_no, locked },
                        recv_data,
                    },
                    RecvOkRaw::Reply { reply_no, locked, recv_data, send_data } => Self::Reply {
                        reply_guard: ReplyGuard { seq, app: Some(app), reply_no, locked },
                        recv_data,
                        send_data,
                    },
                    RecvOkRaw::Ack { send_data } => Self::Ack { send_data },
                }
            }
            pub fn consume(self) -> (Option<(ReplyGuard<'a, TL, SendData, RecvData, CAP>, P)>, Option<SendData>) {
                match self {
                    Self::Payload { reply_guard, recv_data } => (Some((reply_guard, recv_data)), None),
                    Self::Reply { reply_guard, recv_data, send_data } => (Some((reply_guard, recv_data)), Some(send_data)),
                    Self::Ack { send_data } => (None, Some(send_data)),
                }
            }
            pub fn new(recv_data: Option<(ReplyGuard<'a, TL, SendData, RecvData, CAP>, P)>, send_data: Option<SendData>) -> Option<Self> {
                match (recv_data, send_data) {
                    (Some((reply_guard, recv_data)), None) => Some(Self::Payload { reply_guard, recv_data }),
                    (Some((reply_guard, recv_data)), Some(send_data)) => Some(Self::Reply { reply_guard, recv_data, send_data }),
                    (None, Some(send_data)) => Some(Self::Ack { send_data }),
                    (None, None) => None,
                }
            }
            pub fn map<R>(self, f: impl FnOnce(P) -> R) -> $recv<'a, TL, R, SendData, RecvData, CAP> {
                match self {
                    Self::Payload { reply_guard, recv_data } => $recv::Payload { reply_guard, recv_data: f(recv_data) },
                    Self::Reply { reply_guard, recv_data, send_data } => $recv::Reply { reply_guard, recv_data: f(recv_data), send_data },
                    Self::Ack { send_data } => $recv::Ack { send_data },
                }
            }
        }
        impl<'a, TL: TransportLayer<SendData>, P: Into<RecvData>, SendData, RecvData, const CAP: usize> $recv<'a, TL, P, SendData, RecvData, CAP> {
            pub fn into(self) -> $recv<'a, TL, RecvData, SendData, RecvData, CAP> {
                match self {
                    Self::Payload { reply_guard, recv_data } => $recv::Payload { reply_guard, recv_data: recv_data.into() },
                    Self::Reply { reply_guard, recv_data, send_data } => $recv::Reply { reply_guard, recv_data: recv_data.into(), send_data },
                    Self::Ack { send_data } => $recv::Ack { send_data },
                }
            }
        }
    };
}
impl_recvok!(RecvOk, &'a mut SeqEx<SendData, RecvData, CAP>);
pub(crate) use impl_recvok;

impl<SendData, RecvData, const CAP: usize> SeqEx<SendData, RecvData, CAP> {
    pub fn try_send(&mut self, mut app: impl TransportLayer<SendData>, locked: bool, packet_data: SendData) -> Result<(), SendData> {
        match self.try_send_direct(packet_data, app.time()) {
            Ok(mut p) => {
                p.set_locking(locked);
                app.send(p);
                Ok(())
            }
            Err(e) => Err(e),
        }
    }
    pub fn try_send_with(
        &mut self,
        mut app: impl TransportLayer<SendData>,
        locked: bool,
        packet_data: impl FnOnce(SeqNo) -> SendData,
    ) -> Result<(), ()> {
        match self.try_send_direct_with(packet_data, app.time()) {
            Ok(mut p) => {
                p.set_locking(locked);
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
    ) -> Result<crate::seq_queue::RecvOkRaw<SendData, P>, Error<P>> {
        match self.receive_raw_and_direct(packet) {
            Ok(a) => Ok(a),
            Err(DirectError::ResendAck(reply_no)) => {
                app.send(Packet::Ack(reply_no));
                Err(Error::OutOfSequence)
            }
            Err(DirectError::OutOfSequence) => Err(Error::OutOfSequence),
            Err(DirectError::WindowIsFull(p)) => Err(Error::WindowIsFull(p)),
            Err(DirectError::WindowIsLocked(p)) => Err(Error::WindowIsLocked(p)),
        }
    }
    pub fn reply_raw(&mut self, mut app: impl TransportLayer<SendData>, reply_no: SeqNo, unlock: bool, locked_packet: bool, packet_data: SendData) {
        if let Some(mut p) = self.reply_raw_and_direct(reply_no, unlock, packet_data, app.time()) {
            p.set_locking(locked_packet);
            app.send(p)
        }
    }
    pub fn ack_raw(&mut self, mut app: impl TransportLayer<SendData>, reply_no: SeqNo, unlock: bool) {
        if let Some(p) = self.ack_raw_and_direct(reply_no, unlock) {
            app.send(p)
        }
    }
    pub fn service(&mut self, mut app: impl TransportLayer<SendData>) -> i64 {
        let current_time = app.time();
        let mut iter = None;
        while let Some(p) = self.service_direct(current_time, &mut iter) {
            app.send(p)
        }
        self.resend_interval.min(self.next_service_timestamp - current_time)
    }
    pub fn receive<TL: TransportLayer<SendData>, P: Into<RecvData>>(
        &mut self,
        app: TL,
        packet: Packet<P>,
    ) -> Result<RecvOk<'_, TL, P, SendData, RecvData, CAP>, Error<P>> {
        self.receive_raw(app.clone(), packet).map(|r| RecvOk::from_raw(self, app, r))
    }
    pub fn pump<TL: TransportLayer<SendData>>(&mut self, app: TL) -> Result<RecvOk<'_, TL, RecvData, SendData, RecvData, CAP>, PumpError> {
        self.pump_raw().map(|r| RecvOk::from_raw(self, app, r))
    }
}
