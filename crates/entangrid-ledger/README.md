# entangrid-ledger

## What this crate does

This crate is the state machine of the blockchain.

Its job is to answer:

- is a transaction valid?
- what happens to balances and nonces when we apply it?
- what does the state look like after a block?
- what is the deterministic state root for that state?

If you think in Ethereum terms, this is the smallest possible version of the execution layer, but without the EVM.

## How it currently works

The ledger is account-based and very small on purpose.

Current state model:

- balances per account
- nonces per account
- tip hash
- block height
- last processed slot

Current transaction rules:

- the transaction hash must match the transaction contents
- the `from` account must match the signer validator id
- the signature must verify through the crypto backend
- the sender must have enough balance
- the nonce must be exactly the expected next nonce

When a transaction is applied:

- sender balance goes down
- receiver balance goes up
- sender nonce increases by one

When a block is applied:

- every transaction is validated
- every valid transaction is applied in order
- the resulting state root is recomputed
- that recomputed root must match the block header
- if it matches, the snapshot tip/height/slot are updated

The crate also supports replaying blocks from genesis and parsing stored snapshots.

Current main-branch focus:

- keep the ledger intentionally stable while the active work on `main` is still in consensus, evidence, ordering, and sync
- preserve deterministic replay so V1 and V2 branch comparisons stay meaningful

## What the ledger currently supports

Today this crate supports only:

- native transfers
- validator-owned accounts
- deterministic replay

It does not yet support:

- smart contracts
- gas metering
- fees
- rewards
- slashing
- storage tries
- databases
- ordinary user wallet accounts as a separate system

So this is a deliberately minimal chain ledger.

## Why this crate matters

Even though it is small, it is already the source of truth for "what the chain state is."

Other crates rely on it for:

- mempool validation against current balances/nonces
- block execution
- recovery from stored blocks
- snapshot persistence

## Where we want to take it

This crate should grow into a fuller execution layer.

Future direction:

- support more transaction types
- separate validator identities from ordinary end-user accounts
- add fees, rewards, and later economic rules
- improve persistence beyond simple file-backed snapshots
- prepare for more advanced state structures if the project grows
- keep state transitions deterministic and easy to audit

In short: today this crate is a simple transfer ledger, but it is meant to become the clean execution core of Entangrid.
