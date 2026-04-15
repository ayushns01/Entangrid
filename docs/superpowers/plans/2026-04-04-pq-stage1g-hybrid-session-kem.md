# PQ Stage 1G Hybrid Session KEM Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add a feature-gated, per-stream, mutually signed hybrid session handshake that derives session material from deterministic and ML-KEM components while keeping normal frame transport unchanged.

**Architecture:** Keep Stage 1G tightly scoped to the session-establishment boundary. Introduce separate session identity/config types in `entangrid-types`, add a hybrid handshake engine in `entangrid-crypto`, and wire that handshake into stream establishment in `entangrid-network` without changing the existing post-handshake frame loop. Defer encrypted framing and simulator KEM bootstrap to later stages.

**Tech Stack:** Rust, `serde`, `bincode`, `tokio`, `entangrid-types`, `entangrid-crypto`, `entangrid-network`, `entangrid-node`, optional experimental RustCrypto `ml-kem` dependency behind a new cargo feature, existing `pq-ml-dsa` signing path for transcript authentication.

---

## File Structure

### Shared types and config

- Modify: `crates/entangrid-types/src/lib.rs`
  - add session identity/config types
  - add handshake wire structs shared by crypto and network
  - add TOML and binary round-trip tests
- Modify: `crates/entangrid-crypto/src/lib.rs`
- Modify: `crates/entangrid-network/src/lib.rs`
- Modify: `crates/entangrid-node/src/lib.rs`
- Modify: `crates/entangrid-sim/src/lib.rs`
  - update fixture builders and config constructors that instantiate `ValidatorConfig` or `NodeConfig` directly

### Crypto backend and handshake engine

- Modify: `crates/entangrid-crypto/src/lib.rs`
  - add session backend selection and config validation
  - add KEM key-file loading and session identity matching
  - replace the simple `open_session(...)` boundary with explicit handshake helpers
  - add unit tests for deterministic fallback and hybrid session derivation

- Modify: `crates/entangrid-crypto/Cargo.toml`
  - add optional `ml-kem` dependency and feature wiring

### Transport integration

- Modify: `crates/entangrid-network/src/lib.rs`
  - run the handshake once when a stream is established
  - reject application frames before handshake success
  - cache established session state per stream
  - preserve outbound-lane reuse and inbound multi-frame behavior

- Modify: `crates/entangrid-network/Cargo.toml`
  - forward the new session/KEM feature into the network crate

- Modify: `crates/entangrid-node/Cargo.toml`
  - forward the new session/KEM feature into node integration tests

### Documentation

- Modify: `README.md`
- Modify: `docs/architecture.md`
- Modify: `docs/protocol.md`
- Modify: `crates/entangrid-crypto/README.md`
- Modify: `crates/entangrid-types/README.md`
  - document the handshake boundary, config shape, feature flag, and non-goals

## Task 1: Add Session Identity, Handshake, And Config Types

**Files:**
- Modify: `crates/entangrid-types/src/lib.rs`
- Modify: `crates/entangrid-crypto/src/lib.rs`
- Modify: `crates/entangrid-network/src/lib.rs`
- Modify: `crates/entangrid-node/src/lib.rs`
- Modify: `crates/entangrid-sim/src/lib.rs`
- Test: `crates/entangrid-types/src/lib.rs`

- [ ] **Step 1: Write failing type/config tests**

Add focused tests for:

- `SessionKeyScheme` round-trip through TOML and bincode
- `SessionPublicIdentity` single and hybrid round-trip behavior
- `NodeConfig` defaults for session backend and session key path
- `ValidatorConfig` carrying a separate `session_public_identity`
- handshake wire structs round-tripping through bincode

Suggested test shapes:

```rust
#[test]
fn session_backend_defaults_to_deterministic_when_omitted() {
    let parsed: NodeConfig = toml::from_str(MINIMAL_NODE_CONFIG).unwrap();
    assert_eq!(parsed.session_backend, SessionBackendKind::DevDeterministic);
    assert_eq!(parsed.session_key_path, None);
}

#[test]
fn hybrid_session_public_identity_round_trips() {
    let identity = SessionPublicIdentity::try_hybrid(vec![
        SessionPublicIdentityComponent {
            scheme: SessionKeyScheme::DevDeterministic,
            bytes: vec![1, 2, 3],
        },
        SessionPublicIdentityComponent {
            scheme: SessionKeyScheme::MlKem,
            bytes: vec![4, 5, 6],
        },
    ]).unwrap();
    let bytes = bincode::serde::encode_to_vec(&identity, bincode::config::standard()).unwrap();
    let (decoded, _): (SessionPublicIdentity, usize) =
        bincode::serde::decode_from_slice(&bytes, bincode::config::standard()).unwrap();
    assert_eq!(decoded, identity);
}
```

