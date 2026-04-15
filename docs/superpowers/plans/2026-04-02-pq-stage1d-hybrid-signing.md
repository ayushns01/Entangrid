# PQ Stage 1D Hybrid Signing Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add first-class hybrid signature and identity support for transactions, blocks, and proposal votes while keeping hybrid enforcement optional.

**Architecture:** Evolve `TypedSignature` and `PublicIdentity` into backward-compatible sum types that preserve the existing single-scheme encoding while adding hybrid bundle forms. Extend the crypto backend with a real hybrid signer/verifier path, then wire the already-migrated core objects through ledger and node verification without changing consensus policy.

**Tech Stack:** Rust, serde, bincode, `entangrid-types`, `entangrid-crypto`, `entangrid-ledger`, `entangrid-node`, optional `pq-ml-dsa` feature.

---

## File Structure

### Type layer

- Modify: `crates/entangrid-types/src/lib.rs`
  - replace flat single-scheme `TypedSignature` and `PublicIdentity` structs with backward-compatible sum types
  - add component types and helper methods
  - preserve current single-signature and single-identity serialized shape

### Crypto layer

- Modify: `crates/entangrid-crypto/src/lib.rs`
  - add hybrid signer/verifier logic
  - add a hybrid backend selection path
  - keep deterministic-only and ML-DSA-only paths working

### Runtime adoption

- Modify: `crates/entangrid-ledger/src/lib.rs`
  - validate hybrid-signed transactions
- Modify: `crates/entangrid-node/src/lib.rs`
  - sign and verify hybrid blocks and proposal votes
  - update signature-hash helpers and tests
- Modify: `crates/entangrid-sim/src/lib.rs`
  - keep defaults deterministic
  - add any narrow hybrid fixture support needed by tests only

### Documentation

- Modify: `README.md`
- Modify: `docs/architecture.md`
- Modify: `docs/protocol.md`
- Modify: `crates/entangrid-crypto/README.md`
- Modify: `crates/entangrid-types/README.md`

## Task 1: Add Backward-Compatible Hybrid Types

**Files:**
- Modify: `crates/entangrid-types/src/lib.rs`
- Test: `crates/entangrid-types/src/lib.rs`

- [ ] **Step 1: Write failing type tests for hybrid signatures and identities**

Add tests that prove:

- existing single-signature encoding still round-trips
- existing single-identity encoding still round-trips
- hybrid signature round-trips with multiple components
- hybrid identity round-trips with multiple components
- duplicate hybrid signature schemes are rejected by helpers

- [ ] **Step 2: Run the focused tests and verify they fail**

Run:

```bash
cargo test -p entangrid-types hybrid
```

Expected:

- tests fail because hybrid component types and helper constructors do not exist yet

- [ ] **Step 3: Implement the minimal hybrid type model**

Add:

- `SignatureComponent`
- `PublicIdentityComponent`
- backward-compatible `TypedSignature`
- backward-compatible `PublicIdentity`
- helper methods such as:
  - single constructors
  - hybrid constructors
  - `scheme()`
  - `components()`
  - matching/lookup helpers used by crypto verification

Preserve current single-form serde/bincode shape.

- [ ] **Step 4: Re-run the focused tests until green**

Run:

```bash
cargo test -p entangrid-types hybrid
```

- [ ] **Step 5: Commit the type slice**

```bash
git add crates/entangrid-types/src/lib.rs
git commit -m "add hybrid signature and identity types"
```

## Task 2: Add Hybrid Crypto Backend Support

**Files:**
- Modify: `crates/entangrid-crypto/src/lib.rs`
- Test: `crates/entangrid-crypto/src/lib.rs`

- [ ] **Step 1: Write failing crypto tests for hybrid signing and verification**

Add tests that prove:

- a hybrid signature can be emitted for a validator with deterministic + ML-DSA identity material
- a hybrid signature verifies against a matching hybrid identity
- a single deterministic or ML-DSA signature can still verify against a matching hybrid identity during the permissive rollout
- duplicate or mismatched hybrid components fail verification

- [ ] **Step 2: Run the focused tests and verify they fail**

