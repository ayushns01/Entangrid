# PQ Stage 1B ML-DSA Signing Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add a real experimental ML-DSA signing backend behind a feature flag, wire backend selection into node startup, and preserve deterministic signing as the default path.

**Architecture:** Keep genesis as the source of public validator identity, move signer selection into node-local config, and build a crypto backend factory that returns either the deterministic backend or a feature-gated ML-DSA backend. Preserve current consensus and transport behavior by changing only the crypto boundary, node startup wiring, and docs/tests needed to prove backend correctness.

**Tech Stack:** Rust, serde, toml, `entangrid-types`, `entangrid-crypto`, `entangrid-node`, `entangrid-sim`, experimental `ml-dsa` crate behind a cargo feature.

---

## File Structure

### Config and type layer

- Modify: `crates/entangrid-types/src/lib.rs`
  - add node-local signing backend config types
  - preserve deterministic defaults and serde compatibility

### Crypto backend layer

- Modify: `crates/entangrid-crypto/Cargo.toml`
  - add the optional ML-DSA dependency and cargo feature
- Modify: `crates/entangrid-crypto/src/lib.rs`
  - add backend factory support
  - keep deterministic backend default
  - add feature-gated ML-DSA signer/verifier

### Node and simulation wiring

- Modify: `crates/entangrid-node/src/lib.rs`
  - replace hard-coded deterministic backend selection with factory-based startup wiring
  - validate signer identity against validator public identity
- Modify: `crates/entangrid-sim/src/lib.rs`
  - keep localnet/test manifests defaulting to deterministic signing

### Documentation

- Modify: `README.md`
- Modify: `docs/architecture.md`
- Modify: `docs/protocol.md`
- Modify: `crates/entangrid-crypto/README.md`
- Modify: `crates/entangrid-types/README.md`
- Modify: `docs/superpowers/plans/2026-03-31-pq-stage1-foundations.md`
  - add a short Stage 1B progress note only if the implementation materially lands

## Task 1: Add Node-Local Signing Backend Config

**Files:**
- Modify: `crates/entangrid-types/src/lib.rs`
- Modify: `crates/entangrid-sim/src/lib.rs`
- Test: `crates/entangrid-types/src/lib.rs`
- Test: `crates/entangrid-sim/src/lib.rs`

- [ ] **Step 1: Write failing tests for signing backend config defaults and serde**

Add tests that prove:

- `NodeConfig` can round-trip a `signing_backend` value through TOML/serde
- deterministic signing is the default when the field is omitted
- ML-DSA config can carry a key path without affecting existing sim defaults

- [ ] **Step 2: Run the focused tests and verify they fail**

Run:

```bash
cargo test -p entangrid-types signing_backend
cargo test -p entangrid-sim node_config
```

Expected:

- tests fail because `SigningBackendKind` and the new `NodeConfig` fields do not exist yet

- [ ] **Step 3: Implement the minimal config additions**

Add:

- `SigningBackendKind`
- `NodeConfig.signing_backend`
- `NodeConfig.signing_key_path`

Use serde defaults so older node configs still deserialize to deterministic signing.

- [ ] **Step 4: Update sim/localnet config generation**

Keep all current localnet and fixture generation on deterministic signing by default.

- [ ] **Step 5: Re-run the focused tests until green**

Run:

```bash
cargo test -p entangrid-types signing_backend
cargo test -p entangrid-sim node_config
```

- [ ] **Step 6: Commit the config slice**

```bash
git add crates/entangrid-types/src/lib.rs crates/entangrid-sim/src/lib.rs
git commit -m "add pq signing backend config"
```

## Task 2: Add Crypto Backend Factory And Deterministic Selection Path

**Files:**
- Modify: `crates/entangrid-crypto/src/lib.rs`
- Modify: `crates/entangrid-node/src/lib.rs`
- Test: `crates/entangrid-crypto/src/lib.rs`
- Test: `crates/entangrid-node/src/lib.rs`

- [ ] **Step 1: Write failing tests for backend factory selection**

Add tests that prove:

- deterministic config returns the deterministic backend
- startup fails if the selected backend does not match validator public identity
- the node no longer depends on hard-coded `DeterministicCryptoBackend::from_genesis(...)`

- [ ] **Step 2: Run the focused tests and verify they fail**

Run:

```bash
cargo test -p entangrid-crypto backend_factory
cargo test -p entangrid-node startup
```

Expected:

- tests fail because there is no backend factory and node startup still hard-codes deterministic crypto

- [ ] **Step 3: Implement the backend factory**

Add a constructor that builds `Arc<dyn CryptoBackend>` from:

- `GenesisConfig`
- `NodeConfig`

Keep the deterministic backend as the default factory path.

- [ ] **Step 4: Wire the node to use the factory**

Update startup in `crates/entangrid-node/src/lib.rs` so it:

- builds the backend from config
- checks the signer identity against the validator's `public_identity`
- fails clearly on mismatch

- [ ] **Step 5: Re-run the focused tests until green**

Run:

```bash
cargo test -p entangrid-crypto backend_factory
cargo test -p entangrid-node startup
```

