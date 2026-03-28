# entangrid-crypto

## What this crate does

This crate defines the cryptographic interface for the rest of the project.

It answers questions like:

- how does a validator sign something?
- how is a signature verified?
- how do two validators derive session material?
- how do we hash a communication transcript?

The important design choice is that the rest of the code depends on traits, not on one hard-coded crypto library.

## How it currently works

Right now the crate contains:

- `Signer`
- `Verifier`
- `HandshakeProvider`
- `TranscriptHasher`
- `CryptoBackend`

and one concrete implementation:

- `DeterministicCryptoBackend`

That backend is a development-only placeholder.

Current behavior:

- each validator has a `dev_secret`
- signing is simulated by hashing `dev_secret || message`
- verification recomputes the same value
- session material is deterministically derived from:
  - both validators' secrets
  - both validator ids
  - a nonce

This is useful because it gives us:

- reproducible tests
- a working chain skeleton
- a working networking layer
- a clean place to swap in real algorithms later

## What it is not

This crate is **not post-quantum secure yet**.

The current backend is intentionally fake from a security perspective.

It is good enough for:

- localnet testing
- protocol shaping
- storage and networking integration

It is not good enough for:

- production security
- public deployment
- meaningful cryptographic claims

Current main-branch focus:

- stabilize the V2 protocol path on `main` first
- keep the crypto boundary clean while consensus, ordering, and sync are still changing
- only move to real PQ integration after the single-machine V2 matrix is green

## Why this crate matters

This abstraction is important because it lets the rest of the blockchain move forward without being blocked on the final PQ stack.

For example:

- `entangrid-node` can sign blocks
- `entangrid-network` can produce session observations
- `entangrid-ledger` can verify transaction signatures

all without caring which exact cryptographic primitives are underneath.

## Where we want to take it

This crate should eventually become the real cryptographic backbone of Entangrid.

Future direction:

- replace the deterministic dev backend with real post-quantum algorithms
- separate development mode and production mode clearly
- add proper key generation and key storage flows
- stop placing development secrets in genesis/config once real crypto is integrated
- support authenticated PQ-secure transport/session setup
- support key rotation and better identity management

In short: this crate is the seam where the project will later become genuinely post-quantum aware instead of only protocol-shaped around that goal.
