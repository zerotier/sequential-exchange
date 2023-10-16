//! # Sequential Exchange Protocol
//!
//! The reference implementation of the **Sequential Exchange Protocol**, or SEQEX.
//!
//! SEQEX is a lightweight, peer-to-peer transport protocol that guarantees packets of data will be losslessly received by the remote peer, and can optionally guaranteed that specified packets arrive in the order that they were sent. In addition, SEQEX facilitates stateful exchanges between two peers, giving each peer the opportunity to "reply" to any packet sent by the remote peer. This makes SEQEX particularly well-suited for writing async-await code, because unlike TCP, SEQEX will handle multiplexing each reply to the correct awaiter. Even without async-await, a simple `match` statement is sufficient to correctly multiplex packets to their handling code.
//!
//! A "stateful exchange" is defined here as a sequence of packets, where the first packet
//! initiates the exchange, and all subsequent packets are replies to the previous packet in the
//! exchange. Every exchange can be thought of as a linked list, where the head node is a packet containing a normal payload, and all subsequent nodes are replies to previous nodes. The final node is always a simple acknowledgement packet, or Ack, that signals a given exchange is over. Both peers are guaranteed to agree upon the "topology" of these links. Links will never get crossed, replies will always be received and understood, a peer will never deadlock awaiting a reply, and in general it is much easier to write bug-free networking code.
//!
//! SEQEX is a tiny, dead simple protocol and we have implemented it here in around a 1000 lines of code, depending upon how many features you enable.
//!
//! SEQEX is transport agnostic. It does not require being run over a single UDP socket. This allows SEQEX to easily be run over an encrypted tunnel that itself can run over as many or as few UDP sockets as necessary, if indeed UDP is even available. An instance of the SEQEX protocol can be forced to persist through a connection reset event, avoiding common issues with TCP where connection resets can cause unrecoverable packet loss.
//!
//! SEQEX is serialization agnostic, meaning its packets have no pre-defined encoding format. Users of SEQEX are free to choose between serde, packed structs, tagged unions, or anything else as their prefered serialization format. This means SEQEX takes some additional effort to set up up-front, but it means that user have significantly more flexibility long-term.
//!
//! As such it is relatively easier to run SEQEX in parrallel with another raw UDP protocol, or even in parrallel with itself. Multiple instances of SEQEX can be opened between two peers, making it very easy to reduce or even eliminate head-of-line latency in performance critical applications.
//!
//! ## Why not TCP?
//!
//! TCP only guarantees packets will be received in the same order they were sent.
//! It has no inherent concept of "replying to a packet" and as such it cannot guarantee both sides
//! of a conversation have the same view of any stateful exchanges that take place. This must be implemented manually by the user of TCP.
//!
//! TCP is also much higher overhead. It requires a 1.5 RTT handshake to begin any connection,
//! it has a larger amount of metadata that must be transported with packets, and it has quite a few
//! features that slow down runtime regardless of whether or not they are used.
//! A lot of this overhead owes to TCPs sizeable complexity.
//!
//! That being said SEQEX does lack many of TCP's additional features, such as a dynamic resend timer,
//! keep-alives, and fragmentation. This can be both a pro and a con, as it means there is a
//! lot of efficiency to be gained if these features are not needed or are implemented at a
//! different protocol layer.
//!
//! Neither SEQEX nor TCP are cryptographically secure.
#![warn(missing_docs, rust_2018_idioms)]

mod transport_layer;
pub use transport_layer::*;

/// Module which contains the various error types that can be returned by SEQEX.
pub mod error;

/// This module contains the API for using SEQEX in a no-std environment.
/// This API is low level and is the backbone of the `sync` and `tokio` implementations of SEQEX.
///
/// It contains relatively little in the way of safety and correctness guarantees,
/// so it is not recommended to be used unless necessary.
pub mod no_std;
/// Contains a higher level API for the no_std version of SEQEX.
/// no_std by default is extremely low level.
mod single_thread;

/// This module contains the API for using SEQEX safely in a multithreaded environment.
///
/// This is the recommended API for using SEQEX in non-async code.
#[cfg(feature = "std")]
pub mod sync;

/// This module contains the API for using SEQEX safely with tokio for async-await style code.
#[cfg(feature = "tokio")]
pub mod tokio;