- [ ] **Step 2: Run the focused types tests and verify they fail**

Run:

```bash
cargo test -p entangrid-types session_
```

Expected:

- compile failures or missing-field failures because the new session types/config fields do not exist yet

- [ ] **Step 3: Implement the minimal shared types**

In `crates/entangrid-types/src/lib.rs`, add:

- `SessionKeyScheme`
- `SessionPublicIdentityComponent`
- `SessionPublicIdentity`
- `SessionBackendKind`
- `ValidatorConfig.session_public_identity`
- `NodeConfig.session_backend`
- `NodeConfig.session_key_path`
- handshake structs such as:
  - `SessionClientHello`
  - `SessionServerHello`

Recommended shapes:

```rust
pub enum SessionBackendKind {
    DevDeterministic,
    HybridDeterministicMlKemExperimental,
}

pub struct SessionClientHello {
    pub initiator_validator_id: ValidatorId,
    pub nonce: HashBytes,
    pub kem_public_material: Vec<u8>,
    pub session_identity: SessionPublicIdentity,
    pub signature: TypedSignature,
}
```

Keep serialization behavior aligned with the existing typed signature/public identity patterns:

- backward-friendly single-form encoding where reasonable
- explicit hybrid container support

- [ ] **Step 4: Re-run the focused types tests until green**

Run:

```bash
cargo test -p entangrid-types session_
```

- [ ] **Step 5: Commit the shared-type slice**

```bash
git add crates/entangrid-types/src/lib.rs crates/entangrid-crypto/src/lib.rs crates/entangrid-network/src/lib.rs crates/entangrid-node/src/lib.rs crates/entangrid-sim/src/lib.rs
git commit -m "add session identity and handshake types"
```

## Task 2: Add Feature Wiring And Crypto-Side Session Backend Validation

**Files:**
- Modify: `crates/entangrid-crypto/Cargo.toml`
- Modify: `crates/entangrid-network/Cargo.toml`
- Modify: `crates/entangrid-node/Cargo.toml`
- Modify: `crates/entangrid-crypto/src/lib.rs`
- Test: `crates/entangrid-crypto/src/lib.rs`

- [ ] **Step 1: Write failing backend-factory and config-validation tests**

Add tests for:

- hybrid session backend requires the `pq-ml-kem` feature
- hybrid session backend requires `session_key_path`
- local KEM key file must match `validator.session_public_identity`
- deterministic session backend still accepts the current default config

Suggested test names:

```rust
#[cfg(not(feature = "pq-ml-kem"))]
#[test]
fn hybrid_session_backend_requires_pq_kem_feature() { /* ... */ }

#[cfg(feature = "pq-ml-kem")]
#[test]
fn backend_factory_rejects_mismatched_session_public_identity() { /* ... */ }
```

- [ ] **Step 2: Run the focused crypto tests and verify they fail**

Run:

```bash
cargo test -p entangrid-crypto session_backend
```

Expected:

- failures because session backend selection, KEM key loading, and validation are not implemented yet

- [ ] **Step 3: Add feature wiring and minimal validation**

In the Cargo manifests:

- add `pq-ml-kem` to `crates/entangrid-crypto/Cargo.toml`
- forward it through `crates/entangrid-network/Cargo.toml`
- forward it through `crates/entangrid-node/Cargo.toml`

In `crates/entangrid-crypto/src/lib.rs`:

- add feature-gated RustCrypto `ml-kem` dependency use
- add a key-file format for session private/public KEM material
- extend `build_crypto_backend(...)` so it validates:
  - `session_backend`
  - `session_key_path`
  - session public identity match

Recommended key-file direction:

```rust
#[cfg(feature = "pq-ml-kem")]
#[derive(Clone, Debug, Serialize, Deserialize)]
struct MlKemSessionKeyFile {
    decapsulation_key: Vec<u8>,
    encapsulation_key: Vec<u8>,
}
```

- [ ] **Step 4: Re-run focused crypto tests until green**

Run:

```bash
cargo test -p entangrid-crypto session_backend
cargo test -p entangrid-crypto --features pq-ml-kem session_backend
```

- [ ] **Step 5: Commit the feature-wiring and validation slice**

