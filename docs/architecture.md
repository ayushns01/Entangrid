# Architecture

## Design Summary

`Entangrid` is designed as a research blockchain whose main experiment is service-coupled consensus.

Instead of treating networking as a side detail, the protocol records and scores how validators participate in message delivery. Validators do not earn better proposer odds simply by holding stake or opening idle connections. They must maintain secure sessions and produce verifiable evidence that they forwarded useful traffic within deadline windows.

Current status note:

- `main` now carries the active V2-focused architecture work behind `consensus_v2`
- the baseline receipt-driven path is still available when `consensus_v2` is disabled and is preserved as the benchmark line on `codex/consensus-v1`
- committee-attested service evidence, certified sync, and QC-dominant branch choice are now live on `main`
- healthy `6/7/8` bursty runs now repeatedly shut down structurally on one tip, and certified sync now ignores stale certified suffixes instead of downgrading local state
- startup replay suppression and a startup sync barrier now prevent restarted nodes from replaying stale proposer slots before they catch up
- stale-node restart recovery is now fixed enough on `main`: restarted nodes advertise and serve only their certified frontier during recovery, then proactively pull catch-up while peers remain ahead
- the next runtime step is full-matrix validation and hard acceptance gates, not another recovery redesign
- the redesign and stabilization work are documented in [superpowers/plans/2026-03-25-entangrid-consensus-v2.md](superpowers/plans/2026-03-25-entangrid-consensus-v2.md), [superpowers/plans/entangrid-consensus-v2-status.md](superpowers/plans/entangrid-consensus-v2-status.md), and [superpowers/plans/2026-03-27-entangrid-v2-stabilization.md](superpowers/plans/2026-03-27-entangrid-v2-stabilization.md)

## High-Level Components

### 1. Identity Layer

Responsibilities:

- validator identity keys
- transaction signing keys
- epoch metadata
- stake registry for the fixed validator set

Notes:

- validator identity should be long-lived
- session keys should be ephemeral
- transaction authorization should stay separate from transport encryption

### 2. Crypto Layer

Responsibilities:

- post-quantum key establishment for peer sessions
- post-quantum signatures for transactions, blocks, and receipts
- KDF expansion for transport keys
- nonce management and replay protection helpers

The crypto layer should expose a narrow interface so the rest of the codebase does not depend on primitive-specific details.

Current PQ Stage 1 detail on `stage-1/pq-integration`:

- signing and verification are being moved to typed signatures with explicit scheme metadata
- validator config is moving to typed public identities instead of untyped byte blobs
- node config now carries node-local signing backend selection instead of treating signer choice as a consensus feature flag
- the first real experimental PQ backend is ML-DSA-65 behind the `pq-ml-dsa` cargo feature
- first-class hybrid signature and identity containers now exist for the core signed objects
- verification is permissive during rollout: deterministic-only, ML-DSA-only, and hybrid signatures can all validate against the right identities
- an opt-in `require_hybrid_validator_signatures` flag now adds network-wide startup enforcement for hybrid validator identities and runtime enforcement for block, proposal-vote, relay-receipt, and service-attestation signatures
- Stage 1F plus Stage 1H now provide a strict hybrid localnet bootstrap path via `init-localnet --hybrid-enforcement`; it requires a `pq-ml-dsa pq-ml-kem` build, writes hybrid validator identities plus `session_public_identity` into genesis, generates one ML-DSA signing key file plus one ML-KEM session key file per node, selects `HybridDeterministicMlDsaExperimental` plus `HybridDeterministicMlKemExperimental`, enables `require_hybrid_validator_signatures = true`, and forces `consensus_v2 = true`
- this Stage 1F slice is operational/bootstrap coverage, not the full hybrid performance matrix
- Stage 1G now adds a feature-gated hybrid session handshake behind `pq-ml-kem`
- session identity is now separate from signing identity in config and validator metadata
- each TCP stream performs one mutually signed handshake and derives session material from deterministic + ML-KEM components before normal frames flow
- deterministic session establishment remains the default path when `pq-ml-kem` is disabled
- Stage 1I now turns those hybrid sessions into encrypted transport lanes by protecting every post-handshake frame body while keeping the handshake and outer frame length plaintext
- Stage 1J now adds node-local hybrid session TTL expiry via `NodeConfig.session_ttl_millis`; omitted TTL uses a 10 minute default on hybrid lanes, `0` disables expiry, outbound lanes reconnect transparently on expiry, and inbound lanes close expired streams before the next application frame
- rekeying and richer traffic-shaping still come later

### 3. Network Layer

Responsibilities:

- peer discovery for localnet
- authenticated session setup
- encrypted framing on hybrid session lanes
- gossip for transactions and blocks
- witness pulse delivery
- backpressure and connection management

The network layer should not know consensus rules beyond message priority classes.

