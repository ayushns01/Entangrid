# Entangrid Consensus V2 Status Update

This document is a status update for the active `consensus_v2` work as of April 9, 2026. It complements the implementation plan in [2026-03-25-entangrid-consensus-v2.md](2026-03-25-entangrid-consensus-v2.md).

Important note:

- the active proof work is now happening on the PQ-enabled `main` line
- the older V1 benchmark line is preserved on `codex/consensus-v1`
- this is still a progress snapshot, not a claim that V2 is fully finished

## What Is Implemented

The active line now contains the major V2 protocol slices together with Stage 1 PQ integration:

- `consensus_v2` protocol objects and runtime handling for `ProposalVote`, `QuorumCertificate`, `ServiceAttestation`, and `ServiceAggregate`
- certified sync activation and QC-aware recovery
- proposer gating from prior-epoch service aggregates instead of raw local receipt views
- strict hybrid signature enforcement for validator startup identities, transactions, blocks, proposal votes, relay receipts, and service attestations
- hybrid session establishment plus encrypted post-handshake framing on the transport path
- strict hybrid localnet bootstrap through `entangrid-sim`

The project is no longer waiting to "start PQ later." The PQ-capable consensus line already exists.

## Latest Consensus Hardening

The most recent convergence work on this branch added:

- richer `sync-request-throttled` and `sync-blocks-rejected` diagnostics
- stronger pre-QC branch comparison using accumulated vote support
- stricter rejection of local later-height votes on incompatible non-extending branches before QC
- refusal to adopt taller but weaker full snapshots before a local QC exists
- targeted regressions for those pre-QC cases in `entangrid-node`

Those changes improved the shape of the remaining failures, but did not eliminate them.

## What Was Verified

Recent focused verification on the active line includes:

```bash
cargo test -p entangrid-node stronger_vote_supported_branch_before_qc -- --nocapture
cargo test -p entangrid-node validator_cannot_vote_on_taller_non_extending_branch_before_qc -- --nocapture
cargo test -p entangrid-node later_slot_vote_from_same_validator_replaces_earlier_under_certified_head -- --nocapture
cargo test -p entangrid-node taller_weaker_full_snapshot_does_not_replace_stronger_local_branch_before_qc -- --nocapture
```

Latest rigorous matrix:

- [rigorous-matrix-1775726814105.md](/Users/ayushns01/Desktop/Repositories/Entangrid/test-results/rigorous-matrix-1775726814105.md)

Current matrix result:

- `12/14` scenarios pass
- `policy-threshold-055` and the other `4`-validator policy variants pass
- both abuse scenarios pass
- the only remaining failures are `baseline-6-bursty` and `gated-6-bursty`

## What The Remaining Failure Means

The current blocker is not "certified sync does not exist," "service evidence is broken," or "stale restart is still failing."

The latest evidence points to a narrower problem:

- under bursty `6`-validator load, nodes still fragment across uncertified branches before any stable QC anchor forms
- recent pre-QC fixes improved branch preference and prevented some avoidable branch flips
- even so, the live matrix still ends with forked tips in the two bursty `6`-validator scenarios

So the active gap is a pre-QC convergence problem, not the earlier broad V2 stabilization story.

## What This Means

The honest status is:

- the V2 direction is still correct
- PQ Stage 1 integration is already present on the active line
- the branch is not yet ready for final consensus signoff because the last two bursty `6`-validator scenarios still fail

## Next Work

The next implementation priorities are:

1. add a stronger pre-QC convergence anchor so bursty `6`-validator runs stop splitting before the first QC
2. rerun the rigorous matrix until `baseline-6-bursty` and `gated-6-bursty` pass
3. freeze that matrix result as the acceptance gate for the Stage 1 merge path

## Recommended Reading Order

If you are new to the current state of the repo, read in this order:

1. [../../current-flow.md](../../current-flow.md)
2. [2026-03-25-entangrid-consensus-v2.md](2026-03-25-entangrid-consensus-v2.md)
3. this status update
