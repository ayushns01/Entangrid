# entangrid-network

## What this crate does

This crate moves protocol messages between validators.

It is the transport layer used by the node runtime to send and receive:

- transactions
- blocks
- sync messages
- heartbeats
- relay receipts

It also reports network events back to the node, such as:

- message received
- session observed
- session failure

## How it currently works

The current implementation is intentionally simple and explicit.

### Peer model

Peers are static.

Each node is given a list of peers in its config, and the network crate uses those known addresses directly.

There is currently:

- no peer discovery
- no DHT
- no bootnode system
- no internet-scale topology management

### Transport model

Current transport is:

- Tokio TCP listener for inbound connections
- one outbound send request at a time through an internal channel
- direct TCP connect to the peer address
- length-prefixed `bincode` frame
- signed envelope around the payload

So the current mental model is:

"open connection, send signed message, receive and verify on the other side."

### Message integrity

Before sending, the crate:

- hashes the payload
- signs the hash
- wraps it in `SignedEnvelope`

On receive, the crate:

- re-hashes the payload
- verifies the signature
- emits a `NetworkEvent::Received` only if validation passes

Recent hardening:

- inbound frames now have a fixed maximum size before allocation
- oversized frames are rejected early instead of letting an attacker force unbounded memory growth through a large declared frame length

### Session observations

The crate also asks the crypto backend to derive session material so the node can observe transcript/session events.

Right now this is backed by the deterministic development crypto backend.

### Fault injection

The network crate already supports simple localnet fault behavior through `FaultProfile`:

- artificial delay
- deterministic outbound drops
- disable outbound traffic entirely

This is useful for testing service scoring and degraded-node behavior.

## What it is today

Today this is a clean localnet transport layer, not a production peer-to-peer stack.

It does not yet provide:

- persistent authenticated sessions
- real PQ transport security
- backpressure-aware gossip
- peer reputation
- discovery
- NAT traversal
- batching or compression

## Why this crate matters

Entangrid is trying to couple networking and consensus, so the transport layer is not just plumbing here.

This crate is part of how the system observes:

- who relayed what
- who was reachable
- who stayed online
- who looked degraded

## Where we want to take it

This crate should become a more serious communication layer over time.

Future direction:

- replace dev session derivation with real PQ-secure session setup
- add persistent connections instead of opening a new TCP connection per message
- improve sync and broadcast efficiency
- add better retry and buffering behavior
- prepare for more realistic peer topologies and partial connectivity
- eventually support larger-scale networking beyond localhost experiments

In short: today this crate gives us a practical local validator network, but it is meant to become a much stronger transport layer for Entangrid.
