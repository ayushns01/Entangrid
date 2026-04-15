# Sync And Recovery

This document explains how node synchronization and recovery work on the current `main` branch of `Entangrid`.

The short version:

- nodes advertise lightweight sync state continuously
- same-branch peers prefer incremental repair over full snapshots
- certified sync provides a QC-anchored recovery path when local state is stale or diverged
- startup sync barriers stop a restarted node from immediately proposing while peers are still ahead
- the recovery path is materially stronger than older versions, but the hardest bursty `6`-validator cases are not fully closed yet

For current branch status, also see:

- [current-flow.md](current-flow.md)
- [verification.md](verification.md)
- [v2-issue-status.md](v2-issue-status.md)
- [consensus-current-issue.md](consensus-current-issue.md)

## Why Sync Matters Here

`Entangrid` is not just trying to deliver blocks eventually.
It is trying to keep a multi-node validator runtime healthy while:

- service evidence is being produced and scored
- proposal votes and quorum certificates are shaping branch choice
- degraded or bursty localnets are deliberately injected by the simulator

That means sync has to do more than "copy the longest chain."
It has to repair lagging nodes without:

- trusting bad snapshots
- rolling a node backward to stale certified state
- letting a restarted node create fresh divergence before it is caught up

## The Sync Surfaces

The current sync path uses several protocol messages with different purposes:

- `SyncStatus`
  A lightweight summary a node broadcasts periodically so peers know its current height, tip hash, and QC frontier.
- `SyncRequest`
  A direct request for recovery data from a peer.
- `SyncResponse`
  A full snapshot-style response carrying a complete `ChainSnapshot`.
- `SyncBlocks`
  An incremental same-branch repair response carrying a `ChainSegment`.
- `CertifiedSyncRequest`
  A QC-aware request that asks a peer to repair from the highest shared certified frontier.
- `CertifiedSyncResponse`
  A QC-anchored response that can carry certified headers, blocks, quorum certificates, and service aggregates.

At the type layer, these live in the shared protocol model so every node speaks the same recovery language.

## Mental Model

Think of recovery in three tiers:

1. **Lightweight awareness**
   Nodes continuously exchange `SyncStatus` so they know who is ahead and what certified frontier exists.
2. **Cheap same-branch repair**
   If a peer is clearly on the same branch and only missing a suffix, the node prefers incremental repair with `SyncBlocks`.
3. **Heavy repair**
   If the peer is unknown, diverged, stale in a more complex way, or needs QC-anchored recovery, the node escalates to certified sync or full snapshot fallback.

That is the core idea:

- use cheap repair when branch relationship is clear
- use stronger repair when safety or divergence is unclear

## Startup Recovery

One of the most important current behaviors is the **startup sync barrier**.

When a node restarts, it can come back with:

- an old snapshot
- old blocks on disk
- a valid local tip that is still behind the rest of the network

Without a barrier, that restarted node could immediately resume local proposer logic and create more divergence before recovery completes.

The current runtime avoids that by:

- restoring local state from snapshot or replay
- checking whether startup sync should be enabled
- holding proposal behavior behind a startup barrier if peers are present and the local node is plausibly behind
- advertising the certified frontier instead of pushing an uncertified local suffix as if it were fresh truth

In practice, this means a restarted node behaves more like:

"I have local state, but I should confirm where the network is before I act like I am up to date."

## Periodic Sync Behavior

While the node is live, the runtime keeps a periodic sync tick active.

That tick is responsible for:

- broadcasting `SyncStatus`
- noticing when peers are ahead
- requesting catch-up from the best available ahead peer
- preferring repair that matches the local branch relationship

Recent hardening made this more recovery-focused:

- nodes now proactively request catch-up while peers are known ahead
- repeated sync requests from the same peer are throttled
- same-branch peers prefer incremental block suffix repair over full snapshot transfer
- stale certified responses are skipped instead of rolling a node backward after it already advanced

## Same-Branch Incremental Repair

The cheapest repair path is a suffix catch-up.

When a peer is:

- on the same branch
- clearly ahead
- close enough that a suffix repair is cheaper than a whole snapshot

the node prefers `SyncBlocks` with a `ChainSegment`.

This path works well when the local node mostly has the right history and simply needs:

- missing blocks
- matching receipts
- matching proposal-vote context

This is the ideal path because it avoids:

