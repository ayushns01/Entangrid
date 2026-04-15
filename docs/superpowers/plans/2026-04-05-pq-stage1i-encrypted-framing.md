# PQ Stage 1I Encrypted Framing Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Protect every post-handshake frame body with authenticated encryption on hybrid session streams while preserving the existing plaintext handshake and application message formats.

**Architecture:** Keep Stage 1I tightly scoped to the transport boundary. Reuse the Stage 1G session handshake and session material, leave the outer `u32` frame length plaintext, and make `crates/entangrid-network` encrypt/decrypt serialized frame bytes automatically whenever the hybrid session backend is active. Defer encrypted handshake messages, hidden lengths, and rekeying.

**Tech Stack:** Rust, `tokio`, `bincode`, `entangrid-crypto`, `entangrid-network`, `entangrid-node`, optional `chacha20poly1305`, existing `pq-ml-kem` session material path.

---

## File Structure

- Modify: `crates/entangrid-crypto/Cargo.toml`
  - add feature-gated AEAD dependency wiring for encrypted framing
- Modify: `crates/entangrid-crypto/src/lib.rs`
  - add encrypted-frame helpers, nonce derivation, and AEAD tests
- Modify: `crates/entangrid-network/src/lib.rs`
  - add encrypted frame read/write flow after handshake
  - add stream-local counters and fail-closed behavior
- Modify: `crates/entangrid-network/Cargo.toml`
  - forward any new encrypted-framing dependency/feature requirements
- Modify: `crates/entangrid-node/Cargo.toml`
  - forward PQ transport feature coverage into node-side tests if needed
- Optionally modify: `README.md`
- Optionally modify: `docs/architecture.md`
- Optionally modify: `docs/protocol.md`
- Optionally modify: `crates/entangrid-crypto/README.md`
  - only if implementation details differ materially from the current transport docs

## Task 1: Add Crypto-Side Encrypted Framing Tests

**Files:**
- Modify: `crates/entangrid-crypto/src/lib.rs`

- [ ] **Step 1: Write failing AEAD helper tests**

Add focused tests for:

- encrypt/decrypt round-trip with the same session key and counter
- send/receive direction produce distinct nonces
- tampered ciphertext fails authentication
- replayed or wrong-counter ciphertext fails authentication

Suggested test names:

```rust
#[cfg(feature = "pq-ml-kem")]
#[test]
fn encrypted_frame_round_trips_with_matching_counter() { /* ... */ }

#[cfg(feature = "pq-ml-kem")]
#[test]
fn encrypted_frame_rejects_tampered_ciphertext() { /* ... */ }
```

- [ ] **Step 2: Run the focused crypto tests and verify they fail**

Run:

```bash
cargo test -p entangrid-crypto encrypted_frame
```

Expected:

- missing encrypted-frame helpers and/or compile failures because the AEAD layer does not exist yet

## Task 2: Implement Crypto-Side Encrypted Framing Helpers

**Files:**
- Modify: `crates/entangrid-crypto/Cargo.toml`
- Modify: `crates/entangrid-crypto/src/lib.rs`

- [ ] **Step 1: Add feature-gated AEAD dependency wiring**

In `crates/entangrid-crypto/Cargo.toml`:

- add `chacha20poly1305`
- gate it behind the same PQ transport/session feature path used for hybrid session streams

- [ ] **Step 2: Implement encrypted-frame helpers**

In `crates/entangrid-crypto/src/lib.rs`, add helpers that:

- derive a per-frame nonce from:
  - stream direction
  - monotonic `u64` counter
- derive associated data from:
  - frame direction
  - frame counter
  - compact session identifier from the transcript hash
- encrypt plaintext frame bytes into `ciphertext || tag`
- decrypt and authenticate encrypted frame bytes back into plaintext

Keep the session material source unchanged:

- reuse `SessionMaterial.session_key`
- reuse `SessionMaterial.transcript_hash`

- [ ] **Step 3: Re-run focused crypto tests until green**

Run:

```bash
cargo test -p entangrid-crypto encrypted_frame
cargo test -p entangrid-crypto --features "pq-ml-kem" encrypted_frame
```

