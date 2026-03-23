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
- `Transaction` is a native balance transfer:
  - `from`
  - `to`
  - `amount`
  - `nonce`
  - optional `memo`
- `SignedTransaction` wraps that transfer with:
  - signer id
  - signature
  - transaction hash
  - submission timestamp
- `Block` contains:
  - a `BlockHeader`
  - accepted transactions
  - an optional topology commitment
  - the receipt bundle that proves that commitment in the current prototype
  - proposer signature
  - block hash
- `ProtocolMessage` is the current network message enum:
  - transaction broadcast
  - block proposal
  - sync status, sync request/response, and incremental sync blocks
  - heartbeat pulse
  - relay receipt
  - receipt fetch/response
- `FeatureFlags` currently includes:
  - receipt enablement
  - service-gating enablement
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
  - duplicate receipt counts that were ignored by the node
  - sync-throttle, peer-rate-limit, inbound-session-drop, and sync-mode counters so recovery behavior is visible in reports

This crate also provides a few shared helpers:

- `canonical_hash(...)` for deterministic hashing through canonical serialization
- `hash_many(...)` for combining byte parts
- `validator_account(id)` for mapping validator ids to current account names like `validator-1`

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

One recent example of the type layer evolving with the protocol is block commitments:

- `Block` now carries `commitment_receipts` alongside the compact `TopologyCommitment`
- that lets validators recompute the same receipt root from the same proof bundle
- the field is deserialized with a default empty list so older saved block files remain readable

One recent example of the type layer evolving with recovery behavior is sync:

- `ProtocolMessage` now distinguishes small `SyncStatus` announcements from heavier sync payloads
- `ChainSegment` lets same-chain peers exchange only the missing block suffix plus recent receipts
- full `ChainSnapshot` is still available as the fallback for unknown or divergent peers

## Where we want to take it

This crate should grow into the stable protocol schema of Entangrid.

Future direction:

- separate validator identity from ordinary user accounts more clearly
- add richer transaction types beyond simple transfers
- support stronger proof and commitment structures
- introduce versioning and upgrade-friendly wire/state formats
- prepare the type layer for real post-quantum identities and network evidence

In short: this crate should become the clean, stable data model that the rest of the blockchain can rely on for a long time.
