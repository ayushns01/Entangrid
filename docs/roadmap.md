# Roadmap

## Phase 0: Documentation And Invariants

Deliverables:

- protocol documentation
- architecture documentation
- threat model
- benchmark plan
- localnet plan

Success criteria:

- the project has a clear protocol direction
- the first implementation can stay focused on the right constraints

## Phase 1: Crypto And Session Harness

Build:

- validator identity representation
- post-quantum session setup
- encrypted frame exchange
- replay protection
- basic metrics

Success criteria:

- two nodes can establish a secure session repeatedly
- latency, CPU, and memory metrics are recorded
- failed handshakes and rekeys are observable

## Phase 2: Minimal Ledger

Build:

- account-based state
- signed transfer transactions
- block structure
- state root calculation
- persistent block and state storage

Success criteria:

- nodes can submit, validate, and execute transactions
- blocks can be persisted and replayed deterministically

## Phase 3: Baseline Consensus

Build:

- fixed validator set
- epoch boundaries
- proposer selection using public randomness
- block validation and finality path

Success criteria:

- the localnet can keep producing blocks without entanglement scoring enabled
- forks and missed slots are observable

## Phase 4: Witness Entanglement

Build:

- witness assignment logic
- pulse scheduling
- relay receipt generation and validation
- topology commitment creation
- rolling relay score
- proposer gating or service multiplier

Success criteria:

- the chain can verify relay-backed commitments
- score changes reflect real network behavior
- basic Sybil and replay attacks fail in expected ways

## Phase 5: Attack And Stress Harness

Build:

- packet delay injection
- packet loss injection
- peer churn scripts
- isolation scenarios
- tx flood scenarios
- handshake storm scenarios

Success criteria:

- benchmark outputs clearly show where the protocol bends or breaks
- CPU, memory, bandwidth, and latency can be correlated with protocol events

## Phase 6: Advanced Experiments

Possible upgrades:

- multi-hop witness corridors
- dynamic validator changes
- refined scoring functions
- dispute resolution for invalid receipts
- reward accounting
- more realistic peer discovery

Success criteria:

- the project can compare multiple entanglement strategies under the same simulator

## Order Of Work

Implementation order matters.

Recommended order:

1. crypto harness
2. network sessions
3. minimal ledger
4. baseline consensus
5. relay receipts
6. topology commitments
7. attack harness
8. score tuning

## Current Main-Branch Focus

The current focus on `main` is no longer "invent V2 on a side branch." It is:

- stabilize `consensus_v2` on `main`
- keep `codex/consensus-v1` as the benchmark/control line
- make the single-machine `4/5/6/7/8` healthy and degraded bursty matrix go green
- finish certified sync and stronger QC-dominant convergence
- only then move to PQ-safe crypto integration

## What Not To Build Too Early

- smart contracts
- slashing economics
- open validator admission
- internet deployment
- fancy dashboards before metrics exist

## Definition Of A Good V1

V1 is successful if:

- several localhost validators can run as independent processes
- they can exchange post-quantum protected traffic
- they can produce and validate blocks
- they can accumulate witness-based relay evidence
- the benchmark harness can show both honest and adversarial behavior clearly
