# Benchmarking Plan

## Purpose

Benchmarking in `Entangrid` is not just about throughput.

The purpose is to learn:

- how expensive post-quantum sessions are
- how expensive signed relay evidence is
- whether witness entanglement improves or harms liveness
- how the protocol reacts under churn, congestion, and partial isolation

Current benchmark gate on `main`:

- healthy and degraded bursty localnets across `4/5/6/7/8` validators
- `consensus_v2` is the implementation target
- `codex/consensus-v1` is the benchmark/control line
- Issue 1 certified-sync availability is now expected to stay live in `6/7/8`, and certified repair should apply whenever a run actually needs recovery
- Issue 2 healthy branch convergence is now expected to finish on one tip in repeated `6/7/8` runs
- PQ integration stays out of scope until the V2 matrix is green

## Core Metrics

### Cryptography

- session setup time
- signature creation time
- signature verification time
- rekey frequency cost

### Consensus

- slot success rate
- missed proposer rate
- block finalization delay
- fork frequency

### Networking

- block propagation latency
- tx propagation latency
- relay receipt latency
- dropped connection rate
- bytes sent and received per node

### System

- CPU usage per node
- memory usage per node
- disk growth rate
- receipt archive size

## Benchmark Suites

### 1. Baseline Suite

Conditions:

- no churn
- low tx rate
- no fault injection

Purpose:

- establish clean reference numbers

### 2. Crypto Stress Suite

Conditions:

- frequent session setup
- frequent rekeys
- many signed receipts

Purpose:

- isolate post-quantum overhead

### 3. Propagation Suite

Conditions:

- rising tx rate
- rising block size
- normal witness rotation

Purpose:

- measure how witness scoring affects message delivery

### 4. Churn Suite

Conditions:

- repeated disconnects
- session failures
- rejoin attempts

Purpose:

- see whether honest validators recover score reasonably

### 5. Isolation Suite

Conditions:

- one validator gets degraded links
- one validator loses some witnesses

Purpose:

- evaluate liveness and fairness under eclipse-like pressure

### 6. Adversarial Suite

Conditions:

- Sybil-like helper nodes in simulation
- replay attempts
- fake receipt attempts
- excessive handshake storms

Purpose:

- validate that entanglement is not trivially gameable

## Reporting Format

Each benchmark report should include:

- scenario description
- node count
- tx rate
- fault injections
- key metrics
- charts or tables
- brief interpretation

## Success Signals

Positive signals:

- relay score tracks real service quality
- honest temporary loss does not permanently destroy participation
- Sybil helpers do not create outsized scoring gains
- receipt overhead stays bounded

Negative signals:

- relay scoring dominates consensus so strongly that liveness becomes fragile
- receipt verification cost grows faster than expected
- commitment size becomes a bottleneck
- validators can manipulate score without relaying useful traffic

## First Benchmark Milestones

Run these in order:

1. two-node secure session benchmark
2. four-node baseline chain benchmark
3. four-node witness receipt benchmark
4. four-node degraded-link benchmark
5. seven-node churn and isolation benchmark
6. `4/5/6/7/8` healthy and degraded bursty V2 matrix against the V1 benchmark line

## Rule For Interpreting Results

Do not optimize blindly for throughput.

If higher throughput comes from weakening the entanglement evidence or making the score easy to fake, that is a protocol regression, not a win.
