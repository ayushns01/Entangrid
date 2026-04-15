# PQ Stage 1K Relay Receipt Hybrid Enforcement Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Extend `require_hybrid_validator_signatures` so relay receipts are enforced under the same strict hybrid validator-signature policy as blocks and proposal votes.

**Architecture:** Keep Stage 1K in the node/runtime layer. Add receipt-focused hybrid-policy tests first, then wire a narrow receipt policy helper into local emission and imported receipt validation without changing consensus semantics or adding a new flag.

**Tech Stack:** Rust, `tokio`, `entangrid-node`, `entangrid-types`, existing hybrid signing backends, existing Stage 1E node-policy enforcement pattern.

---

## File Structure

- Modify: `crates/entangrid-node/src/lib.rs`
  - add receipt-specific hybrid enforcement tests
  - add a receipt policy helper mirroring the existing block/proposal-vote enforcement shape
  - enforce hybrid receipts on local emission and imported receipt validation
- Optionally modify: `README.md`
- Optionally modify: `docs/architecture.md`
- Optionally modify: `docs/protocol.md`
- Optionally modify: `crates/entangrid-crypto/README.md`
- Optionally modify: `crates/entangrid-types/README.md`
  - only if the implementation meaningfully changes the documented Stage 1K boundary

## Task 1: Add Failing Receipt Enforcement Tests

**Files:**
- Modify: `crates/entangrid-node/src/lib.rs`

- [ ] **Step 1: Add a failing strict-mode rejection test for non-hybrid imported receipts**

Add a focused node test next to the existing hybrid block/proposal-vote tests.

Suggested test:

```rust
#[tokio::test(flavor = "current_thread")]
async fn hybrid_enforcement_rejects_non_hybrid_relay_receipt_signature() {
    let genesis = sample_genesis();
    let mut config =
        sample_node_config(1, reserve_local_address(), Vec::new(), "receipt-reject");
    config.feature_flags.require_hybrid_validator_signatures = true;
    let mut runner = build_test_runner(1, config, genesis.clone()).await;
    let receipt = sample_receipt();

    let error = runner.import_protocol_message(2, ProtocolMessage::RelayReceipt(receipt)).unwrap_err();
    assert!(error.to_string().contains("witness 2"));
}
```

Use the existing receipt helpers if possible instead of inventing a parallel fixture path.

- [ ] **Step 2: Add a failing permissive-mode acceptance test for non-hybrid imported receipts**

Suggested test:

```rust
#[tokio::test(flavor = "current_thread")]
async fn hybrid_enforcement_accepts_non_hybrid_relay_receipt_when_disabled() {
    let genesis = sample_genesis();
    let mut config =
        sample_node_config(1, reserve_local_address(), Vec::new(), "receipt-permissive");
    config.feature_flags.require_hybrid_validator_signatures = false;
    let mut runner = build_test_runner(1, config, genesis.clone()).await;
    let receipt = sample_receipt();

    assert!(runner.import_protocol_message(2, ProtocolMessage::RelayReceipt(receipt)).unwrap());
}
```

Adjust to the actual test harness entrypoint if `import_protocol_message(...)` is not the right helper in this file.

- [ ] **Step 3: Add a failing strict-mode acceptance test for hybrid imported receipts**

Mirror the existing hybrid proposal-vote acceptance pattern:

- build or reuse a hybridized genesis
- configure `require_hybrid_validator_signatures = true`
- use a hybrid signing backend for the receipt signer
- verify import succeeds

Suggested test name:

```rust
#[cfg(feature = "pq-ml-dsa")]
#[tokio::test(flavor = "current_thread")]
async fn hybrid_enforcement_accepts_hybrid_relay_receipt_signature() { /* ... */ }
```

- [ ] **Step 4: Add a failing local-emission test for strict-mode receipts**

Add a test proving locally emitted receipts become hybrid under strict mode.

Suggested test name:

```rust
#[cfg(feature = "pq-ml-dsa")]
#[tokio::test(flavor = "current_thread")]
async fn strict_hybrid_mode_emits_hybrid_relay_receipts() { /* ... */ }
```

