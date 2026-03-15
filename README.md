# Entangrid

`Entangrid` is a post-quantum blockchain research project in Rust built around a deliberately unusual idea:

consensus should care about how well a validator helps the network move real data, not just how much stake it holds.

The project started from "network entanglement" and evolves it into a stronger protocol:

- validators establish post-quantum secure sessions with assigned witnesses
- witnesses issue signed relay receipts for packets, transactions, and blocks actually forwarded on time
- each validator commits those receipts into a topology commitment
- proposer eligibility depends on stake plus a rolling relay score derived from those commitments

This keeps the original spirit of entangling networking and consensus, but avoids the weakest part of the first idea: using raw session secrets as the lottery input.

## Core Protocol Idea

The protocol direction documented in this repository is:

- post-quantum identities and signatures for validators and transactions
- post-quantum key exchange for peer sessions
- rotating witness assignments per epoch
- relay receipts that prove timely forwarding behavior
- topology commitments that summarize a validator's observed service to the network
- proposer election that uses unbiased randomness, then gates or weights eligibility by relay performance

The project goal is not to beat Ethereum or Bitcoin. The goal is to build a real multi-node system, stress it on one machine first, and learn where the bottlenecks and attack surfaces appear when post-quantum cryptography and network-coupled incentives meet.

## Why This Version Is Stronger

The original concept tied block selection directly to live shared secrets from active connections. That was creative, but it had serious problems:

- it was hard for the rest of the network to verify
- it encouraged connection grinding and Sybil peers
- it measured open sockets more than useful relay work
- it made consensus liveness too dependent on raw connectivity

This repository instead documents a more defensible design:

- randomness stays public and auditable
- network contribution is measured through signed witness evidence
- relay work matters more than connection count
- the system can be simulated, benchmarked, and attacked in a controlled way

## Documentation Index

- [Architecture](docs/architecture.md)
- [Protocol Specification](docs/protocol.md)
- [Threat Model](docs/threat-model.md)
- [Roadmap](docs/roadmap.md)
- [Localnet Plan](docs/localnet.md)
- [Benchmarking Plan](docs/benchmarks.md)

## MVP Scope

The first milestone should stay intentionally small:

- fixed validator set
- account-based ledger
- signed transfer transactions only
- one-machine local multi-node network
- baseline proposer selection before advanced entanglement rules
- witness-assigned relay receipts
- metrics for handshake cost, propagation latency, CPU, memory, and bandwidth

## Non-Goals For V1

- smart contracts
- permissionless validator onboarding
- slashing economics
- tokenomics design
- internet-scale peer discovery
- production hardening

## Proposed Workspace Shape

When implementation starts, this repository should become a Cargo workspace with crates similar to:

- `crypto`
- `types`
- `network`
- `ledger`
- `consensus`
- `node`
- `sim`

## Guiding Principle

The chain should not reward a validator merely for being online.

It should reward validators that are online, reachable, diverse, and provably helpful in moving consensus-critical data across the network.
