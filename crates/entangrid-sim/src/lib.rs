use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};

use anyhow::{Result, anyhow};
use clap::{Parser, Subcommand, ValueEnum};
use entangrid_crypto::{DeterministicCryptoBackend, Signer};
use entangrid_types::{
    FaultProfile, FeatureFlags, GenesisConfig, LocalnetManifest, NodeConfig, NodeMetrics,
    PeerConfig, ProtocolMessage, ServiceScoreWeights, SignedEnvelope, SignedTransaction,
    Transaction, ValidatorConfig, canonical_hash, default_service_delivery_weight,
    default_service_diversity_weight, default_service_gating_start_epoch,
    default_service_gating_threshold, default_service_penalty_weight,
    default_service_score_weights, default_service_score_window_epochs,
    default_service_uptime_weight, empty_hash, now_unix_millis, validator_account,
};
use serde::{Deserialize, Serialize};
use tokio::{
    io::AsyncWriteExt,
    net::TcpStream,
    process::{Child, Command},
};
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
        #[arg(long, default_value_t = false)]
        consensus_v2: bool,
        #[arg(long, default_value_t = default_service_gating_start_epoch())]
        service_gating_start_epoch: u64,
        #[arg(long, default_value_t = default_service_gating_threshold())]
        service_gating_threshold: f64,
        #[arg(long)]
        service_score_window_epochs: Option<u64>,
        #[arg(long, default_value_t = default_service_uptime_weight())]
        service_score_uptime_weight: f64,
        #[arg(long, default_value_t = default_service_delivery_weight())]
        service_score_delivery_weight: f64,
        #[arg(long, default_value_t = default_service_diversity_weight())]
        service_score_diversity_weight: f64,
        #[arg(long, default_value_t = default_service_penalty_weight())]
        service_score_penalty_weight: f64,
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
    Report {
        #[arg(long, default_value = "var/localnet")]
        base_dir: PathBuf,
    },
    Matrix {
        #[arg(long, default_value = "var/localnet-matrix")]
        base_dir: PathBuf,
        #[arg(long, default_value = "test-results")]
        output_dir: PathBuf,
        #[arg(long, default_value_t = 18)]
        settle_secs: u64,
    },
}

#[derive(Copy, Clone, Debug, Eq, PartialEq, Serialize, ValueEnum)]
pub enum LoadScenario {
    Idle,
    Steady,
    Bursty,
    LargeBlock,
}

#[derive(Clone, Debug)]
struct MatrixScenario {
    name: &'static str,
    validators: usize,
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
    load_scenario: LoadScenario,
    load_duration_secs: u64,
    abuse_pattern: Option<AbusePattern>,
    settle_secs: u64,
    expected_lowest_validator: Option<u64>,
    min_lowest_score: Option<f64>,
    max_lowest_score: Option<f64>,
    min_validator_gating_rejections: Option<(u64, u64)>,
    max_non_target_below_threshold_count: Option<usize>,
    max_non_target_gating_rejections: Option<u64>,
    min_total_peer_rate_limit_drops: Option<u64>,
    min_total_inbound_session_drops: Option<u64>,
}

#[derive(Clone, Debug)]
enum AbusePattern {
    SyncControlFlood {
        source_validator_id: u64,
        duration_secs: u64,
        bursts_per_second: u64,
    },
    InboundConnectionFlood {
        target_validator_id: u64,
        concurrent_connections: usize,
        hold_millis: u64,
        rounds: u64,
    },
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
            consensus_v2,
            service_gating_start_epoch,
            service_gating_threshold,
            service_score_window_epochs,
            service_score_uptime_weight,
            service_score_delivery_weight,
            service_score_diversity_weight,
            service_score_penalty_weight,
            degraded_validator,
            degraded_delay_ms,
            degraded_drop_probability,
            degraded_disable_outbound,
        } => {
            let service_score_window_epochs = service_score_window_epochs.unwrap_or_else(|| {
                recommended_service_score_window_epochs_for_validators(validators)
            });
            init_localnet(
                validators,
                &base_dir,
                slot_duration_millis,
                slots_per_epoch,
                start_delay_millis,
                enable_service_gating,
                service_gating_start_epoch,
                service_gating_threshold,
                service_score_window_epochs,
                ServiceScoreWeights {
                    uptime_weight: service_score_uptime_weight,
                    delivery_weight: service_score_delivery_weight,
                    diversity_weight: service_score_diversity_weight,
                    penalty_weight: service_score_penalty_weight,
                },
                degraded_validator,
                degraded_delay_ms,
                degraded_drop_probability,
                degraded_disable_outbound,
                consensus_v2,
            )
        }
        Commands::Up { base_dir } => up_localnet(&base_dir).await,
        Commands::Load {
            base_dir,
            scenario,
            duration_secs,
        } => load_scenario(&base_dir, scenario, duration_secs).await,
        Commands::Report { base_dir } => report_localnet(&base_dir),
        Commands::Matrix {
            base_dir,
            output_dir,
            settle_secs,
        } => run_rigorous_matrix(&base_dir, &output_dir, settle_secs).await,
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
    service_gating_threshold: f64,
    service_score_window_epochs: u64,
    service_score_weights: ServiceScoreWeights,
    degraded_validator: Option<u64>,
    degraded_delay_ms: u64,
    degraded_drop_probability: f64,
    degraded_disable_outbound: bool,
    consensus_v2: bool,
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
        witness_count: if consensus_v2 {
            recommended_v2_witness_count_for_validators(validators)
        } else {
            2.min(validators.saturating_sub(1))
        },
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
                consensus_v2,
                service_gating_start_epoch,
                service_gating_threshold,
                service_score_window_epochs,
                service_score_weights: service_score_weights.clone(),
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
    let mut children = spawn_localnet_children(base_dir).await?;

    tokio::signal::ctrl_c().await?;
    shutdown_children(&mut children).await;
    Ok(())
}

async fn spawn_localnet_children(base_dir: &Path) -> Result<Vec<Child>> {
    let manifest = read_manifest(base_dir)?;
    ensure_node_binary_built().await?;
    refresh_genesis_time_for_fresh_localnet(&manifest)?;
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

    Ok(children)
}

