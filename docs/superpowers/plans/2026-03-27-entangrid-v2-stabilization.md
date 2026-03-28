# Entangrid V2 Stabilization Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

Historical note:

- this stabilization plan was drafted while `codex/consensus-v2` was still the primary staging branch
- `main` now carries the active V2 implementation line
- branch/worktree references below should be read as execution context from that time, not as the current repository focus

**Goal:** Make `consensus_v2` the active protocol path by restoring `v1`-level degraded-validator punishment at `4/5` validators while materially improving `6/7/8` validator convergence under bursty load.

**Architecture:** Keep `v1` as the benchmark and keep `v2` as the implementation target. The next V2 phase should stop treating service evidence, branch choice, and sync as loosely coupled local heuristics. Instead, service evidence must become enforceable proposer policy from confirmed aggregates, QC state must become the dominant canonical-branch truth, and sync must repair from the highest shared certified point instead of waiting for legacy drift repair.

**Tech Stack:** Rust, Tokio, serde, bincode, `entangrid-types`, `entangrid-consensus`, `entangrid-node`, `entangrid-network`, `entangrid-sim`.

---

## Benchmark Context

Use these current matrix results as the benchmark and regression oracle.

### `v1` benchmark strengths

- `degraded/4`: `same_chain = 4/4`, `target_v3_score = 0.083`, `target_v3_gated = 9`, `honest_min_score = 0.925`
- `degraded/5`: `same_chain = 5/5`, `target_v3_score = 0.167`, `target_v3_gated = 6`, `honest_min_score = 0.883`

### Current `v2` strengths

- `degraded/4`: `same_chain = 4/4`, `target_v3_score = 0.375`, `target_v3_gated = 9`, `honest_min_score = 1.0`
- `degraded/5`: `same_chain = 5/5`, `target_v3_score = 0.0`, `target_v3_gated = 6`, `honest_min_score = 1.0`
- `healthy/6`: honest scores remain high (`honest_min_score = 0.979`) even though convergence is poor

### Current `v2` weaknesses

- `healthy/5`: `same_chain = 1/5`, `height_spread = 12`
- `healthy/6`: `same_chain = 1/6`, `distinct_tips = 6`
- `degraded/6`: `same_chain = 1/6`, `target_v3_gated = 1`
- `degraded/7`: `same_chain = 1/7`, `target_v3_gated = 3`
- `degraded/8`: `same_chain = 1/8`, `target_v3_score = 0.333`, `target_v3_gated = 4`, `honest_min_score = 0.0`

## Acceptance Targets

This plan is complete only when these outcomes are true on the same bursty gated matrix:

- `degraded/4` and `degraded/5` on `v2` must match or beat current `v1` punishment:
  - `same_chain = N/N`
  - `target_v3_gated >= 6`
  - `honest_min_score >= 0.88`
- `healthy/6`, `healthy/7`, and `healthy/8` on `v2` must improve over current `v2`:
  - `same_chain_count` strictly higher than current baseline
  - `distinct_tips <= 2`
  - no healthy validator forced to `0.0`
- `degraded/6`, `degraded/7`, and `degraded/8` on `v2` must show both:
  - real punishment of validator `3`
  - stronger convergence than current `v2`

## File Structure

**Benchmarking and diagnostics**
- Modify: `crates/entangrid-sim/src/lib.rs`
  - Add a first-class branch-comparison matrix helper or equivalent reproducible scenario runner.
  - Surface V2-specific QC/service metrics needed to explain branch selection and gating.

**Service evidence and proposer policy**
- Modify: `crates/entangrid-consensus/src/lib.rs`
  - Tighten aggregate confirmation semantics and scoring helpers.
- Modify: `crates/entangrid-node/src/lib.rs`
  - Make local proposer rejection derive only from confirmed aggregate state.
  - Emit explicit logs and metrics when score is low but gating is skipped.
- Modify: `crates/entangrid-types/src/lib.rs`
  - Add any missing metrics or evidence-state fields needed for enforcement visibility.

**Ordering and sync**
- Modify: `crates/entangrid-node/src/lib.rs`
  - Make QC-backed canonical branch choice dominate local branch heuristics.
  - Implement certified suffix sync from the highest shared QC.
- Modify: `crates/entangrid-types/src/lib.rs`
  - Finalize `CertifiedSyncRequest` and `CertifiedSyncResponse` payload shape.
- Modify: `crates/entangrid-network/src/lib.rs`
  - Prioritize QC/certified sync traffic where needed.

**Docs**
- Modify: `docs/superpowers/plans/entangrid-consensus-v2-status.md`
- Modify: `docs/protocol.md`
- Modify: `docs/architecture.md`

### Task 1: Lock the V1 Benchmark Into the Test Workflow

**Files:**
- Modify: `crates/entangrid-sim/src/lib.rs`
- Test: `crates/entangrid-sim/src/lib.rs`

- [ ] **Step 1: Add failing tests for branch-comparison report logic**

```rust
#[test]
fn comparison_report_marks_v1_degraded_4_as_benchmark_case() {
    let report = sample_branch_comparison_report();
    assert!(report.benchmark_cases.iter().any(|case| case.name == "v1-degraded-4"));
}
```

