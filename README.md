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
- [Current Main-Branch Flow](docs/current-flow.md)
- [Protocol Specification](docs/protocol.md)
- [Threat Model](docs/threat-model.md)
- [Roadmap](docs/roadmap.md)
- [Localnet Plan](docs/localnet.md)
- [Benchmarking Plan](docs/benchmarks.md)
- [Consensus V2 Redesign Plan](docs/superpowers/plans/2026-03-25-entangrid-consensus-v2.md)
- [Consensus V2 Status Update](docs/superpowers/plans/entangrid-consensus-v2-status.md)
- [Consensus V2 Stabilization Plan](docs/superpowers/plans/2026-03-27-entangrid-v2-stabilization.md)

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
- a baseline receipt-driven path that still remains available when `consensus_v2` is disabled
- an active `consensus_v2` path on `main` that replaces raw local receipt-driven gating with witness-aligned service attestations, prior-epoch service aggregates, and the first QC-ordering slices

Important:

- the current backend is a deterministic development backend, not a production-strength post-quantum implementation
- production-strength PQ rollout still remains a later milestone behind the stable crypto interfaces already in place
- `stage-1/pq-integration` is now the active branch for Stage 1 PQ work:
  - typed signatures and typed public identities are in
  - node-local signing backend selection is in
  - an experimental ML-DSA signing backend now exists behind the `pq-ml-dsa` cargo feature
  - first-class hybrid signature and identity containers are now in for transactions, blocks, and proposal votes
  - permissive hybrid verification is in
  - strict hybrid localnet bootstrap is now available through `cargo run -p entangrid-sim --features "pq-ml-dsa pq-ml-kem" -- init-localnet --validators 4 --hybrid-enforcement --base-dir var/pq-hybrid-smoke`
  - that mode writes hybrid validator identities plus `session_public_identity` into genesis, generates one ML-DSA key file plus one ML-KEM session key file per node, selects `HybridDeterministicMlDsaExperimental` plus `HybridDeterministicMlKemExperimental`, enables `require_hybrid_validator_signatures = true`, and forces `consensus_v2 = true`
  - `--hybrid-enforcement` is an operational/bootstrap slice, not a full hybrid performance matrix
  - Stage 1G now adds a feature-gated hybrid session handshake behind `pq-ml-kem`
  - validators now keep signing identity and session identity separate
  - session setup is per-stream and mutually signed, then derives material from deterministic + ML-KEM components
  - Stage 1I now encrypts every post-handshake frame body automatically when the hybrid session backend is active
  - the handshake stays in cleartext, the outer frame length stays plaintext, and the frame body is protected after session establishment
  - Stage 1J now adds node-local hybrid session lifetime control through `NodeConfig.session_ttl_millis`
  - when that TTL is omitted, hybrid lanes default to a 10 minute lifetime while deterministic transport remains unchanged
  - outbound hybrid lanes lazily drop expired cached streams and reconnect through a fresh handshake before sending the pending frame
  - inbound hybrid lanes close expired streams before accepting the next application frame, so expiry stays transport-local instead of leaking into node logic
  - deterministic transport remains the default path when `pq-ml-kem` is off
  - session rotation and stronger traffic-shaping features still come later
  - relay receipts now join blocks and proposal votes under the strict hybrid-enforcement slice, while transactions and service evidence still remain outside it
- the older V1 baseline is preserved on the `codex/consensus-v1` branch and is still useful as a regression benchmark
- the active protocol work now happens on `main`
- `codex/consensus-v2` remains useful as a staging branch when we want isolated V2 experiments before merging back
- the redesign direction and the latest implementation status are documented in [docs/superpowers/plans/2026-03-25-entangrid-consensus-v2.md](docs/superpowers/plans/2026-03-25-entangrid-consensus-v2.md), [docs/superpowers/plans/entangrid-consensus-v2-status.md](docs/superpowers/plans/entangrid-consensus-v2-status.md), and [docs/superpowers/plans/2026-03-27-entangrid-v2-stabilization.md](docs/superpowers/plans/2026-03-27-entangrid-v2-stabilization.md)

## Current Main-Branch V2 Status

`main` now carries the active `consensus_v2` implementation behind config.

Current V2 shape:

