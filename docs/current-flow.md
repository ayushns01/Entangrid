# Current Flow

This document explains how the current `main` branch of `Entangrid` works from start to finish in simple language.

Important note:

- `main` now carries both the baseline receipt-driven flow and the newer `consensus_v2` path
- this document mainly explains the baseline/default flow, with V2 callouts where the behavior changes
- the V2 path is the active development focus, but it is still not the final certified-sync / PQ-ready architecture

## 1. A Local Network Is Created

The usual starting point is the simulator.

You run:

```bash
cargo run -p entangrid-sim -- init-localnet --validators 4 --base-dir var/localnet
```

This creates:

- one `genesis.toml`
- one `node.toml` per validator
- one data directory per validator

Each node config includes:

- the node's validator id
- the address it should listen on
- a static list of all other validator peers

So nodes do not discover each other automatically. They already know who their peers are from config.

## 2. Validator Processes Start

You then run:

```bash
cargo run -p entangrid-sim -- up --base-dir var/localnet
```

This starts one real `entangrid-node` process per validator.

Each node:

- loads its config
- loads genesis
- opens a TCP listener
- starts its network worker
- starts its slot/epoch loop
- loads any existing blocks, receipts, snapshots, and metrics

At this point, every node is alive as its own OS process.

## 3. Nodes Find And Contact Peers

Nodes find each other from the `peers` list in `node.toml`.

Each peer entry includes:

- validator id
- TCP address

The network layer then uses direct TCP connections to send messages to those peers.

There is no:

- automatic discovery
- bootnode system
- DHT
- libp2p mesh

The network is pre-wired for localnet experiments.

## 4. Messages Are Sent Over Signed TCP Envelopes

When a node sends a protocol message:

1. it chooses a peer from its config
2. it enqueues the payload onto that peer's outbound lane
3. it reuses the current TCP stream for that peer when possible, or reconnects if the stream is gone
4. it wraps the payload in a signed `SignedEnvelope`
5. it serializes the envelope with `bincode`
6. it sends a length-prefixed frame

When the receiver gets a message:

1. it keeps reading frames from the inbound TCP session
2. it reads the frame length
3. it reads the full frame body
4. it decodes the envelope
5. it checks the payload hash
6. it verifies the sender signature
7. it hands the message to the node runtime

So communication today is:

- direct TCP
- mostly persistent per-peer streams instead of one fresh connection per message
- signed messages
- binary framing
- static peers

## 5. Time Advances In Slots And Epochs

The chain uses two time concepts:

- slots
- epochs

Every node can calculate:

- the current slot
- the current epoch
- who should be proposer for a given slot

This is deterministic, so all honest nodes know the same proposer schedule.

## 6. The Proposer For A Slot Is Determined

For each slot:

- the consensus engine chooses exactly one validator as proposer

This is not mining and not a race.

It is more like:

- "for slot X, validator Y is allowed to propose"

If a node is not the proposer for the current slot, it does not try to create a block for that slot.

## 7. Transactions Enter The Network

Transactions usually come from the simulator.

The simulator can inject different traffic patterns, like:

- steady traffic
- bursty traffic
- large-block pressure

When a node receives a transaction:

- it validates it against current state and mempool
- stores it in mempool
- broadcasts it to peers

So transactions spread through the validator network before they are included in a block.

## 8. Nodes Also Produce Service Evidence

This is the Entangrid-specific part.

While the network is running, nodes also exchange and observe:

- heartbeat pulses
- transaction relay
- block relay

Assigned witnesses can issue relay receipts when they observe the required behavior.

When `consensus_v2` is enabled, witnesses also emit scoped `ServiceAttestation` records and nodes build `ServiceAggregate` records from those attestations.

Those receipts are later used to compute service scores.

That is how the project tries to connect network usefulness to consensus participation.

## 9. The Scheduled Proposer Tries To Propose A Block

When a new slot arrives, the proposer node checks:

- am I the scheduled proposer for this slot?

If no:

- do nothing and continue waiting

If yes:

- continue toward block production

If service gating is enabled, there is one more check:

- is my current service score above the configured threshold?

In the baseline path, that score comes from the rolling receipt-driven counters.

In `consensus_v2`, local proposer gating now behaves more carefully:

