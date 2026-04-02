# PQ Stage 1E Hybrid Enforcement Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add a feature-flagged, network-wide hybrid-signature enforcement mode for validator-originated consensus objects.

**Architecture:** Keep the crypto layer permissive and verification-capable for deterministic, ML-DSA, and hybrid signatures, but move the strict policy into node/runtime behavior. When the enforcement flag is enabled, startup requires all validators to advertise hybrid identities, and block/proposal-vote validation requires hybrid signatures.

**Tech Stack:** Rust, serde, bincode, `entangrid-types`, `entangrid-crypto`, `entangrid-node`, `entangrid-ledger`, `entangrid-sim`, optional `pq-ml-dsa` feature.

---

## File Structure

### Type and config layer

- Modify: `crates/entangrid-types/src/lib.rs`
  - add the enforcement flag to `FeatureFlags`
  - preserve config defaults and TOML compatibility
  - add tests for default and TOML round-tripping

### Node policy layer

- Modify: `crates/entangrid-node/src/lib.rs`
  - add startup validation for all-hybrid validator identities when enforcement is enabled
  - enforce hybrid signatures for blocks
  - enforce hybrid signatures for proposal votes
  - add focused tests for startup gating and policy rejection/acceptance

### Simulation and fixtures

- Modify: `crates/entangrid-sim/src/lib.rs`
  - keep deterministic defaults unchanged
  - ensure any fixture/config helpers can express the new enforcement flag cleanly

### Documentation

- Modify: `README.md`
- Modify: `docs/architecture.md`
- Modify: `docs/protocol.md`
- Modify: `crates/entangrid-crypto/README.md`
- Modify: `crates/entangrid-types/README.md`

## Task 1: Add The Hybrid Enforcement Feature Flag

**Files:**
- Modify: `crates/entangrid-types/src/lib.rs`
- Test: `crates/entangrid-types/src/lib.rs`

- [ ] **Step 1: Write failing config tests**

Add tests that prove:

- `FeatureFlags::default()` leaves `require_hybrid_validator_signatures` as `false`
- TOML parsing round-trips the new field on `NodeConfig`

Suggested test shape:

```rust
#[test]
fn hybrid_enforcement_defaults_to_disabled() {
    assert!(!FeatureFlags::default().require_hybrid_validator_signatures);
}

#[test]
fn hybrid_enforcement_round_trips_through_toml() {
    let config = r#"
validator_id = 1
data_dir = "/tmp/node-1"
genesis_path = "/tmp/genesis.toml"
listen_address = "127.0.0.1:3001"
peers = []
log_path = "/tmp/events.log"
metrics_path = "/tmp/metrics.json"
sync_on_startup = true

[feature_flags]
enable_receipts = true
enable_service_gating = false
consensus_v2 = false
require_hybrid_validator_signatures = true
service_gating_start_epoch = 3
service_gating_threshold = 0.40
service_score_window_epochs = 4

[feature_flags.service_score_weights]
uptime_weight = 0.25
delivery_weight = 0.50
diversity_weight = 0.25
penalty_weight = 1.0

[fault_profile]
artificial_delay_ms = 0
outbound_drop_probability = 0.0
pause_slot_production = false
disable_outbound = false
"#;
    let parsed: NodeConfig = toml::from_str(config).unwrap();
    assert!(parsed.feature_flags.require_hybrid_validator_signatures);
}
```

- [ ] **Step 2: Run the focused type tests and verify they fail**

Run:

```bash
cargo test -p entangrid-types hybrid_enforcement
```

Expected:

- tests fail because the feature flag does not exist yet

- [ ] **Step 3: Implement the minimal config change**

Add the field in `FeatureFlags`:

```rust
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct FeatureFlags {
    pub enable_receipts: bool,
    pub enable_service_gating: bool,
    #[serde(default = "default_consensus_v2")]
    pub consensus_v2: bool,
    #[serde(default)]
    pub require_hybrid_validator_signatures: bool,
    #[serde(default = "default_service_gating_start_epoch")]
    pub service_gating_start_epoch: Epoch,
    // ...
}
```

Do not change any other default behavior.

- [ ] **Step 4: Re-run the focused type tests until green**

Run:

```bash
cargo test -p entangrid-types hybrid_enforcement
```

- [ ] **Step 5: Commit the config slice**

```bash
git add crates/entangrid-types/src/lib.rs
git commit -m "add hybrid enforcement feature flag"
```

## Task 2: Enforce Hybrid Identities At Startup

