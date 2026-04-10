# Current Consensus Issue On `stage-1/pq-integration`

Status date: 2026-04-11

This note explains the specific consensus problem currently blocking final signoff on `stage-1/pq-integration`.

## Short Version

The remaining problem is not PQ signing, PQ transport, stale-node restart recovery, or service-gating policy tuning.

The current blockers are:

- `baseline-6-bursty`: bursty `6`-validator consensus still fails to concentrate votes onto one uncertified branch before a stable QC anchor forms
- `gated-6-bursty`: one follower can still get stranded above the certified head and fail to rejoin the converged suffix

That shows up in exactly two failing matrix scenarios:

- `baseline-6-bursty`
- `gated-6-bursty`

## What The Latest Evidence Shows

Latest rigorous matrix reports:

- baseline slice: [rigorous-matrix-1775849508249.md](/Users/ayushns01/Desktop/Repositories/Entangrid/test-results/rigorous-matrix-1775849508249.md)
- non-baseline slice: [rigorous-matrix-1775847670247.md](/Users/ayushns01/Desktop/Repositories/Entangrid/test-results/rigorous-matrix-1775847670247.md)

Current result:

- overall matrix: `12/14`
- `baseline-6-bursty`: **FAIL**
- `gated-6-bursty`: **FAIL**

Current structural symptoms:

- `baseline-6-bursty`
  - same chain: `3/6`
  - height spread: `0`
  - tip spread: `2`
  - fork observed: `12`
  - sync blocks rejected: `11`
  - all validators reached height `14`, but they split across three tips
- `gated-6-bursty`
  - same chain: `5/6`
  - height spread: `8`
  - tip spread: `1`
  - fork observed: `9`
  - sync blocks rejected: `0`
  - five validators converged to height `10`, while one follower remained stranded at height `2`

This means the branch now has two distinct live failure shapes under bursty `6`-validator load:

- a pre-QC multi-tip convergence failure in `baseline-6-bursty`
- a stuck-follower recovery failure in `gated-6-bursty`

## What The Problem Is Not

The current issue is not:

- missing PQ integration
- broken hybrid signatures
- broken hybrid session establishment
- broken encrypted post-handshake framing
- stale-restart recovery as the primary blocker
- older policy fragility like `policy-threshold-055` or `policy-window-1`

Those areas are in much better shape on the current branch.

## Why This Is Confusing

This is confusing because an older branch state really did show these scenarios passing.

Older matrix snapshot:

- [rigorous-matrix-1774892617700.md](/Users/ayushns01/Desktop/Repositories/Entangrid/test-results/rigorous-matrix-1774892617700.md)

That older snapshot showed:

- `baseline-6-bursty`: **PASS**, same chain `6/6`
- `gated-6-bursty`: **PASS**, same chain `6/6`

At that time, the main open issue was stale-restart recovery.

So the honest interpretation is:

- these `6`-validator bursty cases were previously in a better state
- the current `stage-1/pq-integration` branch no longer holds that earlier proof
- this should be treated as a current regression or, at minimum, an invalidated proof on the present branch state

## Current Technical Diagnosis

The best current explanation is:

- once a stable QC exists, branch choice is much stronger than before
- but before the first QC appears, the runtime can still fragment across competing uncertified branches
- under bursty `6`-validator load, proposal votes are still not concentrating quickly enough on one branch in `baseline-6-bursty`
- after most peers converge, one lagging node can still buffer descendants, reject orphans, and fail to trigger certified/full sync takeover in `gated-6-bursty`

In short:

- post-QC behavior is substantially better
- pre-QC convergence is still not strong enough in the hardest baseline bursty case
- follower recovery above a certified head is still not strong enough in the hardest gated bursty case

## What Has Already Been Tried

Recent hardening already added:

- stronger pre-QC branch preference using accumulated vote support
- tighter rejection of non-extending local proposal votes on incompatible uncertified branches
- refusal to adopt taller but weaker full snapshots before QC
- branch-relation tracking for same-QC suffix comparisons
- orphaned-vote replacement so later extending votes can supersede weaker buffered vote state
- certified-sync escalation after same-frontier suffix repair fails
- richer sync diagnostics for throttling and rejection paths

Those changes improved the shape of the problem, but they did not eliminate it.

## Why This Blocks Merge To `main`

This issue blocks final merge signoff because it is a core consensus failure mode, not a cosmetic or policy-only failure.

If these two scenarios remain red, the branch cannot honestly be described as:

- fully consensus-stable
- fully proven under the current localnet acceptance matrix
- ready for final merge into canonical `main`

## What Must Be True To Call This Fixed

This issue should be treated as fixed only when:

- `baseline-6-bursty` passes
- `gated-6-bursty` passes
- the rigorous matrix reaches `14/14`
- the result is stable enough to freeze as the simulator acceptance gate

## Bottom Line

The current branch has real and substantial progress:

- PQ Stage 1 integration is real
- certified sync is live
- service evidence and gating are materially stronger
- stale-restart recovery is no longer the main blocker

But the branch still has two serious unresolved consensus problems:

- bursty `6`-validator pre-QC convergence is not yet reliable in `baseline-6-bursty`
- stuck-follower recovery above the certified head is not yet reliable in `gated-6-bursty`

That is the consensus issue we are currently facing.