Current Stage 1I note:

- the network layer now performs one handshake per TCP stream before normal protocol frames
- handshake success records session metadata and transcript hash for metrics/events
- when the hybrid session backend is active, every later frame body is ChaCha20-Poly1305 protected
- the deterministic/default path still keeps plaintext post-handshake framing
- Stage 1J adds transport-local turnover on top of that:
  - hybrid lanes use a 10 minute TTL by default when `session_ttl_millis` is omitted
  - outbound cached streams reconnect lazily on the next send after expiry
  - inbound streams close before delivering the next post-expiry application frame
  - deterministic lanes still skip TTL-driven expiry

### 4. Witness Engine

Responsibilities:

- derive rotating witness assignments from epoch randomness
- schedule relay obligations
- track sent and received pulses
- issue and verify relay receipts
- maintain rolling service counters

This is the most distinctive subsystem in the project.

The witness engine turns "network entanglement" into measurable protocol evidence instead of private local state.

Current V2 detail on `main`:

- witness-derived service evidence is now moving from raw receipt views to explicit `ServiceAttestation` and `ServiceAggregate` objects
- attestations are intentionally lagged by one epoch after receipt reconciliation so the evidence plane uses settled observations instead of freshest-gossip guesses

### 5. Ledger Layer

Responsibilities:

- account state
- balance updates
- transaction validation
- block execution
- state root calculation

V1 should stay simple and use an account-based state machine.

### 6. Consensus Layer

Responsibilities:

- epoch transitions
- proposer selection
- block validation
- fork choice or finality logic
- topology commitment verification
- service score integration

Consensus should use public randomness and verifiable inputs.

The relay score should affect proposer eligibility or rewards, but not replace auditable randomness.

Current V2 detail on `main`:

- service-evidence gating is partially live behind `consensus_v2`
- certified sync is now live behind `consensus_v2`
- QC-backed canonical branch selection is now active behind `consensus_v2`
- restart-time proposal suppression and startup sync barriers are now active behind `consensus_v2`
- the earlier stale-node suffix catch-up gap is now materially closed on `main`
- the remaining consensus/runtime work is turning the healthy/degraded/stale matrix into a hard acceptance gate before PQ work

### 7. Storage Layer

Responsibilities:

- block storage
- state snapshots
- peer session metadata
- receipt archives
- benchmark event logs

Receipt storage should be prunable after commitments become final.

### 8. Simulator and Benchmark Harness

Responsibilities:

- launch many nodes on one machine
- inject latency, jitter, churn, and partitions
- script attack scenarios
- collect performance metrics

The simulator is a first-class part of the architecture, not an afterthought.

## Component Flow

1. Validators start from a fixed genesis validator registry.
2. At epoch start, consensus derives witness assignments from the last finalized randomness seed.
3. The network layer opens authenticated encrypted sessions to assigned peers.
4. The witness engine creates relay obligations for pulses and real protocol traffic.
5. Peers issue signed receipts when obligations are fulfilled within allowed windows.
6. Each validator aggregates receipt hashes into a topology commitment.
7. The consensus layer validates blocks, commitments, and service scores.
8. Metrics and logs are exported for stress analysis.

On the current V2 path, steps 6 and 7 are in transition:

- service scores are driven by witness-aligned aggregates when `consensus_v2` is enabled
- ordering now includes proposal votes, quorum certificates, certified suffix sync, and QC-dominant canonical branch choice
- the remaining instability is in service evidence and gating at `7/8`, not in whether certified state can dominate local drift

## Process Model For Localnet

The first realistic environment should use separate OS processes, not just async tasks inside a single process.

Each node should have:

- its own config file
- its own port set
- its own data directory
- its own validator key material
- its own log and metrics endpoint

This gives cleaner fault injection and makes CPU and memory usage easier to compare across nodes.

## Suggested Crate Boundaries

### `crypto`

- key wrappers
- scheme-aware signature wrappers
- signature verification
- key exchange
- transcript hashing
- zeroization helpers

### `types`

- transaction
- block
- receipt
- typed signature and identity containers
- epoch assignment
- topology commitment

### `network`

- transport
- framing
- gossip
- peer state
- replay protection

### `ledger`

- state transition logic
- account storage
- execution engine

### `consensus`

- proposer selection
- epoch rules
- commitment verification
- relay score calculations

### `node`

- startup wiring
- config loading
- RPC
- task orchestration

### `sim`

- localnet launcher
- workload generator
- failure injection
- benchmark collector

## Architectural Invariants

- no consensus decision should depend on private, unrevealable state
- networking evidence must be compact enough to validate inside block time
- relay scoring must favor diverse peers over many self-controlled peers
- localnet simulations must be able to reproduce bugs deterministically
- cryptography should be replaceable without rewriting core protocol logic
