use crate::{Error, SeqEx, SeqNo, TransportLayer, DEFAULT_WINDOW_CAP, Payload};

pub struct ReplyGuard<'a, TL: TransportLayer<SendData>, SendData, RecvData, const CAP: usize = DEFAULT_WINDOW_CAP>(
    &'a mut SeqEx<SendData, RecvData, CAP>,
    TL,
    SeqNo,
);
impl<'a, TL: TransportLayer<SendData>, SendData, RecvData, const CAP: usize> ReplyGuard<'a, TL, SendData, RecvData, CAP> {
    /// If you need to reply more than once, say to fragment a large file, then include in your
    /// first reply some identifier, and then `send` all fragments with the same included identifier.
    /// The identifier will tell the remote peer which packets contain fragments of the file,
    /// and since each fragment will be received in order it will be trivial for them to reconstruct
    /// the original file.
    pub fn reply(mut self, packet_data: SendData) {
        self.0.reply_raw(self.1, self.2, packet_data);
        core::mem::forget(self);
    }
}
impl<'a, TL: TransportLayer<SendData>, SendData, RecvData, const CAP: usize> Drop for ReplyGuard<'a, TL, SendData, RecvData, CAP> {
    fn drop(&mut self) {
        if self.0.ack_direct(self.2) {
            self.1.send_ack(self.2)
        }
    }
}

pub struct RecvSuccess<'a, TL: TransportLayer<SendData>, P: Into<RecvData>, SendData, RecvData, const CAP: usize = DEFAULT_WINDOW_CAP> {
    pub guard: ReplyGuard<'a, TL, SendData, RecvData, CAP>,
    pub packet: P,
    pub send_data: Option<SendData>,
}

impl<SendData, RecvData, const CAP: usize> SeqEx<SendData, RecvData, CAP> {
    /// If this returns `Ok` then `try_send` might succeed on next call.
    pub fn receive_raw<P: Into<RecvData>>(
        &mut self,
        mut app: impl TransportLayer<SendData>,
        seq_no: SeqNo,
        reply_no: Option<SeqNo>,
        packet: P,
    ) -> Result<(SeqNo, P, Option<SendData>), Error> {
        let ret = self.receive_direct(seq_no, reply_no, packet);
    }
    pub fn reply_raw(&mut self, mut app: impl TransportLayer<SendData>, reply_no: SeqNo, packet_data: SendData) {
        if let Some(Payload { seq_no, reply_no, data }) = self.reply_direct(reply_no, packet_data, app.time()) {
            app.send(seq_no, reply_no, data)
        }
    }
    pub fn ack_raw(&mut self, mut app: impl TransportLayer<SendData>, reply_no: SeqNo) {
        if self.ack_direct(reply_no) {
            app.send_ack(reply_no)
        }
    }
    pub fn receive<TL: TransportLayer<SendData>, P: Into<RecvData>>(
        &mut self,
        app: TL,
        seq_no: SeqNo,
        reply_no: Option<SeqNo>,
        packet: P,
    ) -> Result<RecvSuccess<'_, TL, P, SendData, RecvData, CAP>, Error> {
        self.receive_raw(app, seq_no, reply_no, packet)
            .map(|(reply_no, packet, send_data)| RecvSuccess { guard: ReplyGuard(self, app, reply_no), packet, send_data })
    }
    pub fn pump<TL: TransportLayer<SendData>>(&mut self, app: TL) -> Result<RecvSuccess<'_, TL, RecvData, SendData, RecvData, CAP>, Error> {
        self.pump_raw()
            .map(|(reply_no, packet, send_data)| RecvSuccess { guard: ReplyGuard(self, app, reply_no), packet, send_data })
    }
}
