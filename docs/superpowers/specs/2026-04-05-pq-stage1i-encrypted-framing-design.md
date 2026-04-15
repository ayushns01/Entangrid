# PQ Stage 1I Encrypted Framing Design

## Goal

Protect every post-handshake transport frame with authenticated encryption while keeping the Stage 1G handshake itself in cleartext and leaving higher-level message encoding unchanged.

## Scope

Stage 1I is intentionally narrow:

- keep `SessionClientHello` and `SessionServerHello` plaintext
- encrypt and authenticate every later frame body on the stream
- preserve the existing `u32` frame-length prefix in cleartext
- use per-stream session material from Stage 1G
- keep encryption logic inside the transport layer

Out of scope:

- encrypted handshake messages
- hidden frame lengths or padding
- session rotation or rekeying
- widening PQ enforcement to more artifact types
- application-layer message format changes

## Architecture

Stage 1I should turn the existing Stage 1G session handshake into a real transport-security boundary.

Recommended flow:

- connect or accept a TCP stream
- perform the Stage 1G hybrid handshake in cleartext
- derive per-stream session material
- switch the stream into encrypted mode
- encrypt and authenticate every subsequent frame body before write
- decrypt and authenticate every subsequent frame body after read

The framing boundary stays simple:

- `u32 ciphertext_len`
- `ciphertext || auth_tag`

The transport layer owns framing security:

- `crates/entangrid-network` encrypts and decrypts frame bodies
- `crates/entangrid-crypto` provides AEAD helpers and nonce/AAD derivation
- `crates/entangrid-node` and higher layers continue to see the same serialized message bytes after decryption

## AEAD And Nonce Model

Stage 1I should use **ChaCha20-Poly1305** as the first transport AEAD.

Recommended model:

- one session key per stream from Stage 1G session material
- one send counter per stream direction
- one receive counter per stream direction
- nonce derived from:
  - stream direction
  - monotonic `u64` frame counter
- counters reset only when a new TCP stream performs a fresh handshake

This gives one clear rule:

- one handshake
- one stream key
- one ordered nonce sequence per direction

## Associated Data

Each encrypted frame should authenticate enough stream-local context to prevent replay or cross-stream reuse.

Recommended associated data:

- frame direction
- frame counter
- compact session identifier derived from the transcript hash

This makes authentication fail if ciphertext is replayed out of order, moved to the opposite direction, or replayed on another stream.

## Failure Handling

Stage 1I should be fail-closed.

If any post-handshake frame:

- fails decryption
- fails AEAD authentication
- arrives with an invalid counter expectation

then the transport must:

- record a framing/session failure
- close the stream immediately
- rely on the existing reconnect path to establish a fresh stream and handshake

There should be no soft recovery inside a compromised or desynchronized stream.

## Rollout Boundary

Encrypted framing should follow directly from the selected session backend.

- deterministic/default session backend:
  - existing plaintext framing remains unchanged
- hybrid session backend:
  - handshake remains plaintext
  - encrypted framing is enabled automatically for all later frames

This avoids a split-brain operator state where a hybrid handshake succeeds but transport still runs in plaintext.

## Transport Integration

Stage 1I should change only the point where serialized frame bytes cross the stream boundary.

Recommended implementation boundary:

- write path:
  - serialize message bytes exactly as today
  - encrypt/authenticate those bytes if the stream is in encrypted mode
  - write `u32` length plus ciphertext payload
- read path:
  - read `u32` length plus ciphertext payload
  - decrypt/authenticate if the stream is in encrypted mode
  - hand the recovered plaintext bytes to existing decode logic

This keeps:

- message structs unchanged
- node logic unchanged
- consensus logic unchanged

## Verification

Stage 1I should prove:

1. default deterministic transport still behaves exactly as before
2. the first post-handshake application frame is encrypted on hybrid streams
3. ordered frames decrypt correctly with matching counters
4. tampered ciphertext fails authentication and drops the stream
5. replayed or misordered ciphertext fails and drops the stream
6. reconnect establishes a fresh stream, fresh handshake, and fresh counters
7. node/network tests still pass above the new transport boundary

Recommended tests:

- crypto tests for nonce derivation and ChaCha20-Poly1305 frame round-trips
- network tests for encrypted write/read flow after handshake
- network tests for tamper failure and replay failure
- network tests for reconnect behavior
- node tests that exercise hybrid session transport paths without changing message semantics

## Success Criteria

Entangrid can establish a hybrid session in cleartext, then automatically protect every subsequent transport frame with per-stream authenticated encryption, while keeping the existing application message formats and reconnect behavior intact.
