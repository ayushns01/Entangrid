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
- `build_crypto_backend(...)`

and two concrete backend paths:

- `DeterministicCryptoBackend`
- `MlDsa65Experimental` behind the `pq-ml-dsa` cargo feature
- a feature-gated hybrid session KEM path behind `pq-ml-kem`

The deterministic backend is still the default development path. The ML-DSA path is experimental for signing/authentication, and the ML-KEM path is experimental for session establishment.

Current behavior:

- each validator has a `dev_secret`
- signing is simulated by hashing `dev_secret || message`
- signatures are now returned as typed signatures with explicit scheme metadata
- verification dispatches by signature scheme instead of assuming one anonymous byte format
- session material is deterministically derived from:
  - both validators' secrets
  - both validator ids
  - a nonce

This is useful because it gives us:

- reproducible tests
- a working chain skeleton
- a working networking layer
- a clean place to swap in real algorithms later

The current deterministic backend emits `SignatureScheme::DevDeterministic`. That means the rest of the system can now distinguish:

- what bytes were signed
- which scheme produced them

without changing consensus rules yet.

The current PQ branch now also supports:

- node-local backend selection through `NodeConfig.signing_backend`
- optional ML-DSA signing key loading through `NodeConfig.signing_key_path`
- validator identity checks at startup so a node cannot silently sign as a mismatched public identity
- scheme-aware verification for both deterministic and ML-DSA signatures
- hybrid deterministic + ML-DSA signing through `HybridDeterministicMlDsaExperimental`
- permissive verification against hybrid identities so single-scheme and hybrid signatures can coexist during rollout
- an opt-in `require_hybrid_validator_signatures` mode in node policy that now enforces hybrid validator identities plus hybrid transaction/block/proposal-vote/relay-receipt/service-attestation signatures
- the simulator's strict hybrid localnet bootstrap path now requires `pq-ml-dsa` and `pq-ml-kem`, writes both signing and session key files, and turns on `require_hybrid_validator_signatures = true` together with `consensus_v2 = true`
- a separate session identity/config surface through `ValidatorConfig.session_public_identity`, `NodeConfig.session_backend`, and `NodeConfig.session_key_path`
- a feature-gated per-stream handshake behind `pq-ml-kem` using `SessionClientHello` / `SessionServerHello`
- mutually signed handshake transcripts that bind the transport session to validator signing identities
- session material derived from the existing deterministic component plus an ML-KEM component when `HybridDeterministicMlKemExperimental` is selected
- Stage 1I encrypted framing over that hybrid session using ChaCha20-Poly1305 for every post-handshake frame body while leaving the handshake and outer frame length plaintext
- Stage 1J transport-local session lifecycle hardening through `NodeConfig.session_ttl_millis`, with a 10 minute default TTL on hybrid lanes, lazy outbound reconnect on expiry, and clean inbound close semantics while keeping expiry policy out of the crypto backend itself

Current enforcement boundary:

- blocks: enforced when the flag is on
- proposal votes, including votes imported via quorum certificates: enforced when the flag is on
- transactions: enforced when the flag is on
- relay receipts: enforced when the flag is on
- service attestations: enforced when the flag is on
- service aggregates: inherit enforcement transitively because aggregate validation re-validates each embedded attestation
- hybrid transport sessions now authenticate the handshake and encrypt later frame bodies automatically

## How to measure ML-DSA signing right now

Stage 1C on `stage-1/pq-integration` adds a report-first measurement flow.

Backend-local measurement:

```bash
cargo test -p entangrid-crypto --features pq-ml-dsa measurement
```

Small sim-side block/proposal-vote proxy report:

```bash
cargo run -p entangrid-sim --features pq-ml-dsa -- pq-measure \
  --validators 4 \
  --iterations 32 \
  --output-path test-results/pq-ml-dsa-measurements.md
```

What this currently measures:

- deterministic vs ML-DSA public identity size
- deterministic vs ML-DSA signature size
- deterministic vs ML-DSA sign latency
- deterministic vs ML-DSA verify latency
- deterministic vs ML-DSA serialized block/proposal-vote proxy size

What it does not measure yet:

- KEM/session overhead
- hybrid dual-signature policy
- full localnet benchmark gates

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
- finish the remaining bursty `6`-validator consensus proof while keeping the current PQ boundary stable

Current PQ branch focus:

- on `stage-1/pq-integration`, the first implementation slices are:
  - make signing and identity types scheme-aware
  - add node-local backend selection
  - add an experimental ML-DSA signing backend behind `pq-ml-dsa`
  - add a feature-gated hybrid ML-KEM session handshake behind `pq-ml-kem`
- that branch now covers signing/authentication, strict hybrid enforcement across transactions and consensus-relevant validator evidence, hybrid session establishment, encrypted framing, and transport-local TTL turnover
- the remaining blocker on that line is not missing PQ crypto plumbing; it is the last bursty `6`-validator consensus proof
- session rotation and richer transport hardening are explicitly deferred to the post-Stage-1 hardening milestone

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
