# PQ Stage 1J Session Lifecycle Hardening Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add node-local hybrid session TTL expiry so long-lived encrypted transport streams renew through fresh handshakes without changing higher-level message handling.

**Architecture:** Keep Stage 1J scoped to transport lifecycle. Add a node-local `session_ttl_millis` setting, track per-stream handshake completion time, expire hybrid streams lazily on the next post-handshake read/write, transparently reconnect outbound lanes before sending the pending frame, and close inbound expired streams cleanly. Leave deterministic transport behavior unchanged and defer in-stream rekeying.

**Tech Stack:** Rust, `tokio`, `serde`, `toml`, `entangrid-types`, `entangrid-network`, `entangrid-crypto`, `entangrid-node`, existing Stage 1G/1I handshake + encrypted framing path.

---

## File Structure

- Modify: `crates/entangrid-types/src/lib.rs`
  - add `NodeConfig.session_ttl_millis`
  - extend config round-trip/default tests
  - optionally extend `NodeMetrics` with a dedicated session-expiry counter if we want explicit observability
- Modify: `crates/entangrid-network/src/lib.rs`
  - add per-stream established-at timestamp and effective TTL
  - add outbound lazy expiry + transparent reconnect
  - add inbound expiry rejection before the next application frame
  - add focused expiry/reuse/disable tests
- Modify: `crates/entangrid-node/src/lib.rs`
  - update test/config builders that construct `NodeConfig` directly
  - add or reuse verification that higher layers stay unaware of expiry churn if needed
- Modify: `crates/entangrid-sim/src/lib.rs`
  - update localnet/config builders that construct `NodeConfig` directly
- Optionally modify: `README.md`
- Optionally modify: `docs/architecture.md`
- Optionally modify: `docs/protocol.md`
- Optionally modify: `crates/entangrid-types/README.md`
  - only if implementation details differ materially from the current Stage 1J design doc

## Task 1: Add Failing Config Tests For Session TTL

**Files:**
- Modify: `crates/entangrid-types/src/lib.rs`
- Modify: `crates/entangrid-sim/src/lib.rs`
- Modify: `crates/entangrid-node/src/lib.rs`

- [ ] **Step 1: Write failing `NodeConfig` tests for `session_ttl_millis`**

Add focused tests covering:

- omitted `session_ttl_millis` parses as `None`
- explicit `session_ttl_millis = 0` round-trips
- non-zero `session_ttl_millis` round-trips

Suggested test names:

```rust
#[test]
fn node_config_defaults_session_ttl_to_none_when_omitted() { /* ... */ }

#[test]
fn node_config_round_trips_explicit_session_ttl_values() { /* ... */ }
```

- [ ] **Step 2: Run the focused types tests and verify they fail**

Run:

```bash
cargo test -p entangrid-types session_ttl
```

Expected:

- compile or missing-field failures because `session_ttl_millis` does not exist yet

- [ ] **Step 3: Add `session_ttl_millis` to `NodeConfig` and update direct constructors**

In `crates/entangrid-types/src/lib.rs`:

- add `#[serde(default)] pub session_ttl_millis: Option<u64>`

Then update every direct `NodeConfig { ... }` constructor in:

- `crates/entangrid-types/src/lib.rs`
- `crates/entangrid-sim/src/lib.rs`
- `crates/entangrid-node/src/lib.rs`

Use `None` by default in tests/builders unless the scenario needs explicit expiry.

- [ ] **Step 4: Re-run focused config tests until green**

Run:

```bash
cargo test -p entangrid-types session_ttl
```

## Task 2: Add Failing Network Tests For Session Expiry

**Files:**
- Modify: `crates/entangrid-network/src/lib.rs`

- [ ] **Step 1: Write failing outbound and inbound expiry tests**

Add focused tests for:

- hybrid outbound stream reconnects after expiry before sending the pending frame
- non-expired hybrid outbound stream still reuses the same connection
- inbound expired stream closes before accepting another application frame
- `session_ttl_millis = 0` disables expiry
- deterministic/default streams remain unaffected

Suggested test names:

```rust
#[cfg(feature = "pq-ml-kem")]
#[tokio::test]
async fn hybrid_outbound_stream_reconnects_after_ttl_expiry() { /* ... */ }

#[cfg(feature = "pq-ml-kem")]
#[tokio::test]
async fn inbound_expired_hybrid_stream_rejects_next_application_frame() { /* ... */ }
```

Test guidance:

- use a very short TTL like `Some(1)` millisecond
- sleep just long enough to cross the TTL boundary
- assert on connection reuse versus reconnect through the existing listener-based tests

- [ ] **Step 2: Run the focused network tests and verify they fail**

Run:

```bash
cargo test -p entangrid-network ttl
cargo test -p entangrid-network expiry
```

