# PQ Stage 1F Hybrid Localnet Bootstrap Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make `entangrid-sim` generate and boot one strict all-hybrid localnet with no manual key or config editing.

**Architecture:** Keep Stage 1F narrowly scoped to simulator bootstrap. Extend `init-localnet` with a strict `--hybrid-enforcement` mode that auto-generates ML-DSA key files, writes hybrid validator identities into genesis, emits node configs that select `HybridDeterministicMlDsaExperimental`, and makes the launch path explicitly `pq-ml-dsa`-aware. Preserve deterministic defaults when the new flag is not used.

**Tech Stack:** Rust, `clap`, `serde`, `tokio`, `entangrid-sim`, `entangrid-types`, `entangrid-crypto`, optional `pq-ml-dsa` feature, local file generation under `var/`.

---

## File Structure

### Simulator bootstrap and launch

- Modify: `crates/entangrid-sim/src/lib.rs`
  - add the `--hybrid-enforcement` CLI flag and thread it through `init_localnet(...)`
  - generate ML-DSA key files during strict hybrid init
  - build hybrid validator identities in genesis
  - emit hybrid-compatible `NodeConfig` values
  - make hybrid bootstrap/launch explicitly `pq-ml-dsa`-aware
  - add init/config tests and one smoke-oriented helper/test path

### Optional simulator feature wiring

- Modify: `crates/entangrid-sim/Cargo.toml`
  - only if needed to keep hybrid bootstrap helpers behind the existing `pq-ml-dsa` feature

### Documentation

- Modify: `README.md`
- Modify: `docs/architecture.md`
- Modify: `docs/protocol.md`
- Modify: `crates/entangrid-crypto/README.md`
- Modify: `crates/entangrid-types/README.md`
  - document the new simulator bootstrap mode, its feature requirements, and its narrow Stage 1F scope

## Task 1: Add The Strict Hybrid Bootstrap Flag And Preserve Defaults

**Files:**
- Modify: `crates/entangrid-sim/src/lib.rs`
- Test: `crates/entangrid-sim/src/lib.rs`

- [ ] **Step 1: Write failing init-localnet flag/config tests**

Add focused tests proving:

- `init-localnet` can express a `hybrid_enforcement` mode through the CLI/config path
- `init_localnet(..., hybrid_enforcement = false, ...)` preserves current deterministic defaults
- existing deterministic init-localnet behavior remains unchanged when the new flag is absent

Suggested test shapes:

```rust
#[test]
fn init_localnet_defaults_to_deterministic_signing_backend() { /* existing test stays */ }

#[test]
fn init_localnet_without_hybrid_enforcement_keeps_hybrid_policy_disabled() {
    // assert signing_backend == DevDeterministic
    // assert signing_key_path == None
    // assert !feature_flags.require_hybrid_validator_signatures
}
```

- [ ] **Step 2: Run the focused sim tests and verify they fail**

Run:

```bash
cargo test -p entangrid-sim init_localnet
```

Expected:

- compile or assertion failures because the new flag/plumbing does not exist yet

- [ ] **Step 3: Implement the minimal CLI and function-plumbing change**

In `crates/entangrid-sim/src/lib.rs`:

- add `#[arg(long, default_value_t = false)] hybrid_enforcement: bool` to `Commands::InitLocalnet`
- thread `hybrid_enforcement` through `cli_main()` into `init_localnet(...)`
- extend `init_localnet(...)` signature with the new boolean
- keep the existing deterministic path untouched when the flag is `false`

Suggested signature change:

```rust
pub fn init_localnet(
    validators: usize,
    base_dir: &Path,
    slot_duration_millis: u64,
    slots_per_epoch: u64,
    start_delay_millis: u64,
    enable_service_gating: bool,
    service_gating_start_epoch: u64,
    service_gating_threshold: f64,
    service_score_window_epochs: u64,
    service_score_weights: ServiceScoreWeights,
    degraded_validator: Option<u64>,
    degraded_delay_ms: u64,
    degraded_drop_probability: f64,
    degraded_disable_outbound: bool,
    consensus_v2: bool,
    hybrid_enforcement: bool,
) -> Result<()>
```

- [ ] **Step 4: Re-run the focused sim tests until green**

Run:

```bash
cargo test -p entangrid-sim init_localnet
```

- [ ] **Step 5: Commit the CLI/default-preservation slice**