- a confirmed low prior-epoch aggregate can reject the proposer
- missing or insufficient prior-epoch evidence skips enforcement instead of forcing a false rejection

If the confirmed score is too low:

- the node misses the slot
- this counts as a gating rejection

If the score is good enough:

- it can build and broadcast a block

## 10. The Block Is Built

The proposer builds a block using:

- the current parent tip
- selected mempool transactions
- commitment/service-related data if required

The block is then:

- hashed
- signed by the proposer
- broadcast to peers

## 11. Other Nodes Validate The Block

When a peer receives the block, it checks:

- correct proposer for the slot
- valid block signature
- correct parent hash
- correct transaction root
- correct commitment/topology root
- valid ledger replay

If the block extends the node's current tip:

- the node accepts it
- updates the ledger
- stores the block
- updates snapshot and metrics

## 12. If The Block Does Not Extend The Current Tip, A Fork Symptom Appears

If the received block's parent does not match the node's current tip:

- the node does not immediately reorg
- it treats the block as an orphan or competing branch
- it logs the event as a fork symptom

This is why bursty runs can temporarily split the network into short competing branches.

The current `main` branch now has proposal votes and quorum certificates behind `consensus_v2`, and equal-QC uncertified siblings are no longer allowed to steal the canonical tip just because they gained extra local votes.

Certified sync and QC-dominant branch choice are now active on `main`, so healthy bursty `6/7/8` runs can reconverge structurally on one tip. Restarted stale nodes now also recover cleanly enough to rejoin the final tip without late full-snapshot rescue; the next job is proving that across the full matrix, not inventing another recovery mechanism.

## 13. Sync Tries To Repair Stale Or Split Nodes

Nodes regularly exchange sync messages:

- `SyncStatus`
- `SyncRequest`
- `SyncBlocks`
- `SyncResponse`

If a peer is only behind on the same branch:

- incremental sync can send just the missing suffix

If a peer is unknown, diverged, or too far behind:

- full snapshot sync is used

This is how the network eventually converges back toward one chain.

So today the single chain emerges from:

- deterministic proposer schedule
- local block acceptance rules
- sync-based convergence

and, on the V2 path, the first QC-backed branch constraints.

## 14. Service Scores Are Refreshed

While blocks and receipts accumulate, each node also refreshes service scores.

The current prototype uses rolling counters based on:

- uptime-like heartbeat evidence
- timely relay deliveries
- distinct peers reached
- penalties like failed sessions and invalid receipts

If service gating is enabled, those scores affect whether a proposer can use its scheduled slot.

So the current Entangrid rule is:

- "it being your turn is not enough"
- "you also need acceptable recent network usefulness"

## 15. Data Is Written To Disk

As the run continues, the node persists:

- blocks
- receipts
- snapshots
- metrics
- logs
- orphaned blocks if forks were observed

That makes it possible to inspect the final state after a run and compare validators.

## 16. End Result

If the run goes well:

- all nodes end on the same tip
- parent hashes are consistent
- snapshots match the accepted chain
- service scores reflect recent observed behavior
- degraded validators may lose proposer eligibility

If the run is stressed:

- nodes may temporarily diverge
- fork/orphan events appear
- sync later tries to pull them back together

## Short Summary

The current `main` branch flow is:

1. simulator creates configs  
2. nodes start as separate processes  
3. nodes connect to static peers over signed TCP  
4. slot time advances  
5. transactions and heartbeats spread  
6. the scheduled proposer tries to build a block  
7. service gating may allow or block that proposer  
8. peers validate and accept the block if it extends their tip  
9. competing blocks become orphans  
10. sync messages repair stale or split nodes  
11. receipts update service scores  
12. the network eventually converges on one chain

## Important Limitation

This is a working prototype flow, but not the final intended architecture.

The active V2 redesign is moving toward:

- committee-attested service evidence
- QC-backed ordering
- certified sync

That future direction is documented in [superpowers/plans/2026-03-25-entangrid-consensus-v2.md](superpowers/plans/2026-03-25-entangrid-consensus-v2.md).
The latest branch-level status update is documented in [superpowers/plans/entangrid-consensus-v2-status.md](superpowers/plans/entangrid-consensus-v2-status.md).
