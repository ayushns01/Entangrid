# Protocol Specification

## Protocol Name

Working name: Entangrid.

This is the protocol direction for `Entangrid`:

- proposer selection uses public randomness
- network usefulness is measured through rotating witness obligations
- signed relay receipts feed a rolling topology score
- stake and service together determine proposer eligibility

Current status note:

- `main` now carries the active `consensus_v2` work behind config
- the baseline receipt-driven path still exists for comparison when `consensus_v2` is disabled and is preserved on `codex/consensus-v1`
- committee-attested service evidence, confirmed prior-epoch gating, certified sync, and QC-dominant branch choice are live on `main`
- restart-time slot suppression, startup sync barriers, and QC-aware certified-sync adoption are now also live on `main`
- stale-node restart recovery is now fixed enough on `main`, and the next step is proving the final matrix before PQ integration
- on `stage-1/pq-integration`, hybrid-signature enforcement is now available behind `require_hybrid_validator_signatures` for validator startup, blocks, and proposal votes
- Stage 1F plus Stage 1H add a strict hybrid localnet bootstrap mode via `init-localnet --hybrid-enforcement`; it requires `pq-ml-dsa pq-ml-kem`, writes hybrid validator identities plus `session_public_identity` into genesis, generates one ML-DSA key file plus one ML-KEM session key file per node, selects `HybridDeterministicMlDsaExperimental` plus `HybridDeterministicMlKemExperimental`, enables `require_hybrid_validator_signatures = true`, and forces `consensus_v2 = true`
- that Stage 1F mode is an operational/bootstrap slice, not the full hybrid performance matrix

On the active `consensus_v2` path on `main`, the protocol direction is tightening further:

- witnesses produce scoped `ServiceAttestation` records for the validators they were actually assigned to observe
- nodes build `ServiceAggregate` records from those attestations
- proposer gating reads prior-epoch aggregate evidence instead of raw local receipt views
- proposal votes, quorum certificates, certified sync, and QC-dominant canonical branch choice are live
- certified sync now rejects stale certified responses that would otherwise downgrade a newer local certified tip
- certified sync responses now also advertise the responder's current tip metadata so the requester can follow a certified catch-up with suffix repair
- stale restarted nodes now suppress historical replay, hold proposals behind the startup barrier, advertise the certified frontier during recovery, and proactively request catch-up while peers remain ahead
- the next blocker is no longer restart recovery itself, but proving the full matrix and freezing the acceptance gates before PQ work

## Protocol Goals

- keep the system post-quantum from the application layer upward
- couple consensus to useful network behavior
- make that coupling publicly auditable
- resist simple connection grinding and Sybil amplification
- stay small enough to prototype on a single machine

## Actors

### Validator

A stake-bearing node allowed to propose and validate blocks.

### Witness

A validator assigned to observe and confirm another validator's relay behavior during an epoch.

### Peer

A network connection endpoint. In V1 every validator is also a peer, but later the network can distinguish between validator peers and non-validator relay peers.

## Epoch Structure

Each epoch has five phases.

### 1. Seed Derivation

The chain derives an epoch seed from the previous finalized state.

This seed is public and deterministic.

### 2. Witness Assignment

Using the epoch seed, the protocol assigns each validator a rotating set of witnesses and relay targets.

Assignment rules should favor:

- diversity across validator identities
- limits on repeated pairings
- limits on same-subnet concentration
- predictable workload per validator

### 3. Session Establishment

Assigned peers open authenticated post-quantum sessions.

The intended shape is:

- validator identity authentication
- post-quantum key exchange
- transcript hash creation
- symmetric transport keys derived from the exchange

The shared secret is used only to derive transport keys.

It is not used directly as consensus randomness.

### 4. Relay Window

During the epoch, validators must relay three classes of traffic:

- heartbeat pulses
- transactions
- block fragments or block announcements

Witnesses observe whether relays arrive on time and in the correct order.

### 5. Commitment and Scoring

Witnesses issue signed relay receipts.

Each validator aggregates these into a topology commitment for the epoch.

The next proposer selection step uses stake plus the rolling relay score derived from recent commitments.

Current V2 runtime detail:

- witnesses first reconcile receipts for the just-finished epoch
- attestations are then published for the prior completed epoch with a one-epoch lag
- this is deliberate and avoids false zero-score epochs caused by late receipt propagation

## Relay Receipts

A relay receipt is a compact, signed record proving that a validator relayed a required message under a specific witness assignment.

Suggested fields:

- epoch
- slot or window id
- source validator id
- destination validator id
- witness validator id
- message class
- transcript digest
- latency bucket
- byte count bucket
- monotonic sequence number
- witness signature

Receipts should avoid large payloads.

They should commit to message digests, counters, and timing buckets rather than raw packet bodies.

## Topology Commitment

Each validator produces a topology commitment from the set of valid relay receipts accumulated during the scoring window.

Suggested structure:

- receipt hashes sorted canonically
- Merkle root over receipt hashes
- summary counters per message class
- diversity counters
- rolling relay score

