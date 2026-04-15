# PQ Stage 1K Service Attestation Hybrid Enforcement Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Extend `require_hybrid_validator_signatures` so service attestations are enforced under the same strict hybrid validator-signature policy as blocks, proposal votes, and relay receipts.

**Architecture:** Keep this Stage 1K slice in the node/runtime layer. Add focused service-attestation hybrid-policy tests first, then wire a narrow attestation policy helper into local emission and imported attestation validation without changing consensus semantics or adding a new flag.

**Tech Stack:** Rust, `tokio`, `entangrid-node`, `entangrid-types`, existing hybrid signing backends, existing Stage 1E and Stage 1K node-policy enforcement patterns.

---

## File Structure

- Modify: `crates/entangrid-node/src/lib.rs`
  - add service-attestation-specific hybrid enforcement tests
  - add an attestation policy helper mirroring the existing block/proposal-vote/receipt enforcement shape
  - enforce hybrid service attestations on local emission and imported validation
- Optionally modify: `README.md`
- Optionally modify: `docs/architecture.md`
- Optionally modify: `docs/protocol.md`
- Optionally modify: `crates/entangrid-crypto/README.md`
- Optionally modify: `crates/entangrid-types/README.md`
  - only if the implementation changes the documented strict hybrid boundary materially

## Task 1: Add Failing Service-Attestation Enforcement Tests

**Files:**
- Modify: `crates/entangrid-node/src/lib.rs`

- [ ] **Step 1: Add a failing strict-mode rejection test for non-hybrid imported service attestations**

Add a focused node test next to the existing service-attestation and hybrid-enforcement tests.

Suggested test:

```rust
#[tokio::test(flavor = "current_thread")]
async fn hybrid_enforcement_rejects_non_hybrid_service_attestation_signature() {
    let genesis = sample_genesis();
    let mut config =
        sample_node_config(1, reserve_local_address(), Vec::new(), "attestation-reject");
    config.feature_flags.consensus_v2 = true;
    config.feature_flags.require_hybrid_validator_signatures = true;
    let mut runner = build_test_runner(1, config, genesis.clone()).await;
    let attestation = signed_service_attestation(
        &DeterministicCryptoBackend::from_genesis(&genesis),
        2,
        1,
        1,
        ServiceCounters::default(),
    );

    let error = runner.import_service_attestation(attestation).unwrap_err();
    assert!(error.to_string().contains("hybrid signature"));
}
```

Adjust the committee member / subject pair to match a valid committee assignment in the existing fixtures.

- [ ] **Step 2: Add a failing permissive-mode acceptance test for non-hybrid imported service attestations**

Suggested test:

```rust
#[tokio::test(flavor = "current_thread")]
async fn hybrid_enforcement_accepts_non_hybrid_service_attestation_when_disabled() {
    let genesis = sample_genesis();
    let mut config =
        sample_node_config(1, reserve_local_address(), Vec::new(), "attestation-permissive");
    config.feature_flags.consensus_v2 = true;
    config.feature_flags.require_hybrid_validator_signatures = false;
    let mut runner = build_test_runner(1, config, genesis.clone()).await;
    let attestation = /* valid deterministic attestation */;

    assert!(runner.import_service_attestation(attestation).unwrap());
}
```

- [ ] **Step 3: Add a failing strict-mode acceptance test for hybrid imported service attestations**

Mirror the existing hybrid proposal-vote / relay-receipt acceptance pattern:

- build or reuse a hybridized genesis
- configure `require_hybrid_validator_signatures = true`
- use a hybrid signing backend for the attestation signer
- verify import succeeds

Suggested test name:

```rust
#[cfg(feature = "pq-ml-dsa")]
#[tokio::test(flavor = "current_thread")]
async fn hybrid_enforcement_accepts_hybrid_service_attestation_signature() { /* ... */ }
```

- [ ] **Step 4: Add a failing strict-mode local-emission success test**

Add a test proving locally emitted service attestations become hybrid under strict mode.

Suggested test name:

```rust
#[cfg(feature = "pq-ml-dsa")]
#[tokio::test(flavor = "current_thread")]
async fn strict_hybrid_mode_emits_hybrid_service_attestations() { /* ... */ }
```

The test should verify the resulting attestation has `signature.scheme() == SignatureScheme::Hybrid`.

- [ ] **Step 5: Add a failing strict-mode local-emission rejection test for non-hybrid attestation emission**

