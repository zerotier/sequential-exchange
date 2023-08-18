use crate::Packet;

/// A trait for giving an instance of SeqEx access to the transport layer.
///
/// The implementor is free to choose how to define the generic types based on how they want to
/// manage memory.
/// It is possible through these generics to make SeqEx no-alloc and zero-copy, but otherwise
/// they are most easily implemented as some combination of custom enums, `Vec<u8>` and `Arc<[u8]>`.
pub trait TransportLayer<SendData>: Clone {
    fn time(&mut self) -> i64;

    fn send(&mut self, packet: Packet<&SendData>);
}