```bash
git add crates/entangrid-sim/src/lib.rs
git commit -m "add hybrid bootstrap flag to simulator"
```

## Task 2: Generate Hybrid Identities And ML-DSA Key Files During Init

**Files:**
- Modify: `crates/entangrid-sim/src/lib.rs`
- Test: `crates/entangrid-sim/src/lib.rs`

- [ ] **Step 1: Write failing generation tests**

Add tests proving that `init_localnet(..., hybrid_enforcement = true, ...)`:

- writes `feature_flags.require_hybrid_validator_signatures = true`
- writes `feature_flags.consensus_v2 = true`
- writes `signing_backend = HybridDeterministicMlDsaExperimental`
- writes a non-empty `signing_key_path`
- creates `node-N/ml-dsa-key.json`
- emits `PublicIdentity::Hybrid` for every validator in generated genesis
- fails early with a clear error when hybrid bootstrap is requested from a simulator binary built without `pq-ml-dsa`

Suggested test names:

```rust
#[cfg(feature = "pq-ml-dsa")]
#[test]
fn init_localnet_hybrid_enforcement_writes_hybrid_node_configs() { /* ... */ }

#[cfg(feature = "pq-ml-dsa")]
#[test]
fn init_localnet_hybrid_enforcement_writes_hybrid_genesis_identities() { /* ... */ }

#[cfg(feature = "pq-ml-dsa")]
#[test]
fn init_localnet_hybrid_enforcement_writes_ml_dsa_key_files() { /* ... */ }

#[cfg(not(feature = "pq-ml-dsa"))]
#[test]
fn init_localnet_hybrid_enforcement_requires_pq_feature() { /* ... */ }
```

- [ ] **Step 2: Run the feature-enabled focused tests and verify they fail**

Run:

```bash
cargo test -p entangrid-sim init_localnet_hybrid_enforcement_requires_pq_feature
cargo test -p entangrid-sim --features pq-ml-dsa hybrid_enforcement
```

Expected:

- tests fail because hybrid key generation and hybrid genesis/config emission are not implemented yet

- [ ] **Step 3: Implement hybrid generation by reusing the existing ML-DSA sim key format**

In `crates/entangrid-sim/src/lib.rs`:

- reuse `MlDsa65SimKeyFile` instead of creating a second key-file schema
- add a helper to build strict-hybrid validator material, for example:

```rust
#[cfg(feature = "pq-ml-dsa")]
fn generate_hybrid_validator_material(
    validator_id: ValidatorId,
    node_dir: &Path,
) -> Result<(ValidatorConfig, SigningBackendKind, String)> {
    let mut rng = OsRng;
    let keypair = MlDsa65::key_gen(&mut rng);
    let signing_key = keypair.signing_key().clone();
    let verifying_key = keypair.verifying_key().clone();
    let key_path = node_dir.join("ml-dsa-key.json");
    let key_file = MlDsa65SimKeyFile {
        signing_key: signing_key.encode().as_slice().to_vec(),
        verifying_key: verifying_key.encode().as_slice().to_vec(),
    };
    fs::write(&key_path, serde_json::to_vec(&key_file)?)?;

    let public_identity = PublicIdentity::try_hybrid(vec![
        PublicIdentityComponent {
            scheme: PublicKeyScheme::DevDeterministic,
            bytes: deterministic_public_identity(validator_id)
                .as_single_bytes()
                .unwrap()
                .to_vec(),
        },
        PublicIdentityComponent {
            scheme: PublicKeyScheme::MlDsa,
            bytes: verifying_key.encode().as_slice().to_vec(),
        },
    ])?;

    Ok((
        ValidatorConfig {
            validator_id,
            stake: 100,
            address: format!("127.0.0.1:{}", 4100 + (validator_id - 1)),
            dev_secret: format!("entangrid-dev-secret-{validator_id}"),
            public_identity,
        },
        SigningBackendKind::HybridDeterministicMlDsaExperimental,
        key_path.display().to_string(),
    ))
}
```

Then update `init_localnet(...)` so:

- `hybrid_enforcement = true` forces `consensus_v2 = true`
- every validator identity in genesis is hybrid
- every node config gets:
  - `signing_backend = HybridDeterministicMlDsaExperimental`
  - `signing_key_path = Some(node_dir.join("ml-dsa-key.json"))`
  - `feature_flags.require_hybrid_validator_signatures = true`