- unnecessary full-state transfer
- throwing away local progress
- more expensive recovery than the situation actually needs

## Full Snapshot Fallback

Full snapshot sync still exists for safety.

The runtime falls back to a full `SyncResponse` snapshot when a peer is:

- unknown
- clearly diverged
- too far behind
- unable to describe its stale state well enough for safe incremental repair

That fallback exists because suffix repair only works when the branch relationship is sufficiently clear.
If that assumption breaks, snapshot sync is safer than repeatedly trying the wrong incremental repair.

The important hardening here is that snapshot adoption is no longer naive.
Incoming snapshot-style recovery is validated more carefully before it can influence local state.

## Certified Sync

`consensus_v2` adds the most important advanced recovery path: **certified sync**.

This path is meant for cases where recovery should anchor on QC-backed state rather than only tip gossip.

The high-level flow is:

1. a node advertises its known QC frontier in `SyncStatus`
2. a lagging node asks for certified repair with `CertifiedSyncRequest`
3. the responder returns a `CertifiedSyncResponse` anchored at the highest shared certified point
4. the recovering node can then rebuild from certified state and continue suffix repair if needed

This is stronger than a plain "give me your current chain" model because it gives recovery a certified anchor instead of trusting whichever uncertified tip happened to arrive first.

The current certified sync response can include:

- certified headers
- blocks
- quorum certificates
- service aggregates

That lets recovery bring over both ordering evidence and the service-evidence context that the active runtime depends on.

## Fork-Aware Catch-Up

The current sync path is deliberately fork-aware.

It does not assume that any taller state is automatically better.

Recent runtime hardening added behavior like:

- refusing taller but weaker full snapshots before a local QC exists
- keeping QC-backed branch choice stronger than uncertified local noise
- comparing branches more carefully before accepting later votes or repair data
- escalating after same-frontier suffix repair fails

This matters because the hardest live failures are no longer "node is simply behind."
They are convergence failures in bursty conditions where several branches may be locally plausible for a while.

## Safety And Abuse Controls

Recovery logic is also protected against spam and malformed inputs.

Important current controls include:

- fixed maximum inbound frame size before allocation
- inbound session caps to limit listener task fan-out
- per-peer rate limiting for spam-prone sync, receipt, and transaction traffic
- sync-request throttling so one peer cannot trigger unbounded recovery work
- structural validation of incoming snapshot history
- validation and deduplication of incoming receipts before they affect local service scoring
- local rejection and logging of malformed peer blocks, transactions, and receipts instead of crashing the process

These are important because sync is a natural attack surface.
If recovery accepts too much too eagerly, a malicious or simply noisy peer can waste CPU, memory, or corrupt local scoring state.

## Where Recovery Is Stronger Now

Compared with older iterations, the current line is much better at:

- keeping certified sync active as a real runtime path
- avoiding rollback onto stale certified responses
- preventing restarted nodes from replaying stale proposer slots immediately
- holding proposals behind a startup barrier while peers are still ahead
- preferring same-branch suffix repair when possible
- exposing recovery state through metrics and diagnostics

This is why stale-restart recovery is no longer described as the broad matrix-wide blocker.

## What Is Still Not Fully Solved

The sync and recovery path is materially improved, but not "done."

The hardest remaining issues still show up in the rigorous matrix:

- `baseline-6-bursty`
  Pre-QC convergence can still fragment across competing uncertified branches before a stable certified anchor exists.
- `gated-6-bursty`
  A lagging follower can still get stranded above the certified head and fail to rejoin the converged suffix cleanly.

So the honest current position is:

- sync and recovery are real, implemented, and much stronger than before
- certified sync is live
- startup barrier behavior is live
- same-branch incremental repair is live
- the recovery model is still not fully signed off until the rigorous matrix reaches `14/14`

## Practical Reading Order

If you want to understand the current recovery model without reading the entire node runtime at once, read in this order:

1. [current-flow.md](current-flow.md)
2. [crates/entangrid-network/README.md](../crates/entangrid-network/README.md)
3. [crates/entangrid-node/README.md](../crates/entangrid-node/README.md)
4. [consensus-current-issue.md](consensus-current-issue.md)
5. [verification.md](verification.md)

That sequence gives you:

- the user-level runtime story
- the transport model
- the node-side recovery decisions
- the known remaining failure modes
- the commands that verify current behavior
