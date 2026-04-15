# Stage 1F PQ Hybrid Localnet Bootstrap Design

**Date:** 2026-04-02
**Branch:** `stage-1/pq-integration`
**Status:** Drafted from approved design discussion

## Goal

Make Entangrid's strict hybrid-enforcement mode runnable in the simulator with no manual key or config editing.

Stage 1F builds on:

- Stage 1 typed signature and identity foundations
- Stage 1B experimental ML-DSA signing support
- Stage 1D hybrid signature and identity support
- Stage 1E hybrid enforcement for validator startup, blocks, and proposal votes

The goal is to move from "the policy exists and unit tests pass" to "the simulator can generate and boot one strict all-hybrid localnet under that policy."

## Why This Stage Exists

Entangrid now has:

- hybrid-capable typed signatures and public identities
- a hybrid deterministic + ML-DSA signing backend
- an opt-in `require_hybrid_validator_signatures` policy
- runtime enforcement for validator startup, blocks, and proposal votes

What it does **not** have yet is a low-friction operational path to use that mode in a real generated localnet.

Right now, strict hybrid enforcement is still mostly a code-and-test feature. Stage 1F exists to make it an actual simulator workflow:

- generate key material automatically
- generate hybrid validator identities automatically
- generate node configs automatically
- prove that a strict all-hybrid localnet can boot successfully

## Scope

### In Scope

- extend `init-localnet` in [`crates/entangrid-sim/src/lib.rs`](../../../../crates/entangrid-sim/src/lib.rs) with a strict hybrid-enforcement mode
- generate one ML-DSA key file per validator automatically during localnet initialization
- write hybrid validator identities into genesis automatically
- write node configs that select `HybridDeterministicMlDsaExperimental`
- write node configs that enable `require_hybrid_validator_signatures = true`
- define the simulator startup behavior for this mode so node launch uses a `pq-ml-dsa`-capable build or fails early with a clear message
- tie the strict hybrid smoke path to `consensus_v2 = true` so proposal-vote enforcement is part of the runnable mode
- add one smoke test proving that a small generated all-hybrid localnet can boot successfully
- document the new localnet bootstrap mode

### Out of Scope

- mixed deterministic/hybrid simulator topologies
- full bursty or degraded hybrid matrices
- transaction hybrid enforcement
- receipt or service-evidence hybrid enforcement
- ML-KEM or session/KEM changes
- production-grade operator key lifecycle tooling

## Architecture

### 1. Strict All-Hybrid Bootstrap Mode

Stage 1F should add one narrow simulator mode for localnet generation:

- every validator gets a hybrid public identity
- every node gets an ML-DSA signing key file
- every node uses `HybridDeterministicMlDsaExperimental`
- every node enables `require_hybrid_validator_signatures`

This should be intentionally strict and uniform.

Why:

- it proves the enforcement mode actually works end to end
- it avoids rollout ambiguity during the first operational slice
- it keeps simulator logic smaller than a mixed-mode design

This stage is not about flexibility. It is about proving that strict hybrid enforcement can really boot.

Because this mode generates real ML-DSA key material, the simulator command itself must be feature-aware:

- `init-localnet --hybrid-enforcement` must run from a binary built with `pq-ml-dsa`
- or fail immediately with a clear message explaining that hybrid bootstrap requires a `pq-ml-dsa` build

### 2. Simulator Generates Key Material Automatically

The simulator should generate ML-DSA key material during `init-localnet` instead of requiring pre-generated files.

Recommended behavior:

- for each validator, generate one ML-DSA keypair
- write the signing/verifying key material to a node-local file such as:
  - `node-N/ml-dsa-key.json`
- store the verifying key bytes in genesis as part of that validator's hybrid `PublicIdentity`
- store the signing key file path in that node's `NodeConfig.signing_key_path`

This is the right trade-off for Stage 1F because:

- it removes operator friction
- it keeps the first runnable path fully reproducible from one simulator command
- it avoids inventing a bigger key-management workflow too early

### 3. Genesis Must Be Fully Hybrid In This Mode

