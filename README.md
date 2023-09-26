# Sequential Exchange Protocol

The reference implementation of the **Sequential Exchange Protocol**, or SEP.

SEP is a lightweight, peer-to-peer transport protocol that guarantees packets of data will be losslessly received by the remote peer, and can optionally guaranteed that specified packets arrive in the order that they were sent. In addition, SEP facilitates stateful exchanges between two peers, giving each peer the opportunity to "reply" to any packet sent by the remote peer. This makes SEP particularly well-suited for writing async-await code, because unlike TCP, SEP will handle multiplexing each reply to the correct awaiter. Even without async-await, a simple `match` statement is sufficient to correctly multiplex packets to their handling code.

A "stateful exchange" is defined here as a sequence of packets, where the first packet
initiates the exchange, and all subsequent packets are replies to the previous packet in the
exchange. Every exchange can be thought of as a linked list, where the head node is a packet containing a normal payload, and all subsequent nodes are replies to previous nodes. The final node is always a simple acknowledgement packet, or Ack, that signals a given exchange is over. Both peers are guaranteed to agree upon the "topology" of these links. Links will never get crossed, replies will always be received and understood, a peer will never deadlock awaiting a reply, and in general it is much easier to write bug-free networking code.

SEP is a tiny, dead simple protocol, the core of which we have implemented in less than 1000 lines of code.

SEP is transport agnostic, and will not take over an entire UDP socket. As such it is relatively easier to run SEP in parrallel with raw UDP, or even with itself. Multiple instances of SEP can be opened between two peers and communication over each can occur in parrallel, making it very easy to reduce or even eliminate front-of-line latency in performance critical applications.

## Why not TCP?

TCP only guarantees packets will be received in the same order they were sent.
It has no inherent concept of "replying to a packet" and as such it cannot guarantee both sides
of a conversation have the same view of any stateful exchanges that take place. This must be implemented manually by the user of TCP.

TCP is also much higher overhead. It requires a 1.5 RTT handshake to begin any connection,
it has a larger amount of metadata that must be transported with packets, and it has quite a few
features that slow down runtime regardless of whether or not they are used.
A lot of this overhead owes to TCPs sizeable complexity.

That being said SEP does lack many of TCP's additional features, such as a dynamic resend timer,
keep-alives, and fragmentation. This can be both a pro and a con, as it means there is a
lot of efficiency to be gained if these features are not needed or are implemented at a
different protocol layer.

Neither SEP nor TCP are cryptographically secure.