Also add a clear error path for `hybrid_enforcement = true` when the sim binary is built without `pq-ml-dsa`:

```rust
#[cfg(not(feature = "pq-ml-dsa"))]
if hybrid_enforcement {
    return Err(anyhow!(
        "hybrid bootstrap requires entangrid-sim to be built with pq-ml-dsa"
    ));
}
```

- [ ] **Step 4: Re-run the feature-enabled focused tests until green**

Run:

```bash
cargo test -p entangrid-sim --features pq-ml-dsa hybrid_enforcement
```

- [ ] **Step 5: Commit the generation slice**

```bash
git add crates/entangrid-sim/src/lib.rs
git commit -m "generate strict hybrid localnet configs"
```

## Task 3: Make Hybrid Launch Feature-Aware And Add One Boot Smoke Path

**Files:**
- Modify: `crates/entangrid-sim/src/lib.rs`
- Test: `crates/entangrid-sim/src/lib.rs`
- Verify: local `var/pq-hybrid-smoke`

- [ ] **Step 1: Write failing launch and smoke-focused tests**

Add tests proving:

- hybrid localnets trigger a `pq-ml-dsa`-aware build/launch decision
- the launch path fails early with a clear error if hybrid configs are present but the sim binary lacks PQ support
- the manifest/config inspection logic can detect that a localnet requires hybrid bootstrap handling

Suggested helper shape:

```rust
fn localnet_requires_hybrid_enforcement(manifest: &LocalnetManifest) -> Result<bool> {
    for config_path in &manifest.node_configs {
        let config: NodeConfig = toml::from_str(&fs::read_to_string(config_path)?)?;
        if config.feature_flags.require_hybrid_validator_signatures
            || config.signing_backend == SigningBackendKind::HybridDeterministicMlDsaExperimental
        {
            return Ok(true);
        }
    }
    Ok(false)
}
```

Suggested tests:

```rust
#[test]
fn hybrid_localnet_detection_returns_false_for_default_localnet() { /* ... */ }

#[cfg(feature = "pq-ml-dsa")]
#[test]
fn hybrid_localnet_detection_returns_true_for_hybrid_localnet() { /* ... */ }
```

- [ ] **Step 2: Run the focused sim tests and verify they fail**

Run:

```bash
cargo test -p entangrid-sim hybrid_localnet_detection
```

Expected:

- tests fail because the helper/build-selection behavior does not exist yet

- [ ] **Step 3: Implement feature-aware build/launch and a narrow smoke recipe**

In `crates/entangrid-sim/src/lib.rs`:

- add `localnet_requires_hybrid_enforcement(...)`
- update `spawn_localnet_children(...)` / `ensure_node_binary_built(...)` to choose one of:
  - build `entangrid-node` with `--features pq-ml-dsa` when hybrid configs are present
  - or fail immediately with a clear operator-facing error when the sim binary itself is not feature-capable

Suggested build helper split:

```rust
async fn ensure_node_binary_built(require_pq_ml_dsa: bool) -> Result<()> {
    let mut command = Command::new("cargo");
    command.arg("build").arg("--bin").arg("entangrid-node");
    if require_pq_ml_dsa {
        command.arg("--features").arg("pq-ml-dsa");
    }
    command.current_dir(workspace_root()?);
    let status = command.status().await?;
    if !status.success() {
        return Err(anyhow!("failed to build entangrid-node"));
    }
    Ok(())
}
```

Also add a smoke verification path, either as:

- a focused ignored async smoke test in `crates/entangrid-sim/src/lib.rs`
- plus a documented manual verification command for local reproduction

Required smoke artifact:

```rust
#[cfg(feature = "pq-ml-dsa")]
#[tokio::test(flavor = "multi_thread")]
#[ignore = "boots a strict hybrid localnet with real localhost sockets"]
async fn hybrid_enforcement_localnet_boot_smoke_test() {
    // init-localnet --hybrid-enforcement into a temp dir
    // spawn localnet children
    // wait through genesis and a few slots
    // assert at least one block artifact/log entry exists
    // assert proposal-vote or QC activity appears
    // shut children down cleanly
}
```

Supplemental manual smoke command:

