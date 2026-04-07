# PQ Stage 1 Status

This document records the current end-state of `stage-1/pq-integration` as of April 7, 2026.

## Stage 1 Scope

Stage 1 is the first mergeable post-quantum integration milestone for Entangrid.

It now includes:

- scheme-aware signatures, public identities, and session identities
- experimental ML-DSA signing behind `pq-ml-dsa`
- experimental hybrid deterministic + ML-DSA signing
- strict hybrid enforcement for:
  - validator startup identities
  - transactions
  - blocks
  - proposal votes
  - relay receipts
  - service attestations
- service aggregate enforcement transitively through validated embedded service attestations
- experimental hybrid deterministic + ML-KEM session establishment behind `pq-ml-kem`
- encrypted post-handshake framing
- hybrid session TTL expiry and transparent reconnect
- strict hybrid localnet bootstrap through `entangrid-sim`

## Explicitly Deferred

These are intentionally not blockers for the Stage 1 merge:

- in-stream rekeying or counter-driven session rotation
- encrypted handshake messages
- hidden frame lengths, padding, or richer traffic-shaping defenses
- production-strength key lifecycle and operator tooling
- production-strength PQ backend selection beyond the current experimental ML-DSA / ML-KEM paths
- full hybrid performance matrix tuning beyond the current smoke and focused validation coverage

Those belong to a later PQ hardening milestone rather than the first mergeable integration slice.

## Final Validation

The current branch was verified with:

```bash
cargo test -p entangrid-crypto --features "pq-ml-dsa pq-ml-kem"
cargo test -p entangrid-network --features pq-ml-kem
cargo test -p entangrid-node --features "pq-ml-dsa pq-ml-kem"
cargo test -p entangrid-sim --features "pq-ml-dsa pq-ml-kem"
cargo test -p entangrid-sim --features "pq-ml-dsa pq-ml-kem" hybrid_enforcement_localnet_boot_smoke_test -- --ignored
```

Notes:

- the `entangrid-network` and ignored `entangrid-sim` smoke tests require real localhost sockets
- the simulator smoke test boots a strict hybrid localnet end to end
- `entangrid-node` also passed without PQ features during the transaction-enforcement closeout work

## Merge Recommendation

`stage-1/pq-integration` is ready to merge as the Stage 1 PQ milestone.

The branch should be merged with the following expectations:

- Stage 1 establishes a real hybrid PQ-capable signing and transport path
- Stage 1 is not the final transport-hardening milestone
- later work should focus on:
  - rekeying and session rotation
  - stronger traffic analysis resistance
  - production backend hardening
  - broader hybrid performance characterization