- [ ] **Step 6: Commit the factory slice**

```bash
git add crates/entangrid-crypto/src/lib.rs crates/entangrid-node/src/lib.rs
git commit -m "add crypto backend factory"
```

## Task 3: Add The Feature-Gated ML-DSA Backend

**Files:**
- Modify: `crates/entangrid-crypto/Cargo.toml`
- Modify: `crates/entangrid-crypto/src/lib.rs`
- Test: `crates/entangrid-crypto/src/lib.rs`

- [ ] **Step 1: Write failing ML-DSA backend tests**

Add tests that prove:

- ML-DSA sign/verify round-trips under the feature
- the emitted signature uses `SignatureScheme::MlDsa`
- selecting ML-DSA without the cargo feature fails clearly

- [ ] **Step 2: Run the focused tests and verify they fail**

Run without the feature:

```bash
cargo test -p entangrid-crypto ml_dsa
```

Then run with the feature:

```bash
cargo test -p entangrid-crypto --features pq-ml-dsa ml_dsa
```

Expected:

- tests fail because the optional dependency, feature, and backend do not exist yet

- [ ] **Step 3: Add the cargo feature and optional dependency**

Update `crates/entangrid-crypto/Cargo.toml` with:

- optional ML-DSA dependency
- `pq-ml-dsa` cargo feature

- [ ] **Step 4: Implement the minimal ML-DSA backend**

Add:

- key loading from `signing_key_path`
- public-key derivation
- sign/verify support for `SignatureScheme::MlDsa`
- clear runtime errors for feature-disabled selection

- [ ] **Step 5: Re-run the focused tests until green**

Run:

```bash
cargo test -p entangrid-crypto ml_dsa
cargo test -p entangrid-crypto --features pq-ml-dsa ml_dsa
```

- [ ] **Step 6: Commit the ML-DSA backend slice**

```bash
git add crates/entangrid-crypto/Cargo.toml crates/entangrid-crypto/src/lib.rs
git commit -m "add experimental ml-dsa backend"
```

## Task 4: Verify Typed Transaction, Block, And Vote Paths Under ML-DSA

**Files:**
- Modify: `crates/entangrid-node/src/lib.rs`
- Modify: `crates/entangrid-ledger/src/lib.rs`
- Test: `crates/entangrid-ledger/src/lib.rs`
- Test: `crates/entangrid-node/src/lib.rs`

- [ ] **Step 1: Write failing end-to-end verification tests for ML-DSA-signed core objects**

Add tests that prove:

- an ML-DSA-signed transaction validates in ledger
- an ML-DSA-signed block validates in node
- an ML-DSA-signed proposal vote validates in node import paths

- [ ] **Step 2: Run the focused tests and verify they fail**

Run:

```bash
cargo test -p entangrid-ledger --features pq-ml-dsa ml_dsa
cargo test -p entangrid-node --features pq-ml-dsa ml_dsa
```

Expected:

- failures point to missing or incomplete ML-DSA verification integration

- [ ] **Step 3: Implement the minimal wiring to make the tests pass**

Keep the typed-signature hashing rules unchanged and only update verification dispatch where needed.

- [ ] **Step 4: Re-run the focused tests until green**

Run:

```bash
cargo test -p entangrid-ledger --features pq-ml-dsa ml_dsa
cargo test -p entangrid-node --features pq-ml-dsa ml_dsa
```

- [ ] **Step 5: Commit the integration slice**

```bash
git add crates/entangrid-ledger/src/lib.rs crates/entangrid-node/src/lib.rs
git commit -m "verify core objects under ml-dsa"
```

## Task 5: Document And Verify Stage 1B

**Files:**
- Modify: `README.md`
- Modify: `docs/architecture.md`
- Modify: `docs/protocol.md`
- Modify: `crates/entangrid-crypto/README.md`
- Modify: `crates/entangrid-types/README.md`

- [ ] **Step 1: Update docs for Stage 1B**

Document:

- node-local backend selection
- ML-DSA feature flag
- deterministic default compatibility
- explicit non-goals: no KEM/session and no hybrid enforcement yet

- [ ] **Step 2: Run formatting and documentation sanity checks**

Run:

```bash
cargo fmt
git diff --check
```

- [ ] **Step 3: Run full verification without the PQ feature**

Run:

```bash
cargo test -p entangrid-types -p entangrid-crypto -p entangrid-ledger -p entangrid-consensus -p entangrid-network -p entangrid-node -p entangrid-sim
```

- [ ] **Step 4: Run full verification with the PQ feature on touched crates**

Run:

```bash
cargo test -p entangrid-types -p entangrid-crypto -p entangrid-ledger -p entangrid-node --features pq-ml-dsa
```

- [ ] **Step 5: Record measurement notes**

Document:

- ML-DSA signature size
- sign latency
- verify latency
- any obvious serialized overhead differences

- [ ] **Step 6: Commit docs and verification updates**

```bash
git add README.md docs/architecture.md docs/protocol.md crates/entangrid-crypto/README.md crates/entangrid-types/README.md
git commit -m "document pq stage 1b ml-dsa signing"
```
