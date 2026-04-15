# Stage 1 PQ Foundations Design

**Date:** 2026-03-31
**Branch:** `stage-1/pq-integration`
**Status:** Drafted from approved design discussion

## Goal

Introduce post-quantum-ready signing foundations without changing current V2 consensus behavior.

Stage 1 makes Entangrid crypto-agile. It adds scheme-aware signature and identity types, upgrades the crypto backend interfaces to support classical, PQ, or hybrid signing, and migrates the first set of core signed objects to the new typed model.

## Why This Stage Exists

Entangrid's long-term goal is post-quantum security, but the current codebase still uses a deterministic development-only backend. The project already has a strong crypto seam in [`crates/entangrid-crypto`](../../../../crates/entangrid-crypto/README.md), so the safest next step is not a full PQ rollout. The safer step is to make the protocol crypto-aware first.

This stage is intentionally narrow:

- hybrid foundations first
- signing and authentication only
- no KEM/session redesign yet
- no consensus-rule changes
- no transport handshake changes

## Scope

### In Scope

- Add scheme-aware key and signature types to [`crates/entangrid-types`](../../../../crates/entangrid-types/README.md)
- Extend [`crates/entangrid-crypto`](../../../../crates/entangrid-crypto/README.md) traits so backends can support classical-only, PQ-only, or hybrid signing
- Keep the deterministic development backend working
- Migrate the first core signed objects:
  - `SignedTransaction`
  - `Block`
  - `ProposalVote`
- Update verification paths in:
  - ledger
  - node
  - consensus
- Update root docs and crate READMEs to reflect the new crypto-agile model

### Out of Scope

- Session/KEM changes
- Transport protocol changes
- Multi-signature hybrid enforcement at consensus policy level
- Service attestation and relay receipt migration
- Full production key generation and storage
- Full PQ-only rollout

## Architecture

### 1. Typed Signatures And Identities

The current protocol uses anonymous `Vec<u8>` signatures in many signed objects. Stage 1 replaces that pattern for the core path with typed crypto metadata.

Recommended additions in [`crates/entangrid-types/src/lib.rs`](../../../../crates/entangrid-types/src/lib.rs):

- `SignatureScheme`
- `PublicKeyScheme`
- `TypedSignature`
- `PublicIdentity`

Representative shape:

```rust
pub enum SignatureScheme {
    DevDeterministic,
    Ed25519,
    MlDsa,
    Hybrid,
}

pub enum PublicKeyScheme {
    DevDeterministic,
    Ed25519,
    MlDsa,
    Hybrid,
}

pub struct TypedSignature {
    pub scheme: SignatureScheme,
    pub bytes: Vec<u8>,
}

pub struct PublicIdentity {
    pub scheme: PublicKeyScheme,
    pub bytes: Vec<u8>,
}
```

This makes verification depend on explicit scheme metadata instead of assuming one global signing format.

### 2. Crypto-Agile Backend

[`crates/entangrid-crypto/src/lib.rs`](../../../../crates/entangrid-crypto/src/lib.rs) already provides a clean backend seam:

- `Signer`
- `Verifier`
- `HandshakeProvider`
- `TranscriptHasher`
- `CryptoBackend`

Stage 1 should preserve that seam but make signing and verification scheme-aware. The deterministic backend remains the default development backend, while the trait layer becomes capable of supporting:

- classical-only backends
- PQ-only backends
- hybrid backends

The important rule is that consensus code should not care which scheme is underneath. It should ask the backend to verify the typed signature against the relevant validator identity and message.

### 3. Hybrid Foundations, Not Hybrid Policy

This stage supports `Hybrid` in the type system and in the backend API, but it does not yet require consensus to enforce dual-signature policy everywhere.

That distinction matters:

- **This stage:** make the protocol crypto-agile
- **Later stage:** decide exactly how hybrid signatures are required or negotiated

This keeps the first PQ slice small and avoids unnecessary conflict with ongoing consensus stabilization.

## Phased Rollout

### Phase 1: Crypto And Type Foundations

Modify:

- [`crates/entangrid-types/src/lib.rs`](../../../../crates/entangrid-types/src/lib.rs)
- [`crates/entangrid-crypto/src/lib.rs`](../../../../crates/entangrid-crypto/src/lib.rs)

Deliverables:

- typed signature and identity types
- scheme enums
- updated crypto traits
- deterministic backend compatibility preserved

### Phase 2: Core Signed Object Migration

Modify:

- [`crates/entangrid-types/src/lib.rs`](../../../../crates/entangrid-types/src/lib.rs)
- [`crates/entangrid-ledger/src/lib.rs`](../../../../crates/entangrid-ledger/src/lib.rs)
- [`crates/entangrid-node/src/lib.rs`](../../../../crates/entangrid-node/src/lib.rs)
- [`crates/entangrid-consensus/src/lib.rs`](../../../../crates/entangrid-consensus/src/lib.rs)

Deliverables:

- `SignedTransaction.signature` becomes typed
- `Block.signature` becomes typed
- `ProposalVote.signature` becomes typed
- verification paths updated
- existing behavior preserved under deterministic dev crypto

### Phase 3: Documentation And Validation

Modify:

- [`README.md`](../../../../README.md)
- [`docs/architecture.md`](../../../../docs/architecture.md)
- [`docs/protocol.md`](../../../../docs/protocol.md)
- [`crates/entangrid-crypto/README.md`](../../../../crates/entangrid-crypto/README.md)
- [`crates/entangrid-types/README.md`](../../../../crates/entangrid-types/README.md)

Deliverables:

- docs reflect crypto-agile architecture
- Stage 1 PQ scope documented clearly
- verification commands recorded

## Compatibility Rules

Stage 1 must preserve these properties:

- current deterministic backend still works for localnet and tests
- current V2 consensus logic remains behaviorally unchanged
- signed object hashing rules stay stable unless explicitly versioned
- all migrated verifiers dispatch by signature scheme

If a compatibility shortcut is needed, prefer adapters rather than invasive consensus changes.

## Testing Strategy

### Unit Tests

- sign/verify round trips per scheme
- deterministic backend compatibility
- typed signature serialization/deserialization
- validation failures on scheme/signature mismatch

### Integration Tests

- ledger transaction verification with typed signatures
- node block verification with typed signatures
- proposal vote import and verification with typed signatures

### Regression Tests

- current deterministic tests continue to pass unchanged where possible
- V2 protocol tests continue to pass under the dev backend

## Risks

### 1. Type Churn Across The Repo

Moving from raw `Vec<u8>` to typed signatures touches shared data structures. This should be phased and limited to the first core objects only.

### 2. Hidden Assumptions About Signature Encoding

Some code paths may assume signatures are just bytes. This is exactly why Stage 1 should migrate only transactions, blocks, and proposal votes first.

### 3. Premature Hybrid Enforcement

Requiring hybrid validation policy too early would couple crypto rollout to consensus redesign. This stage intentionally avoids that.

## Non-Goals

Stage 1 does **not** claim:

- production-ready post-quantum deployment
- real session confidentiality
- finished key management
- full message-format migration
- completion of all PQ work

It only establishes the foundation that lets later PQ phases land without destabilizing the chain.

## Recommended Next Step

Write the implementation plan for Stage 1 PQ foundations on [`stage-1/pq-integration`](../../../../).
