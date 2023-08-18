use crate::{DirectError, Error, Packet, RecvOkRaw, SeqEx, SeqNo, TransportLayer, DEFAULT_WINDOW_CAP};

pub struct ReplyGuard<'a, TL: TransportLayer<SendData>, SendData, RecvData, const CAP: usize = DEFAULT_WINDOW_CAP> {
    seq: &'a mut SeqEx<SendData, RecvData, CAP>,
    app: Option<TL>,
    reply_no: SeqNo,
}
impl<'a, TL: TransportLayer<SendData>, SendData, RecvData, const CAP: usize> ReplyGuard<'a, TL, SendData, RecvData, CAP> {
    /// If you need to reply more than once, say to fragment a large file, then include in your
    /// first reply some identifier, and then `send` all fragments with the same included identifier.
    /// The identifier will tell the remote peer which packets contain fragments of the file,
    /// and since each fragment will be received in order it will be trivial for them to reconstruct
    /// the original file.
    pub fn reply(mut self, packet_data: SendData) {
        let mut app = None;
        core::mem::swap(&mut app, &mut self.app);
        self.seq.reply_raw(app.unwrap(), self.reply_no, packet_data);
        core::mem::forget(self);
    }
}
impl<'a, TL: TransportLayer<SendData>, SendData, RecvData, const CAP: usize> Drop for ReplyGuard<'a, TL, SendData, RecvData, CAP> {
    fn drop(&mut self) {
        if let Some(app) = &mut self.app {
            if let Some(p) = self.seq.ack_raw_and_direct(self.reply_no) {
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

#[derive(Debug)]
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
impl<'a, TL: TransportLayer<SendData>, P, SendData, RecvData, const CAP: usize> RecvOk<'a, TL, P, SendData, RecvData, CAP> {
    pub fn from_raw(seq: &'a mut SeqEx<SendData, RecvData, CAP>, app: TL, value: RecvOkRaw<SendData, P>) -> Self {
        match value {
            RecvOkRaw::Payload { reply_no, recv_data } => RecvOk::Payload {
                reply_guard: ReplyGuard { seq, app: Some(app), reply_no },
                recv_data,
            },
            RecvOkRaw::Reply { reply_no, recv_data, send_data } => RecvOk::Reply {
                reply_guard: ReplyGuard { seq, app: Some(app), reply_no },
                recv_data,
                send_data,
            },
            RecvOkRaw::Ack { send_data } => RecvOk::Ack { send_data },
        }
    }
    pub fn consume(self) -> (Option<(ReplyGuard<'a, TL, SendData, RecvData, CAP>, P)>, Option<SendData>) {
        match self {
            RecvOk::Payload { reply_guard, recv_data } => (Some((reply_guard, recv_data)), None),
            RecvOk::Reply { reply_guard, recv_data, send_data } => (Some((reply_guard, recv_data)), Some(send_data)),
            RecvOk::Ack { send_data } => (None, Some(send_data)),
        }
    }
    pub fn new(recv_data: Option<(ReplyGuard<'a, TL, SendData, RecvData, CAP>, P)>, send_data: Option<SendData>) -> Option<Self> {
        match (recv_data, send_data) {
            (Some((reply_guard, recv_data)), None) => Some(RecvOk::Payload { reply_guard, recv_data }),
            (Some((reply_guard, recv_data)), Some(send_data)) => Some(RecvOk::Reply { reply_guard, recv_data, send_data }),
            (None, Some(send_data)) => Some(RecvOk::Ack { send_data }),
            (None, None) => None,
        }
    }
}
impl<'a, TL: TransportLayer<SendData>, P: Into<RecvData>, SendData, RecvData, const CAP: usize> RecvOk<'a, TL, P, SendData, RecvData, CAP> {
    pub fn into(self) -> RecvOk<'a, TL, RecvData, SendData, RecvData, CAP> {
        match self {
            RecvOk::Payload { reply_guard, recv_data } => RecvOk::Payload { reply_guard, recv_data: recv_data.into() },
            RecvOk::Reply { reply_guard, recv_data, send_data } => RecvOk::Reply { reply_guard, recv_data: recv_data.into(), send_data },
            RecvOk::Ack { send_data } => RecvOk::Ack { send_data },
        }
    }
}

impl<SendData, RecvData, const CAP: usize> SeqEx<SendData, RecvData, CAP> {
    pub fn try_send(&mut self, mut app: impl TransportLayer<SendData>, packet_data: SendData) -> Result<(), SendData> {
        match self.try_send_direct(packet_data, app.time()) {
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
    ) -> Result<crate::seq_queue::RecvOkRaw<SendData, P>, Error> {
        match self.receive_raw_and_direct(packet) {
            Ok(a) => Ok(a),
            Err(DirectError::ResendAck(reply_no)) => {
                app.send(Packet::Ack(reply_no));
                Err(Error::OutOfSequence)
            }
            Err(DirectError::OutOfSequence) => Err(Error::OutOfSequence),
            Err(DirectError::WindowIsFull) => Err(Error::WindowIsFull),
        }
    }
    pub fn reply_raw(&mut self, mut app: impl TransportLayer<SendData>, reply_no: SeqNo, packet_data: SendData) {
        if let Some(p) = self.reply_raw_and_direct(reply_no, packet_data, app.time()) {
            app.send(p)
        }
    }
    pub fn ack_raw(&mut self, mut app: impl TransportLayer<SendData>, reply_no: SeqNo) {
        if let Some(p) = self.ack_raw_and_direct(reply_no) {
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
    ) -> Result<RecvOk<'_, TL, P, SendData, RecvData, CAP>, Error> {
        self.receive_raw(app.clone(), packet).map(|r| RecvOk::from_raw(self, app, r))
    }
    pub fn pump<TL: TransportLayer<SendData>>(&mut self, app: TL) -> Result<RecvOk<'_, TL, RecvData, SendData, RecvData, CAP>, Error> {
        self.pump_raw().map(|r| RecvOk::from_raw(self, app, r))
    }
}
