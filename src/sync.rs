use std::{
    sync::{
        mpsc::{channel, Receiver, Sender},
        Condvar, Mutex,
    },
    time::Instant,
};

use crate::{
    Packet, PumpError, RecvError, RecvOkRaw, SeqEx, SeqNo, TransportLayer, DEFAULT_INITIAL_SEQ_NO, DEFAULT_RESEND_INTERVAL_MS, DEFAULT_WINDOW_CAP,
};

pub struct SeqExSync<SendData, RecvData, const CAP: usize = DEFAULT_WINDOW_CAP> {
    seq_ex: Mutex<(SeqEx<SendData, RecvData, CAP>, usize, bool)>,
    send_block: Condvar,
    reply_block: Condvar,
}

pub struct ReplyGuard<'a, TL: TransportLayer<SendData>, SendData, RecvData, const CAP: usize = DEFAULT_WINDOW_CAP> {
    seq: &'a SeqExSync<SendData, RecvData, CAP>,
    app: Option<TL>,
    reply_no: SeqNo,
    is_holding_lock: bool,
}
impl<'a, TL: TransportLayer<SendData>, SendData, RecvData, const CAP: usize> ReplyGuard<'a, TL, SendData, RecvData, CAP> {
    fn reply_with(mut self, seq_cst: bool, packet_data: impl FnOnce(SeqNo, SeqNo) -> SendData) {
        let app = self.app.take().unwrap();
        let mut seq = self.seq.seq_ex.lock().unwrap();
        let seq_no = seq.0.seq_no();
        seq.0
            .reply_raw(app, self.reply_no, self.is_holding_lock, seq_cst, packet_data(seq_no, self.reply_no));
        if seq.2 {
            seq.2 = false;
            drop(seq);
            self.seq.reply_block.notify_all();
        }
    }
    /// If you need to reply more than once, say to fragment a large file, then include in your
    /// first reply some identifier, and then `send` all fragments with the same included identifier.
    /// The identifier will tell the remote peer which packets contain fragments of the file,
    /// and since each fragment will be received in order it will be trivial for them to reconstruct
    /// the original file.
    pub fn reply(self, seq_cst: bool, packet_data: SendData) {
        self.reply_with(seq_cst, |_, _| packet_data)
    }

