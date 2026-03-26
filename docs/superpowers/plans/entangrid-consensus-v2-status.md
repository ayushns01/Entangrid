# Entangrid Consensus V2 Status Update

This document is a status update for the active `consensus_v2` redesign work. It complements the implementation plan in [2026-03-25-entangrid-consensus-v2.md](2026-03-25-entangrid-consensus-v2.md).

Important note:

- this work is currently happening on the `codex/consensus-v2` branch
- it is not merged into `main` yet
- it is a progress snapshot, not a claim that V2 is finished

## What Was Implemented

The V2 branch now includes real protocol groundwork beyond the original plan document.

### 1. Shared V2 protocol objects and config

Implemented in the V2 branch:

- `consensus_v2` feature flag
- `ProposalVote`
- `QuorumCertificate`
- `ServiceAttestation`
- `ServiceAggregate`
- certified sync placeholder types

This means the redesign is no longer only theoretical. The branch has concrete transportable objects for certificate-backed ordering and committee-attested service evidence.

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

### 4. First QC-ordering slice

The V2 branch now also has:

- `ProposalVote` verification
- vote storage
- QC assembly at supermajority threshold
- local vote emission for accepted V2 blocks
- limited vote discipline for replayable competing orphan branches
- a lock rule that prevents voting for a competing branch that ignores the highest QC already known on the current canonical chain

This is the beginning of certificate-backed ordering, but not the finished fork-choice system yet.

## What Was Verified

The following package test suite passed on the V2 branch:

```bash
cargo test -p entangrid-consensus -p entangrid-node -p entangrid-sim
```

Passing counts in the latest verified run:

- `entangrid-consensus`: `9/9`
- `entangrid-node`: `39/39`
- `entangrid-sim`: `12/12`

So the branch has strong unit/integration coverage for:

- V2 service evidence validation
- V2 score refresh behavior
- QC vote import and QC assembly
- competing-branch vote constraints
- attestation and aggregate replacement behavior

## What Improved In Live Testing

Healthy V2 bursty testing improved on the service side.

In the latest healthy `6`-validator bursty run on the V2 branch:

- healthy validators no longer collapsed to universal `0.000` service scores
- total gating rejections dropped to `0`
- final service scores were around `0.583` across validators

That is a meaningful improvement over earlier V2 runs where healthy larger-topology bursty networks were collapsing into zero scores and broad false gating.

## What Is Still Broken

The biggest remaining blocker is still structural ordering.

Even after the recent service-evidence fixes, a frozen healthy `6`-validator bursty run still ended with:

- `same_chain_count = 2/6`
- `distinct_tips = 5`
- `height_spread = 42`

That means:

- service evidence is healthier
- healthy validators are not being unfairly gated as aggressively
- but the network still does not converge reliably on one final branch in the larger bursty case

Also important:

- QC objects are being built
- but QC-backed canonical reorg behavior is still not strong enough
- certified sync is still incomplete

## What This Means

The redesign direction still looks correct, but the branch is not ready for PQ integration yet.

The honest status is:

- V2 service evidence is substantially better than the legacy model
- the service-side collapse in healthy larger topologies has improved
- the ordering side is still incomplete
- the remaining pre-PQ blocker is QC-backed canonical fork choice and certified sync

## Next Work

The next implementation priorities are:

1. make QCs actually drive canonical branch selection
2. add stronger certified reorg behavior
3. add certified sync for QC-backed suffix repair
4. rerun healthy and degraded V2 `4/6/8` bursty verification
5. only after that move to PQ-safe signature/session integration

## Recommended Reading Order

If you are new to the current state of the repo, read in this order:

1. [../../current-flow.md](../../current-flow.md)
2. [2026-03-25-entangrid-consensus-v2.md](2026-03-25-entangrid-consensus-v2.md)
3. this status update
