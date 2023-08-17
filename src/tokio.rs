use std::{
    ops::{Deref, DerefMut},
    sync::{
        Mutex, MutexGuard,
    },
    time::Instant,
};
use tokio::{task, sync::{Notify, oneshot}};

use crate::{Error, SeqEx, SeqNo, TransportLayer, DEFAULT_INITIAL_SEQ_NO, DEFAULT_RESEND_INTERVAL_MS, DEFAULT_WINDOW_CAP};

type SendData<Packet> = (oneshot::Sender<(Packet, SeqNo)>, Packet);

pub struct SeqExTokio<Packet, const CAP: usize = DEFAULT_WINDOW_CAP> {
    seq_ex: Mutex<(SeqEx<SendData<Packet>, Packet, CAP>, usize)>,
    send_block: Notify,
}

pub struct ReplyGuard<'a, TL: TransportLayer<SendData<Packet>>, Packet, const CAP: usize = DEFAULT_WINDOW_CAP> {
    origin: &'a SeqExTokio<Packet, CAP>,
    app: Option<TL>,
    reply_no: SeqNo,
}
impl<'a, TL: TransportLayer<SendData<Packet>>, Packet, const CAP: usize> ReplyGuard<'a, TL, Packet, CAP> {
    /// If you need to reply more than once, say to fragment a large file, then include in your
    /// first reply some identifier, and then `send` all fragments with the same included identifier.
    /// The identifier will tell the remote peer which packets contain fragments of the file,
    /// and since each fragment will be received in order it will be trivial for them to reconstruct
    /// the original file.
    pub async fn reply(self, packet: Packet) -> Option<(Packet, ReplyGuard<'a, TL, Packet, CAP>)> {
        self.reply_with(|_, _| packet).await
    }
    pub async fn reply_with(mut self, packet: impl FnOnce(SeqNo, SeqNo) -> Packet) -> Option<(Packet, ReplyGuard<'a, TL, Packet, CAP>)> {
        let (tx, rx) = oneshot::channel();
        let mut seq = self.origin.seq_ex.lock().unwrap();
        let seq_no = seq.0.seq_no();
        let app = self.app.take().unwrap();
        seq.0.reply_raw(app.clone(), self.reply_no, (tx, packet(seq_no, self.reply_no)));
        let (packet, reply_no) = rx.await.ok()?;
        let g = ReplyGuard { origin: self.origin, app: Some(app), reply_no };
        Some((packet, g))
    }
}
impl<'a, TL: TransportLayer<SendData<Packet>>, Packet, const CAP: usize> Drop for ReplyGuard<'a, TL, Packet, CAP> {
    fn drop(&mut self) {
        if let Some(app) = self.app.take() {
            let mut seq = self.origin.seq_ex.lock().unwrap();
            seq.0.ack_raw(app, self.reply_no);
        }
    }
}

pub struct ReplyIter<'a, TL: TransportLayer<SendData<Packet>>, Packet, const CAP: usize = DEFAULT_WINDOW_CAP> {
    origin: Option<&'a SeqExTokio<Packet, CAP>>,
    app: TL,
    first: Option<(Packet, ReplyGuard<'a, TL, Packet, CAP>)>,
}

//pub struct SeqExGuard<'a, SendData, RecvData, const CAP: usize>(MutexGuard<'a, (SeqEx<SendData, RecvData, CAP>, usize)>);
//impl<'a, SendData, RecvData, const CAP: usize> Deref for SeqExGuard<'a, SendData, RecvData, CAP> {
//    type Target = SeqEx<SendData, RecvData, CAP>;

//    fn deref(&self) -> &Self::Target {
//        &self.0 .0
//    }
//}
//impl<'a, SendData, RecvData, const CAP: usize> DerefMut for SeqExGuard<'a, SendData, RecvData, CAP> {
//    fn deref_mut(&mut self) -> &mut Self::Target {
//        &mut self.0 .0
//    }
//}

impl<Packet, const CAP: usize> SeqExTokio<Packet, CAP> {
    pub fn new(retry_interval: i64, initial_seq_no: SeqNo) -> Self {
        Self {
            seq_ex: Mutex::new((SeqEx::new(retry_interval, initial_seq_no), 0)),
            send_block: Notify::new(),
        }
    }

