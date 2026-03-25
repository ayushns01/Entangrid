# Entangrid Consensus V2 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace local-receipt-driven proposer gating with committee-attested service evidence and certificate-backed ordering that stays correct across changing validator counts.

**Architecture:** Keep the current ledger, simulator, and validator process model, but split the protocol into three explicit planes: ordering, service evidence, and sync. Ordering moves from ad hoc fork repair to quorum certificates on proposals; service gating moves from local receipt views to prior-epoch committee aggregates; sync moves certified blocks and service aggregates first, with chunked snapshot fallback only when necessary.

**Tech Stack:** Rust, Tokio, serde, bincode, existing `entangrid-types`, `entangrid-consensus`, `entangrid-node`, `entangrid-network`, and `entangrid-sim` crates.

---

## File Structure

**Core protocol types**
- Modify: `crates/entangrid-types/src/lib.rs`
  - Add quorum-certificate, vote, service-attestation, service-aggregate, certified-header, and chunked-sync types.
  - Keep legacy receipt types during migration, but mark them as compatibility-only.

**Consensus rules**
- Modify: `crates/entangrid-consensus/src/lib.rs`
  - Add validator-count-aware service committee assignment.
  - Add QC validation helpers and deterministic fork-choice helpers.
  - Add score calculation from service aggregates instead of raw local receipts.

**Node runtime**
- Modify: `crates/entangrid-node/src/lib.rs`
  - Add vote handling, QC assembly, certified block import, service attestation production, service aggregate assembly/import, and chunk-aware sync.
  - Gate local proposal only from finalized prior-epoch service aggregates.

**Network transport**
- Modify: `crates/entangrid-network/src/lib.rs`
  - Add transport support and prioritization for ordering, service, and sync messages.

**Simulator and verification**
- Modify: `crates/entangrid-sim/src/lib.rs`
  - Add V2 init flags, QC/service-aggregate reporting, and validator-count sweep expectations.
  - Extend simulator reports from node-emitted metrics, not simulator-only state.

**Docs**
- Modify: `README.md`
- Modify: `docs/protocol.md`
- Modify: `docs/architecture.md`
- Modify: `crates/entangrid-types/README.md`
- Modify: `crates/entangrid-consensus/README.md`
- Modify: `crates/entangrid-node/README.md`
- Modify: `crates/entangrid-network/README.md`
- Modify: `crates/entangrid-sim/README.md`

## Migration Principles

- Keep the current chain format bootable during development behind a feature flag or config switch named `consensus_v2`.
- Do not remove legacy receipts until V2 matrix passes at `4/6/8`.
- Do not start PQ signatures until V2 ordering + service evidence are structurally green.
- No tuning that only helps one validator count is acceptable unless the plan explicitly defines the scaling formula.
- Validator-count-aware rules must be derived by code, not operator knobs:
  - service committee size: deterministic formula in `entangrid-consensus`
  - QC threshold: deterministic supermajority formula from validator count
  - simulator and matrix may report these derived values but must not accept free-form overrides

### Task 1: Introduce V2 Protocol Types

**Files:**
- Modify: `crates/entangrid-types/src/lib.rs`
- Test: `crates/entangrid-types/src/lib.rs`

- [ ] **Step 1: Add the failing unit tests for new protocol objects**

```rust
#[test]
fn qc_hash_round_trips() {
    let qc = sample_quorum_certificate();
    assert_eq!(qc.block_hash, signing_hash_target(&qc));
}

#[test]
fn service_aggregate_requires_matching_epoch_and_target() {
    let aggregate = sample_service_aggregate();
    assert!(aggregate.is_well_formed());
}
```

- [ ] **Step 2: Run tests to verify the new objects do not exist yet**

Run: `cargo test -p entangrid-types`
Expected: FAIL with missing V2 types/helpers

- [ ] **Step 3: Add minimal V2 types**