This commitment is included in the block body or block metadata.

## Relay Score

The relay score should reward useful and diverse service, not raw connection count.

A simple V1 model:

`relay_score = uptime_score + delivery_score + diversity_score - penalties`

Where:

- `uptime_score` measures successful witness windows
- `delivery_score` measures timely forwarded traffic
- `diversity_score` rewards distinct assigned peers actually served
- `penalties` capture missed windows, replay attempts, invalid receipts, and excessive failed sessions

Important constraints:

- cap the benefit of repeated receipts from the same peer set
- count only assigned witness relationships
- use rolling windows so short failures do not permanently destroy eligibility
- make all score inputs verifiable from on-chain commitments and signed receipts

Current V2 constraint:

- proposer gating now rejects only on a confirmed prior-epoch low score
- when prior-epoch aggregate evidence is missing or insufficient, the runtime skips enforcement instead of treating temporary evidence delay as confirmed service failure

## Proposer Eligibility

The proposer rule should combine stake, public randomness, and relay performance.

Conceptually:

`eligible = sortition(seed, validator_key, slot) && relay_score >= threshold`

or

`effective_stake = stake * service_multiplier(relay_score)`

This lets the chain preserve public randomness while still punishing validators that do not contribute to network health.

Current V2 note:

- the service side of this rule is live on `main` behind `consensus_v2`
- the ordering side now includes proposal votes, quorum certificates, certified sync, and QC-dominant branch selection
- the remaining pre-PQ gap is reliable service scoring and gating behavior at larger validator counts

Current PQ Stage 1E note on `stage-1/pq-integration`:

- hybrid enforcement is opt-in, not default
- when enabled, every validator in genesis must advertise a hybrid public identity
- validator-originated block signatures must be hybrid
- validator-originated proposal-vote signatures, including votes imported through quorum certificates, must be hybrid
- transactions, receipts, and service evidence remain outside this enforcement slice

Current PQ Stage 1I note on `stage-1/pq-integration`:

- session identity is now separate from signing identity
- the transport layer can run a feature-gated per-stream hybrid handshake behind `pq-ml-kem`
- each handshake is mutually signed with the validators' existing signing identities
- the derived session material mixes the existing deterministic component with an ML-KEM component
- once that hybrid session is established, every later frame body is encrypted and authenticated automatically
- deterministic session establishment remains the default path when the feature is off
- session rotation is still out of scope for this slice

## Why This Is Better Than Raw Shared-Secret Lotteries

This protocol rejects the original shortcut:

`lottery_ticket = H(all live shared secrets)`

That shortcut is appealing, but flawed:

- it is private and hard to audit
- validators can bias it by connection churn
- it rewards open sessions more than useful service
- it encourages Sybil expansion

Relay receipts and topology commitments keep the entanglement idea while making it observable and testable.

## Block Content

At minimum, a block should include:

- header
- parent hash
- slot number
- proposer id
- state root
- transaction root
- topology commitment root
- block signature
- transaction list
- compact commitment summary

Detailed receipt bodies can live off the critical validation path and be requested on demand if the commitment is disputed.

Current PQ Stage 1 direction:

- signature-bearing protocol objects are moving from anonymous byte signatures to typed signatures with explicit scheme metadata
- this stage keeps consensus behavior unchanged while making the wire and state model crypto-agile
- validator public identity stays in genesis while node-local signer choice and key loading now live in node config
- the first experimental PQ signing path is ML-DSA-65 behind the `pq-ml-dsa` cargo feature
- hybrid signature and identity bundles now exist for the core signed objects
- verification is permissive during rollout, so matching deterministic-only, ML-DSA-only, and hybrid signatures can all validate
- hybrid policy enforcement is now available for validator startup, blocks, and proposal votes behind the current rollout flag, while wider enforcement across the rest of the signed surfaces still comes later
- Stage 1F and Stage 1H extend this with a strict hybrid localnet bootstrap path that turns the hybrid signing and session policy into runnable simulator output, but only for the operational smoke case described above

## Slashing And Rewards

V1 should not implement full economics.

Instead, document the rule shape:

- repeated failure to satisfy witness obligations reduces service multiplier
- invalid receipts or forged commitments trigger severe penalties in later versions
- rewards should eventually depend on both block production and relay usefulness

## V1 Simplifications

- fixed validator set
- no delegation
- no dynamic staking
- no smart contracts
- bounded block size
- bounded receipt count per epoch
- static localnet discovery

## Research Questions

- how expensive are receipt signatures under sustained load
- how large can commitments become before validation slows block time
- how much diversity is needed before Sybil gains flatten out
- how sensitive should proposer eligibility be to short-term network failures
- should witness assignments be single-hop only or multi-hop paths

## Distinctive Extension To Explore Later

A later version can move from simple witness pairs to witness-assigned relay paths.

In that model, the epoch seed assigns short multi-hop relay corridors, and validators earn score only when data traverses the full corridor within deadline windows.

That would measure path usefulness, not just pairwise connectivity, and would make the protocol even more tightly coupled to real network topology.
