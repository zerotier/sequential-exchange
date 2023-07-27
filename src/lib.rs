

pub type SeqNum = u32;

pub mod seq_queue;

//struct App();

//use seq_queue::ApplicationLayer;
//impl ApplicationLayer for App {
//    type RecvData = (Vec<u8>, u64);

//    type RecvDataRef<'a> = (&'a [u8], &'a [u8], u64);

//    type RecvReturn = ();

//    type SendData = Vec<u8>;

//    fn send(&self, data: &Self::SendData) {
//        todo!()
//    }
//    fn send_ack(&self, reply_num: SeqNum) {
//        todo!()
//    }
//    fn send_empty_reply(&self, reply_num: SeqNum) {
//        todo!()
//    }

//    fn deserialize<'a>(data: &'a Self::RecvData) -> Self::RecvDataRef<'a> {
//        let s = data.0.split_at(1);
//        (s.0, s.1, data.1)
//    }
//    fn process(
//        &self,
//        packet: Self::RecvDataRef<'_>,
//        reply: seq_queue::ReplyGuard<'_, Self>,
//        send_data: Option<Self::SendData>,
//    ) -> Self::RecvReturn {
//        todo!()
//    }
//}