    fn process<TL: TransportLayer<SendData<Packet>>>(
        &self, app: TL,
        mut seq: MutexGuard<'_, (SeqEx<SendData<Packet>, Packet, CAP>, usize)>,
        result: Result<(SeqNo, Packet, Option<SendData<Packet>>), Error>
    ) -> Option<(Packet, ReplyGuard<'_, TL, Packet, CAP>)> {
        if let Ok((reply_no, packet, send_data)) = result {
            if seq.1 > 0 {
                self.send_block.notify_one();
            }
            if let Some((tx, _)) = send_data {
                if let Err(e) = tx.send((packet, reply_no)) {
                    // Allow the drop code to be run
                    seq.0.ack_raw(app, reply_no);
                }
                None
            } else {
                Some((packet, ReplyGuard { origin: self, app: Some(app), reply_no }))
            }
        } else {
            None
        }
    }
    pub fn receive<TL: TransportLayer<SendData<Packet>>>(
        &self,
        app: TL,
        seq_no: SeqNo,
        reply_no: Option<SeqNo>,
        packet: Packet,
    ) -> Option<(Packet, ReplyGuard<'_, TL, Packet, CAP>)> {
        let mut seq = self.seq_ex.lock().unwrap();
        let result = seq.0.receive_raw(app.clone(), seq_no, reply_no, packet);
        self.process(app, seq, result)
    }
    pub fn pump<TL: TransportLayer<SendData<Packet>>>(&self, app: TL) -> Option<(Packet, ReplyGuard<'_, TL, Packet, CAP>)> {
        let mut seq = self.seq_ex.lock().unwrap();
        let result = seq.0.pump_raw();
        self.process(app, seq, result)
    }
    pub fn receive_all<TL: TransportLayer<SendData<Packet>>>(
        &self,
        app: TL,
        seq_no: SeqNo,
        reply_no: Option<SeqNo>,
        packet: Packet,
    ) -> ReplyIter<'_, TL, Packet, CAP> {
        if let Some(g) = self.receive(app.clone(), seq_no, reply_no, packet) {
            ReplyIter { origin: Some(self), app, first: Some(g) }
        } else {
            ReplyIter { origin: None, app, first: None }
        }
    }
    pub fn receive_ack(&self, reply_no: SeqNo) {
        let mut seq = self.seq_ex.lock().unwrap();
        // We drop the sender to notify the receiver that no packet was received.
        if let Ok(_) = seq.0.receive_ack(reply_no) {
            if seq.1 > 0 {
                self.send_block.notify_one();
            }
        }
    }
    //pub fn try_send<TL: TransportLayer<SendData>>(&self, app: TL, packet_data: SendData) -> Result<(), SendData> {
    //    let mut seq = self.lock();
    //    seq.try_send(app, packet_data)
    //}
    async fn send_inner<TL: TransportLayer<SendData<Packet>>>(
        &self,
        mut seq: MutexGuard<'_, (SeqEx<SendData<Packet>, Packet, CAP>, usize)>,
        app: TL,
        mut tx: oneshot::Sender<(Packet, SeqNo)>,
        mut packet: Packet
    ) {
        while let Err(e) = seq.0.try_send(app.clone(), (tx, packet)) {
            (tx, packet) = e;
            seq.1 += 1;
            drop(seq);
            self.send_block.notified().await;
            seq = self.seq_ex.lock().unwrap();
            seq.1 -= 1;
        }
    }
    /// If this future is dropped then the remote peer's reply to this packet will also be dropped.
    pub async fn send<TL: TransportLayer<SendData<Packet>>>(&self, app: TL, packet: Packet) -> Option<(Packet, ReplyGuard<'_, TL, Packet, CAP>)> {
        self.send_with(app, |_| packet).await
    }
    //pub fn try_send_with<TL: TransportLayer<SendData>>(&self, app: TL, packet_data: impl FnOnce(SeqNo) -> SendData) -> Result<(), SendData> {
    //    let mut seq = self.lock();
    //    let seq_no = seq.seq_no();
    //    seq.try_send(app, packet_data(seq_no))
    //}
    pub async fn send_with<TL: TransportLayer<SendData<Packet>>>(&self, app: TL, packet: impl FnOnce(SeqNo) -> Packet) -> Option<(Packet, ReplyGuard<'_, TL, Packet, CAP>)> {
        let (tx, rx) = oneshot::channel();
        let seq = self.seq_ex.lock().unwrap();
        let seq_no = seq.0.seq_no();
        self.send_inner(seq, app.clone(), tx, packet(seq_no)).await;
        // This can only return an error if the sender was dropped.
        let (packet, reply_no) = rx.await.ok()?;
        Some((packet, ReplyGuard { origin: self, app: Some(app), reply_no }))
    }

    pub async fn main<TL: TransportLayer<SendData<Packet>>>(&self, app: TL) {
        let a = task::spawn(async{

        });
    }
    pub fn service<TL: TransportLayer<SendData<Packet>>>(&self, app: TL) -> i64 {
        self.seq_ex.lock().unwrap().0.service(app)
    }

    //pub fn lock(&self) -> SeqExGuard<'_, SendData, RecvData, CAP> {
    //    SeqExGuard(self.seq_ex.lock().unwrap())
    //}
}
//impl<SendData, RecvData, const CAP: usize> Default for SeqExTokio<SendData, RecvData, CAP> {
//    fn default() -> Self {
//        Self::new(DEFAULT_RESEND_INTERVAL_MS, DEFAULT_INITIAL_SEQ_NO)
//    }
//}