```bash
git add crates/entangrid-crypto/Cargo.toml crates/entangrid-network/Cargo.toml crates/entangrid-node/Cargo.toml crates/entangrid-crypto/src/lib.rs
git commit -m "add hybrid session backend validation"
```

## Task 3: Implement The Crypto Handshake Engine

**Files:**
- Modify: `crates/entangrid-crypto/src/lib.rs`
- Modify: `crates/entangrid-types/src/lib.rs`
- Test: `crates/entangrid-crypto/src/lib.rs`

- [ ] **Step 1: Write failing handshake-engine tests**

Add unit tests proving:

- deterministic session handshake still derives matching session material on both sides
- hybrid session handshake derives matching session material on both sides
- invalid client transcript signature is rejected
- invalid server transcript signature is rejected
- responder rejects a peer whose advertised `SessionPublicIdentity` does not match genesis/config
- malformed or mismatched KEM material is rejected

Suggested test shapes:

```rust
#[test]
fn deterministic_session_handshake_round_trips() {
    let client = backend_for_validator(1);
    let server = backend_for_validator(2);
    let hello = client.build_client_hello(1, 2, [7; 32]).unwrap();
    let (server_hello, server_session) = server.accept_client_hello(2, &hello).unwrap();
    let client_session = client.finalize_client_session(1, 2, &hello, &server_hello).unwrap();
    assert_eq!(client_session, server_session);
}
```

- [ ] **Step 2: Run the focused handshake tests and verify they fail**

Run:

```bash
cargo test -p entangrid-crypto handshake
cargo test -p entangrid-crypto --features pq-ml-kem handshake
```

Expected:

- failures because the explicit handshake helpers do not exist yet

- [ ] **Step 3: Replace the simple session API with explicit handshake helpers**

In `crates/entangrid-crypto/src/lib.rs`:

- evolve `HandshakeProvider` away from only:

```rust
fn open_session(...) -> Result<SessionMaterial>;
```

- toward explicit staged helpers, for example:

```rust
fn build_client_hello(
    &self,
    local_validator_id: ValidatorId,
    peer_validator_id: ValidatorId,
    nonce: HashBytes,
) -> Result<PreparedClientHandshake>;

fn accept_client_hello(
    &self,
    local_validator_id: ValidatorId,
    hello: &SessionClientHello,
) -> Result<(SessionServerHello, SessionMaterial)>;

fn finalize_client_session(
    &self,
    local_validator_id: ValidatorId,
    peer_validator_id: ValidatorId,
    prepared: &PreparedClientHandshake,
    server_hello: &SessionServerHello,
) -> Result<SessionMaterial>;
```

Implementation rules:

- deterministic path remains available for default builds
- hybrid path derives:
  - deterministic component
  - ML-KEM component
  - final transcript hash
  - final session key
- transcript signatures use the configured validator signing backend
- both client and responder must validate the peer `SessionPublicIdentity` from genesis/config before accepting handshake state

- [ ] **Step 4: Re-run focused handshake tests until green**

Run:

```bash
cargo test -p entangrid-crypto handshake
cargo test -p entangrid-crypto --features pq-ml-kem handshake
```

- [ ] **Step 5: Commit the crypto handshake slice**

```bash
git add crates/entangrid-crypto/src/lib.rs crates/entangrid-types/src/lib.rs
git commit -m "add hybrid session handshake engine"
```

## Task 4: Integrate The Handshake Into Stream Establishment

**Files:**
- Modify: `crates/entangrid-network/src/lib.rs`
- Test: `crates/entangrid-network/src/lib.rs`

- [ ] **Step 1: Write failing transport tests**

Add tests proving:

- outbound lane performs one handshake before first frame on a new stream
- inbound side rejects application frames before handshake success
- reconnect performs a fresh handshake and derives a fresh transcript
- bad handshake id/signature/KEM cases emit `NetworkEvent::SessionFailed`
- bad handshake id/signature/KEM cases increment `NodeMetrics.handshake_failures`
- existing same-stream multi-frame behavior still works after handshake integration

Suggested test names:

```rust
#[tokio::test(flavor = "current_thread")]
async fn outbound_lane_performs_handshake_once_per_stream() { /* ... */ }

#[tokio::test(flavor = "current_thread")]
async fn inbound_rejects_frames_before_handshake_success() { /* ... */ }

#[tokio::test(flavor = "current_thread")]
async fn reconnect_replays_full_handshake_before_resuming_frames() { /* ... */ }

#[tokio::test(flavor = "current_thread")]
async fn invalid_handshake_emits_session_failed_and_increments_metrics() { /* ... */ }
```