```rust
pub struct ProposalVote { /* validator_id, epoch, slot, block_hash, signature */ }
pub struct QuorumCertificate { /* block_hash, height, round, vote_root, votes */ }
pub struct ServiceAttestation { /* subject, committee_member, epoch, counters, signature */ }
pub struct ServiceAggregate { /* subject, epoch, attestation_root, attestations, aggregate_counters */ }
pub struct CertifiedBlockHeader { /* header + block_hash + inline QC + optional prior aggregate */ }
pub struct ChunkedSyncRequest { /* from height, want_certified_only */ }
pub struct ChunkedSyncResponse { /* certified headers, blocks, service aggregates carried inline */ }
```

- [ ] **Step 4: Add serde/default/backward-compat support and shared V2 config wiring**

```rust
#[serde(default)]
pub consensus_v2: bool;
```

- [ ] **Step 5: Re-run type tests**

Run: `cargo test -p entangrid-types`
Expected: PASS

- [ ] **Step 6: Commit**

```bash
git add crates/entangrid-types/src/lib.rs
git commit -m "add consensus v2 types"
```

### Task 2: Add Committee-Attested Service Evidence Rules

**Files:**
- Modify: `crates/entangrid-consensus/src/lib.rs`
- Modify: `crates/entangrid-types/src/lib.rs`
- Test: `crates/entangrid-consensus/src/lib.rs`

- [ ] **Step 1: Add failing tests for committee assignment and aggregate scoring**

```rust
#[test]
fn service_committee_scales_with_validator_count() {
    assert_eq!(derived_service_committee_size(4), expected_size_4);
    assert_eq!(derived_service_committee_size(8), expected_size_8);
}

#[test]
fn aggregate_score_uses_attested_obligations_not_raw_receipt_count() {
    let score = aggregate_service_score(&sample_attestations(), &sample_weights());
    assert!(score >= 0.0 && score <= 1.0);
}
```

- [ ] **Step 2: Run consensus tests and verify failure**

Run: `cargo test -p entangrid-consensus`
Expected: FAIL with missing committee/aggregate helpers

- [ ] **Step 3: Add service committee derivation**

```rust
pub fn derive_service_committee(
    validators: &[ValidatorConfig],
    subject: ValidatorId,
    epoch: Epoch,
) -> Vec<ValidatorId> { /* deterministic, validator-count-aware */ }
```

- [ ] **Step 4: Add aggregate validation and scoring**

```rust
pub fn validate_service_aggregate(
    aggregate: &ServiceAggregate,
    validators: &[ValidatorConfig],
) -> Result<(), String> { /* membership, signatures, epoch, subject, attestation_root, threshold, counters */ }

pub fn compute_service_score_from_aggregate(
    aggregate: &ServiceAggregate,
    weights: &ServiceScoreWeights,
) -> f64 { /* attested obligations fulfilled / obligations assigned */ }
```

- [ ] **Step 5: Add helper for prior-finalized aggregate eligibility without switching node gating yet**

```rust
pub fn proposer_is_service_eligible(
    aggregate: Option<&ServiceAggregate>,
    threshold: f64,
    current_epoch: Epoch,
) -> bool { /* aggregate.epoch < current_epoch */ }
```

- [ ] **Step 6: Re-run consensus tests**

Run: `cargo test -p entangrid-consensus`
Expected: PASS

- [ ] **Step 7: Commit**

```bash
git add crates/entangrid-consensus/src/lib.rs crates/entangrid-types/src/lib.rs
git commit -m "add service committee scoring"
```

### Task 3: Add QC-Backed Ordering and Fork Choice

**Files:**
- Modify: `crates/entangrid-node/src/lib.rs`
- Modify: `crates/entangrid-consensus/src/lib.rs`
- Modify: `crates/entangrid-types/src/lib.rs`
- Test: `crates/entangrid-node/src/lib.rs`