Run:

```bash
cargo test -p entangrid-crypto hybrid
cargo test -p entangrid-crypto --features pq-ml-dsa hybrid
```

Expected:

- tests fail because the backend cannot yet build or verify hybrid bundles

- [ ] **Step 3: Implement the minimal hybrid backend path**

Add:

- `SigningBackendKind::HybridDeterministicMlDsaExperimental`
- hybrid local signer state in the crypto backend
- hybrid signature emission by combining deterministic and ML-DSA component signatures
- hybrid identity verification logic
- permissive verification rule:
  - single-scheme signatures still verify against matching single identities
  - single-scheme signatures also verify against matching hybrid identity components
  - full hybrid signatures verify against full hybrid identities

- [ ] **Step 4: Re-run the focused tests until green**

Run:

```bash
cargo test -p entangrid-crypto hybrid
cargo test -p entangrid-crypto --features pq-ml-dsa hybrid
```

- [ ] **Step 5: Commit the crypto slice**

```bash
git add crates/entangrid-crypto/src/lib.rs
git commit -m "add hybrid crypto backend support"
```

## Task 3: Adopt Hybrid Signatures In Core Object Validation

**Files:**
- Modify: `crates/entangrid-ledger/src/lib.rs`
- Modify: `crates/entangrid-node/src/lib.rs`
- Possibly modify: `crates/entangrid-sim/src/lib.rs`
- Test: `crates/entangrid-ledger/src/lib.rs`
- Test: `crates/entangrid-node/src/lib.rs`

- [ ] **Step 1: Write failing integration tests for hybrid-signed core objects**

Add tests that prove:

- hybrid-signed transactions validate in ledger
- hybrid-signed blocks validate in node
- hybrid-signed proposal votes validate in node
- deterministic-only and ML-DSA-only core paths still pass

- [ ] **Step 2: Run the focused tests and verify they fail**

Run:

```bash
cargo test -p entangrid-ledger hybrid
cargo test -p entangrid-node hybrid
cargo test -p entangrid-ledger --features pq-ml-dsa hybrid
cargo test -p entangrid-node --features pq-ml-dsa hybrid
```

Expected:

- tests fail because the runtime paths do not yet understand hybrid object signatures end to end

- [ ] **Step 3: Implement the minimal runtime adoption**

Update:

- transaction validation
- block validation
- proposal-vote validation
- any signature-hash helpers that need to stay neutral to the new signature form

Keep defaults deterministic and avoid any mandatory hybrid policy.

- [ ] **Step 4: Re-run the focused tests until green**

Run:

```bash
cargo test -p entangrid-ledger hybrid
cargo test -p entangrid-node hybrid
cargo test -p entangrid-ledger --features pq-ml-dsa hybrid
cargo test -p entangrid-node --features pq-ml-dsa hybrid
```

- [ ] **Step 5: Commit the runtime slice**

```bash
git add crates/entangrid-ledger/src/lib.rs crates/entangrid-node/src/lib.rs crates/entangrid-sim/src/lib.rs
git commit -m "adopt hybrid signatures for core objects"
```

## Task 4: Update Docs And Run Final Verification

**Files:**
- Modify: `README.md`
- Modify: `docs/architecture.md`
- Modify: `docs/protocol.md`
- Modify: `crates/entangrid-crypto/README.md`
- Modify: `crates/entangrid-types/README.md`

- [ ] **Step 1: Update docs for Stage 1D**

Document:

- hybrid signature capability exists
- hybrid is supported but not yet mandatory
- deterministic and ML-DSA single paths still work

- [ ] **Step 2: Run the final verification set**

Run:

```bash
cargo test -p entangrid-types -p entangrid-crypto -p entangrid-ledger -p entangrid-node
cargo test -p entangrid-crypto -p entangrid-ledger -p entangrid-node --features pq-ml-dsa
git diff --check
```

- [ ] **Step 3: Commit the docs/final verification slice**

```bash
git add README.md docs/architecture.md docs/protocol.md crates/entangrid-crypto/README.md crates/entangrid-types/README.md
git commit -m "document pq hybrid signing support"
```
