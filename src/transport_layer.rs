use crate::SeqNo;

/// A trait for giving an instance of SeqEx access to the transport layer.
///
/// The implementor is free to choose how to define the generic types based on how they want to
/// manage memory.
/// It is possible through these generics to make SeqEx no-alloc and zero-copy, but otherwise
/// they are most easily implemented as some combination of custom enums, `Vec<u8>` and `Arc<[u8]>`.
pub trait TransportLayer: Clone {
    type SendData;

    fn time(&mut self) -> i64;
    #[allow(unused)]
    fn update_service_time(&mut self, timestamp: i64, current_time: i64) {}

    fn send(&mut self, seq_no: SeqNo, reply_no: Option<SeqNo>, payload: &Self::SendData);
    fn send_ack(&mut self, reply_no: SeqNo);
}
