# Verification Guide

This document is the canonical "what to run, what it proves, and what result to expect" guide for the current `main` branch.

The goal is not to claim more than the project has proved.
The current line has substantial PQ, sync, and consensus functionality in place, but the rigorous acceptance matrix is not fully green yet.

Current expected top-line status:

- the PQ plumbing is implemented end to end
- proposal votes, quorum certificates, certified sync, and QC-aware recovery are live
- the latest rigorous localnet matrix is `12/14`
- the remaining open scenarios are `baseline-6-bursty` and `gated-6-bursty`

For the current branch-level status, see:

- [pq-stage-1-status.md](pq-stage-1-status.md)
- [v2-issue-status.md](v2-issue-status.md)
- [consensus-current-issue.md](consensus-current-issue.md)

## 1. Build Smoke

Use this first to confirm the workspace builds and the binaries are available:

```bash
cargo build --bins
```

What this proves:

- the workspace resolves and compiles
- the `entangrid-node` and `entangrid-sim` binaries build on the current toolchain

## 2. Core Workspace Test Smoke

Run the main crate suites without PQ feature flags:

```bash
cargo test -p entangrid-types
cargo test -p entangrid-consensus
cargo test -p entangrid-ledger
cargo test -p entangrid-network
cargo test -p entangrid-node
cargo test -p entangrid-sim
```

What this proves:

- shared types, consensus, ledger, transport, node runtime, and simulator all still pass their default-path regression coverage
- the deterministic development path remains intact

## 3. PQ Stage 1 Feature Verification

Run the current PQ-enabled validation set:

```bash
cargo test -p entangrid-crypto --features "pq-ml-dsa pq-ml-kem"
cargo test -p entangrid-network --features pq-ml-kem
cargo test -p entangrid-node --features "pq-ml-dsa pq-ml-kem"
cargo test -p entangrid-sim --features "pq-ml-dsa pq-ml-kem"
cargo test -p entangrid-sim --features "pq-ml-dsa pq-ml-kem" hybrid_enforcement_localnet_boot_smoke_test -- --ignored
```

What this proves:

- ML-DSA and ML-KEM feature-gated code paths compile and pass their current regression coverage
- hybrid enforcement wiring is alive in the node and simulator path
- strict hybrid localnet bootstrap still works as an end-to-end smoke path

Notes:

- this is still experimental PQ integration, not a production-hardening claim
- the strict hybrid bootstrap path is an operational smoke slice, not a full benchmark signoff

## 4. PQ Measurement Commands

Use these when you want measurement output instead of just pass/fail verification:

```bash
cargo test -p entangrid-crypto --features pq-ml-dsa measurement

cargo run -p entangrid-sim --features pq-ml-dsa -- pq-measure \
  --validators 4 \
  --iterations 32 \
  --output-path test-results/pq-ml-dsa-measurements.md
```

What this proves:

- deterministic vs ML-DSA size and latency comparisons still run
- the simulator-side measurement/reporting path remains intact

What it does not prove:

- full hybrid transport performance signoff
- consensus stability under PQ features at matrix scale

## 5. Default Localnet Runtime Smoke

Generate and run a baseline localnet:

```bash
cargo run -p entangrid-sim -- init-localnet --validators 4 --base-dir var/localnet
cargo run -p entangrid-sim -- up --base-dir var/localnet
```

In another terminal, inject traffic and report:

```bash
cargo run -p entangrid-sim -- load --base-dir var/localnet --scenario steady --duration-secs 12
cargo run -p entangrid-sim -- report --base-dir var/localnet
```

What this proves:

- localnet generation works
- validator processes boot and communicate
- transactions propagate through the runtime
- metrics and report generation still function

## 6. Hybrid Localnet Smoke

Generate and run the strict hybrid smoke path:

```bash
cargo run -p entangrid-sim --features "pq-ml-dsa pq-ml-kem" -- init-localnet \
  --validators 4 \
  --hybrid-enforcement \
  --base-dir var/pq-hybrid-smoke

cargo run -p entangrid-sim --features "pq-ml-dsa pq-ml-kem" -- up --base-dir var/pq-hybrid-smoke
```

What this proves:

- hybrid validator identities and session identities are emitted into generated config/genesis
- ML-DSA and ML-KEM-enabled runtime paths can boot together
- hybrid enforcement and hybrid transport lanes can be exercised in a real local multi-process run

## 7. Service-Gating Demo

Use this to verify the degraded-validator path explicitly:

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

What this proves:

- service-gating configuration is being applied through generated node configs
- a degraded validator can be penalized through the current service-score model
- the report path exposes the counters behind the latest score

Inspect these files after the run:

- `var/localnet-gated/node-4/events.log`
- `var/localnet-gated/node-4/metrics.json`

## 8. Rigorous Matrix

Run the built-in rigorous localnet matrix:

```bash
cargo run -p entangrid-sim -- matrix \
  --base-dir var/localnet-matrix \
  --output-dir test-results \
  --settle-secs 18
```

What this proves:

- current convergence, gating, recovery, and abuse-control behavior across the maintained scenario suite
- the simulator can still drive healthy, degraded, and adversarial localnet scenarios and write structured reports

Where results appear:

- `test-results/`

Current expected result on `main`:

- `12/14` scenarios pass
- `baseline-6-bursty` is still open
- `gated-6-bursty` is still open

Important:

- do not describe the matrix as green until it actually reaches `14/14`
- isolated reruns of the hard `6`-validator case can look better than the sequential matrix result; the acceptance gate is still the rigorous matrix, not the best isolated run

## 9. Targeted Runtime Checks

When you want focused runtime evidence instead of full-matrix cost, these targeted node suites are the most useful current spot checks:

```bash
cargo test -p entangrid-node sync_
cargo test -p entangrid-node gating
cargo test -p entangrid-node quorum_certificate
cargo test -p entangrid-node proposal_votes_build_qc_at_supermajority_threshold
```

What this proves:

- sync and recovery paths are still covered directly
- proposer-gating logic still passes focused coverage
- QC formation and QC-related runtime behavior still pass focused regressions

## 10. Reading Results Honestly

Use this interpretation model when reporting project status:

- "implemented" means the code path exists and has direct coverage
- "verified" means the documented command set passes on the current line
- "matrix-stable" means the rigorous localnet matrix is green
- "production-ready" should not be claimed from this repo today

Safe current summary:

- Stage 1 PQ signing, transport, and hybrid bootstrap integration are implemented
- V2 runtime machinery including QCs and certified sync is live
- the current line still has two unresolved bursty `6`-validator consensus scenarios in the rigorous matrix