- [ ] **Step 2: Run the focused network tests and verify they fail**

Run:

```bash
cargo test -p entangrid-network outbound_lane
cargo test -p entangrid-network inbound_session
```

Expected:

- failures because the network code currently writes signed envelopes directly without a handshake preamble

- [ ] **Step 3: Implement stream-establishment handshake integration**

In `crates/entangrid-network/src/lib.rs`:

- on outbound connect:
  - connect TCP stream
  - run handshake
  - cache established session state alongside the stream
  - only then write application frames

- on inbound accept:
  - complete handshake before frame reads
  - reject and close stream on handshake error
  - emit `SessionObserved` only after handshake success

Recommended state direction:

```rust
struct EstablishedStream {
    stream: TcpStream,
    session: SessionMaterial,
    peer_validator_id: ValidatorId,
}
```

Important behavior:

- do not accept any `SignedEnvelope` frame before handshake success
- keep one handshake per TCP stream
- keep the existing multi-frame read/write loop after handshake success
- surface handshake rejection through the existing `SessionFailed` event path and `handshake_failures` metrics

- [ ] **Step 4: Re-run focused network tests until green**

Run:

```bash
cargo test -p entangrid-network outbound_lane
cargo test -p entangrid-network inbound_session
```

- [ ] **Step 5: Commit the transport integration slice**

```bash
git add crates/entangrid-network/src/lib.rs
git commit -m "run hybrid session handshake on stream connect"
```

## Task 5: Document The New Session Boundary

**Files:**
- Modify: `README.md`
- Modify: `docs/architecture.md`
- Modify: `docs/protocol.md`
- Modify: `crates/entangrid-crypto/README.md`
- Modify: `crates/entangrid-types/README.md`

- [ ] **Step 1: Update docs for Stage 1G**

Add concise documentation covering:

- Stage 1G introduces feature-gated hybrid session establishment
- signing identity and session identity are separate
- the current slice adds handshake-only, not encrypted framing
- deterministic transport remains the default path
- simulator bootstrap for KEM material is still a later slice unless explicitly added later

- [ ] **Step 2: Run doc hygiene checks**

Run:

```bash
git diff --check
```

Expected:

- no whitespace or formatting issues

- [ ] **Step 3: Commit the docs slice**

```bash
git add README.md docs/architecture.md docs/protocol.md crates/entangrid-crypto/README.md crates/entangrid-types/README.md
git commit -m "document hybrid session kem boundary"
```

## Task 6: Full Verification Before Handoff

**Files:**
- Modify: none required
- Test: `crates/entangrid-types/src/lib.rs`
- Test: `crates/entangrid-crypto/src/lib.rs`
- Test: `crates/entangrid-network/src/lib.rs`
- Test: `crates/entangrid-node/src/lib.rs`

- [ ] **Step 1: Run the default verification suite**

Run:

```bash
cargo test -p entangrid-types -p entangrid-crypto -p entangrid-network -p entangrid-node
```

Expected:

- PASS with the deterministic session path unchanged

- [ ] **Step 2: Run the feature-enabled session/KEM verification suite**

Run:

```bash
cargo test -p entangrid-crypto --features "pq-ml-dsa pq-ml-kem"
cargo test -p entangrid-network --features "pq-ml-kem"
cargo test -p entangrid-node --features "pq-ml-dsa pq-ml-kem"
```

Expected:

- PASS with the hybrid handshake path enabled

- [ ] **Step 3: Run formatting and diff hygiene**

Run:

```bash
cargo fmt
git diff --check
```

Expected:

- formatter clean
- no diff-check issues

- [ ] **Step 4: Commit any final verification-driven fixes**

```bash
git add crates/entangrid-types/src/lib.rs crates/entangrid-crypto/src/lib.rs crates/entangrid-network/src/lib.rs README.md docs/architecture.md docs/protocol.md crates/entangrid-crypto/README.md crates/entangrid-types/README.md
git commit -m "finish hybrid session kem groundwork"
```

## Notes For The Implementer

- Keep Stage 1G narrowly scoped to the handshake boundary. Do not add encrypted framing in this plan.
- Reuse the existing typed-signature verification path for transcript authentication instead of inventing a second identity system.
- Fail closed on any transcript, identity, or KEM mismatch.
- Preserve the existing transport behavior after handshake success.
- If simulator-generated KEM key files become necessary for testing, add them in a follow-up stage rather than broadening this plan mid-flight.
