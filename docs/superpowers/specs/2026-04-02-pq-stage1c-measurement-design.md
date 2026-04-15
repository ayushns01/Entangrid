# Stage 1C PQ ML-DSA Measurement Design

**Date:** 2026-04-02
**Branch:** `stage-1/pq-integration`
**Status:** Drafted from approved design discussion

## Goal

Add a reproducible measurement layer for the experimental ML-DSA signing path so Entangrid can quantify PQ signing costs before introducing benchmark gates or changing protocol policy.

Stage 1C builds on the typed-signature foundations from Stage 1 and the feature-gated ML-DSA backend from Stage 1B. It does not change consensus rules, transport semantics, or session behavior. It only measures the cost of the current signing path and records those results in repo-visible reports.

## Why This Stage Exists

Entangrid now has:

- typed, scheme-aware signatures
- a deterministic development backend
- a real experimental ML-DSA signing backend behind `pq-ml-dsa`

That is enough to make PQ signing real, but it is not enough to make good protocol decisions. Before introducing hybrid policy, session/KEM work, or performance gates, the repo needs a reproducible answer to questions like:

- how large are ML-DSA public keys and signatures in this codebase?
- how much slower is ML-DSA signing and verification than the deterministic path?
- does ML-DSA materially change block and vote propagation in a small localnet?

Stage 1C exists to answer those questions without mixing measurement work with policy changes.

## Scope

### In Scope

- measure deterministic vs ML-DSA signature size
- measure deterministic vs ML-DSA public identity size
- measure deterministic vs ML-DSA signing latency
- measure deterministic vs ML-DSA verification latency
- add one small localnet comparison for block and proposal-vote overhead
- write measurement results to human-readable repo-visible reports
- document how to run and interpret the measurements

### Out of Scope

- hard pass/fail benchmark thresholds
- consensus tuning based on PQ measurements
- ML-KEM or transport/session benchmarking
- hybrid dual-signature policy
- large topology or adversarial performance sweeps
- automatic CI gating on measured values

## Architecture

### 1. Two-Layer Measurement Model

Stage 1C should measure PQ cost in two layers:

- **backend-local measurement** close to [`crates/entangrid-crypto/src/lib.rs`](../../../../crates/entangrid-crypto/src/lib.rs)
- **small localnet comparison** through [`crates/entangrid-sim/src/lib.rs`](../../../../crates/entangrid-sim/src/lib.rs)

This split is important.

Backend-local measurement tells us the direct cryptographic cost:

- key size
- signature size
- sign latency
- verify latency

The localnet comparison tells us whether those costs show up materially in:

- block proposal propagation
- proposal-vote propagation
- overall small-topology overhead

That gives Entangrid a clean separation between raw crypto cost and observed protocol impact.

### 2. Keep Measurement Close To The Real Backend

Measurements should use the same backend factory and typed-signature path that production code uses today. That means:

- deterministic measurements should use the configured deterministic backend
- ML-DSA measurements should use the real `pq-ml-dsa` feature path
- no fake PQ-shaped signatures
- no separate ad hoc signing format

This keeps the numbers honest and ensures the reported sizes match the actual serialized types moving through the repo.

### 3. Report-First, Not Gate-First

Stage 1C should produce human-readable reports, not benchmark gates.

Recommended outputs:

- a checked-in or repo-visible markdown report under [`test-results/`](../../../../test-results)
- documentation updates in:
  - [`docs/benchmarks.md`](../../../../docs/benchmarks.md)
  - [`crates/entangrid-crypto/README.md`](../../../../crates/entangrid-crypto/README.md)

This stage should not introduce “ML-DSA must be under X ms” assertions yet. The purpose is to collect stable measurements first and only decide later whether specific thresholds make sense.

## Output Shape

### Crypto Measurement Report

The crypto-side report should include at minimum:

- deterministic public identity size
- ML-DSA public identity size
- deterministic signature size
- ML-DSA signature size
- deterministic median sign latency
- ML-DSA median sign latency
- deterministic median verify latency
- ML-DSA median verify latency

It should also include enough metadata to make runs interpretable:

- feature flag used
- iteration count
- machine-local caveat that results are indicative, not universal

### Localnet Comparison Report

The localnet report should stay small and focused. It should compare deterministic vs ML-DSA in one narrow scenario, for example a small healthy bursty run that exercises block proposals and proposal votes without trying to benchmark the whole V2 system.

The localnet comparison should report:

- validator count
- run duration
- signature backend used
- block proposal latency or proxy event timing
- proposal-vote timing or proxy event timing
- any immediately visible wire-size impact if available from existing metrics

This is a protocol-impact sample, not a full benchmark matrix.

## File And Responsibility Shape

Stage 1C should keep responsibilities clear:

- [`crates/entangrid-crypto/src/lib.rs`](../../../../crates/entangrid-crypto/src/lib.rs)
  - provide measurement helpers and backend-local tests
- [`crates/entangrid-sim/src/lib.rs`](../../../../crates/entangrid-sim/src/lib.rs)
  - add a small deterministic-vs-ML-DSA comparison entrypoint or helper
- [`docs/benchmarks.md`](../../../../docs/benchmarks.md)
  - explain where PQ signing measurements fit in the benchmark story
- [`crates/entangrid-crypto/README.md`](../../../../crates/entangrid-crypto/README.md)
  - document how to run the crypto measurement flow
- [`test-results/`](../../../../test-results)
  - store the generated human-readable report

The measurement code should not leak benchmark concerns into consensus logic.

## Verification Strategy

### Crypto Verification

Stage 1C should include a reproducible command for backend-local measurements, for example:

```bash
cargo test -p entangrid-crypto --features pq-ml-dsa
```

This run should either generate the report directly or validate the helper that generates it.

### Localnet Verification

Stage 1C should also include one focused sim command that compares deterministic and ML-DSA signing on a small localnet path. It should be deliberately narrower than the full V2 matrix so this stage stays about PQ signing cost rather than consensus regression analysis.

### Documentation Verification

Documentation should clearly state:

- what is being measured
- what is not being measured
- that results are measurement outputs, not protocol thresholds

## Risks

### 1. Measurement Noise

Latency measurements on a developer machine will vary. Stage 1C should prefer repeated runs and report medians or similarly stable summary values rather than single-shot timings.

### 2. Scope Creep Into Benchmark Gating

It will be tempting to turn measurement results into pass/fail assertions immediately. That should wait for a later stage after enough data has been gathered.

### 3. Overreading Small Localnet Results

A small localnet comparison is useful, but it is not a replacement for a full benchmark matrix. Stage 1C should present localnet results as indicative overhead measurements, not as final scaling proof.

## Success Criteria

Stage 1C is successful when:

- the repo can reproduce deterministic vs ML-DSA signing measurements locally
- the output includes signature size, public identity size, sign latency, and verify latency
- the repo can produce one small localnet comparison between deterministic and ML-DSA signing
- the results are documented in human-readable form
- no consensus rules, session behavior, or protocol policy change as part of this stage

## Next Stage After Success

If Stage 1C succeeds, the next safe decisions become clearer:

- whether to add hard benchmark thresholds later
- whether to begin hybrid signing policy work
- whether ML-DSA overhead is acceptable for wider localnet experiments
- when to begin session/KEM planning as a separate slice
