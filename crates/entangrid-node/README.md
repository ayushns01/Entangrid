# entangrid-node

## What this crate does

This crate is the main validator process.

It is the crate that actually runs the blockchain.

It wires together:

- config loading
- crypto
- consensus
- ledger
- networking
- storage
- metrics

If you start one validator process, this is the crate doing the work.

## How it currently works

### Startup

When a node starts, it:

- reads `node.toml`
- reads the shared `genesis.toml`
- creates its storage directories if needed
- loads blocks, receipts, and snapshot files if they already exist
- rebuilds ledger state from snapshot or block replay
- starts the network listener

### Main runtime loop

The node then runs a few repeating tasks:

- slot tick
- inbox scan
- metrics flush
- sync request and sync status broadcast
- incoming network event handling

### Slot tick behavior

On each new slot, the node:

- computes current slot and epoch
- refreshes service scores
- logs epoch transitions
- broadcasts a heartbeat
- checks whether it is the proposer for the slot
- proposes a block if allowed
- tries to promote orphan blocks whose parents have arrived

### Inbox and mempool

The node scans its `inbox/` directory for transaction JSON files.

For each valid transaction:

- it validates the transaction against current state plus pending mempool items
- adds it to the mempool
- logs it
- may issue a witness receipt if appropriate
- gossips it to peers
- moves the file into `processed/`

### Block handling

When the node proposes or receives a block, it:

- verifies the block hash
- verifies proposer signature
- checks slot proposer correctness
- checks parent hash
- checks transaction root and topology root
- validates any included commitment against the exact receipt bundle carried with the block
- applies the block to a cloned ledger
- persists the block and updated snapshot

Recent improvement:

- the node no longer tries to recompute commitment roots from whatever local receipt gossip it happened to see
- accepted blocks can import commitment receipt bundles into local receipt storage
- this prevents degraded nodes from crashing on `receipt root mismatch` just because their local receipt cache differed from the proposer’s
- the node now handles interrupt and terminate signals by flushing its latest snapshot and metrics before exiting
- sync snapshot adoption now validates the incoming block history structurally instead of trusting raw replay alone
- sync snapshot adoption now validates and deduplicates incoming receipts before they can influence local service scores
- this closes a class of sync-poisoning bugs where a malicious peer could try to inject invalid receipt evidence or an invalid longer chain through `SyncResponse`
- remote block acceptance no longer applies local service-gating rejection logic to peer blocks
- that means proposer gating is currently enforced on local block production, not on peer-block rejection, until the score source is made more globally shared and deterministic

If the parent is unknown, the block is stored as an orphan.

### Receipts and scores

The node also:

- creates relay receipts when it is an assigned witness
- stores and re-gossips valid receipts
- ignores duplicate logical receipt events so repeated evidence does not inflate the score
- computes rolling service scores
- optionally rejects local block production if its score is below threshold
- respects a configurable `service_gating_start_epoch` instead of using a fixed warmup epoch
- respects a configurable `service_gating_threshold` instead of using a hard-coded rejection line
- respects a configurable rolling service-score window length
- now feeds two real penalty sources into the score instead of leaving them at zero:
  - failed outbound session attempts count against the local validator only when an assigned relay target could not be reached, only after that peer is known live, and only once per target per epoch
  - invalid receipts count against the witness validator that signed them
- clears a prior failed-session observation for a relay target once a successful outbound session to that target is observed in the same epoch
- records the latest local score breakdown in metrics and logs
- drops and logs invalid peer receipts, invalid peer transactions, and invalid peer blocks instead of crashing the process
- writes metrics more eagerly around block and slot transitions so localnet reports stay closer to the saved chain state

Current `consensus_v2` additions on `main`:

- committee witnesses emit `ServiceAttestation` records for validators they were actually assigned to observe
- nodes import and store those attestations, then publish/import `ServiceAggregate` records
- at epoch transition, the node first reconciles receipts for `epoch - 1`, then emits attestations for `epoch - 2`
- this one-epoch lag is intentional: it prevents witnesses from publishing empty evidence before the prior epoch's receipts have finished propagating
- local proposer gating now distinguishes confirmed low-score rejection from no-evidence and insufficient-evidence skips
- when no newer usable aggregate exists yet, the node now preserves the last known score for observability while skipping enforcement unless the immediately prior epoch is confirmed
- proposal votes and quorum certificates are live in the runtime
- equal-QC uncertified sibling branches no longer replace the current canonical tip just because they gained extra local vote support

Current Stage 1 PQ additions on `stage-1/pq-integration`:

- strict hybrid validator-identity enforcement is available behind `require_hybrid_validator_signatures`
- blocks, proposal votes, transactions, relay receipts, and service attestations can all be enforced as hybrid-signed validator artifacts
- the runtime can use hybrid per-stream session establishment, encrypted post-handshake frame bodies, and transport-local session TTL turnover when built with `pq-ml-dsa pq-ml-kem`

That means the node can now explain service gating in a much more concrete way:

- what the score was
- how many uptime windows were observed
- how many deliveries were timely
- how many peers were actually reached
- how many failed sessions and invalid receipts were counted as penalties

## What this crate is today

Today this crate is a real working validator runtime for a local research chain.

It already does a lot:

- block production
- block validation
- mempool handling
- sync status announcements, sync requests/responses, and incremental catch-up
- heartbeat broadcasting
- receipt generation and storage
- file-backed persistence
- service-gating prototype behavior
- hybrid validator-signature enforcement and hybrid transport lanes when the Stage 1 PQ features are enabled

The recent gating-focused improvement in this crate was mostly about observability and control:

- service gating start is now configurable per localnet
- service gating threshold is now configurable per localnet
- the rolling score window is now configurable per localnet
- service-score weights are now configurable per localnet
- proposer checks log the counters behind the score
- missed slots log the reason more clearly
- `metrics.json` keeps the latest local service counters alongside the score
- `metrics.json` now also stores the active score-weight profile so reports can tell you not just what the counters were, but how the node interpreted them
- `metrics.json` now also tracks how many duplicate receipts were ignored

The current recommended prototype localnet profile is:

- gating start epoch `3`
- threshold `0.40`
- score window `4`
- score weights `[0.25, 0.50, 0.25, -1.00]`

That profile is still the shared default emitted by `init-localnet`, but it should currently be treated as the best 4-validator baseline rather than a final all-topology signoff.

Important current limit:

- the baseline receipt-driven path still exists and is preserved as the benchmark line on `codex/consensus-v1`
- `main` is now the active V2-focused node runtime line
- certified sync and QC-dominant branch choice are now live on `main`
- healthy bursty `6/7/8` runs now repeatedly shut down structurally on one tip
- stale certified-sync responses are now skipped instead of re-adopting an older certified suffix
- restarted nodes now suppress historical proposer-slot replay and can hold proposals behind a startup sync barrier while peers are still ahead
- stale-node restart recovery is no longer the broad matrix-wide blocker, but one stuck-follower recovery case still remains in `gated-6-bursty`

The recent validation-focused improvement in this crate was about correctness under degraded networking and stale restart recovery:

- commitment validation now uses explicit proof data from the block
- stricter receipt assignment checks are enforced
- malformed peer data is rejected locally instead of taking the node down
- the periodic sync tick now broadcasts lightweight `SyncStatus` instead of a full chain snapshot
- when a peer is on the same branch but behind, the node now prefers an incremental block segment over a full snapshot
- when a peer is unknown, diverged, too far behind, or cannot advertise its stale state accurately, the node falls back to a full snapshot so recovery still works instead of looping on stale incremental sync segments
- repeated sync requests from the same peer are throttled, which makes the prototype harder to abuse without breaking recovery
- spam-prone peer traffic now hits a per-peer rate limiter before expensive receipt and sync handling runs
- startup replay now tolerates a truncated trailing JSONL entry, which protects restart/reporting paths from an interrupted final append
- the active matrix focus on `main` is the V2 `4/5/6/7/8` healthy and degraded bursty sweep, using `codex/consensus-v1` as the benchmark/control line
- the latest rigorous matrix on the active branch is `12/14`, with only `baseline-6-bursty` and `gated-6-bursty` still failing
- certified sync now refuses stale certified responses that would otherwise pull a node backward after it already advanced
- restarted nodes now seed `last_processed_slot` from real time and stored state so they do not replay historical proposer slots on startup
- restarted nodes can wait behind a startup sync barrier instead of immediately proposing while peers are known ahead
- certified sync responses now carry responder tip metadata so a catching-up node can follow certified repair with suffix sync instead of stalling at the certified frontier
- startup-barrier sync serving is now capped to the certified frontier instead of exporting a stale uncertified suffix
- live slots now proactively request catch-up while peers are known ahead instead of waiting only for the periodic sync tick
- the next node-runtime milestone is closing the remaining bursty `6`-validator pair and then freezing the acceptance gates

So the next node-runtime milestone is:

- finishing the last two failing bursty `6`-validator scenarios on current `main`
- simulator acceptance gates that fail loudly on any regression

But it is still intentionally early-stage.

Missing pieces include:

- RPC or HTTP API
- production-grade sync
- advanced finality/fork handling
- production-grade PQ backend hardening and key-management tooling
- production storage engine
- validator operations tooling

## Why this crate matters

This is the crate that turns the rest of the workspace from "library code" into an actual blockchain node.

In localnet, each `node-N` directory corresponds to one process running this crate.

## Where we want to take it

This crate should eventually become a stronger validator client.

Future direction:

- expose safe transaction submission and status APIs
- improve chain sync and recovery behavior
- make networking more persistent and efficient
- integrate real PQ-secure crypto without changing the high-level runtime shape too much
- improve observability, benchmarking hooks, and operator ergonomics
- support more complex protocol phases as Entangrid matures

In short: today this crate is the working backbone of the local chain, and later it should become the polished validator client for Entangrid.
