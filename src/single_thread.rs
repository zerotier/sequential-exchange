use crate::{DirectError, Error, Packet, SeqEx, SeqNo, TransportLayer, DEFAULT_WINDOW_CAP};

pub struct ReplyGuard<'a, TL: TransportLayer<SendData>, SendData, RecvData, const CAP: usize = DEFAULT_WINDOW_CAP>(
    &'a mut SeqEx<SendData, RecvData, CAP>,
    Option<TL>,
    SeqNo,
);
impl<'a, TL: TransportLayer<SendData>, SendData, RecvData, const CAP: usize> ReplyGuard<'a, TL, SendData, RecvData, CAP> {
    /// If you need to reply more than once, say to fragment a large file, then include in your
    /// first reply some identifier, and then `send` all fragments with the same included identifier.
    /// The identifier will tell the remote peer which packets contain fragments of the file,
    /// and since each fragment will be received in order it will be trivial for them to reconstruct
    /// the original file.
    pub fn reply(mut self, packet_data: SendData) {
        let mut app = None;
        core::mem::swap(&mut app, &mut self.1);
        self.0.reply_raw(app.unwrap(), self.2, packet_data);
        core::mem::forget(self);
    }
}
impl<'a, TL: TransportLayer<SendData>, SendData, RecvData, const CAP: usize> Drop for ReplyGuard<'a, TL, SendData, RecvData, CAP> {
    fn drop(&mut self) {
        if let Some(app) = &mut self.1 {
            if let Some(p) = self.0.ack_direct(self.2) {
                app.send(p)
            }
        }
    }
}

pub struct RecvSuccess<'a, TL: TransportLayer<SendData>, P: Into<RecvData>, SendData, RecvData, const CAP: usize = DEFAULT_WINDOW_CAP> {
    pub guard: ReplyGuard<'a, TL, SendData, RecvData, CAP>,
    pub packet: P,
    pub send_data: Option<SendData>,
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
        seq_no: SeqNo,
        reply_no: Option<SeqNo>,
        packet: P,
    ) -> Result<(SeqNo, P, Option<SendData>), Error> {
        match self.receive_direct(seq_no, reply_no, packet) {
            Ok(a) => Ok(a),
            Err(DirectError::ResendAck(reply_no)) => {
                app.send(Packet::Ack { reply_no });
                Err(Error::OutOfSequence)
            }
            Err(DirectError::OutOfSequence) => Err(Error::OutOfSequence),
            Err(DirectError::WindowIsFull) => Err(Error::WindowIsFull),
        }
    }
    pub fn reply_raw(&mut self, mut app: impl TransportLayer<SendData>, reply_no: SeqNo, packet_data: SendData) {
        if let Some(p) = self.reply_direct(reply_no, packet_data, app.time()) {
            app.send(p)
        }
    }
    pub fn ack_raw(&mut self, mut app: impl TransportLayer<SendData>, reply_no: SeqNo) {
        if let Some(p) = self.ack_direct(reply_no) {
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
        seq_no: SeqNo,
        reply_no: Option<SeqNo>,
        packet: P,
    ) -> Result<RecvSuccess<'_, TL, P, SendData, RecvData, CAP>, Error> {
        self.receive_raw(app.clone(), seq_no, reply_no, packet)
            .map(|(reply_no, packet, send_data)| RecvSuccess {
                guard: ReplyGuard(self, Some(app), reply_no),
                packet,
                send_data,
            })
    }
    pub fn pump<TL: TransportLayer<SendData>>(&mut self, app: TL) -> Result<RecvSuccess<'_, TL, RecvData, SendData, RecvData, CAP>, Error> {
        self.pump_raw().map(|(reply_no, packet, send_data)| RecvSuccess {
            guard: ReplyGuard(self, Some(app), reply_no),
            packet,
            send_data,
        })
    }
}
