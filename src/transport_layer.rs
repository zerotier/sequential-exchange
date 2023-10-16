/// A 32-bit sequence number. Packets transported with SEQEX are expected to contain at least one
/// sequence number, and sometimes two.
/// All packets will either have a seq_no, a reply_no, or both.
pub type SeqNo = u32;

/// The resend interval for a default instance of SeqEx.
pub const DEFAULT_RESEND_INTERVAL_MS: i64 = 250;
/// The initial sequence number for a default instance of SeqEx.
pub const DEFAULT_INITIAL_SEQ_NO: SeqNo = 0;
/// The default maximum capacity of the SEQEX send and receive window.
/// The larger the capacity, the longer packets can be sent through SEQEX without needing to wait
/// for acknowledgements to arrive.
///
/// The memory usage of SEQEX increases linearly with this number.
pub const DEFAULT_WINDOW_CAP: usize = 64;

/// SEQEX is serialization agnostic. The user is free to choose whatever serialization format they
/// want for packets originating from SEQEX. All that is required is that this enum can be
/// serialized and deserialized accurately on both ends of a connection.
///
/// The easiest serialization implementation is to simply use serde to write this enum to a Vec<u8>
/// and send the resulting bytes over the wire to the other end.
/// The receiver can then deserialize with serde.
///
/// However if more efficiency is required, this enum can easily be serialized as a "tagged union"
/// (https://en.wikipedia.org/wiki/Tagged_union).
///
/// It is possible and even encouraged in real-time environments to include a "channel id" along
/// with the serialized packet. That way each end of a SEQEX connection can run multiple instances of
/// SEQEX in parrallel, each instance identified by the channel id. By running multiple in parallel,
/// Head-of-line blocking can be avoided even when SeqCst packets are being sent.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum Packet<RecvData> {
    /// This is a normal payload of data within SEQEX.
    /// When it is sent to a remote peer, SEQEX guarantees lossless delivery,
    /// meaning the remote peer is guaranteed to receive this payload exactly once.
    ///
    /// This payload is not guaranteed to be received in the order it was sent relative to other
    /// payloads.
    ///
    /// The contained `SeqNo` is the packet sequence number.
    Payload(SeqNo, RecvData),
    /// This is a payload of data with the added guarantee that it is received in order relative to
    /// other SeqCst payloads and replies.
    /// When it is sent to a remote peer, SEQEX guarantees in-order, losslessness delivery.
    ///
    /// Any normal payload sent before a SeqCst payload will be received before the SeqCst payload
    /// is received, however normal payloads sent after a SeqCst payload can be received in any
    /// order.
    ///
    /// As a result of in-order delivery this payload usually exhibits higher latency than a normal
    /// payload.
    ///
    /// The contained `SeqNo` is the packet sequence number.
    SeqCstPayload(SeqNo, RecvData),
    /// This is a normal reply to a received packet within SEQEX.
    /// SEQEX provides the unique capability for the user to reply to any kind of payload, as well
    /// as any kind of reply. SEQEX guarantees that replies are unambiguous, meaning that both the
    /// sender and receiver of a reply will always agree upon which packet the reply is replying to.
    ///
    /// This essentially means that all conversations within SEQEX are linked-lists, with replies
    /// acting as nodes with a link to the next node, and payloads being nodes with null links.
    /// The most recently received packet is the "head" of the linked list, and the linked list can
    /// be appended to by replying to that packet.
    ///
    /// Similar to a normal payload, when a normal reply is sent to a remote peer,
    /// SEQEX guarantees lossless delivery.
    ///
    /// The first `SeqNo` is the packet sequence number, and the second is the packet reply number.
    Reply(SeqNo, SeqNo, RecvData),
    /// This is a reply to a received packet with the added guarantee that it is received in order
    /// relative to other SeqCst payloads and replies.
    ///
    /// SEQEX guarantees that SeqCst replies are unambiguous, and are received in-order.
    /// See the documentation for `Reply` and `SeqCstPayload` for further information about these
    /// guarantees.
    ///
    /// The first `SeqNo` is the packet sequence number, and the second is the packet reply number.
    SeqCstReply(SeqNo, SeqNo, RecvData),
    /// This is an acknowledgement within SEQEX.
    /// It is sent by default in response to any received packet, unless the user has chosen to
    /// send a reply instead.
    ///
    /// To provide the transport guarantees that it does, all packets within SEQEX are sent over the
    /// wire multiple times until they are acknowledged by either an ack or a reply.
    ///
    /// Within the linked list analogy, an ack is the last final node appended to the head of the
    /// linked list. Nothing further can be appended afterwards.
    ///
    /// The contained `SeqNo` is the packet reply number.
    Ack(SeqNo),
}
use Packet::*;
impl<RecvData> Packet<RecvData> {
    /// Create a new payload or reply packet.
    /// If `reply_no` is `None`, this will output a reply packet.
    /// If it is `Some`, this will output a payload packet.
    pub fn new_with_data(seq_no: SeqNo, reply_no: Option<SeqNo>, seq_cst: bool, data: RecvData) -> Self {
        Self::new(Some(seq_no), reply_no, seq_cst, Some(data)).unwrap()
    }
    /// Attempt to create a packet from raw parts, if the raw parts correctly specify some valid
    /// packet type within SEQEX.
    pub fn new(seq_no: Option<SeqNo>, reply_no: Option<SeqNo>, seq_cst: bool, data: Option<RecvData>) -> Option<Self> {
        match (seq_no, reply_no, seq_cst, data) {
            (Some(s), None, false, Some(d)) => Some(Payload(s, d)),
            (Some(s), None, true, Some(d)) => Some(SeqCstPayload(s, d)),
            (Some(s), Some(r), false, Some(d)) => Some(Reply(s, r, d)),
            (Some(s), Some(r), true, Some(d)) => Some(SeqCstReply(s, r, d)),
            (None, Some(r), false, None) => Some(Ack(r)),
            _ => None,
        }
    }
    ///Converts from &Packet<RecvData> to Packet<&RecvData>.
    pub fn as_ref(&self) -> Packet<&RecvData> {
        match self {
            Payload(seq_no, data) => Payload(*seq_no, data),
            SeqCstPayload(seq_no, data) => SeqCstPayload(*seq_no, data),
            Reply(seq_no, reply_no, data) => Reply(*seq_no, *reply_no, data),
            SeqCstReply(seq_no, reply_no, data) => SeqCstReply(*seq_no, *reply_no, data),
            Ack(reply_no) => Ack(*reply_no),
        }
    }
    /// Maps a Packet<RecvData> to Packet<T> by applying a function to the contained data
    /// (if this is a reply or payload variant) or by doing nothing (if this is the ack variant).
    pub fn map<T>(self, f: impl FnOnce(RecvData) -> T) -> Packet<T> {
        match self {
            Payload(seq_no, data) => Payload(seq_no, f(data)),
            SeqCstPayload(seq_no, data) => SeqCstPayload(seq_no, f(data)),
            Reply(seq_no, reply_no, data) => Reply(seq_no, reply_no, f(data)),
            SeqCstReply(seq_no, reply_no, data) => SeqCstReply(seq_no, reply_no, f(data)),
            Ack(reply_no) => Ack(reply_no),
        }
    }
    /// Returns any data contained within this packet (if this is a reply or payload variant).
    pub fn payload(self) -> Option<RecvData> {
        self.consume().ok()
    }
    /// Returns any data contained within this packet (if this is a reply or payload variant), or an
    /// `Err` containing the reply_no of an acknowledgement (if this is an ack variant).
    pub fn consume(self) -> Result<RecvData, SeqNo> {
        match self {
            Payload(_, data) | SeqCstPayload(_, data) | Reply(_, _, data) | SeqCstReply(_, _, data) => Ok(data),
            Ack(r) => Err(r),
        }
    }
    /// Returns true if this is a SeqCst payload or SeqCst reply.
    pub fn is_seq_cst(&self) -> bool {
        matches!(self, SeqCstPayload(..) | SeqCstReply(..))
    }
    /// If this is a reply or payload variant, this function will change it to a SeqCst variant if
    /// `seq_cst` is true, or to a normal variant if `seq_cst` is false.
    pub fn set_seq_cst(&mut self, seq_cst: bool) {
        let mut tmp = Ack(0);
        core::mem::swap(&mut tmp, self);
        match tmp {
            Payload(seq_no, data) | SeqCstPayload(seq_no, data) => {
                *self = if seq_cst {
                    SeqCstPayload(seq_no, data)
                } else {
                    Payload(seq_no, data)
                }
            }
            Reply(seq_no, reply_no, data) | SeqCstReply(seq_no, reply_no, data) => {
                *self = if seq_cst {
                    SeqCstReply(seq_no, reply_no, data)
                } else {
                    Reply(seq_no, reply_no, data)
                }
            }
            Ack(reply_no) => *self = Ack(reply_no),
        }
    }
}
impl<RecvData: Clone> Packet<&RecvData> {
    /// Maps a Packet<&RecvData> to Packet<RecvData> by cloning any data it containts.
    pub fn cloned(&self) -> Packet<RecvData> {
        self.map(|d| d.clone())
    }
}

/// A trait for giving an instance of SeqEx access to the transport layer.
///
/// The implementor is free to choose how to define the generic types based on how they want to
/// manage memory.
/// It is possible through these generics to make SeqEx no-alloc and zero-copy, but otherwise
/// they are most easily implemented as some combination of custom enums, `Vec<u8>` and `Arc<[u8]>`.
pub trait TransportLayer<SendData>: Clone + Copy {
    /// A callback that should return the current time in milliseconds.
    /// The source of this time does not have to be monotonic.
    ///
    /// The timestamp that this function returns can be the time at which this instance of
    /// `TransportLayer` was created/passed into a SEQEX function, rather than the time at which
    /// this function was called.
    fn time(&mut self) -> i64;

    /// A callback that should attempt to serialize and send `packet` to the remote peer.
    ///
    fn send(&mut self, packet: Packet<&SendData>);
}
