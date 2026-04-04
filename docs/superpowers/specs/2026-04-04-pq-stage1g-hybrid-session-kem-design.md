# Stage 1G PQ Hybrid Session KEM Design

**Date:** 2026-04-04
**Branch:** `stage-1/pq-integration`
**Status:** Drafted from approved design discussion

## Goal

Add a real, feature-gated, per-stream hybrid session handshake that combines deterministic session derivation with ML-KEM shared secret material, while keeping the current frame transport unchanged.

Stage 1G builds on:

- Stage 1 typed signature and identity foundations
- Stage 1B experimental ML-DSA signing support
- Stage 1D hybrid signature and identity support
- Stage 1E hybrid validator-signature enforcement
- Stage 1F strict hybrid localnet bootstrap

The goal is to move Entangrid from "PQ-capable signing/authentication" to "PQ-capable authenticated session establishment" without taking on encrypted framing or broader transport redesign in the same slice.

## Why This Stage Exists

Entangrid now has:

- typed signatures and typed public identities
- experimental ML-DSA signing behind `pq-ml-dsa`
- hybrid signing and hybrid validator enforcement
- a strict all-hybrid localnet bootstrap path

What it does **not** have yet is a real PQ-capable transport session handshake.

Today, session material is still derived through a deterministic local function in [`crates/entangrid-crypto/src/lib.rs`](../../../../crates/entangrid-crypto/src/lib.rs), and [`crates/entangrid-network/src/lib.rs`](../../../../crates/entangrid-network/src/lib.rs) calls that logic per message hash rather than performing one explicit authenticated handshake per TCP stream.

Stage 1G exists to correct that boundary:

- session establishment should happen once per stream
- session establishment should be authenticated by validator identities
- session establishment should derive hybrid secret material from both deterministic and PQ components
- current message framing should remain stable while this new boundary lands

## Scope

### In Scope

- add a feature-gated hybrid session/KEM path
- introduce a real per-stream handshake for outbound and inbound TCP streams
- authenticate the handshake with the existing validator signing identities
- derive session material from:
  - deterministic component
  - ML-KEM component
  - ordered transcript hash
- add separate session/KEM public identity types and node-local KEM key configuration
- keep the current persistent outbound lane and multi-frame inbound stream behavior
- fail closed on invalid handshake identity, signature, or KEM material
- add tests for handshake success, rejection, and reconnect behavior
- document the new boundary and its non-goals

### Out of Scope

- encrypted framing
- per-frame authentication tags
- rekeying or session rotation
- transaction, receipt, or service-evidence policy changes
- widening hybrid enforcement beyond current validator-originated objects
- merging the PQ branch back to `main`

## Architecture

### 1. Use A Real Per-Stream Handshake

Stage 1G should introduce a real handshake on TCP stream establishment instead of continuing to derive session material ad hoc from application-message hashes.

Recommended shape:

- outbound worker connects a TCP stream
- before any protocol frames are sent, the peers run one authenticated hybrid handshake
- inbound side completes that handshake before entering the existing multi-frame receive loop
- the resulting session material is cached in stream state and reused for the life of that stream

This is the right boundary because Entangrid already uses persistent per-peer outbound lanes and multi-frame inbound sessions. The session handshake should align with that connection model rather than being re-derived per message.

### 2. Bind Sessions To Validator Identity With Signed Transcripts

The handshake should be authenticated using the same validator signing identity model already introduced in earlier PQ stages.

Recommended rule:

- each side sends handshake material
- each side signs the handshake transcript with its configured validator signing backend
- transcript verification is mandatory before the stream is accepted

Why this is the right model:

- it reuses the signing/authentication infrastructure already in place
- it strongly binds transport sessions to validator identity
- it avoids inventing a weaker or parallel authentication story for the network layer

### 3. Use A Hybrid Session Secret

Stage 1G should use a hybrid session derivation model, not a PQ-only session model.

The final session material should be derived from:

- deterministic session component from the existing validator-secret path
- ML-KEM shared secret component
- transcript hash over the ordered handshake exchange

That means the session key is not just "the KEM output." It is a combined result that matches the staged hybrid rollout already used for signatures.

This is the right design because:

- it keeps the transport rollout aligned with the signing rollout
- it provides a compatible migration path instead of a sudden PQ-only transport boundary
- it makes the first session/KEM slice easier to compare against the current deterministic path

### 4. Keep Signing Identity And Session Identity Separate

Signing identity and session/KEM identity should remain separate concepts.

Recommended type direction in [`crates/entangrid-types/src/lib.rs`](../../../../crates/entangrid-types/src/lib.rs):

- keep `PublicIdentity` for signing/authentication
- add separate session/KEM-facing types, conceptually:
  - `SessionKeyScheme`
  - `SessionPublicIdentity`

Recommended config direction:

- keep node-local signing backend and signing key configuration as-is
- add separate node-local KEM private key configuration

Why:

- signing and transport encryption have different lifecycle needs
- future rotation policies will likely differ
- mixing them into one identity container would make later stages harder

The handshake should therefore:

- authenticate peers with signing identities
- derive transport session material from session/KEM identities

### 5. Keep Framing Unchanged In This Slice

Stage 1G should stop at authenticated hybrid session establishment.

It should **not** yet:

- encrypt frame payloads
- add per-frame AEAD tags
- add stream rekeying
- change protocol message framing

This is intentional.

The first priority is to create the right handshake and session boundary without destabilizing the transport fixes already landed on `main` and reused on this branch.

## Handshake Shape

Stage 1G should introduce an explicit two-message handshake.

### ClientHello

