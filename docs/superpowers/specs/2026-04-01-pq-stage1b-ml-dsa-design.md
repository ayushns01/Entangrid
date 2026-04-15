# Stage 1B PQ ML-DSA Signing Design

**Date:** 2026-04-01
**Branch:** `stage-1/pq-integration`
**Status:** Drafted from approved design discussion

## Goal

Add a real experimental post-quantum signing backend to Entangrid without changing V2 consensus rules or transport/session behavior.

Stage 1B builds directly on the typed-signature foundations from Stage 1. It keeps deterministic dev signing as the default path, adds a feature-gated ML-DSA backend for signing and verification, and wires backend selection into node startup in a way that does not disturb current consensus behavior.

## Why This Stage Exists

Stage 1 made the protocol crypto-agile, but it still uses only the deterministic development backend. The next safe step is not a full PQ rollout. The next safe step is to prove that the crypto boundary can host a real PQ signing backend while preserving:

- current V2 consensus semantics
- current deterministic localnet behavior
- current session and transport behavior
- current test ergonomics for non-PQ workflows

This stage is intentionally narrow:

- signing and authentication only
- no KEM/session changes
- no hybrid enforcement policy yet
- no consensus timing changes
- no transport handshake redesign

## Scope

### In Scope

- Add a real experimental ML-DSA signing backend behind a cargo feature
- Add node-local backend selection and signing key loading
- Keep deterministic signing as the default backend
- Verify typed signatures using validator public identity metadata
- Update node startup so signer identity must match configured validator identity
- Add tests for backend selection, key mismatch rejection, and ML-DSA sign/verify
- Add documentation for the new backend boundary and rollout

### Out of Scope

- ML-KEM or session confidentiality changes
- hybrid dual-signature enforcement
- consensus policy changes based on scheme choice
- PQ key distribution protocol changes
- production HSM or secure enclave integration
- broad fixture migration to PQ by default

## Architecture

### 1. Separation Of Concerns

This stage keeps three distinct responsibilities separate:

- **Genesis config** defines the validator's public identity
- **Node config** defines how the local node signs as that validator
- **Crypto backend** performs scheme-specific sign/verify operations

This is the right split for Entangrid because validator identity is network-visible, but signer choice and secret-key loading are local operational concerns.

### 2. Public Identity Remains In Genesis

[`ValidatorConfig.public_identity`](../../../../crates/entangrid-types/src/lib.rs) is already the network-visible identity record and should remain the source of truth.

For Stage 1B:

- `public_identity.scheme` tells verifiers which public-key family the validator uses
- `public_identity.bytes` stores the encoded verifying key for that validator

For ML-DSA validators, `public_identity.bytes` should contain the encoded ML-DSA verifying key.

Private key material must not be stored in genesis.

### 3. Backend Selection Belongs In Node Config

[`FeatureFlags`](../../../../crates/entangrid-types/src/lib.rs) should remain consensus and runtime policy only. Signing backend choice is not a consensus feature flag. It is a node-local identity and key-management concern.

Stage 1B should extend [`NodeConfig`](../../../../crates/entangrid-types/src/lib.rs) with local signing configuration, conceptually:

```rust
pub enum SigningBackendKind {
    DevDeterministic,
    MlDsa65Experimental,
}

pub struct NodeConfig {
    // existing fields...
    pub signing_backend: SigningBackendKind,
    pub signing_key_path: Option<String>,
}
```

Recommended semantics:

- `DevDeterministic`
  - uses the existing validator `dev_secret`
  - ignores `signing_key_path`
- `MlDsa65Experimental`
  - loads the private signing key from `signing_key_path`
  - derives the verifying key
  - checks it against the validator's configured `public_identity`

### 4. Crypto Backend Factory

[`crates/entangrid-crypto/src/lib.rs`](../../../../crates/entangrid-crypto/src/lib.rs) already provides the right seam through:

- `Signer`
- `Verifier`
- `HandshakeProvider`
- `TranscriptHasher`
- `CryptoBackend`

Stage 1B should add a small factory layer that builds `Arc<dyn CryptoBackend>` from:

- [`GenesisConfig`](../../../../crates/entangrid-types/src/lib.rs)
- [`NodeConfig`](../../../../crates/entangrid-types/src/lib.rs)

