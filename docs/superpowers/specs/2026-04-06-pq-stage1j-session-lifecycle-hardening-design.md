# PQ Stage 1J Session Lifecycle Hardening Design

## Goal

Add predictable transport-local session turnover for hybrid session lanes by expiring long-lived streams and reconnecting through a fresh handshake, without changing message schemas or introducing rekeying yet.

## Scope

Stage 1J is intentionally narrow:

- add node-local session TTL configuration
- track session age from handshake completion time
- expire hybrid transport streams lazily on the next post-handshake read or write
- reconnect transparently on outbound expiry before sending the pending frame
- close inbound expired streams and require the peer to reconnect

Out of scope:

- in-stream rekeying
- session rotation based on frame counters
- encrypted handshake messages
- traffic padding or length hiding
- widening hybrid enforcement to more artifact types

## Architecture

Stage 1J should harden the existing Stage 1I encrypted transport without changing the transport boundary.

Recommended flow:

- a stream completes the Stage 1G handshake
- the stream records handshake completion time
- the stream continues using Stage 1I encrypted framing
- before the next post-handshake read or write, the transport checks session age against the configured TTL
- if the session is expired:
  - outbound side drops the cached stream locally, reconnects, handshakes again, and sends the pending frame on the fresh stream
  - inbound side closes the expired stream immediately and requires the peer to reconnect

This keeps responsibilities clear:

- `crates/entangrid-types` owns the node-local TTL setting
- `crates/entangrid-network` owns expiry checks and reconnect behavior
- `crates/entangrid-crypto` keeps owning session material and encrypted framing helpers
- `crates/entangrid-node` remains unaware of session lifecycle details

## Config Model

Session lifecycle policy should stay node-local.

Recommended `NodeConfig` addition:

- `session_ttl_millis: Option<u64>`

Recommended semantics:

- `None`
  - deterministic/default session backend: keep current behavior
  - hybrid session backend: use a safe built-in default TTL
- `Some(0)`
  - disable expiry explicitly
- `Some(n)`
  - expire sessions after `n` milliseconds of age

Recommended first default:

- hybrid session backend default TTL: `600_000` milliseconds (10 minutes)
- deterministic/default backend: unchanged unless explicitly configured later

This keeps transport lifecycle tunable without coupling it to genesis or consensus-wide policy.

## Expiry Semantics

Session age should be measured from **handshake completion time**, not last activity time.

Reasons:

- simpler and more predictable than idle-based expiry
- avoids hidden differences between busy and quiet streams
- gives operators a clear upper bound on session lifetime

Outbound expiry:

- checked immediately before sending a post-handshake frame
- expired cached stream is dropped locally
- expiry is treated as an expected lifecycle event, not a handshake failure
- a fresh connection and handshake are performed transparently
- the pending frame is then sent on the new stream

Inbound expiry:

- checked immediately before accepting the next post-handshake application frame
- expired stream is closed
- no more application frames are accepted on that stream
- peer must reconnect and complete a fresh handshake

## Metrics And Failure Semantics

Stage 1J should distinguish **expiry** from **error**.

Expiry events:

- are expected lifecycle turnover
- should not increment handshake failure counters
- should increment dedicated session-expiry or session-rotation counters if metrics are extended

Transport/authentication failures:

- invalid ciphertext
- bad authentication tag
- counter mismatch
- malformed encrypted frame

These remain real failure events and should continue to count as transport/session failures.

This separation keeps observability honest:

- frequent expiry means policy tuning
- frequent auth failure means breakage or attack

## Transport Integration

Stage 1J should extend the existing per-stream state in `crates/entangrid-network`.

Recommended additions:

- `session_established_at_unix_millis`
- effective TTL for the stream

Recommended behavior:

- cached outbound stream:
  - if not expired, reuse as today
  - if expired, clear the cached stream and reconnect through the existing lane path
- inbound stream:
  - if not expired, continue reading application frames
  - if expired, close before accepting the next application frame

The Stage 1I framing boundary stays unchanged:

- handshake plaintext
- outer frame length plaintext
- post-handshake frame body encrypted on hybrid lanes

## Verification

Stage 1J should prove:

1. hybrid outbound stream reconnects automatically after TTL expiry
2. hybrid outbound stream still reuses a non-expired cached connection
3. inbound expired stream is closed before another application frame is accepted
4. `session_ttl_millis = 0` disables expiry
5. omitted TTL uses the hybrid default
6. deterministic/default transport remains unchanged
7. decrypt/auth failures still behave as transport failures rather than expiry events

Recommended tests:

- type/config tests for `NodeConfig.session_ttl_millis`
- network tests for outbound lazy expiry and transparent reconnect
- network tests for inbound expiry rejection
- network tests for explicit TTL disable
- node tests proving higher layers stay unaware of expiry/reconnect churn

## Success Criteria

Entangrid can bound the lifetime of hybrid transport sessions with a node-local TTL, transparently renew outbound sessions by reconnecting through a fresh handshake, and reject inbound post-expiry traffic cleanly, all without changing higher-level protocol messages or introducing rekeying yet.
