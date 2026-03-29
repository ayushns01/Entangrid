# entangrid-network

## What this crate does

This crate moves protocol messages between validators.

It is the transport layer used by the node runtime to send and receive:

- transactions
- blocks
- sync messages
- heartbeats
- relay receipts
- service attestations and service aggregates
- proposal votes and quorum certificates

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
- one outbound lane per peer
- direct TCP streams that are reused per peer when possible
- length-prefixed `bincode` frame
- signed envelope around the payload

So the current mental model is:

"enqueue onto the peer lane, reuse the stream if it is alive, reconnect only when needed, then receive and verify on the other side."

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
- concurrent inbound session handling is now capped, so one noisy peer cannot force unbounded task fan-out on the listener side
- outbound sends now retry transient connect failures before reporting a hard session failure, which reduces false negatives during localnet churn
- outbound session observations are emitted only after the connect/write/flush path succeeds, so the node's penalty accounting is tied to real delivery attempts instead of pre-send optimistic bookkeeping
- outbound transport now keeps a persistent lane per peer instead of creating a new connection for every message
- inbound sessions can now accept multiple frames on the same stream, which materially reduced handshake churn in larger V2 bursty runs

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

- real PQ transport security
- backpressure-aware gossip
- peer reputation
- discovery
- NAT traversal
- batching or compression
- robust large-topology recovery under heavy stale-restart sync-control pressure

Current main-branch focus:

- keep the transport simple and observable while `consensus_v2` is stabilized on `main`
- support the active V2 evidence and ordering messages cleanly
- defer real PQ transport and larger-topology networking work until the single-machine V2 matrix is green

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
- improve sync and broadcast efficiency
- add better retry and buffering behavior
- prepare for more realistic peer topologies and partial connectivity
- eventually support larger-scale networking beyond localhost experiments

Current runtime note:

- the persistent-lane transport work materially improved Issue 3 on `main`
- the transport/runtime edge is no longer basic bursty delivery or stale-node self-throttling
- the next step is keeping this transport behavior stable while the full V2 matrix is frozen into acceptance gates

In short: today this crate gives us a practical local validator network, but it is meant to become a much stronger transport layer for Entangrid.
