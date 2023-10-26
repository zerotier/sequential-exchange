use crate::SeqNo;

/// These are the error types that can be returned by a non-blocking SEQEX receive function.
///
/// Some of these errors specify that SEQEX is waiting on some event to occur before it can proceed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TryRawRecvError {
    /// This packet had to be dropped because it arrived far enough out-of-order that it was outside
    /// the receive window.
    /// This packet will eventually be resent, so no data will be lost.
    DroppedTooEarly,
    /// This packet was a duplicate of a previously received packet. It must have been resent before
    /// the remote peer received the ack for the packet.
    /// Since this packet is a duplicate, no data is lost by dropping it.
    DroppedDuplicate,
    /// This packet was a duplicate of a previously received packet. We need to resend an Ack packet
    /// containing the reply number within this error instance.
    ///
    /// So `Packet::Ack(SeqNo)` should be sent to the remote peer immediately.
    DroppedDuplicateResendAck(SeqNo),
    /// In order to preserve losslessness or in-order transport, the received packet cannot be
    /// process until some other packet is received. The packet was saved to the receive window.
    WaitingForRecv,
    /// Either the receive window is full, or the received packet is SeqCst and cannot be processed
    /// yet. In either case some currently issued reply number must be returned to SEQEX to send
    /// either an Ack or a Reply. If using reply guards, then some currently existing reply guard
    /// must be dropped or consumed.
    /// Until this occurs the received packet cannot be processed.
    ///
    /// If the receive window was full, the packet was dropped.
    /// Otherwise if the packet is SeqCst, then the packet was saved to the receive window.
    WaitingForReply,
}

/// These are the error types that can be returned by a blocking SEQEX receive function.
///
/// Some of these errors specify that SEQEX is waiting on some event to occur before it can proceed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TryRecvError {
    /// This packet had to be dropped because it arrived far enough out-of-order that it was outside
    /// the receive window.
    /// This packet will eventually be resent, so no data will be lost.
    DroppedTooEarly,
    /// This packet was a duplicate of a previously received packet. It must have been resent before
    /// the remote peer received the ack for the packet.
    /// Since this packet is a duplicate, no data is lost by dropping it.
    DroppedDuplicate,
    /// In order to preserve losslessness or in-order transport, the received packet cannot be
    /// process until some other packet is received. The packet was saved to the receive window.
    WaitingForRecv,
    /// Either the receive window is full, or the received packet is SeqCst and cannot enter the
    /// critical section where it is processed. In either case there currently exists some reply
    /// guard that must be dropped or consumed before this packet can be processed.
    ///
    /// If the receive window was full, the packet was dropped.
    /// Otherwise if the packet is SeqCst, then the packet was saved to the receive window.
    WaitingForReply,
}

/// These are the error types that can be returned by a blocking SEQEX receive function.
///
/// Some of these errors specify that SEQEX is waiting on some event to occur before it can proceed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecvError {
    /// This packet had to be dropped because it arrived far enough out-of-order that it was outside
    /// the receive window.
    /// This packet will eventually be resent, so no data will be lost.
    DroppedTooEarly,
    /// This packet was a duplicate of a previously received packet. It must have been resent before
    /// the remote peer received the ack for the packet.
    /// Since this packet is a duplicate, no data is lost by dropping it.
    DroppedDuplicate,
    /// In order to preserve losslessness or in-order transport, the received packet cannot be
    /// process until some other packet is received. The packet was saved to the receive window.
    WaitingForRecv,
    /// Either the receive window is full, or the received packet is SeqCst and cannot enter the
    /// critical section where it is processed. In either case there currently exists some reply
    /// guard that must be dropped or consumed before this packet can be processed.
    ///
    /// If the receive window was full, the packet was dropped.
    /// Otherwise if the packet is SeqCst, then the packet was saved to the receive window.
    WaitingForReply,
    Closed,
}

/// A generic error that can be returned by a `try_send` or `try_pump` function.
/// They specify what event must occur before a future call to `try_send` or `try_pump` can succeed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TryError {
    /// The packet could not be sent or processed at this time.
    /// Some other packet must be received from the remote peer first.
    WaitingForRecv,
    /// Some currently issued reply number must be returned to SEQEX to send either an Ack or a
    /// Reply. If using reply guards, then some currently existing reply guard must be dropped or
    /// consumed.
    /// Until this occurs the packet cannot be sent or processed.
    WaitingForReply,
}
/// A generic error that can be returned by a `try_send` or `try_pump` function.
/// They specify what event must occur before a future call to `try_send` or `try_pump` can succeed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TrySendError {
    /// The packet could not be sent or processed at this time.
    /// Some other packet must be received from the remote peer first.
    WaitingForRecv,
    /// Some currently issued reply number must be returned to SEQEX to send either an Ack or a
    /// Reply. If using reply guards, then some currently existing reply guard must be dropped or
    /// consumed.
    /// Until this occurs the packet cannot be sent or processed.
    WaitingForReply,
    Closed,
}
/// This instance of `SeqEx` has been explicitly closed.
/// It can no longer send or receive data.
///
/// This error can only occur after `SeqEx::close` has been called.
/// An instance of `SeqEx` will never close by itself, only the caller can close it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClosedError;