- [ ] **Step 1: Add failing tests for QC assembly and certified fork choice**

```rust
#[tokio::test]
async fn competing_branches_choose_highest_qc() {
    let mut node = sample_node_v2();
    let chain = competing_branches_with_qcs();
    assert_eq!(node.choose_fork(&chain), chain.branch_b_tip());
}

#[tokio::test]
async fn peer_block_without_qc_is_not_preferred_over_certified_tip() {
    let mut node = sample_node_v2();
    assert!(node.import_peer_block(unqc_block()).await.is_ok());
    assert_eq!(node.tip_hash(), certified_tip_hash());
}
```

- [ ] **Step 2: Run node tests and verify failure**

Run: `cargo test -p entangrid-node`
Expected: FAIL with missing QC path

- [ ] **Step 3: Add proposal vote handling**

```rust
async fn handle_proposal_vote(&mut self, vote: ProposalVote) -> Result<()> { /* verify and store */ }
fn maybe_build_qc(&mut self, block_hash: HashBytes) -> Option<QuorumCertificate> { /* threshold */ }
```

- [ ] **Step 4: Add certified fork choice**

```rust
fn compare_branches(&self, a: &BranchTip, b: &BranchTip) -> Ordering {
    /* highest QC height, then deterministic tie-break */
}
```

- [ ] **Step 5: Add short-range reorg support**

```rust
async fn adopt_certified_branch_suffix(&mut self, ancestor: HashBytes, suffix: &[Block]) -> Result<()> {
    /* rewind, replay, persist */
}
```

- [ ] **Step 6: Re-run node tests**

Run: `cargo test -p entangrid-node`
Expected: PASS

- [ ] **Step 7: Commit**

```bash
git add crates/entangrid-node/src/lib.rs crates/entangrid-consensus/src/lib.rs crates/entangrid-types/src/lib.rs
git commit -m "add qc ordering"
```

### Task 4: Add Service Attestation Production and Aggregate Import

**Files:**
- Modify: `crates/entangrid-node/src/lib.rs`
- Modify: `crates/entangrid-types/src/lib.rs`
- Test: `crates/entangrid-node/src/lib.rs`

- [ ] **Step 1: Add failing tests for attestation production/import**

```rust
#[tokio::test]
async fn committee_member_emits_attestation_for_subject() {
    let mut node = sample_node_v2();
    node.on_epoch_close().await.unwrap();
    assert!(node.pending_attestations_for(subject(3)).len() > 0);
}

#[tokio::test]
async fn aggregate_import_updates_local_service_view() {
    let mut node = sample_node_v2();
    node.import_service_aggregate(sample_service_aggregate()).unwrap();
    assert_eq!(node.service_score_for(validator(3)), 0.75);
}
```

- [ ] **Step 2: Run node tests and verify failure**

Run: `cargo test -p entangrid-node committee_member_emits_attestation_for_subject -- --nocapture`
Expected: FAIL

- [ ] **Step 3: Add attestation emission for committee members**

```rust
async fn emit_service_attestations(&mut self, closing_epoch: Epoch) -> Result<()> {
    /* derive committee, sign subject evidence, broadcast service message */
}
```

- [ ] **Step 4: Add aggregate assembly/import**

```rust
fn maybe_build_service_aggregate(&mut self, subject: ValidatorId, epoch: Epoch) -> Option<ServiceAggregate> {
    /* collect committee attestations, validate threshold */
}

fn import_service_aggregate(&mut self, aggregate: ServiceAggregate) -> Result<()> {
    /* validate and store finalized service evidence */
}
```

- [ ] **Step 5: Switch local proposer gating to prior-epoch finalized aggregates**

```rust
fn local_proposer_may_build(&self, epoch: Epoch) -> bool {
    self.prior_epoch_aggregate_score() >= self.service_gating_threshold()
}
```

- [ ] **Step 6: Re-run node tests**

