use std::sync::{Mutex, MutexGuard};

use crate::{Error, SeqEx, SeqNo, TransportLayer};

pub struct SeqExLock<TL: TransportLayer>(pub Mutex<SeqEx<TL>>);

pub struct ReplyGuard<'a, TL: TransportLayer>(&'a SeqExLock<TL>, TL, SeqNo);
impl<'a, TL: TransportLayer> ReplyGuard<'a, TL> {
    pub fn reply(self, packet_data: impl FnOnce(SeqNo, SeqNo) -> TL::SendData) {
        let mut seq = self.0 .0.lock().unwrap();
        let p = packet_data(seq.seq_no(), self.2);
        seq.reply_raw(self.1.clone(), self.2, p);
        core::mem::forget(self);
    }
}
impl<'a, TL: TransportLayer> Drop for ReplyGuard<'a, TL> {
    fn drop(&mut self) {
        let mut seq = self.0 .0.lock().unwrap();
        seq.reply_empty_raw(self.1.clone(), self.2);
    }
}

impl<TL: TransportLayer> SeqExLock<TL> {
    pub fn receive<P: Into<TL::RecvData>, T>(
        &self,
        app: TL,
        seq_no: SeqNo,
        reply_no: Option<SeqNo>,
        packet: P,
    ) -> Result<(ReplyGuard<'_, TL>, P, Option<TL::SendData>), Error> {
        let mut seq = self.lock();
        seq.receive_raw(app.clone(), seq_no, reply_no, packet)
            .map(|(reply_no, packet, data)| (ReplyGuard(self, app, reply_no), packet, data))
    }
    pub fn pump(&self, app: TL) -> Result<(ReplyGuard<'_, TL>, TL::RecvData, Option<TL::SendData>), Error> {
        let mut seq = self.lock();
        seq.pump_raw()
            .map(|(reply_no, packet, data)| (ReplyGuard(self, app, reply_no), packet, data))
    }

    pub fn new(retry_interval: i64, initial_seq_no: SeqNo) -> Self {
        Self(Mutex::new(SeqEx::new(retry_interval, initial_seq_no)))
    }
    pub fn send(&self, app: TL, packet_data: impl FnOnce(SeqNo) -> TL::SendData) -> bool {
        let mut seq = self.lock();
        let p = packet_data(seq.seq_no());
        seq.send(app, p)
    }
    pub fn receive_ack(&self, reply_no: SeqNo) {
        self.lock().receive_ack(reply_no)
    }
    pub fn receive_empty_reply(&self, reply_no: SeqNo) -> Option<TL::SendData> {
        self.lock().receive_empty_reply(reply_no)
    }
    pub fn service(&self, app: TL) -> i64 {
        self.lock().service(app)
    }

    pub fn lock(&self) -> MutexGuard<SeqEx<TL>> {
        self.0.lock().unwrap()
    }
}