Expected:

- failures because the network does not yet track session age or reconnect on expiry

## Task 3: Implement Effective Session TTL Semantics

**Files:**
- Modify: `crates/entangrid-network/src/lib.rs`
- Optionally modify: `crates/entangrid-types/src/lib.rs`

- [ ] **Step 1: Add helpers for effective TTL selection**

In `crates/entangrid-network/src/lib.rs`, add small helpers that compute:

- whether a session backend should participate in TTL expiry
- the effective TTL for a stream:
  - `None` config + deterministic backend => no expiry
  - `None` config + hybrid backend => `600_000` ms
  - `Some(0)` => expiry disabled
  - `Some(n)` => explicit TTL

Keep this logic transport-local so it does not bleed into consensus or crypto code.

- [ ] **Step 2: Extend per-stream state with lifecycle timestamps**

Add fields such as:

- `session_established_at_unix_millis`
- effective TTL or enough state to compute expiry cheaply

to the cached outbound stream state and the inbound handling path.

- [ ] **Step 3: Re-run a small compile-targeted network test**

Run:

```bash
cargo test -p entangrid-network deterministic_stream_keeps_plaintext_frames
```

Expected:

- compile green before the lazy-expiry behavior is wired in

## Task 4: Implement Lazy Outbound Expiry And Transparent Reconnect

**Files:**
- Modify: `crates/entangrid-network/src/lib.rs`

- [ ] **Step 1: Check cached outbound stream age before sending a post-handshake frame**

If the cached stream is expired:

- clear it locally
- do **not** count it as a handshake failure
- optionally increment a dedicated expiry counter if metrics are extended

- [ ] **Step 2: Reuse the existing outbound connect/handshake path**

After dropping an expired cached stream:

- reconnect exactly through the existing lane flow
- run a fresh handshake
- send the pending frame on the fresh stream

Do not bubble expiry to node logic.

- [ ] **Step 3: Re-run focused outbound expiry tests until green**

Run:

```bash
cargo test -p entangrid-network --features pq-ml-kem hybrid_outbound_stream_reconnects_after_ttl_expiry
cargo test -p entangrid-network --features pq-ml-kem outbound_lane_reuses_same_connection_for_same_peer
```

## Task 5: Implement Lazy Inbound Expiry And Clean Close

**Files:**
- Modify: `crates/entangrid-network/src/lib.rs`

- [ ] **Step 1: Check inbound session age before accepting the next application frame**

If the stream is expired:

- close it immediately
- do not decode the pending application frame
- treat it as expiry, not authentication failure

- [ ] **Step 2: Keep decrypt/auth failures separate**

Make sure:

- expiry does not increment handshake failure counters
- decrypt/auth errors still do

If you extend `NodeMetrics`, add a dedicated counter such as `session_expirations` instead of overloading failure counters.

- [ ] **Step 3: Re-run focused inbound expiry tests until green**

Run:

```bash
cargo test -p entangrid-network --features pq-ml-kem inbound_expired_hybrid_stream_rejects_next_application_frame
cargo test -p entangrid-network --features pq-ml-kem tampered_encrypted_frame_drops_stream
```

## Task 6: Final Verification And Documentation Touch-Up

**Files:**
- Modify: any touched Stage 1J files
- Optionally modify: `README.md`
- Optionally modify: `docs/architecture.md`
- Optionally modify: `docs/protocol.md`
- Optionally modify: `crates/entangrid-types/README.md`

- [ ] **Step 1: Update docs only if behavior differs materially from the current spec**

If docs need adjustment, describe:

- node-local `session_ttl_millis`
- hybrid default TTL
- outbound lazy reconnect semantics
- inbound expiry-close semantics

- [ ] **Step 2: Run the Stage 1J verification set**

Run:

```bash
cargo test -p entangrid-types
cargo test -p entangrid-network
cargo test -p entangrid-network --features pq-ml-kem
cargo test -p entangrid-node --features "pq-ml-dsa pq-ml-kem"
cargo fmt
git diff --check
```

- [ ] **Step 3: Commit the Stage 1J slice**

```bash
git add crates/entangrid-types/src/lib.rs crates/entangrid-network/src/lib.rs crates/entangrid-node/src/lib.rs crates/entangrid-sim/src/lib.rs README.md docs/architecture.md docs/protocol.md crates/entangrid-types/README.md
git commit -m "harden hybrid session lifecycle"
```

## Notes For The Implementer

- Keep Stage 1J focused on expiry plus reconnect, not rekeying.
- Measure age from handshake completion time, not last activity.
- Do not change message schemas or encrypted framing format.
- Keep deterministic/default transport behavior unchanged unless explicitly configured later.
- Expiry is an expected lifecycle event; do not record it as a transport/authentication failure.
