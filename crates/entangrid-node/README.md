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
- validates any included commitment
- applies the block to a cloned ledger
- persists the block and updated snapshot

If the parent is unknown, the block is stored as an orphan.

### Receipts and scores

The node also:

- creates relay receipts when it is an assigned witness
- stores and re-gossips valid receipts
- ignores duplicate logical receipt events so repeated evidence does not inflate the score
- computes rolling service scores
- optionally rejects local block production if its score is below threshold
- respects a configurable `service_gating_start_epoch` instead of using a fixed warmup epoch
- respects a configurable rolling service-score window length
- records the latest local score breakdown in metrics and logs

That means the node can now explain service gating in a much more concrete way:

- what the score was
- how many uptime windows were observed
- how many deliveries were timely
- how many peers were actually reached

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
- the rolling score window is now configurable per localnet
- proposer checks log the counters behind the score
- missed slots log the reason more clearly
- `metrics.json` keeps the latest local service counters alongside the score
- `metrics.json` now also tracks how many duplicate receipts were ignored

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
