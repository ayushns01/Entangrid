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

Current prototype detail:

- in the direct-delivery localnet model, witnesses are intentionally aligned with relay targets
- that is because only the receiving relay target actually observes the message today
- this keeps the witness rules consistent with what the runtime can really prove, instead of assuming third-party observers that the current network layer does not yet model

Current `consensus_v2` detail on `main`:

- the V2 service committee for a validator now uses that validator's actual assigned witnesses
- this removes the earlier mismatch where a derived observer set could be asked to score work it never directly observed
- the proposal-vote, quorum-certificate, and certified-sync runtime path now consumes these same deterministic timing and scoring rules through `entangrid-node`

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

Current runtime detail:

- the node now feeds real penalty inputs into those counters instead of leaving them as placeholders
- failed outbound session attempts penalize the local validator only when an assigned relay target could not be reached, and only once per target per epoch
- invalid receipts penalize the witness validator that signed them
- so the `minus penalties` part of the formula is now active in the running prototype, not just present in the math
- the score weights are now configurable through shared config, so localnet experiments can tune the same consensus formula without recompiling the crate
- the current recommended prototype profile is:
  - gating start epoch `3`
  - gating threshold `0.40`
  - score window `4`
  - uptime `0.25`
  - delivery `0.50`
  - diversity `0.25`
  - penalty `1.00`

Current `consensus_v2` scoring detail on `main`:

- witness attestations are now scoped to the specific obligations that witness can actually observe for the subject validator
- service aggregates are intended to summarize witness-attested evidence from earlier epochs, not whatever raw receipt gossip one node happened to cache locally
- the latest runtime change publishes attestations with a one-epoch reconciliation lag so witnesses do not prematurely emit zeroed evidence before receipt propagation settles

### Commitments

The crate can summarize receipts into a `TopologyCommitment`.

That includes:

- receipt root
- receipt count
- message-class counts
- distinct peers
- computed relay score

Recent improvement:

- the crate can now filter the exact receipt bundle for one validator and epoch
- it can validate whether a receipt matches the assigned witness and relay-target relationship
- it can build a commitment directly from an explicit receipt bundle

That matters because different nodes may hear different gossip receipts in a degraded network.
By working from the exact receipt bundle carried with a block, every validator can recompute the same commitment root instead of guessing from its local receipt cache.

### Basic block validation

Current block checks are intentionally simple:

- proposer must match the slot schedule
- parent hash must match the expected current tip
- optional service gating can reject a proposer below threshold

Important detail:

- this crate computes the score and enforces the threshold when asked
- the node runtime decides **when** service gating starts for a given network through config
- the node runtime also decides **what threshold** service gating uses for a given network through config
- the node runtime also decides how many recent epochs are included in the rolling score window
- the node runtime now also decides which score-weight profile is applied to those counters before the final service score is computed
- that start epoch is now configurable instead of being hard-coded in the runtime

## What this consensus is today

This crate is the deterministic policy and scoring engine for the chain.

That means:

- slot timing, proposer selection, witness assignment, and relay-score rules live here
- proposal votes, quorum certificates, certified sync, and branch adoption are now live in the node runtime on top of these shared rules
- validator-set changes and slashing/reward settlement do not exist yet

So think of it as the current rule layer for Entangrid consensus, not a standalone finality engine by itself.

Important current limit:

- the baseline receipt-driven scoring model is still useful as the benchmark path on `codex/consensus-v1`
- `main` now carries the active V2 scoring work behind `consensus_v2`
- service-evidence gating from prior-epoch aggregates is live, and the node/runtime path now has certified sync plus QC-dominant branch choice
- service evidence and degraded punishment are materially better after the recent transport/session hardening on `main`
- the broad stale-node restart recovery gap is no longer the main blocker on the active line
- PQ Stage 1 integration now exists on the active branch; the next runtime step is closing `baseline-6-bursty` and `gated-6-bursty`, then freezing the simulator acceptance gates

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