```bash
cargo run -p entangrid-sim --features pq-ml-dsa -- init-localnet --validators 4 --hybrid-enforcement --base-dir var/pq-hybrid-smoke
cargo run -p entangrid-sim --features pq-ml-dsa -- up --base-dir var/pq-hybrid-smoke
```

Expected smoke result:

- node processes stay alive past genesis time for a few slots
- at least one block is proposed or accepted
- proposal-vote or QC activity appears in logs/metrics

- [ ] **Step 4: Re-run the focused tests and execute the smoke validation**

Run:

```bash
cargo test -p entangrid-sim
cargo test -p entangrid-sim --features pq-ml-dsa
cargo test -p entangrid-sim --features pq-ml-dsa hybrid_enforcement_localnet_boot_smoke_test -- --ignored
cargo test -p entangrid-crypto -p entangrid-ledger -p entangrid-node --features pq-ml-dsa
cargo run -p entangrid-sim --features pq-ml-dsa -- init-localnet --validators 4 --hybrid-enforcement --base-dir var/pq-hybrid-smoke
```

If local node booting is part of the verification, use unrestricted localhost execution and inspect:

- `var/pq-hybrid-smoke/node-1/events.log`
- `var/pq-hybrid-smoke/node-1/metrics.json`
- `var/pq-hybrid-smoke/node-*/stdout.log`

- [ ] **Step 5: Commit the launch/smoke slice**

```bash
git add crates/entangrid-sim/src/lib.rs
git commit -m "boot strict hybrid localnets from simulator"
```

## Task 4: Document The New Hybrid Bootstrap Mode

**Files:**
- Modify: `README.md`
- Modify: `docs/architecture.md`
- Modify: `docs/protocol.md`
- Modify: `crates/entangrid-crypto/README.md`
- Modify: `crates/entangrid-types/README.md`

- [ ] **Step 1: Write the doc updates**

Document:

- the `--hybrid-enforcement` simulator flag
- that it requires a `pq-ml-dsa` build
- that it forces strict all-hybrid validator bootstrap
- that Stage 1F is an operational/bootstrap slice, not a full hybrid performance matrix

Suggested README snippet:

````md
### Strict Hybrid Localnet Bootstrap

Use the simulator's `--hybrid-enforcement` mode to generate a strict all-hybrid localnet:

```bash
cargo run -p entangrid-sim --features pq-ml-dsa -- init-localnet --validators 4 --hybrid-enforcement --base-dir var/pq-hybrid-smoke
```

This mode:

- writes hybrid validator identities into genesis
- generates one ML-DSA key file per node
- selects `HybridDeterministicMlDsaExperimental`
- enables `require_hybrid_validator_signatures = true`
- enables `consensus_v2 = true`
````

- [ ] **Step 2: Run doc hygiene checks**

Run:

```bash
git diff --check
```

Expected:

- no whitespace or formatting errors

- [ ] **Step 3: Commit the docs slice**

```bash
git add README.md docs/architecture.md docs/protocol.md crates/entangrid-crypto/README.md crates/entangrid-types/README.md
git commit -m "document strict hybrid localnet bootstrap"
```

## Final Verification

- [ ] **Step 1: Run the full Stage 1F verification set**

```bash
cargo test -p entangrid-sim
cargo test -p entangrid-sim --features pq-ml-dsa
cargo test -p entangrid-sim --features pq-ml-dsa hybrid_enforcement_localnet_boot_smoke_test -- --ignored
cargo test -p entangrid-crypto -p entangrid-ledger -p entangrid-node --features pq-ml-dsa
```

- [ ] **Step 2: Run the hybrid bootstrap smoke flow**

```bash
cargo run -p entangrid-sim --features pq-ml-dsa -- init-localnet --validators 4 --hybrid-enforcement --base-dir var/pq-hybrid-smoke
cargo run -p entangrid-sim --features pq-ml-dsa -- up --base-dir var/pq-hybrid-smoke
```

Expected:

- generated configs are all hybrid and strict
- nodes start successfully
- at least one block and one V2 vote/QC path appear during the smoke window

- [ ] **Step 3: Commit any final verification-only fixes**

```bash
git add crates/entangrid-sim/src/lib.rs README.md docs/architecture.md docs/protocol.md crates/entangrid-crypto/README.md crates/entangrid-types/README.md
git commit -m "finalize stage 1f hybrid bootstrap verification"
```
