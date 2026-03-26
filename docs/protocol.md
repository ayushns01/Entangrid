# Protocol Specification

## Protocol Name

Working name: Entangrid.

This is the protocol direction for `Entangrid`:

- proposer selection uses public randomness
- network usefulness is measured through rotating witness obligations
- signed relay receipts feed a rolling topology score
- stake and service together determine proposer eligibility

Current status note:

- the `main` branch still implements the first working receipt-driven prototype of this idea
- that prototype is useful, but it is not yet the final validator-count-scalable form of the protocol
- the active redesign toward committee-attested service evidence and certificate-backed ordering is documented in [superpowers/plans/2026-03-25-entangrid-consensus-v2.md](superpowers/plans/2026-03-25-entangrid-consensus-v2.md)
- the latest status update for that redesign is in [superpowers/plans/entangrid-consensus-v2-status.md](superpowers/plans/entangrid-consensus-v2-status.md)

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

## Proposer Eligibility

The proposer rule should combine stake, public randomness, and relay performance.

Conceptually:

`eligible = sortition(seed, validator_key, slot) && relay_score >= threshold`

or

`effective_stake = stake * service_multiplier(relay_score)`

This lets the chain preserve public randomness while still punishing validators that do not contribute to network health.

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
