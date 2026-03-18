use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};

use anyhow::{Result, anyhow};
use clap::{Parser, Subcommand, ValueEnum};
use entangrid_crypto::{DeterministicCryptoBackend, Signer};
use entangrid_types::{
    FaultProfile, FeatureFlags, GenesisConfig, LocalnetManifest, NodeConfig, PeerConfig,
    SignedTransaction, Transaction, ValidatorConfig, canonical_hash, empty_hash, now_unix_millis,
    validator_account,
};
use tokio::process::{Child, Command};
use tracing::info;

#[derive(Parser)]
#[command(name = "entangrid-sim")]
#[command(about = "Manage an Entangrid localhost validator network")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    InitLocalnet {
        #[arg(long, default_value_t = 4)]
        validators: usize,
        #[arg(long, default_value = "var/localnet")]
        base_dir: PathBuf,
        #[arg(long, default_value_t = 2_000)]
        slot_duration_millis: u64,
        #[arg(long, default_value_t = 10)]
        slots_per_epoch: u64,
        #[arg(long, default_value_t = 3_000)]
        start_delay_millis: u64,
        #[arg(long, default_value_t = false)]
        enable_service_gating: bool,
        #[arg(long, default_value_t = 2)]
        service_gating_start_epoch: u64,
        #[arg(long)]
        degraded_validator: Option<u64>,
        #[arg(long, default_value_t = 0)]
        degraded_delay_ms: u64,
        #[arg(long, default_value_t = 0.0)]
        degraded_drop_probability: f64,
        #[arg(long, default_value_t = false)]
        degraded_disable_outbound: bool,
    },
    Up {
        #[arg(long, default_value = "var/localnet")]
        base_dir: PathBuf,
    },
    Load {
        #[arg(long, default_value = "var/localnet")]
        base_dir: PathBuf,
        #[arg(long, value_enum, default_value_t = LoadScenario::Steady)]
        scenario: LoadScenario,
        #[arg(long, default_value_t = 12)]
        duration_secs: u64,
    },
}

#[derive(Copy, Clone, Debug, Eq, PartialEq, ValueEnum)]
pub enum LoadScenario {
    Idle,
    Steady,
    Bursty,
    LargeBlock,
}

pub async fn cli_main() -> Result<()> {
    let _ = tracing_subscriber::fmt()
        .with_env_filter("info")
        .with_target(false)
        .try_init();
    let cli = Cli::parse();
    match cli.command {
        Commands::InitLocalnet {
            validators,
            base_dir,
            slot_duration_millis,
            slots_per_epoch,
            start_delay_millis,
            enable_service_gating,
            service_gating_start_epoch,
            degraded_validator,
            degraded_delay_ms,
            degraded_drop_probability,
            degraded_disable_outbound,
        } => init_localnet(
            validators,
            &base_dir,
            slot_duration_millis,
            slots_per_epoch,
            start_delay_millis,
            enable_service_gating,
            service_gating_start_epoch,
            degraded_validator,
            degraded_delay_ms,
            degraded_drop_probability,
            degraded_disable_outbound,
        ),
        Commands::Up { base_dir } => up_localnet(&base_dir).await,
        Commands::Load {
            base_dir,
            scenario,
            duration_secs,
        } => load_scenario(&base_dir, scenario, duration_secs).await,
    }
}

