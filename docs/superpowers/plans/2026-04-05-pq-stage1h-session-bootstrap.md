# PQ Stage 1H Session Bootstrap Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Extend strict hybrid localnet bootstrap so simulator-generated hybrid localnets include separate ML-KEM session key material and session identity wiring.

**Architecture:** Keep Stage 1H narrowly scoped to the simulator/bootstrap boundary. Reuse the existing `init-localnet --hybrid-enforcement` path, add feature-gated ML-KEM key generation plus node/genesis session wiring, and fail clearly when hybrid bootstrap is requested without a `pq-ml-kem` build.

**Tech Stack:** Rust, `tokio`, `serde_json`, `toml`, `entangrid-types`, `entangrid-crypto`, `entangrid-sim`, optional RustCrypto `ml-kem` behind `pq-ml-kem`.

---

## File Structure

- Modify: `crates/entangrid-sim/Cargo.toml`
  - add `pq-ml-kem` feature forwarding and optional dependency wiring needed for simulator-side session key generation
- Modify: `crates/entangrid-sim/src/lib.rs`
  - extend hybrid bootstrap material generation
  - write `ml-kem-session-key.json`
  - populate `session_public_identity`, `session_backend`, and `session_key_path`
  - add feature-mismatch and generated-config tests
- Optionally modify: `README.md`
  - only if the bootstrap command or requirements text needs updating after implementation

## Task 1: Add Failing Simulator Tests For Session Bootstrap

**Files:**
- Modify: `crates/entangrid-sim/src/lib.rs`

- [ ] **Step 1: Write failing tests for the new hybrid bootstrap behavior**

Add focused tests for:

- generated hybrid genesis includes non-`None` `session_public_identity`
- generated node config includes:
  - `session_backend = HybridDeterministicMlKemExperimental`
  - non-empty `session_key_path`
- generated node directory includes `ml-kem-session-key.json`
- `--hybrid-enforcement` without `pq-ml-kem` fails with a clear message

- [ ] **Step 2: Run the focused simulator tests and verify they fail**

Run:

```bash
cargo test -p entangrid-sim hybrid_enforcement
```

Expected:

- failures because strict hybrid bootstrap still leaves the session side deterministic

## Task 2: Add Simulator-Side ML-KEM Key Generation And Config Wiring

**Files:**
- Modify: `crates/entangrid-sim/Cargo.toml`
- Modify: `crates/entangrid-sim/src/lib.rs`

- [ ] **Step 1: Add feature wiring for simulator-side session bootstrap**

In `crates/entangrid-sim/Cargo.toml`:

- add `pq-ml-kem`
- forward it to `entangrid-crypto/pq-ml-kem`
- add the minimal optional dependency wiring needed to generate ML-KEM key material in the simulator

- [ ] **Step 2: Implement minimal ML-KEM session material generation**

In `crates/entangrid-sim/src/lib.rs`:

- generate one session key file per hybrid node:
  - `ml-kem-session-key.json`
- populate `ValidatorConfig.session_public_identity`
- populate `NodeConfig.session_backend`
- populate `NodeConfig.session_key_path`

Keep signing and session key files separate.

- [ ] **Step 3: Fail clearly when hybrid bootstrap is requested without `pq-ml-kem`**

Reuse the current `pq-ml-dsa` guard style and add the matching `pq-ml-kem` guard so the simulator never emits partial strict-hybrid config.

- [ ] **Step 4: Re-run the focused simulator tests until green**

Run:

```bash
cargo test -p entangrid-sim hybrid_enforcement
cargo test -p entangrid-sim --features "pq-ml-dsa pq-ml-kem" hybrid_enforcement
```

## Task 3: Verify Generated Localnet Artifacts

**Files:**
- Modify: none required unless a test reveals a gap

- [ ] **Step 1: Run a focused hybrid init-localnet generation check**

Run:

```bash
cargo run -p entangrid-sim --features "pq-ml-dsa pq-ml-kem" -- init-localnet --validators 4 --hybrid-enforcement --base-dir var/pq-hybrid-session-bootstrap
```

Confirm:

- each node directory contains `ml-dsa-key.json`
- each node directory contains `ml-kem-session-key.json`
- generated `node.toml` files carry hybrid signing and session backends
- generated `genesis.toml` carries `session_public_identity` for each validator

## Task 4: Final Verification And Commit

**Files:**
- Modify: any touched Stage 1H files

- [ ] **Step 1: Run the Stage 1H verification set**

Run:

```bash
cargo test -p entangrid-sim
cargo test -p entangrid-sim --features "pq-ml-dsa pq-ml-kem"
git diff --check
```

- [ ] **Step 2: Commit the Stage 1H slice**

```bash
git add crates/entangrid-sim/Cargo.toml crates/entangrid-sim/src/lib.rs
git commit -m "bootstrap hybrid session key material"
```

## Notes For The Implementer

- Keep Stage 1H limited to bootstrap/config generation.
- Do not broaden this into encrypted framing or broader transport changes.
- Do not collapse signing and session key files into one artifact.
- Prefer a clear simulator error over generating incomplete strict-hybrid config.
