# Localnet Plan

## Goal

The first real environment for `Entangrid` should be a reproducible multi-node localnet running entirely on one machine.

The localnet is not just for demos. It is the main laboratory for protocol validation and attack testing.

Current status note:

- the active protocol work now happens on `main`
- the baseline V1 path is preserved on `codex/consensus-v1` as a benchmark line
- the current correctness gate is the single-machine healthy and degraded bursty matrix across `4/5/6/7/8` validators with `consensus_v2`

## Node Layout

Start with 4 to 8 validator processes.

Each node should have:

- a unique validator id
- a unique config file
- a unique network port set
- a unique data directory
- a unique metrics endpoint
- a separate log file

## Suggested Directory Shape

When code exists, a localnet run should create a structure similar to:

- `var/localnet/node-1/`
- `var/localnet/node-2/`
- `var/localnet/node-3/`
- `var/localnet/node-4/`

Each node directory can hold:

- generated key material
- config snapshot
- database files
- logs
- exported metrics

## Initial Network Topology

V1 should begin with static peer configuration.

Why:

- fewer moving parts
- deterministic debugging
- easier comparison across benchmark runs

Later experiments can add:

- randomized startup order
- delayed peer availability
- asymmetric link conditions
- partial partitions

## Epoch And Slot Suggestions

For the first localnet:

- use a human-readable slot duration
- keep epochs short enough to rotate witnesses frequently
- avoid tiny slots until baseline stability is proven

The first target is observability, not raw throughput.

## Localnet Workloads

Start with these workloads:

- idle heartbeat only
- steady transfer traffic
- bursty transfer traffic
- large block propagation
- handshake churn
- partial witness failure

Each workload should be scriptable and reproducible.

## Fault Injection Scenarios

The simulator should support:

- fixed packet delay
- random jitter
- packet drop rate
- forced disconnects
- node pause and resume
- skewed witness assignments for testing

## Metrics To Export

Every node should export:

- handshake attempts and failures
- session setup latency
- active sessions
- tx ingress and propagation latency
- block proposal latency
- block validation latency
- receipt creation rate
- receipt verification cost
- relay score over time
- memory usage
- CPU usage
- bandwidth usage

## Logs Worth Keeping

- epoch transition log
- witness assignment log
- receipt acceptance and rejection log
- proposer decision log
- fork and reorg log
- disconnect and reconnect log

## Minimum Demo Target

A good first demo is:

- 4 validators
- fixed static topology
- transfer transactions only
- witness receipts enabled
- blocks produced continuously
- visible relay score changes when one node is degraded

If localnet cannot make that demo stable, the protocol is not ready for broader experiments.

Current main-branch focus:

- make the `consensus_v2` matrix go green on one machine first
- use `codex/consensus-v1` as the benchmark comparison
- do not move to PQ integration until that happens