pub fn init_localnet(
    validators: usize,
    base_dir: &Path,
    slot_duration_millis: u64,
    slots_per_epoch: u64,
    start_delay_millis: u64,
    enable_service_gating: bool,
    service_gating_start_epoch: u64,
    degraded_validator: Option<u64>,
    degraded_delay_ms: u64,
    degraded_drop_probability: f64,
    degraded_disable_outbound: bool,
) -> Result<()> {
    if validators < 4 {
        return Err(anyhow!("at least 4 validators are recommended"));
    }
    if let Some(degraded_validator) = degraded_validator {
        if degraded_validator == 0 || degraded_validator > validators as u64 {
            return Err(anyhow!(
                "degraded validator id must be within the validator set"
            ));
        }
    }
    fs::create_dir_all(base_dir)?;
    let genesis_path = base_dir.join("genesis.toml");
    let manifest_path = manifest_path(base_dir);

    let mut validator_configs = Vec::new();
    let mut initial_balances = BTreeMap::new();
    for index in 0..validators {
        let validator_id = (index + 1) as u64;
        let address = format!("127.0.0.1:{}", 4100 + index);
        validator_configs.push(ValidatorConfig {
            validator_id,
            stake: 100,
            address: address.clone(),
            dev_secret: format!("entangrid-dev-secret-{validator_id}"),
            public_identity: format!("validator-{validator_id}").into_bytes(),
        });
        initial_balances.insert(validator_account(validator_id), 1_000_000);
    }

    let genesis = GenesisConfig {
        chain_id: "entangrid-localnet".into(),
        epoch_seed: empty_hash(),
        genesis_time_unix_millis: now_unix_millis() + start_delay_millis,
        slot_duration_millis,
        slots_per_epoch,
        max_txs_per_block: 128,
        witness_count: 2,
        validators: validator_configs.clone(),
        initial_balances,
    };
    fs::write(&genesis_path, toml::to_string_pretty(&genesis)?)?;

    let mut node_configs = Vec::new();
    for validator in &validator_configs {
        let node_dir = base_dir.join(format!("node-{}", validator.validator_id));
        fs::create_dir_all(node_dir.join("inbox"))?;
        fs::create_dir_all(node_dir.join("processed"))?;
        let peers = validator_configs
            .iter()
            .filter(|peer| peer.validator_id != validator.validator_id)
            .map(|peer| PeerConfig {
                validator_id: peer.validator_id,
                address: peer.address.clone(),
            })
            .collect();
        let fault_profile = degraded_fault_profile(
            validator.validator_id,
            degraded_validator,
            degraded_delay_ms,
            degraded_drop_probability,
            degraded_disable_outbound,
        );
        let config = NodeConfig {
            validator_id: validator.validator_id,
            data_dir: node_dir.to_string_lossy().to_string(),
            genesis_path: genesis_path.to_string_lossy().to_string(),
            listen_address: validator.address.clone(),
            peers,
            log_path: node_dir.join("events.log").to_string_lossy().to_string(),
            metrics_path: node_dir.join("metrics.json").to_string_lossy().to_string(),
            feature_flags: FeatureFlags {
                enable_receipts: true,
                enable_service_gating,
                service_gating_start_epoch,
            },
            fault_profile,
            sync_on_startup: true,
        };
        let config_path = node_dir.join("node.toml");
        fs::write(&config_path, toml::to_string_pretty(&config)?)?;
        node_configs.push(config_path.to_string_lossy().to_string());
    }

    let manifest = LocalnetManifest {
        base_dir: base_dir.to_string_lossy().to_string(),
        genesis_path: genesis_path.to_string_lossy().to_string(),
        node_configs,
    };
    fs::write(manifest_path, toml::to_string_pretty(&manifest)?)?;
    Ok(())
}

pub async fn up_localnet(base_dir: &Path) -> Result<()> {
    let manifest = read_manifest(base_dir)?;
    let node_binary = node_binary_path()?;
    let mut children: Vec<Child> = Vec::new();

    for config_path in manifest.node_configs {
        let config_path = PathBuf::from(config_path);
        let config_contents = fs::read_to_string(&config_path)?;
        let config: NodeConfig = toml::from_str(&config_contents)?;
        let node_dir = PathBuf::from(&config.data_dir);
        let stdout_path = node_dir.join("stdout.log");
        let stderr_path = node_dir.join("stderr.log");
        let stdout = fs::File::create(stdout_path)?;
        let stderr = fs::File::create(stderr_path)?;

        let mut command = Command::new(&node_binary);
        command
            .arg("run")
            .arg("--config")
            .arg(&config_path)
            .stdout(Stdio::from(stdout))
            .stderr(Stdio::from(stderr));

        info!("starting node with {}", config_path.display());
        children.push(command.spawn()?);
    }

    tokio::signal::ctrl_c().await?;
    for child in &mut children {
        let _ = child.kill().await;
    }
    Ok(())
}

