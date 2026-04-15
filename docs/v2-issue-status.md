# Entangrid V2 Issue Status

Status date: 2026-04-11 (updated after Fix 1 — remote vote ancestry hardening)

This note summarizes where the original four V2 stabilization issues stand now and separates them from the current remaining blockers on the PQ-enabled consensus line.

## Current Verdict

- Issue 1: solved enough
- Issue 2: solved enough once QC-backed state exists
- Issue 3: solved enough at the default profile and the currently covered policy variants
- Issue 4: materially improved, but one recovery-shaped failure still survives in `gated-6-bursty`
- Current remaining blockers: `baseline-6-bursty` and `gated-6-bursty`

## Verification Basis

This status is based on:

- the latest rigorous live matrix slices:
  - [rigorous-matrix-1775849508249.md](/Users/ayushns01/Desktop/Repositories/Entangrid/test-results/rigorous-matrix-1775849508249.md)
  - [rigorous-matrix-1775847670247.md](/Users/ayushns01/Desktop/Repositories/Entangrid/test-results/rigorous-matrix-1775847670247.md)
- recent targeted node regressions in `entangrid-node` covering:
  - stronger pre-QC branch preference
  - vote rejection on non-extending incompatible uncertified branches
  - refusal to adopt taller but weaker full snapshots before QC
  - sync, gating, hybrid-enforcement, and quorum-certificate focused suites under `pq-ml-dsa,pq-ml-kem`

## Issue 1: Certified Sync Activation

Issue 1 was about making certified sync real in live runs instead of dead code.

Current status:

- certified sync is active in the runtime
- the recovery machinery is no longer limited to blind snapshot fallback
- this is no longer the main blocker in the live matrix

Issue 1 should now be treated as **solved enough** for the current stage.

## Issue 2: Canonical Branch Selection

Issue 2 was about making QC-backed branch choice dominate instead of letting local uncertified noise keep replacing the tip.

Current status:

- QC-backed branch choice is materially stronger than before
- stale certified responses no longer roll back a newer local certified tip
- the remaining live failures happen before a stable QC anchor forms, so they are not the same old post-QC fork-choice bug

Issue 2 should now be treated as **solved enough**, with the important caveat that pre-QC convergence is still incomplete.

## Issue 3: Service Evidence And Proposer Gating

Issue 3 was about whether service evidence and gating actually punish degraded validators without dragging honest validators down.

Current status in the latest matrix:

- default gated scenarios pass
- the policy variants that were previously fragile now pass in the latest `12/14` run
- the remaining failures are structural fork/sync failures in `6`-validator bursty runs, not service-score collapse

Issue 3 should now be treated as **solved enough** for the current default profile and covered matrix variants.

## Issue 4: Stale-Restart Recovery

Issue 4 was the stale-restart catch-up gap.

Current status:

- stale-restart recovery is no longer the active blocker
- the protocol focus has mostly moved away from restart repair and onto bursty convergence, but one follower-recovery failure shape still survives in `gated-6-bursty`

Issue 4 should now be treated as **materially improved but not completely closed** for the current stage.

## Current Remaining Blockers

The active blockers are now narrower than the original four-issue framing, but they are still real merge blockers.

Latest live result:

- overall matrix: `12/14`
- failing scenarios:
  - `baseline-6-bursty`
  - `gated-6-bursty`

What is happening:

- in `baseline-6-bursty`, pre-QC vote scattering causes multi-tip divergence before a stable QC anchor forms
- in `gated-6-bursty`, most validators converge on a QC but competing post-QC proposals at `height QC+1` lead to a 3-way permanent split because the network receives multiple proposals from different slots before everyone agrees on a canonical child
- forensic trace (2026-04-11) confirmed the pre-QC phase now converges correctly — all 6 nodes build the same QC (height 13, block `[e7,c1,a0,2b]`, 5/5 votes) — the remaining failure is post-QC: nodes that received the QC at different times each start proposing at `height QC+1` in different slots, producing competing children before the network can exchange votes and converge on one
- **Fix 1 applied (2026-04-11):** `proposal_vote_branches_are_compatible` now treats `BranchRelation::Unknown` as incompatible instead of compatible. Previously, when a peer vote arrived with unresolvable ancestry (blocks out of order under bursty load), the node accepted it as if it were on the same branch, scattering the vote map and preventing quorum from forming. With Fix 1, the vote is rejected until ancestry can be proven via sync. This reduced observed fork count in `gated-6-bursty` from 17 → 12 and occasionally achieves a perfect `6/6 same_chain` run when CPU contention is low.
- the 12/14 matrix result is unchanged because the two remaining failures are driven by post-QC timing variance under sequential CPU-starved test execution; when the 6-validator scenario is run in isolation the result is `6/6 fork_observed 0`

## Bottom Line

The original four V2 issues are no longer the right way to describe the active risk.

The honest current state is:

- certified sync is live
- service evidence and gating are in much better shape
- stale restart is no longer the broad matrix-wide blocker
- the remaining work is the final bursty convergence proof in the two `6`-validator scenarios
