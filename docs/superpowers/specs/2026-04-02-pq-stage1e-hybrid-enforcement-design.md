# Stage 1E PQ Hybrid Enforcement Design

**Date:** 2026-04-02
**Branch:** `stage-1/pq-integration`
**Status:** Drafted from approved design discussion

## Goal

Add a real, network-wide hybrid-signature enforcement mode for validator-originated consensus objects while keeping the rollout reversible behind a feature flag.

Stage 1E builds on:

- Stage 1 typed signature and identity foundations
- Stage 1B experimental ML-DSA signing support
- Stage 1D hybrid signature and identity support for core objects

The goal is to move from "hybrid-capable" to "hybrid-enforceable" for the first validator-originated consensus surface.

## Why This Stage Exists

Entangrid now has:

- typed signatures and typed public identities
- experimental ML-DSA signing
- hybrid signature and identity bundles
- permissive verification that accepts deterministic-only, ML-DSA-only, and hybrid forms when they match identity material

That means the next protocol step is not more representation work. The next step is policy:

- when should hybrid become required
- for which signed objects
- how should the network fail if identity material is incomplete

Stage 1E exists to introduce a strict policy mode without forcing that policy onto every deployment immediately.

## Scope

### In Scope

- add a feature flag that requires hybrid validator signatures
- require every validator in genesis to advertise a hybrid public identity when the flag is on
- enforce hybrid signatures on:
  - `Block`
  - `ProposalVote`
- reject non-hybrid validator signatures for those objects when enforcement is enabled
- update docs and tests for the enforcement mode

### Out of Scope

- transaction hybrid enforcement
- relay receipt hybrid enforcement
- service attestation or aggregate hybrid enforcement
- ML-KEM or session/KEM changes
- production key lifecycle and operational rollout tooling
- consensus rule changes outside validator-originated signature policy

## Architecture

### 1. Policy Lives In Node Configuration, Not Crypto Verification

The crypto layer should remain capable of verifying:

- deterministic-only signatures
- ML-DSA-only signatures
- hybrid signatures

That is still the right design boundary because cryptography should answer "is this signature valid for this identity," not "is this the only acceptable signature policy for this deployment."

The enforcement rule should therefore live in node/runtime policy, not inside the raw crypto verifier.

Recommended location:

- add `require_hybrid_validator_signatures: bool` under `FeatureFlags` in [`crates/entangrid-types/src/lib.rs`](../../../../crates/entangrid-types/src/lib.rs)

Why this is the right place:

- it is a deployment/runtime policy
- it changes protocol acceptance behavior
- it should be visible alongside other protocol feature flags

### 2. Startup Must Validate Genesis Identity Compatibility

When hybrid enforcement is enabled, startup in [`crates/entangrid-node/src/lib.rs`](../../../../crates/entangrid-node/src/lib.rs) must fail unless **every validator** in genesis advertises a hybrid `PublicIdentity`.

This is a deliberate hard fail, not a soft warning.

Why:

- mixed identity sets create ambiguous enforcement semantics
- the network policy should be uniform
- failing at startup is safer than discovering the mismatch after peers have already exchanged blocks and votes

The startup rule should be:

- if enforcement flag is `false`, current permissive behavior remains unchanged
- if enforcement flag is `true`, every validator identity must be `PublicKeyScheme::Hybrid`

### 3. First Enforcement Surface: Blocks And Proposal Votes

Stage 1E should enforce hybrid signatures only for validator-originated consensus objects:

- `Block`
- `ProposalVote`

These are the right first objects because:

- they are central to consensus progression
- they are validator-originated, not user-originated
- they already run through the typed-signature and configured-backend path

When the flag is enabled:

- block validation must reject any block whose signature is not hybrid
- proposal-vote validation must reject any proposal vote whose signature is not hybrid

The object must satisfy both:

- signer identity is hybrid
- object signature is hybrid and verifies successfully

### 4. Transactions Stay Out Of Scope For This Slice

Transactions should remain outside hybrid enforcement in Stage 1E.

Reason:

- transactions represent a different migration surface than validator-controlled consensus objects
- user-signature rollout will likely need a different compatibility story
- mixing user-signature migration into the first enforcement slice would create unnecessary complexity

This stage is specifically about validator-originated consensus policy.

## Enforcement Rules

With `require_hybrid_validator_signatures = true`:

### Startup

- node startup fails if any validator in genesis has a non-hybrid public identity

### Block Validation

- reject if proposer identity is not hybrid
- reject if block signature is not hybrid
- reject if hybrid verification fails

### Proposal Vote Validation

- reject if validator identity is not hybrid
- reject if proposal-vote signature is not hybrid
- reject if hybrid verification fails

### Runtime Signing

This stage does **not** require changing the configured signing backend API surface beyond what Stage 1D already introduced.

The expected operator path is:

- use `HybridDeterministicMlDsaExperimental`
- advertise hybrid validator identities in genesis
- enable the enforcement flag

If a validator enables enforcement but configures a non-hybrid local signing backend, runtime behavior should naturally fail at object production or validation, and tests should cover the expected failure mode.

## Rollout Plan

### Phase 1: Config And Startup Gate

Modify:

- [`crates/entangrid-types/src/lib.rs`](../../../../crates/entangrid-types/src/lib.rs)
- [`crates/entangrid-node/src/lib.rs`](../../../../crates/entangrid-node/src/lib.rs)

Deliverables:

- new enforcement feature flag
- startup validation that requires all-hybrid validator identities when the flag is enabled

### Phase 2: Block And Proposal-Vote Enforcement

Modify:

- [`crates/entangrid-node/src/lib.rs`](../../../../crates/entangrid-node/src/lib.rs)

Deliverables:

- non-hybrid block rejection when enforcement is enabled
- non-hybrid proposal-vote rejection when enforcement is enabled
- existing permissive behavior preserved when the flag is disabled

### Phase 3: Documentation

Modify:

- [`README.md`](../../../../README.md)
- [`docs/architecture.md`](../../../../docs/architecture.md)
- [`docs/protocol.md`](../../../../docs/protocol.md)
- [`crates/entangrid-crypto/README.md`](../../../../crates/entangrid-crypto/README.md)
- [`crates/entangrid-types/README.md`](../../../../crates/entangrid-types/README.md)

Deliverables:

- clear documentation of the enforcement flag
- clear statement that enforcement is currently limited to blocks and proposal votes
- clear statement that transactions and session/KEM remain out of scope

## Testing Strategy

### Unit And Integration Tests

Add tests that prove:

- startup fails with mixed validator identities when enforcement is enabled
- startup succeeds when all validator identities are hybrid
- non-hybrid block is rejected when enforcement is enabled
- non-hybrid proposal vote is rejected when enforcement is enabled
- hybrid block is accepted when enforcement is enabled
- hybrid proposal vote is accepted when enforcement is enabled
- permissive behavior remains intact when the enforcement flag is disabled

### Verification

Expected verification commands:

```bash
cargo test -p entangrid-types -p entangrid-crypto -p entangrid-ledger -p entangrid-consensus -p entangrid-network -p entangrid-node -p entangrid-sim
cargo test -p entangrid-crypto -p entangrid-ledger -p entangrid-node --features pq-ml-dsa
```

If node or network tests require unrestricted localhost socket access, rerun them outside the sandbox as needed.

## Risks And Trade-Offs

### Why Not Enforce Transactions Too?

Because validator-controlled objects and user-controlled objects have different rollout costs.

Blocks and proposal votes are under operator control. Transactions are not. Stage 1E should therefore prove hybrid enforcement where the network can realistically coordinate first.

### Why Require All-Hybrid Genesis Identities?

Because partial enforcement is easy to misread and hard to reason about operationally.

A network-wide enforcement mode should mean one thing:

- all validators advertise hybrid identities
- all validator-originated consensus objects must be hybrid signed

That is easier to debug, document, and test than per-validator partial enforcement.

## Success Criteria

Stage 1E is successful when:

- the network can be started in a strict hybrid-enforcement mode
- startup fails loudly for mixed validator identity sets
- blocks and proposal votes are rejected unless they are hybrid signed in that mode
- permissive mode still works when the flag is off
- all relevant tests and docs are updated

## Summary

Stage 1E is the first real policy step after Stage 1D's hybrid-capable representation work.

It keeps the crypto layer flexible while making the node policy strict when explicitly requested. It focuses on the highest-value validator-originated consensus objects first, avoids dragging user transactions into the same migration, and gives Entangrid a controlled path from hybrid-capable to hybrid-enforced.