Run: `cargo test -p entangrid-node`
Expected: PASS

- [ ] **Step 7: Commit**

```bash
git add crates/entangrid-node/src/lib.rs crates/entangrid-types/src/lib.rs
git commit -m "add service aggregates"
```

### Task 5: Rework Network Planes and Sync

**Files:**
- Modify: `crates/entangrid-network/src/lib.rs`
- Modify: `crates/entangrid-node/src/lib.rs`
- Modify: `crates/entangrid-types/src/lib.rs`
- Test: `crates/entangrid-network/src/lib.rs`
- Test: `crates/entangrid-node/src/lib.rs`

- [ ] **Step 1: Add failing tests for message-plane routing and certified sync**

```rust
#[test]
fn service_aggregate_uses_service_plane() {
    assert_eq!(message_plane(&ProtocolMessage::ServiceAggregate(sample())), MessagePlane::Service);
}

#[tokio::test]
async fn chunked_sync_prefers_certified_blocks_before_snapshot() {
    let mut node = sample_node_v2();
    assert!(node.try_chunk_sync(peer()).await.is_ok());
}
```

- [ ] **Step 2: Run focused tests and verify failure**

Run: `cargo test -p entangrid-network -p entangrid-node`
Expected: FAIL with missing plane/chunk logic

- [ ] **Step 3: Add explicit ordering, service, and sync planes**

```rust
pub enum MessagePlane { Ordering, Service, Sync }
```

- [ ] **Step 4: Add certified chunk sync**

```rust
async fn request_certified_suffix(&mut self, peer: ValidatorId, from_height: u64) -> Result<()> { /* chunks */ }
async fn apply_certified_sync_response(&mut self, response: ChunkedSyncResponse) -> Result<()> {
    /* verify inline QC payloads, aggregate payloads, and replace/dual-run current snapshot validators under consensus_v2 */
}
```

- [ ] **Step 5: Keep snapshot fallback only as last resort**

```rust
async fn request_snapshot_fallback_if_chunk_sync_fails(&mut self, peer: ValidatorId) -> Result<()> { /* only after certified sync misses */ }
```

- [ ] **Step 6: Re-run focused tests**

Run: `cargo test -p entangrid-network -p entangrid-node`
Expected: PASS

- [ ] **Step 7: Commit**

```bash
git add crates/entangrid-network/src/lib.rs crates/entangrid-node/src/lib.rs crates/entangrid-types/src/lib.rs
git commit -m "separate protocol planes"
```

### Task 6: Add Simulator Support and Matrix Gates

**Files:**
- Modify: `crates/entangrid-sim/src/lib.rs`
- Modify: `crates/entangrid-node/README.md`
- Modify: `crates/entangrid-sim/README.md`
- Test: `crates/entangrid-sim/src/lib.rs`

- [ ] **Step 1: Add failing simulator tests for V2 defaults**

```rust
#[test]
fn v2_matrix_includes_validator_count_sweep() {
    let scenarios = rigorous_matrix_scenarios_v2();
    assert!(scenarios.iter().any(|s| s.validators == 8 && s.enable_service_gating));
}

#[test]
fn v2_report_includes_qc_and_service_aggregate_counts() {
    let report = sample_matrix_report_v2();
    assert!(report.total_qcs > 0);
}
```

- [ ] **Step 2: Run simulator tests and verify failure**

Run: `cargo test -p entangrid-sim`
Expected: FAIL with missing V2 scenario/report fields

- [ ] **Step 3: Add V2 init flags and reporting**

```rust
--consensus-v2
// report derived committee size and qc threshold, do not accept free-form overrides
```

- [ ] **Step 4: Extend matrix expectations**

```rust
assert_eq!(scenario.same_chain, scenario.validators);
assert_eq!(scenario.tip_spread, 0);
assert_eq!(scenario.honest_below_threshold, 0);
```