- [ ] **Step 2: Run simulator tests to verify the comparison helper does not exist yet**

Run: `cargo test -p entangrid-sim`
Expected: FAIL with missing branch-comparison helpers

- [ ] **Step 3: Add a reusable comparison report builder**

```rust
pub struct BranchComparisonCase { /* variant, mode, validators, same_chain_count, ... */ }
pub struct BranchComparisonReport { /* cases, benchmark_cases, summaries */ }
```

- [ ] **Step 4: Encode current `v1 degraded/4` and `v1 degraded/5` cases as benchmark references**

```rust
pub fn v1_benchmark_targets() -> Vec<BenchmarkTarget> { /* exact thresholds from latest matrix */ }
```

- [ ] **Step 5: Re-run simulator tests**

Run: `cargo test -p entangrid-sim`
Expected: PASS

- [ ] **Step 6: Commit**

```bash
git add crates/entangrid-sim/src/lib.rs
git commit -m "add v1 benchmark comparison targets"
```

### Task 2: Make Confirmed Low Scores Always Reach Proposer Gating

**Files:**
- Modify: `crates/entangrid-node/src/lib.rs`
- Modify: `crates/entangrid-consensus/src/lib.rs`
- Modify: `crates/entangrid-types/src/lib.rs`
- Test: `crates/entangrid-node/src/lib.rs`

- [ ] **Step 1: Add failing tests for confirmed-low-score proposer rejection**

```rust
#[test]
fn local_v2_proposer_is_rejected_when_confirmed_aggregate_score_is_below_threshold() {
    let mut node = sample_node_v2();
    node.install_confirmed_aggregate(3, 0.25);
    assert!(node.local_proposal_allowed(3).is_err());
}

#[test]
fn insufficient_evidence_does_not_count_as_gating_rejection() {
    let mut node = sample_node_v2();
    node.install_insufficient_evidence(3);
    assert!(node.local_proposal_allowed(3).is_ok());
    assert_eq!(node.metrics().service_gating_rejections, 0);
}
```

- [ ] **Step 2: Run the targeted node tests and verify failure**

Run: `cargo test -p entangrid-node local_v2_proposer_is_rejected_when_confirmed_aggregate_score_is_below_threshold`
Expected: FAIL

- [ ] **Step 3: Add an explicit enforcement-state helper**

```rust
enum V2GatingState {
    AllowNoEvidence,
    AllowInsufficientEvidence,
    AllowScore(f64),
    RejectScore(f64),
}
```

- [ ] **Step 4: Route local proposal checks through that helper and update metrics/logs**

```rust
fn v2_gating_state(&self, validator_id: ValidatorId, epoch: Epoch) -> V2GatingState { /* ... */ }
```

- [ ] **Step 5: Add metrics/logs for the gap that currently hides failures**

```rust
metrics.service_gating_enforcement_skips += 1;
event!("service-gating-skip", ...);
event!("service-gating-reject", ...);
```

- [ ] **Step 6: Re-run targeted and package tests**

Run: `cargo test -p entangrid-node`
Expected: PASS

- [ ] **Step 7: Commit**

```bash
git add crates/entangrid-node/src/lib.rs crates/entangrid-consensus/src/lib.rs crates/entangrid-types/src/lib.rs
git commit -m "enforce v2 gating from confirmed aggregates"
```

### Task 3: Replace Local Branch Drift With QC-Dominant Canonical Selection

**Files:**
- Modify: `crates/entangrid-node/src/lib.rs`
- Test: `crates/entangrid-node/src/lib.rs`

- [ ] **Step 1: Add failing tests for canonical-branch choice from QC state**

```rust
#[test]
fn branch_with_higher_qc_height_beats_local_uncertified_tip() {
    let mut node = sample_node_v2();
    assert_eq!(node.select_canonical_tip(&branches), certified_branch_tip);
}

#[test]
fn equal_qc_height_keeps_current_branch_until_certified_suffix_is_better() {
    let mut node = sample_node_v2();
    assert_eq!(node.select_canonical_tip(&branches), current_tip);
}
```

- [ ] **Step 2: Run the targeted node tests and verify failure**

Run: `cargo test -p entangrid-node branch_with_higher_qc_height_beats_local_uncertified_tip`
Expected: FAIL

- [ ] **Step 3: Centralize branch scoring in one QC-first helper**

```rust
struct BranchCertState { /* highest_qc_height, highest_qc_hash, suffix_len, tip_hash */ }
fn compare_canonical_branch(a: &BranchCertState, b: &BranchCertState) -> Ordering { /* QC-first */ }
```

- [ ] **Step 4: Remove or demote vote-count-only branch adoption**

```rust
// only allow local drift if it does not replace the current canonical branch
```

- [ ] **Step 5: Re-run node tests**

Run: `cargo test -p entangrid-node`
Expected: PASS

- [ ] **Step 6: Commit**

```bash
git add crates/entangrid-node/src/lib.rs
git commit -m "make qc state decide canonical branch"
```

