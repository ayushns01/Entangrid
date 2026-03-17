# entangrid-sim

## What this crate does

This crate is the local lab for Entangrid.

It helps you:

- generate a multi-node local network
- launch validator processes
- inject traffic into that network
- simulate degraded validator behavior

It is called a simulator, but an important detail is:

it does **not** fake the chain in memory.

Instead, it launches real `entangrid-node` processes and manages them.

## How it currently works

### `init-localnet`

This command creates a fresh localnet directory.

It writes:

- `genesis.toml`
- `localnet-manifest.toml`
- one `node.toml` per validator
- one `node-N/` directory per validator
- `inbox/` and `processed/` folders for each node

It also:

- assigns validator ids
- assigns localhost ports
- creates validator dev secrets
- seeds initial balances
- configures all peers statically
- can enable service gating
- can mark one validator as degraded

### `up`

This command reads the manifest and launches one OS process per node.

For each node it captures:

- `stdout.log`
- `stderr.log`

The nodes then run until you stop the simulator with `Ctrl+C`.

### `load`

This command creates traffic.

Current scenarios:

- `idle`
- `steady`
- `bursty`
- `large-block`

The simulator:

- reads genesis
- uses the deterministic crypto backend
- creates signed transfer transactions
- picks recipients deterministically from the validator set
- writes those transactions into the target node's `inbox/`

Then the node process reads and handles them.

### Fault and degradation controls

The simulator can configure a degraded validator using:

- added delay
- outbound drop probability
- outbound disablement

That is how we currently test service scoring and proposer gating behavior.

## What it is today

Today this crate is a local orchestration and workload tool.

It is already very useful because it gives us:

- repeatable local experiments
- easy multi-validator startup
- transaction generation without an RPC layer
- degraded-node testing

It is not yet a full benchmarking harness or a full network simulator.

## Why this crate matters

Without this crate, testing the chain would be slow and manual.

This crate is what lets us quickly answer questions like:

- does the network start correctly?
- are blocks being produced?
- do receipts appear?
- does service gating visibly reject a degraded proposer?

## Where we want to take it

This crate should grow into a much stronger experimentation tool.

Future direction:

- add richer traffic patterns and scenarios
- add better automated verification after runs
- expand fault injection beyond simple drop/delay
- emit clearer benchmark summaries
- support more realistic network topologies
- help compare protocol changes across repeatable local experiments

In short: today this crate is the easiest way to run and stress the project locally, and later it should become the main experimentation harness for Entangrid.