    pub fn to_components(mut self) -> (TL, SeqNo, bool) {
        (self.app.take().unwrap(), self.reply_no, self.is_holding_lock)
    }
    pub unsafe fn from_components(seq: &'a SeqExSync<SendData, RecvData, CAP>, app: TL, reply_no: SeqNo, is_holding_lock: bool) -> Self {
        ReplyGuard { seq, app: Some(app), reply_no, is_holding_lock }
    }
}
impl<'a, TL: TransportLayer<SendData>, SendData, RecvData, const CAP: usize> Drop for ReplyGuard<'a, TL, SendData, RecvData, CAP> {
    fn drop(&mut self) {
        if let Some(app) = self.app.take() {
            let mut seq = self.seq.seq_ex.lock().unwrap();
            seq.0.ack_raw(app, self.reply_no, self.is_holding_lock);
            if seq.2 {
                seq.2 = false;
                drop(seq);
                self.seq.reply_block.notify_all();
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
crate::impl_recvok!(RecvOk, &'a SeqExSync<SendData, RecvData, CAP>);

pub struct RecvIter<'a, TL: TransportLayer<SendData>, P, SendData, RecvData, const CAP: usize = DEFAULT_WINDOW_CAP> {
    seq: Option<&'a SeqExSync<SendData, RecvData, CAP>>,
    app: TL,
    first: Option<RecvOk<'a, TL, P, SendData, RecvData, CAP>>,
    blocking: bool,
}

impl<SendData, RecvData, const CAP: usize> SeqExSync<SendData, RecvData, CAP> {
    pub fn new(retry_interval: i64, initial_seq_no: SeqNo) -> Self {
        Self {
            seq_ex: Mutex::new((SeqEx::new(retry_interval, initial_seq_no), 0, false)),
            send_block: Condvar::default(),
            reply_block: Condvar::default(),
        }
    }

    pub fn receive<TL: TransportLayer<SendData>, P: Into<RecvData>>(
        &self,
        app: TL,
        packet: Packet<P>,
    ) -> Result<RecvOk<'_, TL, P, SendData, RecvData, CAP>, RecvError> {
        let mut seq = self.seq_ex.lock().unwrap();
        match seq.0.receive_raw(app.clone(), packet) {
            Ok(r) => {
                if seq.1 > 0 {
                    seq.1 -= 1;
                    drop(seq);
                    self.send_block.notify_one();
                }
                Ok(RecvOk::from_raw(self, app, r))
            }
            Err(e) => Err(e),
        }
    }
    pub fn try_pump<TL: TransportLayer<SendData>>(&self, app: TL) -> Result<RecvOk<'_, TL, RecvData, SendData, RecvData, CAP>, PumpError> {
        let mut seq = self.seq_ex.lock().unwrap();
        // Enforce that only one thread may pump at a time.
        if seq.2 {
            return Err(PumpError::WaitingForReply);
        }
        match seq.0.try_pump_raw() {
            Ok(r) => {
                if seq.1 > 0 {
                    seq.1 -= 1;
                    drop(seq);
                    self.send_block.notify_one();
                }
                Ok(RecvOk::from_raw(self, app, r))
            }
            Err(e) => Err(e),
        }
    }
    pub fn pump<TL: TransportLayer<SendData>>(&self, app: TL) -> Option<RecvOk<'_, TL, RecvData, SendData, RecvData, CAP>> {
        let mut seq = self.seq_ex.lock().unwrap();
        // Enforce that only one thread may pump at a time.
        if seq.2 {
            return None;
        }
        loop {
            match seq.0.try_pump_raw() {
                Ok(r) => {
                    if seq.1 > 0 {
                        seq.1 -= 1;
                        drop(seq);
                        self.send_block.notify_one();
                    }
                    return Some(RecvOk::from_raw(self, app, r));
                }
                Err(PumpError::WaitingForRecv) => return None,
                Err(PumpError::WaitingForReply) => {
                    seq.2 = true;
                    seq = self.reply_block.wait(seq).unwrap();
                }
            }
        }
    }

    fn receive_all_inner<TL: TransportLayer<SendData>, P: Into<RecvData>>(
        &self,
        app: TL,
        blocking: bool,
        packet: Packet<P>,
    ) -> RecvIter<'_, TL, P, SendData, RecvData, CAP> {
        match self.receive(app.clone(), packet) {
            Ok(r) => RecvIter { seq: Some(self), app, first: Some(r), blocking },
            Err(RecvError::WaitingForReply) if blocking => RecvIter { seq: Some(self), app, first: None, blocking },
            Err(_) => RecvIter { seq: None, app, first: None, blocking },
        }
    }
    pub fn receive_all<TL: TransportLayer<SendData>, P: Into<RecvData>>(
        &self,
        app: TL,
        packet: Packet<P>,
    ) -> RecvIter<'_, TL, P, SendData, RecvData, CAP> {
        self.receive_all_inner(app, true, packet)
    }
    pub fn try_receive_all<TL: TransportLayer<SendData>, P: Into<RecvData>>(
        &self,
        app: TL,
        packet: Packet<P>,
    ) -> RecvIter<'_, TL, P, SendData, RecvData, CAP> {
        self.receive_all_inner(app, false, packet)
    }

    pub fn try_send_with<TL: TransportLayer<SendData>, F: FnOnce(SeqNo) -> SendData>(&self, app: TL, seq_cst: bool, packet_data: F) -> Result<(), F> {
        let mut seq = self.seq_ex.lock().unwrap();
        seq.0.try_send_with(app, seq_cst, packet_data)
    }
    pub fn try_send<TL: TransportLayer<SendData>>(&self, app: TL, seq_cst: bool, packet_data: SendData) -> Result<(), SendData> {
        let mut seq = self.seq_ex.lock().unwrap();
        seq.0.try_send(app, seq_cst, packet_data)
    }

    pub fn send_with<TL: TransportLayer<SendData>>(&self, app: TL, seq_cst: bool, mut packet_data: impl FnOnce(SeqNo) -> SendData) {
        let mut seq = self.seq_ex.lock().unwrap();
        while let Err(p) = seq.0.try_send_with(app.clone(), seq_cst, packet_data) {
            packet_data = p;
            seq.1 += 1;
            seq = self.send_block.wait(seq).unwrap();
        }
    }
    pub fn send<TL: TransportLayer<SendData>>(&self, app: TL, seq_cst: bool, mut packet_data: SendData) {
        let mut seq = self.seq_ex.lock().unwrap();
        while let Err(p) = seq.0.try_send(app.clone(), seq_cst, packet_data) {
            packet_data = p;
            seq.1 += 1;
            seq = self.send_block.wait(seq).unwrap();
        }
    }

    pub fn service<TL: TransportLayer<SendData>>(&self, app: TL) -> i64 {
        self.seq_ex.lock().unwrap().0.service(app)
    }
}
impl<SendData, RecvData, const CAP: usize> Default for SeqExSync<SendData, RecvData, CAP> {
    fn default() -> Self {
        Self::new(DEFAULT_RESEND_INTERVAL_MS, DEFAULT_INITIAL_SEQ_NO)
    }
}

impl<'a, TL: TransportLayer<SendData>, P, SendData, RecvData, const CAP: usize> RecvIter<'a, TL, P, SendData, RecvData, CAP> {
    pub fn take_first(&mut self) -> Option<RecvOk<'a, TL, P, SendData, RecvData, CAP>> {
        self.first.take()
    }
}
impl<'a, TL: TransportLayer<SendData>, P: Into<RecvData>, SendData, RecvData, const CAP: usize> Iterator
    for RecvIter<'a, TL, P, SendData, RecvData, CAP>
{
    type Item = RecvOk<'a, TL, RecvData, SendData, RecvData, CAP>;
    fn next(&mut self) -> Option<Self::Item> {
        if let Some(g) = self.first.take() {
            Some(g.into())
        } else if let Some(origin) = self.seq {
            if self.blocking {
                origin.pump(self.app.clone())
            } else {
                origin.try_pump(self.app.clone()).ok()
            }
        } else {
            None
        }
    }
}

#[derive(Clone, Debug)]
pub struct MpscTransport<Payload: Clone> {
    pub channel: Sender<Packet<Payload>>,
    pub time: Instant,
}

impl<Payload: Clone> MpscTransport<Payload> {
    pub fn new() -> (Self, Receiver<Packet<Payload>>) {
        let (send, recv) = channel();
        (Self { channel: send, time: std::time::Instant::now() }, recv)
    }
    pub fn from_sender(send: Sender<Packet<Payload>>) -> Self {
        Self { channel: send, time: std::time::Instant::now() }
    }
}
impl<Payload: Clone> TransportLayer<Payload> for &MpscTransport<Payload> {
    fn time(&mut self) -> i64 {
        self.time.elapsed().as_millis() as i64
    }

    fn send(&mut self, packet: Packet<&Payload>) {
        let _ = self.channel.send(packet.cloned());
    }
}