## Task 3: Add Network-Side Failing Encrypted Framing Tests

**Files:**
- Modify: `crates/entangrid-network/src/lib.rs`

- [ ] **Step 1: Write failing transport tests for encrypted streams**

Add focused tests for:

- hybrid session streams encrypt the first post-handshake frame
- deterministic/default session streams still use plaintext framing
- tampered encrypted frame causes stream drop
- reconnect or fresh stream restarts counters and succeeds

Suggested test names:

```rust
#[cfg(feature = "pq-ml-kem")]
#[tokio::test]
async fn hybrid_stream_encrypts_post_handshake_frames() { /* ... */ }

#[cfg(feature = "pq-ml-kem")]
#[tokio::test]
async fn tampered_encrypted_frame_drops_stream() { /* ... */ }
```

- [ ] **Step 2: Run the focused network tests and verify they fail**

Run:

```bash
cargo test -p entangrid-network handshake
cargo test -p entangrid-network encrypted
```

Expected:

- failures because the network layer still reads and writes raw post-handshake frames

## Task 4: Implement Encrypted Read/Write Flow In The Network Layer

**Files:**
- Modify: `crates/entangrid-network/src/lib.rs`
- Modify: `crates/entangrid-network/Cargo.toml`
- Modify: `crates/entangrid-node/Cargo.toml`

- [ ] **Step 1: Add stream-local encrypted framing state**

In `crates/entangrid-network/src/lib.rs`, extend per-stream state with:

- whether encrypted framing is active
- send counter
- receive counter
- session material needed to encrypt/decrypt frame bodies

Enable that state automatically only when the hybrid session backend is active and the Stage 1G handshake has succeeded.

- [ ] **Step 2: Encrypt post-handshake writes**

Update the post-handshake write path to:

- serialize frame bytes exactly as today
- encrypt/authenticate them when encrypted framing is active
- write the plaintext `u32` length plus encrypted payload
- increment the send counter only after a successful write preparation

- [ ] **Step 3: Decrypt post-handshake reads**

Update the post-handshake read path to:

- read the plaintext `u32` length plus encrypted payload
- decrypt/authenticate when encrypted framing is active
- increment the receive counter only after successful decrypt/auth
- pass recovered plaintext bytes into the existing decode path

- [ ] **Step 4: Fail closed on decrypt/auth errors**

On any decrypt/auth or counter failure:

- record the failure in transport/session metrics if available
- close the stream immediately
- rely on the existing reconnect path instead of soft recovery

- [ ] **Step 5: Re-run focused network tests until green**

Run:

```bash
cargo test -p entangrid-network
cargo test -p entangrid-network --features "pq-ml-kem"
```

## Task 5: Node-Level Verification And Final Checks

**Files:**
- Modify: any touched Stage 1I files

- [ ] **Step 1: Run node-side verification on the PQ transport path**

Run:

```bash
cargo test -p entangrid-node
cargo test -p entangrid-node --features "pq-ml-dsa pq-ml-kem"
```

Confirm:

- higher-level message handling still works unchanged
- hybrid session transport tests continue to pass with encrypted framing

- [ ] **Step 2: Run the Stage 1I verification set**

Run:

```bash
cargo test -p entangrid-crypto --features "pq-ml-kem"
cargo test -p entangrid-network --features "pq-ml-kem"
cargo test -p entangrid-node --features "pq-ml-dsa pq-ml-kem"
cargo fmt
git diff --check
```

- [ ] **Step 3: Commit the Stage 1I slice**

```bash
git add crates/entangrid-crypto/Cargo.toml crates/entangrid-crypto/src/lib.rs crates/entangrid-network/Cargo.toml crates/entangrid-network/src/lib.rs crates/entangrid-node/Cargo.toml
git commit -m "encrypt hybrid transport frames"
```

## Notes For The Implementer

- Keep handshake messages plaintext in this stage.
- Do not change application message schemas or consensus logic.
- Do not hide lengths or add padding yet.
- Do not add rekeying or session rotation in Stage 1I.
- Make encrypted framing follow automatically from the hybrid session backend so operators cannot enable the handshake and accidentally keep plaintext transport.
