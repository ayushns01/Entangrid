# Entangrid

`Entangrid` is a post-quantum blockchain research project in Rust built around a deliberately unusual idea:

consensus should care about how well a validator helps the network move real data, not just how much stake it holds.

The project started from "network entanglement" and evolves it into a stronger protocol:

- validators establish post-quantum secure sessions with assigned witnesses
- witnesses issue signed relay receipts for packets, transactions, and blocks actually forwarded on time
- each validator commits those receipts into a topology commitment
- proposer eligibility depends on stake plus a rolling relay score derived from those commitments

This keeps the original spirit of entangling networking and consensus, but avoids the weakest part of the first idea: using raw session secrets as the lottery input.

## Core Protocol Idea

The protocol direction documented in this repository is:

- post-quantum identities and signatures for validators and transactions
- post-quantum key exchange for peer sessions
- rotating witness assignments per epoch
- relay receipts that prove timely forwarding behavior
- topology commitments that summarize a validator's observed service to the network
- proposer election that uses unbiased randomness, then gates or weights eligibility by relay performance

The project goal is not to beat Ethereum or Bitcoin. The goal is to build a real multi-node system, stress it on one machine first, and learn where the bottlenecks and attack surfaces appear when post-quantum cryptography and network-coupled incentives meet.

## Why This Version Is Stronger

The original concept tied block selection directly to live shared secrets from active connections. That was creative, but it had serious problems:

- it was hard for the rest of the network to verify
- it encouraged connection grinding and Sybil peers
- it measured open sockets more than useful relay work
- it made consensus liveness too dependent on raw connectivity

This repository instead documents a more defensible design:

- randomness stays public and auditable
- network contribution is measured through signed witness evidence
- relay work matters more than connection count
- the system can be simulated, benchmarked, and attacked in a controlled way

## Documentation Index

- [Architecture](docs/architecture.md)
- [Protocol Specification](docs/protocol.md)
- [Threat Model](docs/threat-model.md)
- [Roadmap](docs/roadmap.md)
- [Localnet Plan](docs/localnet.md)
- [Benchmarking Plan](docs/benchmarks.md)

## MVP Scope

The first milestone should stay intentionally small:

- fixed validator set
- account-based ledger
- signed transfer transactions only
- one-machine local multi-node network
- baseline proposer selection before advanced entanglement rules
- witness-assigned relay receipts
- metrics for handshake cost, propagation latency, CPU, memory, and bandwidth

## Non-Goals For V1

- smart contracts
- permissionless validator onboarding
- slashing economics
- tokenomics design
- internet-scale peer discovery
- production hardening

## Proposed Workspace Shape

When implementation starts, this repository should become a Cargo workspace with crates similar to:

- `crypto`
- `types`
- `network`
- `ledger`
- `consensus`
- `node`
- `sim`

## Current Implementation Status

The repository now includes a working Rust workspace with:

- shared protocol/config types
- deterministic mock crypto for local development
- file-backed ledger state and block logs
- baseline proposer selection and witness assignment logic
- TCP-based static-peer networking
- a CLI node binary and localnet simulator
- block commitments backed by explicit receipt bundles in the runtime prototype
- a direct-delivery witness model where relay targets act as the current prototype witnesses for receipt generation
- an experimental `consensus_v2` path that replaces local receipt-driven gating with witness-aligned service attestations and prior-epoch service aggregates

Important:

- the current backend is a deterministic development backend, not a production-strength post-quantum implementation
- real PQ signatures and key exchange remain a later milestone behind the stable crypto interfaces already in place

## Experimental Consensus V2 Status

This branch also carries the first real `consensus_v2` implementation behind config.

Current V2 shape:

- service evidence is produced by the validator's actual assigned witnesses, not by a separate ad hoc observer set
- each witness attests only to the obligations it can really observe for that validator
- nodes reconcile receipts for the just-finished epoch first, then publish attestations for the prior completed epoch with a one-epoch lag
- proposer gating reads the latest available aggregate from an earlier epoch instead of dropping straight to zero when the newest aggregate is still in flight

Current live V2 bursty status is mixed:

- `4 validators`: healthy, converged, and no false gating in the latest run
- `6 validators`: service scores are now healthy, but ordering still diverges under bursty load
- `8 validators`: service evidence is much better than before, but still not strong enough to avoid some score collapse and partial convergence trouble

So the main remaining V2 blockers are now:

- QC-backed ordering and fork choice
- further validator-count-aware service coverage improvement at larger topologies

## Current Recommended Prototype Policy

The current prototype policy for Entangrid-style localnet work is still:

- `service_gating_start_epoch = 3`
- `service_gating_threshold = 0.40`
- `service_score_window_epochs = 4`
- `service_score_weights = [0.25 uptime, 0.50 delivery, 0.25 diversity, 1.00 penalty]`

This is still the best current 4-validator policy baseline:

- earlier gating start epochs risk startup noise
- lower thresholds like `0.25` are viable, but they have not shown a clear advantage over the current midpoint
- higher thresholds like `0.55` are also viable, but they add more gating pressure than we need to adopt as the default yet
- a `1`-epoch window is more reactive but more brittle
- an `8`-epoch window is smoother but slower to reflect degradation
- a penalty weight of `1.00` remains the clearest neutral default while we continue policy tuning

Important:

- the harsh 4-validator Entangrid scenarios and abuse scenarios are currently in a much better place than before
- the current live matrix still shows that 6-validator bursty scenarios do not reconverge quickly enough after the normal settle window
- because of that, this policy should be treated as the current 4-validator pre-PQ baseline, not as a final all-topology signoff

