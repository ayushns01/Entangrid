# PQ Stage 1 Status

This document records the current state of `stage-1/pq-integration` as of April 11, 2026.

## Stage 1 Scope

Stage 1 is the first mergeable post-quantum integration milestone for Entangrid.

It now includes:

- scheme-aware signatures, public identities, and session identities
- experimental ML-DSA signing behind `pq-ml-dsa`
- experimental hybrid deterministic + ML-DSA signing
- strict hybrid enforcement for:
  - validator startup identities
  - transactions
  - blocks
  - proposal votes
  - relay receipts
  - service attestations
- service aggregate enforcement transitively through validated embedded service attestations
- experimental hybrid deterministic + ML-KEM session establishment behind `pq-ml-kem`
- encrypted post-handshake framing
- hybrid session TTL expiry and transparent reconnect
- strict hybrid localnet bootstrap through `entangrid-sim`

## What Is Implemented

The PQ plumbing for Stage 1 is in place end to end:

- validators can advertise hybrid public identities in genesis
- nodes can sign validator-originated consensus and service objects with hybrid identities
- hybrid sessions can establish per-stream session material from deterministic + ML-KEM inputs
- every post-handshake frame body is encrypted and authenticated when the hybrid session backend is active
- the simulator can generate a strict all-hybrid localnet and boot it in `consensus_v2`

In other words, the remaining work on this branch is not "add PQ signing" or "add PQ transport." Those pieces are already integrated.

## Current Verification Snapshot

The branch has been verified with the Stage 1 crate and smoke coverage:

```bash
cargo test -p entangrid-crypto --features "pq-ml-dsa pq-ml-kem"
cargo test -p entangrid-network --features pq-ml-kem
cargo test -p entangrid-node --features "pq-ml-dsa pq-ml-kem"
cargo test -p entangrid-sim --features "pq-ml-dsa pq-ml-kem"
cargo test -p entangrid-sim --features "pq-ml-dsa pq-ml-kem" hybrid_enforcement_localnet_boot_smoke_test -- --ignored
```

Recent targeted node coverage on this same line also passed for:

- `sync_`: `58/58`
- `gating`: `3/3`
- `hybrid_enforcement_`: `24/24`
- `quorum_certificate`: `3/3`
- `proposal_votes_build_qc_at_supermajority_threshold`: `1/1`

Recent consensus-hardening work on this same line also added and passed focused node regressions for:

- stronger pre-QC branch preference before any QC exists
- rejecting non-extending local proposal votes on incompatible uncertified branches
- refusing taller but weaker full snapshots before QC

The latest rigorous live matrix coverage on the current branch is split across:

- baseline slice: [rigorous-matrix-1775849508249.md](/Users/ayushns01/Desktop/Repositories/Entangrid/test-results/rigorous-matrix-1775849508249.md)
- non-baseline slice: [rigorous-matrix-1775847670247.md](/Users/ayushns01/Desktop/Repositories/Entangrid/test-results/rigorous-matrix-1775847670247.md)

Current result:

- `12/14` scenarios pass
- all `4`-validator baseline, gated, policy, and abuse scenarios pass
- the only remaining failures are `baseline-6-bursty` and `gated-6-bursty`

**Fix 1 applied (2026-04-11):** Remote vote import now treats `BranchRelation::Unknown` as incompatible. Forensic trace confirmed the pre-QC consensus phase is now working correctly — all 6 nodes converge on the same QC. The remaining failures are post-QC timing races where competing proposals at `height QC+1` arrive from different slots. When the 6-validator bursty scenario runs in isolation (full CPU budget), it achieves `6/6 same_chain fork_observed 0`.

## What Is Still Open Before Final Signoff

The remaining blockers are consensus proof gaps, not cryptography gaps:

- `baseline-6-bursty` is a pre-QC vote scattering scenario now partially resolved by Fix 1; remaining divergence is post-QC timing variance under CPU-starved sequential testing
- `gated-6-bursty` forensically confirmed to achieve a correct QC in all 6 nodes; the remaining failure is competing post-QC child proposals arriving from multiple slots before the network can converge on one canonical child
- because of that, this branch should not yet be presented as fully signed off for final merge

## Explicitly Deferred

These are intentionally not Stage 1 blockers:

- in-stream rekeying or counter-driven session rotation
- encrypted handshake messages
- hidden frame lengths, padding, or richer traffic-shaping defenses
- production-strength key lifecycle and operator tooling
- production-strength PQ backend selection beyond the current experimental ML-DSA / ML-KEM paths
- full hybrid performance matrix tuning beyond the current smoke and focused validation coverage

Those belong to a later PQ hardening milestone rather than the first mergeable integration slice.

## Merge Position

The honest current position is:

- Stage 1 PQ signing, transport, and hybrid bootstrap integration are implemented
- the branch still needs the last two `6`-validator bursty consensus proofs before final merge signoff
- once that closes, `stage-1/main` can absorb this work as the Stage 1 PQ milestone