That factory should:

- return the deterministic backend by default
- return an ML-DSA backend when selected and available
- fail loudly if the requested backend is unavailable or misconfigured

This keeps backend selection out of consensus code and prevents scheme-specific branching from spreading across the repo.

### 5. Feature-Gated ML-DSA Backend

The ML-DSA implementation should live in [`crates/entangrid-crypto`](../../../../crates/entangrid-crypto/README.md) behind a cargo feature, for example:

- feature name: `pq-ml-dsa`

When the feature is enabled:

- the crate builds an experimental `MlDsa65CryptoBackend`
- startup can select `MlDsa65Experimental`

When the feature is disabled:

- deterministic signing still works normally
- selecting `MlDsa65Experimental` causes startup to fail with a clear error

This preserves a fast default development loop while keeping the PQ backend real and testable.

## Data Flow

### Deterministic Path

1. node startup loads `NodeConfig`
2. backend factory selects `DevDeterministic`
3. backend signs and verifies using `dev_secret`
4. typed signatures carry `SignatureScheme::DevDeterministic`

### ML-DSA Path

1. node startup loads `NodeConfig`
2. backend factory selects `MlDsa65Experimental`
3. backend loads the ML-DSA secret key from `signing_key_path`
4. backend derives the verifying key
5. startup checks the derived key against the validator's `public_identity`
6. typed signatures carry `SignatureScheme::MlDsa`
7. verification dispatches by `public_identity.scheme`

Consensus code should remain unchanged: it only signs and verifies through the backend and typed signature model.

## Error Handling

Stage 1B should fail early and clearly.

Startup errors:

- requested ML-DSA backend but cargo feature disabled
- missing `signing_key_path` for ML-DSA mode
- malformed key file
- unknown validator id
- derived verifying key does not match validator `public_identity`

Runtime verification behavior:

- wrong signature scheme for the validator identity returns `false`
- malformed signatures return verification failure, not panic
- no silent fallback from ML-DSA to deterministic mode

This stage should prefer explicit startup failure over ambiguous runtime behavior.

## Compatibility Rules

Stage 1B must preserve these properties:

- deterministic signing remains the default behavior for local tests and localnet
- current typed-signature paths for transactions, blocks, and proposal votes stay valid
- consensus rules remain unchanged
- transport/session behavior remains unchanged
- non-PQ workflows do not need PQ dependencies unless the feature is explicitly enabled

The protocol becomes capable of PQ signing, but it does not become PQ-only.

## Testing Strategy

### Unit Tests

- deterministic backend still signs and verifies unchanged
- ML-DSA sign/verify round trip
- typed signature scheme matches backend
- backend factory chooses the requested backend
- feature-disabled ML-DSA selection fails clearly
- startup identity mismatch is rejected

### Integration Tests

- node startup with deterministic backend
- node startup with ML-DSA backend
- transaction verification under ML-DSA
- block signature verification under ML-DSA
- proposal vote verification under ML-DSA

### Measurement Tests

Record and document:

- signature size
- sign latency
- verify latency
- serialized message overhead for typed PQ signatures

Stage 1B should not optimize these numbers yet. It should measure and document them.

## Risks

### 1. Key Encoding Drift

If the encoded verifying key in `public_identity.bytes` is not stable, startup validation becomes brittle. Stage 1B should choose one canonical encoded key format and document it.

### 2. Type/Fixture Churn

The typed-signature model is already in place, but ML-DSA tests will still touch many fixtures. Keep deterministic as the default so PQ-specific changes stay targeted.

### 3. Feature-Flag Build Gaps

A PQ feature that only works in one build mode can rot quickly. Stage 1B should explicitly test both:

- without `pq-ml-dsa`
- with `pq-ml-dsa`

## Non-Goals

Stage 1B does **not** claim:

- hybrid enforcement is complete
- transport confidentiality is PQ-ready
- genesis/key distribution is production-ready
- key storage is hardened for production
- the network should switch to PQ by default

It only proves that Entangrid can host a real PQ signing backend cleanly.

## Recommended Next Step

Write the implementation plan for Stage 1B ML-DSA signing integration on [`stage-1/pq-integration`](../../../../).