impl<'a, TL: TransportLayer<SendData<Packet>>, Packet, const CAP: usize> Iterator for ReplyIter<'a, TL, Packet, CAP> {
    type Item = (Packet, ReplyGuard<'a, TL, Packet, CAP>);
    fn next(&mut self) -> Option<Self::Item> {
        if let Some(g) = self.first.take() {
            Some(g)
        } else if let Some(origin) = self.origin {
            origin.pump(self.app.clone())
        } else {
            None
        }
    }
}

pub trait TokioTransport<Packet>: Clone {
    fn time(&mut self) -> i64;
    #[allow(unused)]
    fn update_service_time(&mut self, timestamp: i64, current_time: i64) {}

    fn send(&mut self, seq_no: SeqNo, reply_no: Option<SeqNo>, payload: &Packet);
    fn send_ack(&mut self, reply_no: SeqNo);
}

impl<Packet, Tl: TokioTransport<Packet>> TransportLayer<SendData<Packet>> for (Tl, ) {
    fn time(&mut self) -> i64 {
        todo!()
    }
    fn update_service_time(&mut self, timestamp: i64, current_time: i64) {
        
    }

    fn send(&mut self, seq_no: SeqNo, reply_no: Option<SeqNo>, payload: &SendData<Packet>) {
        todo!()
    }

    fn send_ack(&mut self, reply_no: SeqNo) {
        todo!()
    }
}

//#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
//#[derive(Clone)]
//pub enum PacketType<Payload: Clone> {
//    Payload(SeqNo, Option<SeqNo>, Payload),
//    Ack(SeqNo),
//}

//#[derive(Clone)]
//pub struct MpscTransport<Payload: Clone> {
//    pub channel: Sender<PacketType<Payload>>,
//    pub time: Instant,
//}
//pub type MpscGuard<'a, Packet> = ReplyGuard<'a, &'a MpscTransport<Packet>, Packet, Packet>;
//pub type MpscSeqEx<Packet> = SeqExTokio<Packet, Packet>;

//impl<Payload: Clone> MpscTransport<Payload> {
//    pub fn new() -> (Self, Receiver<PacketType<Payload>>) {
//        let (send, recv) = channel();
//        (Self { channel: send, time: std::time::Instant::now() }, recv)
//    }
//    pub fn from_sender(send: Sender<PacketType<Payload>>) -> Self {
//        Self { channel: send, time: std::time::Instant::now() }
//    }
//}
//impl<Payload: Clone> TransportLayer<Payload> for &MpscTransport<Payload> {
//    fn time(&mut self) -> i64 {
//        self.time.elapsed().as_millis() as i64
//    }

//    fn send(&mut self, seq_no: SeqNo, reply_no: Option<SeqNo>, payload: &Payload) {
//        let _ = self.channel.send(PacketType::Payload(seq_no, reply_no, payload.clone()));
//    }
//    fn send_ack(&mut self, reply_no: SeqNo) {
//        let _ = self.channel.send(PacketType::Ack(reply_no));
//    }
//}