## Quickstart

Build the binaries:

```bash
cargo build --bins
```

Generate a four-node localnet:

```bash
cargo run -p entangrid-sim -- init-localnet --validators 4 --base-dir var/localnet
```

Start the localnet:

```bash
cargo run -p entangrid-sim -- up --base-dir var/localnet
```

`up` rebuilds `entangrid-node` before launch, and for a brand-new localnet it will nudge the fresh genesis time forward if you waited too long between `init-localnet` and `up`.
During runtime, nodes now broadcast lightweight sync status on the sync tick, answer explicit sync requests with incremental block catch-up when possible, and fall back to full snapshots when a peer is unknown, clearly on a different branch, or simply far enough behind that stale incremental sync would be wasteful. Healthy peers also proactively push the best available sync bundle to peers that still look stale, so degraded validators can recover without the old constant full-snapshot broadcast.

Inject steady transfer traffic from another terminal:

```bash
cargo run -p entangrid-sim -- load --base-dir var/localnet --scenario steady --duration-secs 12
```

Summarize the latest localnet metrics:

```bash
cargo run -p entangrid-sim -- report --base-dir var/localnet
```

Run a service-gating demo with one degraded validator:

```bash
cargo run -p entangrid-sim -- init-localnet \
  --validators 4 \
  --base-dir var/localnet-gated \
  --slot-duration-millis 1000 \
  --slots-per-epoch 5 \
  --enable-service-gating \
  --service-gating-start-epoch 3 \
  --service-gating-threshold 0.40 \
  --service-score-window-epochs 4 \
  --service-score-penalty-weight 1.00 \
  --degraded-validator 4 \
  --degraded-drop-probability 0.85

cargo run -p entangrid-sim -- up --base-dir var/localnet-gated

cargo run -p entangrid-sim -- load --base-dir var/localnet-gated --scenario steady --duration-secs 15

cargo run -p entangrid-sim -- report --base-dir var/localnet-gated
```

Run the built-in rigorous localnet matrix and write reports into `test-results/`:

```bash
cargo run -p entangrid-sim -- matrix \
  --base-dir var/localnet-matrix \
  --output-dir test-results \
  --settle-secs 18
```

The matrix runner now waits for convergence during the settle window, captures reports at that converged moment, checks scenario-specific scoring/gating expectations, and then asks nodes to shut down cleanly, so the generated summaries are a much better fit for regression checking. Those expectations now cover both sides of the policy: harsh degraded runs must actually gate the targeted validator, baseline runs must keep honest validators above a minimum score floor, and the policy-sweep cases now track how many non-target validators fell below threshold or suffered gating fallout under different threshold, score-window, and penalty-weight settings.
The built-in matrix now also includes a healthy `gated-6-bursty` scenario, which is useful because it currently exposes the remaining convergence gap under heavier multi-validator traffic instead of silently hiding it.
The recommended prototype defaults above are the same values used by the current shared config defaults, so a plain gated `init-localnet` run now starts from the current 4-validator matrix-selected policy instead of an older warmup profile.
The localnet reports now also surface the penalty inputs behind the latest score, including failed session counts and invalid receipts, so threshold, weight, and window tuning is easier to inspect from one run to the next.
The built-in matrix also includes abuse-control scenarios now, so we can verify that sync-control floods trip peer rate limits and inbound connection floods trip listener session caps without breaking the Entangrid-specific degraded-validator cases.
Recent hardening also tightened two protocol-surface issues found during adversarial review:

- sync snapshot adoption now validates incoming blocks structurally instead of trusting raw ledger replay alone
- sync snapshot adoption now validates incoming receipt bundles before they can affect local service scoring
- inbound network frames now have a fixed maximum size to avoid unbounded allocation on a malicious length prefix
- sync now uses `SyncStatus` plus incremental block segments for same-chain peers, with per-peer request throttling and full snapshots kept as the safe fallback
- inbound session handling is now capped, and nodes apply per-peer rate limits to spam-prone sync/receipt/tx gossip before that traffic reaches more expensive logic
- the matrix now reports total `peer_rate_limit_drops` and `inbound_session_drops` so abuse-control regressions show up in the same report as convergence and gating outcomes
- the matrix now also records non-target below-threshold counts and non-target gating rejections, which makes threshold/window tuning much easier to judge from one report
- service-score weights are now configurable through localnet config, so matrix sweeps can compare not only thresholds and windows but also how strongly penalty counters pull scores down
- failed-session penalties now only count once a relay target is known live, which avoids treating the earliest bootstrap churn as definitive service failure
- the transport now retries transient outbound connect failures before surfacing them as hard network failures
- remote block acceptance no longer rejects peer blocks based on the local node's private service-score view; proposer gating is currently enforced only on local block production until the project has a stronger shared score source

Then inspect:

- `node-4/events.log` for missed slots due to low service score
- `node-4/metrics.json` for `service_gating_rejections`, `duplicate_receipts_ignored`, `peer_rate_limit_drops`, `inbound_session_drops`, `service_gating_start_epoch`, the latest local score, and the latest local service counters, including any failed-session or invalid-receipt penalties that contributed to that score

To initialize a localnet with the experimental V2 path enabled:

```bash
cargo run -p entangrid-sim -- init-localnet \
  --validators 4 \
  --base-dir var/localnet-v2 \
  --enable-service-gating \
  --consensus-v2
```

## Guiding Principle

The chain should not reward a validator merely for being online.

It should reward validators that are online, reachable, diverse, and provably helpful in moving consensus-critical data across the network.
