# entangrid-types

## What this crate does

This crate is the shared language of the project.

It defines the common Rust types that every other crate agrees on:

- validator ids, slots, epochs, hashes, and account ids
- genesis and node configuration structs
- transactions and signed transactions
- blocks and block headers
- heartbeat pulses, relay receipts, and topology commitments
- protocol messages sent over the network
- metrics, snapshots, and log entry formats

If `entangrid-node`, `entangrid-ledger`, `entangrid-consensus`, and `entangrid-network` are the moving parts, `entangrid-types` is the contract that keeps them speaking the same language.

## How it currently works

Right now this crate is intentionally simple and very central.

It also now contains the first crypto-agility primitives for the PQ work:

- `SignatureScheme`
- `PublicKeyScheme`
- `TypedSignature`
- `PublicIdentity`
- `SigningBackendKind`
- `SessionKeyScheme`
- `SessionPublicIdentity`
- `SessionBackendKind`

- `GenesisConfig` describes the chain start state:
  - validator list
  - slot timing
  - epoch size
  - initial balances
  - witness count
- `NodeConfig` describes one running validator node:
  - validator id
  - data directory
  - static peers
  - feature flags
  - fault profile
  - `signing_backend`
  - optional `signing_key_path`
  - `session_backend`
  - optional `session_key_path`
- `ValidatorConfig` now carries both:
  - signing-facing `public_identity`
  - optional transport-facing `session_public_identity`
- `Transaction` is a native balance transfer:
  - `from`
  - `to`
  - `amount`
  - `nonce`
  - optional `memo`
- `SignedTransaction` wraps that transfer with:
  - signer id
  - typed signature
  - transaction hash
  - submission timestamp
- `Block` contains:
  - a `BlockHeader`
  - accepted transactions
  - an optional topology commitment
  - the receipt bundle that proves that commitment in the current prototype
  - typed proposer signature
  - block hash
- `ProtocolMessage` is the current network message enum:
  - transaction broadcast
  - block proposal
  - proposal votes and quorum certificates
  - sync status, sync request/response, and incremental sync blocks
  - heartbeat pulse
  - relay receipt
  - receipt fetch/response
  - service attestations and service aggregates for `consensus_v2`
- `FeatureFlags` currently includes:
  - receipt enablement
  - service-gating enablement
  - `consensus_v2` enablement
  - `require_hybrid_validator_signatures` for opt-in hybrid enforcement on validator-originated transactions and consensus objects
  - the epoch where gating should start
  - the score threshold used by service gating
  - the number of epochs included in the rolling service-score window
  - the shared score-weight profile used to turn counters into a final service score
- `ServiceCounters` is the shared score-breakdown struct used to describe:
  - uptime windows
  - timely deliveries
  - peer diversity
  - failure and invalid-receipt counts
- `NodeMetrics` now carries both:
  - the latest local service score
  - the latest local service counters used to explain that score
  - service-gating rejection and enforcement-skip counts
  - duplicate receipt counts that were ignored by the node
  - sync-throttle, peer-rate-limit, inbound-session-drop, and sync-mode counters so recovery behavior is visible in reports

This crate also provides a few shared helpers:

- `canonical_hash(...)` for deterministic hashing through canonical serialization
- `hash_many(...)` for combining byte parts
- `validator_account(id)` for mapping validator ids to current account names like `validator-1`

It now also carries the Stage 1G handshake wire types shared by crypto and transport:

- `SessionClientHello`
- `SessionServerHello`

## What this means in the current implementation

Today, the chain is built around validator-owned accounts and transfer-only state transitions.

That means:

- there are no user wallets as separate first-class types yet
- there is no smart contract call type yet
- there is no EVM or Solidity-facing transaction format yet
- the protocol is optimized for a local research chain, not a production compatibility target

This crate is doing exactly what it should do at this stage: keep the rest of the system consistent while the protocol is still evolving.

One recent example of that role is service gating:

