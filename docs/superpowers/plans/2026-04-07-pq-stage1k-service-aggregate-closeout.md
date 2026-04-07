# PQ Stage 1K Service Aggregate Hybrid Enforcement Closeout Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Close Stage 1K by proving service aggregates inherit strict hybrid validator-signature enforcement transitively through their embedded service attestations.

**Architecture:** Keep aggregate validation where it already lives in `entangrid-node`. Add focused aggregate tests first, prove strict-mode aggregate rejection/acceptance through the existing attestation validator, and avoid adding redundant aggregate-specific hybrid policy code.

**Tech Stack:** Rust, `tokio`, `entangrid-node`, existing hybrid signing backends, existing Stage 1K service-attestation enforcement path.

---

## File Structure

- Modify: `crates/entangrid-node/src/lib.rs`
  - add aggregate-closeout tests proving transitive hybrid enforcement
  - keep existing aggregate validation code unchanged unless tests reveal a real gap
- Optionally modify: `README.md`
- Optionally modify: `docs/architecture.md`
- Optionally modify: `docs/protocol.md`
- Optionally modify: `crates/entangrid-crypto/README.md`
- Optionally modify: `crates/entangrid-types/README.md`
  - only if the implementation changes the documented Stage 1K completion story materially

## Task 1: Add Aggregate-Closeout Tests

**Files:**
- Modify: `crates/entangrid-node/src/lib.rs`

- [ ] **Step 1: Add a strict-mode rejection test for aggregates with non-hybrid embedded attestations**

Suggested test:

```rust
#[tokio::test(flavor = "current_thread")]
async fn hybrid_enforcement_rejects_service_aggregate_with_non_hybrid_attestation() {
    let genesis = sample_genesis();
    let mut config =
        sample_node_config(1, reserve_local_address(), Vec::new(), "aggregate-strict-reject");
    config.feature_flags.consensus_v2 = true;
    config.feature_flags.require_hybrid_validator_signatures = true;
    let mut runner = build_test_runner(1, config, genesis.clone()).await;
    let aggregate = service_aggregate_for_subject(
        &DeterministicCryptoBackend::from_genesis(&genesis),
        &runner.consensus,
        1,
        1,
        ServiceCounters::default(),
    );

    let error = runner.import_service_aggregate(aggregate).unwrap_err();
    assert!(error.to_string().contains("hybrid signature"));
}
```

- [ ] **Step 2: Add a permissive-mode acceptance test for deterministic aggregates**

Suggested test:

```rust
#[tokio::test(flavor = "current_thread")]
async fn hybrid_enforcement_accepts_service_aggregate_when_disabled() {
    let genesis = sample_genesis();
    let mut config =
        sample_node_config(1, reserve_local_address(), Vec::new(), "aggregate-permissive");
    config.feature_flags.consensus_v2 = true;
    config.feature_flags.require_hybrid_validator_signatures = false;
    let mut runner = build_test_runner(1, config, genesis.clone()).await;
    let aggregate = /* deterministic aggregate */;

    assert!(runner.import_service_aggregate(aggregate.clone()).unwrap());
    assert_eq!(runner.service_aggregates.get(&(aggregate.subject_validator_id, aggregate.epoch)).unwrap(), &aggregate);
}
```

- [ ] **Step 3: Add a strict-mode acceptance test for aggregates with hybrid embedded attestations**

Mirror the service-attestation hybrid test pattern:

- build a genesis where the required committee member identities have real hybrid keys
- build matching hybrid signers
- create a hybrid aggregate from hybrid attestations
- verify strict import succeeds

Suggested test name:

```rust
#[cfg(feature = "pq-ml-dsa")]
#[tokio::test(flavor = "current_thread")]
async fn hybrid_enforcement_accepts_service_aggregate_with_hybrid_attestations() { /* ... */ }
```

- [ ] **Step 4: Add a strict-mode local-build test proving aggregate attestations are hybrid**

Suggested test name:

```rust
#[cfg(feature = "pq-ml-dsa")]
#[tokio::test(flavor = "current_thread")]
async fn strict_hybrid_mode_builds_service_aggregate_from_hybrid_attestations() { /* ... */ }
```

Populate strict-mode local state with hybrid attestations first, then call `build_service_aggregate(...)` and verify every embedded attestation has `signature.scheme() == SignatureScheme::Hybrid`.

- [ ] **Step 5: Run the focused aggregate tests**

Run:

```bash
cargo test -p entangrid-node service_aggregate
```

and, for the hybrid-positive tests:

```bash
cargo test -p entangrid-node --features pq-ml-dsa service_aggregate
```

Expected:

- the new aggregate tests either fail for a real gap or prove the existing transitive enforcement behavior directly

## Task 2: Minimal Implementation Or Cleanup

**Files:**
- Modify: `crates/entangrid-node/src/lib.rs`

- [ ] **Step 1: Only change production code if the new tests reveal a real gap**

Preferred outcome:

- no new aggregate-specific hybrid helper
- no new aggregate signature field
- reuse the existing `validate_service_aggregate(...) -> validate_service_attestation(...)` flow

If a test reveals a real gap, make the smallest possible fix in the node validation path and keep the aggregate model unsigned.

- [ ] **Step 2: Re-run the focused aggregate tests until green**

Run:

```bash
cargo test -p entangrid-node service_aggregate
```

and, if needed:

```bash
cargo test -p entangrid-node --features pq-ml-dsa service_aggregate
```

Expected:

- all aggregate-closeout tests pass

## Task 3: Final Stage 1K Closeout

**Files:**
- Modify: any touched Stage 1K files
- Optionally modify: `README.md`
- Optionally modify: `docs/architecture.md`
- Optionally modify: `docs/protocol.md`
- Optionally modify: `crates/entangrid-crypto/README.md`
- Optionally modify: `crates/entangrid-types/README.md`

- [ ] **Step 1: Update docs to mark aggregate enforcement as transitive and Stage 1K as complete**

If docs need adjustment, explain that:

- service aggregates are not signed directly
- they inherit strict hybrid enforcement through validated service attestations
- Stage 1K is complete after this closeout slice

- [ ] **Step 2: Run the full verification set**

Run:

```bash
cargo fmt
cargo test -p entangrid-node
cargo test -p entangrid-node --features pq-ml-dsa
git diff --check
```

- [ ] **Step 3: Commit**

```bash
git add crates/entangrid-node/src/lib.rs README.md docs/architecture.md docs/protocol.md crates/entangrid-crypto/README.md crates/entangrid-types/README.md
git commit -m "close out stage 1k aggregate hybrid enforcement"
```

## Notes

- Keep this final Stage 1K slice strictly limited to aggregate proof and closeout.
- Do not widen the work to transactions or post-Stage-1K hardening here.
- Prefer tests and docs over new aggregate policy code unless tests prove a real gap.
