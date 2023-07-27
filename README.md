# Sequential Exchange Protocol

The reference implementation of the **Sequential Exchange Protocol**, or SEP.

SEP is a peer-to-peer transport protocol that guarantees packets of data will always be received
in the same order they were sent. In addition, it also guarantees the sequential consistency of
stateful exchanges between the two communicating peers.

A "stateful exchange" is defined here as a sequence of packets, where the first packet
initiates the exchange, and all subsequent packets are replies to the previous packet in the
exchange.

SEP guarantees both peers will agree upon which packets are members of which exchanges,
and it guarantees each packet is received by each peer in sequential order.

SEP is a tiny, dead simple protocol and we have implemented it here in less than 500 lines of code.

## Why not TCP?

TCP only guarantees packets will be received in the same order they were sent.
It has no inherent concept of "replying to a packet" and as such it cannot guarantee both sides
of a conversation have the same view of any stateful exchanges that take place.

TCP is also much higher overhead. It requires a 1.5 RTT handshake to begin any connection,
it has a larger amount of metadata that must be transported with packets, and it has quite a few
features that slow down runtime regardless of whether or not they are used.
A lot of this overhead owes to TCPs sizeable complexity.

That being said SEP does lack many of TCP's additional features, such as a dynamic resend timer,
keep-alives, and fragmentation. This can be both a pro and a con, as it means there is a
lot of efficiency to be gained if these features are not needed or are implemented at a
different protocol layer.

Neither SEP nor TCP are cryptographically secure.
