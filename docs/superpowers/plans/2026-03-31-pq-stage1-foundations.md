# PQ Stage 1 Foundations Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make Entangrid crypto-agile for signing and authentication by adding scheme-aware signature and identity types, upgrading the crypto backend, and migrating transactions, blocks, and proposal votes without changing V2 consensus behavior.

**Architecture:** Keep current V2 protocol logic unchanged while moving the crypto boundary from anonymous byte blobs to typed signatures and identities. Land the work in small phases: foundation types and traits first, core signed-object migration second, and docs plus full verification last so the deterministic development backend remains fully compatible during the transition.

**Tech Stack:** Rust, serde, `entangrid-types`, `entangrid-crypto`, `entangrid-ledger`, `entangrid-consensus`, `entangrid-node`.

---

## Stage 1B Status Note

Stage 1B now builds on this plan with:

- `SigningBackendKind` in `NodeConfig`
- a crypto backend factory instead of hard-coded deterministic startup wiring
- an experimental `MlDsa65Experimental` backend behind `pq-ml-dsa`
- ML-DSA validation tests for transactions, blocks, and proposal votes

Session/KEM work and hybrid enforcement are still intentionally out of scope.

## File Structure

### Crypto and type foundation

- Modify: `crates/entangrid-types/src/lib.rs`
  - add `SignatureScheme`, `PublicKeyScheme`, `TypedSignature`, and typed public identity support
  - migrate the first shared structs to the typed signature container
- Modify: `crates/entangrid-crypto/src/lib.rs`
  - make signing and verification scheme-aware while preserving deterministic dev compatibility

### Core verification path

- Modify: `crates/entangrid-ledger/src/lib.rs`
  - verify typed transaction signatures
- Modify: `crates/entangrid-consensus/src/lib.rs`
  - validate typed proposal vote signatures
- Modify: `crates/entangrid-node/src/lib.rs`
  - sign and verify typed block and proposal vote signatures

### Docs

- Modify: `README.md`
- Modify: `docs/architecture.md`
- Modify: `docs/protocol.md`
- Modify: `crates/entangrid-crypto/README.md`
- Modify: `crates/entangrid-types/README.md`

## Task 1: Add Typed Signature And Identity Foundations

**Files:**
- Modify: `crates/entangrid-types/src/lib.rs`
- Modify: `crates/entangrid-crypto/src/lib.rs`
- Test: `crates/entangrid-types/src/lib.rs`
- Test: `crates/entangrid-crypto/src/lib.rs`

- [ ] **Step 1: Write failing type-layer tests for scheme-aware signatures**

Add tests that prove:

- `TypedSignature` and `PublicIdentity` serialize and deserialize cleanly
- scheme metadata survives round-trip
- deterministic signatures can be represented as `SignatureScheme::DevDeterministic`

- [ ] **Step 2: Run the focused tests and verify they fail for the expected reason**

Run:

```bash
cargo test -p entangrid-types typed_signature
cargo test -p entangrid-crypto deterministic_backend
```

Expected:

- tests fail because the new types and scheme-aware crypto API do not exist yet

- [ ] **Step 3: Implement the minimal type additions**

Add:

- `SignatureScheme`
- `PublicKeyScheme`
- `TypedSignature`
- `PublicIdentity`

Keep serde derives and backward-compatible defaults where appropriate.

- [ ] **Step 4: Implement the minimal crypto trait and backend changes**

Update the crypto backend so:

- signing returns `TypedSignature`
- verification accepts `TypedSignature`
- deterministic backend emits `SignatureScheme::DevDeterministic`
- existing transcript/session behavior remains unchanged

- [ ] **Step 5: Re-run the focused tests and make them pass**

Run:

```bash
cargo test -p entangrid-types typed_signature
cargo test -p entangrid-crypto deterministic_backend
```

Expected:

- all new and existing focused tests pass

- [ ] **Step 6: Commit the foundation slice**

```bash
git add crates/entangrid-types/src/lib.rs crates/entangrid-crypto/src/lib.rs
git commit -m "add typed crypto foundations"
```

## Task 2: Migrate Transactions, Blocks, And Proposal Votes

**Files:**
- Modify: `crates/entangrid-types/src/lib.rs`
- Modify: `crates/entangrid-ledger/src/lib.rs`
- Modify: `crates/entangrid-consensus/src/lib.rs`
- Modify: `crates/entangrid-node/src/lib.rs`
- Test: `crates/entangrid-ledger/src/lib.rs`
- Test: `crates/entangrid-consensus/src/lib.rs`
- Test: `crates/entangrid-node/src/lib.rs`

- [ ] **Step 1: Write failing migration tests**

Add tests that prove:

- typed transaction signatures validate in ledger
- typed block signatures validate in node
- typed proposal vote signatures validate in consensus and node import paths

- [ ] **Step 2: Run the focused tests and verify they fail**

Run:

```bash
cargo test -p entangrid-ledger typed_signature
cargo test -p entangrid-consensus proposal_vote
cargo test -p entangrid-node typed_signature
```

Expected:

- failures point to raw-byte signature assumptions in the migrated structs and verifiers

- [ ] **Step 3: Migrate the shared structs**

Change:

- `SignedTransaction.signature`
- `Block.signature`
- `ProposalVote.signature`

from raw `Vec<u8>` to `TypedSignature`.

- [ ] **Step 4: Update verification and signing paths**

Update:

- ledger transaction validation
- block signing and block verification
- proposal vote signing and proposal vote verification

Keep message hashing and consensus behavior unchanged.

- [ ] **Step 5: Re-run focused tests until green**

Run:

```bash
cargo test -p entangrid-ledger
cargo test -p entangrid-consensus
cargo test -p entangrid-node
```

- [ ] **Step 6: Commit the migration slice**

```bash
git add crates/entangrid-types/src/lib.rs crates/entangrid-ledger/src/lib.rs crates/entangrid-consensus/src/lib.rs crates/entangrid-node/src/lib.rs
git commit -m "migrate core signatures to typed crypto"
```

## Task 3: Document The PQ Foundation Model

**Files:**
- Modify: `README.md`
- Modify: `docs/architecture.md`
- Modify: `docs/protocol.md`
- Modify: `crates/entangrid-crypto/README.md`
- Modify: `crates/entangrid-types/README.md`

- [ ] **Step 1: Update root and crate docs**

Document:

- Stage 1 scope
- typed signature and identity model
- hybrid foundations versus hybrid policy
- deterministic backend compatibility

- [ ] **Step 2: Run documentation sanity checks**

Run:

```bash
git diff --check
```

- [ ] **Step 3: Commit the docs slice**

```bash
git add README.md docs/architecture.md docs/protocol.md crates/entangrid-crypto/README.md crates/entangrid-types/README.md
git commit -m "document pq stage 1 foundations"
```

## Task 4: Full Verification

**Files:**
- Verify: `crates/entangrid-types`
- Verify: `crates/entangrid-crypto`
- Verify: `crates/entangrid-ledger`
- Verify: `crates/entangrid-consensus`
- Verify: `crates/entangrid-node`

- [ ] **Step 1: Run the full Rust verification suite**

```bash
cargo test -p entangrid-types -p entangrid-crypto -p entangrid-ledger -p entangrid-consensus -p entangrid-node
```

- [ ] **Step 2: Review compatibility risks**

Check that:

- deterministic backend still signs and verifies all migrated objects
- V2-specific verification logic did not change behavior
- no unexpected serialization regressions were introduced

- [ ] **Step 3: Commit any final verification-only fixes**

```bash
git add -A
git commit -m "polish pq stage 1 verification"
```
