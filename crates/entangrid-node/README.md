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
- sync request broadcast
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
  - failed outbound session attempts count against the local validator only when an assigned relay target could not be reached, and only once per target per epoch
  - invalid receipts count against the witness validator that signed them
- records the latest local score breakdown in metrics and logs
- drops and logs invalid peer receipts, invalid peer transactions, and invalid peer blocks instead of crashing the process
- writes metrics more eagerly around block and slot transitions so localnet reports stay closer to the saved chain state

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
- sync requests/responses
- heartbeat broadcasting
- receipt generation and storage
- file-backed persistence
- service-gating prototype behavior

The recent gating-focused improvement in this crate was mostly about observability and control:

- service gating start is now configurable per localnet
- service gating threshold is now configurable per localnet
- the rolling score window is now configurable per localnet
- proposer checks log the counters behind the score
- missed slots log the reason more clearly
- `metrics.json` keeps the latest local service counters alongside the score
- `metrics.json` now also tracks how many duplicate receipts were ignored

The recent validation-focused improvement in this crate was about correctness under degraded networking:

- commitment validation now uses explicit proof data from the block
- stricter receipt assignment checks are enforced
- malformed peer data is rejected locally instead of taking the node down
- when a peer appears to be on a stale or forked branch, the node now pushes its own longer chain snapshot back to that peer instead of only asking for sync in the opposite direction
- nodes now also broadcast their current chain snapshot on the periodic sync tick, so a heavily degraded peer can still recover through inbound sync traffic even if it cannot send a clean sync request itself
- startup replay now tolerates a truncated trailing JSONL entry, which protects restart/reporting paths from an interrupted final append

But it is still intentionally early-stage.

Missing pieces include:

- RPC or HTTP API
- production-grade sync
- advanced finality/fork handling
- real PQ crypto backend integration
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
