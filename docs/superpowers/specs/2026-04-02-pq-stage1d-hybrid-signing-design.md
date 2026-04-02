# Stage 1D PQ Hybrid Signing Design

**Date:** 2026-04-02
**Branch:** `stage-1/pq-integration`
**Status:** Drafted from approved design discussion

## Goal

Introduce a first-class hybrid signing format for Entangrid's core signed objects without making hybrid signatures mandatory in consensus policy yet.

Stage 1D builds on:

- Stage 1 typed signature foundations
- Stage 1B experimental ML-DSA signing support
- Stage 1C measurement results

The goal is to make hybrid signing real at the type, serialization, and verification layers before turning it into a required protocol rule.

## Why This Stage Exists

Entangrid now has:

- scheme-aware signatures and public identities
- a deterministic development signing path
- an experimental ML-DSA signing path
- real measurements showing ML-DSA overhead is materially higher than the development path

That means the next safe step is not immediate mandatory PQ-only or mandatory hybrid enforcement. The next safe step is to define what **hybrid** actually means in the protocol and make it representable and verifiable everywhere the core signing path needs it.

This stage exists to answer:

- how hybrid signatures are encoded
- how hybrid identities are represented
- how verification works across single-scheme and hybrid objects
- how the protocol stays compatible while hybrid remains optional

## Scope

### In Scope

- Add a first-class hybrid signature representation
- Add a first-class hybrid identity representation where needed for validation
- Update crypto backends and verification paths so they can understand hybrid bundles
- Support hybrid signatures on the already-migrated core objects:
  - `SignedTransaction`
  - `Block`
  - `ProposalVote`
- Preserve deterministic-only and ML-DSA-only operation
- Document the rollout boundary clearly

### Out of Scope

- mandatory hybrid enforcement in consensus policy
- ML-KEM or session/KEM changes
- service attestation or relay receipt hybrid rollout
- production identity migration across every config flow
- transport/session redesign

## Architecture

### 1. Hybrid Format First, Enforcement Later

Stage 1D should define a real hybrid signature format, but it should not require consensus to reject single-scheme objects yet.

That means:

- deterministic-only objects remain valid
- ML-DSA-only objects remain valid
- hybrid-signed objects become valid too

This is the right rollout order because it lets Entangrid stabilize encoding, storage, verification, and compatibility before turning hybrid into a protocol-wide requirement.

### 2. Keep One Signature Field Per Signed Object

Signed protocol objects should continue to expose a single `signature` field. Hybrid capability should live inside that field rather than by adding parallel signature fields everywhere.

That avoids:

- repetitive object churn
- wider serialization changes than necessary
- future migration pain when hybrid becomes more common

The top-level signed object model should remain:

- one object
- one `signature` field
- one verifier entrypoint

### 3. TypedSignature Must Become A Real Sum Type

Today, `TypedSignature` is effectively a single-scheme container. Stage 1D should evolve it into a representation that can express either:

- a single signature
- a hybrid bundle made of multiple signature components

Recommended direction:

```rust
pub enum TypedSignature {
    Single {
        scheme: SignatureScheme,
        bytes: Vec<u8>,
    },
    Hybrid {
        components: Vec<SignatureComponent>,
    },
}

pub struct SignatureComponent {
    pub scheme: SignatureScheme,
    pub bytes: Vec<u8>,
}
```

Why this is better than “scheme + bytes + optional extras”:

- hybrid is not a normal single-scheme signature with attachments
- the enum avoids invalid states
- the verifier can dispatch cleanly by signature form

The component list should be constrained by validation rules:

- no duplicate component schemes
- no empty hybrid bundle
- deterministic component and ML-DSA component can coexist

### 4. PublicIdentity Needs A Matching Hybrid Form

If a validator can sign with a hybrid bundle, the identity model must be able to describe the public keys that correspond to that bundle.

Stage 1D should extend `PublicIdentity` so verification can understand either:

- a single public key identity
- a hybrid identity made of multiple public-key components

This can be done as:

- an enum mirroring the signature shape
- or another constrained component-based container

The exact encoding can stay narrow to validator startup and verification needs for this stage. The important rule is:

- hybrid signatures must validate against hybrid identities
- single-scheme signatures must still validate against single-scheme identities

### 5. Verification Must Become Form-Aware

Verification can no longer assume “one scheme, one byte array.”

Stage 1D should teach the crypto layer to:

- validate a `Single` signature against the corresponding single identity
- validate a `Hybrid` signature by validating each required component against the matching public-key component

