# Architecture

## Design Summary

`Entangrid` is designed as a research blockchain whose main experiment is service-coupled consensus.

Instead of treating networking as a side detail, the protocol records and scores how validators participate in message delivery. Validators do not earn better proposer odds simply by holding stake or opening idle connections. They must maintain secure sessions and produce verifiable evidence that they forwarded useful traffic within deadline windows.

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

### 3. Network Layer

Responsibilities:

- peer discovery for localnet
- authenticated session setup
- encrypted framing
- gossip for transactions and blocks
- witness pulse delivery
- backpressure and connection management

The network layer should not know consensus rules beyond message priority classes.

### 4. Witness Engine

Responsibilities:

- derive rotating witness assignments from epoch randomness
- schedule relay obligations
- track sent and received pulses
- issue and verify relay receipts
- maintain rolling service counters

This is the most distinctive subsystem in the project.

The witness engine turns "network entanglement" into measurable protocol evidence instead of private local state.

Current V2 branch detail:

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

Current V2 branch detail:

- service-evidence gating is partially live behind `consensus_v2`
- QC-backed ordering is still the next consensus milestone and is the main remaining reason larger bursty `6/8` validator runs can still diverge structurally

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

- service scores are increasingly driven by witness-aligned aggregates
- fork choice is still legacy until proposal votes and quorum certificates are added

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
- signature verification
- key exchange
- transcript hashing
- zeroization helpers

### `types`

- transaction
- block
- receipt
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
