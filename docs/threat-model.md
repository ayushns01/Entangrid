# Threat Model

## Security Goal

`Entangrid` should remain safe and measurable under realistic adversarial behavior while experimenting with service-coupled consensus.

The system is not trying to solve every production blockchain problem in V1. It is trying to surface whether the witness-entangled design can stay auditable, hard to game, and operational under load.

Current main-branch focus:

- test the active `consensus_v2` line on one-machine `4/5/6/7/8` bursty matrices
- treat `codex/consensus-v1` as the benchmark/control line
- keep Stage 1 PQ integration in scope, but gate final merge on the same convergence and recovery matrix

## Protected Properties

- proposer eligibility should not be easy to bias locally
- network contribution claims should be verifiable
- transaction and block authenticity should hold under post-quantum assumptions
- a small amount of packet loss or churn should not fully destroy liveness
- attack scenarios should be reproducible in localnet

## Adversary Capabilities

Assume an adversary can:

- run multiple validator-controlled peers
- open and close many connections
- delay, replay, or drop traffic
- collude with a subset of witnesses
- exploit network asymmetry
- flood the network with valid but costly traffic
- attempt to isolate a validator
- compromise a limited number of node processes

Do not assume the adversary can:

- break the chosen post-quantum primitives
- compromise most validators in V1 localnet
- control the entire operating system of all nodes simultaneously

## Major Threats And Mitigations

### 1. Sybil Relay Inflation

Threat:

A validator creates many helper peers to inflate its relay evidence.

Mitigations:

- count only stake-registered validators as witnesses in V1
- use seed-derived witness assignments
- cap the contribution of any repeated identity pair
- add diversity scoring rather than pure receipt counting

### 2. Connection Grinding

Threat:

A validator repeatedly opens and closes sessions to search for favorable local state.

Mitigations:

- do not use private session state as lottery randomness
- derive assignments from public epoch seed
- score only assigned witness windows
- penalize excessive failed setup attempts

### 3. Fake Relay Claims

Threat:

A validator claims to have forwarded data it never relayed.

Mitigations:

- require signed witness receipts
- include transcript digests and sequence numbers
- bind receipts to epoch, window, and peer identities
- sample-revalidate receipt bodies on dispute

### 4. Witness Collusion

Threat:

A witness signs false receipts for a favored validator.

Mitigations:

- rotate witnesses each epoch
- require multiple independent witness relationships over time
- make receipt histories auditable
- add later penalties for dishonest witnesses

### 5. Replay Attacks

Threat:

Old receipts or pulses are replayed as fresh evidence.

Mitigations:

- monotonic counters per relationship
- epoch-scoped transcript hashes
- per-window unique ids
- strict freshness rules in validation

### 6. Eclipse And Isolation

Threat:

A validator is surrounded or partially isolated so its service score collapses.

Mitigations:

- use multiple witnesses
- use rolling score windows instead of single-window exclusion
- avoid requiring perfect service for basic eligibility
- test partial isolation explicitly in localnet

### 7. Resource Exhaustion

Threat:

An attacker forces expensive signature verification or handshake storms.

Mitigations:

- fixed maximum inbound frame sizes
- rate limits per peer
- capped concurrent inbound sessions
- bounded receipts per window
- separate control plane from bulk data paths
- benchmark signature verification cost early

### 8. Key Compromise

Threat:

A node's long-lived identity or session material is stolen.

Mitigations:

- keep session keys ephemeral
- store validator keys separately from transport secrets
- support key rotation in later milestones
- minimize secret retention in memory

### 9. State Bloat

Threat:

Relay evidence grows too quickly and burdens storage or validation.

Mitigations:

- commit receipt hashes, not full payloads, on chain
- prune receipts after dispute windows
- bound receipts per epoch
- aggregate counters where exact detail is unnecessary

## Open Risks

- collusion-resistant scoring is still a research problem
- a very harsh relay threshold could make consensus brittle
- a very soft relay threshold could make entanglement meaningless
- multi-hop witness corridors may be much harder to validate efficiently than pairwise receipts
- bursty `6`-validator convergence is still not fully proven on the active Stage 1 line even though certified sync and QC-aware branch choice are live

## What Must Be Tested Early

- can a single validator boost score with a small Sybil cluster
- how quickly does score recover after honest temporary packet loss
- how much CPU do receipt signatures consume under tx flood
- how large do commitment proofs become at increasing validator counts
- what happens when witnesses disagree about a relay event
