use crate::SeqNo;

/// A trait for giving an instance of SeqEx access to the transport layer.
///
/// The implementor is free to choose how to define the generic types based on how they want to
/// manage memory.
/// It is possible through these generics to implement SeqEx to be no-alloc and zero-copy, but otherwise
/// a lot of them are most easily implemented as tuples of custom enums and Vec<u8>.
pub trait TransportLayer: Sized + Clone {
    type RecvData;
    type SendData;

    fn time(&self) -> i64;

    fn send(&self, data: &Self::SendData);
    fn send_ack(&self, reply_no: SeqNo);
    fn send_empty_reply(&self, reply_no: SeqNo);
}