### Task 4: Implement Certified Sync From the Highest Shared QC

**Files:**
- Modify: `crates/entangrid-types/src/lib.rs`
- Modify: `crates/entangrid-node/src/lib.rs`
- Modify: `crates/entangrid-network/src/lib.rs`
- Test: `crates/entangrid-node/src/lib.rs`

- [ ] **Step 1: Add failing tests for certified suffix repair**

```rust
#[tokio::test]
async fn certified_sync_repairs_diverged_suffix_from_highest_shared_qc() {
    let mut node = sample_node_v2();
    let peer = peer_with_certified_suffix();
    node.sync_from_peer(peer).await.unwrap();
    assert_eq!(node.tip_hash(), peer.tip_hash());
}
```

- [ ] **Step 2: Run the targeted node test and verify failure**

Run: `cargo test -p entangrid-node certified_sync_repairs_diverged_suffix_from_highest_shared_qc`
Expected: FAIL

- [ ] **Step 3: Finalize certified sync message payloads**

```rust
pub enum CertifiedSyncResponse {
    Certified { blocks: Vec<Block>, qcs: Vec<QuorumCertificate>, service_aggregates: Vec<ServiceAggregate> },
    Unavailable,
}
```

- [ ] **Step 4: Implement responder-side highest-shared-QC suffix selection**

```rust
fn certified_suffix_from_shared_qc(&self, peer_highest_qc: Option<BlockHash>) -> Option<CertifiedSuffix> { /* ... */ }
```

- [ ] **Step 5: Implement requester-side suffix import and canonical-head reevaluation**

```rust
async fn handle_certified_sync_response(&mut self, response: CertifiedSyncResponse) -> Result<()> { /* ... */ }
```

- [ ] **Step 6: Re-run node tests**

Run: `cargo test -p entangrid-node`
Expected: PASS

- [ ] **Step 7: Commit**

```bash
git add crates/entangrid-types/src/lib.rs crates/entangrid-node/src/lib.rs crates/entangrid-network/src/lib.rs
git commit -m "add certified qc suffix sync"
```

### Task 5: Make Matrix Scenarios Fail on the Right V2 Regressions

**Files:**
- Modify: `crates/entangrid-sim/src/lib.rs`
- Test: `crates/entangrid-sim/src/lib.rs`

- [ ] **Step 1: Add failing tests for new V2 pass criteria**

```rust
#[test]
fn v2_matrix_requires_degraded_4_and_5_to_match_benchmark_gating() {
    let report = sample_v2_matrix_report();
    assert!(report.case("degraded-4").passes_benchmark_gate());
}
```

- [ ] **Step 2: Encode V2-specific pass rules**

```rust
// degraded 4/5 must match benchmark punishment
// healthy 6/7/8 must improve same_chain_count and distinct_tip_count
```

- [ ] **Step 3: Re-run simulator tests**

Run: `cargo test -p entangrid-sim`
Expected: PASS

- [ ] **Step 4: Commit**

```bash
git add crates/entangrid-sim/src/lib.rs
git commit -m "tighten v2 matrix acceptance rules"
```

### Task 6: Run the Full V1-vs-V2 Comparison and Document the Delta

**Files:**
- Modify: `docs/superpowers/plans/entangrid-consensus-v2-status.md`
- Modify: `docs/protocol.md`
- Modify: `docs/architecture.md`

- [ ] **Step 1: Run the full comparison matrix**

Run:

```bash
/tmp/entangrid_branch_compare.sh /Users/ayushns01/Desktop/Repositories/Entangrid v1 0 test-results/branch-compare-$(date +%s) $(date +%s)
/tmp/entangrid_branch_compare.sh /path/to/v2-worktree v2 1 test-results/branch-compare-$(date +%s) $(date +%s)
```

Expected: summary files for `v1` and `v2` exist and contain all `healthy/degraded 4/5/6/7/8` rows

- [ ] **Step 2: Update the V2 status doc with current benchmark-vs-target status**

```md
- benchmark matched: degraded/4
- benchmark matched: degraded/5
- still failing: healthy/6, degraded/6, healthy/7, degraded/7, healthy/8, degraded/8
```

- [ ] **Step 3: Re-read the plan and status docs for consistency**

Run: `sed -n '1,240p' docs/superpowers/plans/2026-03-27-entangrid-v2-stabilization.md`
Expected: plan reflects current benchmark and next steps

- [ ] **Step 4: Commit**

```bash
git add docs/superpowers/plans/entangrid-consensus-v2-status.md docs/protocol.md docs/architecture.md docs/superpowers/plans/2026-03-27-entangrid-v2-stabilization.md
git commit -m "document v2 stabilization plan"
```

## Execution Notes

- Run all implementation work on a clean `codex/consensus-v2` worktree, not on `main`.
- Do not port current `v3` behavior wholesale into `v2`.
- Only port `v3` ideas after they are shown to improve the `v2` matrix without breaking:
  - degraded/4 benchmark
  - degraded/5 benchmark
- Keep `v1` as the regression oracle until `v2` beats it on the selected benchmark cases.