When strict hybrid-enforcement localnet generation is enabled, the generated genesis should contain only hybrid validator identities.

Each validator identity should include:

- deterministic component
- ML-DSA component

That keeps generated genesis consistent with Stage 1E's startup requirement:

- when enforcement is on, every validator identity must be hybrid

The simulator should generate this state directly rather than generating a permissive genesis and expecting operators to patch it later.

### 4. Node Config Must Be Fully Compatible With Enforcement

Each generated node config should include:

- `signing_backend = HybridDeterministicMlDsaExperimental`
- `signing_key_path = ".../ml-dsa-key.json"`
- `feature_flags.require_hybrid_validator_signatures = true`
- `feature_flags.consensus_v2 = true`

Other defaults should remain unchanged unless they are already required by existing localnet behavior.

This keeps the bootstrap mode aligned with current runtime expectations:

- startup passes identity enforcement
- locally produced blocks use hybrid signatures
- locally produced proposal votes use hybrid signatures

The `consensus_v2` requirement is intentional for this mode.

Reason:

- Stage 1E block enforcement applies in both the general block path and the V2-enabled runtime
- Stage 1E proposal-vote enforcement only matters when proposal votes are live
- a strict hybrid localnet that claims to exercise validator-originated enforcement should therefore turn on `consensus_v2`

### 5. Keep The CLI Surface Small

Stage 1F should add one narrow `init-localnet` flag rather than a large family of PQ simulator modes.

Recommended flag:

- `--hybrid-enforcement`

Why this name is preferable:

- it expresses the operator-facing effect
- it aligns with the runtime policy flag
- it does not overfit the stage to ML-DSA internals even though ML-DSA is the current PQ component

The simulator behavior behind this flag can still use the current `pq-ml-dsa` implementation under the hood.

### 6. Launch Path Must Be Feature-Aware

Strict hybrid bootstrap is only meaningful if the generated localnet can actually start nodes that understand:

- `HybridDeterministicMlDsaExperimental`
- generated ML-DSA key files
- hybrid validator identities

That means simulator startup must do one of two things:

- build/spawn `entangrid-node` with `pq-ml-dsa` enabled
- or fail immediately with a clear operator-facing error explaining that hybrid-enforcement localnets require a `pq-ml-dsa` build

Stage 1F should not silently generate a hybrid localnet that the existing launch path cannot boot.

## File And Output Shape

### Generated Genesis

Generated genesis should contain:

- hybrid validator identities for every validator
- no mixed identity sets in this mode

### Generated Node Directories

Each generated node directory should contain at least:

- `node.toml`
- `ml-dsa-key.json`

Normal runtime output files such as logs and metrics should continue to appear later when the node actually boots.

### Node Config Shape

Generated node config should set:

- `signing_backend = HybridDeterministicMlDsaExperimental`
- `signing_key_path = "..."`
- `feature_flags.require_hybrid_validator_signatures = true`
- `feature_flags.consensus_v2 = true`

All of this should be generated automatically by simulator init.

## Rollout Plan

### Phase 1: Simulator Config And Material Generation

Modify:

- [`crates/entangrid-sim/src/lib.rs`](../../../../crates/entangrid-sim/src/lib.rs)

Deliverables:

- `init-localnet --hybrid-enforcement`
- ML-DSA key generation per validator
- hybrid validator identities in generated genesis
- compatible node config generation
- clear early failure if hybrid bootstrap is requested from a simulator binary built without `pq-ml-dsa`
- feature-aware launch requirements for hybrid localnets

### Phase 2: Smoke Validation

Modify:

- [`crates/entangrid-sim/src/lib.rs`](../../../../crates/entangrid-sim/src/lib.rs)
- supporting tests if needed in simulator or node crates

Deliverables:

- one smoke test for a small strict all-hybrid localnet
- proof that nodes can start under `require_hybrid_validator_signatures`

### Phase 3: Documentation

Modify:

- [`README.md`](../../../../README.md)
- [`docs/architecture.md`](../../../../docs/architecture.md)
- [`docs/protocol.md`](../../../../docs/protocol.md)
- [`crates/entangrid-crypto/README.md`](../../../../crates/entangrid-crypto/README.md)
- [`crates/entangrid-types/README.md`](../../../../crates/entangrid-types/README.md)

