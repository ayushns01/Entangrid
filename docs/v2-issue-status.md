# Entangrid V2 Issue Status

Status date: 2026-03-30

This note summarizes the current status of the four V2 consensus issues that have driven the recent stabilization work.

It answers three questions for each issue:

- what the issue is
- what has been fixed
- what is still not fixed

## Current Verdict

- Issue 1: mostly solved
- Issue 2: solved enough
- Issue 3: solved at the default protocol profile, but not fully robust across all policy variants
- Issue 4: still open

## Verification Basis

The current status is based on:

- full code verification:
  - `cargo test -p entangrid-types -p entangrid-consensus -p entangrid-network -p entangrid-node -p entangrid-sim`
- rigorous live matrix:
  - [rigorous-matrix-1774892617700.md](/Users/ayushns01/Desktop/Repositories/Entangrid/test-results/rigorous-matrix-1774892617700.md)
- fresh stale-restart `8` live reruns:
  - [stale8-certified-tail-fix-1774903000](/Users/ayushns01/Desktop/Repositories/Entangrid/var/stale8-certified-tail-fix-1774903000)
  - [stale8-followup-fix-1774903300](/Users/ayushns01/Desktop/Repositories/Entangrid/var/stale8-followup-fix-1774903300)

## Issue 1: Certified Sync Activation

### What It Is

Issue 1 is the problem of making V2 certified recovery actually activate in live runs.

The intended design is:

- nodes recover from the highest shared QC / certified anchor
- recovery is proof-driven
- legacy full snapshot sync is fallback, not the main path

### What Was Wrong

Earlier V2 runs showed:

- `certified_sync_applied = 0`
- `certified_sync_served = 0`
- legacy sync dominating recovery

That meant the protocol had certified-sync code, but live nodes were not really using it.

### What Was Fixed

The main fixes were:

- peer QC anchor history instead of a single QC hint
- highest-shared-QC selection for recovery
- QC-aware sync mode selection
- proactive certified push
- anchored suffix tail attached to V2 recovery paths
- follow-up sync requests chained after successful recovery applies

### What Is Fixed Now

Issue 1 is now materially better:

- certified sync is real and covered by tests
- V2 recovery is no longer dead code
- the node-level recovery machinery is much closer to the intended architecture

### What Is Still Not Fully Fixed

Issue 1 is not perfectly clean in every live replay:

- stale-restart recovery does not always finish through certified sync alone
- some runs finish catch-up through incremental or snapshot follow-up instead

So Issue 1 is best described as **mostly solved**, not perfectly complete.

## Issue 2: Canonical Branch Selection

### What It Is

Issue 2 is the ordering and fork-choice problem.

The intended design is:

- once QC-backed state exists, it should dominate canonical branch choice
- uncertified siblings should not keep replacing each other based on local noise

### What Was Wrong

Earlier V2 runs showed:

- poor `same_chain_count`
- too many distinct tips
- branch drift after the first QC

The old path was still too heuristic and too local.

### What Was Fixed

The main fixes were:

- separate true orphans from pending certified children
- buffer votes for unknown blocks
- tighten same-height vote discipline
- prune stale losing-branch votes when stronger certified state arrives
- make QC-backed branch choice dominate
- reject stale certified sync responses that would move a node backward

### What Is Fixed Now

Issue 2 is in good shape:

- healthy and degraded default runs converge structurally
- the rigorous matrix passes the baseline and gated structural scenarios
- branch selection is no longer the main live blocker

### What Is Still Not Fully Fixed

There is no major remaining live failure that still points at the old Issue 2 root cause.

So Issue 2 should be treated as **solved enough** for the current V2 stage.

## Issue 3: Service Evidence And Proposer Gating

### What It Is

Issue 3 is the service-evidence and proposer-gating problem.

The intended design is:

- degraded validators lose score
- degraded validators get gated
- honest validators stay above threshold

### What Was Wrong

Earlier V2 service behavior had several failures:

- evidence formation was weak or fragmented
- aggregate import was not converging cleanly
- freshness of score view and freshness of gating did not line up
- transport/session behavior undermined the evidence path

### What Was Fixed

The main fixes were:

- aggregate merge behavior improved
- score freshness and gating freshness were aligned
- deterministic service aggregators were introduced
- observer surface was improved for V2 localnet correctness
- transport/session handling was improved
- weighted recent confirmed evidence was used for gating instead of a single latest aggregate

### What Is Fixed Now

At the default protocol profile, Issue 3 is in a good place:

- default degraded scenarios punish the target validator
- honest validators usually stay above threshold
- default gated scenarios in the rigorous matrix pass

Examples from the current rigorous matrix:

- `gated-drop95`: pass, lowest score `v3 = 0.265`, gating `1`
- `gated-outbound-disabled`: pass, lowest score `v3 = 0.000`, gating `3`
- `policy-window-8`: pass, lowest score `v3 = 0.190`, gating `4`

### What Is Still Not Fully Fixed

Issue 3 is still policy-fragile in the rigorous matrix:

- `policy-threshold-055`: fail
- `policy-window-1`: fail

That means:

- the default V2 policy is behaving acceptably
- but the service/gating model is not yet robust across more aggressive threshold and window variants

So Issue 3 should be treated as **solved at the default profile, but not fully robust**.

## Issue 4: Stale-Restart Recovery

### What It Is

Issue 4 is the stale-restart recovery edge case.

The intended design is:

- if one node goes down and rejoins late
- it should catch up fully
- it should not finish one or more blocks behind the cluster

### What Was Wrong

Earlier stale-restart runs showed:

- restarted node catches up partway
- restarted node reaches the certified frontier
- restarted node often stops short of the final tip before shutdown

This was the last major live recovery gap.

### What Was Fixed

Recent Issue 4 work included:

- startup barrier improvements
- suppression of historical proposer replay after restart
- smarter recovery throttling
- QC-aware responder sync paths
- anchored suffix tail attached to certified recovery
- follow-up sync after successful certified, incremental, and snapshot repair

### What Is Fixed Now

Issue 4 is much better than before:

- stale node no longer collapses badly
- stale node often applies many incremental repairs
- stale node no longer shows the earlier large recovery failures
- the recovery path is far more efficient and much less noisy

### What Is Still Not Fixed

Repeated stale-restart `8` runs still finish one block short.

Fresh examples:

- [node-1/state_snapshot.json](/Users/ayushns01/Desktop/Repositories/Entangrid/var/stale8-certified-tail-fix-1774903000/node-1/state_snapshot.json): height `24`
- [node-8/state_snapshot.json](/Users/ayushns01/Desktop/Repositories/Entangrid/var/stale8-certified-tail-fix-1774903000/node-8/state_snapshot.json): height `23`

- [node-1/state_snapshot.json](/Users/ayushns01/Desktop/Repositories/Entangrid/var/stale8-followup-fix-1774903300/node-1/state_snapshot.json): height `26`
- [node-8/state_snapshot.json](/Users/ayushns01/Desktop/Repositories/Entangrid/var/stale8-followup-fix-1774903300/node-8/state_snapshot.json): height `25`

So Issue 4 is **still open**.

## Bottom Line

- Issue 1 is mostly solved
- Issue 2 is solved enough
- Issue 3 is good at the default profile but still policy-fragile
- Issue 4 is still the main remaining live blocker

## Practical Meaning

The current V2 state is:

- structurally much stronger than before
- good enough on healthy and default degraded behavior to justify continued V2 focus
- not yet at the point where stale-restart recovery can be called fully complete

The next protocol focus should remain on the last-mile stale-restart recovery gap rather than reopening the earlier broad Issue 1, 2, or 3 work.
