# Entangrid V2 Issue Status

Status date: 2026-04-11

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

- in `baseline-6-bursty`, all validators can reach the same height while still splitting across multiple uncertified tips before the first stable QC anchor forms
- in `gated-6-bursty`, most validators converge but one follower can still remain stranded above the certified head and fail to rejoin the converged suffix
- recent fixes improved branch scoring, vote discipline, snapshot preference, and same-frontier repair escalation
- those fixes were directionally correct, but not enough to make the last two scenarios converge

## Bottom Line

The original four V2 issues are no longer the right way to describe the active risk.

The honest current state is:

- certified sync is live
- service evidence and gating are in much better shape
- stale restart is no longer the broad matrix-wide blocker
- the remaining work is the final bursty convergence proof in the two `6`-validator scenarios
