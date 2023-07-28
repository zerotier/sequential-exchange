use crate::{Error, SeqEx, SeqNo, TransportLayer};

pub struct ReplyGuard<'a, TL: TransportLayer>(&'a mut SeqEx<TL>, TL, SeqNo);
//impl<'a, TL: TransportLayer> ReplyGuard<'a, TL> {
//    pub fn reply_no(&self) {

//    }
//}

impl<TL: TransportLayer> SeqEx<TL> {
    pub fn receive_guarded<P: Into<TL::RecvData>, T>(
        &mut self,
        app: TL,
        seq_no: SeqNo,
        reply_no: Option<SeqNo>,
        packet: P,
    ) -> Result<(ReplyGuard<'_, TL>, P, Option<TL::SendData>), Error> {
        self.receive(app.clone(), seq_no, reply_no, packet).map(|(reply_no, packet, data)| (ReplyGuard(self, app, reply_no), packet, data))
    }
}
