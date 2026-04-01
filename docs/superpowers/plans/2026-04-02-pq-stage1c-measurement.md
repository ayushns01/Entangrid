# PQ Stage 1C ML-DSA Measurement Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add reproducible deterministic-vs-ML-DSA signing measurements and one small localnet comparison report without changing consensus behavior or introducing benchmark gates.

**Architecture:** Keep measurements close to the real code paths by adding backend-local measurement helpers in `entangrid-crypto` and a narrow comparison entrypoint in `entangrid-sim`. Write outputs as human-readable markdown under `test-results/`, then document how to run and interpret them in the benchmark docs and crypto README.

**Tech Stack:** Rust, `entangrid-crypto`, `entangrid-sim`, markdown reports, `pq-ml-dsa` cargo feature.

---

## File Structure

### Crypto measurement layer

- Modify: `crates/entangrid-crypto/src/lib.rs`
  - add measurement data structures/helpers
  - add report rendering
  - add focused tests for deterministic and ML-DSA measurement output

### Simulation comparison layer

- Modify: `crates/entangrid-sim/src/lib.rs`
  - add a small PQ measurement/report subcommand or helper
  - compare deterministic vs ML-DSA on a small localnet path
  - write a markdown report under `test-results/`

### Documentation

- Modify: `docs/benchmarks.md`
- Modify: `crates/entangrid-crypto/README.md`
- Create or update generated report in: `test-results/pq-ml-dsa-measurements.md`

## Task 1: Add Backend-Local ML-DSA Measurement Helpers

**Files:**
- Modify: `crates/entangrid-crypto/src/lib.rs`
- Test: `crates/entangrid-crypto/src/lib.rs`

- [ ] **Step 1: Write failing tests for measurement output shape**

Add focused tests that prove:

- deterministic measurement reports include public identity size and signature size
- ML-DSA measurement reports include `SignatureScheme::MlDsa`
- latency summaries render stable markdown headings/fields

- [ ] **Step 2: Run the focused tests and verify they fail**

Run:

```bash
cargo test -p entangrid-crypto measurement
cargo test -p entangrid-crypto --features pq-ml-dsa measurement
```

Expected:

- tests fail because measurement helpers and report rendering do not exist yet

- [ ] **Step 3: Implement the minimal measurement types and helpers**

Add:

- deterministic and ML-DSA measurement collection helpers
- repeated sign/verify timing loops that report medians
- signature/public identity size extraction
- markdown rendering for the crypto report section

- [ ] **Step 4: Re-run the focused tests until green**

Run:

```bash
cargo test -p entangrid-crypto measurement
cargo test -p entangrid-crypto --features pq-ml-dsa measurement
```

- [ ] **Step 5: Commit the crypto measurement slice**

```bash
git add crates/entangrid-crypto/src/lib.rs
git commit -m "add pq signing measurement helpers"
```

## Task 2: Add A Small Deterministic-vs-ML-DSA Localnet Comparison Report

**Files:**
- Modify: `crates/entangrid-sim/src/lib.rs`
- Test: `crates/entangrid-sim/src/lib.rs`

- [ ] **Step 1: Write failing tests for the PQ measurement report command/helper**

Add focused tests that prove:

- the sim layer can render a deterministic-vs-ML-DSA comparison markdown report
- report output includes validator count, backend names, and timing/overhead fields
- the report path lands under `test-results/`

- [ ] **Step 2: Run the focused tests and verify they fail**

Run:

```bash
cargo test -p entangrid-sim pq_measurement
```

Expected:

- tests fail because the comparison helper/command does not exist yet

- [ ] **Step 3: Implement the minimal sim comparison flow**

Add:

- a narrow CLI/helper path for PQ signing comparison
- one small localnet deterministic-vs-ML-DSA comparison
- markdown report generation to `test-results/`

Keep scope narrow:

- small validator count only
- no consensus retuning
- no benchmark pass/fail assertions

- [ ] **Step 4: Re-run the focused tests until green**

Run:

```bash
cargo test -p entangrid-sim pq_measurement
```

- [ ] **Step 5: Commit the sim comparison slice**

```bash
git add crates/entangrid-sim/src/lib.rs
git commit -m "add pq localnet measurement report"
```

## Task 3: Document And Generate The Measurement Report

**Files:**
- Modify: `docs/benchmarks.md`
- Modify: `crates/entangrid-crypto/README.md`
- Create or Modify: `test-results/pq-ml-dsa-measurements.md`

- [ ] **Step 1: Write failing doc/report expectations as tests or verifiable checks**

Add or prepare checks that prove:

- benchmark docs mention PQ signing measurements as report-first outputs
- crypto README explains how to run the measurement flow
- the generated report contains deterministic vs ML-DSA size and latency sections

- [ ] **Step 2: Run the relevant tests/checks and verify the gap**

Run:

```bash
git diff --check
```

Expected:

- no formatting issues, but the required Stage 1C documentation/report content is still missing

- [ ] **Step 3: Update docs and generate the first report**

Update:

- `docs/benchmarks.md`
- `crates/entangrid-crypto/README.md`

Generate:

- `test-results/pq-ml-dsa-measurements.md`

- [ ] **Step 4: Run the full Stage 1C verification commands**

Run:

```bash
cargo test -p entangrid-crypto --features pq-ml-dsa
cargo test -p entangrid-sim
git diff --check
```

Expected:

- tests pass
- docs are clean
- report file is present and readable

- [ ] **Step 5: Commit the docs/report slice**

```bash
git add docs/benchmarks.md crates/entangrid-crypto/README.md test-results/pq-ml-dsa-measurements.md
git commit -m "document pq signing measurements"
```

## Task 4: Final Stage 1C Verification

**Files:**
- Verify: `crates/entangrid-crypto/src/lib.rs`
- Verify: `crates/entangrid-sim/src/lib.rs`
- Verify: `docs/benchmarks.md`
- Verify: `crates/entangrid-crypto/README.md`
- Verify: `test-results/pq-ml-dsa-measurements.md`

- [ ] **Step 1: Run the full intended verification set**

Run:

```bash
cargo test -p entangrid-crypto --features pq-ml-dsa
cargo test -p entangrid-sim
git diff --check
```

- [ ] **Step 2: Run the measurement flow and inspect the report**

Run the Stage 1C measurement entrypoint and verify the generated markdown includes:

- deterministic public identity size
- ML-DSA public identity size
- deterministic signature size
- ML-DSA signature size
- deterministic sign/verify latency
- ML-DSA sign/verify latency
- small localnet comparison summary

- [ ] **Step 3: Commit any final cleanup**

```bash
git add crates/entangrid-crypto/src/lib.rs crates/entangrid-sim/src/lib.rs docs/benchmarks.md crates/entangrid-crypto/README.md test-results/pq-ml-dsa-measurements.md
git commit -m "finish pq stage 1c measurements"
```
