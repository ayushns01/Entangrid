# Entangrid Consensus V2 Status Update

This document is a status update for the active `consensus_v2` redesign work. It complements the implementation plan in [2026-03-25-entangrid-consensus-v2.md](2026-03-25-entangrid-consensus-v2.md).

Important note:

- this work now exists on `main` as the active protocol line
- the older V1 benchmark line is preserved on `codex/consensus-v1`
- `codex/consensus-v2` remains the staging branch for continued V2 work
- this is still a progress snapshot, not a claim that V2 is finished

## What Was Implemented

The active V2 code on `main` now includes real protocol groundwork beyond the original plan document.

### 1. Shared V2 protocol objects and config

Implemented in the active V2 code:

- `consensus_v2` feature flag
- `ProposalVote`
- `QuorumCertificate`
- `ServiceAttestation`
- `ServiceAggregate`
- certified sync placeholder types

The redesign is no longer only theoretical. `main` now has concrete transportable objects for certificate-backed ordering and committee-attested service evidence.

### 2. Committee-attested service evidence

The V2 service plane now:

- uses the subject validator's actual assigned witnesses as the committee
- scores only witness-observable obligations
- stores and imports service attestations and aggregates in the node runtime
- computes local proposer eligibility from prior-epoch aggregates instead of the legacy local receipt-only score path when `consensus_v2` is enabled

### 3. Better evidence timing and repair

Recent V2 work also improved how service evidence is produced:

- lagged service attestation emission to avoid publishing too early
- preserved the last known score when no newer valid aggregate is available
- allowed improved service attestations and aggregates to replace earlier weaker versions
- added recent-epoch backfill so late-arriving receipts can refresh earlier weak evidence

This was important because earlier V2 runs were freezing zeroed evidence too early in bursty larger-validator scenarios.

### 4. QC ordering and certified sync activation

The active V2 code now also has:

- `ProposalVote` verification
- vote storage
- QC assembly at supermajority threshold
- local vote emission for accepted V2 blocks
- limited vote discipline for replayable competing orphan branches
- a lock rule that prevents voting for a competing branch that ignores the highest QC already known on the current canonical chain
- certified sync requests and responses
- recent-QC anchor exchange in sync status and sync repair requests
- highest-shared-QC certified suffix repair in live runs

This is the beginning of certificate-backed ordering and repair, but not the finished fork-choice system yet.

## What Was Verified

The following package test suite passed on the active V2 code:

```bash
cargo test -p entangrid-types -p entangrid-consensus -p entangrid-node -p entangrid-sim
```

Passing counts in the latest verified run:

- `entangrid-types`: `4/4`
- `entangrid-consensus`: `9/9`
- `entangrid-node`: `51/51`
- `entangrid-sim`: `13/13`

So the active V2 code has strong unit/integration coverage for:

- V2 service evidence validation
- V2 score refresh behavior
- QC vote import and QC assembly
- competing-branch vote constraints
- attestation and aggregate replacement behavior

## What Improved In Live Testing

Comparative bursty testing now shows two different truths at once:

- certified sync activation is now real in live runs
- larger-validator convergence is now much healthier structurally

Most importantly:

- `v2` can now punish degraded validators in the smaller benchmark topologies instead of leaving them at `1.000`
- `degraded/4` and `degraded/5` on `v2` already produce real gating while keeping honest validators at `1.0`
- healthy `6`-validator runs keep honest service scores high much more often than the older V2 service-collapse cases

Latest repeated healthy `6/7/8` bursty validation on `main` showed:

- `6`: `same_chain = 6/6`, height `32`, `certified_sync_served > 0`, `full_sync_applied = 0`
- `7`: `same_chain = 7/7`, height `19`, `certified_sync_served > 0`, `full_sync_applied = 0`
- `8`: `same_chain = 8/8`, height `10`, `certified_sync_served > 0`, `full_sync_applied = 0`

The latest node-runtime hardening behind those results was a stale certified-sync guard:

- certified-sync responses are now ignored when they would downgrade a node that already holds a newer certified tip
- that closes the last oscillation we were still seeing in the 8-validator healthy shutdown path

That means both Issue 1 and Issue 2 have been cut down substantially:

- certified sync is active in live recovery when needed, and its availability remains live in repeated healthy runs
- QC-dominant branch choice plus the pending-certified-child lane now keep healthy `6/7/8` runs on one tip

## What Is Still Broken

The biggest remaining blocker is now service-score stability and gating semantics at larger validator counts.

The latest healthy bursty runs on `main` make that pretty clear:

- structural convergence is now much better than the older `3/6`, `2/7`, `3/8` shapes
- full-snapshot fallback is no longer driving these healthy `6/7/8` runs
- but `7` and especially `8` still show service-score collapse and `0` gating rejections

In the latest cross-branch comparison:

- `v1 degraded` average same-chain ratio was `0.587`
- `v2 degraded` average same-chain ratio was `0.487`
- `v1 degraded` average target score was `0.090`
- `v2 degraded` average target score was `0.217`

So the service side is still the next thing to beat: the full live matrix is not green yet because larger healthy runs can converge structurally while still ending with incorrect score/gating behavior.

## What This Means

The redesign direction still looks correct, but `main` is not ready for PQ integration yet.

The honest status is:

- V2 service evidence is substantially better than the legacy model
- degraded punishment is working in smaller benchmark cases but is still not reliable enough across the full matrix
- Issue 1 certified-sync activation is effectively closed
- Issue 2 QC-backed canonical branch selection is effectively closed for the healthy `6/7/8` structural runs we just repeated
- the remaining pre-PQ blocker is service-gating enforcement at scale

## Next Work

The next implementation priorities are:

1. tighten service-gating enforcement at scale
2. rerun healthy and degraded V2 `4/5/6/7/8` bursty verification against the V1 benchmark line
3. only after that move to PQ-safe signature/session integration

## Recommended Reading Order

If you are new to the current state of the repo, read in this order:

1. [../../current-flow.md](../../current-flow.md)
2. [2026-03-25-entangrid-consensus-v2.md](2026-03-25-entangrid-consensus-v2.md)
3. this status update