Should carry:

- initiator validator id
- session nonce
- initiator ephemeral ML-KEM public material
- session identity context
- signature over the hello transcript

### ServerHello

Should carry:

- responder validator id
- same session nonce
- responder ML-KEM response/ciphertext material
- responder session identity context
- signature over the negotiated transcript

### Session Derivation

After successful exchange and verification, both peers derive:

- deterministic component
- ML-KEM component
- final transcript hash
- final session key

The transcript hash should be emitted into the existing session-observation path so service accounting and metrics still have a stable session-level identifier.

## Crypto Boundary Changes

[`crates/entangrid-crypto/src/lib.rs`](../../../../crates/entangrid-crypto/src/lib.rs) should grow from a simple `open_session(...)` API into a handshake-oriented interface.

Recommended direction:

- preserve the default deterministic path for non-PQ builds
- add feature-gated ML-KEM support
- add transcript signing/verification helpers for handshake messages
- add explicit handshake roles or stages so network code does not have to know crypto internals

The important design boundary is:

- crypto layer constructs, verifies, and derives handshake state
- network layer runs the state machine and owns stream lifecycle

## Transport Flow

[`crates/entangrid-network/src/lib.rs`](../../../../crates/entangrid-network/src/lib.rs) should change at stream boundaries only.

### Outbound

- open TCP stream
- run handshake
- cache established session state
- send normal protocol frames over the already-established stream

### Inbound

- accept TCP stream
- complete handshake
- reject the stream immediately if handshake validation fails
- only after handshake success enter the existing frame-read loop

### Reconnect

- every fresh TCP stream performs a fresh handshake
- no session carryover across reconnects

This keeps the current outbound lane reuse model intact while making session establishment explicit and correct.

## Failure Handling

Handshake failure should be explicit and fail closed.

Examples:

- wrong validator id in transcript
- invalid initiator or responder transcript signature
- session public key mismatch against configured peer identity
- ML-KEM decode / encapsulate / decapsulate failure
- feature/config mismatch for KEM-enabled sessions

Recommended behavior:

- drop the stream immediately
- report a handshake failure through the existing metrics and event surfaces
- do not accept or process any application frame before handshake success

Where possible, startup should fail early for obviously broken local configuration, especially:

- hybrid session mode enabled without required cargo feature
- local KEM private key missing or malformed
- node-local KEM key does not match the configured validator session identity

## File And Config Shape

### Types

Modify:

- [`crates/entangrid-types/src/lib.rs`](../../../../crates/entangrid-types/src/lib.rs)

Expected additions:

- `SessionKeyScheme`
- `SessionPublicIdentity`
- optional validator-level public session identity in genesis/config
- node-local KEM config fields alongside the existing signing backend config

### Crypto

Modify:

- [`crates/entangrid-crypto/src/lib.rs`](../../../../crates/entangrid-crypto/src/lib.rs)

Expected additions:

- feature-gated ML-KEM signing/session support
- KEM key-file loading
- handshake message helpers
- hybrid deterministic + ML-KEM session derivation helpers

### Network

Modify:

- [`crates/entangrid-network/src/lib.rs`](../../../../crates/entangrid-network/src/lib.rs)

Expected additions:

- stream-establishment handshake logic
- stream-local session state
- failure reporting for handshake-stage errors

## Rollout Plan

### Phase 1: Type And Config Foundation

Deliverables:

- separate session identity types
- separate node-local KEM key config
- clear default behavior when session KEM feature is disabled

### Phase 2: Crypto Handshake Engine

Deliverables:

- feature-gated ML-KEM support
- handshake transcript signing and verification
- hybrid session derivation

### Phase 3: Network Integration

Deliverables:

- one handshake per stream
- established session state cached per stream
- reconnect performs fresh handshake
- application frames rejected before handshake success

### Phase 4: Documentation And Verification

Deliverables:

- protocol/docs updates
- operator notes for hybrid session mode
- tests for deterministic fallback and hybrid session path

## Testing Strategy

### Unit Tests

Add tests that prove:

- deterministic session path still works with the default build
- valid hybrid handshake derives matching session material on both sides
- invalid transcript signature is rejected
- mismatched session public identity is rejected
- malformed ML-KEM material is rejected

### Network Tests

Add tests that prove:

- outbound lane performs handshake once per stream
- inbound side rejects frames before handshake success
- reconnect performs a fresh handshake
- existing multi-frame inbound behavior still works after handshake integration

### Feature-Gated Verification

Run both:

- default test suite without the PQ session feature
- feature-enabled test suite with ML-KEM hybrid session support

### Smoke Goal

This stage does not require a full hybrid matrix.

The first smoke bar is:

- a feature-enabled local stream can establish one authenticated hybrid session
- then carry normal protocol frames over that stream

## Non-Goals

Stage 1G is not trying to finish PQ transport in one step.

It is **not**:

- encrypted framing
- rekeying
- full secure-channel semantics
- broader hybrid policy rollout
- performance gating for final production settings

Those can follow once the handshake boundary is stable.

## Success Criteria

Stage 1G is successful when:

- Entangrid supports a feature-gated, per-stream, mutually signed hybrid handshake
- signing identity and session/KEM identity are separate
- session material is derived from deterministic + ML-KEM components
- the existing transport framing remains stable after handshake success
- reconnects perform fresh handshakes
- invalid session identity or transcript state is rejected cleanly

At that point, the project will have completed the first real PQ transport milestone:

- PQ-capable signing/authentication
- PQ-capable hybrid session establishment

while still deferring encrypted framing and broader rollout policy to later stages.