At this stage, “required component” should mean:

- every component present in the hybrid signature must verify cleanly
- the hybrid signature must match the advertised hybrid identity

But consensus policy should still be permissive about whether a core object uses:

- deterministic-only
- ML-DSA-only
- hybrid

That keeps this stage focused on format and verification, not on policy enforcement.

## Rollout Plan

### Phase 1: Hybrid Type Representation

Modify:

- [`crates/entangrid-types/src/lib.rs`](../../../../crates/entangrid-types/src/lib.rs)

Deliverables:

- real hybrid signature shape
- hybrid-capable public identity shape
- serialization and deserialization support
- tests for hybrid type round-trips

### Phase 2: Hybrid Crypto Verification

Modify:

- [`crates/entangrid-crypto/src/lib.rs`](../../../../crates/entangrid-crypto/src/lib.rs)

Deliverables:

- hybrid verification support
- backend helpers that can emit hybrid signatures
- compatibility with deterministic-only and ML-DSA-only paths

### Phase 3: Core Object Adoption

Modify:

- [`crates/entangrid-ledger/src/lib.rs`](../../../../crates/entangrid-ledger/src/lib.rs)
- [`crates/entangrid-node/src/lib.rs`](../../../../crates/entangrid-node/src/lib.rs)
- supporting tests in the relevant crates

Deliverables:

- `SignedTransaction` can carry hybrid signatures
- `Block` can carry hybrid signatures
- `ProposalVote` can carry hybrid signatures
- verification paths accept and validate hybrid objects

### Phase 4: Documentation

Modify:

- [`README.md`](../../../../README.md)
- [`docs/architecture.md`](../../../../docs/architecture.md)
- [`docs/protocol.md`](../../../../docs/protocol.md)
- [`crates/entangrid-crypto/README.md`](../../../../crates/entangrid-crypto/README.md)
- [`crates/entangrid-types/README.md`](../../../../crates/entangrid-types/README.md)

Deliverables:

- hybrid capability is documented clearly
- docs explain that hybrid is supported but not yet mandatory

## Compatibility Rules

Stage 1D must preserve these properties:

- current deterministic localnet and tests still work by default
- current ML-DSA experimental path still works
- single-signature objects remain valid during the rollout
- no consensus-rule change is required to accept hybrid-capable objects
- transport/session behavior remains unchanged

This stage makes hybrid **possible**, not **mandatory**.

## Validation Rules

Stage 1D should define clear structural rules for hybrid signatures:

- hybrid signature must contain at least one component
- hybrid signature must not contain duplicate schemes
- hybrid identity must contain matching public-key components
- every component must verify
- unknown or unsupported component schemes fail verification cleanly

These are format and correctness rules, not policy rules.

## Testing Strategy

### Unit Tests

- hybrid signature serialization/deserialization
- hybrid public identity serialization/deserialization
- hybrid verifier accepts valid deterministic + ML-DSA bundles
- hybrid verifier rejects:
  - duplicate schemes
  - missing identity component
  - invalid component bytes

### Integration Tests

- hybrid-signed transaction validates in ledger
- hybrid-signed block validates in node
- hybrid-signed proposal vote validates in node
- deterministic-only and ML-DSA-only paths continue to pass

### Regression Tests

- current deterministic tests continue to work
- current ML-DSA feature-enabled tests continue to work
- Stage 1C measurement/report path still works after the type changes

## Risks

### 1. Type Churn

Changing `TypedSignature` and `PublicIdentity` again will touch shared structures. The rollout should stay tightly limited to the core signed objects already migrated in Stage 1.

### 2. Invalid Hybrid States

If the type representation is too loose, hybrid signatures can carry contradictory or incomplete data. That is why the enum/sum-type direction is preferred over an “optional extras” structure.

### 3. Premature Policy Coupling

If Stage 1D tries to also enforce mandatory hybrid policy, it will couple crypto rollout to consensus policy too early. That should wait for a later stage after compatibility is proven.

## Success Criteria

Stage 1D is successful when:

- the repo can represent and serialize real hybrid signatures
- the repo can represent matching hybrid validator identities
- hybrid signatures validate on transactions, blocks, and proposal votes
- deterministic-only and ML-DSA-only paths still work
- consensus policy still treats hybrid as supported, not mandatory

## Recommended Next Step

Write the Stage 1D implementation plan on [`stage-1/pq-integration`](../../../../) and then implement hybrid representation and verification before discussing mandatory hybrid enforcement.