pub async fn load_scenario(
    base_dir: &Path,
    scenario: LoadScenario,
    duration_secs: u64,
) -> Result<()> {
    let manifest = read_manifest(base_dir)?;
    let genesis_contents = fs::read_to_string(&manifest.genesis_path)?;
    let genesis: GenesisConfig = toml::from_str(&genesis_contents)?;
    let crypto = DeterministicCryptoBackend::from_genesis(&genesis);
    let mut next_nonce = current_nonces(base_dir, &genesis)?;

    match scenario {
        LoadScenario::Idle => return Ok(()),
        LoadScenario::Steady => {
            for second in 0..duration_secs {
                for validator in &genesis.validators {
                    write_transaction_for_validator(
                        base_dir,
                        &genesis,
                        &crypto,
                        validator.validator_id,
                        1,
                        &mut next_nonce,
                        second,
                        false,
                    )?;
                }
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
        }
        LoadScenario::Bursty => {
            for second in 0..duration_secs {
                if second % 3 == 0 {
                    for validator in &genesis.validators {
                        for burst_index in 0..5 {
                            write_transaction_for_validator(
                                base_dir,
                                &genesis,
                                &crypto,
                                validator.validator_id,
                                1 + burst_index as u64,
                                &mut next_nonce,
                                second * 10 + burst_index as u64,
                                false,
                            )?;
                        }
                    }
                }
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
        }
        LoadScenario::LargeBlock => {
            for validator in &genesis.validators {
                for index in 0..(genesis.max_txs_per_block as u64 * 2) {
                    write_transaction_for_validator(
                        base_dir,
                        &genesis,
                        &crypto,
                        validator.validator_id,
                        1,
                        &mut next_nonce,
                        index,
                        true,
                    )?;
                }
            }
        }
    }

    Ok(())
}

fn write_transaction_for_validator(
    base_dir: &Path,
    genesis: &GenesisConfig,
    crypto: &DeterministicCryptoBackend,
    validator_id: u64,
    amount: u64,
    next_nonce: &mut BTreeMap<String, u64>,
    sequence: u64,
    large_memo: bool,
) -> Result<()> {
    let sender_account = validator_account(validator_id);
    let recipient_id =
        deterministic_recipient_id(genesis, validator_id, sequence, amount, large_memo)
            .ok_or_else(|| anyhow!("failed to select recipient for validator {validator_id}"))?;
    let recipient_account = validator_account(recipient_id);
    let nonce = next_nonce.entry(sender_account.clone()).or_default();
    let transaction = Transaction {
        from: sender_account.clone(),
        to: recipient_account,
        amount,
        nonce: *nonce,
        memo: if large_memo {
            Some("x".repeat(2048))
        } else {
            Some(format!("scenario-{sequence}"))
        },
    };
    let tx_hash = canonical_hash(&transaction);
    let signed = SignedTransaction {
        transaction,
        signer_id: validator_id,
        signature: crypto.sign(validator_id, &tx_hash)?,
        tx_hash,
        submitted_at_unix_millis: now_unix_millis(),
    };
    *nonce += 1;

    let inbox_path = base_dir
        .join(format!("node-{validator_id}"))
        .join("inbox")
        .join(format!("tx-{}-{sequence}.json", now_unix_millis()));
    fs::write(inbox_path, serde_json::to_vec_pretty(&signed)?)?;
    Ok(())
}

fn current_nonces(base_dir: &Path, genesis: &GenesisConfig) -> Result<BTreeMap<String, u64>> {
    let snapshot_path = base_dir.join("node-1").join("state_snapshot.json");
    let mut nonces = BTreeMap::new();
    if snapshot_path.exists() {
        let contents = fs::read_to_string(snapshot_path)?;
        let snapshot: entangrid_types::StateSnapshot = serde_json::from_str(&contents)?;
        nonces = snapshot.nonces;
    } else {
        for validator in &genesis.validators {
            nonces.insert(validator_account(validator.validator_id), 0);
        }
    }
    Ok(nonces)
}

fn deterministic_recipient_id(
    genesis: &GenesisConfig,
    sender_validator_id: u64,
    sequence: u64,
    amount: u64,
    large_memo: bool,
) -> Option<u64> {
    let recipients: Vec<_> = genesis
        .validators
        .iter()
        .map(|validator| validator.validator_id)
        .filter(|validator_id| *validator_id != sender_validator_id)
        .collect();
    if recipients.is_empty() {
        return None;
    }
    let selector = canonical_hash(&(
        genesis.chain_id.as_str(),
        sender_validator_id,
        sequence,
        amount,
        large_memo,
    ));
    let mut selector_bytes = [0u8; 8];
    selector_bytes.copy_from_slice(&selector[..8]);
    let index = u64::from_le_bytes(selector_bytes) as usize % recipients.len();
    Some(recipients[index])
}

fn degraded_fault_profile(
    validator_id: u64,
    degraded_validator: Option<u64>,
    degraded_delay_ms: u64,
    degraded_drop_probability: f64,
    degraded_disable_outbound: bool,
) -> FaultProfile {
    if degraded_validator != Some(validator_id) {
        return FaultProfile::default();
    }
    let outbound_drop_probability =
        if degraded_delay_ms == 0 && degraded_drop_probability == 0.0 && !degraded_disable_outbound
        {
            0.85
        } else {
            degraded_drop_probability
        };
    FaultProfile {
        artificial_delay_ms: degraded_delay_ms,
        outbound_drop_probability,
        pause_slot_production: false,
        disable_outbound: degraded_disable_outbound,
    }
}

fn manifest_path(base_dir: &Path) -> PathBuf {
    base_dir.join("localnet-manifest.toml")
}

fn read_manifest(base_dir: &Path) -> Result<LocalnetManifest> {
    let contents = fs::read_to_string(manifest_path(base_dir))?;
    Ok(toml::from_str(&contents)?)
}

fn node_binary_path() -> Result<PathBuf> {
    let current_exe = std::env::current_exe()?;
    let parent = current_exe
        .parent()
        .ok_or_else(|| anyhow!("sim executable has no parent directory"))?;
    let mut candidate = parent.join("entangrid-node");
    if cfg!(windows) {
        candidate.set_extension("exe");
    }
    if !candidate.exists() {
        return Err(anyhow!(
            "expected node binary at {}, build the workspace first",
            candidate.display()
        ));
    }
    Ok(candidate)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn init_localnet_creates_manifest() {
        let unique_dir =
            std::env::temp_dir().join(format!("entangrid-sim-test-{}", now_unix_millis()));
        init_localnet(
            4,
            &unique_dir,
            2_000,
            10,
            1_000,
            false,
            2,
            None,
            0,
            0.0,
            false,
        )
        .unwrap();
        assert!(manifest_path(&unique_dir).exists());
    }

    #[test]
    fn deterministic_recipient_selection_never_targets_sender() {
        let genesis = GenesisConfig {
            chain_id: "entangrid-test".into(),
            epoch_seed: empty_hash(),
            genesis_time_unix_millis: now_unix_millis(),
            slot_duration_millis: 1_000,
            slots_per_epoch: 5,
            max_txs_per_block: 16,
            witness_count: 2,
            validators: (1..=4)
                .map(|validator_id| ValidatorConfig {
                    validator_id,
                    stake: 100,
                    address: format!("127.0.0.1:{}", 4100 + validator_id),
                    dev_secret: format!("secret-{validator_id}"),
                    public_identity: vec![],
                })
                .collect(),
            initial_balances: BTreeMap::new(),
        };

        for sequence in 0..20 {
            for sender in 1..=4 {
                let recipient =
                    deterministic_recipient_id(&genesis, sender, sequence, 1, false).unwrap();
                assert_ne!(sender, recipient);
            }
        }
    }

    #[test]
    fn init_localnet_can_enable_service_gating_and_degrade_one_node() {
        let unique_dir =
            std::env::temp_dir().join(format!("entangrid-sim-degraded-test-{}", now_unix_millis()));
        init_localnet(
            4,
            &unique_dir,
            1_000,
            5,
            1_000,
            true,
            3,
            Some(3),
            0,
            0.75,
            false,
        )
        .unwrap();
        let node_three_path = unique_dir.join("node-3").join("node.toml");
        let contents = fs::read_to_string(node_three_path).unwrap();
        let node_config: NodeConfig = toml::from_str(&contents).unwrap();
        assert!(node_config.feature_flags.enable_service_gating);
        assert_eq!(node_config.feature_flags.service_gating_start_epoch, 3);
        assert_eq!(node_config.fault_profile.outbound_drop_probability, 0.75);
    }
}