**Files:**
- Modify: `crates/entangrid-node/src/lib.rs`
- Test: `crates/entangrid-node/src/lib.rs`

- [ ] **Step 1: Write failing startup-policy tests**

Add tests that prove:

- startup fails when `require_hybrid_validator_signatures = true` and any validator identity is single-scheme
- startup succeeds when the flag is on and all validator identities are hybrid

Suggested helper shape:

```rust
fn validate_hybrid_enforcement_genesis(
    config: &NodeConfig,
    genesis: &GenesisConfig,
) -> Result<()> {
    if !config.feature_flags.require_hybrid_validator_signatures {
        return Ok(());
    }
    for validator in &genesis.validators {
        if validator.public_identity.scheme() != PublicKeyScheme::Hybrid {
            return Err(anyhow!(
                "hybrid enforcement requires every validator to advertise a hybrid public identity"
            ));
        }
    }
    Ok(())
}
```

Suggested tests:

```rust
#[test]
fn hybrid_enforcement_rejects_mixed_validator_identities_at_startup() { /* ... */ }

#[test]
fn hybrid_enforcement_accepts_all_hybrid_validator_identities_at_startup() { /* ... */ }
```

- [ ] **Step 2: Run the focused node tests and verify they fail**

Run:

```bash
cargo test -p entangrid-node hybrid_enforcement_rejects_mixed_validator_identities_at_startup
cargo test -p entangrid-node hybrid_enforcement_accepts_all_hybrid_validator_identities_at_startup
```

Expected:

- tests fail because startup does not enforce the identity rule yet

- [ ] **Step 3: Implement the startup gate**

In `run_node(...)`, validate the genesis before the node starts networking:

```rust
pub async fn run_node(config: NodeConfig, genesis: GenesisConfig) -> Result<()> {
    validate_hybrid_enforcement_genesis(&config, &genesis)?;
    let crypto = build_crypto_backend(&genesis, &config)?;
    // ...
}
```

Keep the check policy-only:

- no changes to `build_crypto_backend(...)`
- no changes to crypto verification rules

- [ ] **Step 4: Re-run the focused node tests until green**

Run:

```bash
cargo test -p entangrid-node hybrid_enforcement_rejects_mixed_validator_identities_at_startup
cargo test -p entangrid-node hybrid_enforcement_accepts_all_hybrid_validator_identities_at_startup
```

- [ ] **Step 5: Commit the startup slice**

```bash
git add crates/entangrid-node/src/lib.rs
git commit -m "enforce hybrid validator identities at startup"
```

## Task 3: Enforce Hybrid Blocks

**Files:**
- Modify: `crates/entangrid-node/src/lib.rs`
- Test: `crates/entangrid-node/src/lib.rs`

- [ ] **Step 1: Write failing block-policy tests**

Add tests that prove:

- a deterministic-only block is rejected when the flag is enabled
- a hybrid-signed block is accepted when the flag is enabled
- block validation remains permissive when the flag is disabled

Suggested helper:

```rust
fn validate_hybrid_block_policy(&self, block: &Block) -> Result<()> {
    if !self.config.feature_flags.require_hybrid_validator_signatures {
        return Ok(());
    }
    if block.signature.scheme() != SignatureScheme::Hybrid {
        return Err(anyhow!("hybrid enforcement requires hybrid block signatures"));
    }
    Ok(())
}
```

Suggested tests:

```rust
#[tokio::test(flavor = "current_thread")]
async fn hybrid_enforcement_rejects_non_hybrid_block_signature() { /* ... */ }

#[cfg(feature = "pq-ml-dsa")]
#[tokio::test(flavor = "current_thread")]
async fn hybrid_enforcement_accepts_hybrid_block_signature() { /* ... */ }
```

- [ ] **Step 2: Run the focused block tests and verify they fail**

Run:

```bash
cargo test -p entangrid-node hybrid_enforcement_rejects_non_hybrid_block_signature
cargo test -p entangrid-node --features pq-ml-dsa hybrid_enforcement_accepts_hybrid_block_signature
```

Expected:

- non-hybrid block test fails because block policy is still permissive

- [ ] **Step 3: Implement block enforcement**

Call the helper from `accept_block(...)` after hash/schedule checks and before the block is accepted:

```rust
self.validate_hybrid_block_policy(&block)?;
```

Do not alter the underlying signature verifier.

- [ ] **Step 4: Re-run the focused block tests until green**

Run:

```bash
cargo test -p entangrid-node hybrid_enforcement_rejects_non_hybrid_block_signature
cargo test -p entangrid-node --features pq-ml-dsa hybrid_enforcement_accepts_hybrid_block_signature
```