- the simulator writes `service_gating_start_epoch` into node config through these shared types
- the simulator also writes the service-score window length through these shared types
- the simulator now also writes shared `ServiceScoreWeights` through these same config types
- the node reports score breakdowns back into `NodeMetrics` through these shared types
- the consensus crate consumes the same `ServiceCounters` layout when turning receipts into scores
- `NodeMetrics` now also carries the active score-weight profile so reports can explain not just the counters, but the policy that turned them into a score

Another recent example is Stage 1G plus Stage 1I transport setup:

- validator metadata can now advertise a separate session public identity
- node-local config can now point at a separate session key file
- node-local config now also carries `session_ttl_millis` so hybrid session lifetime stays transport-local instead of becoming a genesis setting
- crypto and network share one handshake wire format through this crate
- that shared handshake now feeds an encrypted post-handshake transport lane when the hybrid session backend is active
- omitted TTL now means a 10 minute default lifetime for hybrid lanes, while `0` disables expiry and deterministic transport remains unchanged
- the deterministic session path remains the default when `pq-ml-kem` is disabled

The current recommended prototype policy now lives in these shared defaults too:

- gating start epoch `3`
- gating threshold `0.40`
- score window `4` epochs
- score weights `[0.25 uptime, 0.50 delivery, 0.25 diversity, 1.00 penalty]`

That matters because new localnets now inherit the same baseline policy that the rigorous matrix selected, instead of relying on older hard-coded warmup values in different crates.

One recent example of the type layer evolving with the protocol is block commitments:

- `Block` now carries `commitment_receipts` alongside the compact `TopologyCommitment`
- that lets validators recompute the same receipt root from the same proof bundle
- the field is deserialized with a default empty list so older saved block files remain readable

One recent example of the type layer evolving with recovery behavior is sync:

- `ProtocolMessage` now distinguishes small `SyncStatus` announcements from heavier sync payloads
- `ChainSegment` lets same-chain peers exchange only the missing block suffix plus recent receipts
- full `ChainSnapshot` is still available as the fallback for unknown or divergent peers

One recent example of the type layer evolving with the active main-branch V2 work is ordering and evidence:

- `ProposalVote` and `QuorumCertificate` now travel through the shared type layer
- `ServiceAttestation` and `ServiceAggregate` now carry the witness-aligned evidence needed for the V2 path
- `NodeMetrics` now distinguishes actual gating rejections from enforcement skips when evidence is missing or insufficient

One recent example of the type layer evolving with the active PQ branch is signing and identity metadata:

- signature-bearing protocol objects now carry `TypedSignature` instead of anonymous bytes
- validator config now carries `PublicIdentity` instead of an untyped byte blob
- node config now carries `SigningBackendKind` so signer selection is local configuration, not a consensus feature flag
- verification code can now dispatch by explicit signature scheme
- typed signatures and identities now support first-class hybrid bundles while preserving the legacy single-form encoding
- `FeatureFlags.require_hybrid_validator_signatures` now lets a network opt into hybrid validator identity enforcement at startup plus hybrid transaction/block/proposal-vote/relay-receipt/service-attestation enforcement at runtime
- service aggregates stay unsigned in the type layer and inherit that strict policy transitively through embedded service attestations instead of gaining a second policy surface
- Stage 1F plus Stage 1H use these same fields to bootstrap a strict hybrid localnet: the simulator writes hybrid validator identities plus `session_public_identity` into genesis, sets `signing_backend = HybridDeterministicMlDsaExperimental` plus `session_backend = HybridDeterministicMlKemExperimental` per node, enables `require_hybrid_validator_signatures = true`, and forces `consensus_v2 = true`

## Where we want to take it

This crate should grow into the stable protocol schema of Entangrid.

Future direction:

- separate validator identity from ordinary user accounts more clearly
- add richer transaction types beyond simple transfers
- support stronger proof and commitment structures
- introduce versioning and upgrade-friendly wire/state formats
- extend the typed identity and signature model into full PQ and hybrid production formats
- decide when hybrid signatures become mandatory instead of permissive

In short: this crate should become the clean, stable data model that the rest of the blockchain can rely on for a long time.