Suggested test name:

```rust
#[tokio::test(flavor = "current_thread")]
async fn hybrid_enforcement_rejects_non_hybrid_local_service_attestation_emission() { /* ... */ }
```

Use the deterministic backend under strict mode and assert the error mentions hybrid signatures.

- [ ] **Step 6: Run the focused service-attestation tests and verify they fail**

Run:

```bash
cargo test -p entangrid-node service_attestation
```

and, if the hybrid acceptance/emission tests are feature-gated:

```bash
cargo test -p entangrid-node --features pq-ml-dsa service_attestation
```

Expected:

- the new service-attestation policy tests fail because Stage 1K attestation enforcement is not wired yet

## Task 2: Implement Service-Attestation Hybrid Policy Enforcement

**Files:**
- Modify: `crates/entangrid-node/src/lib.rs`

- [ ] **Step 1: Add a focused attestation policy helper**

Add a helper near `validate_hybrid_block_policy(...)`, `validate_hybrid_proposal_vote_policy(...)`, and `validate_hybrid_receipt_policy(...)`.

Suggested shape:

```rust
fn validate_hybrid_service_attestation_policy(
    &self,
    attestation: &ServiceAttestation,
) -> Result<()> {
    if !self.config.feature_flags.require_hybrid_validator_signatures {
        return Ok(());
    }
    if attestation.signature.scheme() != entangrid_types::SignatureScheme::Hybrid {
        return Err(anyhow!(
            "hybrid enforcement requires service attestation from committee member {} to use a hybrid signature",
            attestation.committee_member_id
        ));
    }
    Ok(())
}
```

Keep the error wording parallel with the other hybrid enforcement helpers so logs and tests stay consistent.

- [ ] **Step 2: Enforce the helper on imported service-attestation validation**

In `validate_service_attestation(...)`:

- keep the existing committee membership check
- keep the existing signature verification
- add the attestation hybrid-policy gate in the same validation path so strict mode rejects non-hybrid attestations before persistence or rebroadcast

Do not move attestation semantic validation into consensus or create a new flag.

- [ ] **Step 3: Enforce the helper on local service-attestation emission**

In `build_local_service_attestation(...)`:

- keep signing through the existing `service_attestation_signing_hash(...)`
- after signing, run `validate_hybrid_service_attestation_policy(&attestation)?`
- if strict mode is misconfigured and the resulting attestation is non-hybrid, fail before returning the attestation

This keeps local emission aligned with imported-attestation acceptance.

- [ ] **Step 4: Re-run the focused service-attestation tests until green**

Run:

```bash
cargo test -p entangrid-node service_attestation
```

and, if needed:

```bash
cargo test -p entangrid-node --features pq-ml-dsa service_attestation
```

Expected:

- all new service-attestation policy tests pass

## Task 3: Final Verification And Documentation Touch-Up

**Files:**
- Modify: any touched Stage 1K files
- Optionally modify: `README.md`
- Optionally modify: `docs/architecture.md`
- Optionally modify: `docs/protocol.md`
- Optionally modify: `crates/entangrid-crypto/README.md`
- Optionally modify: `crates/entangrid-types/README.md`

- [ ] **Step 1: Update docs only if the user-facing policy story changed materially**

If docs need adjustment, mention that service attestations now join blocks, proposal votes, and relay receipts under strict hybrid validator enforcement.

- [ ] **Step 2: Run the full verification set**

Run:

```bash
cargo fmt
cargo test -p entangrid-node
cargo test -p entangrid-node --features pq-ml-dsa
git diff --check
```

If broader cross-crate verification is needed after touching shared helpers, also run:

```bash
cargo test -p entangrid-types -p entangrid-consensus -p entangrid-ledger -p entangrid-network -p entangrid-node --features pq-ml-dsa
```

- [ ] **Step 3: Commit**

```bash
git add crates/entangrid-node/src/lib.rs README.md docs/architecture.md docs/protocol.md crates/entangrid-crypto/README.md crates/entangrid-types/README.md
git commit -m "enforce hybrid service attestation signatures"
```

## Notes

- Keep this Stage 1K slice strictly limited to service attestations.
- Do not widen the work to service aggregates or transactions in this plan.
- Reuse the existing Stage 1E and Stage 1K helper/fixture style rather than inventing a new enforcement framework.
