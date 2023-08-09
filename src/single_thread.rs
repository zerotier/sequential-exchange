use crate::{Error, SeqEx, SeqNo, TransportLayer};

pub struct ReplyGuard<'a, TL: TransportLayer>(&'a mut SeqEx<TL>, TL, SeqNo);
impl<'a, TL: TransportLayer> ReplyGuard<'a, TL> {
    /// If you need to reply more than once, say to fragment a large file, then include in your
    /// first reply some identifier, and then `send` all fragments with the same included identifier.
    /// The identifier will tell the remote peer which packets contain fragments of the file,
    /// and since each fragment will be received in order it will be trivial for them to reconstruct
    /// the original file.
    pub fn reply(self, packet_data: TL::SendData) {
        self.0.reply_raw(self.1.clone(), self.2, packet_data);
        core::mem::forget(self);
    }
}
impl<'a, TL: TransportLayer> Drop for ReplyGuard<'a, TL> {
    fn drop(&mut self) {
        self.0.reply_empty_raw(self.1.clone(), self.2)
    }
}

pub struct RecvSuccess<'a, TL: TransportLayer, P> {
    pub guard: ReplyGuard<'a, TL>,
    pub packet: P,
    pub send_data: Option<TL::SendData>,
}

impl<TL: TransportLayer> SeqEx<TL> {
    pub fn receive<P: Into<TL::RecvData>>(
        &mut self,
        app: TL,
        seq_no: SeqNo,
        reply_no: Option<SeqNo>,
        packet: P,
    ) -> Result<RecvSuccess<'_, TL, P>, Error> {
        self.receive_raw(app.clone(), seq_no, reply_no, packet)
            .map(|(reply_no, packet, send_data)| RecvSuccess { guard: ReplyGuard(self, app, reply_no), packet, send_data })
    }
    pub fn pump(&mut self, app: TL) -> Result<RecvSuccess<'_, TL, TL::RecvData>, Error> {
        self.pump_raw()
            .map(|(reply_no, packet, send_data)| RecvSuccess { guard: ReplyGuard(self, app, reply_no), packet, send_data })
    }
}