- [ ] **Step 5: Commit the block slice**

```bash
git add crates/entangrid-node/src/lib.rs
git commit -m "enforce hybrid block signatures"
```

## Task 4: Enforce Hybrid Proposal Votes

**Files:**
- Modify: `crates/entangrid-node/src/lib.rs`
- Test: `crates/entangrid-node/src/lib.rs`

- [ ] **Step 1: Write failing proposal-vote policy tests**

Add tests that prove:

- a deterministic-only proposal vote is rejected when the flag is enabled
- a hybrid proposal vote is accepted when the flag is enabled
- vote policy remains permissive when the flag is disabled

Suggested helper:

```rust
fn validate_hybrid_proposal_vote_policy(&self, vote: &ProposalVote) -> Result<()> {
    if !self.config.feature_flags.require_hybrid_validator_signatures {
        return Ok(());
    }
    if vote.signature.scheme() != SignatureScheme::Hybrid {
        return Err(anyhow!("hybrid enforcement requires hybrid proposal vote signatures"));
    }
    Ok(())
}
```

Suggested tests:

```rust
#[tokio::test(flavor = "current_thread")]
async fn hybrid_enforcement_rejects_non_hybrid_proposal_vote_signature() { /* ... */ }

#[cfg(feature = "pq-ml-dsa")]
#[tokio::test(flavor = "current_thread")]
async fn hybrid_enforcement_accepts_hybrid_proposal_vote_signature() { /* ... */ }
```

- [ ] **Step 2: Run the focused proposal-vote tests and verify they fail**

Run:

```bash
cargo test -p entangrid-node hybrid_enforcement_rejects_non_hybrid_proposal_vote_signature
cargo test -p entangrid-node --features pq-ml-dsa hybrid_enforcement_accepts_hybrid_proposal_vote_signature
```

Expected:

- non-hybrid proposal-vote test fails because vote policy is still permissive

- [ ] **Step 3: Implement proposal-vote enforcement**

Call the helper from `import_proposal_vote(...)` after signer/signature verification and before the vote is stored:

```rust
self.validate_hybrid_proposal_vote_policy(&vote)?;
```

Keep the underlying crypto verifier generic.

- [ ] **Step 4: Re-run the focused proposal-vote tests until green**

Run:

```bash
cargo test -p entangrid-node hybrid_enforcement_rejects_non_hybrid_proposal_vote_signature
cargo test -p entangrid-node --features pq-ml-dsa hybrid_enforcement_accepts_hybrid_proposal_vote_signature
```

- [ ] **Step 5: Commit the proposal-vote slice**

```bash
git add crates/entangrid-node/src/lib.rs
git commit -m "enforce hybrid proposal vote signatures"
```

## Task 5: Update Fixtures, Docs, And Final Verification

**Files:**
- Modify: `crates/entangrid-sim/src/lib.rs`
- Modify: `README.md`
- Modify: `docs/architecture.md`
- Modify: `docs/protocol.md`
- Modify: `crates/entangrid-crypto/README.md`
- Modify: `crates/entangrid-types/README.md`

- [ ] **Step 1: Update fixture/config helpers only as needed**

Ensure localnet and test helpers can express the new flag without changing deterministic defaults.

Minimal target:

```rust
FeatureFlags {
    require_hybrid_validator_signatures: false,
    ..FeatureFlags::default()
}
```

- [ ] **Step 2: Update docs**

Document:

- the new `require_hybrid_validator_signatures` flag
- the fact that Stage 1E enforcement currently covers only:
  - blocks
  - proposal votes
- the fact that transactions, receipts, service evidence, and session/KEM remain out of scope

- [ ] **Step 3: Run the full verification pass**

Run:

```bash
cargo test -p entangrid-types -p entangrid-crypto -p entangrid-ledger -p entangrid-consensus -p entangrid-network -p entangrid-node -p entangrid-sim
cargo test -p entangrid-crypto -p entangrid-ledger -p entangrid-node --features pq-ml-dsa
cargo fmt
git diff --check
```

If sandboxed localhost tests fail with `Operation not permitted`, rerun the affected suites with unrestricted localhost access.

- [ ] **Step 4: Commit the documentation and verification slice**

```bash
git add crates/entangrid-sim/src/lib.rs README.md docs/architecture.md docs/protocol.md crates/entangrid-crypto/README.md crates/entangrid-types/README.md
git commit -m "document hybrid enforcement mode"
```
