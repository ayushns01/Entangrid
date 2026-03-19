# entangrid-consensus

## What this crate does

This crate decides the scheduling and scoring rules of the chain.

It is responsible for:

- slot and epoch math
- proposer selection
- witness assignment
- relay score calculation
- topology commitment construction
- basic block-rule checks

This is where the "who gets to propose" and "how useful was a validator to the network" logic lives.

## How it currently works

### Slot and epoch timing

The crate turns wall-clock time into:

- a current slot
- a current epoch

using values from genesis:

- `genesis_time_unix_millis`
- `slot_duration_millis`
- `slots_per_epoch`

### Proposer selection

For each slot, exactly one proposer is chosen.

Current rule:

- derive an epoch seed from the base epoch seed plus the epoch number
- combine that with the slot number
- hash it
- choose one validator from the sorted validator list

This makes proposer selection deterministic and public.

### Witness assignments

For each epoch, validators are rotated and assigned:

- witness validators
- relay targets

The assignments are deterministic, so every node can recompute them from genesis and the epoch number.

### Relay scoring

The crate turns relay receipts into service counters, then into a service score.

Current score formula:

- `0.25 * uptime`
- `0.50 * timely delivery`
- `0.25 * peer diversity`
- minus penalties

The result is clamped into the range `[0, 1]`.

Recent improvement:

- component ratios are capped so duplicate or excessive receipts do not give more than full credit for one score dimension
- this makes the score harder to inflate accidentally during noisy local experiments

The counters themselves now use the shared `ServiceCounters` type from `entangrid-types`, which makes it easier for the node runtime and metrics output to explain why a score is high or low.

### Commitments

The crate can summarize receipts into a `TopologyCommitment`.

That includes:

- receipt root
- receipt count
- message-class counts
- distinct peers
- computed relay score

### Basic block validation

Current block checks are intentionally simple:

- proposer must match the slot schedule
- parent hash must match the expected current tip
- optional service gating can reject a proposer below threshold

Important detail:

- this crate computes the score and enforces the threshold when asked
- the node runtime decides **when** service gating starts for a given network through config
- the node runtime also decides how many recent epochs are included in the rolling score window
- that start epoch is now configurable instead of being hard-coded in the runtime

## What this consensus is today

This is a **baseline deterministic proposer schedule**, not a full finality protocol.

That means:

- no BFT voting yet
- no sophisticated fork choice yet
- no validator-set changes yet
- no slashing/reward settlement yet

So think of it as the current policy engine for the chain, not the finished production consensus design.

## Why this crate matters

This crate contains the heart of the Entangrid research idea.

It is the place where the project moves beyond "pure stake selects proposers" and toward:

- validators being observed
- relay work being measured
- service scores becoming consensus-relevant

## Where we want to take it

This crate should become a much stronger consensus layer over time.

Future direction:

- improve fork choice and finality behavior
- refine witness assignment and relay-score robustness
- integrate service gating more cleanly into long-term proposer eligibility
- add better anti-gaming logic for receipts and relay evidence
- introduce validator economics and penalties where appropriate
- keep the rules deterministic and auditable by any node

In short: this crate already carries the main Entangrid idea, but it still needs to mature from a good research prototype into a stronger consensus engine.