#[cfg(feature = "std")]
impl std::fmt::Display for TryRawRecvError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TryRawRecvError::DroppedTooEarly => write!(f, "packet arrived too early"),
            TryRawRecvError::DroppedDuplicate => write!(f, "packet was a duplicate"),
            TryRawRecvError::DroppedDuplicateResendAck(_) => write!(f, "packet was a duplicate, resending ack"),
            TryRawRecvError::WaitingForRecv => write!(f, "can't process until another packet is received"),
            TryRawRecvError::WaitingForReply => write!(f, "can't process until a reply is finished"),
        }
    }
}
#[cfg(feature = "std")]
impl std::error::Error for TryRawRecvError {}

#[cfg(feature = "std")]
impl std::fmt::Display for TryRecvError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::DroppedTooEarly => write!(f, "packet arrived too early"),
            Self::DroppedDuplicate => write!(f, "packet was a duplicate"),
            Self::WaitingForRecv => write!(f, "can't process packet until another packet is received"),
            Self::WaitingForReply => write!(f, "can't process packet until a reply is finished"),
        }
    }
}
#[cfg(feature = "std")]
impl std::error::Error for TryRecvError {}

#[cfg(feature = "std")]
impl std::fmt::Display for RecvError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::DroppedTooEarly => write!(f, "packet arrived too early"),
            Self::DroppedDuplicate => write!(f, "packet was a duplicate"),
            Self::WaitingForRecv => write!(f, "can't process packet until another packet is received"),
            Self::WaitingForReply => write!(f, "can't process packet until a reply is finished"),
            Self::Closed => write!(f, "can't receive packet because the session was explicitly closed"),
        }
    }
}
#[cfg(feature = "std")]
impl std::error::Error for RecvError {}

#[cfg(feature = "std")]
impl std::fmt::Display for TryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TryError::WaitingForRecv => write!(f, "can't process until another packet is received"),
            TryError::WaitingForReply => write!(f, "can't process until a reply is finished"),
        }
    }
}
#[cfg(feature = "std")]
impl std::error::Error for TryError {}

#[cfg(feature = "std")]
impl std::fmt::Display for TrySendError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TrySendError::WaitingForRecv => write!(f, "can't send until another packet is received"),
            TrySendError::WaitingForReply => write!(f, "can't send until a reply is finished"),
            TrySendError::Closed => write!(f, "can't send because the session was explicitly closed"),
        }
    }
}
#[cfg(feature = "std")]
impl std::error::Error for TrySendError {}

#[cfg(feature = "std")]
impl std::fmt::Display for ClosedError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "can't send because the session was explicitly closed")
    }
}
#[cfg(feature = "std")]
impl std::error::Error for ClosedError {}

impl From<TryRecvError> for RecvError {
    fn from(value: TryRecvError) -> Self {
        match value {
            TryRecvError::DroppedTooEarly => RecvError::DroppedTooEarly,
            TryRecvError::DroppedDuplicate => RecvError::DroppedDuplicate,
            TryRecvError::WaitingForRecv => RecvError::WaitingForRecv,
            TryRecvError::WaitingForReply => RecvError::WaitingForReply,
        }
    }
}

impl From<TryError> for TrySendError {
    fn from(value: TryError) -> Self {
        match value {
            TryError::WaitingForRecv => TrySendError::WaitingForRecv,
            TryError::WaitingForReply => TrySendError::WaitingForReply,
        }
    }
}

impl From<TryRawRecvError> for TryRecvError {
    fn from(value: TryRawRecvError) -> Self {
        match value {
            TryRawRecvError::DroppedTooEarly => TryRecvError::DroppedTooEarly,
            TryRawRecvError::DroppedDuplicate => TryRecvError::DroppedDuplicate,
            TryRawRecvError::DroppedDuplicateResendAck(_) => TryRecvError::DroppedDuplicate,
            TryRawRecvError::WaitingForRecv => TryRecvError::WaitingForRecv,
            TryRawRecvError::WaitingForReply => TryRecvError::WaitingForReply,
        }
    }
}