Deliverables:

- clear docs for the new simulator flag
- clear statement that strict all-hybrid localnet bootstrap now exists
- clear statement that this is still a simulator/bootstrap slice, not full hybrid performance validation

## Testing Strategy

### Unit And Integration Tests

Add tests that prove:

- `init-localnet --hybrid-enforcement` writes `require_hybrid_validator_signatures = true`
- `init-localnet --hybrid-enforcement` writes `consensus_v2 = true`
- generated node configs select `HybridDeterministicMlDsaExperimental`
- generated node configs contain an ML-DSA key path
- generated genesis contains hybrid identities for all validators
- without `--hybrid-enforcement`, `init-localnet` still preserves the current deterministic defaults:
  - deterministic signing backend
  - no generated ML-DSA key files
  - `require_hybrid_validator_signatures = false`
  - current default consensus behavior remains unchanged

### Smoke Test

Add one smoke path that proves:

- a small generated localnet, preferably `4` validators
- can boot successfully
- under strict hybrid enforcement

Concrete pass condition:

- node processes stay alive past genesis time for a few slots
- at least one block is proposed or accepted
- because this mode enables `consensus_v2`, at least one proposal-vote or quorum-certificate path is observed in logs or metrics

The smoke test should stay intentionally narrow:

- no bursty matrix
- no degradation scenarios
- no throughput claims

This stage is about operational correctness, not performance.

### Verification

Expected verification commands:

```bash
cargo test -p entangrid-sim
cargo test -p entangrid-sim --features pq-ml-dsa
cargo test -p entangrid-crypto -p entangrid-ledger -p entangrid-node --features pq-ml-dsa
cargo run -p entangrid-sim --features pq-ml-dsa -- init-localnet --validators 4 --hybrid-enforcement --base-dir var/pq-hybrid-smoke
```

If the smoke test boots nodes as part of verification, unrestricted localhost socket access may be needed outside the sandbox. The startup path should use a `pq-ml-dsa`-capable node build; otherwise the simulator should fail early with a clear message instead of attempting a broken launch.

## Risks And Trade-Offs

### Why Not Mixed-Mode Simulator Support First?

Because mixed-mode is a rollout policy question, not the first operational proof target.

Stage 1F should first prove one simple invariant:

- strict hybrid enforcement can be generated and booted automatically

That is more valuable right now than broader mode flexibility.

### Why Auto-Generate Keys Instead Of Requiring Pre-Provisioned Files?

Because the operator workflow is not the target of this stage.

Stage 1F is about:

- proving the simulator can bootstrap the mode
- minimizing friction
- making verification reproducible

Production-style key provisioning can come later.

### Why Only One Boot Smoke Test?

Because a full bursty or degraded matrix would mix operational bootstrap proof with performance and protocol-behavior questions.

Stage 1F should first prove:

- generation works
- startup works

Only after that should we widen hybrid runs into larger matrices.

### Why Preserve Deterministic Defaults Explicitly?

Because Stage 1F is adding a new bootstrap mode, not replacing the current simulator baseline.

The branch already uses deterministic localnets for:

- existing PQ measurement flows
- existing non-hybrid tests
- general localnet development

So the strict hybrid path must be additive. The simulator should remain deterministic by default unless `--hybrid-enforcement` is explicitly requested.

## Success Criteria

Stage 1F is successful when:

- the simulator can generate a strict all-hybrid localnet automatically
- every generated validator identity is hybrid
- every generated node config selects the hybrid backend, enables hybrid enforcement, and enables `consensus_v2`
- every generated node has an ML-DSA key file
- the hybrid localnet startup path is explicitly feature-aware and does not silently attempt an incompatible launch
- one small localnet can boot successfully under that generated configuration, with:
  - nodes alive past genesis time
  - block production or acceptance observed
  - proposal-vote or QC activity observed
- deterministic localnet generation remains unchanged when `--hybrid-enforcement` is not used
- docs clearly describe the new bootstrap mode and its boundaries