async fn shutdown_children(children: &mut [Child]) {
    #[cfg(unix)]
    for child in children.iter_mut() {
        if let Some(pid) = child.id() {
            let _ = Command::new("kill")
                .arg("-INT")
                .arg(pid.to_string())
                .status()
                .await;
        }
    }

    for child in children {
        let exited_cleanly = tokio::time::timeout(Duration::from_secs(2), child.wait())
            .await
            .is_ok();

        if !exited_cleanly {
            let _ = child.kill().await;
            let _ = child.wait().await;
        }
    }
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

pub fn report_localnet(base_dir: &Path) -> Result<()> {
    let manifest = read_manifest(base_dir)?;
    let report = build_localnet_report(base_dir, &manifest)?;
    println!("{}", report.render_text());
    Ok(())
}

pub async fn run_rigorous_matrix(
    base_dir: &Path,
    output_dir: &Path,
    settle_secs: u64,
) -> Result<()> {
    fs::create_dir_all(base_dir)?;
    fs::create_dir_all(output_dir)?;

    let run_id = now_unix_millis();
    let mut scenario_results = Vec::new();
    for scenario in rigorous_matrix_scenarios(settle_secs) {
        let scenario_base_dir = base_dir.join(format!("{}-{run_id}", scenario.name));
        info!("running rigorous scenario {}", scenario.name);
        scenario_results.push(run_matrix_scenario(&scenario_base_dir, &scenario).await?);
    }

    let report = MatrixReport {
        generated_at_unix_millis: now_unix_millis(),
        base_dir: base_dir.to_string_lossy().to_string(),
        output_dir: output_dir.to_string_lossy().to_string(),
        scenarios: scenario_results,
    };

    let json_path = output_dir.join(format!("rigorous-matrix-{run_id}.json"));
    let markdown_path = output_dir.join(format!("rigorous-matrix-{run_id}.md"));
    fs::write(&json_path, serde_json::to_vec_pretty(&report)?)?;
    fs::write(&markdown_path, report.render_markdown())?;

    println!("{}", report.render_text());
    println!("wrote {}", markdown_path.display());
    println!("wrote {}", json_path.display());
    Ok(())
}

async fn run_matrix_scenario(
    base_dir: &Path,
    scenario: &MatrixScenario,
) -> Result<MatrixScenarioResult> {
    init_localnet(
        scenario.validators,
        base_dir,
        scenario.slot_duration_millis,
        scenario.slots_per_epoch,
        scenario.start_delay_millis,
        scenario.enable_service_gating,
        scenario.service_gating_start_epoch,
        scenario.service_gating_threshold,
        scenario.service_score_window_epochs,
        scenario.service_score_weights.clone(),
        scenario.degraded_validator,
        scenario.degraded_delay_ms,
        scenario.degraded_drop_probability,
        scenario.degraded_disable_outbound,
        false,
    )?;

    let mut children = spawn_localnet_children(base_dir).await?;
    let run_result = async {
        let load_future = load_scenario(
            base_dir,
            scenario.load_scenario,
            scenario.load_duration_secs,
        );
        let abuse_future = run_abuse_pattern(base_dir, scenario.abuse_pattern.clone());
        let (load_result, abuse_result) = tokio::join!(load_future, abuse_future);
        load_result?;
        abuse_result?;
        wait_for_scenario_expectations(base_dir, scenario).await
    }
    .await;
    shutdown_children(&mut children).await;
    let (localnet_report, structural_report) = run_result?;
    let policy_target_validator = scenario
        .degraded_validator
        .or(scenario.expected_lowest_validator);
    let non_target_below_threshold_count = count_non_target_below_threshold(
        &localnet_report,
        scenario.service_gating_threshold,
        policy_target_validator,
    );
    let non_target_gating_rejections =
        total_non_target_gating_rejections(&localnet_report, policy_target_validator);
    Ok(MatrixScenarioResult {
        name: scenario.name.to_string(),
        base_dir: base_dir.to_string_lossy().to_string(),
        load_scenario: scenario.load_scenario,
        load_duration_secs: scenario.load_duration_secs,
        abuse_pattern: scenario.abuse_pattern.as_ref().map(describe_abuse_pattern),
        settle_secs: scenario.settle_secs,
        gating_enabled: scenario.enable_service_gating,
        gating_threshold: scenario.service_gating_threshold,
        score_window_epochs: scenario.service_score_window_epochs,
        score_weights: scenario.service_score_weights.clone(),
        policy_target_validator,
        expected_lowest_validator: scenario.expected_lowest_validator,
        min_lowest_score: scenario.min_lowest_score,
        max_lowest_score: scenario.max_lowest_score,
        min_validator_gating_rejections: scenario.min_validator_gating_rejections,
        max_non_target_below_threshold_count: scenario.max_non_target_below_threshold_count,
        max_non_target_gating_rejections: scenario.max_non_target_gating_rejections,
        min_total_peer_rate_limit_drops: scenario.min_total_peer_rate_limit_drops,
        min_total_inbound_session_drops: scenario.min_total_inbound_session_drops,
        non_target_below_threshold_count,
        non_target_gating_rejections,
        localnet_report,
        structural_report,
    })
}

fn describe_abuse_pattern(pattern: &AbusePattern) -> String {
    match pattern {
        AbusePattern::SyncControlFlood {
            source_validator_id,
            duration_secs,
            bursts_per_second,
        } => format!(
            "sync-control-flood source v{source_validator_id} duration {duration_secs}s bursts/s {bursts_per_second}"
        ),
        AbusePattern::InboundConnectionFlood {
            target_validator_id,
            concurrent_connections,
            hold_millis,
            rounds,
        } => format!(
            "inbound-connection-flood target v{target_validator_id} connections {concurrent_connections} hold {hold_millis}ms rounds {rounds}"
        ),
    }
}

async fn run_abuse_pattern(base_dir: &Path, pattern: Option<AbusePattern>) -> Result<()> {
    let Some(pattern) = pattern else {
        return Ok(());
    };
    tokio::time::sleep(Duration::from_secs(2)).await;
    let manifest = read_manifest(base_dir)?;
    let genesis_contents = fs::read_to_string(&manifest.genesis_path)?;
    let genesis: GenesisConfig = toml::from_str(&genesis_contents)?;
    let crypto = DeterministicCryptoBackend::from_genesis(&genesis);
    let node_configs = read_node_configs(&manifest)?;

    match pattern {
        AbusePattern::SyncControlFlood {
            source_validator_id,
            duration_secs,
            bursts_per_second,
        } => {
            let source_config = node_configs
                .iter()
                .find(|config| config.validator_id == source_validator_id)
                .ok_or_else(|| anyhow!("unknown abuse source validator {source_validator_id}"))?;
            let interval_millis = (1_000 / bursts_per_second.max(1)).max(1);
            let start = std::time::Instant::now();
            while start.elapsed() < Duration::from_secs(duration_secs) {
                for peer in &source_config.peers {
                    let payload = ProtocolMessage::ReceiptFetch {
                        requester_id: source_validator_id,
                        epoch: 0,
                        validator_id: source_validator_id,
                    };
                    if let Err(error) =
                        send_protocol_message(&crypto, source_validator_id, peer, payload).await
                    {
                        info!(
                            "ignoring failed sync-control flood message from validator {} to {}: {}",
                            source_validator_id, peer.validator_id, error
                        );
                    }
                }
                tokio::time::sleep(Duration::from_millis(interval_millis)).await;
            }
        }
        AbusePattern::InboundConnectionFlood {
            target_validator_id,
            concurrent_connections,
            hold_millis,
            rounds,
        } => {
            let target_config = node_configs
                .iter()
                .find(|config| config.validator_id == target_validator_id)
                .ok_or_else(|| anyhow!("unknown abuse target validator {target_validator_id}"))?;
            for _ in 0..rounds {
                let mut streams = Vec::with_capacity(concurrent_connections);
                for _ in 0..concurrent_connections {
                    if let Ok(stream) = TcpStream::connect(&target_config.listen_address).await {
                        streams.push(stream);
                    }
                }
                tokio::time::sleep(Duration::from_millis(hold_millis)).await;
                drop(streams);
                tokio::time::sleep(Duration::from_millis(150)).await;
            }
        }
    }

    Ok(())
}

async fn send_protocol_message(
    crypto: &DeterministicCryptoBackend,
    from_validator_id: u64,
    peer: &PeerConfig,
    payload: ProtocolMessage,
) -> Result<()> {
    let message_hash = canonical_hash(&payload);
    let envelope = SignedEnvelope {
        from_validator_id,
        message_hash,
        signature: crypto.sign(from_validator_id, &message_hash)?,
        payload,
    };
    let bytes = bincode::serde::encode_to_vec(&envelope, bincode::config::standard())?;
    let mut stream = TcpStream::connect(&peer.address).await?;
    stream.write_u32(bytes.len() as u32).await?;
    stream.write_all(&bytes).await?;
    stream.flush().await?;
    Ok(())
}

fn read_node_configs(manifest: &LocalnetManifest) -> Result<Vec<NodeConfig>> {
    manifest
        .node_configs
        .iter()
        .map(|config_path| {
            let contents = fs::read_to_string(config_path)?;
            Ok(toml::from_str(&contents)?)
        })
        .collect()
}

async fn wait_for_scenario_expectations(
    base_dir: &Path,
    scenario: &MatrixScenario,
) -> Result<(LocalnetReport, StructuralReport)> {
    let manifest = read_manifest(base_dir)?;
    let deadline = std::time::Instant::now() + Duration::from_secs(scenario.settle_secs);

    loop {
        if let (Ok(localnet_report), Ok(structural_report)) = (
            build_localnet_report(base_dir, &manifest),
            build_structural_report(&manifest),
        ) {
            if scenario_expectations_met(scenario, &manifest, &localnet_report, &structural_report)
            {
                return Ok((localnet_report, structural_report));
            }
        }

        if std::time::Instant::now() >= deadline {
            let localnet_report = build_localnet_report(base_dir, &manifest)?;
            let structural_report = build_structural_report(&manifest)?;
            return Ok((localnet_report, structural_report));
        }

        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}

fn scenario_expectations_met(
    scenario: &MatrixScenario,
    manifest: &LocalnetManifest,
    localnet_report: &LocalnetReport,
    structural_report: &StructuralReport,
) -> bool {
    if structural_report.same_chain_count != manifest.node_configs.len()
        || !structural_report.all_parent_ok
        || !structural_report.all_tips_match
        || !structural_report.all_stderr_clean
    {
        return false;
    }

    if let Some(expected_validator) = scenario.expected_lowest_validator {
        if localnet_report
            .lowest_score
            .map(|(validator_id, _)| validator_id)
            != Some(expected_validator)
        {
            return false;
        }
    }

    if let Some(min_score) = scenario.min_lowest_score {
        if localnet_report
            .lowest_score
            .map(|(_, score)| score < min_score)
            .unwrap_or(true)
        {
            return false;
        }
    }

    if let Some(max_score) = scenario.max_lowest_score {
        if localnet_report
            .lowest_score
            .map(|(_, score)| score > max_score)
            .unwrap_or(true)
        {
            return false;
        }
    }

    if let Some((validator_id, minimum)) = scenario.min_validator_gating_rejections {
        if localnet_report
            .validators
            .iter()
            .find(|validator| validator.validator_id == validator_id)
            .map(|validator| validator.service_gating_rejections < minimum)
            .unwrap_or(true)
        {
            return false;
        }
    }

    if let Some(minimum) = scenario.min_total_peer_rate_limit_drops {
        if localnet_report.total_peer_rate_limit_drops < minimum {
            return false;
        }
    }

    if let Some(minimum) = scenario.min_total_inbound_session_drops {
        if localnet_report.total_inbound_session_drops < minimum {
            return false;
        }
    }

    let policy_target_validator = scenario
        .degraded_validator
        .or(scenario.expected_lowest_validator);
    if let Some(maximum) = scenario.max_non_target_below_threshold_count {
        if count_non_target_below_threshold(
            localnet_report,
            scenario.service_gating_threshold,
            policy_target_validator,
        ) > maximum
        {
            return false;
        }
    }

    if let Some(maximum) = scenario.max_non_target_gating_rejections {
        if total_non_target_gating_rejections(localnet_report, policy_target_validator) > maximum {
            return false;
        }
    }

    true
}

fn count_non_target_below_threshold(
    localnet_report: &LocalnetReport,
    gating_threshold: f64,
    target_validator: Option<u64>,
) -> usize {
    localnet_report
        .validators
        .iter()
        .filter(|validator| {
            Some(validator.validator_id) != target_validator
                && validator.last_local_service_score < gating_threshold
        })
        .count()
}

fn total_non_target_gating_rejections(
    localnet_report: &LocalnetReport,
    target_validator: Option<u64>,
) -> u64 {
    localnet_report
        .validators
        .iter()
        .filter(|validator| Some(validator.validator_id) != target_validator)
        .map(|validator| validator.service_gating_rejections)
        .sum()
}

fn rigorous_matrix_scenarios(settle_secs: u64) -> Vec<MatrixScenario> {
    vec![
        MatrixScenario {
            name: "baseline-4-steady",
            validators: 4,
            slot_duration_millis: 1_000,
            slots_per_epoch: 5,
            start_delay_millis: 1_000,
            enable_service_gating: false,
            service_gating_start_epoch: default_service_gating_start_epoch(),
            service_gating_threshold: default_service_gating_threshold(),
            service_score_window_epochs: recommended_service_score_window_epochs_for_validators(4),
            service_score_weights: default_service_score_weights(),
            degraded_validator: None,
            degraded_delay_ms: 0,
            degraded_drop_probability: 0.0,
            degraded_disable_outbound: false,
            load_scenario: LoadScenario::Steady,
            load_duration_secs: 12,
            abuse_pattern: None,
            settle_secs,
            expected_lowest_validator: None,
            min_lowest_score: Some(0.95),
            max_lowest_score: None,
            min_validator_gating_rejections: None,
            max_non_target_below_threshold_count: None,
            max_non_target_gating_rejections: None,
            min_total_peer_rate_limit_drops: None,
            min_total_inbound_session_drops: None,
        },
        MatrixScenario {
            name: "baseline-6-bursty",
            validators: 6,
            slot_duration_millis: 800,
            slots_per_epoch: 6,
            start_delay_millis: 1_000,
            enable_service_gating: false,
            service_gating_start_epoch: default_service_gating_start_epoch(),
            service_gating_threshold: default_service_gating_threshold(),
            service_score_window_epochs: recommended_service_score_window_epochs_for_validators(6),
            service_score_weights: default_service_score_weights(),
            degraded_validator: None,
            degraded_delay_ms: 0,
            degraded_drop_probability: 0.0,
            degraded_disable_outbound: false,
            load_scenario: LoadScenario::Bursty,
            load_duration_secs: 18,
            abuse_pattern: None,
            settle_secs,
            expected_lowest_validator: None,
            min_lowest_score: Some(0.45),
            max_lowest_score: None,
            min_validator_gating_rejections: None,
            max_non_target_below_threshold_count: None,
            max_non_target_gating_rejections: None,
            min_total_peer_rate_limit_drops: None,
            min_total_inbound_session_drops: None,
        },
        MatrixScenario {
            name: "gated-6-bursty",
            validators: 6,
            slot_duration_millis: 800,
            slots_per_epoch: 6,
            start_delay_millis: 1_000,
            enable_service_gating: true,
            service_gating_start_epoch: default_service_gating_start_epoch(),
            service_gating_threshold: default_service_gating_threshold(),
            service_score_window_epochs: recommended_service_score_window_epochs_for_validators(6),
            service_score_weights: default_service_score_weights(),
            degraded_validator: None,
            degraded_delay_ms: 0,
            degraded_drop_probability: 0.0,
            degraded_disable_outbound: false,
            load_scenario: LoadScenario::Bursty,
            load_duration_secs: 18,
            abuse_pattern: None,
            settle_secs,
            expected_lowest_validator: None,
            min_lowest_score: Some(0.70),
            max_lowest_score: None,
            min_validator_gating_rejections: None,
            max_non_target_below_threshold_count: Some(0),
            max_non_target_gating_rejections: Some(0),
            min_total_peer_rate_limit_drops: None,
            min_total_inbound_session_drops: None,
        },
        MatrixScenario {
            name: "gated-drop85",
            validators: 4,
            slot_duration_millis: 1_000,
            slots_per_epoch: 5,
            start_delay_millis: 1_000,
            enable_service_gating: true,
            service_gating_start_epoch: default_service_gating_start_epoch(),
            service_gating_threshold: default_service_gating_threshold(),
            service_score_window_epochs: recommended_service_score_window_epochs_for_validators(4),
            service_score_weights: default_service_score_weights(),
            degraded_validator: Some(4),
            degraded_delay_ms: 0,
            degraded_drop_probability: 0.85,
            degraded_disable_outbound: false,
            load_scenario: LoadScenario::Steady,
            load_duration_secs: 18,
            abuse_pattern: None,
            settle_secs,
            expected_lowest_validator: Some(4),
            min_lowest_score: None,
            max_lowest_score: Some(0.85),
            min_validator_gating_rejections: None,
            max_non_target_below_threshold_count: Some(0),
            max_non_target_gating_rejections: Some(0),
            min_total_peer_rate_limit_drops: None,
            min_total_inbound_session_drops: None,
        },
        MatrixScenario {
            name: "gated-drop95",
            validators: 4,
            slot_duration_millis: 1_000,
            slots_per_epoch: 5,
            start_delay_millis: 1_000,
            enable_service_gating: true,
            service_gating_start_epoch: default_service_gating_start_epoch(),
            service_gating_threshold: default_service_gating_threshold(),
            service_score_window_epochs: recommended_service_score_window_epochs_for_validators(4),
            service_score_weights: default_service_score_weights(),
            degraded_validator: Some(3),
            degraded_delay_ms: 0,
            degraded_drop_probability: 0.95,
            degraded_disable_outbound: false,
            load_scenario: LoadScenario::Steady,
            load_duration_secs: 20,
            abuse_pattern: None,
            settle_secs,
            expected_lowest_validator: Some(3),
            min_lowest_score: None,
            max_lowest_score: None,
            min_validator_gating_rejections: Some((3, 1)),
            max_non_target_below_threshold_count: Some(0),
            max_non_target_gating_rejections: Some(0),
            min_total_peer_rate_limit_drops: None,
            min_total_inbound_session_drops: None,
        },
        MatrixScenario {
            name: "gated-outbound-disabled",
            validators: 4,
            slot_duration_millis: 1_000,
            slots_per_epoch: 5,
            start_delay_millis: 1_000,
            enable_service_gating: true,
            service_gating_start_epoch: default_service_gating_start_epoch(),
            service_gating_threshold: default_service_gating_threshold(),
            service_score_window_epochs: recommended_service_score_window_epochs_for_validators(4),
            service_score_weights: default_service_score_weights(),
            degraded_validator: Some(3),
            degraded_delay_ms: 0,
            degraded_drop_probability: 0.0,
            degraded_disable_outbound: true,
            load_scenario: LoadScenario::Steady,
            load_duration_secs: 20,
            abuse_pattern: None,
            settle_secs,
            expected_lowest_validator: Some(3),
            min_lowest_score: None,
            max_lowest_score: Some(0.05),
            min_validator_gating_rejections: Some((3, 1)),
            max_non_target_below_threshold_count: Some(0),
            max_non_target_gating_rejections: Some(0),
            min_total_peer_rate_limit_drops: None,
            min_total_inbound_session_drops: None,
        },
        MatrixScenario {
            name: "policy-threshold-025",
            validators: 4,
            slot_duration_millis: 1_000,
            slots_per_epoch: 5,
            start_delay_millis: 1_000,
            enable_service_gating: true,
            service_gating_start_epoch: default_service_gating_start_epoch(),
            service_gating_threshold: 0.25,
            service_score_window_epochs: recommended_service_score_window_epochs_for_validators(4),
            service_score_weights: default_service_score_weights(),
            degraded_validator: Some(3),
            degraded_delay_ms: 0,
            degraded_drop_probability: 0.95,
            degraded_disable_outbound: false,
            load_scenario: LoadScenario::Steady,
            load_duration_secs: 20,
            abuse_pattern: None,
            settle_secs,
            expected_lowest_validator: Some(3),
            min_lowest_score: None,
            max_lowest_score: None,
            min_validator_gating_rejections: Some((3, 1)),
            max_non_target_below_threshold_count: Some(0),
            max_non_target_gating_rejections: Some(0),
            min_total_peer_rate_limit_drops: None,
            min_total_inbound_session_drops: None,
        },
        MatrixScenario {
            name: "policy-threshold-055",
            validators: 4,
            slot_duration_millis: 1_000,
            slots_per_epoch: 5,
            start_delay_millis: 1_000,
            enable_service_gating: true,
            service_gating_start_epoch: default_service_gating_start_epoch(),
            service_gating_threshold: 0.55,
            service_score_window_epochs: recommended_service_score_window_epochs_for_validators(4),
            service_score_weights: default_service_score_weights(),
            degraded_validator: Some(3),
            degraded_delay_ms: 0,
            degraded_drop_probability: 0.95,
            degraded_disable_outbound: false,
            load_scenario: LoadScenario::Steady,
            load_duration_secs: 20,
            abuse_pattern: None,
            settle_secs,
            expected_lowest_validator: Some(3),
            min_lowest_score: None,
            max_lowest_score: None,
            min_validator_gating_rejections: Some((3, 1)),
            max_non_target_below_threshold_count: Some(0),
            max_non_target_gating_rejections: Some(0),
            min_total_peer_rate_limit_drops: None,
            min_total_inbound_session_drops: None,
        },
        MatrixScenario {
            name: "policy-window-1",
            validators: 4,
            slot_duration_millis: 1_000,
            slots_per_epoch: 5,
            start_delay_millis: 1_000,
            enable_service_gating: true,
            service_gating_start_epoch: default_service_gating_start_epoch(),
            service_gating_threshold: default_service_gating_threshold(),
            service_score_window_epochs: 1,
            service_score_weights: default_service_score_weights(),
            degraded_validator: Some(3),
            degraded_delay_ms: 0,
            degraded_drop_probability: 0.95,
            degraded_disable_outbound: false,
            load_scenario: LoadScenario::Steady,
            load_duration_secs: 22,
            abuse_pattern: None,
            settle_secs,
            expected_lowest_validator: Some(3),
            min_lowest_score: None,
            max_lowest_score: None,
            min_validator_gating_rejections: Some((3, 1)),
            max_non_target_below_threshold_count: Some(1),
            max_non_target_gating_rejections: Some(1),
            min_total_peer_rate_limit_drops: None,
            min_total_inbound_session_drops: None,
        },
        MatrixScenario {
            name: "policy-window-8",
            validators: 4,
            slot_duration_millis: 1_000,
            slots_per_epoch: 5,
            start_delay_millis: 1_000,
            enable_service_gating: true,
            service_gating_start_epoch: default_service_gating_start_epoch(),
            service_gating_threshold: default_service_gating_threshold(),
            service_score_window_epochs: 8,
            service_score_weights: default_service_score_weights(),
            degraded_validator: Some(3),
            degraded_delay_ms: 0,
            degraded_drop_probability: 0.95,
            degraded_disable_outbound: false,
            load_scenario: LoadScenario::Steady,
            load_duration_secs: 24,
            abuse_pattern: None,
            settle_secs,
            expected_lowest_validator: Some(3),
            min_lowest_score: None,
            max_lowest_score: None,
            min_validator_gating_rejections: Some((3, 1)),
            max_non_target_below_threshold_count: Some(0),
            max_non_target_gating_rejections: Some(0),
            min_total_peer_rate_limit_drops: None,
            min_total_inbound_session_drops: None,
        },
        MatrixScenario {
            name: "policy-penalty-050",
            validators: 4,
            slot_duration_millis: 1_000,
            slots_per_epoch: 5,
            start_delay_millis: 1_000,
            enable_service_gating: true,
            service_gating_start_epoch: default_service_gating_start_epoch(),
            service_gating_threshold: default_service_gating_threshold(),
            service_score_window_epochs: recommended_service_score_window_epochs_for_validators(4),
            service_score_weights: ServiceScoreWeights {
                penalty_weight: 0.50,
                ..default_service_score_weights()
            },
            degraded_validator: Some(3),
            degraded_delay_ms: 0,
            degraded_drop_probability: 0.95,
            degraded_disable_outbound: false,
            load_scenario: LoadScenario::Steady,
            load_duration_secs: 20,
            abuse_pattern: None,
            settle_secs,
            expected_lowest_validator: Some(3),
            min_lowest_score: None,
            max_lowest_score: None,
            min_validator_gating_rejections: Some((3, 1)),
            max_non_target_below_threshold_count: Some(0),
            max_non_target_gating_rejections: Some(0),
            min_total_peer_rate_limit_drops: None,
            min_total_inbound_session_drops: None,
        },
        MatrixScenario {
            name: "policy-penalty-150",
            validators: 4,
            slot_duration_millis: 1_000,
            slots_per_epoch: 5,
            start_delay_millis: 1_000,
            enable_service_gating: true,
            service_gating_start_epoch: default_service_gating_start_epoch(),
            service_gating_threshold: default_service_gating_threshold(),
            service_score_window_epochs: recommended_service_score_window_epochs_for_validators(4),
            service_score_weights: ServiceScoreWeights {
                penalty_weight: 1.50,
                ..default_service_score_weights()
            },
            degraded_validator: Some(3),
            degraded_delay_ms: 0,
            degraded_drop_probability: 0.95,
            degraded_disable_outbound: false,
            load_scenario: LoadScenario::Steady,
            load_duration_secs: 20,
            abuse_pattern: None,
            settle_secs,
            expected_lowest_validator: Some(3),
            min_lowest_score: None,
            max_lowest_score: None,
            min_validator_gating_rejections: Some((3, 1)),
            max_non_target_below_threshold_count: Some(0),
            max_non_target_gating_rejections: Some(0),
            min_total_peer_rate_limit_drops: None,
            min_total_inbound_session_drops: None,
        },
        MatrixScenario {
            name: "abuse-sync-control-flood",
            validators: 4,
            slot_duration_millis: 1_000,
            slots_per_epoch: 5,
            start_delay_millis: 1_000,
            enable_service_gating: false,
            service_gating_start_epoch: default_service_gating_start_epoch(),
            service_gating_threshold: default_service_gating_threshold(),
            service_score_window_epochs: recommended_service_score_window_epochs_for_validators(4),
            service_score_weights: default_service_score_weights(),
            degraded_validator: None,
            degraded_delay_ms: 0,
            degraded_drop_probability: 0.0,
            degraded_disable_outbound: false,
            load_scenario: LoadScenario::Steady,
            load_duration_secs: 12,
            abuse_pattern: Some(AbusePattern::SyncControlFlood {
                source_validator_id: 4,
                duration_secs: 8,
                bursts_per_second: 20,
            }),
            settle_secs,
            expected_lowest_validator: None,
            min_lowest_score: Some(0.85),
            max_lowest_score: None,
            min_validator_gating_rejections: None,
            max_non_target_below_threshold_count: None,
            max_non_target_gating_rejections: None,
            min_total_peer_rate_limit_drops: Some(8),
            min_total_inbound_session_drops: None,
        },
        MatrixScenario {
            name: "abuse-inbound-connection-flood",
            validators: 4,
            slot_duration_millis: 1_000,
            slots_per_epoch: 5,
            start_delay_millis: 1_000,
            enable_service_gating: false,
            service_gating_start_epoch: default_service_gating_start_epoch(),
            service_gating_threshold: default_service_gating_threshold(),
            service_score_window_epochs: recommended_service_score_window_epochs_for_validators(4),
            service_score_weights: default_service_score_weights(),
            degraded_validator: None,
            degraded_delay_ms: 0,
            degraded_drop_probability: 0.0,
            degraded_disable_outbound: false,
            load_scenario: LoadScenario::Steady,
            load_duration_secs: 12,
            abuse_pattern: Some(AbusePattern::InboundConnectionFlood {
                target_validator_id: 1,
                concurrent_connections: 96,
                hold_millis: 750,
                rounds: 3,
            }),
            settle_secs,
            expected_lowest_validator: None,
            min_lowest_score: Some(0.85),
            max_lowest_score: None,
            min_validator_gating_rejections: None,
            max_non_target_below_threshold_count: None,
            max_non_target_gating_rejections: None,
            min_total_peer_rate_limit_drops: None,
            min_total_inbound_session_drops: Some(1),
        },
    ]
}

fn recommended_service_score_window_epochs_for_validators(validators: usize) -> u64 {
    default_service_score_window_epochs().max(validators as u64)
}

fn recommended_v2_witness_count_for_validators(validators: usize) -> usize {
    let max_relations = validators.saturating_sub(1);
    if max_relations <= 3 {
        return max_relations;
    }
    let mut power = 1usize;
    let mut ceil_log2 = 0usize;
    while power < validators {
        power <<= 1;
        ceil_log2 += 1;
    }
    (ceil_log2 + 1).clamp(3, max_relations)
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

#[derive(Clone, Debug, PartialEq, Serialize)]
struct ValidatorReport {
    validator_id: u64,
    current_epoch: u64,
    current_slot: u64,
    last_local_service_score: f64,
    service_score_weights: ServiceScoreWeights,
    failed_sessions: u64,
    invalid_receipts: u64,
    service_gating_rejections: u64,
    missed_proposer_slots: u64,
    duplicate_receipts_ignored: u64,
    peer_rate_limit_drops: u64,
    inbound_session_drops: u64,
    blocks_proposed: u64,
    blocks_validated: u64,
    tx_ingress: u64,
    receipts_created: u64,
    service_attestations_emitted: u64,
    service_attestations_imported: u64,
    service_aggregates_published: u64,
    service_aggregates_imported: u64,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
struct LocalnetReport {
    base_dir: String,
    configured_validators: usize,
    validators_with_metrics: usize,
    total_blocks_proposed: u64,
    total_missed_slots: u64,
    total_gating_rejections: u64,
    total_duplicate_receipts_ignored: u64,
    total_peer_rate_limit_drops: u64,
    total_inbound_session_drops: u64,
    total_service_attestations_emitted: u64,
    total_service_attestations_imported: u64,
    total_service_aggregates_published: u64,
    total_service_aggregates_imported: u64,
    lowest_score: Option<(u64, f64)>,
    highest_score: Option<(u64, f64)>,
    validators: Vec<ValidatorReport>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
struct StructuralNodeReport {
    validator_id: u64,
    blocks: usize,
    parent_ok: bool,
    tip_matches_snapshot: bool,
    stderr_clean: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
struct StructuralReport {
    same_chain_count: usize,
    canonical_height: usize,
    min_height: usize,
    max_height: usize,
    height_spread: usize,
    distinct_tip_count: usize,
    tip_spread: usize,
    total_fork_observed: u64,
    total_sync_blocks_rejected: u64,
    all_parent_ok: bool,
    all_tips_match: bool,
    all_stderr_clean: bool,
    nodes: Vec<StructuralNodeReport>,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
struct MatrixScenarioResult {
    name: String,
    base_dir: String,
    load_scenario: LoadScenario,
    load_duration_secs: u64,
    abuse_pattern: Option<String>,
    settle_secs: u64,
    gating_enabled: bool,
    gating_threshold: f64,
    score_window_epochs: u64,
    score_weights: ServiceScoreWeights,
    policy_target_validator: Option<u64>,
    expected_lowest_validator: Option<u64>,
    min_lowest_score: Option<f64>,
    max_lowest_score: Option<f64>,
    min_validator_gating_rejections: Option<(u64, u64)>,
    max_non_target_below_threshold_count: Option<usize>,
    max_non_target_gating_rejections: Option<u64>,
    min_total_peer_rate_limit_drops: Option<u64>,
    min_total_inbound_session_drops: Option<u64>,
    non_target_below_threshold_count: usize,
    non_target_gating_rejections: u64,
    localnet_report: LocalnetReport,
    structural_report: StructuralReport,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
struct MatrixReport {
    generated_at_unix_millis: u64,
    base_dir: String,
    output_dir: String,
    scenarios: Vec<MatrixScenarioResult>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
struct BenchmarkTarget {
    name: String,
    variant: String,
    mode: String,
    validators: usize,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
struct BranchComparisonCase {
    name: String,
    variant: String,
    mode: String,
    validators: usize,
    same_chain_count: usize,
    configured_validators: usize,
    distinct_tip_count: usize,
    height_spread: usize,
    target_validator: Option<u64>,
    target_score: Option<f64>,
    target_gating_rejections: u64,
    honest_min_score: Option<f64>,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
struct BranchComparisonReport {
    cases: Vec<BranchComparisonCase>,
    benchmark_cases: Vec<BranchComparisonCase>,
}

fn v1_benchmark_targets() -> Vec<BenchmarkTarget> {
    vec![
        BenchmarkTarget {
            name: "v1-degraded-4".into(),
            variant: "v1".into(),
            mode: "degraded".into(),
            validators: 4,
        },
        BenchmarkTarget {
            name: "v1-degraded-5".into(),
            variant: "v1".into(),
            mode: "degraded".into(),
            validators: 5,
        },
    ]
}

fn build_branch_comparison_report(cases: Vec<BranchComparisonCase>) -> BranchComparisonReport {
    let benchmark_targets = v1_benchmark_targets();
    let benchmark_cases = cases
        .iter()
        .filter(|case| {
            benchmark_targets.iter().any(|target| {
                target.name == case.name
                    && target.variant == case.variant
                    && target.mode == case.mode
                    && target.validators == case.validators
            })
        })
        .cloned()
        .collect();

    BranchComparisonReport {
        cases,
        benchmark_cases,
    }
}

impl LocalnetReport {
    fn render_text(&self) -> String {
        let mut lines = vec![
            format!("Localnet report: {}", self.base_dir),
            format!(
                "validators with metrics: {}/{}",
                self.validators_with_metrics, self.configured_validators
            ),
            format!("total blocks proposed: {}", self.total_blocks_proposed),
            format!("total missed proposer slots: {}", self.total_missed_slots),
            format!("total gating rejections: {}", self.total_gating_rejections),
            format!(
                "total duplicate receipts ignored: {}",
                self.total_duplicate_receipts_ignored
            ),
            format!(
                "total peer rate-limit drops: {}",
                self.total_peer_rate_limit_drops
            ),
            format!(
                "total inbound session drops: {}",
                self.total_inbound_session_drops
            ),
            format!(
                "total service attestations emitted: {}",
                self.total_service_attestations_emitted
            ),
            format!(
                "total service attestations imported: {}",
                self.total_service_attestations_imported
            ),
            format!(
                "total service aggregates published: {}",
                self.total_service_aggregates_published
            ),
            format!(
                "total service aggregates imported: {}",
                self.total_service_aggregates_imported
            ),
        ];
        if let Some((validator_id, score)) = self.highest_score {
            lines.push(format!(
                "highest score: validator {validator_id} = {score:.3}"
            ));
        }
        if let Some((validator_id, score)) = self.lowest_score {
            lines.push(format!(
                "lowest score: validator {validator_id} = {score:.3}"
            ));
        }
        for validator in &self.validators {
            lines.push(format!(
                "validator {}: epoch {} slot {} score {:.3} weights [{:.2},{:.2},{:.2},-{:.2}] failed_sessions {} invalid_receipts {} proposed {} validated {} txs {} receipts {} attestations_emitted {} attestations_imported {} aggregates_published {} aggregates_imported {} missed {} gated {} duplicate_receipts {} peer_rate_limited {} inbound_session_drops {}",
                validator.validator_id,
                validator.current_epoch,
                validator.current_slot,
                validator.last_local_service_score,
                validator.service_score_weights.uptime_weight,
                validator.service_score_weights.delivery_weight,
                validator.service_score_weights.diversity_weight,
                validator.service_score_weights.penalty_weight,
                validator.failed_sessions,
                validator.invalid_receipts,
                validator.blocks_proposed,
                validator.blocks_validated,
                validator.tx_ingress,
                validator.receipts_created,
                validator.service_attestations_emitted,
                validator.service_attestations_imported,
                validator.service_aggregates_published,
                validator.service_aggregates_imported,
                validator.missed_proposer_slots,
                validator.service_gating_rejections,
                validator.duplicate_receipts_ignored,
                validator.peer_rate_limit_drops,
                validator.inbound_session_drops
            ));
        }
        lines.join("\n")
    }
}

impl MatrixScenarioResult {
    fn passed(&self) -> bool {
        self.structural_report.same_chain_count == self.localnet_report.configured_validators
            && self.structural_report.all_parent_ok
            && self.structural_report.all_tips_match
            && self.structural_report.all_stderr_clean
            && self.expected_lowest_validator.is_none_or(|validator_id| {
                self.localnet_report.lowest_score.map(|(actual, _)| actual) == Some(validator_id)
            })
            && self.min_lowest_score.is_none_or(|min_score| {
                self.localnet_report
                    .lowest_score
                    .map(|(_, score)| score >= min_score)
                    .unwrap_or(false)
            })
            && self.max_lowest_score.is_none_or(|max_score| {
                self.localnet_report
                    .lowest_score
                    .map(|(_, score)| score <= max_score)
                    .unwrap_or(false)
            })
            && self
                .min_validator_gating_rejections
                .is_none_or(|(validator_id, minimum)| {
                    self.localnet_report
                        .validators
                        .iter()
                        .find(|validator| validator.validator_id == validator_id)
                        .map(|validator| validator.service_gating_rejections >= minimum)
                        .unwrap_or(false)
                })
            && self
                .min_total_peer_rate_limit_drops
                .is_none_or(|minimum| self.localnet_report.total_peer_rate_limit_drops >= minimum)
            && self
                .min_total_inbound_session_drops
                .is_none_or(|minimum| self.localnet_report.total_inbound_session_drops >= minimum)
            && self
                .max_non_target_below_threshold_count
                .is_none_or(|maximum| self.non_target_below_threshold_count <= maximum)
            && self
                .max_non_target_gating_rejections
                .is_none_or(|maximum| self.non_target_gating_rejections <= maximum)
    }

    fn failure_signal(&self) -> Option<&'static str> {
        if self.passed() {
            return None;
        }

        let structural_fork_signal = self.structural_report.same_chain_count
            != self.localnet_report.configured_validators
            || self.structural_report.height_spread > 0
            || self.structural_report.tip_spread > 0
            || self.structural_report.total_fork_observed > 0
            || self.structural_report.total_sync_blocks_rejected > 0;

        if structural_fork_signal {
            Some("sync/fork")
        } else {
            Some("policy/gating")
        }
    }
}

impl MatrixReport {
    fn render_text(&self) -> String {
        let passed = self
            .scenarios
            .iter()
            .filter(|scenario| scenario.passed())
            .count();
        let mut lines = vec![
            "Rigorous Entangrid localnet matrix".to_string(),
            format!("scenarios passed: {passed}/{}", self.scenarios.len()),
        ];
        for scenario in &self.scenarios {
            let signal = scenario.failure_signal().unwrap_or("ok");
            lines.push(format!(
                "{}: {} signal {} same_chain {}/{} height_spread {} tip_spread {} fork_observed {} sync_blocks_rejected {} parent_ok {} tips_ok {} stderr_clean {} lowest_score {} gating_rejections {} honest_below_threshold {} honest_gating_rejections {} weights [{:.2},{:.2},{:.2},-{:.2}] rate_limit_drops {} inbound_drops {}",
                scenario.name,
                if scenario.passed() { "PASS" } else { "FAIL" },
                signal,
                scenario.structural_report.same_chain_count,
                scenario.localnet_report.configured_validators,
                scenario.structural_report.height_spread,
                scenario.structural_report.tip_spread,
                scenario.structural_report.total_fork_observed,
                scenario.structural_report.total_sync_blocks_rejected,
                scenario.structural_report.all_parent_ok,
                scenario.structural_report.all_tips_match,
                scenario.structural_report.all_stderr_clean,
                scenario
                    .localnet_report
                    .lowest_score
                    .map(|(validator_id, score)| format!("v{validator_id}={score:.3}"))
                    .unwrap_or_else(|| "n/a".into()),
                scenario.localnet_report.total_gating_rejections,
                scenario.non_target_below_threshold_count,
                scenario.non_target_gating_rejections,
                scenario.score_weights.uptime_weight,
                scenario.score_weights.delivery_weight,
                scenario.score_weights.diversity_weight,
                scenario.score_weights.penalty_weight,
                scenario.localnet_report.total_peer_rate_limit_drops,
                scenario.localnet_report.total_inbound_session_drops
            ));
        }
        lines.join("\n")
    }

    fn render_markdown(&self) -> String {
        let passed = self
            .scenarios
            .iter()
            .filter(|scenario| scenario.passed())
            .count();
        let mut lines = vec![
            "# Entangrid Rigorous Matrix".to_string(),
            String::new(),
            format!(
                "Generated at unix millis `{}`. Scenarios passed: `{}/{}`.",
                self.generated_at_unix_millis,
                passed,
                self.scenarios.len()
            ),
            String::new(),
            "| Scenario | Status | Signal | Validators | Same Chain | Height Spread | Tip Spread | Parent Hashes | Snapshots | Clean stderr | Lowest Score | Fork Observed | Sync Blocks Rejected | Gating Rejections | Honest Below Threshold | Honest Gating Rejections |".to_string(),
            "|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|".to_string(),
        ];

        for scenario in &self.scenarios {
            let lowest_score = scenario
                .localnet_report
                .lowest_score
                .map(|(validator_id, score)| format!("v{validator_id}={score:.3}"))
                .unwrap_or_else(|| "n/a".into());
            let signal = scenario.failure_signal().unwrap_or("ok");
            lines.push(format!(
                "| {} | {} | {} | {} | {}/{} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} |",
                scenario.name,
                if scenario.passed() { "PASS" } else { "FAIL" },
                signal,
                scenario.localnet_report.configured_validators,
                scenario.structural_report.same_chain_count,
                scenario.structural_report.height_spread,
                scenario.structural_report.tip_spread,
                scenario.localnet_report.configured_validators,
                scenario.structural_report.all_parent_ok,
                scenario.structural_report.all_tips_match,
                scenario.structural_report.all_stderr_clean,
                lowest_score,
                scenario.structural_report.total_fork_observed,
                scenario.structural_report.total_sync_blocks_rejected,
                scenario.localnet_report.total_gating_rejections,
                scenario.non_target_below_threshold_count,
                scenario.non_target_gating_rejections
            ));
        }

        for scenario in &self.scenarios {
            lines.push(String::new());
            lines.push(format!("## {}", scenario.name));
            lines.push(String::new());
            lines.push(format!(
                "- Load: `{:?}` for {}s, settle {}s",
                scenario.load_scenario, scenario.load_duration_secs, scenario.settle_secs
            ));
            if let Some(abuse_pattern) = &scenario.abuse_pattern {
                lines.push(format!("- Abuse: `{abuse_pattern}`"));
            }
            lines.push(format!(
                "- Gating: `{}` threshold `{:.3}` score window `{}` epochs weights `[{:.2}, {:.2}, {:.2}, -{:.2}]`",
                scenario.gating_enabled,
                scenario.gating_threshold,
                scenario.score_window_epochs,
                scenario.score_weights.uptime_weight,
                scenario.score_weights.delivery_weight,
                scenario.score_weights.diversity_weight,
                scenario.score_weights.penalty_weight
            ));
            if let Some(target_validator) = scenario.policy_target_validator {
                lines.push(format!(
                    "- Policy target validator: `v{}`; non-target validators below threshold `{}`; non-target gating rejections `{}`",
                    target_validator,
                    scenario.non_target_below_threshold_count,
                    scenario.non_target_gating_rejections
                ));
            }
            lines.push(format!(
                "- Structural: same chain `{}/{}`, height spread `{}` (range `{}..{}`), tip spread `{}` ({} unique tips), fork-observed `{}`, sync-blocks-rejected `{}`, parent hashes `{}`, snapshots `{}`, clean stderr `{}`",
                scenario.structural_report.same_chain_count,
                scenario.localnet_report.configured_validators,
                scenario.structural_report.height_spread,
                scenario.structural_report.min_height,
                scenario.structural_report.max_height,
                scenario.structural_report.tip_spread,
                scenario.structural_report.distinct_tip_count,
                scenario.structural_report.total_fork_observed,
                scenario.structural_report.total_sync_blocks_rejected,
                scenario.structural_report.all_parent_ok,
                scenario.structural_report.all_tips_match,
                scenario.structural_report.all_stderr_clean
            ));
            lines.push(format!(
                "- Abuse counters: peer rate-limit drops `{}`, inbound session drops `{}`",
                scenario.localnet_report.total_peer_rate_limit_drops,
                scenario.localnet_report.total_inbound_session_drops
            ));
            lines.push(String::new());
            lines.push("| Validator | Score | Penalty Weight | Failed Sessions | Invalid Receipts | Proposed | Validated | Missed | Gated | Receipts | Rate-Limited | Inbound Drops |".to_string());
            lines.push("|---|---|---|---|---|---|---|---|---|---|---|---|".to_string());
            for validator in &scenario.localnet_report.validators {
                lines.push(format!(
                    "| {} | {:.3} | {:.2} | {} | {} | {} | {} | {} | {} | {} | {} | {} |",
                    validator.validator_id,
                    validator.last_local_service_score,
                    validator.service_score_weights.penalty_weight,
                    validator.failed_sessions,
                    validator.invalid_receipts,
                    validator.blocks_proposed,
                    validator.blocks_validated,
                    validator.missed_proposer_slots,
                    validator.service_gating_rejections,
                    validator.receipts_created,
                    validator.peer_rate_limit_drops,
                    validator.inbound_session_drops
                ));
            }
        }

        lines.join("\n")
    }
}

fn build_localnet_report(base_dir: &Path, manifest: &LocalnetManifest) -> Result<LocalnetReport> {
    let mut validators = Vec::new();
    let mut total_blocks_proposed = 0;
    let mut total_missed_slots = 0;
    let mut total_gating_rejections = 0;
    let mut total_duplicate_receipts_ignored = 0;
    let mut total_peer_rate_limit_drops = 0;
    let mut total_inbound_session_drops = 0;
    let mut total_service_attestations_emitted = 0;
    let mut total_service_attestations_imported = 0;
    let mut total_service_aggregates_published = 0;
    let mut total_service_aggregates_imported = 0;

    for config_path in &manifest.node_configs {
        let contents = fs::read_to_string(config_path)?;
        let config: NodeConfig = toml::from_str(&contents)?;
        let metrics_path = PathBuf::from(&config.metrics_path);
        if !metrics_path.exists() {
            continue;
        }
        let metrics: NodeMetrics = serde_json::from_str(&fs::read_to_string(metrics_path)?)?;
        total_blocks_proposed += metrics.blocks_proposed;
        total_missed_slots += metrics.missed_proposer_slots;
        total_gating_rejections += metrics.service_gating_rejections;
        total_duplicate_receipts_ignored += metrics.duplicate_receipts_ignored;
        total_peer_rate_limit_drops += metrics.peer_rate_limit_drops;
        total_inbound_session_drops += metrics.inbound_session_drops;
        total_service_attestations_emitted += metrics.service_attestations_emitted;
        total_service_attestations_imported += metrics.service_attestations_imported;
        total_service_aggregates_published += metrics.service_aggregates_published;
        total_service_aggregates_imported += metrics.service_aggregates_imported;
        validators.push(ValidatorReport {
            validator_id: metrics.validator_id,
            current_epoch: metrics.current_epoch,
            current_slot: metrics.current_slot,
            last_local_service_score: metrics.last_local_service_score,
            service_score_weights: metrics.service_score_weights.clone(),
            failed_sessions: metrics.last_local_service_counters.failed_sessions,
            invalid_receipts: metrics.last_local_service_counters.invalid_receipts,
            service_gating_rejections: metrics.service_gating_rejections,
            missed_proposer_slots: metrics.missed_proposer_slots,
            duplicate_receipts_ignored: metrics.duplicate_receipts_ignored,
            peer_rate_limit_drops: metrics.peer_rate_limit_drops,
            inbound_session_drops: metrics.inbound_session_drops,
            blocks_proposed: metrics.blocks_proposed,
            blocks_validated: metrics.blocks_validated,
            tx_ingress: metrics.tx_ingress,
            receipts_created: metrics.receipts_created,
            service_attestations_emitted: metrics.service_attestations_emitted,
            service_attestations_imported: metrics.service_attestations_imported,
            service_aggregates_published: metrics.service_aggregates_published,
            service_aggregates_imported: metrics.service_aggregates_imported,
        });
    }

    validators.sort_by_key(|validator| validator.validator_id);
    let lowest_score = validators
        .iter()
        .map(|validator| (validator.validator_id, validator.last_local_service_score))
        .min_by(|left, right| left.1.total_cmp(&right.1));
    let highest_score = validators
        .iter()
        .map(|validator| (validator.validator_id, validator.last_local_service_score))
        .max_by(|left, right| left.1.total_cmp(&right.1));

    Ok(LocalnetReport {
        base_dir: base_dir.to_string_lossy().to_string(),
        configured_validators: manifest.node_configs.len(),
        validators_with_metrics: validators.len(),
        total_blocks_proposed,
        total_missed_slots,
        total_gating_rejections,
        total_duplicate_receipts_ignored,
        total_peer_rate_limit_drops,
        total_inbound_session_drops,
        total_service_attestations_emitted,
        total_service_attestations_imported,
        total_service_aggregates_published,
        total_service_aggregates_imported,
        lowest_score,
        highest_score,
        validators,
    })
}

fn build_structural_report(manifest: &LocalnetManifest) -> Result<StructuralReport> {
    let mut nodes = Vec::new();
    let mut chains: Vec<Vec<[u8; 32]>> = Vec::new();
    let mut heights: Vec<usize> = Vec::new();
    let mut tip_hashes: BTreeSet<[u8; 32]> = BTreeSet::new();
    let mut total_fork_observed = 0;
    let mut total_sync_blocks_rejected = 0;

    for config_path in &manifest.node_configs {
        let contents = fs::read_to_string(config_path)?;
        let config: NodeConfig = toml::from_str(&contents)?;
        let node_dir = PathBuf::from(&config.data_dir);
        let blocks: Vec<entangrid_types::Block> = read_json_lines(&node_dir.join("blocks.jsonl"))?;
        let snapshot = read_state_snapshot(&node_dir.join("state_snapshot.json"))?;
        let events: Vec<entangrid_types::EventLogEntry> =
            read_json_lines(&node_dir.join("events.log"))?;

        let mut parent_ok = true;
        let mut previous_hash = empty_hash();
        for (index, block) in blocks.iter().enumerate() {
            if block.header.block_number != (index as u64 + 1)
                || block.header.parent_hash != previous_hash
            {
                parent_ok = false;
                break;
            }
            previous_hash = block.block_hash;
        }

        let last_hash = blocks
            .last()
            .map(|block| block.block_hash)
            .unwrap_or_else(empty_hash);
        heights.push(blocks.len());
        tip_hashes.insert(last_hash);
        total_fork_observed += events
            .iter()
            .filter(|entry| entry.event == "fork-observed")
            .count() as u64;
        total_sync_blocks_rejected += events
            .iter()
            .filter(|entry| entry.event == "sync-blocks-rejected")
            .count() as u64;
        let tip_matches_snapshot = snapshot
            .as_ref()
            .map(|snapshot| {
                snapshot.tip_hash == last_hash && snapshot.height == blocks.len() as u64
            })
            .unwrap_or(false);
        let stderr_path = node_dir.join("stderr.log");
        let stderr_clean = if stderr_path.exists() {
            fs::read_to_string(stderr_path)?.trim().is_empty()
        } else {
            true
        };

        chains.push(blocks.iter().map(|block| block.block_hash).collect());
        nodes.push(StructuralNodeReport {
            validator_id: config.validator_id,
            blocks: blocks.len(),
            parent_ok,
            tip_matches_snapshot,
            stderr_clean,
        });
    }

    nodes.sort_by_key(|node| node.validator_id);
    let canonical_chain = chains
        .iter()
        .max_by_key(|chain| chain.len())
        .cloned()
        .unwrap_or_default();
    let same_chain_count = chains
        .iter()
        .filter(|chain| **chain == canonical_chain)
        .count();
    let min_height = heights.iter().copied().min().unwrap_or(0);
    let max_height = heights.iter().copied().max().unwrap_or(0);
    let height_spread = max_height.saturating_sub(min_height);
    let distinct_tip_count = tip_hashes.len();
    let tip_spread = distinct_tip_count.saturating_sub(1);

    Ok(StructuralReport {
        same_chain_count,
        canonical_height: canonical_chain.len(),
        min_height,
        max_height,
        height_spread,
        distinct_tip_count,
        tip_spread,
        total_fork_observed,
        total_sync_blocks_rejected,
        all_parent_ok: nodes.iter().all(|node| node.parent_ok),
        all_tips_match: nodes.iter().all(|node| node.tip_matches_snapshot),
        all_stderr_clean: nodes.iter().all(|node| node.stderr_clean),
        nodes,
    })
}

fn read_state_snapshot(path: &Path) -> Result<Option<entangrid_types::StateSnapshot>> {
    if !path.exists() {
        return Ok(None);
    }
    Ok(Some(serde_json::from_str(&fs::read_to_string(path)?)?))
}

fn read_json_lines<T>(path: &Path) -> Result<Vec<T>>
where
    T: serde::de::DeserializeOwned,
{
    let mut values = Vec::new();
    if !path.exists() {
        return Ok(values);
    }
    let contents = fs::read_to_string(path)?;
    let lines: Vec<&str> = contents
        .lines()
        .filter(|line| !line.trim().is_empty())
        .collect();
    for (index, line) in lines.iter().enumerate() {
        match serde_json::from_str(line) {
            Ok(value) => values.push(value),
            Err(error)
                if index + 1 == lines.len()
                    && error.classify() == serde_json::error::Category::Eof =>
            {
                info!(
                    "ignoring truncated trailing JSONL entry while reading {}",
                    path.display()
                );
                break;
            }
            Err(error) => return Err(error.into()),
        }
    }
    Ok(values)
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

async fn ensure_node_binary_built() -> Result<()> {
    let status = Command::new("cargo")
        .arg("build")
        .arg("--bin")
        .arg("entangrid-node")
        .current_dir(workspace_root()?)
        .status()
        .await?;
    if !status.success() {
        return Err(anyhow!("failed to build entangrid-node"));
    }
    Ok(())
}

fn workspace_root() -> Result<PathBuf> {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .map(Path::to_path_buf)
        .ok_or_else(|| anyhow!("failed to resolve workspace root"))
}

fn refresh_genesis_time_for_fresh_localnet(manifest: &LocalnetManifest) -> Result<()> {
    if !localnet_is_fresh(manifest)? {
        return Ok(());
    }

    let genesis_path = PathBuf::from(&manifest.genesis_path);
    let contents = fs::read_to_string(&genesis_path)?;
    let mut genesis: GenesisConfig = toml::from_str(&contents)?;
    let now = now_unix_millis();
    if now < genesis.genesis_time_unix_millis {
        return Ok(());
    }

    genesis.genesis_time_unix_millis = now + genesis.slot_duration_millis.max(1_000);
    fs::write(&genesis_path, toml::to_string_pretty(&genesis)?)?;
    Ok(())
}

fn localnet_is_fresh(manifest: &LocalnetManifest) -> Result<bool> {
    for config_path in &manifest.node_configs {
        let config_contents = fs::read_to_string(config_path)?;
        let config: NodeConfig = toml::from_str(&config_contents)?;
        let node_dir = PathBuf::from(&config.data_dir);
        for file_name in ["blocks.jsonl", "receipts.jsonl", "state_snapshot.json"] {
            let path = node_dir.join(file_name);
            if path.exists() && fs::metadata(path)?.len() > 0 {
                return Ok(false);
            }
        }
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn block_chain(
        validator_id: u64,
        chain_name: &str,
        block_count: usize,
    ) -> Vec<entangrid_types::Block> {
        let mut parent_hash = empty_hash();
        let mut blocks = Vec::with_capacity(block_count);
        for block_number in 1..=block_count {
            let block_hash = canonical_hash(&(chain_name, validator_id, block_number as u64));
            let header = entangrid_types::BlockHeader {
                block_number: block_number as u64,
                parent_hash,
                slot: block_number as u64,
                epoch: 1,
                proposer_id: validator_id,
                timestamp_unix_millis: block_number as u64,
                state_root: block_hash,
                transactions_root: block_hash,
                topology_root: block_hash,
            };
            blocks.push(entangrid_types::Block {
                header,
                transactions: Vec::new(),
                commitment: None,
                commitment_receipts: Vec::new(),
                signature: vec![validator_id as u8],
                block_hash,
            });
            parent_hash = block_hash;
        }
        blocks
    }

    fn write_structural_node_artifacts(
        base_dir: &Path,
        validator_id: u64,
        blocks: Vec<entangrid_types::Block>,
        events: &[&str],
    ) {
        let node_dir = base_dir.join(format!("node-{validator_id}"));
        fs::write(
            node_dir.join("blocks.jsonl"),
            blocks
                .iter()
                .map(|block| serde_json::to_string(block).unwrap())
                .collect::<Vec<_>>()
                .join("\n"),
        )
        .unwrap();

        let tip_hash = blocks
            .last()
            .map(|block| block.block_hash)
            .unwrap_or_else(empty_hash);
        let snapshot = entangrid_types::StateSnapshot {
            balances: BTreeMap::new(),
            nonces: BTreeMap::new(),
            tip_hash,
            height: blocks.len() as u64,
            last_slot: blocks.last().map(|block| block.header.slot).unwrap_or(0),
        };
        fs::write(
            node_dir.join("state_snapshot.json"),
            serde_json::to_string(&snapshot).unwrap(),
        )
        .unwrap();

        fs::write(
            node_dir.join("events.log"),
            events
                .iter()
                .enumerate()
                .map(|(index, event)| {
                    serde_json::to_string(&entangrid_types::EventLogEntry {
                        timestamp_unix_millis: 1_000 + index as u64,
                        event: (*event).to_string(),
                        detail: format!("{} detail", event),
                    })
                    .unwrap()
                })
                .collect::<Vec<_>>()
                .join("\n"),
        )
        .unwrap();

        fs::write(node_dir.join("stderr.log"), "").unwrap();
    }

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
            0.40,
            4,
            default_service_score_weights(),
            None,
            0,
            0.0,
            false,
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
            0.35,
            6,
            default_service_score_weights(),
            Some(3),
            0,
            0.75,
            false,
            false,
        )
        .unwrap();
        let node_three_path = unique_dir.join("node-3").join("node.toml");
        let contents = fs::read_to_string(node_three_path).unwrap();
        let node_config: NodeConfig = toml::from_str(&contents).unwrap();
        assert!(node_config.feature_flags.enable_service_gating);
        assert_eq!(node_config.feature_flags.service_gating_start_epoch, 3);
        assert!((node_config.feature_flags.service_gating_threshold - 0.35).abs() < f64::EPSILON);
        assert_eq!(node_config.feature_flags.service_score_window_epochs, 6);
        assert_eq!(
            node_config.feature_flags.service_score_weights,
            default_service_score_weights()
        );
        assert_eq!(node_config.fault_profile.outbound_drop_probability, 0.75);
    }

    #[test]
    fn init_localnet_can_enable_consensus_v2() {
        let unique_dir = std::env::temp_dir().join(format!(
            "entangrid-sim-consensus-v2-test-{}",
            now_unix_millis()
        ));
        init_localnet(
            4,
            &unique_dir,
            1_000,
            5,
            1_000,
            true,
            3,
            0.40,
            4,
            default_service_score_weights(),
            None,
            0,
            0.0,
            false,
            true,
        )
        .unwrap();
        let node_one_path = unique_dir.join("node-1").join("node.toml");
        let contents = fs::read_to_string(node_one_path).unwrap();
        let node_config: NodeConfig = toml::from_str(&contents).unwrap();
        assert!(node_config.feature_flags.consensus_v2);
    }

    #[test]
    fn report_summarizes_existing_metrics() {
        let unique_dir =
            std::env::temp_dir().join(format!("entangrid-sim-report-test-{}", now_unix_millis()));
        init_localnet(
            4,
            &unique_dir,
            1_000,
            5,
            1_000,
            true,
            3,
            0.40,
            4,
            default_service_score_weights(),
            None,
            0,
            0.0,
            false,
            false,
        )
        .unwrap();
        let manifest = read_manifest(&unique_dir).unwrap();
        let first_config: NodeConfig =
            toml::from_str(&fs::read_to_string(&manifest.node_configs[0]).unwrap()).unwrap();
        let second_config: NodeConfig =
            toml::from_str(&fs::read_to_string(&manifest.node_configs[1]).unwrap()).unwrap();

        fs::write(
            &first_config.metrics_path,
            serde_json::to_vec_pretty(&NodeMetrics {
                validator_id: 1,
                current_epoch: 4,
                current_slot: 19,
                last_local_service_score: 0.85,
                last_local_service_counters: entangrid_types::ServiceCounters {
                    failed_sessions: 1,
                    ..Default::default()
                },
                blocks_proposed: 3,
                blocks_validated: 7,
                tx_ingress: 11,
                receipts_created: 9,
                service_attestations_emitted: 5,
                service_attestations_imported: 8,
                service_aggregates_published: 2,
                service_aggregates_imported: 4,
                missed_proposer_slots: 0,
                service_gating_rejections: 0,
                duplicate_receipts_ignored: 1,
                ..NodeMetrics::default()
            })
            .unwrap(),
        )
        .unwrap();
        fs::write(
            &second_config.metrics_path,
            serde_json::to_vec_pretty(&NodeMetrics {
                validator_id: 2,
                current_epoch: 4,
                current_slot: 19,
                last_local_service_score: 0.25,
                last_local_service_counters: entangrid_types::ServiceCounters {
                    invalid_receipts: 2,
                    ..Default::default()
                },
                blocks_proposed: 1,
                blocks_validated: 6,
                tx_ingress: 8,
                receipts_created: 7,
                service_attestations_emitted: 3,
                service_attestations_imported: 6,
                service_aggregates_published: 1,
                service_aggregates_imported: 3,
                missed_proposer_slots: 2,
                service_gating_rejections: 2,
                duplicate_receipts_ignored: 3,
                ..NodeMetrics::default()
            })
            .unwrap(),
        )
        .unwrap();

        let report = build_localnet_report(&unique_dir, &manifest).unwrap();
        assert_eq!(report.validators_with_metrics, 2);
        assert_eq!(report.total_blocks_proposed, 4);
        assert_eq!(report.total_missed_slots, 2);
        assert_eq!(report.total_gating_rejections, 2);
        assert_eq!(report.total_duplicate_receipts_ignored, 4);
        assert_eq!(report.total_service_attestations_emitted, 8);
        assert_eq!(report.total_service_attestations_imported, 14);
        assert_eq!(report.total_service_aggregates_published, 3);
        assert_eq!(report.total_service_aggregates_imported, 7);
        assert_eq!(report.highest_score, Some((1, 0.85)));
        assert_eq!(report.lowest_score, Some((2, 0.25)));
        assert_eq!(report.validators[0].failed_sessions, 1);
        assert_eq!(report.validators[1].invalid_receipts, 2);
        let rendered = report.render_text();
        assert!(rendered.contains("total service aggregates imported: 7"));
        assert!(
            rendered.contains(
                "attestations_emitted 5 attestations_imported 8 aggregates_published 2 aggregates_imported 4"
            )
        );
    }

    #[test]
    fn refreshes_fresh_localnet_genesis_time_before_start() {
        let unique_dir = std::env::temp_dir().join(format!(
            "entangrid-sim-genesis-refresh-test-{}",
            now_unix_millis()
        ));
        init_localnet(
            4,
            &unique_dir,
            1_000,
            5,
            1,
            true,
            3,
            0.40,
            4,
            default_service_score_weights(),
            None,
            0,
            0.0,
            false,
            false,
        )
        .unwrap();

        std::thread::sleep(std::time::Duration::from_millis(5));
        let manifest = read_manifest(&unique_dir).unwrap();
        let before: GenesisConfig =
            toml::from_str(&fs::read_to_string(&manifest.genesis_path).unwrap()).unwrap();
        refresh_genesis_time_for_fresh_localnet(&manifest).unwrap();
        let after: GenesisConfig =
            toml::from_str(&fs::read_to_string(&manifest.genesis_path).unwrap()).unwrap();

        assert!(localnet_is_fresh(&manifest).unwrap());
        assert!(after.genesis_time_unix_millis > before.genesis_time_unix_millis);
    }

    #[test]
    fn rigorous_matrix_scenarios_cover_expected_cases() {
        let scenarios = rigorous_matrix_scenarios(12);
        let names: Vec<_> = scenarios.iter().map(|scenario| scenario.name).collect();
        assert_eq!(
            names,
            vec![
                "baseline-4-steady",
                "baseline-6-bursty",
                "gated-6-bursty",
                "gated-drop85",
                "gated-drop95",
                "gated-outbound-disabled",
                "policy-threshold-025",
                "policy-threshold-055",
                "policy-window-1",
                "policy-window-8",
                "policy-penalty-050",
                "policy-penalty-150",
                "abuse-sync-control-flood",
                "abuse-inbound-connection-flood",
            ]
        );
        assert_eq!(
            scenarios
                .iter()
                .find(|scenario| scenario.name == "baseline-4-steady")
                .unwrap()
                .service_score_window_epochs,
            4
        );
        assert_eq!(
            scenarios
                .iter()
                .find(|scenario| scenario.name == "baseline-6-bursty")
                .unwrap()
                .service_score_window_epochs,
            6
        );
        assert_eq!(
            scenarios
                .iter()
                .find(|scenario| scenario.name == "gated-6-bursty")
                .unwrap()
                .service_score_window_epochs,
            6
        );
    }

    #[test]
    fn recommended_score_window_scales_with_validator_count() {
        assert_eq!(recommended_service_score_window_epochs_for_validators(4), 4);
        assert_eq!(recommended_service_score_window_epochs_for_validators(6), 6);
        assert_eq!(recommended_service_score_window_epochs_for_validators(8), 8);
        assert_eq!(
            recommended_service_score_window_epochs_for_validators(12),
            12
        );
    }

    #[test]
    fn consensus_v2_localnet_scales_observer_surface_with_validator_count() {
        let unique_dir = std::env::temp_dir().join(format!(
            "entangrid-sim-v2-witnesses-test-{}",
            now_unix_millis()
        ));
        init_localnet(
            6,
            &unique_dir,
            1_000,
            5,
            1_000,
            true,
            3,
            0.35,
            6,
            default_service_score_weights(),
            None,
            0,
            0.0,
            false,
            true,
        )
        .unwrap();
        let genesis: GenesisConfig =
            toml::from_str(&fs::read_to_string(unique_dir.join("genesis.toml")).unwrap()).unwrap();
        assert_eq!(genesis.witness_count, 4);
    }

    #[test]
    fn matrix_report_renders_pass_fail_summary() {
        let report = MatrixReport {
            generated_at_unix_millis: 1,
            base_dir: "var/localnet-matrix".into(),
            output_dir: "test-results".into(),
            scenarios: vec![MatrixScenarioResult {
                name: "demo".into(),
                base_dir: "var/localnet-matrix/demo".into(),
                load_scenario: LoadScenario::Steady,
                load_duration_secs: 12,
                abuse_pattern: None,
                settle_secs: 10,
                gating_enabled: true,
                gating_threshold: 0.40,
                score_window_epochs: 4,
                score_weights: default_service_score_weights(),
                policy_target_validator: Some(4),
                expected_lowest_validator: Some(4),
                min_lowest_score: None,
                max_lowest_score: Some(0.30),
                min_validator_gating_rejections: Some((4, 1)),
                max_non_target_below_threshold_count: Some(0),
                max_non_target_gating_rejections: Some(0),
                min_total_peer_rate_limit_drops: None,
                min_total_inbound_session_drops: None,
                non_target_below_threshold_count: 0,
                non_target_gating_rejections: 0,
                localnet_report: LocalnetReport {
                    base_dir: "var/localnet-matrix/demo".into(),
                    configured_validators: 4,
                    validators_with_metrics: 4,
                    total_blocks_proposed: 8,
                    total_missed_slots: 2,
                    total_gating_rejections: 2,
                    total_duplicate_receipts_ignored: 0,
                    total_peer_rate_limit_drops: 0,
                    total_inbound_session_drops: 0,
                    total_service_attestations_emitted: 0,
                    total_service_attestations_imported: 0,
                    total_service_aggregates_published: 0,
                    total_service_aggregates_imported: 0,
                    lowest_score: Some((4, 0.25)),
                    highest_score: Some((1, 1.0)),
                    validators: vec![ValidatorReport {
                        validator_id: 4,
                        current_epoch: 4,
                        current_slot: 19,
                        last_local_service_score: 0.25,
                        service_score_weights: default_service_score_weights(),
                        failed_sessions: 1,
                        invalid_receipts: 2,
                        service_gating_rejections: 2,
                        missed_proposer_slots: 2,
                        duplicate_receipts_ignored: 0,
                        peer_rate_limit_drops: 0,
                        inbound_session_drops: 0,
                        blocks_proposed: 1,
                        blocks_validated: 6,
                        tx_ingress: 8,
                        receipts_created: 7,
                        service_attestations_emitted: 0,
                        service_attestations_imported: 0,
                        service_aggregates_published: 0,
                        service_aggregates_imported: 0,
                    }],
                },
                structural_report: StructuralReport {
                    same_chain_count: 4,
                    canonical_height: 20,
                    min_height: 20,
                    max_height: 20,
                    height_spread: 0,
                    distinct_tip_count: 1,
                    tip_spread: 0,
                    total_fork_observed: 0,
                    total_sync_blocks_rejected: 0,
                    all_parent_ok: true,
                    all_tips_match: true,
                    all_stderr_clean: true,
                    nodes: vec![],
                },
            }],
        };

        let rendered = report.render_text();
        assert!(rendered.contains("scenarios passed: 1/1"));
        assert!(rendered.contains("demo: PASS"));
        let markdown = report.render_markdown();
        assert!(markdown.contains("# Entangrid Rigorous Matrix"));
        assert!(markdown.contains("| Scenario | Status | Signal | Validators |"));
    }

    #[test]
    fn comparison_report_marks_v1_degraded_4_as_benchmark_case() {
        let report = build_branch_comparison_report(vec![
            BranchComparisonCase {
                name: "v1-degraded-4".into(),
                variant: "v1".into(),
                mode: "degraded".into(),
                validators: 4,
                same_chain_count: 4,
                configured_validators: 4,
                distinct_tip_count: 1,
                height_spread: 0,
                target_validator: Some(3),
                target_score: Some(0.083),
                target_gating_rejections: 9,
                honest_min_score: Some(0.925),
            },
            BranchComparisonCase {
                name: "v2-degraded-4".into(),
                variant: "v2".into(),
                mode: "degraded".into(),
                validators: 4,
                same_chain_count: 4,
                configured_validators: 4,
                distinct_tip_count: 1,
                height_spread: 0,
                target_validator: Some(3),
                target_score: Some(0.375),
                target_gating_rejections: 9,
                honest_min_score: Some(1.0),
            },
        ]);

        assert_eq!(report.benchmark_cases.len(), 1);
        assert!(
            report
                .benchmark_cases
                .iter()
                .any(|case| case.name == "v1-degraded-4")
        );
    }

    #[test]
    fn matrix_result_fails_when_expectations_are_not_met() {
        let scenario = MatrixScenarioResult {
            name: "demo".into(),
            base_dir: "var/localnet-matrix/demo".into(),
            load_scenario: LoadScenario::Steady,
            load_duration_secs: 12,
            abuse_pattern: None,
            settle_secs: 10,
            gating_enabled: true,
            gating_threshold: 0.40,
            score_window_epochs: 4,
            score_weights: default_service_score_weights(),
            policy_target_validator: Some(4),
            expected_lowest_validator: Some(4),
            min_lowest_score: None,
            max_lowest_score: Some(0.30),
            min_validator_gating_rejections: Some((4, 1)),
            max_non_target_below_threshold_count: Some(0),
            max_non_target_gating_rejections: Some(0),
            min_total_peer_rate_limit_drops: None,
            min_total_inbound_session_drops: None,
            non_target_below_threshold_count: 0,
            non_target_gating_rejections: 0,
            localnet_report: LocalnetReport {
                base_dir: "var/localnet-matrix/demo".into(),
                configured_validators: 4,
                validators_with_metrics: 4,
                total_blocks_proposed: 8,
                total_missed_slots: 0,
                total_gating_rejections: 0,
                total_duplicate_receipts_ignored: 0,
                total_peer_rate_limit_drops: 0,
                total_inbound_session_drops: 0,
                total_service_attestations_emitted: 0,
                total_service_attestations_imported: 0,
                total_service_aggregates_published: 0,
                total_service_aggregates_imported: 0,
                lowest_score: Some((4, 0.35)),
                highest_score: Some((1, 1.0)),
                validators: vec![ValidatorReport {
                    validator_id: 4,
                    current_epoch: 4,
                    current_slot: 19,
                    last_local_service_score: 0.35,
                    service_score_weights: default_service_score_weights(),
                    failed_sessions: 0,
                    invalid_receipts: 0,
                    service_gating_rejections: 0,
                    missed_proposer_slots: 0,
                    duplicate_receipts_ignored: 0,
                    peer_rate_limit_drops: 0,
                    inbound_session_drops: 0,
                    blocks_proposed: 1,
                    blocks_validated: 6,
                    tx_ingress: 8,
                    receipts_created: 7,
                    service_attestations_emitted: 0,
                    service_attestations_imported: 0,
                    service_aggregates_published: 0,
                    service_aggregates_imported: 0,
                }],
            },
            structural_report: StructuralReport {
                same_chain_count: 4,
                canonical_height: 20,
                min_height: 20,
                max_height: 20,
                height_spread: 0,
                distinct_tip_count: 1,
                tip_spread: 0,
                total_fork_observed: 0,
                total_sync_blocks_rejected: 0,
                all_parent_ok: true,
                all_tips_match: true,
                all_stderr_clean: true,
                nodes: vec![],
            },
        };

        assert!(!scenario.passed());
    }

    #[test]
    fn structural_report_and_matrix_summary_surface_sync_fork_signals() {
        let unique_dir = std::env::temp_dir().join(format!(
            "entangrid-sim-structural-signal-test-{}",
            now_unix_millis()
        ));
        init_localnet(
            4,
            &unique_dir,
            1_000,
            5,
            1_000,
            false,
            3,
            0.40,
            4,
            default_service_score_weights(),
            None,
            0,
            0.0,
            false,
            false,
        )
        .unwrap();

        write_structural_node_artifacts(
            &unique_dir,
            1,
            block_chain(1, "alpha", 2),
            &["fork-observed"],
        );
        write_structural_node_artifacts(
            &unique_dir,
            2,
            block_chain(2, "beta", 4),
            &["fork-observed", "fork-observed", "sync-blocks-rejected"],
        );
        write_structural_node_artifacts(
            &unique_dir,
            3,
            block_chain(3, "gamma", 3),
            &["sync-blocks-rejected", "sync-blocks-rejected"],
        );
        write_structural_node_artifacts(&unique_dir, 4, block_chain(4, "delta", 1), &[]);

        let manifest = read_manifest(&unique_dir).unwrap();
        let structural_report = build_structural_report(&manifest).unwrap();
        assert_eq!(structural_report.same_chain_count, 1);
        assert_eq!(structural_report.canonical_height, 4);
        assert_eq!(structural_report.min_height, 1);
        assert_eq!(structural_report.max_height, 4);
        assert_eq!(structural_report.height_spread, 3);
        assert_eq!(structural_report.distinct_tip_count, 4);
        assert_eq!(structural_report.tip_spread, 3);
        assert_eq!(structural_report.total_fork_observed, 3);
        assert_eq!(structural_report.total_sync_blocks_rejected, 3);
        assert!(structural_report.all_tips_match);

        let scenario = MatrixScenarioResult {
            name: "sync-fork-demo".into(),
            base_dir: unique_dir.to_string_lossy().to_string(),
            load_scenario: LoadScenario::Bursty,
            load_duration_secs: 18,
            abuse_pattern: None,
            settle_secs: 12,
            gating_enabled: false,
            gating_threshold: 0.40,
            score_window_epochs: 4,
            score_weights: default_service_score_weights(),
            policy_target_validator: None,
            expected_lowest_validator: None,
            min_lowest_score: Some(0.45),
            max_lowest_score: None,
            min_validator_gating_rejections: None,
            max_non_target_below_threshold_count: None,
            max_non_target_gating_rejections: None,
            min_total_peer_rate_limit_drops: None,
            min_total_inbound_session_drops: None,
            non_target_below_threshold_count: 0,
            non_target_gating_rejections: 0,
            localnet_report: LocalnetReport {
                base_dir: unique_dir.to_string_lossy().to_string(),
                configured_validators: 4,
                validators_with_metrics: 4,
                total_blocks_proposed: 0,
                total_missed_slots: 0,
                total_gating_rejections: 0,
                total_duplicate_receipts_ignored: 0,
                total_peer_rate_limit_drops: 0,
                total_inbound_session_drops: 0,
                total_service_attestations_emitted: 0,
                total_service_attestations_imported: 0,
                total_service_aggregates_published: 0,
                total_service_aggregates_imported: 0,
                lowest_score: Some((1, 0.90)),
                highest_score: Some((2, 1.0)),
                validators: vec![],
            },
            structural_report: structural_report.clone(),
        };

        assert_eq!(scenario.failure_signal(), Some("sync/fork"));
        let report = MatrixReport {
            generated_at_unix_millis: 1,
            base_dir: unique_dir.to_string_lossy().to_string(),
            output_dir: unique_dir.to_string_lossy().to_string(),
            scenarios: vec![scenario],
        };
        let rendered = report.render_text();
        assert!(rendered.contains("signal sync/fork"));
        assert!(rendered.contains("height_spread 3"));
        assert!(rendered.contains("tip_spread 3"));
        assert!(rendered.contains("fork_observed 3"));
        assert!(rendered.contains("sync_blocks_rejected 3"));
        let markdown = report.render_markdown();
        assert!(markdown.contains("| Scenario | Status | Signal |"));
        assert!(markdown.contains("| sync-fork-demo | FAIL | sync/fork |"));
    }

    #[test]
    fn read_json_lines_ignores_truncated_trailing_entry() {
        let unique_dir =
            std::env::temp_dir().join(format!("entangrid-sim-jsonl-test-{}", now_unix_millis()));
        fs::create_dir_all(&unique_dir).unwrap();
        let path = unique_dir.join("structural.jsonl");
        let valid = serde_json::json!({
            "validator_id": 1,
            "blocks": 3,
            "parent_ok": true,
            "tip_matches_snapshot": true,
            "stderr_clean": true
        });
        fs::write(
            &path,
            format!(
                "{}\n{}",
                serde_json::to_string(&valid).unwrap(),
                "{\"validator_id\":2,\"blocks\":3,\"parent_ok\":true,\"tip_matches_snapshot\":true,\"stderr_clean\":tr"
            ),
        )
        .unwrap();

        let reports = read_json_lines::<StructuralNodeReport>(&path).unwrap();
        assert_eq!(reports.len(), 1);
        assert_eq!(reports[0].validator_id, 1);
    }
}