- [ ] **Step 5: Add 4/6/8 gated and degraded scenarios as required pass gates**

```rust
baseline-4-v2
gated-4-v2
degraded-4-v2
baseline-6-v2
gated-6-v2
degraded-6-v2
baseline-8-v2
gated-8-v2
degraded-8-v2
```

- [ ] **Step 6: Re-run simulator tests**

Run: `cargo test -p entangrid-sim`
Expected: PASS

- [ ] **Step 7: Commit**

```bash
git add crates/entangrid-sim/src/lib.rs crates/entangrid-node/README.md crates/entangrid-sim/README.md
git commit -m "add v2 matrix coverage"
```

### Task 7: Update Protocol Docs and Migration Notes

**Files:**
- Modify: `README.md`
- Modify: `docs/protocol.md`
- Modify: `docs/architecture.md`
- Modify: `crates/entangrid-types/README.md`
- Modify: `crates/entangrid-consensus/README.md`
- Modify: `crates/entangrid-node/README.md`
- Modify: `crates/entangrid-network/README.md`
- Modify: `crates/entangrid-sim/README.md`

- [ ] **Step 1: Document the new protocol flow**

```text
proposal -> votes -> QC -> certified fork choice
service observations -> committee attestations -> service aggregate -> next-epoch gating
certified suffix sync -> snapshot fallback
```

- [ ] **Step 2: Add migration notes**

```text
Legacy receipt path remains compatibility-only until V2 matrix passes.
PQ signatures are deferred until V2 certified ordering is stable.
```

- [ ] **Step 3: Verify docs are internally consistent**

Run: `rg -n "local receipt|service aggregate|quorum certificate|consensus v2" README.md docs crates/*/README.md`
Expected: updated references only

- [ ] **Step 4: Commit**

```bash
git add README.md docs/protocol.md docs/architecture.md crates/entangrid-types/README.md crates/entangrid-consensus/README.md crates/entangrid-node/README.md crates/entangrid-network/README.md crates/entangrid-sim/README.md
git commit -m "document consensus v2"
```

### Task 8: Full Verification Gate Before PQ

**Files:**
- Modify: `test-results/test-review-3.md`
- Test: `crates/entangrid-types/src/lib.rs`
- Test: `crates/entangrid-consensus/src/lib.rs`
- Test: `crates/entangrid-network/src/lib.rs`
- Test: `crates/entangrid-node/src/lib.rs`
- Test: `crates/entangrid-sim/src/lib.rs`

- [ ] **Step 1: Run the full crate test suite**

Run: `cargo test -p entangrid-types -p entangrid-consensus -p entangrid-network -p entangrid-node -p entangrid-sim`
Expected: PASS

- [ ] **Step 2: Run the V2 matrix**

Run: `cargo run -p entangrid-sim -- matrix --base-dir var/localnet-rigorous-v2 --output-dir test-results --settle-secs 18 --consensus-v2`
Expected: all V2 `4/6/8` scenarios PASS with `tip_spread = 0`

- [ ] **Step 3: Write the verification review**

```markdown
# Test Review 3
- V2 matrix status
- 4/6/8 validator comparison
- degraded validator behavior
- honest validator fallout
- recommendation: PQ ready / not ready
```

- [ ] **Step 4: Commit**

```bash
git add test-results/test-review-3.md test-results
git commit -m "verify consensus v2"
```

## Exit Criteria

- `consensus_v2` is available behind config and can run end-to-end.
- Proposer eligibility depends on **prior-finalized service aggregates**, not raw local receipt views.
- Normal fork choice depends on **quorum certificates**, not ad hoc snapshot preference.
- Sync prefers certified suffix chunks and only uses full snapshot fallback as a last resort.
- The V2 matrix passes at `4`, `6`, and `8` validators in gated Entangrid mode.
- Honest validators are not broadly gated in the `8` validator case.
- Only after all of the above is it acceptable to start PQ signatures.