The test should verify the resulting stored or broadcast receipt has `signature.scheme() == SignatureScheme::Hybrid`.

- [ ] **Step 5: Run the focused receipt tests and verify they fail**

Run:

```bash
cargo test -p entangrid-node relay_receipt
```

and, if the hybrid acceptance/emission tests are feature-gated:

```bash
cargo test -p entangrid-node --features pq-ml-dsa relay_receipt
```

Expected:

- the new receipt-policy tests fail because Stage 1K enforcement is not wired yet

## Task 2: Implement Receipt Hybrid Policy Enforcement

**Files:**
- Modify: `crates/entangrid-node/src/lib.rs`

- [ ] **Step 1: Add a focused receipt policy helper**

Add a helper near `validate_hybrid_block_policy(...)` and `validate_hybrid_proposal_vote_policy(...)`.

Suggested shape:

```rust
fn validate_hybrid_receipt_policy(&self, receipt: &RelayReceipt) -> Result<()> {
    if !self.config.feature_flags.require_hybrid_validator_signatures {
        return Ok(());
    }
    if receipt.signature.scheme() != entangrid_types::SignatureScheme::Hybrid {
        return Err(anyhow!(
            "hybrid enforcement requires relay receipt from witness {} to use a hybrid signature",
            receipt.witness_validator_id
        ));
    }
    Ok(())
}
```

Keep the error wording parallel with block/proposal-vote enforcement so tests and logs stay consistent.

- [ ] **Step 2: Enforce the helper on imported receipt validation**

In `validate_receipt(...)`:

- keep the existing duplicate check
- keep the existing signature verification
- keep the existing consensus assignment validation
- add the receipt hybrid-policy gate in the same validation path so strict mode rejects non-hybrid receipts before persistence/rebroadcast

Do not move receipt semantic validation into consensus or create a new flag.

- [ ] **Step 3: Enforce the helper on local receipt emission**

In the local receipt creation path:

- keep signing through the existing `receipt_signing_hash(...)`
- after signing, run `validate_hybrid_receipt_policy(&receipt)?`
- if strict mode is misconfigured and the resulting receipt is non-hybrid, fail before storing or broadcasting

This keeps local emission aligned with imported-receipt acceptance.

- [ ] **Step 4: Re-run the focused receipt tests until green**

Run:

```bash
cargo test -p entangrid-node relay_receipt
```

and, if needed:

```bash
cargo test -p entangrid-node --features pq-ml-dsa relay_receipt
```

Expected:

- all new receipt-policy tests pass

## Task 3: Final Verification And Documentation Touch-Up

**Files:**
- Modify: any touched Stage 1K files
- Optionally modify: `README.md`
- Optionally modify: `docs/architecture.md`
- Optionally modify: `docs/protocol.md`
- Optionally modify: `crates/entangrid-crypto/README.md`
- Optionally modify: `crates/entangrid-types/README.md`

- [ ] **Step 1: Update docs only if the user-facing policy story changed materially**

If docs need adjustment, mention that relay receipts now join blocks and proposal votes under strict hybrid validator enforcement.

- [ ] **Step 2: Run the full verification set**

Run:

```bash
cargo test -p entangrid-node
cargo test -p entangrid-node --features pq-ml-dsa
cargo fmt
git diff --check
```

If broader cross-crate verification is needed after touching shared helpers, also run:

```bash
cargo test -p entangrid-types -p entangrid-consensus -p entangrid-ledger -p entangrid-network -p entangrid-node --features pq-ml-dsa
```

- [ ] **Step 3: Commit**

```bash
git add crates/entangrid-node/src/lib.rs README.md docs/architecture.md docs/protocol.md crates/entangrid-crypto/README.md crates/entangrid-types/README.md
git commit -m "enforce hybrid relay receipt signatures"
```

## Notes

- Keep Stage 1K strictly limited to relay receipts.
- Do not widen the work to service attestations or aggregates in this plan.
- Reuse the existing Stage 1E helper/fixture style rather than inventing a new enforcement framework.