- service evidence is produced by the validator's actual assigned witnesses, not by a separate ad hoc observer set
- each witness attests only to the obligations it can really observe for that validator
- nodes reconcile receipts for the just-finished epoch first, then publish attestations for the prior completed epoch with a one-epoch lag
- local proposer gating distinguishes:
  - confirmed low-score rejection
  - insufficient-evidence skip
  - no-evidence skip
- proposal votes and quorum certificates are live in the node runtime
- equal-QC uncertified sibling branches no longer replace the current canonical tip just because they gained extra local votes
- restarted nodes now suppress historical slot replay and can hold back local proposals behind a startup sync barrier while peers are still ahead
- certified sync responses now carry responder tip metadata so a catching-up node can continue with suffix repair instead of guessing whether the peer is still ahead

The main remaining V2 blockers are now:

- proving the final healthy and degraded `4/5/6/7/8` bursty matrix on current `main`
- tightening the simulator acceptance gates so regressions fail loudly
- real PQ integration only after the full matrix is green

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
- the current active correctness gate is still the single-machine healthy and degraded bursty matrix across `4/5/6/7/8`
- because of that, this policy should be treated as the current pre-PQ baseline, not as a final all-topology signoff

## Active Redesign Direction

The current prototype proved the core Entangrid idea is implementable, and `main` now carries the first real V2 stabilization slices, but the scaling work is not finished yet:

- the baseline local-receipt path is still too topology-sensitive at larger validator counts
- healthy `6/7/8` bursty runs now repeatedly shut down on one tip with QC-dominant branch choice active
- certified sync now skips stale certified suffixes instead of rolling a node back after it already advanced
- V2 service evidence and degraded punishment are materially better after the transport/session hardening on `main`
- stale-restart recovery is now fixed enough that a restarted validator can catch up without falling back to full snapshot sync
- the next step is to prove the full healthy/degraded matrix on current `main`, not to keep redesigning recovery again

That work is tracked in [docs/superpowers/plans/2026-03-25-entangrid-consensus-v2.md](docs/superpowers/plans/2026-03-25-entangrid-consensus-v2.md), [docs/superpowers/plans/entangrid-consensus-v2-status.md](docs/superpowers/plans/entangrid-consensus-v2-status.md), and [docs/superpowers/plans/2026-03-27-entangrid-v2-stabilization.md](docs/superpowers/plans/2026-03-27-entangrid-v2-stabilization.md).
Treat `main` as the active V2 development line, `codex/consensus-v1` as the benchmark branch, and the current architecture as not PQ-ready yet.

## Quickstart

Build the binaries:

```bash
cargo build --bins
```

Generate a four-node localnet:

```bash
cargo run -p entangrid-sim -- init-localnet --validators 4 --base-dir var/localnet
```

That default localnet still starts in the baseline path.
To run the active V2 line on `main`, pass `--consensus-v2` when you initialize the network.

To boot the strict hybrid Stage 1F slice, use:

```bash
cargo run -p entangrid-sim --features "pq-ml-dsa pq-ml-kem" -- init-localnet \
  --validators 4 \
  --hybrid-enforcement \
  --base-dir var/pq-hybrid-smoke
```

This mode now requires a build with both `pq-ml-dsa` and `pq-ml-kem`, writes hybrid validator identities plus `session_public_identity` into genesis, generates one `ml-dsa-key.json` plus one `ml-kem-session-key.json` per node, selects `HybridDeterministicMlDsaExperimental` plus `HybridDeterministicMlKemExperimental`, enables `require_hybrid_validator_signatures = true`, and forces `consensus_v2 = true`. It is meant as a bootstrap/operational smoke path, not as the full hybrid performance matrix.

To start that hybrid localnet, use the same feature-enabled simulator and the matching base directory:

```bash
cargo run -p entangrid-sim --features "pq-ml-dsa pq-ml-kem" -- up --base-dir var/pq-hybrid-smoke
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
The built-in matrix now also includes larger-validator healthy and degraded bursty scenarios because the current runtime goal is no longer just proving structural convergence on `6/7/8`, but also proving that stale and degraded nodes recover cleanly without regressing the V2 sync path.
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

To initialize a localnet with the active V2 path enabled on `main`:

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
