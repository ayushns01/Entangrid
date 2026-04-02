use std::{
    cmp::Ordering,
    collections::{BTreeMap, BTreeSet},
    fs,
    io::Write,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::Duration,
};

use anyhow::{Context, Result, anyhow};
use clap::{Parser, Subcommand};
use entangrid_consensus::ConsensusEngine;
use entangrid_crypto::{CryptoBackend, build_crypto_backend};
use entangrid_ledger::LedgerState;
use entangrid_network::{NetworkEvent, NetworkFailureKind, NetworkHandle, spawn_network};
use entangrid_types::{
    Block, BlockHeader, CertifiedBlockHeader, ChainSegment, ChainSnapshot, ChunkedSyncRequest,
    ChunkedSyncResponse, Epoch, EventLogEntry, GenesisConfig, HashBytes, HeartbeatPulse,
    MessageClass, NodeConfig, NodeMetrics, PeerConfig, ProposalVote, ProtocolMessage,
    PublicKeyScheme, QuorumCertificate, RelayReceipt, ServiceAggregate, ServiceAttestation,
    ServiceCounters, SignedTransaction, StateSnapshot, SyncQcAnchor, TopologyCommitment,
    TypedSignature, ValidatorId, canonical_hash, empty_hash, now_unix_millis,
};
use tokio::{sync::mpsc, time::MissedTickBehavior};
use tracing::info;

const SNAPSHOT_FILE: &str = "state_snapshot.json";
const BLOCKS_FILE: &str = "blocks.jsonl";
const RECEIPTS_FILE: &str = "receipts.jsonl";
const SERVICE_ATTESTATIONS_FILE: &str = "service_attestations.jsonl";
const SERVICE_AGGREGATES_FILE: &str = "service_aggregates.jsonl";
const ORPHANS_FILE: &str = "orphans.jsonl";
const MAX_INCREMENTAL_SYNC_BLOCKS: usize = 64;
const MAX_PREFERRED_INCREMENTAL_SYNC_BLOCKS: usize = 12;
const SYNC_REQUEST_COOLDOWN_MILLIS: u64 = 1_500;
const INCREMENTAL_SYNC_FAILURES_BEFORE_FULL_SNAPSHOT: u64 = 1;
const PEER_MESSAGE_WINDOW_MILLIS: u64 = 5_000;
const MAX_SYNC_CONTROL_MESSAGES_PER_WINDOW: u64 = 24;
const MAX_TRANSACTION_GOSSIP_MESSAGES_PER_WINDOW: u64 = 256;
const MAX_RECEIPT_GOSSIP_MESSAGES_PER_WINDOW: u64 = 512;

#[derive(Parser)]
#[command(name = "entangrid-node")]
#[command(about = "Run an Entangrid validator node")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    Run {
        #[arg(long)]
        config: PathBuf,
    },
}

pub async fn cli_main() -> Result<()> {
    init_tracing();
    let cli = Cli::parse();
    match cli.command {
        Commands::Run { config } => run_node_from_path(&config).await,
    }
}

pub async fn run_node_from_path(config_path: &Path) -> Result<()> {
    let config_contents = fs::read_to_string(config_path)
        .with_context(|| format!("failed to read config {}", config_path.display()))?;
    let config: NodeConfig = toml::from_str(&config_contents)
        .with_context(|| format!("failed to parse config {}", config_path.display()))?;
    let genesis_path = PathBuf::from(&config.genesis_path);
    let genesis_contents = fs::read_to_string(&genesis_path)
        .with_context(|| format!("failed to read genesis {}", genesis_path.display()))?;
    let genesis: GenesisConfig = toml::from_str(&genesis_contents)
        .with_context(|| format!("failed to parse genesis {}", genesis_path.display()))?;
    run_node(config, genesis).await
}

pub async fn run_node(config: NodeConfig, genesis: GenesisConfig) -> Result<()> {
    validate_hybrid_enforcement_genesis(&config, &genesis)?;
    let crypto = build_crypto_backend(&genesis, &config)?;
    let consensus = ConsensusEngine::new(genesis.clone());
    let storage = Storage::new(&config)?;
    storage.init()?;
    let loaded_blocks = storage.load_blocks()?;
    let loaded_receipts = storage.load_receipts()?;
    let loaded_service_attestations = storage.load_service_attestations()?;
    let loaded_service_aggregates = storage.load_service_aggregates()?;
    let snapshot = storage.load_snapshot()?;
    let restored_last_slot = snapshot.as_ref().map(|snapshot| snapshot.last_slot);
    let ledger = match snapshot {
        Some(snapshot) => LedgerState::from_snapshot(snapshot),
        None => LedgerState::replay_blocks(&genesis, &loaded_blocks, crypto.as_ref())
            .unwrap_or_else(|_| LedgerState::from_genesis(&genesis)),
    };
    let initial_last_processed_slot =
        initial_last_processed_slot(&consensus, restored_last_slot, now_unix_millis());
    let startup_sync_barrier = config.sync_on_startup
        && !config.peers.is_empty()
        && ledger.block_height() > 0
        && now_unix_millis() >= genesis.genesis_time_unix_millis;

    let metrics = Arc::new(Mutex::new(NodeMetrics {
        validator_id: config.validator_id,
        ..NodeMetrics::default()
    }));
    let (network_event_tx, network_event_rx) = mpsc::unbounded_channel();
    let network = spawn_network(
        config.validator_id,
        config.listen_address.clone(),
        config.peers.clone(),
        config.fault_profile.clone(),
        Arc::clone(&crypto),
        Arc::clone(&metrics),
        network_event_tx,
    )
    .await?;

    let mut runner = NodeRunner {
        config,
        genesis,
        consensus,
        crypto,
        storage,
        network,
        network_event_rx,
        metrics,
        ledger,
        blocks: loaded_blocks,
        receipts: loaded_receipts,
        orphan_blocks: Vec::new(),
        pending_certified_children: Vec::new(),
        mempool: BTreeMap::new(),
        seen_transactions: BTreeSet::new(),
        seen_blocks: BTreeSet::new(),
        seen_receipts: BTreeSet::new(),
        seen_receipt_events: BTreeSet::new(),
        service_attestations: BTreeMap::new(),
        service_aggregates: BTreeMap::new(),
        proposal_votes: BTreeMap::new(),
        buffered_proposal_votes: BTreeMap::new(),
        quorum_certificates: BTreeMap::new(),
        seen_service_attestations: BTreeSet::new(),
        seen_service_aggregates: BTreeSet::new(),
        last_processed_slot: initial_last_processed_slot,
        last_logged_epoch: None,
        last_heartbeat_slot: None,
        last_proposed_slot: None,
        startup_sync_barrier,
        latest_service_scores: BTreeMap::new(),
        latest_service_counters: BTreeMap::new(),
        failed_sessions: 0,
        invalid_receipts: 0,
        observed_failed_sessions: BTreeSet::new(),
        observed_successful_sessions: BTreeSet::new(),
        observed_invalid_receipts: BTreeMap::new(),
        known_live_peers: BTreeSet::new(),
        last_sync_request_served: BTreeMap::new(),
        last_sync_push_served: BTreeMap::new(),
        last_recovery_sync_request: None,
        last_recovery_sync_status: None,
        peer_sync_status: BTreeMap::new(),
        peer_sync_repair_failures: BTreeMap::new(),
        peer_message_windows: BTreeMap::new(),
    };
    runner.restore_service_evidence(loaded_service_attestations, loaded_service_aggregates)?;
    runner.rebuild_seen_sets();
    runner.run().await
}

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
                "hybrid enforcement requires validator {} to advertise a hybrid public identity",
                validator.validator_id
            ));
        }
    }

    Ok(())
}

struct NodeRunner {
    config: NodeConfig,
    genesis: GenesisConfig,
    consensus: ConsensusEngine,
    crypto: Arc<dyn CryptoBackend>,
    storage: Storage,
    network: NetworkHandle,
    network_event_rx: mpsc::UnboundedReceiver<NetworkEvent>,
    metrics: Arc<Mutex<NodeMetrics>>,
    ledger: LedgerState,
    blocks: Vec<Block>,
    receipts: Vec<RelayReceipt>,
    orphan_blocks: Vec<Block>,
    pending_certified_children: Vec<Block>,
    mempool: BTreeMap<HashBytes, SignedTransaction>,
    seen_transactions: BTreeSet<HashBytes>,
    seen_blocks: BTreeSet<HashBytes>,
    seen_receipts: BTreeSet<HashBytes>,
    seen_receipt_events: BTreeSet<HashBytes>,
    service_attestations: BTreeMap<(ValidatorId, Epoch), BTreeMap<ValidatorId, ServiceAttestation>>,
    service_aggregates: BTreeMap<(ValidatorId, Epoch), ServiceAggregate>,
    proposal_votes: BTreeMap<HashBytes, BTreeMap<ValidatorId, ProposalVote>>,
    buffered_proposal_votes: BTreeMap<HashBytes, BTreeMap<ValidatorId, ProposalVote>>,
    quorum_certificates: BTreeMap<HashBytes, QuorumCertificate>,
    seen_service_attestations: BTreeSet<HashBytes>,
    seen_service_aggregates: BTreeSet<HashBytes>,
    last_processed_slot: Option<u64>,
    last_logged_epoch: Option<Epoch>,
    last_heartbeat_slot: Option<u64>,
    last_proposed_slot: Option<u64>,
    startup_sync_barrier: bool,
    latest_service_scores: BTreeMap<ValidatorId, f64>,
    latest_service_counters: BTreeMap<ValidatorId, ServiceCounters>,
    failed_sessions: u64,
    invalid_receipts: u64,
    observed_failed_sessions: BTreeSet<(Epoch, ValidatorId, ValidatorId)>,
    observed_successful_sessions: BTreeSet<(Epoch, ValidatorId, ValidatorId)>,
    observed_invalid_receipts: BTreeMap<(Epoch, ValidatorId), u64>,
    known_live_peers: BTreeSet<ValidatorId>,
    last_sync_request_served: BTreeMap<ValidatorId, ServedSyncRequest>,
    last_sync_push_served: BTreeMap<ValidatorId, ServedSyncPush>,
    last_recovery_sync_request: Option<RecoverySyncRequestMemo>,
    last_recovery_sync_status: Option<RecoverySyncStatusMemo>,
    peer_sync_status: BTreeMap<ValidatorId, PeerSyncStatus>,
    peer_sync_repair_failures: BTreeMap<ValidatorId, u64>,
    peer_message_windows: BTreeMap<ValidatorId, PeerMessageWindow>,
}

#[derive(Clone, Copy, Debug)]
struct ServedSyncRequest {
    served_at_unix_millis: u64,
    known_height: u64,
    known_tip_hash: HashBytes,
    served_local_height: u64,
    served_local_tip_hash: HashBytes,
    served_local_highest_qc_height: u64,
    served_local_highest_qc_hash: Option<HashBytes>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ServedSyncPush {
    peer_height: u64,
    peer_tip_hash: HashBytes,
    peer_highest_qc_hash: Option<HashBytes>,
    peer_highest_qc_height: u64,
    local_height: u64,
    local_tip_hash: HashBytes,
    local_highest_qc_hash: Option<HashBytes>,
    local_highest_qc_height: u64,
}

#[derive(Clone, Debug)]
struct PeerSyncStatus {
    height: u64,
    tip_hash: HashBytes,
    highest_qc_hash: Option<HashBytes>,
    highest_qc_height: u64,
    recent_qc_anchors: Vec<SyncQcAnchor>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct LocalSyncView {
    height: u64,
    tip_hash: HashBytes,
    highest_qc_hash: Option<HashBytes>,
    highest_qc_height: u64,
}

const CERTIFIED_SYNC_ANCHOR_LIMIT: usize = 32;
const RECOVERY_SYNC_RETRY_SLOTS: u64 = 2;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum BlockAcceptance {
    Accepted,
    Duplicate,
    Orphan,
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum V2GatingState {
    AllowNoEvidence,
    AllowInsufficientEvidence,
    AllowScore(f64),
    RejectScore(f64),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct RecoverySyncRequestMemo {
    slot: u64,
    peer_validator_id: ValidatorId,
    local_view: LocalSyncView,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct RecoverySyncStatusMemo {
    slot: u64,
    local_view: LocalSyncView,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum V2SyncMode {
    Certified,
    Legacy,
}

#[derive(Clone, Debug, PartialEq)]
enum V2ServiceEvidenceState {
    RecentConfirmed {
        latest_epoch: Epoch,
        counters: ServiceCounters,
        score: f64,
    },
    StaleConfirmed,
    NoEvidence,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PeerMessageClass {
    SyncControl,
    TransactionGossip,
    ReceiptGossip,
}

#[derive(Clone, Debug, Default)]
struct PeerMessageWindow {
    started_at_unix_millis: u64,
    sync_control_messages: u64,
    transaction_gossip_messages: u64,
    receipt_gossip_messages: u64,
}

impl NodeRunner {
    async fn run(&mut self) -> Result<()> {
        info!("starting validator {}", self.config.validator_id);
        self.log_event(
            "node-started",
            format!(
                "validator {} listening on {}",
                self.config.validator_id, self.config.listen_address
            ),
        )?;

        if self.config.sync_on_startup {
            self.run_sync_maintenance()?;
        }

        let mut slot_tick = tokio::time::interval(Duration::from_millis(250));
        slot_tick.set_missed_tick_behavior(MissedTickBehavior::Delay);
        let mut inbox_tick = tokio::time::interval(Duration::from_millis(500));
        inbox_tick.set_missed_tick_behavior(MissedTickBehavior::Delay);
        let mut metrics_tick = tokio::time::interval(Duration::from_secs(2));
        let mut sync_tick = tokio::time::interval(Duration::from_secs(5));
        let mut shutdown = std::pin::pin!(shutdown_signal());

        loop {
            tokio::select! {
                _ = &mut shutdown => {
                    self.handle_shutdown()?;
                    return Ok(());
                }
                _ = slot_tick.tick() => {
                    self.process_slot_tick().await?;
                }
                _ = inbox_tick.tick() => {
                    self.process_inbox().await?;
                }
                _ = metrics_tick.tick() => {
                    self.refresh_service_scores();
                    self.storage.write_metrics(&self.snapshot_metrics())?;
                }
                _ = sync_tick.tick() => {
                    if self.config.sync_on_startup {
                        self.run_sync_maintenance()?;
                    }
                }
                maybe_event = self.network_event_rx.recv() => {
                    match maybe_event {
                        Some(event) => self.handle_network_event(event).await?,
                        None => return Err(anyhow!("network event stream closed")),
                    }
                }
            }
        }
    }

    fn handle_shutdown(&mut self) -> Result<()> {
        self.refresh_service_scores();
        self.storage.write_snapshot(self.ledger.snapshot())?;
        self.persist_metrics()?;
        self.log_event(
            "node-stopped",
            format!(
                "validator {} shutting down cleanly",
                self.config.validator_id
            ),
        )?;
        Ok(())
    }

    async fn process_slot_tick(&mut self) -> Result<()> {
        let now = now_unix_millis();
        if now < self.genesis.genesis_time_unix_millis {
            return Ok(());
        }
        let current_slot = self.consensus.slot_at(now);
        if self.last_processed_slot == Some(current_slot) {
            return Ok(());
        }
        for slot in pending_slots_to_process(self.last_processed_slot, current_slot) {
            let is_live_slot = slot == current_slot;
            self.last_processed_slot = Some(slot);
            let epoch = self.consensus.epoch_for_slot(slot);
            self.refresh_service_scores();
            self.update_metrics(|metrics| {
                metrics.current_slot = slot;
                metrics.current_epoch = epoch;
            });

            if self.last_logged_epoch != Some(epoch) {
                self.last_logged_epoch = Some(epoch);
                if is_live_slot {
                    let assignment = self
                        .consensus
                        .assignment_for(epoch, self.config.validator_id)
                        .unwrap_or_else(|| entangrid_types::EpochAssignment {
                            epoch,
                            validator_id: self.config.validator_id,
                            witnesses: Vec::new(),
                            relay_targets: Vec::new(),
                        });
                    self.log_event(
                        "epoch-transition",
                        format!(
                            "epoch {epoch} witnesses {:?} relay_targets {:?}",
                            assignment.witnesses, assignment.relay_targets
                        ),
                    )?;
                    self.log_current_service_score(epoch)?;
                    if epoch > 0 {
                        self.request_receipt_reconciliation(epoch - 1)?;
                    }
                    if epoch > 2 {
                        self.refresh_recent_service_attestations(epoch)?;
                    }
                }
            }

            if is_live_slot {
                self.broadcast_heartbeat(slot, epoch)?;
                if self.config.sync_on_startup
                    && self
                        .config
                        .peers
                        .iter()
                        .any(|peer| self.peer_status_is_ahead(peer.validator_id))
                {
                    self.maybe_broadcast_recovery_sync_status(slot)?;
                    self.maybe_request_recovery_sync_from_ahead_peer(slot)?;
                }
                if self.config.feature_flags.consensus_v2 {
                    self.maybe_repair_stale_pending_certified_child(slot)?;
                }
                self.maybe_propose_block(slot, epoch).await?;
                self.try_promote_orphans().await?;
            }
        }
        self.persist_metrics()?;
        Ok(())
    }

    fn broadcast_heartbeat(&mut self, slot: u64, epoch: Epoch) -> Result<()> {
        if self.last_heartbeat_slot == Some(slot) {
            return Ok(());
        }
        self.last_heartbeat_slot = Some(slot);
        let pulse = HeartbeatPulse {
            epoch,
            slot,
            source_validator_id: self.config.validator_id,
            sequence_number: slot,
            emitted_at_unix_millis: now_unix_millis(),
        };
        self.network
            .broadcast(&self.config.peers, ProtocolMessage::HeartbeatPulse(pulse))?;
        Ok(())
    }

    async fn maybe_propose_block(&mut self, slot: u64, epoch: Epoch) -> Result<()> {
        if self.config.fault_profile.pause_slot_production {
            return Ok(());
        }
        if self.last_proposed_slot == Some(slot) {
            return Ok(());
        }
        let proposer = self.consensus.proposer_for_slot(slot);
        self.log_event(
            "proposer-decision",
            format!("slot {slot} proposer {proposer}"),
        )?;
        if proposer != self.config.validator_id {
            return Ok(());
        }
        if self.startup_sync_blocks_proposal() {
            self.log_event(
                "startup-sync-wait",
                format!(
                    "slot {slot} proposer {} waiting for startup sync local_height {} peer_statuses {}/{}",
                    self.config.validator_id,
                    self.ledger.block_height(),
                    self.peer_sync_status.len(),
                    self.config.peers.len()
                ),
            )?;
            return Ok(());
        }

        if self.config.feature_flags.enable_service_gating {
            if !self.service_gating_active(epoch) {
                self.log_event(
                    "service-gating-warmup",
                    format!(
                        "slot {slot} skipping service gating until epoch {} (current epoch {epoch})",
                        self.config.feature_flags.service_gating_start_epoch
                    ),
                )?;
            } else {
                let counters = self.local_service_counters();
                if self.config.feature_flags.consensus_v2 {
                    match self.v2_gating_state(self.config.validator_id, epoch) {
                        V2GatingState::AllowNoEvidence => {
                            self.update_metrics(|metrics| {
                                metrics.service_gating_enforcement_skips += 1;
                                metrics.last_local_service_score =
                                    self.service_score_for_validator(self.config.validator_id);
                                metrics.service_gating_threshold = self.service_gating_threshold();
                                metrics.last_local_service_counters = counters.clone();
                            });
                            self.log_event(
                                "service-gating-skip",
                                format!(
                                    "slot {slot} no confirmed aggregate for validator {} prior epoch {}",
                                    self.config.validator_id,
                                    epoch.saturating_sub(1)
                                ),
                            )?;
                        }
                        V2GatingState::AllowInsufficientEvidence => {
                            self.update_metrics(|metrics| {
                                metrics.service_gating_enforcement_skips += 1;
                                metrics.last_local_service_score =
                                    self.service_score_for_validator(self.config.validator_id);
                                metrics.service_gating_threshold = self.service_gating_threshold();
                                metrics.last_local_service_counters = counters.clone();
                            });
                            self.log_event(
                                "service-gating-skip",
                                format!(
                                    "slot {slot} confirmed aggregate missing for validator {} prior epoch {} despite older evidence",
                                    self.config.validator_id,
                                    epoch.saturating_sub(1)
                                ),
                            )?;
                        }
                        V2GatingState::AllowScore(score) => {
                            self.log_event(
                                "service-gating-check",
                                format!(
                                    "slot {slot} confirmed_score {score:.3} threshold {:.3} window {} weights [{:.2},{:.2},{:.2},-{:.2}] uptime {}/{} timely {}/{} peers {}/{} failed_sessions {} invalid_receipts {}",
                                    self.service_gating_threshold(),
                                    self.config.feature_flags.service_score_window_epochs,
                                    self.config.feature_flags.service_score_weights.uptime_weight,
                                    self.config.feature_flags.service_score_weights.delivery_weight,
                                    self.config.feature_flags.service_score_weights.diversity_weight,
                                    self.config.feature_flags.service_score_weights.penalty_weight,
                                    counters.uptime_windows,
                                    counters.total_windows,
                                    counters.timely_deliveries,
                                    counters.expected_deliveries,
                                    counters.distinct_peers,
                                    counters.expected_peers,
                                    counters.failed_sessions,
                                    counters.invalid_receipts
                                ),
                            )?;
                        }
                        V2GatingState::RejectScore(score) => {
                            self.update_metrics(|metrics| {
                                metrics.missed_proposer_slots += 1;
                                metrics.service_gating_rejections += 1;
                                metrics.last_local_service_score = score;
                                metrics.service_gating_threshold = self.service_gating_threshold();
                                metrics.last_local_service_counters = counters.clone();
                            });
                            self.log_event(
                                "service-gating-reject",
                                format!(
                                    "slot {slot} rejected due to confirmed service score {score:.3} below threshold {:.3}",
                                    self.service_gating_threshold(),
                                ),
                            )?;
                            self.log_event(
                                "missed-slot",
                                format!(
                                    "slot {slot} missed due to service score {score:.3} below threshold {:.3} with window {} weights [{:.2},{:.2},{:.2},-{:.2}] uptime {}/{} timely {}/{} peers {}/{} failed_sessions {} invalid_receipts {}",
                                    self.service_gating_threshold(),
                                    self.config.feature_flags.service_score_window_epochs,
                                    self.config.feature_flags.service_score_weights.uptime_weight,
                                    self.config.feature_flags.service_score_weights.delivery_weight,
                                    self.config.feature_flags.service_score_weights.diversity_weight,
                                    self.config.feature_flags.service_score_weights.penalty_weight,
                                    counters.uptime_windows,
                                    counters.total_windows,
                                    counters.timely_deliveries,
                                    counters.expected_deliveries,
                                    counters.distinct_peers,
                                    counters.expected_peers,
                                    counters.failed_sessions,
                                    counters.invalid_receipts
                                ),
                            )?;
                            self.persist_metrics()?;
                            return Ok(());
                        }
                    }
                } else {
                    let score = self.service_score_for_validator(self.config.validator_id);
                    self.log_event(
                        "service-gating-check",
                        format!(
                            "slot {slot} local_score {score:.3} threshold {:.3} window {} weights [{:.2},{:.2},{:.2},-{:.2}] uptime {}/{} timely {}/{} peers {}/{} failed_sessions {} invalid_receipts {}",
                            self.service_gating_threshold(),
                            self.config.feature_flags.service_score_window_epochs,
                            self.config.feature_flags.service_score_weights.uptime_weight,
                            self.config.feature_flags.service_score_weights.delivery_weight,
                            self.config.feature_flags.service_score_weights.diversity_weight,
                            self.config.feature_flags.service_score_weights.penalty_weight,
                            counters.uptime_windows,
                            counters.total_windows,
                            counters.timely_deliveries,
                            counters.expected_deliveries,
                            counters.distinct_peers,
                            counters.expected_peers,
                            counters.failed_sessions,
                            counters.invalid_receipts
                        ),
                    )?;
                    if score < self.service_gating_threshold() {
                        self.update_metrics(|metrics| {
                            metrics.missed_proposer_slots += 1;
                            metrics.service_gating_rejections += 1;
                            metrics.last_local_service_score = score;
                            metrics.service_gating_threshold = self.service_gating_threshold();
                            metrics.last_local_service_counters = counters.clone();
                        });
                        self.log_event(
                            "missed-slot",
                            format!(
                                "slot {slot} missed due to service score {score:.3} below threshold {:.3} with window {} weights [{:.2},{:.2},{:.2},-{:.2}] uptime {}/{} timely {}/{} peers {}/{} failed_sessions {} invalid_receipts {}",
                                self.service_gating_threshold(),
                                self.config.feature_flags.service_score_window_epochs,
                                self.config.feature_flags.service_score_weights.uptime_weight,
                                self.config.feature_flags.service_score_weights.delivery_weight,
                                self.config.feature_flags.service_score_weights.diversity_weight,
                                self.config.feature_flags.service_score_weights.penalty_weight,
                                counters.uptime_windows,
                                counters.total_windows,
                                counters.timely_deliveries,
                                counters.expected_deliveries,
                                counters.distinct_peers,
                                counters.expected_peers,
                                counters.failed_sessions,
                                counters.invalid_receipts
                            ),
                        )?;
                        self.persist_metrics()?;
                        return Ok(());
                    }
                }
            }
        }

        self.last_proposed_slot = Some(slot);
        if let Some(pending_child) = self.preferred_pending_child_for_certified_head() {
            self.maybe_broadcast_local_proposal_vote(&pending_child)?;
            self.network.broadcast(
                &self.config.peers,
                ProtocolMessage::BlockProposal(pending_child.clone()),
            )?;
            self.log_event(
                "pending-block-rebroadcast",
                format!(
                    "height {} slot {} block {:02x?}",
                    pending_child.header.block_number,
                    pending_child.header.slot,
                    &pending_child.block_hash[..4]
                ),
            )?;
            return Ok(());
        }
        let (commitment, commitment_receipts) =
            if self.config.feature_flags.enable_receipts && epoch > 0 {
                let commitment_receipts = self.consensus.receipts_for_validator(
                    self.config.validator_id,
                    epoch - 1,
                    &self.receipts,
                );
                let commitment = self.consensus.commitment_from_receipts_with_weights(
                    self.config.validator_id,
                    epoch - 1,
                    &commitment_receipts,
                    0,
                    0,
                    &self.config.feature_flags.service_score_weights,
                );
                (Some(commitment), commitment_receipts)
            } else {
                (None, Vec::new())
            };

        let transactions = self.select_transactions_for_block();
        let mut simulated_ledger = self.ledger.clone();
        let mut accepted = Vec::new();
        for transaction in transactions {
            if simulated_ledger
                .validate_tx(&transaction, self.crypto.as_ref())
                .is_ok()
                && simulated_ledger.apply_transaction(&transaction).is_ok()
            {
                accepted.push(transaction);
            }
        }

        let topology_root = commitment
            .as_ref()
            .map(canonical_hash)
            .unwrap_or_else(empty_hash);
        let transactions_root = canonical_hash(&accepted);
        let header = BlockHeader {
            block_number: self.ledger.block_height() + 1,
            parent_hash: self.ledger.snapshot().tip_hash,
            slot,
            epoch,
            proposer_id: self.config.validator_id,
            timestamp_unix_millis: now_unix_millis(),
            state_root: simulated_ledger.state_root(),
            transactions_root,
            topology_root,
        };
        let block_hash = canonical_hash(&(header.clone(), &accepted, &commitment));
        let signature = self.crypto.sign(self.config.validator_id, &block_hash)?;
        let block = Block {
            header,
            transactions: accepted,
            commitment,
            commitment_receipts,
            signature,
            block_hash,
        };

        let acceptance = self.accept_block(block.clone(), true).await?;
        if matches!(acceptance, BlockAcceptance::Accepted)
            || (matches!(acceptance, BlockAcceptance::Orphan)
                && self.is_pending_certified_head_block(&block))
        {
            if matches!(acceptance, BlockAcceptance::Accepted) {
                self.try_promote_orphans().await?;
            }
            self.network
                .broadcast(&self.config.peers, ProtocolMessage::BlockProposal(block))?;
        }
        Ok(())
    }

    fn select_transactions_for_block(&self) -> Vec<SignedTransaction> {
        self.mempool
            .values()
            .take(self.genesis.max_txs_per_block)
            .cloned()
            .collect()
    }

    async fn process_inbox(&mut self) -> Result<()> {
        for entry in fs::read_dir(&self.storage.inbox_dir)? {
            let entry = entry?;
            let path = entry.path();
            if path.extension().and_then(|ext| ext.to_str()) != Some("json") {
                continue;
            }
            let contents = fs::read_to_string(&path)?;
            let transaction: SignedTransaction = serde_json::from_str(&contents)
                .with_context(|| format!("failed to parse inbox tx {}", path.display()))?;
            if let Err(error) = self
                .handle_transaction(transaction.clone(), self.config.validator_id, true)
                .await
            {
                self.log_event(
                    "inbox-tx-rejected",
                    format!("path {} detail {}", path.display(), error),
                )?;
            }
            let processed_path = self.storage.processed_dir.join(
                path.file_name()
                    .ok_or_else(|| anyhow!("inbox file missing name"))?,
            );
            fs::rename(path, processed_path)?;
        }
        Ok(())
    }

    async fn handle_network_event(&mut self, event: NetworkEvent) -> Result<()> {
        match event {
            NetworkEvent::Received {
                from_validator_id,
                payload,
                bytes,
            } => {
                self.known_live_peers.insert(from_validator_id);
                self.log_event(
                    "network-recv",
                    format!("from {from_validator_id} bytes {bytes}"),
                )?;
                self.handle_protocol_message(from_validator_id, payload)
                    .await?;
            }
            NetworkEvent::SessionObserved {
                peer_validator_id,
                transcript_hash,
                outbound,
                service_accountable,
            } => {
                if should_track_outbound_service_session(outbound, service_accountable) {
                    self.known_live_peers.insert(peer_validator_id);
                    self.record_successful_session(
                        self.current_epoch(),
                        self.config.validator_id,
                        peer_validator_id,
                    );
                }
                self.log_event(
                    "session-observed",
                    format!(
                        "peer {peer_validator_id} transcript {:02x?}",
                        &transcript_hash[..4]
                    ),
                )?;
            }
            NetworkEvent::SessionFailed {
                peer_validator_id,
                detail,
                outbound,
                service_accountable,
                kind,
            } => {
                self.failed_sessions += 1;
                if should_record_failed_service_session(outbound, service_accountable, kind) {
                    if let Some(peer_validator_id) = peer_validator_id {
                        self.record_failed_session(
                            self.current_epoch(),
                            self.config.validator_id,
                            peer_validator_id,
                        );
                    }
                }
                self.update_metrics(|metrics| {
                    metrics.handshake_failures += 1;
                });
                self.log_event(
                    "disconnect",
                    format!("peer {:?} detail {detail}", peer_validator_id),
                )?;
            }
            NetworkEvent::InboundSessionDropped { detail } => {
                self.log_event("inbound-session-dropped", detail)?;
            }
        }
        Ok(())
    }

    async fn handle_protocol_message(
        &mut self,
        from_validator_id: ValidatorId,
        payload: ProtocolMessage,
    ) -> Result<()> {
        if let Some(message_class) = classify_peer_message(&payload) {
            if !self.allow_peer_message(from_validator_id, message_class, now_unix_millis()) {
                self.update_metrics(|metrics| {
                    metrics.peer_rate_limit_drops += 1;
                });
                self.log_event(
                    "peer-rate-limited",
                    format!(
                        "from {from_validator_id} class {}",
                        peer_message_class_label(message_class)
                    ),
                )?;
                return Ok(());
            }
        }

        match payload {
            ProtocolMessage::TransactionBroadcast(transaction) => {
                if let Err(error) = self
                    .handle_transaction(transaction, from_validator_id, false)
                    .await
                {
                    self.log_event(
                        "peer-tx-rejected",
                        format!("from {from_validator_id} detail {error}"),
                    )?;
                }
            }
            ProtocolMessage::BlockProposal(block) => {
                self.record_peer_sync_status(
                    from_validator_id,
                    block.header.block_number,
                    block.block_hash,
                    None,
                    0,
                    Vec::new(),
                );
                self.maybe_issue_receipt(
                    from_validator_id,
                    self.config.validator_id,
                    MessageClass::Block,
                    block.header.slot,
                    block.block_hash,
                    canonical_hash(&block.header),
                    block.header.block_number,
                )
                .await?;
                match self.accept_block(block.clone(), false).await {
                    Ok(BlockAcceptance::Accepted) => {
                        self.try_promote_orphans().await?;
                        self.network
                            .broadcast(&self.config.peers, ProtocolMessage::BlockProposal(block))?;
                    }
                    Ok(BlockAcceptance::Orphan) => {
                        if self.is_pending_certified_head_block(&block) {
                            if let Err(error) =
                                self.request_pending_certified_child_sync(from_validator_id)
                            {
                                self.log_event(
                                    "sync-request-failed",
                                    format!("to {from_validator_id} detail {error}"),
                                )?;
                            }
                            return Ok(());
                        }
                        let peer_height = block.header.block_number.saturating_sub(1);
                        let peer_tip_hash = block.header.parent_hash;
                        self.record_sync_repair_failure(from_validator_id);
                        if let Err(error) =
                            self.push_best_sync_to(from_validator_id, peer_height, peer_tip_hash)
                        {
                            self.log_event(
                                "sync-push-failed",
                                format!("to {from_validator_id} detail {error}"),
                            )?;
                        }
                        if let Err(error) = self.request_sync_from(from_validator_id) {
                            self.log_event(
                                "sync-request-failed",
                                format!("to {from_validator_id} detail {error}"),
                            )?;
                        }
                    }
                    Ok(BlockAcceptance::Duplicate) => {}
                    Err(error) => {
                        self.log_event(
                            "peer-block-rejected",
                            format!(
                                "from {from_validator_id} height {} slot {} detail {}",
                                block.header.block_number, block.header.slot, error
                            ),
                        )?;
                    }
                }
            }
            ProtocolMessage::ProposalVote(vote) => {
                if self.config.feature_flags.consensus_v2 {
                    if let Err(error) = self.import_proposal_vote(vote.clone()) {
                        self.log_event(
                            "proposal-vote-rejected",
                            format!(
                                "from {from_validator_id} block {:02x?} detail {error}",
                                &vote.block_hash[..4]
                            ),
                        )?;
                    }
                }
            }
            ProtocolMessage::QuorumCertificate(qc) => {
                if self.config.feature_flags.consensus_v2 {
                    if self.known_block(qc.block_hash).is_none() {
                        self.record_peer_sync_status(
                            from_validator_id,
                            qc.block_number,
                            qc.block_hash,
                            Some(qc.block_hash),
                            qc.block_number,
                            vec![SyncQcAnchor {
                                block_hash: qc.block_hash,
                                block_number: qc.block_number,
                            }],
                        );
                        self.log_event(
                            "qc-rejected",
                            format!(
                                "from {from_validator_id} block {:02x?} detail quorum certificate references unknown block",
                                &qc.block_hash[..4]
                            ),
                        )?;
                        if let Err(error) = self.request_sync_from(from_validator_id) {
                            self.log_event(
                                "sync-request-failed",
                                format!("to {from_validator_id} detail {error}"),
                            )?;
                        }
                        return Ok(());
                    }
                    match self.import_quorum_certificate(qc.clone()) {
                        Ok(true) => {
                            self.announce_certified_progress()?;
                        }
                        Ok(false) => {}
                        Err(error) => {
                            self.log_event(
                                "qc-rejected",
                                format!(
                                    "from {from_validator_id} block {:02x?} detail {error}",
                                    &qc.block_hash[..4]
                                ),
                            )?;
                        }
                    }
                }
            }
            ProtocolMessage::SyncStatus {
                validator_id,
                height,
                tip_hash,
                highest_qc_hash,
                highest_qc_height,
                recent_qc_anchors,
            } => {
                self.record_peer_sync_status(
                    validator_id,
                    height,
                    tip_hash,
                    highest_qc_hash,
                    highest_qc_height,
                    recent_qc_anchors,
                );
                let local_sync_view = self.local_sync_view();
                if height > local_sync_view.height {
                    if let Err(error) = self.request_sync_from(validator_id) {
                        self.log_event(
                            "sync-request-failed",
                            format!("to {validator_id} detail {error}"),
                        )?;
                    }
                } else if height < local_sync_view.height || tip_hash != local_sync_view.tip_hash {
                    if let Err(error) = self.push_best_sync_to(validator_id, height, tip_hash) {
                        self.log_event(
                            "sync-push-failed",
                            format!("to {validator_id} detail {error}"),
                        )?;
                    }
                }
            }
            ProtocolMessage::SyncRequest {
                requester_id,
                known_height,
                known_tip_hash,
            } => {
                let force_full_snapshot = is_force_full_snapshot_hint(known_height, known_tip_hash);
                self.record_peer_sync_status(
                    requester_id,
                    known_height,
                    known_tip_hash,
                    None,
                    0,
                    Vec::new(),
                );
                if !force_full_snapshot
                    && self.sync_request_is_throttled(requester_id, known_height, known_tip_hash)
                {
                    self.update_metrics(|metrics| {
                        metrics.sync_requests_throttled += 1;
                    });
                    self.log_event(
                        "sync-request-throttled",
                        format!("from {requester_id} height {known_height}"),
                    )?;
                    return Ok(());
                }
                if !force_full_snapshot {
                    self.record_sync_request_served(requester_id, known_height, known_tip_hash);
                    if self.config.feature_flags.consensus_v2 {
                        self.push_best_sync_to(requester_id, known_height, known_tip_hash)?;
                    } else {
                        self.send_best_sync_to(requester_id, known_height, known_tip_hash)?;
                    }
                } else {
                    self.send_full_snapshot_to(self.find_peer(requester_id)?)?;
                }
            }
            ProtocolMessage::SyncResponse {
                responder_id,
                chain,
            } => {
                self.record_peer_sync_status(
                    responder_id,
                    chain.snapshot.height,
                    chain.snapshot.tip_hash,
                    None,
                    0,
                    Vec::new(),
                );
                let local_snapshot = self.ledger.snapshot().clone();
                if should_adopt_snapshot(&local_snapshot, &chain.snapshot) {
                    match self.apply_chain_snapshot(chain) {
                        Ok(()) => {
                            self.try_promote_orphans().await?;
                            self.clear_sync_repair_memory(responder_id);
                            self.update_metrics(|metrics| {
                                metrics.full_sync_applied += 1;
                            });
                            self.log_event(
                                "sync-applied",
                                format!("adopted chain from validator {responder_id}"),
                            )?;
                            if self.peer_status_is_ahead(responder_id) {
                                self.request_sync_from(responder_id)?;
                            }
                            if let Err(error) = self.maybe_push_tip_progress_to_stale_peers() {
                                self.log_event(
                                    "sync-push-failed",
                                    format!("tip-progress detail {error}"),
                                )?;
                            }
                        }
                        Err(error) => {
                            self.log_event(
                                "sync-rejected",
                                format!("from {responder_id} detail {error}"),
                            )?;
                        }
                    }
                } else if should_adopt_snapshot(&chain.snapshot, &local_snapshot) {
                    if let Err(error) = self.push_best_sync_to(
                        responder_id,
                        chain.snapshot.height,
                        chain.snapshot.tip_hash,
                    ) {
                        self.log_event(
                            "sync-push-failed",
                            format!("to {responder_id} detail {error}"),
                        )?;
                    }
                }
            }
            ProtocolMessage::SyncBlocks {
                responder_id,
                chain,
            } => {
                self.record_peer_sync_status(
                    responder_id,
                    chain.target_snapshot.height,
                    chain.target_snapshot.tip_hash,
                    None,
                    0,
                    Vec::new(),
                );
                let local_snapshot = self.ledger.snapshot().clone();
                if !self.should_adopt_chain_segment(&local_snapshot, &chain) {
                    return Ok(());
                }
                match self.apply_chain_segment(chain) {
                    Ok(()) => {
                        self.try_promote_orphans().await?;
                        self.clear_sync_repair_memory(responder_id);
                        self.update_metrics(|metrics| {
                            metrics.incremental_sync_applied += 1;
                        });
                        self.log_event(
                            "sync-blocks-applied",
                            format!("adopted incremental sync from validator {responder_id}"),
                        )?;
                        if self.peer_status_is_ahead(responder_id) {
                            self.request_sync_from(responder_id)?;
                        }
                        if let Err(error) = self.maybe_push_tip_progress_to_stale_peers() {
                            self.log_event(
                                "sync-push-failed",
                                format!("tip-progress detail {error}"),
                            )?;
                        }
                    }
                    Err(error) => {
                        self.record_sync_repair_failure(responder_id);
                        self.log_event(
                            "sync-blocks-rejected",
                            format!("from {responder_id} detail {error}"),
                        )?;
                        if let Err(request_error) = self.request_sync_from(responder_id) {
                            self.log_event(
                                "sync-request-failed",
                                format!("to {responder_id} detail {request_error}"),
                            )?;
                        }
                    }
                }
            }
            ProtocolMessage::HeartbeatPulse(pulse) => {
                self.maybe_issue_receipt(
                    pulse.source_validator_id,
                    self.config.validator_id,
                    MessageClass::Heartbeat,
                    pulse.slot,
                    canonical_hash(&pulse),
                    canonical_hash(&pulse),
                    pulse.sequence_number,
                )
                .await?;
            }
            ProtocolMessage::CertifiedSyncRequest(request) => {
                if self.config.feature_flags.consensus_v2 {
                    let requester_id = request.requester_id;
                    let peer = self.find_peer(requester_id)?;
                    let peer_qc_anchors = peer_qc_anchors_from_request(&request);
                    let shared_anchor = self.highest_shared_qc_anchor(&peer_qc_anchors);
                    let response = self.build_certified_sync_response(request);
                    if matches!(response, ChunkedSyncResponse::Certified { .. }) {
                        self.update_metrics(|metrics| {
                            metrics.certified_sync_served += 1;
                        });
                    }
                    self.network
                        .send_control_to(peer, ProtocolMessage::CertifiedSyncResponse(response))?;
                    if let Some(shared_anchor) = shared_anchor {
                        self.send_anchored_suffix_sync_to(requester_id, shared_anchor)?;
                    }
                }
            }
            ProtocolMessage::CertifiedSyncResponse(response) => {
                if self.config.feature_flags.consensus_v2 {
                    let unavailable_from = match &response {
                        ChunkedSyncResponse::Unavailable { responder_id } => Some(*responder_id),
                        _ => None,
                    };
                    match self.import_certified_sync_response(response) {
                        Ok(true) => {
                            self.try_promote_orphans().await?;
                            self.announce_certified_progress()?;
                            if self.peer_status_is_ahead(from_validator_id) {
                                self.request_sync_from(from_validator_id)?;
                            }
                        }
                        Ok(false) => {
                            if let Some(responder_id) = unavailable_from {
                                let tried_followup =
                                    self.request_best_available_sync_from_ahead_peers();
                                match tried_followup {
                                    Ok(true) => {}
                                    Ok(false) => {
                                        if let Err(error) =
                                            self.request_legacy_sync_from(responder_id)
                                        {
                                            self.log_event(
                                                "sync-request-failed",
                                                format!("to {responder_id} detail {error}"),
                                            )?;
                                        }
                                    }
                                    Err(error) => {
                                        self.log_event(
                                            "sync-request-failed",
                                            format!("to {responder_id} detail {error}"),
                                        )?;
                                    }
                                }
                            } else if self.peer_status_is_ahead(from_validator_id) {
                                self.request_sync_from(from_validator_id)?;
                            }
                        }
                        Err(error) => {
                            self.log_event(
                                "certified-sync-rejected",
                                format!("from {from_validator_id} detail {error}"),
                            )?;
                            if let Err(request_error) =
                                self.request_legacy_sync_from(from_validator_id)
                            {
                                self.log_event(
                                    "sync-request-failed",
                                    format!("to {from_validator_id} detail {request_error}"),
                                )?;
                            }
                        }
                    }
                }
            }
            ProtocolMessage::RelayReceipt(receipt) => {
                if self.store_receipt(receipt.clone())? {
                    self.network
                        .broadcast(&self.config.peers, ProtocolMessage::RelayReceipt(receipt))?;
                }
            }
            ProtocolMessage::ServiceAttestation(attestation) => {
                if self.config.feature_flags.consensus_v2 {
                    if let Err(error) = self.import_service_attestation(attestation.clone()) {
                        self.log_event(
                            "service-attestation-rejected",
                            format!(
                                "from {from_validator_id} subject {} epoch {} detail {error}",
                                attestation.subject_validator_id, attestation.epoch
                            ),
                        )?;
                    } else {
                        self.update_metrics(|metrics| {
                            metrics.service_attestations_imported += 1;
                        });
                        self.maybe_publish_service_aggregate(
                            attestation.subject_validator_id,
                            attestation.epoch,
                        )?;
                    }
                }
            }
            ProtocolMessage::ServiceAggregate(aggregate) => {
                if self.config.feature_flags.consensus_v2 {
                    if let Err(error) = self.import_service_aggregate(aggregate.clone()) {
                        self.log_event(
                            "service-aggregate-rejected",
                            format!(
                                "from {from_validator_id} subject {} epoch {} detail {error}",
                                aggregate.subject_validator_id, aggregate.epoch
                            ),
                        )?;
                    } else {
                        self.update_metrics(|metrics| {
                            metrics.service_aggregates_imported += 1;
                        });
                        self.refresh_service_scores();
                    }
                }
            }
            ProtocolMessage::ReceiptFetch {
                requester_id,
                epoch,
                validator_id,
            } => {
                let receipts: Vec<_> = self
                    .receipts
                    .iter()
                    .filter(|receipt| {
                        receipt.epoch == epoch && receipt.source_validator_id == validator_id
                    })
                    .cloned()
                    .collect();
                self.network.send_control_to(
                    self.find_peer(requester_id)?,
                    ProtocolMessage::ReceiptResponse {
                        responder_id: self.config.validator_id,
                        epoch,
                        validator_id,
                        receipts,
                    },
                )?;
            }
            ProtocolMessage::ReceiptResponse { receipts, .. } => {
                for receipt in receipts {
                    self.store_receipt(receipt)?;
                }
            }
        }
        Ok(())
    }

    async fn handle_transaction(
        &mut self,
        transaction: SignedTransaction,
        from_validator_id: ValidatorId,
        local_ingress: bool,
    ) -> Result<()> {
        let tx_hash = transaction.tx_hash;
        if self.seen_transactions.contains(&tx_hash) {
            return Ok(());
        }
        self.validate_tx_against_mempool(&transaction)?;
        self.seen_transactions.insert(tx_hash);
        self.mempool.insert(tx_hash, transaction.clone());
        self.update_metrics(|metrics| {
            metrics.tx_ingress += 1;
            metrics.tx_propagated += 1;
        });
        self.log_event(
            "tx-accepted",
            format!(
                "{} -> {} amount {} nonce {}",
                transaction.transaction.from,
                transaction.transaction.to,
                transaction.transaction.amount,
                transaction.transaction.nonce
            ),
        )?;
        self.persist_metrics()?;

        let slot = self
            .last_processed_slot
            .unwrap_or_else(|| self.consensus.current_slot());
        self.maybe_issue_receipt(
            from_validator_id,
            self.config.validator_id,
            MessageClass::Transaction,
            slot,
            transaction.tx_hash,
            transaction.tx_hash,
            transaction.transaction.nonce,
        )
        .await?;

        if local_ingress || from_validator_id != self.config.validator_id {
            self.network.broadcast(
                &self.config.peers,
                ProtocolMessage::TransactionBroadcast(transaction),
            )?;
        }
        Ok(())
    }

    fn validate_tx_against_mempool(&self, transaction: &SignedTransaction) -> Result<()> {
        let mut state = self.ledger.clone();
        let mut pending: Vec<_> = self
            .mempool
            .values()
            .filter(|pending| pending.transaction.from == transaction.transaction.from)
            .cloned()
            .collect();
        pending.sort_by_key(|pending| pending.transaction.nonce);
        for pending_tx in pending {
            if state.validate_tx(&pending_tx, self.crypto.as_ref()).is_ok() {
                let _ = state.apply_transaction(&pending_tx);
            }
        }
        state.validate_tx(transaction, self.crypto.as_ref())
    }

    async fn maybe_issue_receipt(
        &mut self,
        source_validator_id: ValidatorId,
        destination_validator_id: ValidatorId,
        message_class: MessageClass,
        slot: u64,
        transcript_digest: HashBytes,
        byte_hash: HashBytes,
        sequence_number: u64,
    ) -> Result<()> {
        if !self.config.feature_flags.enable_receipts {
            return Ok(());
        }
        if source_validator_id == self.config.validator_id {
            return Ok(());
        }
        let epoch = self.consensus.epoch_for_slot(slot);
        if !self
            .consensus
            .is_witness_for(self.config.validator_id, source_validator_id, epoch)
        {
            return Ok(());
        }
        if !self
            .consensus
            .is_relay_target_for(source_validator_id, destination_validator_id, epoch)
        {
            return Ok(());
        }

        let mut receipt = RelayReceipt {
            epoch,
            slot,
            source_validator_id,
            destination_validator_id,
            witness_validator_id: self.config.validator_id,
            message_class,
            transcript_digest,
            latency_bucket_ms: self.genesis.slot_duration_millis.min(1000),
            byte_count_bucket: u64::from(byte_hash[0]),
            sequence_number,
            signature: TypedSignature::default(),
        };
        let receipt_hash = receipt_signing_hash(&receipt);
        receipt.signature = self.crypto.sign(self.config.validator_id, &receipt_hash)?;
        if self.store_receipt(receipt.clone())? {
            self.network
                .broadcast(&self.config.peers, ProtocolMessage::RelayReceipt(receipt))?;
        }
        Ok(())
    }

    fn store_receipt(&mut self, receipt: RelayReceipt) -> Result<bool> {
        if let Err(error) = self.validate_receipt(&receipt) {
            self.log_event(
                "invalid-receipt",
                format!(
                    "source {} witness {} slot {} detail {}",
                    receipt.source_validator_id, receipt.witness_validator_id, receipt.slot, error
                ),
            )?;
            return Ok(false);
        }
        self.persist_receipt(receipt)
    }

    fn validate_receipt(&mut self, receipt: &RelayReceipt) -> Result<()> {
        let receipt_hash = canonical_hash(&receipt);
        if self.seen_receipts.contains(&receipt_hash) {
            return Ok(());
        }
        let receipt_hash_to_verify = receipt_signing_hash(&receipt);
        let verified = self.crypto.verify(
            receipt.witness_validator_id,
            &receipt_hash_to_verify,
            &receipt.signature,
        )?;
        if !verified {
            self.invalid_receipts += 1;
            self.record_invalid_receipt(receipt.epoch, receipt.witness_validator_id);
            return Err(anyhow!("invalid receipt signature"));
        }
        if let Err(error) = self.consensus.validate_receipt_assignment(receipt) {
            self.invalid_receipts += 1;
            self.record_invalid_receipt(receipt.epoch, receipt.witness_validator_id);
            return Err(anyhow!(error));
        }
        Ok(())
    }

    fn persist_receipt(&mut self, receipt: RelayReceipt) -> Result<bool> {
        let receipt_hash = canonical_hash(&receipt);
        if self.seen_receipts.contains(&receipt_hash) {
            return Ok(false);
        }
        let receipt_event_hash = receipt_event_hash(&receipt);
        if self.seen_receipt_events.contains(&receipt_event_hash) {
            self.update_metrics(|metrics| {
                metrics.duplicate_receipts_ignored += 1;
            });
            return Ok(false);
        }
        self.seen_receipts.insert(receipt_hash);
        self.seen_receipt_events.insert(receipt_event_hash);
        self.receipts.push(receipt.clone());
        self.storage
            .append_json_line(&self.storage.receipts_path, &receipt)?;
        self.update_metrics(|metrics| {
            metrics.receipts_created +=
                u64::from(receipt.witness_validator_id == self.config.validator_id);
            metrics.receipts_verified += 1;
        });
        self.log_event(
            "receipt-accepted",
            format!(
                "source {} witness {} slot {}",
                receipt.source_validator_id, receipt.witness_validator_id, receipt.slot
            ),
        )?;
        self.persist_metrics()?;
        Ok(true)
    }

    async fn accept_block(
        &mut self,
        block: Block,
        local_proposal: bool,
    ) -> Result<BlockAcceptance> {
        if self.known_block(block.block_hash).is_some() {
            return Ok(BlockAcceptance::Duplicate);
        }
        let expected_hash =
            canonical_hash(&(block.header.clone(), &block.transactions, &block.commitment));
        if expected_hash != block.block_hash {
            return Err(anyhow!("block hash mismatch"));
        }
        let verified = self.crypto.verify(
            block.header.proposer_id,
            &block.block_hash,
            &block.signature,
        )?;
        if !verified {
            return Err(anyhow!("invalid block signature"));
        }
        let expected_proposer = self.consensus.proposer_for_slot(block.header.slot);
        if block.header.proposer_id != expected_proposer {
            return Err(anyhow!("unexpected proposer"));
        }

        let transactions_root = canonical_hash(&block.transactions);
        if transactions_root != block.header.transactions_root {
            return Err(anyhow!("transactions root mismatch"));
        }
        let topology_root = block
            .commitment
            .as_ref()
            .map(canonical_hash)
            .unwrap_or_else(empty_hash);
        if topology_root != block.header.topology_root {
            return Err(anyhow!("topology root mismatch"));
        }

        if block.commitment.is_none() && !block.commitment_receipts.is_empty() {
            return Err(anyhow!("commitment receipts present without commitment"));
        }
        if let Some(commitment) = &block.commitment {
            self.validate_commitment(commitment, &block.commitment_receipts)?;
        }
        self.validate_hybrid_block_policy(&block)?;

        if block.header.parent_hash != self.ledger.snapshot().tip_hash {
            if !self
                .orphan_blocks
                .iter()
                .any(|orphan| orphan.block_hash == block.block_hash)
            {
                self.orphan_blocks.push(block.clone());
                self.storage
                    .append_json_line(&self.storage.orphan_path, &block)?;
                self.log_event(
                    "fork-observed",
                    format!(
                        "orphan block {} parent {:02x?}",
                        block.header.block_number,
                        &block.header.parent_hash[..4]
                    ),
                )?;
            }
            self.maybe_broadcast_vote_for_competing_block(&block)?;
            let adopted = self.maybe_adopt_preferred_competing_branch(&block)?
                || self.ledger.snapshot().tip_hash == block.block_hash;
            if adopted {
                return Ok(BlockAcceptance::Accepted);
            }
            return Ok(BlockAcceptance::Orphan);
        }

        let (service_gating_enforced, service_score) = if should_enforce_service_gating_for_block(
            self.config.feature_flags.enable_service_gating,
            local_proposal,
            block.header.epoch,
            self.config.feature_flags.service_gating_start_epoch,
        ) {
            if self.config.feature_flags.consensus_v2 {
                match self.v2_gating_state(block.header.proposer_id, block.header.epoch) {
                    V2GatingState::AllowNoEvidence | V2GatingState::AllowInsufficientEvidence => {
                        (false, None)
                    }
                    V2GatingState::AllowScore(score) | V2GatingState::RejectScore(score) => {
                        (true, Some(score))
                    }
                }
            } else {
                (
                    true,
                    Some(self.service_score_for_validator(block.header.proposer_id)),
                )
            }
        } else {
            (false, None)
        };
        self.consensus
            .validate_block_basic(
                &block,
                self.ledger.snapshot().tip_hash,
                service_score,
                service_gating_enforced,
                self.service_gating_threshold(),
            )
            .map_err(|error| anyhow!(error))?;

        let mut next_ledger = self.ledger.clone();
        next_ledger.apply_block(&block, self.crypto.as_ref())?;
        if self.certified_head_only_active()
            && !self.quorum_certificates.contains_key(&block.block_hash)
        {
            if !self
                .pending_certified_children
                .iter()
                .any(|pending| pending.block_hash == block.block_hash)
            {
                self.pending_certified_children.push(block.clone());
                self.log_event(
                    "certified-head-pending",
                    format!(
                        "height {} slot {} parent {:02x?}",
                        block.header.block_number,
                        block.header.slot,
                        &block.header.parent_hash[..4]
                    ),
                )?;
            }
            if !self.recovery_sync_only_active() {
                self.import_buffered_proposal_votes_for_block(&block)?;
                let should_support_block = self
                    .preferred_pending_child_for_certified_head()
                    .map(|pending| pending.block_hash == block.block_hash)
                    .unwrap_or(false);
                if should_support_block {
                    self.maybe_broadcast_local_proposal_vote(&block)?;
                }
                let _ = self.maybe_finalize_quorum_certificate(block.block_hash)?;
            }
            if self.ledger.snapshot().tip_hash == block.block_hash {
                return Ok(BlockAcceptance::Accepted);
            }
            self.persist_metrics()?;
            return Ok(BlockAcceptance::Orphan);
        }
        self.ledger = next_ledger;
        self.seen_blocks.insert(block.block_hash);
        self.blocks.push(block.clone());
        self.storage
            .append_json_line(&self.storage.blocks_path, &block)?;
        self.storage.write_snapshot(self.ledger.snapshot())?;
        self.import_commitment_receipts(&block.commitment_receipts)?;
        self.import_buffered_proposal_votes_for_block(&block)?;
        let _ = self.maybe_finalize_quorum_certificate(block.block_hash)?;
        for transaction in &block.transactions {
            self.mempool.remove(&transaction.tx_hash);
        }
        self.update_metrics(|metrics| {
            metrics.blocks_validated += 1;
            if local_proposal {
                metrics.blocks_proposed += 1;
            }
        });
        self.maybe_broadcast_local_proposal_vote(&block)?;
        self.log_event(
            if local_proposal {
                "block-proposed"
            } else {
                "block-accepted"
            },
            format!(
                "height {} slot {} txs {}",
                block.header.block_number,
                block.header.slot,
                block.transactions.len()
            ),
        )?;
        if let Err(error) = self.maybe_push_tip_progress_to_stale_peers() {
            self.log_event("sync-push-failed", format!("tip-progress detail {error}"))?;
        }
        self.persist_metrics()?;
        Ok(BlockAcceptance::Accepted)
    }

    fn validate_hybrid_block_policy(&self, block: &Block) -> Result<()> {
        if !self.config.feature_flags.require_hybrid_validator_signatures {
            return Ok(());
        }
        if block.signature.scheme() != entangrid_types::SignatureScheme::Hybrid {
            return Err(anyhow!(
                "hybrid enforcement requires block proposer {} to sign with a hybrid signature",
                block.header.proposer_id
            ));
        }
        Ok(())
    }

    fn maybe_broadcast_vote_for_competing_block(&mut self, block: &Block) -> Result<()> {
        if !self.config.feature_flags.consensus_v2 {
            return Ok(());
        }
        if !self.can_vote_for_competing_block(block)? {
            return Ok(());
        }
        self.maybe_broadcast_local_proposal_vote(block)
    }

    fn can_vote_for_competing_block(&self, block: &Block) -> Result<bool> {
        let Some(mut parent_chain) = self.build_parent_chain_for_block(block)? else {
            return Ok(false);
        };
        if let Some(locked_qc_hash) = self.highest_locked_qc_hash() {
            let locked_in_chain = parent_chain
                .iter()
                .any(|ancestor| ancestor.block_hash == locked_qc_hash);
            if !locked_in_chain {
                return Ok(false);
            }
        }
        let mut ledger =
            LedgerState::replay_blocks(&self.genesis, &parent_chain, self.crypto.as_ref())?;
        ledger.apply_block(block, self.crypto.as_ref())?;
        parent_chain.push(block.clone());
        let candidate_quality =
            self.branch_quality_with_extra_tip_vote(&parent_chain, Some(block.block_hash));
        let current_quality = self.branch_quality_with_extra_tip_vote(&self.blocks, None);
        Ok(candidate_quality >= current_quality)
    }

    fn build_parent_chain_for_block(&self, block: &Block) -> Result<Option<Vec<Block>>> {
        if block.header.block_number == 1 && block.header.parent_hash == empty_hash() {
            return Ok(Some(Vec::new()));
        }
        Ok(self.build_chain_to_tip(block.header.parent_hash))
    }

    fn highest_locked_qc_hash(&self) -> Option<HashBytes> {
        self.blocks
            .iter()
            .rev()
            .find(|block| self.quorum_certificates.contains_key(&block.block_hash))
            .map(|block| block.block_hash)
    }

    fn certified_head_only_active(&self) -> bool {
        self.config.feature_flags.consensus_v2 && self.highest_locked_qc_hash().is_some()
    }

    fn recovery_sync_only_active(&self) -> bool {
        self.startup_sync_barrier
            && self
                .config
                .peers
                .iter()
                .any(|peer| self.peer_status_is_ahead(peer.validator_id))
    }

    fn is_pending_certified_head_block(&self, block: &Block) -> bool {
        self.certified_head_only_active()
            && block.header.parent_hash == self.ledger.snapshot().tip_hash
            && self
                .pending_certified_children
                .iter()
                .any(|pending| pending.block_hash == block.block_hash)
    }

    fn preferred_pending_child_for_certified_head(&self) -> Option<Block> {
        if !self.certified_head_only_active() {
            return None;
        }
        let parent_hash = self.ledger.snapshot().tip_hash;
        let block_number = self.ledger.block_height() + 1;
        self.pending_certified_children
            .iter()
            .filter(|block| {
                block.header.parent_hash == parent_hash && block.header.block_number == block_number
            })
            .min_by(|left, right| {
                left.header
                    .slot
                    .cmp(&right.header.slot)
                    .then_with(|| left.block_hash.cmp(&right.block_hash))
            })
            .cloned()
    }

    fn maybe_broadcast_local_proposal_vote(&mut self, block: &Block) -> Result<()> {
        if !self.config.feature_flags.consensus_v2 {
            return Ok(());
        }
        let vote = self.build_local_proposal_vote(block)?;
        let imported = match self.import_proposal_vote(vote.clone()) {
            Ok(imported) => imported,
            Err(error) if error.to_string().contains("conflicting proposal vote") => {
                self.log_event(
                    "proposal-vote-skipped",
                    format!(
                        "block {:02x?} height {} slot {} validator {} detail {}",
                        &block.block_hash[..4],
                        block.header.block_number,
                        block.header.slot,
                        self.config.validator_id,
                        error
                    ),
                )?;
                return Ok(());
            }
            Err(error) => return Err(error),
        };
        if imported {
            self.network
                .broadcast(&self.config.peers, ProtocolMessage::ProposalVote(vote))?;
        }
        Ok(())
    }

    fn build_local_proposal_vote(&self, block: &Block) -> Result<ProposalVote> {
        let mut vote = ProposalVote {
            validator_id: self.config.validator_id,
            block_hash: block.block_hash,
            block_number: block.header.block_number,
            epoch: block.header.epoch,
            slot: block.header.slot,
            signature: TypedSignature::default(),
        };
        let hash = proposal_vote_signing_hash(&vote);
        vote.signature = self.crypto.sign(self.config.validator_id, &hash)?;
        Ok(vote)
    }

    fn import_proposal_vote(&mut self, vote: ProposalVote) -> Result<bool> {
        if !self.config.feature_flags.consensus_v2 {
            return Err(anyhow!("proposal votes require consensus_v2"));
        }
        self.validate_proposal_vote_signature_and_signer(&vote)?;
        self.validate_hybrid_proposal_vote_policy(&vote)?;
        let Some(block) = self.known_block(vote.block_hash).cloned() else {
            return self.store_buffered_proposal_vote(vote);
        };
        if self.recovery_sync_only_active()
            && self
                .pending_certified_children
                .iter()
                .any(|pending| pending.block_hash == block.block_hash)
        {
            self.log_event(
                "proposal-vote-deferred",
                format!(
                    "block {:02x?} height {} slot {} validator {}",
                    &vote.block_hash[..4],
                    vote.block_number,
                    vote.slot,
                    vote.validator_id
                ),
            )?;
            return Ok(false);
        }
        self.validate_proposal_vote_for_block(&vote, &block)?;
        let inserted = self.store_proposal_vote(vote.clone())?;
        if inserted {
            if self
                .maybe_finalize_quorum_certificate(vote.block_hash)?
                .is_none()
            {
                self.maybe_adopt_vote_supported_branch(vote.block_hash)?;
            }
        }
        Ok(inserted)
    }

    fn validate_hybrid_proposal_vote_policy(&self, vote: &ProposalVote) -> Result<()> {
        if !self.config.feature_flags.require_hybrid_validator_signatures {
            return Ok(());
        }
        if vote.signature.scheme() != entangrid_types::SignatureScheme::Hybrid {
            return Err(anyhow!(
                "hybrid enforcement requires proposal vote from validator {} to use a hybrid signature",
                vote.validator_id
            ));
        }
        Ok(())
    }

    fn import_quorum_certificate(&mut self, qc: QuorumCertificate) -> Result<bool> {
        if !self.config.feature_flags.consensus_v2 {
            return Err(anyhow!("quorum certificates require consensus_v2"));
        }
        let Some(block) = self.known_block(qc.block_hash) else {
            return Err(anyhow!("quorum certificate references unknown block"));
        };
        self.validate_quorum_certificate_for_block(&qc, block)?;
        for vote in qc.votes.iter().cloned() {
            self.remove_conflicting_votes_for_certified_vote(&vote);
            let _ = self.store_proposal_vote(vote)?;
        }
        if self.quorum_certificates.contains_key(&qc.block_hash) {
            return Ok(false);
        }
        self.quorum_certificates.insert(qc.block_hash, qc.clone());
        self.prune_votes_not_extending_certified_block(qc.block_hash, qc.block_number);
        self.maybe_adopt_certified_branch(qc.block_hash)?;
        self.log_event(
            "qc-imported",
            format!(
                "height {} slot {} block {:02x?} votes {}",
                qc.block_number,
                qc.slot,
                &qc.block_hash[..4],
                qc.votes.len()
            ),
        )?;
        Ok(true)
    }

    fn validate_proposal_vote_signature_and_signer(&self, vote: &ProposalVote) -> Result<()> {
        if !self
            .genesis
            .validators
            .iter()
            .any(|validator| validator.validator_id == vote.validator_id)
        {
            return Err(anyhow!("proposal vote signer is not a validator"));
        }
        let hash = proposal_vote_signing_hash(vote);
        let verified = self
            .crypto
            .verify(vote.validator_id, &hash, &vote.signature)?;
        if !verified {
            return Err(anyhow!("invalid proposal vote signature"));
        }
        Ok(())
    }

    fn remove_conflicting_votes_for_certified_vote(&mut self, vote: &ProposalVote) {
        self.proposal_votes.retain(|block_hash, by_validator| {
            if *block_hash != vote.block_hash {
                by_validator.retain(|validator_id, existing| {
                    !(*validator_id == vote.validator_id
                        && existing.block_hash != vote.block_hash
                        && existing.block_number == vote.block_number)
                });
            }
            !by_validator.is_empty()
        });
        self.buffered_proposal_votes
            .retain(|block_hash, by_validator| {
                if *block_hash != vote.block_hash {
                    by_validator.retain(|validator_id, existing| {
                        !(*validator_id == vote.validator_id
                            && existing.block_hash != vote.block_hash
                            && existing.block_number == vote.block_number)
                    });
                }
                !by_validator.is_empty()
            });
    }

    fn validate_proposal_vote_for_block(&self, vote: &ProposalVote, block: &Block) -> Result<()> {
        self.validate_proposal_vote_signature_and_signer(vote)?;
        if vote.block_number != block.header.block_number
            || vote.epoch != block.header.epoch
            || vote.slot != block.header.slot
        {
            return Err(anyhow!("proposal vote metadata does not match block"));
        }
        Ok(())
    }

    fn validate_quorum_certificate_for_block(
        &self,
        qc: &QuorumCertificate,
        block: &Block,
    ) -> Result<()> {
        if qc.block_hash != block.block_hash
            || qc.block_number != block.header.block_number
            || qc.epoch != block.header.epoch
            || qc.slot != block.header.slot
        {
            return Err(anyhow!("quorum certificate metadata does not match block"));
        }
        if !qc.is_well_formed() {
            return Err(anyhow!("malformed quorum certificate"));
        }
        let threshold = quorum_certificate_threshold(self.genesis.validators.len());
        if qc.votes.len() < threshold {
            return Err(anyhow!("quorum certificate does not meet threshold"));
        }
        if entangrid_types::quorum_certificate_vote_root(&qc.votes) != qc.vote_root {
            return Err(anyhow!("quorum certificate vote root mismatch"));
        }
        for vote in &qc.votes {
            self.validate_proposal_vote_for_block(vote, block)?;
        }
        Ok(())
    }

    fn store_proposal_vote(&mut self, vote: ProposalVote) -> Result<bool> {
        self.resolve_conflicting_proposal_vote(&vote)?;
        let by_validator = self.proposal_votes.entry(vote.block_hash).or_default();
        if let Some(existing) = by_validator.get(&vote.validator_id) {
            if proposal_vote_signing_hash(existing) == proposal_vote_signing_hash(&vote)
                && existing.signature == vote.signature
            {
                return Ok(false);
            }
            return Err(anyhow!("conflicting proposal vote"));
        }
        by_validator.insert(vote.validator_id, vote);
        Ok(true)
    }

    fn store_buffered_proposal_vote(&mut self, vote: ProposalVote) -> Result<bool> {
        self.resolve_conflicting_proposal_vote(&vote)?;
        let by_validator = self
            .buffered_proposal_votes
            .entry(vote.block_hash)
            .or_default();
        if let Some(existing) = by_validator.get(&vote.validator_id) {
            if proposal_vote_signing_hash(existing) == proposal_vote_signing_hash(&vote)
                && existing.signature == vote.signature
            {
                return Ok(false);
            }
            return Err(anyhow!("conflicting proposal vote"));
        }
        by_validator.insert(vote.validator_id, vote.clone());
        self.log_event(
            "proposal-vote-buffered",
            format!(
                "block {:02x?} height {} slot {} validator {}",
                &vote.block_hash[..4],
                vote.block_number,
                vote.slot,
                vote.validator_id
            ),
        )?;
        Ok(true)
    }

    fn can_replace_conflicting_proposal_vote(
        &self,
        existing: &ProposalVote,
        candidate: &ProposalVote,
    ) -> bool {
        if !self.certified_head_only_active() || existing.block_number != candidate.block_number {
            return false;
        }
        if self.quorum_certificates.contains_key(&existing.block_hash) {
            return false;
        }
        let certified_head_hash = self.ledger.snapshot().tip_hash;
        let Some(existing_block) = self.known_block(existing.block_hash) else {
            return false;
        };
        let Some(candidate_block) = self.known_block(candidate.block_hash) else {
            return false;
        };
        if existing_block.header.parent_hash != certified_head_hash
            || candidate_block.header.parent_hash != certified_head_hash
        {
            return false;
        }
        candidate_block.header.slot < existing_block.header.slot
            || (candidate_block.header.slot == existing_block.header.slot
                && candidate_block.block_hash < existing_block.block_hash)
    }

    fn remove_proposal_vote_from_maps(&mut self, vote: &ProposalVote) {
        if let Some(by_validator) = self.proposal_votes.get_mut(&vote.block_hash) {
            by_validator.remove(&vote.validator_id);
            if by_validator.is_empty() {
                self.proposal_votes.remove(&vote.block_hash);
            }
        }
        if let Some(by_validator) = self.buffered_proposal_votes.get_mut(&vote.block_hash) {
            by_validator.remove(&vote.validator_id);
            if by_validator.is_empty() {
                self.buffered_proposal_votes.remove(&vote.block_hash);
            }
        }
    }

    fn resolve_conflicting_proposal_vote(&mut self, vote: &ProposalVote) -> Result<()> {
        let mut conflicts = Vec::new();
        for existing_votes in self
            .proposal_votes
            .values()
            .chain(self.buffered_proposal_votes.values())
        {
            if let Some(existing) = existing_votes.get(&vote.validator_id) {
                if existing.block_hash == vote.block_hash {
                    continue;
                }
                if existing.block_number != vote.block_number
                    && !(existing.epoch == vote.epoch && existing.slot == vote.slot)
                {
                    continue;
                }
                if self.can_replace_conflicting_proposal_vote(existing, vote) {
                    conflicts.push(existing.clone());
                    continue;
                }
                return Err(anyhow!("conflicting proposal vote"));
            }
        }

        for existing in conflicts {
            self.remove_proposal_vote_from_maps(&existing);
            self.log_event(
                "proposal-vote-replaced",
                format!(
                    "validator {} old {:02x?} slot {} new {:02x?} slot {}",
                    vote.validator_id,
                    &existing.block_hash[..4],
                    existing.slot,
                    &vote.block_hash[..4],
                    vote.slot
                ),
            )?;
        }
        Ok(())
    }

    fn maybe_build_qc(&mut self, block_hash: HashBytes) -> Result<Option<QuorumCertificate>> {
        if let Some(existing) = self.quorum_certificates.get(&block_hash) {
            return Ok(Some(existing.clone()));
        }
        let Some(block) = self.known_block(block_hash) else {
            return Ok(None);
        };
        let block_number = block.header.block_number;
        let epoch = block.header.epoch;
        let slot = block.header.slot;
        let Some(by_validator) = self.proposal_votes.get(&block_hash) else {
            return Ok(None);
        };
        let threshold = quorum_certificate_threshold(self.genesis.validators.len());
        if by_validator.len() < threshold {
            return Ok(None);
        }
        let votes = by_validator.values().cloned().collect::<Vec<_>>();
        let qc = QuorumCertificate {
            block_hash,
            block_number,
            epoch,
            slot,
            vote_root: entangrid_types::quorum_certificate_vote_root(&votes),
            votes,
        };
        if !qc.is_well_formed() {
            return Err(anyhow!("built malformed quorum certificate"));
        }
        self.quorum_certificates.insert(block_hash, qc.clone());
        self.prune_votes_not_extending_certified_block(block_hash, block_number);
        Ok(Some(qc))
    }

    fn maybe_finalize_quorum_certificate(
        &mut self,
        block_hash: HashBytes,
    ) -> Result<Option<QuorumCertificate>> {
        let already_had_qc = self.quorum_certificates.contains_key(&block_hash);
        let Some(qc) = self.maybe_build_qc(block_hash)? else {
            return Ok(None);
        };
        self.maybe_adopt_certified_branch(qc.block_hash)?;
        if !already_had_qc {
            self.network.broadcast(
                &self.config.peers,
                ProtocolMessage::QuorumCertificate(qc.clone()),
            )?;
            self.log_event(
                "qc-built",
                format!(
                    "height {} slot {} block {:02x?} votes {}",
                    qc.block_number,
                    qc.slot,
                    &qc.block_hash[..4],
                    qc.votes.len()
                ),
            )?;
            self.announce_certified_progress()?;
        }
        Ok(Some(qc))
    }

    fn import_buffered_proposal_votes_for_block(&mut self, block: &Block) -> Result<()> {
        let Some(by_validator) = self.buffered_proposal_votes.remove(&block.block_hash) else {
            return Ok(());
        };
        for vote in by_validator.into_values() {
            if let Err(error) = self.validate_proposal_vote_for_block(&vote, block) {
                self.log_event(
                    "proposal-vote-buffer-dropped",
                    format!(
                        "block {:02x?} validator {} detail {error}",
                        &vote.block_hash[..4],
                        vote.validator_id
                    ),
                )?;
                continue;
            }
            if let Err(error) = self.store_proposal_vote(vote.clone()) {
                self.log_event(
                    "proposal-vote-buffer-dropped",
                    format!(
                        "block {:02x?} validator {} detail {error}",
                        &vote.block_hash[..4],
                        vote.validator_id
                    ),
                )?;
            }
        }
        Ok(())
    }

    fn vote_extends_certified_block(
        &self,
        block_hash: HashBytes,
        certified_hash: HashBytes,
        certified_height: u64,
    ) -> bool {
        if block_hash == certified_hash {
            return true;
        }
        let Some(block) = self.known_block(block_hash) else {
            return true;
        };
        if block.header.block_number < certified_height {
            return true;
        }
        self.build_chain_to_tip(block_hash)
            .map(|chain| {
                chain
                    .iter()
                    .any(|ancestor| ancestor.block_hash == certified_hash)
            })
            .unwrap_or(false)
    }

    fn prune_votes_not_extending_certified_block(
        &mut self,
        certified_hash: HashBytes,
        certified_height: u64,
    ) {
        let proposal_vote_hashes_to_keep = self
            .proposal_votes
            .keys()
            .copied()
            .filter(|block_hash| {
                self.vote_extends_certified_block(*block_hash, certified_hash, certified_height)
            })
            .collect::<BTreeSet<_>>();
        let buffered_vote_hashes_to_keep = self
            .buffered_proposal_votes
            .keys()
            .copied()
            .filter(|block_hash| {
                self.vote_extends_certified_block(*block_hash, certified_hash, certified_height)
            })
            .collect::<BTreeSet<_>>();

        self.proposal_votes.retain(|block_hash, by_validator| {
            proposal_vote_hashes_to_keep.contains(block_hash) && !by_validator.is_empty()
        });
        self.buffered_proposal_votes
            .retain(|block_hash, by_validator| {
                buffered_vote_hashes_to_keep.contains(block_hash) && !by_validator.is_empty()
            });
    }

    fn known_block(&self, block_hash: HashBytes) -> Option<&Block> {
        self.blocks
            .iter()
            .find(|block| block.block_hash == block_hash)
            .or_else(|| {
                self.pending_certified_children
                    .iter()
                    .find(|block| block.block_hash == block_hash)
            })
            .or_else(|| {
                self.orphan_blocks
                    .iter()
                    .find(|block| block.block_hash == block_hash)
            })
    }

    fn maybe_adopt_certified_branch(&mut self, candidate_tip_hash: HashBytes) -> Result<bool> {
        let Some(candidate_chain) = self.build_chain_to_tip(candidate_tip_hash) else {
            return Ok(false);
        };
        if candidate_chain.is_empty() {
            return Ok(false);
        }
        if self.compare_branch_quality(&candidate_chain, &self.blocks) != Ordering::Greater {
            return Ok(false);
        }
        self.adopt_canonical_chain(candidate_chain, "certified-reorg")?;
        Ok(true)
    }

    fn maybe_adopt_vote_supported_branch(&mut self, candidate_tip_hash: HashBytes) -> Result<bool> {
        if !self.config.feature_flags.consensus_v2 {
            return Ok(false);
        }
        if self.certified_head_only_active() {
            return Ok(false);
        }
        let Some(locked_qc_hash) = self.highest_locked_qc_hash() else {
            return Ok(false);
        };
        let Some(candidate_chain) = self.build_chain_to_tip(candidate_tip_hash) else {
            return Ok(false);
        };
        if !candidate_chain
            .iter()
            .any(|block| block.block_hash == locked_qc_hash)
        {
            return Ok(false);
        }
        if self.compare_branch_quality(&candidate_chain, &self.blocks) != Ordering::Greater {
            return Ok(false);
        }
        self.adopt_canonical_chain(candidate_chain, "vote-supported-branch-adopted")?;
        Ok(true)
    }

    fn maybe_adopt_preferred_competing_branch(&mut self, block: &Block) -> Result<bool> {
        if !self.config.feature_flags.consensus_v2 {
            return Ok(false);
        }
        if self.certified_head_only_active() {
            return Ok(false);
        }
        let Some(locked_qc_hash) = self.highest_locked_qc_hash() else {
            return Ok(false);
        };
        let Some(mut candidate_chain) = self.build_parent_chain_for_block(block)? else {
            return Ok(false);
        };
        if !candidate_chain
            .iter()
            .any(|ancestor| ancestor.block_hash == locked_qc_hash)
        {
            return Ok(false);
        }
        let mut candidate_ledger =
            LedgerState::replay_blocks(&self.genesis, &candidate_chain, self.crypto.as_ref())?;
        candidate_ledger.apply_block(block, self.crypto.as_ref())?;
        candidate_chain.push(block.clone());
        if self.compare_branch_quality(&candidate_chain, &self.blocks) != Ordering::Greater {
            return Ok(false);
        }
        self.adopt_canonical_chain(candidate_chain, "preferred-branch-adopted")?;
        Ok(true)
    }

    fn build_chain_to_tip(&self, tip_hash: HashBytes) -> Option<Vec<Block>> {
        let mut by_hash = BTreeMap::new();
        for block in self
            .blocks
            .iter()
            .chain(self.pending_certified_children.iter())
            .chain(self.orphan_blocks.iter())
        {
            by_hash.insert(block.block_hash, block.clone());
        }

        let mut chain = Vec::new();
        let mut current_hash = tip_hash;
        let mut visited = BTreeSet::new();
        while current_hash != empty_hash() {
            if !visited.insert(current_hash) {
                return None;
            }
            let block = by_hash.get(&current_hash)?.clone();
            current_hash = block.header.parent_hash;
            chain.push(block);
        }
        chain.reverse();

        let mut expected_parent = empty_hash();
        let mut expected_height = 1u64;
        for block in &chain {
            if block.header.parent_hash != expected_parent
                || block.header.block_number != expected_height
            {
                return None;
            }
            expected_parent = block.block_hash;
            expected_height += 1;
        }
        Some(chain)
    }

    fn compare_branch_quality(
        &self,
        candidate_chain: &[Block],
        current_chain: &[Block],
    ) -> Ordering {
        let candidate_certified = self.latest_certified_block(candidate_chain);
        let current_certified = self.latest_certified_block(current_chain);
        let candidate_certified_height = candidate_certified
            .map(|block| block.header.block_number)
            .unwrap_or(0);
        let current_certified_height = current_certified
            .map(|block| block.header.block_number)
            .unwrap_or(0);
        let certified_cmp = candidate_certified_height.cmp(&current_certified_height);
        if certified_cmp != Ordering::Equal {
            return certified_cmp;
        }
        if candidate_certified_height > 0 {
            let candidate_tip_certified = candidate_chain
                .last()
                .map(|block| self.quorum_certificates.contains_key(&block.block_hash))
                .unwrap_or(false);
            let current_tip_certified = current_chain
                .last()
                .map(|block| self.quorum_certificates.contains_key(&block.block_hash))
                .unwrap_or(false);
            let tip_certified_cmp = candidate_tip_certified.cmp(&current_tip_certified);
            if tip_certified_cmp != Ordering::Equal {
                return tip_certified_cmp;
            }
            let candidate_certified_hash = candidate_certified
                .map(|block| block.block_hash)
                .unwrap_or_else(empty_hash);
            let current_certified_hash = current_certified
                .map(|block| block.block_hash)
                .unwrap_or_else(empty_hash);
            return candidate_certified_hash.cmp(&current_certified_hash);
        }
        let candidate_quality = self.branch_quality_with_extra_tip_vote(candidate_chain, None);
        let current_quality = self.branch_quality_with_extra_tip_vote(current_chain, None);
        candidate_quality.cmp(&current_quality)
    }

    fn latest_certified_block<'a>(&self, chain: &'a [Block]) -> Option<&'a Block> {
        chain
            .iter()
            .rev()
            .find(|block| self.quorum_certificates.contains_key(&block.block_hash))
    }

    fn latest_certified_height(&self, chain: &[Block]) -> u64 {
        self.latest_certified_block(chain)
            .map(|block| block.header.block_number)
            .unwrap_or(0)
    }

    fn branch_quality_with_extra_tip_vote(
        &self,
        chain: &[Block],
        extra_tip_vote_for: Option<HashBytes>,
    ) -> (u64, u64, usize, u64, HashBytes) {
        let latest_certified_height = self.latest_certified_height(chain);
        let tip = chain.last();
        let tip_vote_count = tip
            .map(|block| {
                let existing = self
                    .proposal_votes
                    .get(&block.block_hash)
                    .map(|votes| votes.len())
                    .unwrap_or(0);
                let receives_extra_local_vote = extra_tip_vote_for == Some(block.block_hash)
                    && !self
                        .proposal_votes
                        .get(&block.block_hash)
                        .map(|votes| votes.contains_key(&self.config.validator_id))
                        .unwrap_or(false);
                existing + usize::from(receives_extra_local_vote)
            })
            .unwrap_or(0);
        (
            latest_certified_height,
            tip.map(|block| block.header.block_number).unwrap_or(0),
            tip_vote_count,
            tip.map(|block| block.header.slot).unwrap_or(0),
            tip.map(|block| block.block_hash).unwrap_or_else(empty_hash),
        )
    }

    fn adopt_canonical_chain(
        &mut self,
        candidate_chain: Vec<Block>,
        event_name: &str,
    ) -> Result<()> {
        let ledger =
            LedgerState::replay_blocks(&self.genesis, &candidate_chain, self.crypto.as_ref())?;
        let candidate_hashes: BTreeSet<_> = candidate_chain
            .iter()
            .map(|block| block.block_hash)
            .collect();
        let orphan_blocks = self
            .blocks
            .iter()
            .chain(self.orphan_blocks.iter())
            .chain(self.pending_certified_children.iter())
            .filter(|block| !candidate_hashes.contains(&block.block_hash))
            .cloned()
            .collect::<Vec<_>>();

        self.ledger = ledger;
        self.blocks = candidate_chain;
        self.orphan_blocks = orphan_blocks;
        self.pending_certified_children.clear();
        let receipts_to_import = self
            .blocks
            .iter()
            .flat_map(|block| block.commitment_receipts.clone())
            .collect::<Vec<_>>();
        for receipt in receipts_to_import {
            let _ = self.persist_receipt(receipt)?;
        }
        self.rebuild_seen_sets();
        self.storage.write_snapshot(self.ledger.snapshot())?;
        self.storage
            .overwrite_json_lines(&self.storage.blocks_path, &self.blocks)?;
        self.storage
            .overwrite_json_lines(&self.storage.orphan_path, &self.orphan_blocks)?;
        self.log_event(
            event_name,
            format!(
                "adopted tip {:02x?} height {}",
                &self.ledger.snapshot().tip_hash[..4],
                self.ledger.snapshot().height
            ),
        )?;
        Ok(())
    }

    fn validate_commitment(
        &mut self,
        commitment: &TopologyCommitment,
        commitment_receipts: &[RelayReceipt],
    ) -> Result<()> {
        validate_commitment_bundle(
            &self.consensus,
            self.crypto.as_ref(),
            commitment,
            commitment_receipts,
            &self.config.feature_flags.service_score_weights,
        )
    }

    fn import_commitment_receipts(&mut self, receipts: &[RelayReceipt]) -> Result<()> {
        for receipt in receipts {
            let _ = self.persist_receipt(receipt.clone())?;
        }
        Ok(())
    }

    fn request_receipt_reconciliation(&self, epoch: Epoch) -> Result<()> {
        if !self.config.feature_flags.enable_receipts || self.config.peers.is_empty() {
            return Ok(());
        }

        let peer_ids: BTreeSet<_> = self
            .config
            .peers
            .iter()
            .map(|peer| peer.validator_id)
            .collect();
        let mut attempted = 0u64;
        let mut failed = 0u64;
        let validator_id = self.config.validator_id;

        for peer_id in build_receipt_fetch_plan(self.config.validator_id, &peer_ids) {
            attempted += 1;
            if let Err(error) = self.network.send_control_to(
                self.find_peer(peer_id)?,
                ProtocolMessage::ReceiptFetch {
                    requester_id: self.config.validator_id,
                    epoch,
                    validator_id,
                },
            ) {
                failed += 1;
                self.log_event(
                    "receipt-fetch-failed",
                    format!("epoch {epoch} peer {peer_id} validator {validator_id} detail {error}"),
                )?;
            }
        }

        self.log_event(
            "receipt-fetch-round",
            format!(
                "epoch {epoch} peers {} validators 1 attempted {attempted} failed {failed}",
                peer_ids.len()
            ),
        )?;
        Ok(())
    }

    async fn try_promote_orphans(&mut self) -> Result<()> {
        let mut progress = true;
        while progress {
            progress = false;
            let tip_hash = self.ledger.snapshot().tip_hash;
            if let Some(index) = self
                .orphan_blocks
                .iter()
                .position(|orphan| orphan.header.parent_hash == tip_hash)
            {
                let orphan = self.orphan_blocks.remove(index);
                match self.accept_block(orphan.clone(), false).await {
                    Ok(BlockAcceptance::Accepted | BlockAcceptance::Duplicate) => {}
                    Ok(BlockAcceptance::Orphan) => {
                        self.log_event(
                            "orphan-rejected",
                            format!(
                                "height {} slot {} detail still orphaned after promotion",
                                orphan.header.block_number, orphan.header.slot
                            ),
                        )?;
                    }
                    Err(error) => {
                        self.log_event(
                            "orphan-rejected",
                            format!(
                                "height {} slot {} detail {}",
                                orphan.header.block_number, orphan.header.slot, error
                            ),
                        )?;
                    }
                }
                progress = true;
            }
        }
        Ok(())
    }

    fn apply_chain_snapshot(&mut self, chain: ChainSnapshot) -> Result<()> {
        let (ledger, receipts) =
            validate_chain_snapshot(&self.genesis, &self.consensus, self.crypto.as_ref(), &chain)?;
        self.ledger = ledger;
        self.blocks = chain.blocks;
        self.receipts = receipts;
        self.orphan_blocks.clear();
        self.pending_certified_children.clear();
        self.buffered_proposal_votes.clear();
        self.rebuild_seen_sets();
        self.storage.write_snapshot(self.ledger.snapshot())?;
        self.storage
            .overwrite_json_lines(&self.storage.blocks_path, &self.blocks)?;
        self.storage
            .overwrite_json_lines(&self.storage.orphan_path, &self.orphan_blocks)?;
        self.storage
            .overwrite_json_lines(&self.storage.receipts_path, &self.receipts)?;
        Ok(())
    }

    fn apply_chain_segment(&mut self, chain: ChainSegment) -> Result<()> {
        let (base_blocks, base_snapshot) = self.sync_repair_base_for_segment(&chain)?;
        let chain = align_chain_segment_to_local_chain(&base_blocks, &base_snapshot, chain)?;

        let receipts =
            validate_snapshot_receipts(&self.consensus, self.crypto.as_ref(), &chain.receipts)?;
        let mut ledger =
            LedgerState::replay_blocks(&self.genesis, &base_blocks, self.crypto.as_ref())?;
        let mut expected_parent_hash = base_snapshot.tip_hash;
        for block in &chain.blocks {
            validate_snapshot_block(
                &self.consensus,
                self.crypto.as_ref(),
                &ledger,
                block,
                expected_parent_hash,
            )?;
            let mut next_ledger = ledger.clone();
            next_ledger.apply_block(block, self.crypto.as_ref())?;
            ledger = next_ledger;
            expected_parent_hash = block.block_hash;
        }

        if ledger.snapshot() != &chain.target_snapshot {
            return Err(anyhow!("incremental sync replay mismatch"));
        }

        self.ledger = ledger;
        self.blocks = base_blocks;
        self.blocks.extend(chain.blocks);
        self.orphan_blocks.clear();
        self.pending_certified_children.clear();
        self.buffered_proposal_votes.clear();
        self.rebuild_seen_sets();
        self.storage.write_snapshot(self.ledger.snapshot())?;
        self.storage
            .overwrite_json_lines(&self.storage.blocks_path, &self.blocks)?;
        self.storage
            .overwrite_json_lines(&self.storage.orphan_path, &self.orphan_blocks)?;
        for receipt in receipts {
            self.store_receipt(receipt)?;
        }
        for vote in chain.proposal_votes {
            let _ = self.import_proposal_vote(vote);
        }
        Ok(())
    }

    fn sync_repair_base_for_segment(
        &self,
        chain: &ChainSegment,
    ) -> Result<(Vec<Block>, StateSnapshot)> {
        let local_snapshot = self.ledger.snapshot().clone();
        if !self.config.feature_flags.consensus_v2 {
            return Ok((self.blocks.clone(), local_snapshot));
        }
        if let Some(local_qc) = self.highest_local_qc() {
            if local_snapshot.height > local_qc.block_number
                && chain.base_height == local_qc.block_number
                && chain.base_tip_hash == local_qc.block_hash
            {
                let prefix_len = local_qc.block_number as usize;
                let ledger = LedgerState::replay_blocks(
                    &self.genesis,
                    &self.blocks[..prefix_len],
                    self.crypto.as_ref(),
                )?;
                return Ok((
                    self.blocks[..prefix_len].to_vec(),
                    ledger.snapshot().clone(),
                ));
            }
        }
        Ok((self.blocks.clone(), local_snapshot))
    }

    fn should_adopt_chain_segment(
        &self,
        local_snapshot: &StateSnapshot,
        chain: &ChainSegment,
    ) -> bool {
        if should_adopt_snapshot(local_snapshot, &chain.target_snapshot) {
            return true;
        }
        if !self.config.feature_flags.consensus_v2 {
            return false;
        }
        let Some(local_qc) = self.highest_local_qc() else {
            return false;
        };
        local_snapshot.height > local_qc.block_number
            && chain.base_height == local_qc.block_number
            && chain.base_tip_hash == local_qc.block_hash
            && chain.target_snapshot.height > local_qc.block_number
            && snapshot_preference(&chain.target_snapshot) != snapshot_preference(local_snapshot)
    }

    fn import_certified_sync_response(&mut self, response: ChunkedSyncResponse) -> Result<bool> {
        match response {
            ChunkedSyncResponse::Unavailable { responder_id } => {
                self.log_event(
                    "certified-sync-unavailable",
                    format!("from validator {responder_id}"),
                )?;
                Ok(false)
            }
            ChunkedSyncResponse::Certified {
                responder_id,
                responder_height,
                responder_tip_hash,
                shared_qc_hash,
                shared_qc_height,
                headers,
                blocks,
                qcs,
                service_aggregates,
            } => {
                if blocks.is_empty() {
                    return Ok(false);
                }
                if blocks.len() != qcs.len() || headers.len() != blocks.len() {
                    return Err(anyhow!("certified sync payload length mismatch"));
                }
                let local_shared_qc = if shared_qc_height == 0 {
                    empty_hash()
                } else {
                    self.blocks
                        .get(shared_qc_height.saturating_sub(1) as usize)
                        .map(|block| block.block_hash)
                        .ok_or_else(|| anyhow!("missing local shared QC height"))?
                };
                if local_shared_qc != shared_qc_hash {
                    return Err(anyhow!("certified sync shared QC mismatch"));
                }

                let prefix_len = shared_qc_height as usize;
                let mut candidate_chain = self.blocks[..prefix_len].to_vec();
                candidate_chain.extend(blocks.iter().cloned());
                let local_snapshot = self.ledger.snapshot().clone();
                let local_highest_qc_height = self
                    .highest_local_qc()
                    .map(|qc| qc.block_number)
                    .unwrap_or(0);

                let mut ledger = LedgerState::from_genesis(&self.genesis);
                let mut expected_parent_hash = empty_hash();
                for block in &candidate_chain {
                    validate_snapshot_block(
                        &self.consensus,
                        self.crypto.as_ref(),
                        &ledger,
                        block,
                        expected_parent_hash,
                    )?;
                    let mut next_ledger = ledger.clone();
                    next_ledger.apply_block(block, self.crypto.as_ref())?;
                    ledger = next_ledger;
                    expected_parent_hash = block.block_hash;
                }

                for ((header, block), qc) in headers.iter().zip(blocks.iter()).zip(qcs.iter()) {
                    if header.block_hash != block.block_hash || header.header != block.header {
                        return Err(anyhow!("certified sync header mismatch"));
                    }
                    self.validate_quorum_certificate_for_block(qc, block)?;
                }

                let candidate_snapshot = ledger.snapshot().clone();
                let (candidate_highest_qc_hash, candidate_highest_qc_height) = qcs
                    .last()
                    .map(|qc| (Some(qc.block_hash), qc.block_number))
                    .unwrap_or((
                        (shared_qc_height > 0).then_some(shared_qc_hash),
                        shared_qc_height,
                    ));
                self.record_peer_sync_status(
                    responder_id,
                    responder_height.max(candidate_snapshot.height),
                    responder_tip_hash,
                    candidate_highest_qc_hash,
                    candidate_highest_qc_height,
                    qcs.iter()
                        .rev()
                        .take(CERTIFIED_SYNC_ANCHOR_LIMIT)
                        .map(|qc| SyncQcAnchor {
                            block_hash: qc.block_hash,
                            block_number: qc.block_number,
                        })
                        .collect(),
                );

                if !should_adopt_certified_snapshot(
                    &local_snapshot,
                    local_highest_qc_height,
                    &candidate_snapshot,
                    candidate_highest_qc_height,
                ) {
                    self.log_event(
                        "certified-sync-skipped",
                        format!(
                            "from validator {responder_id} tip {:02x?} height {}",
                            &candidate_snapshot.tip_hash[..4],
                            candidate_snapshot.height
                        ),
                    )?;
                    return Ok(false);
                }

                self.adopt_canonical_chain(candidate_chain, "certified-sync-applied")?;
                for qc in qcs {
                    let _ = self.import_quorum_certificate(qc)?;
                }
                for aggregate in service_aggregates {
                    let _ = self.import_service_aggregate(aggregate)?;
                }
                self.refresh_service_scores();
                self.update_metrics(|metrics| {
                    metrics.certified_sync_applied += 1;
                });
                self.clear_sync_repair_memory(responder_id);
                Ok(true)
            }
        }
    }

    fn sync_request_is_throttled(
        &self,
        validator_id: ValidatorId,
        known_height: u64,
        known_tip_hash: HashBytes,
    ) -> bool {
        let local_sync_view = self.local_sync_view();
        self.last_sync_request_served
            .get(&validator_id)
            .map(|served| {
                sync_request_should_throttle(
                    *served,
                    now_unix_millis(),
                    known_height,
                    known_tip_hash,
                    local_sync_view.height,
                    local_sync_view.tip_hash,
                    local_sync_view.highest_qc_height,
                    local_sync_view.highest_qc_hash,
                )
            })
            .unwrap_or(false)
    }

    fn record_sync_request_served(
        &mut self,
        validator_id: ValidatorId,
        known_height: u64,
        known_tip_hash: HashBytes,
    ) {
        let local_sync_view = self.local_sync_view();
        self.last_sync_request_served.insert(
            validator_id,
            ServedSyncRequest {
                served_at_unix_millis: now_unix_millis(),
                known_height,
                known_tip_hash,
                served_local_height: local_sync_view.height,
                served_local_tip_hash: local_sync_view.tip_hash,
                served_local_highest_qc_height: local_sync_view.highest_qc_height,
                served_local_highest_qc_hash: local_sync_view.highest_qc_hash,
            },
        );
    }

    fn sync_request_hint_for_peer(&self, validator_id: ValidatorId) -> (u64, HashBytes) {
        let force_full_snapshot = self.should_force_full_snapshot_for_peer(validator_id);
        if self.config.feature_flags.consensus_v2
            && !force_full_snapshot
            && !self.startup_sync_barrier
            && self.peer_status_is_ahead(validator_id)
        {
            if let Some(local_qc) = self.highest_local_qc() {
                if self.ledger.snapshot().height > local_qc.block_number {
                    return sync_request_known_state(
                        local_qc.block_number,
                        local_qc.block_hash,
                        false,
                    );
                }
            }
        }
        let local_sync_view = self.local_sync_view();
        sync_request_known_state(
            local_sync_view.height,
            local_sync_view.tip_hash,
            force_full_snapshot,
        )
    }

    fn should_force_full_snapshot_for_peer(&self, validator_id: ValidatorId) -> bool {
        sync_repair_should_force_full_snapshot(
            self.peer_sync_repair_failures
                .get(&validator_id)
                .copied()
                .unwrap_or(0),
        )
    }

    fn record_sync_repair_failure(&mut self, validator_id: ValidatorId) {
        *self
            .peer_sync_repair_failures
            .entry(validator_id)
            .or_default() += 1;
    }

    fn clear_sync_repair_memory(&mut self, validator_id: ValidatorId) {
        self.peer_sync_repair_failures.remove(&validator_id);
    }

    fn refresh_service_scores(&mut self) {
        let current_epoch = self
            .last_processed_slot
            .map(|slot| self.consensus.epoch_for_slot(slot))
            .unwrap_or(0);
        let validator_ids: Vec<_> = self
            .genesis
            .validators
            .iter()
            .map(|validator| validator.validator_id)
            .collect();
        let completed_epoch = current_epoch.saturating_sub(1);

        if current_epoch == 0 {
            for validator_id in validator_ids {
                self.latest_service_scores.insert(validator_id, 1.0);
                self.latest_service_counters
                    .insert(validator_id, ServiceCounters::default());
            }
            let scores = self.latest_service_scores.clone();
            let local_score = scores
                .get(&self.config.validator_id)
                .copied()
                .unwrap_or(1.0);
            let local_counters = self.local_service_counters();
            self.update_metrics(|metrics| {
                metrics.relay_scores = scores;
                metrics.last_local_service_score = local_score;
                metrics.service_gating_threshold = self.service_gating_threshold();
                metrics.last_completed_service_epoch = 0;
                metrics.service_gating_start_epoch =
                    self.config.feature_flags.service_gating_start_epoch;
                metrics.service_score_window_epochs =
                    self.config.feature_flags.service_score_window_epochs;
                metrics.service_score_weights =
                    self.config.feature_flags.service_score_weights.clone();
                metrics.last_local_service_counters = local_counters;
            });
            return;
        }

        if self.config.feature_flags.consensus_v2 {
            let mut local_service_epoch = 0;
            let previous_scores = self.latest_service_scores.clone();
            let previous_counters = self.latest_service_counters.clone();
            let previous_completed_epoch = self.snapshot_metrics().last_completed_service_epoch;
            for validator_id in validator_ids {
                let recent_confirmed =
                    match self.v2_service_evidence_state(validator_id, current_epoch) {
                        V2ServiceEvidenceState::RecentConfirmed {
                            latest_epoch,
                            counters,
                            score,
                        } => Some((latest_epoch, counters, score)),
                        V2ServiceEvidenceState::StaleConfirmed
                        | V2ServiceEvidenceState::NoEvidence => None,
                    };
                if let Some((aggregate_epoch, aggregate_counters, score)) = recent_confirmed {
                    self.latest_service_scores.insert(validator_id, score);
                    if validator_id == self.config.validator_id {
                        local_service_epoch = aggregate_epoch;
                    }
                    self.latest_service_counters
                        .insert(validator_id, aggregate_counters);
                } else {
                    self.latest_service_scores.insert(
                        validator_id,
                        previous_scores.get(&validator_id).copied().unwrap_or(1.0),
                    );
                    self.latest_service_counters.insert(
                        validator_id,
                        previous_counters
                            .get(&validator_id)
                            .cloned()
                            .unwrap_or_default(),
                    );
                    if validator_id == self.config.validator_id {
                        local_service_epoch = previous_completed_epoch;
                    }
                }
            }
            let scores = self.latest_service_scores.clone();
            let local_score = scores
                .get(&self.config.validator_id)
                .copied()
                .unwrap_or(0.0);
            let local_counters = self.local_service_counters();
            self.update_metrics(|metrics| {
                metrics.relay_scores = scores;
                metrics.last_local_service_score = local_score;
                metrics.service_gating_threshold = self.service_gating_threshold();
                metrics.last_completed_service_epoch = local_service_epoch;
                metrics.service_gating_start_epoch =
                    self.config.feature_flags.service_gating_start_epoch;
                metrics.service_score_window_epochs =
                    self.config.feature_flags.service_score_window_epochs;
                metrics.service_score_weights =
                    self.config.feature_flags.service_score_weights.clone();
                metrics.last_local_service_counters = local_counters;
            });
            return;
        }

        let window_epochs = self.config.feature_flags.service_score_window_epochs.max(1);
        let earliest_epoch = completed_epoch.saturating_sub(window_epochs.saturating_sub(1));
        self.prune_penalty_observations(earliest_epoch);
        for validator_id in validator_ids {
            let mut aggregate = ServiceCounters::default();
            for epoch in earliest_epoch..=completed_epoch {
                let failed_sessions = self.failed_sessions_for(validator_id, epoch);
                let invalid_receipts = self.invalid_receipts_for(validator_id, epoch);
                let counters = self.consensus.counters_for_validator(
                    validator_id,
                    epoch,
                    &self.receipts,
                    failed_sessions,
                    invalid_receipts,
                );
                let weight = epoch.saturating_sub(earliest_epoch) + 1;
                accumulate_weighted_counters(&mut aggregate, &counters, weight);
            }
            let score = self.consensus.compute_service_score_with_weights(
                &aggregate,
                &self.config.feature_flags.service_score_weights,
            );
            self.latest_service_scores.insert(validator_id, score);
            self.latest_service_counters.insert(validator_id, aggregate);
        }
        let scores = self.latest_service_scores.clone();
        let local_score = scores
            .get(&self.config.validator_id)
            .copied()
            .unwrap_or(1.0);
        let local_counters = self.local_service_counters();
        self.update_metrics(|metrics| {
            metrics.relay_scores = scores;
            metrics.last_local_service_score = local_score;
            metrics.service_gating_threshold = self.service_gating_threshold();
            metrics.last_completed_service_epoch = completed_epoch;
            metrics.service_gating_start_epoch =
                self.config.feature_flags.service_gating_start_epoch;
            metrics.service_score_window_epochs =
                self.config.feature_flags.service_score_window_epochs;
            metrics.service_score_weights = self.config.feature_flags.service_score_weights.clone();
            metrics.last_local_service_counters = local_counters;
        });
    }

    fn latest_service_aggregate_before_epoch(
        &self,
        validator_id: ValidatorId,
        current_epoch: Epoch,
    ) -> Option<&ServiceAggregate> {
        if current_epoch == 0 {
            return None;
        }
        self.service_aggregates
            .range((validator_id, 0)..=(validator_id, current_epoch - 1))
            .next_back()
            .map(|(_, aggregate)| aggregate)
    }

    fn weighted_recent_service_evidence_before_epoch(
        &self,
        validator_id: ValidatorId,
        current_epoch: Epoch,
    ) -> Option<(Epoch, ServiceCounters, f64)> {
        if current_epoch == 0 {
            return None;
        }
        let window_epochs = self.config.feature_flags.service_score_window_epochs.max(1);
        let first_epoch = current_epoch.saturating_sub(window_epochs);
        let mut aggregate_counters = ServiceCounters::default();
        let mut latest_epoch = None;
        for (_, aggregate) in self
            .service_aggregates
            .range((validator_id, first_epoch)..=(validator_id, current_epoch - 1))
        {
            if self
                .consensus
                .validate_service_aggregate(aggregate)
                .is_err()
            {
                continue;
            }
            let weight = aggregate.epoch.saturating_sub(first_epoch) + 1;
            accumulate_weighted_counters(
                &mut aggregate_counters,
                &aggregate.aggregate_counters,
                weight,
            );
            latest_epoch = Some(aggregate.epoch);
        }
        if latest_epoch.is_none() {
            let fallback_max_age = window_epochs.saturating_add(1);
            if let Some(fallback) =
                self.latest_service_aggregate_before_epoch(validator_id, current_epoch)
            {
                let fallback_age = current_epoch.saturating_sub(fallback.epoch);
                if fallback_age <= fallback_max_age
                    && self.consensus.validate_service_aggregate(fallback).is_ok()
                {
                    accumulate_weighted_counters(
                        &mut aggregate_counters,
                        &fallback.aggregate_counters,
                        1,
                    );
                    latest_epoch = Some(fallback.epoch);
                }
            }
        }
        let latest_epoch = latest_epoch?;
        let score = self.consensus.compute_service_score_with_weights(
            &aggregate_counters,
            &self.config.feature_flags.service_score_weights,
        );
        Some((latest_epoch, aggregate_counters, score))
    }

    fn v2_service_evidence_state(
        &self,
        validator_id: ValidatorId,
        current_epoch: Epoch,
    ) -> V2ServiceEvidenceState {
        if let Some((latest_epoch, counters, score)) =
            self.weighted_recent_service_evidence_before_epoch(validator_id, current_epoch)
        {
            return V2ServiceEvidenceState::RecentConfirmed {
                latest_epoch,
                counters,
                score,
            };
        }

        if self
            .latest_service_aggregate_before_epoch(validator_id, current_epoch)
            .is_some()
        {
            V2ServiceEvidenceState::StaleConfirmed
        } else {
            V2ServiceEvidenceState::NoEvidence
        }
    }

    fn v2_gating_state(&self, validator_id: ValidatorId, epoch: Epoch) -> V2GatingState {
        if epoch == 0 {
            return V2GatingState::AllowNoEvidence;
        }

        match self.v2_service_evidence_state(validator_id, epoch) {
            V2ServiceEvidenceState::RecentConfirmed { score, .. } => {
                if score < self.service_gating_threshold() {
                    V2GatingState::RejectScore(score)
                } else {
                    V2GatingState::AllowScore(score)
                }
            }
            V2ServiceEvidenceState::StaleConfirmed => V2GatingState::AllowInsufficientEvidence,
            V2ServiceEvidenceState::NoEvidence => V2GatingState::AllowNoEvidence,
        }
    }

    fn service_score_for_validator(&self, validator_id: ValidatorId) -> f64 {
        self.latest_service_scores
            .get(&validator_id)
            .copied()
            .unwrap_or(1.0)
    }

    fn local_service_counters(&self) -> ServiceCounters {
        self.latest_service_counters
            .get(&self.config.validator_id)
            .cloned()
            .unwrap_or_default()
    }

    fn current_epoch(&self) -> Epoch {
        self.last_processed_slot
            .map(|slot| self.consensus.epoch_for_slot(slot))
            .unwrap_or_else(|| self.consensus.epoch_for_slot(self.consensus.current_slot()))
    }

    fn service_gating_active(&self, epoch: Epoch) -> bool {
        epoch >= self.config.feature_flags.service_gating_start_epoch
    }

    fn service_gating_threshold(&self) -> f64 {
        self.config.feature_flags.service_gating_threshold
    }

    fn run_sync_maintenance(&mut self) -> Result<()> {
        self.broadcast_sync_request()?;
        self.broadcast_sync_status()?;
        self.push_sync_to_stale_peers()?;
        if !self.startup_sync_barrier {
            self.request_best_available_sync_from_ahead_peers()?;
        }
        Ok(())
    }

    fn broadcast_sync_request(&self) -> Result<()> {
        if self.startup_sync_barrier {
            if self.request_best_available_sync_from_ahead_peers()? {
                return Ok(());
            }
        }
        let local_sync_view = self.local_sync_view();
        self.network.broadcast_control(
            &self.config.peers,
            ProtocolMessage::SyncRequest {
                requester_id: self.config.validator_id,
                known_height: local_sync_view.height,
                known_tip_hash: local_sync_view.tip_hash,
            },
        )
    }

    fn broadcast_sync_status(&self) -> Result<()> {
        let local_sync_view = self.local_sync_view();
        self.network.broadcast_control(
            &self.config.peers,
            ProtocolMessage::SyncStatus {
                validator_id: self.config.validator_id,
                height: local_sync_view.height,
                tip_hash: local_sync_view.tip_hash,
                highest_qc_hash: local_sync_view.highest_qc_hash,
                highest_qc_height: local_sync_view.highest_qc_height,
                recent_qc_anchors: self.local_qc_anchors(CERTIFIED_SYNC_ANCHOR_LIMIT),
            },
        )
    }

    fn request_sync_from(&self, validator_id: ValidatorId) -> Result<()> {
        if self.config.feature_flags.consensus_v2 {
            return match self.preferred_v2_sync_mode_for_peer(validator_id) {
                V2SyncMode::Certified => self.request_certified_sync_from(validator_id),
                V2SyncMode::Legacy => self.request_legacy_sync_from(validator_id),
            };
        }
        self.request_legacy_sync_from(validator_id)
    }

    fn preferred_v2_sync_mode_for_peer(&self, validator_id: ValidatorId) -> V2SyncMode {
        let Some(status) = self.peer_sync_status.get(&validator_id) else {
            return if self.highest_local_qc().is_some() {
                V2SyncMode::Certified
            } else {
                V2SyncMode::Legacy
            };
        };
        let local_sync_view = self.local_sync_view();
        let local_height = local_sync_view.height;
        let local_tip_hash = local_sync_view.tip_hash;
        let local_qc_height = local_sync_view.highest_qc_height;
        let local_qc_hash = local_sync_view.highest_qc_hash;

        if status.highest_qc_height == 0 && local_qc_height == 0 {
            return V2SyncMode::Legacy;
        }
        if status.highest_qc_height > local_qc_height {
            return V2SyncMode::Certified;
        }
        if status.highest_qc_height == local_qc_height
            && status.highest_qc_hash == local_qc_hash
            && (status.height > local_height
                || (status.height == local_height && status.tip_hash != local_tip_hash))
        {
            return V2SyncMode::Legacy;
        }
        if status.highest_qc_height == local_qc_height && status.highest_qc_hash != local_qc_hash {
            return V2SyncMode::Certified;
        }
        if status.height > local_height
            || (status.height == local_height && status.tip_hash != local_tip_hash)
        {
            return V2SyncMode::Legacy;
        }
        if local_qc_height > 0 {
            V2SyncMode::Certified
        } else {
            V2SyncMode::Legacy
        }
    }

    fn peer_status_is_ahead(&self, validator_id: ValidatorId) -> bool {
        let Some(status) = self.peer_sync_status.get(&validator_id) else {
            return false;
        };
        let local_sync_view = self.local_sync_view();
        let local_height = local_sync_view.height;
        let local_tip_hash = local_sync_view.tip_hash;
        let local_qc_height = local_sync_view.highest_qc_height;
        let local_qc_hash = local_sync_view.highest_qc_hash;
        status.height > local_height
            || (status.height == local_height && status.tip_hash != local_tip_hash)
            || status.highest_qc_height > local_qc_height
            || (status.highest_qc_height == local_qc_height
                && status.highest_qc_hash != local_qc_hash)
    }

    fn startup_sync_blocks_proposal(&mut self) -> bool {
        if !self.startup_sync_barrier {
            return false;
        }
        if self.peer_sync_status.len() < self.config.peers.len() {
            return true;
        }
        if self
            .config
            .peers
            .iter()
            .any(|peer| self.peer_status_is_ahead(peer.validator_id))
        {
            return true;
        }
        self.startup_sync_barrier = false;
        false
    }

    fn request_legacy_sync_from(&self, validator_id: ValidatorId) -> Result<()> {
        let (known_height, known_tip_hash) = self.sync_request_hint_for_peer(validator_id);
        self.network.send_control_to(
            self.find_peer(validator_id)?,
            ProtocolMessage::SyncRequest {
                requester_id: self.config.validator_id,
                known_height,
                known_tip_hash,
            },
        )
    }

    fn request_certified_sync_from(&self, validator_id: ValidatorId) -> Result<()> {
        let peer = self.find_peer(validator_id)?;
        let highest_qc = self.highest_local_qc();
        self.network.send_control_to(
            peer,
            ProtocolMessage::CertifiedSyncRequest(ChunkedSyncRequest {
                requester_id: self.config.validator_id,
                known_qc_hash: highest_qc.map(|qc| qc.block_hash),
                known_qc_height: highest_qc.map(|qc| qc.block_number).unwrap_or(0),
                known_qc_anchors: self.local_qc_anchors(CERTIFIED_SYNC_ANCHOR_LIMIT),
                from_height: self.ledger.block_height(),
                want_certified_only: true,
            }),
        )
    }

    fn request_pending_certified_child_sync(&self, validator_id: ValidatorId) -> Result<()> {
        self.request_legacy_sync_from(validator_id)
    }

    fn maybe_repair_stale_pending_certified_child(&self, current_slot: u64) -> Result<bool> {
        let Some(pending_child) = self.preferred_pending_child_for_certified_head() else {
            return Ok(false);
        };
        if pending_child.header.slot >= current_slot {
            return Ok(false);
        }
        let proposer_id = pending_child.header.proposer_id;
        let mut requested = false;
        let mut excluded_peer = Some(self.config.validator_id);
        if proposer_id != self.config.validator_id && self.find_peer(proposer_id).is_ok() {
            self.request_legacy_sync_from(proposer_id)?;
            requested = true;
            excluded_peer = Some(proposer_id);
        }
        if self.request_best_available_sync_from_ahead_peers_excluding(excluded_peer)? {
            requested = true;
        }
        if requested {
            self.log_event(
                "pending-certified-repair-request",
                format!(
                    "slot {current_slot} pending_height {} pending_slot {}",
                    pending_child.header.block_number, pending_child.header.slot
                ),
            )?;
        }
        Ok(requested)
    }

    fn maybe_broadcast_recovery_sync_status(&mut self, current_slot: u64) -> Result<bool> {
        let local_view = self.local_sync_view();
        if self.last_recovery_sync_status.is_some_and(|memo| {
            memo.local_view == local_view
                && current_slot.saturating_sub(memo.slot) < RECOVERY_SYNC_RETRY_SLOTS
        }) {
            return Ok(false);
        }
        self.broadcast_sync_status()?;
        self.last_recovery_sync_status = Some(RecoverySyncStatusMemo {
            slot: current_slot,
            local_view,
        });
        Ok(true)
    }

    fn best_available_ahead_peer(&self) -> Option<ValidatorId> {
        let mut candidates = self
            .config
            .peers
            .iter()
            .filter_map(|peer| {
                let status = self.peer_sync_status.get(&peer.validator_id)?.clone();
                if !self.peer_status_is_ahead(peer.validator_id) {
                    return None;
                }
                Some((peer.validator_id, status))
            })
            .collect::<Vec<_>>();
        candidates.sort_by(|(left_id, left_status), (right_id, right_status)| {
            right_status
                .highest_qc_height
                .cmp(&left_status.highest_qc_height)
                .then(right_status.height.cmp(&left_status.height))
                .then(right_id.cmp(left_id))
        });
        candidates
            .into_iter()
            .next()
            .map(|(validator_id, _)| validator_id)
    }

    fn maybe_request_recovery_sync_from_ahead_peer(&mut self, current_slot: u64) -> Result<bool> {
        let Some(validator_id) = self.best_available_ahead_peer() else {
            self.last_recovery_sync_request = None;
            return Ok(false);
        };
        let local_view = self.local_sync_view();
        if self.last_recovery_sync_request.is_some_and(|memo| {
            memo.peer_validator_id == validator_id
                && memo.local_view == local_view
                && current_slot.saturating_sub(memo.slot) < RECOVERY_SYNC_RETRY_SLOTS
        }) {
            return Ok(false);
        }
        self.request_sync_from(validator_id)?;
        self.last_recovery_sync_request = Some(RecoverySyncRequestMemo {
            slot: current_slot,
            peer_validator_id: validator_id,
            local_view,
        });
        Ok(true)
    }

    fn request_best_available_sync_from_ahead_peers(&self) -> Result<bool> {
        self.request_best_available_sync_from_ahead_peers_excluding(None)
    }

    fn request_best_available_sync_from_ahead_peers_excluding(
        &self,
        excluded_validator_id: Option<ValidatorId>,
    ) -> Result<bool> {
        let mut candidates = self
            .config
            .peers
            .iter()
            .filter_map(|peer| {
                if excluded_validator_id == Some(peer.validator_id) {
                    return None;
                }
                let status = self.peer_sync_status.get(&peer.validator_id)?.clone();
                if !self.peer_status_is_ahead(peer.validator_id) {
                    return None;
                }
                Some((peer.validator_id, status))
            })
            .collect::<Vec<_>>();
        candidates.sort_by(|(left_id, left_status), (right_id, right_status)| {
            right_status
                .highest_qc_height
                .cmp(&left_status.highest_qc_height)
                .then(right_status.height.cmp(&left_status.height))
                .then(right_id.cmp(left_id))
        });
        let Some((validator_id, _)) = candidates.into_iter().next() else {
            return Ok(false);
        };
        self.request_sync_from(validator_id)?;
        Ok(true)
    }

    fn highest_local_qc(&self) -> Option<&QuorumCertificate> {
        self.blocks
            .iter()
            .rev()
            .find_map(|block| self.quorum_certificates.get(&block.block_hash))
    }

    fn local_qc_anchors(&self, limit: usize) -> Vec<SyncQcAnchor> {
        self.blocks
            .iter()
            .rev()
            .filter_map(|block| {
                self.quorum_certificates
                    .get(&block.block_hash)
                    .map(|qc| SyncQcAnchor {
                        block_hash: qc.block_hash,
                        block_number: qc.block_number,
                    })
            })
            .take(limit)
            .collect()
    }

    fn sync_push_state_for_peer(
        &self,
        validator_id: ValidatorId,
        known_height: u64,
        known_tip_hash: HashBytes,
    ) -> ServedSyncPush {
        let peer_status =
            self.peer_sync_status
                .get(&validator_id)
                .cloned()
                .unwrap_or(PeerSyncStatus {
                    height: known_height,
                    tip_hash: known_tip_hash,
                    highest_qc_hash: None,
                    highest_qc_height: 0,
                    recent_qc_anchors: Vec::new(),
                });
        let local_sync_view = self.local_sync_view();
        ServedSyncPush {
            peer_height: peer_status.height,
            peer_tip_hash: peer_status.tip_hash,
            peer_highest_qc_hash: peer_status.highest_qc_hash,
            peer_highest_qc_height: peer_status.highest_qc_height,
            local_height: local_sync_view.height,
            local_tip_hash: local_sync_view.tip_hash,
            local_highest_qc_hash: local_sync_view.highest_qc_hash,
            local_highest_qc_height: local_sync_view.highest_qc_height,
        }
    }

    fn record_sync_push_served(&mut self, validator_id: ValidatorId, state: ServedSyncPush) {
        self.last_sync_push_served.insert(validator_id, state);
    }

    fn push_best_sync_to(
        &mut self,
        validator_id: ValidatorId,
        known_height: u64,
        known_tip_hash: HashBytes,
    ) -> Result<()> {
        let state = self.sync_push_state_for_peer(validator_id, known_height, known_tip_hash);
        if self.last_sync_push_served.get(&validator_id) == Some(&state) {
            return Ok(());
        }
        if self.config.feature_flags.consensus_v2 {
            if let Some(shared_anchor) = self.send_best_certified_sync_to(validator_id)? {
                self.send_anchored_suffix_sync_to(validator_id, shared_anchor)?;
                self.record_sync_push_served(validator_id, state);
                return Ok(());
            }
        }
        self.send_best_sync_to(validator_id, known_height, known_tip_hash)?;
        self.record_sync_push_served(validator_id, state);
        Ok(())
    }

    fn send_best_certified_sync_to(
        &self,
        validator_id: ValidatorId,
    ) -> Result<Option<SyncQcAnchor>> {
        let Some(status) = self.peer_sync_status.get(&validator_id).cloned() else {
            return Ok(None);
        };
        let peer_qc_anchors = if status.recent_qc_anchors.is_empty() {
            status
                .highest_qc_hash
                .zip((status.highest_qc_height > 0).then_some(status.highest_qc_height))
                .map(|(block_hash, block_number)| {
                    vec![SyncQcAnchor {
                        block_hash,
                        block_number,
                    }]
                })
                .unwrap_or_default()
        } else {
            status.recent_qc_anchors.clone()
        };
        if peer_qc_anchors.is_empty() {
            return Ok(None);
        }
        let Some(shared_anchor) = self.highest_shared_qc_anchor(&peer_qc_anchors) else {
            return Ok(None);
        };
        let response = self.build_certified_sync_response_for_peer(&peer_qc_anchors);
        let ChunkedSyncResponse::Certified { .. } = &response else {
            return Ok(None);
        };
        self.update_metrics(|metrics| {
            metrics.certified_sync_served += 1;
        });
        self.network.send_control_to(
            self.find_peer(validator_id)?,
            ProtocolMessage::CertifiedSyncResponse(response),
        )?;
        Ok(Some(shared_anchor))
    }

    fn send_anchored_suffix_sync_to(
        &self,
        validator_id: ValidatorId,
        shared_anchor: SyncQcAnchor,
    ) -> Result<()> {
        let local_sync_view = self.local_sync_view();
        if local_sync_view.height <= shared_anchor.block_number {
            return Ok(());
        }
        let prefix_len = self.sync_export_prefix_len();
        let Some(chain) = build_chain_segment_from_chain(
            &self.sync_export_snapshot()?,
            &self.blocks[..prefix_len],
            &self.receipts,
            self.config.feature_flags.service_score_window_epochs,
            shared_anchor.block_number,
            shared_anchor.block_hash,
            MAX_INCREMENTAL_SYNC_BLOCKS,
        ) else {
            return Ok(());
        };
        self.update_metrics(|metrics| {
            metrics.incremental_sync_served += 1;
        });
        self.network.send_control_to(
            self.find_peer(validator_id)?,
            ProtocolMessage::SyncBlocks {
                responder_id: self.config.validator_id,
                chain,
            },
        )
    }

    fn send_best_sync_to(
        &self,
        validator_id: ValidatorId,
        known_height: u64,
        known_tip_hash: HashBytes,
    ) -> Result<()> {
        let peer = self.find_peer(validator_id)?;
        if let Some(chain) = self.build_chain_segment(known_height, known_tip_hash)? {
            self.update_metrics(|metrics| {
                metrics.incremental_sync_served += 1;
            });
            return self.network.send_control_to(
                peer,
                ProtocolMessage::SyncBlocks {
                    responder_id: self.config.validator_id,
                    chain,
                },
            );
        }

        self.send_full_snapshot_to(peer)
    }

    fn send_full_snapshot_to(&self, peer: PeerConfig) -> Result<()> {
        let chain = self.build_chain_snapshot()?;
        self.update_metrics(|metrics| {
            metrics.full_sync_served += 1;
        });
        self.network.send_control_to(
            peer,
            ProtocolMessage::SyncResponse {
                responder_id: self.config.validator_id,
                chain,
            },
        )
    }

    fn build_certified_sync_response(&self, request: ChunkedSyncRequest) -> ChunkedSyncResponse {
        if !self.config.feature_flags.consensus_v2 || !request.want_certified_only {
            return ChunkedSyncResponse::Unavailable {
                responder_id: self.config.validator_id,
            };
        }
        let peer_qc_anchors = if request.known_qc_anchors.is_empty() {
            request
                .known_qc_hash
                .zip((request.known_qc_height > 0).then_some(request.known_qc_height))
                .map(|(block_hash, block_number)| {
                    vec![SyncQcAnchor {
                        block_hash,
                        block_number,
                    }]
                })
                .unwrap_or_default()
        } else {
            request.known_qc_anchors
        };
        self.build_certified_sync_response_for_peer(&peer_qc_anchors)
    }

    fn build_certified_sync_response_for_peer(
        &self,
        peer_qc_anchors: &[SyncQcAnchor],
    ) -> ChunkedSyncResponse {
        let Some(shared_qc_anchor) = self.highest_shared_qc_anchor(peer_qc_anchors) else {
            return ChunkedSyncResponse::Unavailable {
                responder_id: self.config.validator_id,
            };
        };
        let shared_qc_hash = shared_qc_anchor.block_hash;
        let Some(shared_qc_block) = self
            .blocks
            .iter()
            .find(|block| block.block_hash == shared_qc_hash)
        else {
            return ChunkedSyncResponse::Unavailable {
                responder_id: self.config.validator_id,
            };
        };
        if !self.quorum_certificates.contains_key(&shared_qc_hash) {
            return ChunkedSyncResponse::Unavailable {
                responder_id: self.config.validator_id,
            };
        }
        let Some((blocks, qcs, service_aggregates)) =
            self.certified_suffix_from_shared_qc(shared_qc_hash)
        else {
            return ChunkedSyncResponse::Unavailable {
                responder_id: self.config.validator_id,
            };
        };
        if blocks.is_empty() {
            return ChunkedSyncResponse::Unavailable {
                responder_id: self.config.validator_id,
            };
        }
        let headers = blocks
            .iter()
            .zip(qcs.iter())
            .map(|(block, qc)| CertifiedBlockHeader {
                header: block.header.clone(),
                block_hash: block.block_hash,
                quorum_certificate: Some(qc.clone()),
                prior_service_aggregate: None,
            })
            .collect();

        ChunkedSyncResponse::Certified {
            responder_id: self.config.validator_id,
            responder_height: self.local_sync_view().height,
            responder_tip_hash: self.local_sync_view().tip_hash,
            shared_qc_hash,
            shared_qc_height: shared_qc_block.header.block_number,
            headers,
            blocks,
            qcs,
            service_aggregates,
        }
    }

    fn highest_shared_qc_anchor(&self, peer_qc_anchors: &[SyncQcAnchor]) -> Option<SyncQcAnchor> {
        let mut anchors = peer_qc_anchors.to_vec();
        anchors.sort_by(|left, right| {
            right
                .block_number
                .cmp(&left.block_number)
                .then_with(|| right.block_hash.cmp(&left.block_hash))
        });
        anchors.into_iter().find(|anchor| {
            self.blocks
                .iter()
                .any(|block| block.block_hash == anchor.block_hash)
                && self.quorum_certificates.contains_key(&anchor.block_hash)
        })
    }

    fn certified_suffix_from_shared_qc(
        &self,
        shared_qc_hash: HashBytes,
    ) -> Option<(Vec<Block>, Vec<QuorumCertificate>, Vec<ServiceAggregate>)> {
        let shared_index = self
            .blocks
            .iter()
            .position(|block| block.block_hash == shared_qc_hash)?;
        let mut blocks = Vec::new();
        let mut qcs = Vec::new();
        for block in self.blocks.iter().skip(shared_index + 1) {
            let Some(qc) = self.quorum_certificates.get(&block.block_hash) else {
                break;
            };
            blocks.push(block.clone());
            qcs.push(qc.clone());
        }
        if blocks.is_empty() {
            return None;
        }
        let first_epoch = blocks.first().map(|block| block.header.epoch).unwrap_or(0);
        let last_epoch = blocks.last().map(|block| block.header.epoch).unwrap_or(0);
        let earliest_epoch =
            first_epoch.saturating_sub(self.config.feature_flags.service_score_window_epochs);
        let service_aggregates = self
            .service_aggregates
            .values()
            .filter(|aggregate| aggregate.epoch >= earliest_epoch && aggregate.epoch <= last_epoch)
            .cloned()
            .collect();
        Some((blocks, qcs, service_aggregates))
    }

    fn build_chain_snapshot(&self) -> Result<ChainSnapshot> {
        let prefix_len = self.sync_export_prefix_len();
        Ok(ChainSnapshot {
            snapshot: self.sync_export_snapshot()?,
            blocks: self.blocks[..prefix_len].to_vec(),
            receipts: self.receipts.clone(),
        })
    }

    fn build_chain_segment(
        &self,
        known_height: u64,
        known_tip_hash: HashBytes,
    ) -> Result<Option<ChainSegment>> {
        let prefix_len = self.sync_export_prefix_len();
        let preferred_max_blocks = if self.config.feature_flags.consensus_v2
            && self
                .highest_local_qc()
                .map(|qc| {
                    known_height == qc.block_number
                        && known_tip_hash == qc.block_hash
                        && prefix_len > qc.block_number as usize
                })
                .unwrap_or(false)
        {
            MAX_INCREMENTAL_SYNC_BLOCKS
        } else {
            MAX_PREFERRED_INCREMENTAL_SYNC_BLOCKS
        };
        let Some(mut chain) = build_chain_segment_from_chain(
            &self.sync_export_snapshot()?,
            &self.blocks[..prefix_len],
            &self.receipts,
            self.config.feature_flags.service_score_window_epochs,
            known_height,
            known_tip_hash,
            preferred_max_blocks,
        ) else {
            return Ok(None);
        };
        let missing_hashes = chain
            .blocks
            .iter()
            .map(|block| block.block_hash)
            .collect::<BTreeSet<_>>();
        chain.proposal_votes = self
            .proposal_votes
            .iter()
            .filter(|(block_hash, _)| missing_hashes.contains(*block_hash))
            .flat_map(|(_, by_validator)| by_validator.values().cloned())
            .collect();
        Ok(Some(chain))
    }

    fn push_sync_to_stale_peers(&mut self) -> Result<()> {
        let local_sync_view = self.local_sync_view();
        let peers = self.config.peers.clone();
        for peer in peers {
            if let Some(status) = self.peer_sync_status.get(&peer.validator_id).cloned() {
                if local_sync_view.height > status.height
                    || (local_sync_view.height == status.height
                        && local_sync_view.tip_hash != status.tip_hash)
                {
                    self.push_best_sync_to(peer.validator_id, status.height, status.tip_hash)?;
                }
            } else if local_sync_view.height > 0 {
                self.send_full_snapshot_to(peer.clone())?;
            }
        }
        Ok(())
    }

    fn maybe_push_tip_progress_to_stale_peers(&mut self) -> Result<()> {
        if !self.config.feature_flags.consensus_v2 {
            return Ok(());
        }
        let local_sync_view = self.local_sync_view();
        let has_known_stale_peer = self.peer_sync_status.iter().any(|(validator_id, status)| {
            *validator_id != self.config.validator_id
                && (local_sync_view.height > status.height
                    || (local_sync_view.height == status.height
                        && local_sync_view.tip_hash != status.tip_hash))
        });
        if !has_known_stale_peer {
            return Ok(());
        }
        self.broadcast_sync_status()?;
        self.push_sync_to_stale_peers()
    }

    fn announce_certified_progress(&mut self) -> Result<()> {
        self.broadcast_sync_status()?;
        self.push_sync_to_stale_peers()?;
        self.request_best_available_sync_from_ahead_peers()?;
        Ok(())
    }

    fn record_peer_sync_status(
        &mut self,
        validator_id: ValidatorId,
        height: u64,
        tip_hash: HashBytes,
        highest_qc_hash: Option<HashBytes>,
        highest_qc_height: u64,
        recent_qc_anchors: Vec<SyncQcAnchor>,
    ) {
        if !should_record_peer_sync_status(height, tip_hash) {
            return;
        }
        let existing = self.peer_sync_status.get(&validator_id).cloned();
        let restart_frontier_update = existing
            .as_ref()
            .map(|status| {
                status.height > height
                    && highest_qc_height > 0
                    && highest_qc_height == height
                    && highest_qc_hash == Some(tip_hash)
            })
            .unwrap_or(false);
        if restart_frontier_update {
            let recent_qc_anchors = if recent_qc_anchors.is_empty() {
                vec![SyncQcAnchor {
                    block_hash: tip_hash,
                    block_number: height,
                }]
            } else {
                recent_qc_anchors
            };
            self.peer_sync_status.insert(
                validator_id,
                PeerSyncStatus {
                    height,
                    tip_hash,
                    highest_qc_hash,
                    highest_qc_height,
                    recent_qc_anchors,
                },
            );
            return;
        }
        let (height, tip_hash) = match existing.as_ref() {
            Some(status) if status.height > height => (status.height, status.tip_hash),
            _ => (height, tip_hash),
        };
        let (highest_qc_hash, highest_qc_height) = match existing.as_ref() {
            Some(status) if status.highest_qc_height > highest_qc_height => {
                (status.highest_qc_hash, status.highest_qc_height)
            }
            _ => (highest_qc_hash, highest_qc_height),
        };
        let recent_qc_anchors = merge_sync_qc_anchors(
            existing
                .as_ref()
                .map(|status| status.recent_qc_anchors.as_slice())
                .unwrap_or(&[]),
            &recent_qc_anchors,
            CERTIFIED_SYNC_ANCHOR_LIMIT,
        );
        self.peer_sync_status.insert(
            validator_id,
            PeerSyncStatus {
                height,
                tip_hash,
                highest_qc_hash,
                highest_qc_height,
                recent_qc_anchors,
            },
        );
    }

    fn allow_peer_message(
        &mut self,
        validator_id: ValidatorId,
        message_class: PeerMessageClass,
        now_unix_millis: u64,
    ) -> bool {
        let window = self.peer_message_windows.entry(validator_id).or_default();
        allow_peer_message_in_window(window, message_class, now_unix_millis)
    }

    fn snapshot_metrics(&self) -> NodeMetrics {
        self.metrics
            .lock()
            .map(|metrics| metrics.clone())
            .unwrap_or_else(|_| NodeMetrics::default())
    }

    fn update_metrics(&self, update: impl FnOnce(&mut NodeMetrics)) {
        if let Ok(mut metrics) = self.metrics.lock() {
            update(&mut metrics);
            metrics.last_updated_unix_millis = now_unix_millis();
        }
    }

    fn local_sync_view(&self) -> LocalSyncView {
        let local_snapshot = self.ledger.snapshot();
        let local_qc = self.highest_local_qc();
        let raw_view = LocalSyncView {
            height: local_snapshot.height,
            tip_hash: local_snapshot.tip_hash,
            highest_qc_hash: local_qc.map(|qc| qc.block_hash),
            highest_qc_height: local_qc.map(|qc| qc.block_number).unwrap_or(0),
        };
        if !self.startup_sync_barrier {
            return raw_view;
        }
        let Some(qc) = local_qc else {
            return raw_view;
        };
        if raw_view.height <= qc.block_number {
            return raw_view;
        }
        LocalSyncView {
            height: qc.block_number,
            tip_hash: qc.block_hash,
            highest_qc_hash: Some(qc.block_hash),
            highest_qc_height: qc.block_number,
        }
    }

    fn sync_export_prefix_len(&self) -> usize {
        self.local_sync_view().height.min(self.blocks.len() as u64) as usize
    }

    fn sync_export_snapshot(&self) -> Result<StateSnapshot> {
        let prefix_len = self.sync_export_prefix_len();
        if prefix_len == self.blocks.len() {
            return Ok(self.ledger.snapshot().clone());
        }
        let ledger = LedgerState::replay_blocks(
            &self.genesis,
            &self.blocks[..prefix_len],
            self.crypto.as_ref(),
        )?;
        Ok(ledger.snapshot().clone())
    }

    fn persist_metrics(&self) -> Result<()> {
        self.storage.write_metrics(&self.snapshot_metrics())
    }

    fn find_peer(&self, validator_id: ValidatorId) -> Result<PeerConfig> {
        self.config
            .peers
            .iter()
            .find(|peer| peer.validator_id == validator_id)
            .cloned()
            .ok_or_else(|| anyhow!("unknown peer {validator_id}"))
    }

    fn rebuild_seen_sets(&mut self) {
        self.seen_blocks = self.blocks.iter().map(|block| block.block_hash).collect();
        self.seen_receipts = self.receipts.iter().map(canonical_hash).collect();
        self.seen_receipt_events = self.receipts.iter().map(receipt_event_hash).collect();
        self.seen_service_attestations = self
            .service_attestations
            .values()
            .flat_map(|by_member| by_member.values().map(service_attestation_signing_hash))
            .collect();
        self.seen_service_aggregates = self
            .service_aggregates
            .values()
            .map(canonical_hash)
            .collect();
        for block in &self.blocks {
            for transaction in &block.transactions {
                self.seen_transactions.insert(transaction.tx_hash);
            }
        }
    }

    fn restore_service_evidence(
        &mut self,
        attestations: Vec<ServiceAttestation>,
        aggregates: Vec<ServiceAggregate>,
    ) -> Result<()> {
        if !self.config.feature_flags.consensus_v2 {
            return Ok(());
        }
        for attestation in attestations {
            self.validate_service_attestation(&attestation)?;
            self.insert_service_attestation(attestation, false)?;
        }
        for aggregate in aggregates {
            self.validate_service_aggregate(&aggregate)?;
            self.merge_service_aggregate(aggregate, false)?;
        }
        Ok(())
    }

    fn emit_service_attestations_for_completed_epoch(&mut self, epoch: Epoch) -> Result<()> {
        if !self.config.feature_flags.consensus_v2 {
            return Ok(());
        }

        let validator_ids: Vec<_> = self
            .genesis
            .validators
            .iter()
            .map(|validator| validator.validator_id)
            .collect();
        for subject_validator_id in validator_ids {
            if subject_validator_id == self.config.validator_id {
                continue;
            }
            let committee = self
                .consensus
                .service_committee_for(epoch, subject_validator_id);
            if !committee.contains(&self.config.validator_id) {
                continue;
            }
            let attestation = self.build_local_service_attestation(subject_validator_id, epoch)?;
            if self.import_service_attestation(attestation.clone())? {
                self.update_metrics(|metrics| {
                    metrics.service_attestations_emitted += 1;
                });
                let aggregators = self
                    .consensus
                    .service_aggregators_for(epoch, subject_validator_id);
                for peer in self
                    .config
                    .peers
                    .iter()
                    .filter(|peer| aggregators.contains(&peer.validator_id))
                {
                    self.network.send_control_to(
                        peer.clone(),
                        ProtocolMessage::ServiceAttestation(attestation.clone()),
                    )?;
                }
            }
            self.maybe_publish_service_aggregate(subject_validator_id, epoch)?;
        }
        Ok(())
    }

    fn refresh_recent_service_attestations(&mut self, current_epoch: Epoch) -> Result<()> {
        if !self.config.feature_flags.consensus_v2 || current_epoch <= 2 {
            return Ok(());
        }
        let backfill_epochs = self.config.feature_flags.service_score_window_epochs.max(1);
        let last_attestable_epoch = current_epoch.saturating_sub(3);
        let first_attestable_epoch =
            last_attestable_epoch.saturating_sub(backfill_epochs.saturating_sub(1));
        for epoch in first_attestable_epoch..=last_attestable_epoch {
            self.emit_service_attestations_for_completed_epoch(epoch)?;
        }
        Ok(())
    }

    fn build_local_service_attestation(
        &self,
        subject_validator_id: ValidatorId,
        epoch: Epoch,
    ) -> Result<ServiceAttestation> {
        let counters = self.consensus.counters_for_validator_from_observer(
            subject_validator_id,
            self.config.validator_id,
            epoch,
            &self.receipts,
        );
        let mut attestation = ServiceAttestation {
            subject_validator_id,
            committee_member_id: self.config.validator_id,
            epoch,
            counters,
            signature: TypedSignature::default(),
        };
        let hash = service_attestation_signing_hash(&attestation);
        attestation.signature = self.crypto.sign(self.config.validator_id, &hash)?;
        Ok(attestation)
    }

    fn import_service_attestation(&mut self, attestation: ServiceAttestation) -> Result<bool> {
        if !self.config.feature_flags.consensus_v2 {
            return Err(anyhow!("service attestations require consensus_v2"));
        }
        self.validate_service_attestation(&attestation)?;
        self.store_service_attestation(attestation)
    }

    fn validate_service_attestation(&self, attestation: &ServiceAttestation) -> Result<()> {
        let committee = self
            .consensus
            .service_committee_for(attestation.epoch, attestation.subject_validator_id);
        if !committee.contains(&attestation.committee_member_id) {
            return Err(anyhow!(
                "service attestation signer is not in the service committee"
            ));
        }
        let hash = service_attestation_signing_hash(attestation);
        let verified = self.crypto.verify(
            attestation.committee_member_id,
            &hash,
            &attestation.signature,
        )?;
        if !verified {
            return Err(anyhow!("invalid service attestation signature"));
        }
        Ok(())
    }

    fn store_service_attestation(&mut self, attestation: ServiceAttestation) -> Result<bool> {
        self.insert_service_attestation(attestation, true)
    }

    fn insert_service_attestation(
        &mut self,
        attestation: ServiceAttestation,
        persist: bool,
    ) -> Result<bool> {
        let key = (attestation.subject_validator_id, attestation.epoch);
        let attestation_id = service_attestation_signing_hash(&attestation);
        if self.seen_service_attestations.contains(&attestation_id) {
            return Ok(false);
        }
        let by_member = self.service_attestations.entry(key).or_default();
        if let Some(existing) = by_member.get(&attestation.committee_member_id) {
            if service_attestation_signing_hash(existing) == attestation_id {
                return Ok(false);
            }
            if service_counters_dominate(&existing.counters, &attestation.counters) {
                return Ok(false);
            }
            if !service_counters_dominate(&attestation.counters, &existing.counters) {
                return Err(anyhow!("conflicting service attestation"));
            }
        }
        by_member.insert(attestation.committee_member_id, attestation.clone());
        self.seen_service_attestations.insert(attestation_id);
        if persist {
            self.storage
                .append_json_line(&self.storage.service_attestations_path, &attestation)?;
        }
        Ok(true)
    }

    fn maybe_publish_service_aggregate(
        &mut self,
        subject_validator_id: ValidatorId,
        epoch: Epoch,
    ) -> Result<bool> {
        if !self.config.feature_flags.consensus_v2 {
            return Ok(false);
        }
        if !self.consensus.is_service_aggregator_for(
            epoch,
            subject_validator_id,
            self.config.validator_id,
        ) {
            return Ok(false);
        }
        let Some(aggregate) = self.build_service_aggregate(subject_validator_id, epoch)? else {
            return Ok(false);
        };
        if self.import_service_aggregate(aggregate.clone())? {
            self.update_metrics(|metrics| {
                metrics.service_aggregates_published += 1;
            });
            self.refresh_service_scores();
            self.network.broadcast(
                &self.config.peers,
                ProtocolMessage::ServiceAggregate(aggregate),
            )?;
            return Ok(true);
        }
        Ok(false)
    }

    fn build_service_aggregate(
        &self,
        subject_validator_id: ValidatorId,
        epoch: Epoch,
    ) -> Result<Option<ServiceAggregate>> {
        if !self.config.feature_flags.consensus_v2 {
            return Ok(None);
        }
        let Some(by_member) = self
            .service_attestations
            .get(&(subject_validator_id, epoch))
        else {
            return Ok(None);
        };
        let attestations = by_member.values().cloned().collect::<Vec<_>>();
        let aggregate = ServiceAggregate {
            subject_validator_id,
            epoch,
            attestation_root: entangrid_types::service_attestation_root(&attestations),
            aggregate_counters: entangrid_types::aggregate_service_counters(&attestations),
            attestations,
        };
        if self
            .consensus
            .validate_service_aggregate(&aggregate)
            .is_err()
        {
            return Ok(None);
        }
        Ok(Some(aggregate))
    }

    fn import_service_aggregate(&mut self, aggregate: ServiceAggregate) -> Result<bool> {
        if !self.config.feature_flags.consensus_v2 {
            return Err(anyhow!("service aggregates require consensus_v2"));
        }
        self.validate_service_aggregate(&aggregate)?;
        self.merge_service_aggregate(aggregate, true)
    }

    fn validate_service_aggregate(&self, aggregate: &ServiceAggregate) -> Result<()> {
        self.consensus
            .validate_service_aggregate(aggregate)
            .map_err(|error| anyhow!(error))?;
        for attestation in &aggregate.attestations {
            self.validate_service_attestation(attestation)?;
        }
        Ok(())
    }

    fn merge_service_aggregate(
        &mut self,
        aggregate: ServiceAggregate,
        persist: bool,
    ) -> Result<bool> {
        let subject_validator_id = aggregate.subject_validator_id;
        let epoch = aggregate.epoch;
        let mut changed = false;
        for attestation in aggregate.attestations {
            changed |= self.insert_service_attestation(attestation, persist)?;
        }
        let Some(canonical) = self.build_service_aggregate(subject_validator_id, epoch)? else {
            return Ok(changed);
        };
        changed |= self.insert_service_aggregate(canonical, persist)?;
        Ok(changed)
    }

    fn insert_service_aggregate(
        &mut self,
        aggregate: ServiceAggregate,
        persist: bool,
    ) -> Result<bool> {
        let key = (aggregate.subject_validator_id, aggregate.epoch);
        let aggregate_id = canonical_hash(&aggregate);
        if self.seen_service_aggregates.contains(&aggregate_id) {
            return Ok(false);
        }
        if let Some(existing) = self.service_aggregates.get(&key) {
            if canonical_hash(existing) == aggregate_id {
                return Ok(false);
            }
        }
        self.service_aggregates.insert(key, aggregate.clone());
        self.seen_service_aggregates.insert(aggregate_id);
        if persist {
            self.storage
                .append_json_line(&self.storage.service_aggregates_path, &aggregate)?;
        }
        Ok(true)
    }

    fn record_failed_session(
        &mut self,
        epoch: Epoch,
        validator_id: ValidatorId,
        peer_validator_id: ValidatorId,
    ) {
        if !should_record_failed_session(
            &self.consensus,
            &self.known_live_peers,
            &self.observed_successful_sessions,
            epoch,
            validator_id,
            peer_validator_id,
        ) {
            return;
        }
        self.observed_failed_sessions
            .insert((epoch, validator_id, peer_validator_id));
    }

    fn record_successful_session(
        &mut self,
        epoch: Epoch,
        validator_id: ValidatorId,
        peer_validator_id: ValidatorId,
    ) {
        if !self
            .consensus
            .is_relay_target_for(validator_id, peer_validator_id, epoch)
        {
            return;
        }
        self.observed_successful_sessions
            .insert((epoch, validator_id, peer_validator_id));
        self.observed_failed_sessions
            .remove(&(epoch, validator_id, peer_validator_id));
    }

    fn record_invalid_receipt(&mut self, epoch: Epoch, validator_id: ValidatorId) {
        *self
            .observed_invalid_receipts
            .entry((epoch, validator_id))
            .or_default() += 1;
    }

    fn failed_sessions_for(&self, validator_id: ValidatorId, epoch: Epoch) -> u64 {
        self.observed_failed_sessions
            .iter()
            .filter(|(recorded_epoch, recorded_validator_id, _)| {
                *recorded_epoch == epoch && *recorded_validator_id == validator_id
            })
            .count() as u64
    }

    fn invalid_receipts_for(&self, validator_id: ValidatorId, epoch: Epoch) -> u64 {
        self.observed_invalid_receipts
            .get(&(epoch, validator_id))
            .copied()
            .unwrap_or(0)
    }

    fn prune_penalty_observations(&mut self, earliest_epoch: Epoch) {
        self.observed_failed_sessions
            .retain(|(epoch, _, _)| *epoch >= earliest_epoch);
        self.observed_successful_sessions
            .retain(|(epoch, _, _)| *epoch >= earliest_epoch);
        self.observed_invalid_receipts
            .retain(|(epoch, _), _| *epoch >= earliest_epoch);
    }

    fn log_event(&self, event: impl Into<String>, detail: impl Into<String>) -> Result<()> {
        let entry = EventLogEntry {
            timestamp_unix_millis: now_unix_millis(),
            event: event.into(),
            detail: detail.into(),
        };
        self.storage
            .append_json_line(&self.storage.log_path, &entry)
    }

    fn log_current_service_score(&self, epoch: Epoch) -> Result<()> {
        let counters = self.local_service_counters();
        let v2_completed_epoch = self.snapshot_metrics().last_completed_service_epoch;
        if epoch < self.config.feature_flags.service_gating_start_epoch {
            return self.log_event(
                "service-score",
                format!(
                    "epoch {epoch} warmup until epoch {} local_score {:.3} threshold {:.3} window {} weights [{:.2},{:.2},{:.2},-{:.2}] uptime {}/{} timely {}/{} peers {}/{} failed_sessions {} invalid_receipts {}",
                    self.config.feature_flags.service_gating_start_epoch,
                    self.service_score_for_validator(self.config.validator_id),
                    self.service_gating_threshold(),
                    self.config.feature_flags.service_score_window_epochs,
                    self.config.feature_flags.service_score_weights.uptime_weight,
                    self.config.feature_flags.service_score_weights.delivery_weight,
                    self.config.feature_flags.service_score_weights.diversity_weight,
                    self.config.feature_flags.service_score_weights.penalty_weight,
                    counters.uptime_windows,
                    counters.total_windows,
                    counters.timely_deliveries,
                    counters.expected_deliveries,
                    counters.distinct_peers,
                    counters.expected_peers,
                    counters.failed_sessions,
                    counters.invalid_receipts
                ),
            );
        }

        self.log_event(
            "service-score",
            format!(
                "epoch {epoch} completed_epoch {} local_score {:.3} threshold {:.3} window {} weights [{:.2},{:.2},{:.2},-{:.2}] uptime {}/{} timely {}/{} peers {}/{} failed_sessions {} invalid_receipts {}",
                if self.config.feature_flags.consensus_v2 {
                    v2_completed_epoch
                } else {
                    epoch.saturating_sub(1)
                },
                self.service_score_for_validator(self.config.validator_id),
                self.service_gating_threshold(),
                self.config.feature_flags.service_score_window_epochs,
                self.config.feature_flags.service_score_weights.uptime_weight,
                self.config.feature_flags.service_score_weights.delivery_weight,
                self.config.feature_flags.service_score_weights.diversity_weight,
                self.config.feature_flags.service_score_weights.penalty_weight,
                counters.uptime_windows,
                counters.total_windows,
                counters.timely_deliveries,
                counters.expected_deliveries,
                counters.distinct_peers,
                counters.expected_peers,
                counters.failed_sessions,
                counters.invalid_receipts
            ),
        )
    }
}

fn receipt_signing_hash(receipt: &RelayReceipt) -> HashBytes {
    let mut unsigned = receipt.clone();
    unsigned.signature = TypedSignature::default();
    canonical_hash(&unsigned)
}

fn service_attestation_signing_hash(attestation: &ServiceAttestation) -> HashBytes {
    let mut unsigned = attestation.clone();
    unsigned.signature = TypedSignature::default();
    canonical_hash(&unsigned)
}

fn proposal_vote_signing_hash(vote: &ProposalVote) -> HashBytes {
    let mut unsigned = vote.clone();
    unsigned.signature = TypedSignature::default();
    canonical_hash(&unsigned)
}

fn receipt_event_hash(receipt: &RelayReceipt) -> HashBytes {
    canonical_hash(&(
        receipt.epoch,
        receipt.slot,
        receipt.source_validator_id,
        receipt.destination_validator_id,
        receipt.witness_validator_id,
        receipt.message_class.clone(),
        receipt.sequence_number,
    ))
}

fn pending_slots_to_process(last_processed_slot: Option<u64>, current_slot: u64) -> Vec<u64> {
    match last_processed_slot {
        None => (0..=current_slot).collect(),
        Some(previous_slot) if previous_slot >= current_slot => vec![current_slot],
        Some(previous_slot) => ((previous_slot + 1)..=current_slot).collect(),
    }
}

fn initial_last_processed_slot(
    consensus: &ConsensusEngine,
    restored_last_slot: Option<u64>,
    now_unix_millis: u64,
) -> Option<u64> {
    if now_unix_millis < consensus.genesis().genesis_time_unix_millis {
        return restored_last_slot;
    }
    let current_slot = consensus.slot_at(now_unix_millis);
    if current_slot == 0 {
        return restored_last_slot;
    }
    let live_floor = current_slot - 1;
    Some(restored_last_slot.unwrap_or(live_floor).max(live_floor))
}

fn classify_peer_message(payload: &ProtocolMessage) -> Option<PeerMessageClass> {
    match payload {
        ProtocolMessage::SyncStatus { .. }
        | ProtocolMessage::SyncRequest { .. }
        | ProtocolMessage::CertifiedSyncRequest(_)
        | ProtocolMessage::ReceiptFetch { .. } => Some(PeerMessageClass::SyncControl),
        ProtocolMessage::TransactionBroadcast(_) => Some(PeerMessageClass::TransactionGossip),
        ProtocolMessage::RelayReceipt(_) => Some(PeerMessageClass::ReceiptGossip),
        ProtocolMessage::BlockProposal(_)
        | ProtocolMessage::ProposalVote(_)
        | ProtocolMessage::QuorumCertificate(_)
        | ProtocolMessage::SyncResponse { .. }
        | ProtocolMessage::SyncBlocks { .. }
        | ProtocolMessage::CertifiedSyncResponse(_)
        | ProtocolMessage::HeartbeatPulse(_)
        | ProtocolMessage::ServiceAttestation(_)
        | ProtocolMessage::ServiceAggregate(_)
        | ProtocolMessage::ReceiptResponse { .. } => None,
    }
}

fn peer_message_class_label(message_class: PeerMessageClass) -> &'static str {
    match message_class {
        PeerMessageClass::SyncControl => "sync-control",
        PeerMessageClass::TransactionGossip => "tx-gossip",
        PeerMessageClass::ReceiptGossip => "receipt-gossip",
    }
}

fn quorum_certificate_threshold(validator_count: usize) -> usize {
    if validator_count == 0 {
        return 0;
    }
    ((validator_count * 2) / 3) + 1
}

fn service_counters_dominate(candidate: &ServiceCounters, existing: &ServiceCounters) -> bool {
    candidate.total_windows == existing.total_windows
        && candidate.expected_deliveries == existing.expected_deliveries
        && candidate.expected_peers == existing.expected_peers
        && candidate.uptime_windows >= existing.uptime_windows
        && candidate.timely_deliveries >= existing.timely_deliveries
        && candidate.distinct_peers >= existing.distinct_peers
        && candidate.failed_sessions >= existing.failed_sessions
        && candidate.invalid_receipts >= existing.invalid_receipts
}

fn allow_peer_message_in_window(
    window: &mut PeerMessageWindow,
    message_class: PeerMessageClass,
    now_unix_millis: u64,
) -> bool {
    if window.started_at_unix_millis == 0
        || now_unix_millis.saturating_sub(window.started_at_unix_millis)
            >= PEER_MESSAGE_WINDOW_MILLIS
    {
        *window = PeerMessageWindow {
            started_at_unix_millis: now_unix_millis,
            ..PeerMessageWindow::default()
        };
    }

    let (counter, limit) = match message_class {
        PeerMessageClass::SyncControl => (
            &mut window.sync_control_messages,
            MAX_SYNC_CONTROL_MESSAGES_PER_WINDOW,
        ),
        PeerMessageClass::TransactionGossip => (
            &mut window.transaction_gossip_messages,
            MAX_TRANSACTION_GOSSIP_MESSAGES_PER_WINDOW,
        ),
        PeerMessageClass::ReceiptGossip => (
            &mut window.receipt_gossip_messages,
            MAX_RECEIPT_GOSSIP_MESSAGES_PER_WINDOW,
        ),
    };
    if *counter >= limit {
        return false;
    }
    *counter += 1;
    true
}

fn should_track_outbound_service_session(outbound: bool, service_accountable: bool) -> bool {
    outbound && service_accountable
}

fn should_record_failed_service_session(
    outbound: bool,
    service_accountable: bool,
    kind: NetworkFailureKind,
) -> bool {
    should_track_outbound_service_session(outbound, service_accountable)
        && kind == NetworkFailureKind::FaultInjected
}

fn build_chain_segment_from_chain(
    target_snapshot: &StateSnapshot,
    blocks: &[Block],
    receipts: &[RelayReceipt],
    window_epochs: u64,
    known_height: u64,
    known_tip_hash: HashBytes,
    preferred_max_blocks: usize,
) -> Option<ChainSegment> {
    if known_height >= blocks.len() as u64 {
        return None;
    }

    let expected_tip_hash = if known_height == 0 {
        empty_hash()
    } else {
        blocks
            .get(known_height.saturating_sub(1) as usize)?
            .block_hash
    };
    if expected_tip_hash != known_tip_hash {
        return None;
    }

    let missing_blocks = blocks.get(known_height as usize..)?.to_vec();
    if missing_blocks.is_empty()
        || missing_blocks.len() > MAX_INCREMENTAL_SYNC_BLOCKS
        || missing_blocks.len() > preferred_max_blocks
    {
        return None;
    }

    let latest_epoch = blocks.last().map(|block| block.header.epoch).unwrap_or(0);
    let earliest_recent_epoch = latest_epoch.saturating_sub(window_epochs.saturating_sub(1));
    let base_epoch = if known_height == 0 {
        0
    } else {
        blocks
            .get(known_height.saturating_sub(1) as usize)
            .map(|block| block.header.epoch)
            .unwrap_or(0)
    };
    let earliest_receipt_epoch = earliest_recent_epoch.min(base_epoch.saturating_sub(1));
    let recent_receipts = receipts
        .iter()
        .filter(|receipt| receipt.epoch >= earliest_receipt_epoch)
        .cloned()
        .collect();

    Some(ChainSegment {
        base_height: known_height,
        base_tip_hash: known_tip_hash,
        target_snapshot: target_snapshot.clone(),
        blocks: missing_blocks,
        receipts: recent_receipts,
        proposal_votes: Vec::new(),
    })
}

fn peer_qc_anchors_from_request(request: &ChunkedSyncRequest) -> Vec<SyncQcAnchor> {
    if !request.known_qc_anchors.is_empty() {
        return request.known_qc_anchors.clone();
    }
    request
        .known_qc_hash
        .zip((request.known_qc_height > 0).then_some(request.known_qc_height))
        .map(|(block_hash, block_number)| {
            vec![SyncQcAnchor {
                block_hash,
                block_number,
            }]
        })
        .unwrap_or_default()
}

fn build_receipt_fetch_plan(
    requester_id: ValidatorId,
    peer_ids: &BTreeSet<ValidatorId>,
) -> Vec<ValidatorId> {
    peer_ids
        .iter()
        .copied()
        .filter(|peer_id| *peer_id != requester_id)
        .collect()
}

fn align_chain_segment_to_local_chain(
    local_blocks: &[Block],
    local_snapshot: &StateSnapshot,
    mut chain: ChainSegment,
) -> Result<ChainSegment> {
    if chain.base_height > local_snapshot.height {
        return Err(anyhow!(
            "incremental sync height mismatch local {} remote {}",
            local_snapshot.height,
            chain.base_height
        ));
    }

    let expected_base_tip_hash = if chain.base_height == 0 {
        empty_hash()
    } else {
        local_blocks
            .get(chain.base_height.saturating_sub(1) as usize)
            .map(|block| block.block_hash)
            .ok_or_else(|| anyhow!("incremental sync missing local base block"))?
    };
    if expected_base_tip_hash != chain.base_tip_hash {
        return Err(anyhow!("incremental sync tip mismatch"));
    }

    if chain.base_height == local_snapshot.height {
        if chain.base_tip_hash != local_snapshot.tip_hash {
            return Err(anyhow!("incremental sync tip mismatch"));
        }
        return Ok(chain);
    }

    let overlap = local_snapshot.height.saturating_sub(chain.base_height) as usize;
    if overlap > chain.blocks.len() {
        return Err(anyhow!("incremental sync missing local overlap"));
    }

    for (offset, block) in chain.blocks.iter().take(overlap).enumerate() {
        let local_index = chain.base_height as usize + offset;
        let local_block = local_blocks
            .get(local_index)
            .ok_or_else(|| anyhow!("incremental sync missing local overlap"))?;
        if local_block.block_hash != block.block_hash {
            return Err(anyhow!("incremental sync diverged from local chain"));
        }
    }

    chain.base_height = local_snapshot.height;
    chain.base_tip_hash = local_snapshot.tip_hash;
    chain.blocks = chain.blocks.into_iter().skip(overlap).collect();
    Ok(chain)
}

fn validate_chain_snapshot(
    genesis: &GenesisConfig,
    consensus: &ConsensusEngine,
    crypto: &dyn CryptoBackend,
    chain: &ChainSnapshot,
) -> Result<(LedgerState, Vec<RelayReceipt>)> {
    let receipts = validate_snapshot_receipts(consensus, crypto, &chain.receipts)?;
    let mut ledger = LedgerState::from_genesis(genesis);
    let mut expected_parent_hash = empty_hash();

    for block in &chain.blocks {
        validate_snapshot_block(consensus, crypto, &ledger, block, expected_parent_hash)?;
        let mut next_ledger = ledger.clone();
        next_ledger.apply_block(block, crypto)?;
        ledger = next_ledger;
        expected_parent_hash = block.block_hash;
    }

    if ledger.snapshot() != &chain.snapshot {
        return Err(anyhow!("snapshot replay mismatch"));
    }

    Ok((ledger, receipts))
}

fn validate_snapshot_receipts(
    consensus: &ConsensusEngine,
    crypto: &dyn CryptoBackend,
    receipts: &[RelayReceipt],
) -> Result<Vec<RelayReceipt>> {
    let mut exact_hashes = BTreeSet::new();
    let mut event_hashes = BTreeSet::new();
    let mut sanitized = Vec::with_capacity(receipts.len());

    for receipt in receipts {
        let receipt_hash = canonical_hash(receipt);
        if !exact_hashes.insert(receipt_hash) {
            return Err(anyhow!("duplicate receipt in snapshot"));
        }
        let event_hash = receipt_event_hash(receipt);
        if !event_hashes.insert(event_hash) {
            return Err(anyhow!("duplicate receipt event in snapshot"));
        }

        let receipt_hash_to_verify = receipt_signing_hash(receipt);
        let verified = crypto.verify(
            receipt.witness_validator_id,
            &receipt_hash_to_verify,
            &receipt.signature,
        )?;
        if !verified {
            return Err(anyhow!("invalid receipt signature in snapshot"));
        }
        consensus
            .validate_receipt_assignment(receipt)
            .map_err(|error| anyhow!(error))?;
        sanitized.push(receipt.clone());
    }

    Ok(sanitized)
}

fn validate_snapshot_block(
    consensus: &ConsensusEngine,
    crypto: &dyn CryptoBackend,
    ledger: &LedgerState,
    block: &Block,
    expected_parent_hash: HashBytes,
) -> Result<()> {
    let expected_hash =
        canonical_hash(&(block.header.clone(), &block.transactions, &block.commitment));
    if expected_hash != block.block_hash {
        return Err(anyhow!("block hash mismatch in snapshot"));
    }
    let verified = crypto.verify(
        block.header.proposer_id,
        &block.block_hash,
        &block.signature,
    )?;
    if !verified {
        return Err(anyhow!("invalid block signature in snapshot"));
    }
    if block.header.block_number != ledger.block_height() + 1 {
        return Err(anyhow!("unexpected block number in snapshot"));
    }
    consensus
        .validate_block_basic(block, expected_parent_hash, None, false, 0.0)
        .map_err(|error| anyhow!(error))?;

    let transactions_root = canonical_hash(&block.transactions);
    if transactions_root != block.header.transactions_root {
        return Err(anyhow!("transactions root mismatch in snapshot"));
    }
    let topology_root = block
        .commitment
        .as_ref()
        .map(canonical_hash)
        .unwrap_or_else(empty_hash);
    if topology_root != block.header.topology_root {
        return Err(anyhow!("topology root mismatch in snapshot"));
    }

    if block.commitment.is_none() && !block.commitment_receipts.is_empty() {
        return Err(anyhow!("commitment receipts present without commitment"));
    }
    if let Some(commitment) = &block.commitment {
        validate_commitment_bundle(
            consensus,
            crypto,
            commitment,
            &block.commitment_receipts,
            &entangrid_types::default_service_score_weights(),
        )?;
    }

    Ok(())
}

fn validate_commitment_bundle(
    consensus: &ConsensusEngine,
    crypto: &dyn CryptoBackend,
    commitment: &TopologyCommitment,
    commitment_receipts: &[RelayReceipt],
    weights: &entangrid_types::ServiceScoreWeights,
) -> Result<()> {
    let mut exact_hashes = BTreeSet::new();
    let mut event_hashes = BTreeSet::new();
    for receipt in commitment_receipts {
        if receipt.source_validator_id != commitment.validator_id {
            return Err(anyhow!("commitment receipt source mismatch"));
        }
        if receipt.epoch != commitment.epoch {
            return Err(anyhow!("commitment receipt epoch mismatch"));
        }
        let receipt_hash_to_verify = receipt_signing_hash(receipt);
        let verified = crypto.verify(
            receipt.witness_validator_id,
            &receipt_hash_to_verify,
            &receipt.signature,
        )?;
        if !verified {
            return Err(anyhow!("invalid commitment receipt signature"));
        }
        consensus
            .validate_receipt_assignment(receipt)
            .map_err(|error| anyhow!(error))?;
        let receipt_hash = canonical_hash(receipt);
        if !exact_hashes.insert(receipt_hash) {
            return Err(anyhow!("duplicate receipt in commitment"));
        }
        let event_hash = receipt_event_hash(receipt);
        if !event_hashes.insert(event_hash) {
            return Err(anyhow!("duplicate receipt event in commitment"));
        }
    }
    let expected = consensus.commitment_from_receipts_with_weights(
        commitment.validator_id,
        commitment.epoch,
        commitment_receipts,
        0,
        0,
        weights,
    );
    if expected != *commitment {
        return Err(anyhow!("receipt root mismatch"));
    }
    Ok(())
}

fn accumulate_weighted_counters(
    aggregate: &mut ServiceCounters,
    counters: &ServiceCounters,
    weight: u64,
) {
    aggregate.uptime_windows = aggregate
        .uptime_windows
        .saturating_add(counters.uptime_windows.saturating_mul(weight));
    aggregate.total_windows = aggregate
        .total_windows
        .saturating_add(counters.total_windows.saturating_mul(weight));
    aggregate.timely_deliveries = aggregate
        .timely_deliveries
        .saturating_add(counters.timely_deliveries.saturating_mul(weight));
    aggregate.expected_deliveries = aggregate
        .expected_deliveries
        .saturating_add(counters.expected_deliveries.saturating_mul(weight));
    aggregate.distinct_peers = aggregate
        .distinct_peers
        .saturating_add(counters.distinct_peers.saturating_mul(weight));
    aggregate.expected_peers = aggregate
        .expected_peers
        .saturating_add(counters.expected_peers.saturating_mul(weight));
    aggregate.failed_sessions = aggregate
        .failed_sessions
        .saturating_add(counters.failed_sessions.saturating_mul(weight));
    aggregate.invalid_receipts = aggregate
        .invalid_receipts
        .saturating_add(counters.invalid_receipts.saturating_mul(weight));
}

fn should_record_failed_session(
    consensus: &ConsensusEngine,
    known_live_peers: &BTreeSet<ValidatorId>,
    observed_successful_sessions: &BTreeSet<(Epoch, ValidatorId, ValidatorId)>,
    epoch: Epoch,
    validator_id: ValidatorId,
    peer_validator_id: ValidatorId,
) -> bool {
    if !consensus.is_relay_target_for(validator_id, peer_validator_id, epoch) {
        return false;
    }
    if !known_live_peers.contains(&peer_validator_id) {
        return false;
    }
    if observed_successful_sessions.contains(&(epoch, validator_id, peer_validator_id)) {
        return false;
    }
    true
}

fn should_enforce_service_gating_for_block(
    enable_service_gating: bool,
    local_proposal: bool,
    epoch: Epoch,
    service_gating_start_epoch: Epoch,
) -> bool {
    enable_service_gating && local_proposal && epoch >= service_gating_start_epoch
}

fn sync_request_known_state(
    local_height: u64,
    local_tip_hash: HashBytes,
    force_full_snapshot: bool,
) -> (u64, HashBytes) {
    if force_full_snapshot {
        // A sentinel height forces the responder to fall back to a full snapshot.
        return (u64::MAX, empty_hash());
    }

    (local_height, local_tip_hash)
}

fn is_force_full_snapshot_hint(known_height: u64, known_tip_hash: HashBytes) -> bool {
    known_height == u64::MAX && known_tip_hash == empty_hash()
}

fn should_record_peer_sync_status(height: u64, tip_hash: HashBytes) -> bool {
    !is_force_full_snapshot_hint(height, tip_hash)
}

fn merge_sync_qc_anchors(
    existing: &[SyncQcAnchor],
    incoming: &[SyncQcAnchor],
    limit: usize,
) -> Vec<SyncQcAnchor> {
    let mut anchors = Vec::with_capacity(existing.len() + incoming.len());
    anchors.extend_from_slice(existing);
    anchors.extend_from_slice(incoming);
    anchors.sort_by(|left, right| {
        right
            .block_number
            .cmp(&left.block_number)
            .then_with(|| right.block_hash.cmp(&left.block_hash))
    });
    anchors.dedup_by(|left, right| left.block_hash == right.block_hash);
    anchors.truncate(limit);
    anchors
}

fn sync_repair_should_force_full_snapshot(failure_count: u64) -> bool {
    failure_count >= INCREMENTAL_SYNC_FAILURES_BEFORE_FULL_SNAPSHOT
}

fn sync_request_should_throttle(
    served: ServedSyncRequest,
    now_unix_millis: u64,
    known_height: u64,
    known_tip_hash: HashBytes,
    local_height: u64,
    local_tip_hash: HashBytes,
    local_highest_qc_height: u64,
    local_highest_qc_hash: Option<HashBytes>,
) -> bool {
    if is_force_full_snapshot_hint(known_height, known_tip_hash) {
        return false;
    }
    let anchored_at_responder_certified_frontier = local_highest_qc_height == known_height
        && local_highest_qc_hash == Some(known_tip_hash)
        && local_height > local_highest_qc_height;
    if anchored_at_responder_certified_frontier {
        return false;
    }
    let current_snapshot_preference = (local_height, 0, local_tip_hash);
    let served_snapshot_preference = (served.served_local_height, 0, served.served_local_tip_hash);
    let responder_advanced = local_highest_qc_height > served.served_local_highest_qc_height
        || (local_highest_qc_height == served.served_local_highest_qc_height
            && local_highest_qc_hash != served.served_local_highest_qc_hash)
        || current_snapshot_preference > served_snapshot_preference;
    if responder_advanced {
        return false;
    }
    now_unix_millis.saturating_sub(served.served_at_unix_millis) < SYNC_REQUEST_COOLDOWN_MILLIS
        && served.known_height == known_height
        && served.known_tip_hash == known_tip_hash
}

fn snapshot_preference(snapshot: &StateSnapshot) -> (u64, u64, HashBytes) {
    (snapshot.height, snapshot.last_slot, snapshot.tip_hash)
}

fn should_adopt_snapshot(local: &StateSnapshot, remote: &StateSnapshot) -> bool {
    snapshot_preference(remote) > snapshot_preference(local)
}

fn should_adopt_certified_snapshot(
    local: &StateSnapshot,
    local_highest_qc_height: u64,
    remote: &StateSnapshot,
    remote_highest_qc_height: u64,
) -> bool {
    if remote_highest_qc_height != local_highest_qc_height {
        return remote_highest_qc_height > local_highest_qc_height;
    }
    should_adopt_snapshot(local, remote)
}

#[derive(Clone, Debug)]
struct Storage {
    data_dir: PathBuf,
    inbox_dir: PathBuf,
    processed_dir: PathBuf,
    blocks_path: PathBuf,
    receipts_path: PathBuf,
    service_attestations_path: PathBuf,
    service_aggregates_path: PathBuf,
    orphan_path: PathBuf,
    snapshot_path: PathBuf,
    log_path: PathBuf,
    metrics_path: PathBuf,
}

impl Storage {
    fn new(config: &NodeConfig) -> Result<Self> {
        let data_dir = PathBuf::from(&config.data_dir);
        Ok(Self {
            inbox_dir: data_dir.join("inbox"),
            processed_dir: data_dir.join("processed"),
            blocks_path: data_dir.join(BLOCKS_FILE),
            receipts_path: data_dir.join(RECEIPTS_FILE),
            service_attestations_path: data_dir.join(SERVICE_ATTESTATIONS_FILE),
            service_aggregates_path: data_dir.join(SERVICE_AGGREGATES_FILE),
            orphan_path: data_dir.join(ORPHANS_FILE),
            snapshot_path: data_dir.join(SNAPSHOT_FILE),
            log_path: PathBuf::from(&config.log_path),
            metrics_path: PathBuf::from(&config.metrics_path),
            data_dir,
        })
    }

    fn init(&self) -> Result<()> {
        fs::create_dir_all(&self.data_dir)?;
        fs::create_dir_all(&self.inbox_dir)?;
        fs::create_dir_all(&self.processed_dir)?;
        Ok(())
    }

    fn load_blocks(&self) -> Result<Vec<Block>> {
        self.load_json_lines(&self.blocks_path)
    }

    fn load_receipts(&self) -> Result<Vec<RelayReceipt>> {
        self.load_json_lines(&self.receipts_path)
    }

    fn load_service_attestations(&self) -> Result<Vec<ServiceAttestation>> {
        self.load_json_lines(&self.service_attestations_path)
    }

    fn load_service_aggregates(&self) -> Result<Vec<ServiceAggregate>> {
        self.load_json_lines(&self.service_aggregates_path)
    }

    fn load_snapshot(&self) -> Result<Option<StateSnapshot>> {
        if !self.snapshot_path.exists() {
            return Ok(None);
        }
        let contents = fs::read_to_string(&self.snapshot_path)?;
        let snapshot = serde_json::from_str(&contents)?;
        Ok(Some(snapshot))
    }

    fn write_snapshot(&self, snapshot: &StateSnapshot) -> Result<()> {
        if let Some(parent) = self.snapshot_path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(&self.snapshot_path, serde_json::to_vec_pretty(snapshot)?)?;
        Ok(())
    }

    fn write_metrics(&self, metrics: &NodeMetrics) -> Result<()> {
        if let Some(parent) = self.metrics_path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(&self.metrics_path, serde_json::to_vec_pretty(metrics)?)?;
        Ok(())
    }

    fn append_json_line<T: serde::Serialize>(&self, path: &Path, value: &T) -> Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let mut file = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)?;
        serde_json::to_writer(&mut file, value)?;
        writeln!(file)?;
        Ok(())
    }

    fn overwrite_json_lines<T: serde::Serialize>(&self, path: &Path, values: &[T]) -> Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let mut file = fs::File::create(path)?;
        for value in values {
            serde_json::to_writer(&mut file, value)?;
            writeln!(file)?;
        }
        Ok(())
    }

    fn load_json_lines<T: for<'de> serde::Deserialize<'de>>(&self, path: &Path) -> Result<Vec<T>> {
        if !path.exists() {
            return Ok(Vec::new());
        }
        let contents = fs::read_to_string(path)?;
        let mut values = Vec::new();
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
                        "ignoring truncated trailing JSONL entry while loading {}",
                        path.display()
                    );
                    break;
                }
                Err(error) => return Err(error.into()),
            }
        }
        Ok(values)
    }
}

async fn shutdown_signal() {
    #[cfg(unix)]
    {
        if let Ok(mut terminate) =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        {
            tokio::select! {
                _ = tokio::signal::ctrl_c() => {}
                _ = terminate.recv() => {}
            }
            return;
        }
    }

    let _ = tokio::signal::ctrl_c().await;
}

fn init_tracing() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter("info")
        .with_target(false)
        .try_init();
}

#[cfg(test)]
mod tests {
    use std::{
        collections::BTreeMap,
        sync::{Arc, Mutex},
        time::Duration,
    };

    use entangrid_crypto::{CryptoBackend, DeterministicCryptoBackend, Signer};
    use entangrid_network::{NetworkEvent, spawn_network};
    use entangrid_types::{
        ChunkedSyncResponse, FaultProfile, FeatureFlags, GenesisConfig, NodeConfig, PeerConfig,
        ProposalVote, ProtocolMessage, PublicIdentity, ServiceAggregate, ServiceAttestation,
        ServiceCounters, SignatureScheme, SignedTransaction, SyncQcAnchor, Transaction,
        TypedSignature, ValidatorConfig, empty_hash, validator_account,
    };
    use tokio::{sync::mpsc, time::timeout};

    use super::*;

    fn sample_genesis() -> GenesisConfig {
        let mut balances = BTreeMap::new();
        for validator_id in 1..=4 {
            balances.insert(validator_account(validator_id), 1_000);
        }
        GenesisConfig {
            chain_id: "test".into(),
            epoch_seed: empty_hash(),
            genesis_time_unix_millis: 0,
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
                    public_identity: PublicIdentity::default(),
                })
                .collect(),
            initial_balances: balances,
        }
    }

    fn hybrid_test_identity(validator_id: ValidatorId) -> PublicIdentity {
        PublicIdentity::try_hybrid(vec![
            entangrid_types::PublicIdentityComponent {
                scheme: entangrid_types::PublicKeyScheme::DevDeterministic,
                bytes: format!("validator-{validator_id}").into_bytes(),
            },
            entangrid_types::PublicIdentityComponent {
                scheme: entangrid_types::PublicKeyScheme::MlDsa,
                bytes: format!("validator-{validator_id}-ml-dsa").into_bytes(),
            },
        ])
        .unwrap()
    }

    fn hybridized_sample_genesis() -> GenesisConfig {
        let mut genesis = sample_genesis();
        for validator in &mut genesis.validators {
            validator.public_identity = hybrid_test_identity(validator.validator_id);
        }
        genesis
    }

    fn sample_receipt() -> RelayReceipt {
        RelayReceipt {
            epoch: 1,
            slot: 2,
            source_validator_id: 1,
            destination_validator_id: 2,
            witness_validator_id: 3,
            message_class: MessageClass::Transaction,
            transcript_digest: [1u8; 32],
            latency_bucket_ms: 100,
            byte_count_bucket: 8,
            sequence_number: 7,
            signature: TypedSignature::single(SignatureScheme::DevDeterministic, vec![1, 2, 3]),
        }
    }

    fn first_valid_block(genesis: &GenesisConfig) -> Block {
        let crypto = DeterministicCryptoBackend::from_genesis(genesis);
        let consensus = ConsensusEngine::new(genesis.clone());
        let mut ledger = LedgerState::from_genesis(genesis);

        let transaction = Transaction {
            from: validator_account(1),
            to: validator_account(2),
            amount: 10,
            nonce: 0,
            memo: Some("sync-test".into()),
        };
        let tx_hash = canonical_hash(&transaction);
        let signed_transaction = SignedTransaction {
            transaction,
            signer_id: 1,
            signature: crypto.sign(1, &tx_hash).unwrap(),
            tx_hash,
            submitted_at_unix_millis: now_unix_millis(),
        };
        ledger.apply_transaction(&signed_transaction).unwrap();

        let slot = (0..20)
            .find(|slot| consensus.proposer_for_slot(*slot) == 1)
            .unwrap();
        let epoch = consensus.epoch_for_slot(slot);
        let transactions = vec![signed_transaction];
        let commitment: Option<TopologyCommitment> = None;
        let header = BlockHeader {
            block_number: 1,
            parent_hash: empty_hash(),
            slot,
            epoch,
            proposer_id: 1,
            timestamp_unix_millis: now_unix_millis(),
            state_root: ledger.state_root(),
            transactions_root: canonical_hash(&transactions),
            topology_root: empty_hash(),
        };
        let block_hash = canonical_hash(&(header.clone(), &transactions, &commitment));
        Block {
            header,
            transactions,
            commitment,
            commitment_receipts: Vec::new(),
            signature: crypto.sign(1, &block_hash).unwrap(),
            block_hash,
        }
    }

    #[cfg(feature = "pq-ml-dsa")]
    fn first_valid_block_with_crypto(genesis: &GenesisConfig, crypto: &dyn CryptoBackend) -> Block {
        let consensus = ConsensusEngine::new(genesis.clone());
        let mut ledger = LedgerState::from_genesis(genesis);

        let transaction = Transaction {
            from: validator_account(1),
            to: validator_account(2),
            amount: 10,
            nonce: 0,
            memo: Some("ml-dsa-sync-test".into()),
        };
        let tx_hash = canonical_hash(&transaction);
        let signed_transaction = SignedTransaction {
            transaction,
            signer_id: 1,
            signature: crypto.sign(1, &tx_hash).unwrap(),
            tx_hash,
            submitted_at_unix_millis: now_unix_millis(),
        };
        ledger.apply_transaction(&signed_transaction).unwrap();

        let slot = (0..20)
            .find(|slot| consensus.proposer_for_slot(*slot) == 1)
            .unwrap();
        let epoch = consensus.epoch_for_slot(slot);
        let transactions = vec![signed_transaction];
        let commitment: Option<TopologyCommitment> = None;
        let header = BlockHeader {
            block_number: 1,
            parent_hash: empty_hash(),
            slot,
            epoch,
            proposer_id: 1,
            timestamp_unix_millis: now_unix_millis(),
            state_root: ledger.state_root(),
            transactions_root: canonical_hash(&transactions),
            topology_root: empty_hash(),
        };
        let block_hash = canonical_hash(&(header.clone(), &transactions, &commitment));
        Block {
            header,
            transactions,
            commitment,
            commitment_receipts: Vec::new(),
            signature: crypto.sign(1, &block_hash).unwrap(),
            block_hash,
        }
    }

    fn valid_empty_block_on_parent(
        genesis: &GenesisConfig,
        parent_hash: HashBytes,
        block_number: u64,
        proposer_id: ValidatorId,
        min_slot: u64,
    ) -> Block {
        let crypto = DeterministicCryptoBackend::from_genesis(genesis);
        let consensus = ConsensusEngine::new(genesis.clone());
        let ledger = LedgerState::from_genesis(genesis);
        let slot = (min_slot..(min_slot + 50))
            .find(|slot| consensus.proposer_for_slot(*slot) == proposer_id)
            .unwrap();
        let epoch = consensus.epoch_for_slot(slot);
        let transactions = Vec::new();
        let commitment: Option<TopologyCommitment> = None;
        let header = BlockHeader {
            block_number,
            parent_hash,
            slot,
            epoch,
            proposer_id,
            timestamp_unix_millis: now_unix_millis(),
            state_root: ledger.state_root(),
            transactions_root: canonical_hash(&transactions),
            topology_root: empty_hash(),
        };
        let block_hash = canonical_hash(&(header.clone(), &transactions, &commitment));
        Block {
            header,
            transactions,
            commitment,
            commitment_receipts: Vec::new(),
            signature: crypto.sign(proposer_id, &block_hash).unwrap(),
            block_hash,
        }
    }

    fn unique_test_dir(label: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "entangrid-node-{label}-{}-{}",
            std::process::id(),
            now_unix_millis()
        ))
    }

    fn reserve_local_address() -> String {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap().to_string();
        drop(listener);
        address
    }

    fn sample_node_config(
        validator_id: ValidatorId,
        listen_address: String,
        peers: Vec<PeerConfig>,
        data_label: &str,
    ) -> NodeConfig {
        let data_dir = unique_test_dir(data_label);
        NodeConfig {
            validator_id,
            data_dir: data_dir.display().to_string(),
            genesis_path: data_dir.join("genesis.toml").display().to_string(),
            listen_address,
            peers,
            log_path: data_dir.join("events.log").display().to_string(),
            metrics_path: data_dir.join("metrics.json").display().to_string(),
            feature_flags: FeatureFlags {
                enable_receipts: true,
                enable_service_gating: true,
                consensus_v2: true,
                service_gating_start_epoch: 1,
                service_gating_threshold: entangrid_types::default_service_gating_threshold(),
                service_score_window_epochs: entangrid_types::default_service_score_window_epochs(),
                service_score_weights: entangrid_types::default_service_score_weights(),
                ..FeatureFlags::default()
            },
            fault_profile: FaultProfile::default(),
            sync_on_startup: false,
            signing_backend: entangrid_types::SigningBackendKind::DevDeterministic,
            signing_key_path: None,
        }
    }

    async fn spawn_test_network(
        validator_id: ValidatorId,
        listen_address: String,
        peers: Vec<PeerConfig>,
        genesis: &GenesisConfig,
    ) -> (
        Arc<dyn CryptoBackend>,
        NetworkHandle,
        Arc<Mutex<NodeMetrics>>,
        mpsc::UnboundedReceiver<NetworkEvent>,
    ) {
        let crypto: Arc<dyn CryptoBackend> =
            Arc::new(DeterministicCryptoBackend::from_genesis(genesis));
        let metrics = Arc::new(Mutex::new(NodeMetrics {
            validator_id,
            ..NodeMetrics::default()
        }));
        let (network_event_tx, network_event_rx) = mpsc::unbounded_channel();
        let network = spawn_network(
            validator_id,
            listen_address,
            peers,
            FaultProfile::default(),
            Arc::clone(&crypto),
            Arc::clone(&metrics),
            network_event_tx,
        )
        .await
        .unwrap();
        (crypto, network, metrics, network_event_rx)
    }

    async fn build_test_runner(
        validator_id: ValidatorId,
        config: NodeConfig,
        genesis: GenesisConfig,
    ) -> NodeRunner {
        let consensus = ConsensusEngine::new(genesis.clone());
        let ledger = LedgerState::from_genesis(&genesis);
        let crypto = build_crypto_backend(&genesis, &config).unwrap();
        let (_network_crypto, network, metrics, network_event_rx) = spawn_test_network(
            validator_id,
            config.listen_address.clone(),
            config.peers.clone(),
            &genesis,
        )
        .await;
        let storage = Storage::new(&config).unwrap();
        storage.init().unwrap();
        NodeRunner {
            config,
            genesis: genesis.clone(),
            consensus,
            crypto,
            storage,
            network,
            network_event_rx,
            metrics,
            ledger,
            blocks: Vec::new(),
            receipts: Vec::new(),
            orphan_blocks: Vec::new(),
            pending_certified_children: Vec::new(),
            mempool: BTreeMap::new(),
            seen_transactions: BTreeSet::new(),
            seen_blocks: BTreeSet::new(),
            seen_receipts: BTreeSet::new(),
            seen_receipt_events: BTreeSet::new(),
            service_attestations: BTreeMap::new(),
            service_aggregates: BTreeMap::new(),
            proposal_votes: BTreeMap::new(),
            buffered_proposal_votes: BTreeMap::new(),
            quorum_certificates: BTreeMap::new(),
            seen_service_attestations: BTreeSet::new(),
            seen_service_aggregates: BTreeSet::new(),
            last_processed_slot: None,
            last_logged_epoch: None,
            last_heartbeat_slot: None,
            last_proposed_slot: None,
            startup_sync_barrier: false,
            latest_service_scores: BTreeMap::new(),
            latest_service_counters: BTreeMap::new(),
            failed_sessions: 0,
            invalid_receipts: 0,
            observed_failed_sessions: BTreeSet::new(),
            observed_successful_sessions: BTreeSet::new(),
            observed_invalid_receipts: BTreeMap::new(),
            known_live_peers: BTreeSet::new(),
            last_sync_request_served: BTreeMap::new(),
            last_sync_push_served: BTreeMap::new(),
            last_recovery_sync_request: None,
            last_recovery_sync_status: None,
            peer_sync_status: BTreeMap::new(),
            peer_sync_repair_failures: BTreeMap::new(),
            peer_message_windows: BTreeMap::new(),
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn hybrid_enforcement_rejects_mixed_validator_identities_at_startup() {
        let mut genesis = sample_genesis();
        genesis.validators = genesis
            .validators
            .into_iter()
            .map(|validator| ValidatorConfig {
                public_identity: PublicIdentity::try_hybrid(vec![
                    entangrid_types::PublicIdentityComponent {
                        scheme: entangrid_types::PublicKeyScheme::DevDeterministic,
                        bytes: format!("validator-{}", validator.validator_id).into_bytes(),
                    },
                    entangrid_types::PublicIdentityComponent {
                        scheme: entangrid_types::PublicKeyScheme::MlDsa,
                        bytes: format!("validator-{}-ml-dsa", validator.validator_id).into_bytes(),
                    },
                ])
                .unwrap(),
                ..validator
            })
            .collect();
        genesis.validators[0].public_identity = PublicIdentity::single(
            entangrid_types::PublicKeyScheme::DevDeterministic,
            b"validator-1".to_vec(),
        );
        let mut config = sample_node_config(
            1,
            reserve_local_address(),
            Vec::new(),
            "hybrid-startup-mixed",
        );
        config.feature_flags.require_hybrid_validator_signatures = true;

        let error = run_node(config, genesis).await.unwrap_err();
        assert!(
            error.to_string().contains("validator 1"),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn hybrid_enforcement_accepts_all_hybrid_validator_identities_at_startup() {
        let mut genesis = sample_genesis();
        genesis.validators = genesis
            .validators
            .into_iter()
            .map(|validator| ValidatorConfig {
                public_identity: PublicIdentity::try_hybrid(vec![
                    entangrid_types::PublicIdentityComponent {
                        scheme: entangrid_types::PublicKeyScheme::DevDeterministic,
                        bytes: format!("validator-{}", validator.validator_id).into_bytes(),
                    },
                    entangrid_types::PublicIdentityComponent {
                        scheme: entangrid_types::PublicKeyScheme::MlDsa,
                        bytes: format!("validator-{}-ml-dsa", validator.validator_id).into_bytes(),
                    },
                ])
                .unwrap(),
                ..validator
            })
            .collect();
        let mut config = sample_node_config(
            1,
            reserve_local_address(),
            Vec::new(),
            "hybrid-startup-all-hybrid",
        );
        config.feature_flags.require_hybrid_validator_signatures = true;

        validate_hybrid_enforcement_genesis(&config, &genesis).unwrap();
    }

    fn signed_service_attestation(
        crypto: &dyn CryptoBackend,
        subject_validator_id: ValidatorId,
        committee_member_id: ValidatorId,
        epoch: Epoch,
        counters: ServiceCounters,
    ) -> ServiceAttestation {
        let mut attestation = ServiceAttestation {
            subject_validator_id,
            committee_member_id,
            epoch,
            counters,
            signature: TypedSignature::default(),
        };
        let hash = service_attestation_signing_hash(&attestation);
        attestation.signature = crypto.sign(committee_member_id, &hash).unwrap();
        attestation
    }

    fn service_aggregate_for_subject(
        crypto: &dyn CryptoBackend,
        consensus: &ConsensusEngine,
        subject_validator_id: ValidatorId,
        epoch: Epoch,
        counters: ServiceCounters,
    ) -> ServiceAggregate {
        let attestations = consensus
            .service_committee_for(epoch, subject_validator_id)
            .into_iter()
            .map(|committee_member_id| {
                signed_service_attestation(
                    crypto,
                    subject_validator_id,
                    committee_member_id,
                    epoch,
                    counters.clone(),
                )
            })
            .collect::<Vec<_>>();
        ServiceAggregate {
            subject_validator_id,
            epoch,
            attestation_root: entangrid_types::service_attestation_root(&attestations),
            aggregate_counters: entangrid_types::aggregate_service_counters(&attestations),
            attestations,
        }
    }

    fn service_aggregate_from_attestations(
        subject_validator_id: ValidatorId,
        epoch: Epoch,
        attestations: Vec<ServiceAttestation>,
    ) -> ServiceAggregate {
        ServiceAggregate {
            subject_validator_id,
            epoch,
            attestation_root: entangrid_types::service_attestation_root(&attestations),
            aggregate_counters: entangrid_types::aggregate_service_counters(&attestations),
            attestations,
        }
    }

    fn signed_proposal_vote(
        crypto: &dyn CryptoBackend,
        validator_id: ValidatorId,
        block: &Block,
    ) -> ProposalVote {
        let mut vote = ProposalVote {
            validator_id,
            block_hash: block.block_hash,
            block_number: block.header.block_number,
            epoch: block.header.epoch,
            slot: block.header.slot,
            signature: TypedSignature::default(),
        };
        let hash = proposal_vote_signing_hash(&vote);
        vote.signature = crypto.sign(validator_id, &hash).unwrap();
        vote
    }

    #[test]
    fn first_valid_block_uses_typed_signature() {
        let genesis = sample_genesis();
        let block = first_valid_block(&genesis);
        assert_eq!(
            block.signature.scheme(),
            entangrid_types::SignatureScheme::DevDeterministic
        );
        assert!(block.signature.as_single_bytes().is_some());
    }

    #[test]
    fn signed_proposal_vote_uses_typed_signature() {
        let genesis = sample_genesis();
        let crypto = DeterministicCryptoBackend::from_genesis(&genesis);
        let block = first_valid_block(&genesis);
        let vote = signed_proposal_vote(&crypto, 2, &block);
        assert_eq!(
            vote.signature.scheme(),
            entangrid_types::SignatureScheme::DevDeterministic
        );
        assert!(vote.signature.as_single_bytes().is_some());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn hybrid_enforcement_rejects_non_hybrid_block_signature() {
        let genesis = hybridized_sample_genesis();
        let mut config = sample_node_config(1, reserve_local_address(), Vec::new(), "block-reject");
        config.feature_flags.require_hybrid_validator_signatures = true;
        let mut runner = build_test_runner(1, config, genesis.clone()).await;
        let block = first_valid_block(&genesis);

        let error = runner.accept_block(block, true).await.unwrap_err();
        assert!(
            error.to_string().contains("block proposer 1"),
            "unexpected error: {error}"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn hybrid_enforcement_rejects_non_hybrid_competing_block_signature() {
        let genesis = hybridized_sample_genesis();
        let mut config =
            sample_node_config(1, reserve_local_address(), Vec::new(), "block-competing-reject");
        config.feature_flags.require_hybrid_validator_signatures = false;
        let mut runner = build_test_runner(1, config, genesis.clone()).await;
        let tip_block = first_valid_block(&genesis);
        assert_eq!(
            runner.accept_block(tip_block.clone(), true).await.unwrap(),
            BlockAcceptance::Accepted
        );
        runner.config.feature_flags.require_hybrid_validator_signatures = true;

        let competing_block =
            valid_empty_block_on_parent(&genesis, empty_hash(), 1, 2, tip_block.header.slot + 1);
        let error = runner
            .accept_block(competing_block.clone(), false)
            .await
            .unwrap_err();
        assert!(
            error.to_string().contains("block proposer 2"),
            "unexpected error: {error}"
        );
        assert!(
            !runner.proposal_votes.contains_key(&competing_block.block_hash),
            "rejecting a non-hybrid competing block should not record a local vote"
        );
        assert!(
            !runner
                .orphan_blocks
                .iter()
                .any(|block| block.block_hash == competing_block.block_hash),
            "rejecting a non-hybrid competing block should not retain it as an orphan"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn hybrid_enforcement_accepts_non_hybrid_block_signature_when_disabled() {
        let genesis = sample_genesis();
        let mut config =
            sample_node_config(1, reserve_local_address(), Vec::new(), "block-permissive");
        config.feature_flags.require_hybrid_validator_signatures = false;
        let mut runner = build_test_runner(1, config, genesis.clone()).await;
        let block = first_valid_block(&genesis);

        assert_eq!(
            runner.accept_block(block, true).await.unwrap(),
            BlockAcceptance::Accepted
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn hybrid_enforcement_rejects_non_hybrid_proposal_vote_signature() {
        let genesis = sample_genesis();
        let mut config = sample_node_config(1, reserve_local_address(), Vec::new(), "vote-reject");
        config.feature_flags.require_hybrid_validator_signatures = true;
        let mut runner = build_test_runner(1, config, genesis.clone()).await;
        let block = first_valid_block(&genesis);
        let vote = signed_proposal_vote(runner.crypto.as_ref(), 2, &block);

        let error = runner.import_proposal_vote(vote).unwrap_err();
        assert!(
            error.to_string().contains("validator 2"),
            "unexpected error: {error}"
        );
        assert!(runner.proposal_votes.is_empty());
        assert!(runner.buffered_proposal_votes.is_empty());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn hybrid_enforcement_accepts_non_hybrid_proposal_vote_signature_when_disabled() {
        let genesis = sample_genesis();
        let mut config =
            sample_node_config(1, reserve_local_address(), Vec::new(), "vote-permissive");
        config.feature_flags.require_hybrid_validator_signatures = false;
        let mut runner = build_test_runner(1, config, genesis.clone()).await;
        let block = first_valid_block(&genesis);
        let vote = signed_proposal_vote(runner.crypto.as_ref(), 2, &block);

        assert!(runner.import_proposal_vote(vote.clone()).unwrap());
        assert!(
            runner
                .buffered_proposal_votes
                .get(&vote.block_hash)
                .and_then(|by_validator| by_validator.get(&2))
                .is_some()
        );
    }

    #[cfg(feature = "pq-ml-dsa")]
    #[tokio::test(flavor = "current_thread")]
    async fn ml_dsa_block_and_proposal_vote_validate_with_test_runner() {
        use ml_dsa::{KeyGen, MlDsa65};
        use rand_core::OsRng;
        use serde::Serialize;

        #[derive(Serialize)]
        struct MlDsa65KeyFileFixture {
            signing_key: Vec<u8>,
            verifying_key: Vec<u8>,
        }

        let mut rng = OsRng;
        let keypair = MlDsa65::key_gen(&mut rng);
        let signing_key = keypair.signing_key().clone();
        let verifying_key = keypair.verifying_key().clone();
        let key_path =
            std::env::temp_dir().join(format!("entangrid-node-ml-dsa-{}.json", std::process::id()));
        let key_file = MlDsa65KeyFileFixture {
            signing_key: signing_key.encode().as_slice().to_vec(),
            verifying_key: verifying_key.encode().as_slice().to_vec(),
        };
        fs::write(&key_path, serde_json::to_vec(&key_file).unwrap()).unwrap();

        let mut genesis = sample_genesis();
        genesis.validators[0].public_identity = PublicIdentity::single(
            entangrid_types::PublicKeyScheme::MlDsa,
            verifying_key.encode().as_slice().to_vec(),
        );

        let mut config =
            sample_node_config(1, reserve_local_address(), Vec::new(), "ml-dsa-runner");
        config.signing_backend = entangrid_types::SigningBackendKind::MlDsa65Experimental;
        config.signing_key_path = Some(key_path.display().to_string());

        let mut runner = build_test_runner(1, config, genesis.clone()).await;
        let block = first_valid_block_with_crypto(&genesis, runner.crypto.as_ref());
        assert_eq!(
            block.signature.scheme(),
            entangrid_types::SignatureScheme::MlDsa
        );
        assert_eq!(
            runner.accept_block(block.clone(), true).await.unwrap(),
            BlockAcceptance::Accepted
        );

        let vote = signed_proposal_vote(runner.crypto.as_ref(), 1, &block);
        assert_eq!(
            vote.signature.scheme(),
            entangrid_types::SignatureScheme::MlDsa
        );
        assert!(!runner.import_proposal_vote(vote).unwrap());
        let stored_vote = runner
            .proposal_votes
            .get(&block.block_hash)
            .and_then(|by_validator| by_validator.get(&1))
            .expect("runner should retain the ML-DSA local vote");
        assert_eq!(
            stored_vote.signature.scheme(),
            entangrid_types::SignatureScheme::MlDsa
        );
    }

    #[cfg(feature = "pq-ml-dsa")]
    #[tokio::test(flavor = "current_thread")]
    async fn hybrid_enforcement_accepts_hybrid_proposal_vote_signature() {
        use ml_dsa::{KeyGen, MlDsa65};
        use rand_core::OsRng;
        use serde::Serialize;

        #[derive(Serialize)]
        struct MlDsa65KeyFileFixture {
            signing_key: Vec<u8>,
            verifying_key: Vec<u8>,
        }

        let mut rng = OsRng;
        let keypair = MlDsa65::key_gen(&mut rng);
        let signing_key = keypair.signing_key().clone();
        let verifying_key = keypair.verifying_key().clone();
        let key_path =
            std::env::temp_dir().join(format!("entangrid-node-hybrid-{}.json", std::process::id()));
        let key_file = MlDsa65KeyFileFixture {
            signing_key: signing_key.encode().as_slice().to_vec(),
            verifying_key: verifying_key.encode().as_slice().to_vec(),
        };
        fs::write(&key_path, serde_json::to_vec(&key_file).unwrap()).unwrap();

        let mut genesis = sample_genesis();
        genesis.validators[0].public_identity = PublicIdentity::try_hybrid(vec![
            entangrid_types::PublicIdentityComponent {
                scheme: entangrid_types::PublicKeyScheme::DevDeterministic,
                bytes: format!("validator-{}", 1).into_bytes(),
            },
            entangrid_types::PublicIdentityComponent {
                scheme: entangrid_types::PublicKeyScheme::MlDsa,
                bytes: verifying_key.encode().as_slice().to_vec(),
            },
        ])
        .unwrap();

        let mut config =
            sample_node_config(1, reserve_local_address(), Vec::new(), "hybrid-runner");
        config.signing_backend =
            entangrid_types::SigningBackendKind::HybridDeterministicMlDsaExperimental;
        config.signing_key_path = Some(key_path.display().to_string());
        config.feature_flags.require_hybrid_validator_signatures = true;

        let mut runner = build_test_runner(1, config, genesis.clone()).await;
        let block = first_valid_block_with_crypto(&genesis, runner.crypto.as_ref());
        assert_eq!(
            runner.accept_block(block.clone(), true).await.unwrap(),
            BlockAcceptance::Accepted
        );

        let vote = runner.build_local_proposal_vote(&block).unwrap();
        assert_eq!(
            vote.signature.scheme(),
            entangrid_types::SignatureScheme::Hybrid
        );
        assert!(
            vote.signature
                .component_bytes(entangrid_types::SignatureScheme::DevDeterministic)
                .is_some()
        );
        assert!(
            vote.signature
                .component_bytes(entangrid_types::SignatureScheme::MlDsa)
                .is_some()
        );
        assert!(!runner.import_proposal_vote(vote).unwrap());
        let stored_vote = runner
            .proposal_votes
            .get(&block.block_hash)
            .and_then(|by_validator| by_validator.get(&1))
            .expect("runner should retain the hybrid local vote");
        assert_eq!(
            stored_vote.signature.scheme(),
            entangrid_types::SignatureScheme::Hybrid
        );
    }

    async fn recv_protocol_message(
        events: &mut mpsc::UnboundedReceiver<NetworkEvent>,
    ) -> ProtocolMessage {
        loop {
            let event = timeout(Duration::from_secs(5), events.recv())
                .await
                .unwrap()
                .expect("peer network should stay alive");
            if let NetworkEvent::Received { payload, .. } = event {
                return payload;
            }
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn committee_member_emits_lagged_attestation_after_reconciliation_in_v2_mode() {
        let mut genesis = sample_genesis();
        genesis.genesis_time_unix_millis = now_unix_millis()
            .saturating_sub(genesis.slots_per_epoch * genesis.slot_duration_millis * 3);
        let local_address = reserve_local_address();
        let peer_address = reserve_local_address();
        let peer = PeerConfig {
            validator_id: 2,
            address: peer_address.clone(),
        };
        let config = sample_node_config(1, local_address, vec![peer], "attestation-emission");
        let mut runner = build_test_runner(1, config, genesis.clone()).await;
        let (_, _, _, mut peer_events) =
            spawn_test_network(2, peer_address, Vec::new(), &genesis).await;
        let expected_subjects: BTreeSet<_> = genesis
            .validators
            .iter()
            .map(|validator| validator.validator_id)
            .filter(|subject_validator_id| *subject_validator_id != 1)
            .filter(|subject_validator_id| {
                runner
                    .consensus
                    .service_aggregators_for(0, *subject_validator_id)
                    .contains(&2)
            })
            .collect();

        runner.process_slot_tick().await.unwrap();

        let mut seen_subjects = BTreeSet::new();
        while seen_subjects.len() < expected_subjects.len() {
            match recv_protocol_message(&mut peer_events).await {
                ProtocolMessage::ServiceAttestation(attestation) => {
                    assert_eq!(attestation.epoch, 0);
                    assert_eq!(attestation.committee_member_id, 1);
                    seen_subjects.insert(attestation.subject_validator_id);
                }
                ProtocolMessage::ReceiptFetch { .. } | ProtocolMessage::HeartbeatPulse(_) => {}
                other => panic!("expected service attestation, got {other:?}"),
            }
        }
        assert_eq!(seen_subjects, expected_subjects);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn aggregate_import_updates_local_v2_service_score_view() {
        let genesis = sample_genesis();
        let config = sample_node_config(1, reserve_local_address(), Vec::new(), "aggregate-view");
        let mut runner = build_test_runner(1, config, genesis.clone()).await;
        runner.last_processed_slot = Some(genesis.slots_per_epoch * 2);

        let counters = ServiceCounters {
            uptime_windows: 5,
            total_windows: 5,
            timely_deliveries: 5,
            expected_deliveries: 5,
            distinct_peers: 2,
            expected_peers: 2,
            failed_sessions: 0,
            invalid_receipts: 0,
        };
        let aggregate = service_aggregate_for_subject(
            runner.crypto.as_ref(),
            &runner.consensus,
            1,
            1,
            counters,
        );

        runner.import_service_aggregate(aggregate.clone()).unwrap();
        runner.refresh_service_scores();

        let (_, expected_counters, expected_score) = runner
            .weighted_recent_service_evidence_before_epoch(1, runner.current_epoch())
            .unwrap();
        assert_eq!(runner.service_score_for_validator(1), expected_score);
        assert_eq!(runner.local_service_counters(), expected_counters);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn proposal_votes_build_qc_at_supermajority_threshold() {
        let genesis = sample_genesis();
        let config = sample_node_config(1, reserve_local_address(), Vec::new(), "qc-assembly");
        let mut runner = build_test_runner(1, config, genesis.clone()).await;
        let block = first_valid_block(&genesis);

        assert_eq!(
            runner.accept_block(block.clone(), true).await.unwrap(),
            BlockAcceptance::Accepted
        );
        assert!(runner.quorum_certificates.get(&block.block_hash).is_none());

        let vote_2 = signed_proposal_vote(runner.crypto.as_ref(), 2, &block);
        let vote_3 = signed_proposal_vote(runner.crypto.as_ref(), 3, &block);

        assert!(runner.import_proposal_vote(vote_2).unwrap());
        assert!(runner.quorum_certificates.get(&block.block_hash).is_none());
        assert!(runner.import_proposal_vote(vote_3).unwrap());

        let qc = runner.quorum_certificates.get(&block.block_hash).unwrap();
        assert_eq!(qc.block_hash, block.block_hash);
        assert_eq!(qc.block_number, block.header.block_number);
        assert_eq!(qc.slot, block.header.slot);
        assert_eq!(qc.epoch, block.header.epoch);
        assert_eq!(qc.votes.len(), 3);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn certified_competing_branch_reorgs_uncertified_current_tip() {
        let genesis = sample_genesis();
        let config = sample_node_config(1, reserve_local_address(), Vec::new(), "qc-reorg");
        let mut runner = build_test_runner(1, config, genesis.clone()).await;
        let current_tip = first_valid_block(&genesis);
        let competing_tip =
            valid_empty_block_on_parent(&genesis, empty_hash(), 1, 2, current_tip.header.slot + 1);

        assert_eq!(
            runner
                .accept_block(current_tip.clone(), true)
                .await
                .unwrap(),
            BlockAcceptance::Accepted
        );
        assert_eq!(
            runner
                .accept_block(competing_tip.clone(), false)
                .await
                .unwrap(),
            BlockAcceptance::Orphan
        );
        assert_eq!(runner.ledger.snapshot().tip_hash, current_tip.block_hash);

        assert!(
            runner
                .import_proposal_vote(signed_proposal_vote(
                    runner.crypto.as_ref(),
                    2,
                    &competing_tip
                ))
                .unwrap()
        );
        assert!(
            runner
                .import_proposal_vote(signed_proposal_vote(
                    runner.crypto.as_ref(),
                    3,
                    &competing_tip
                ))
                .unwrap()
        );
        assert!(
            runner
                .import_proposal_vote(signed_proposal_vote(
                    runner.crypto.as_ref(),
                    4,
                    &competing_tip
                ))
                .unwrap()
        );

        assert_eq!(runner.ledger.snapshot().tip_hash, competing_tip.block_hash);
        assert_eq!(runner.blocks.len(), 1);
        assert_eq!(runner.blocks[0].block_hash, competing_tip.block_hash);
        assert!(
            runner
                .orphan_blocks
                .iter()
                .any(|block| block.block_hash == current_tip.block_hash)
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn imported_quorum_certificate_reorgs_competing_branch() {
        let genesis = sample_genesis();
        let config = sample_node_config(1, reserve_local_address(), Vec::new(), "qc-import");
        let mut runner = build_test_runner(1, config, genesis.clone()).await;
        let current_tip = first_valid_block(&genesis);
        let competing_tip =
            valid_empty_block_on_parent(&genesis, empty_hash(), 1, 2, current_tip.header.slot + 1);

        assert_eq!(
            runner
                .accept_block(current_tip.clone(), true)
                .await
                .unwrap(),
            BlockAcceptance::Accepted
        );
        assert_eq!(
            runner
                .accept_block(competing_tip.clone(), false)
                .await
                .unwrap(),
            BlockAcceptance::Orphan
        );

        let votes = vec![
            signed_proposal_vote(runner.crypto.as_ref(), 2, &competing_tip),
            signed_proposal_vote(runner.crypto.as_ref(), 3, &competing_tip),
            signed_proposal_vote(runner.crypto.as_ref(), 4, &competing_tip),
        ];
        let qc = QuorumCertificate {
            block_hash: competing_tip.block_hash,
            block_number: competing_tip.header.block_number,
            epoch: competing_tip.header.epoch,
            slot: competing_tip.header.slot,
            vote_root: entangrid_types::quorum_certificate_vote_root(&votes),
            votes,
        };

        assert!(runner.import_quorum_certificate(qc).unwrap());
        assert_eq!(runner.ledger.snapshot().tip_hash, competing_tip.block_hash);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn unknown_quorum_certificate_triggers_certified_sync_request() {
        let genesis = sample_genesis();
        let local_address = reserve_local_address();
        let peer_address = reserve_local_address();
        let peer = PeerConfig {
            validator_id: 2,
            address: peer_address.clone(),
        };
        let config = sample_node_config(1, local_address, vec![peer], "qc-unknown-sync");
        let mut runner = build_test_runner(1, config, genesis.clone()).await;
        let (_, _, _, mut peer_events) =
            spawn_test_network(2, peer_address, Vec::new(), &genesis).await;
        let certified_root = first_valid_block(&genesis);

        assert_eq!(
            runner
                .accept_block(certified_root.clone(), true)
                .await
                .unwrap(),
            BlockAcceptance::Accepted
        );
        assert!(
            runner
                .import_proposal_vote(signed_proposal_vote(
                    runner.crypto.as_ref(),
                    2,
                    &certified_root
                ))
                .unwrap()
        );
        assert!(
            runner
                .import_proposal_vote(signed_proposal_vote(
                    runner.crypto.as_ref(),
                    3,
                    &certified_root
                ))
                .unwrap()
        );

        let crypto = DeterministicCryptoBackend::from_genesis(&genesis);
        let consensus = ConsensusEngine::new(genesis.clone());
        let mut root_ledger = LedgerState::from_genesis(&genesis);
        root_ledger.apply_block(&certified_root, &crypto).unwrap();
        let slot = ((certified_root.header.slot + 1)..(certified_root.header.slot + 50))
            .find(|slot| consensus.proposer_for_slot(*slot) == 3)
            .unwrap();
        let epoch = consensus.epoch_for_slot(slot);
        let transactions = Vec::new();
        let commitment: Option<TopologyCommitment> = None;
        let header = BlockHeader {
            block_number: 2,
            parent_hash: certified_root.block_hash,
            slot,
            epoch,
            proposer_id: 3,
            timestamp_unix_millis: now_unix_millis(),
            state_root: root_ledger.state_root(),
            transactions_root: canonical_hash(&transactions),
            topology_root: empty_hash(),
        };
        let block_hash = canonical_hash(&(header.clone(), &transactions, &commitment));
        let unknown_child = Block {
            header,
            transactions,
            commitment,
            commitment_receipts: Vec::new(),
            signature: crypto.sign(3, &block_hash).unwrap(),
            block_hash,
        };
        let votes = [2_u64, 3, 4]
            .into_iter()
            .map(|validator_id| {
                signed_proposal_vote(runner.crypto.as_ref(), validator_id, &unknown_child)
            })
            .collect::<Vec<_>>();
        let qc = QuorumCertificate {
            block_hash: unknown_child.block_hash,
            block_number: unknown_child.header.block_number,
            epoch: unknown_child.header.epoch,
            slot: unknown_child.header.slot,
            vote_root: entangrid_types::quorum_certificate_vote_root(&votes),
            votes,
        };

        runner
            .handle_network_event(NetworkEvent::Received {
                from_validator_id: 2,
                payload: ProtocolMessage::QuorumCertificate(qc),
                bytes: 0,
            })
            .await
            .unwrap();

        loop {
            match recv_protocol_message(&mut peer_events).await {
                ProtocolMessage::CertifiedSyncRequest(request) => {
                    assert_eq!(request.requester_id, 1);
                    assert_eq!(request.known_qc_height, 1);
                    assert_eq!(request.from_height, 1);
                    assert!(
                        request
                            .known_qc_anchors
                            .iter()
                            .any(|anchor| anchor.block_hash == certified_root.block_hash)
                    );
                    break;
                }
                ProtocolMessage::SyncStatus { .. }
                | ProtocolMessage::SyncResponse { .. }
                | ProtocolMessage::ProposalVote(_)
                | ProtocolMessage::QuorumCertificate(_)
                | ProtocolMessage::HeartbeatPulse(_)
                | ProtocolMessage::ReceiptFetch { .. } => {}
                other => panic!("expected certified sync request, got {other:?}"),
            }
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn uncertified_extension_of_certified_head_stays_pending_until_qc() {
        let genesis = sample_genesis();
        let config = sample_node_config(
            1,
            reserve_local_address(),
            Vec::new(),
            "certified-head-only",
        );
        let mut runner = build_test_runner(1, config, genesis.clone()).await;
        let certified_root = first_valid_block(&genesis);
        let crypto = DeterministicCryptoBackend::from_genesis(&genesis);
        let consensus = ConsensusEngine::new(genesis.clone());

        assert_eq!(
            runner
                .accept_block(certified_root.clone(), true)
                .await
                .unwrap(),
            BlockAcceptance::Accepted
        );
        assert!(
            runner
                .import_proposal_vote(signed_proposal_vote(
                    runner.crypto.as_ref(),
                    2,
                    &certified_root
                ))
                .unwrap()
        );
        assert!(
            runner
                .import_proposal_vote(signed_proposal_vote(
                    runner.crypto.as_ref(),
                    3,
                    &certified_root
                ))
                .unwrap()
        );
        assert!(
            runner
                .quorum_certificates
                .contains_key(&certified_root.block_hash)
        );

        let mut root_ledger = LedgerState::from_genesis(&genesis);
        root_ledger.apply_block(&certified_root, &crypto).unwrap();
        let slot = ((certified_root.header.slot + 1)..(certified_root.header.slot + 51))
            .find(|slot| consensus.proposer_for_slot(*slot) == 2)
            .unwrap();
        let epoch = consensus.epoch_for_slot(slot);
        let transactions = Vec::new();
        let commitment: Option<TopologyCommitment> = None;
        let header = BlockHeader {
            block_number: 2,
            parent_hash: certified_root.block_hash,
            slot,
            epoch,
            proposer_id: 2,
            timestamp_unix_millis: now_unix_millis(),
            state_root: root_ledger.state_root(),
            transactions_root: canonical_hash(&transactions),
            topology_root: empty_hash(),
        };
        let block_hash = canonical_hash(&(header.clone(), &transactions, &commitment));
        let child = Block {
            header,
            transactions,
            commitment,
            commitment_receipts: Vec::new(),
            signature: crypto.sign(2, &block_hash).unwrap(),
            block_hash,
        };

        assert_eq!(
            runner.accept_block(child.clone(), false).await.unwrap(),
            BlockAcceptance::Orphan
        );
        assert_eq!(runner.ledger.snapshot().tip_hash, certified_root.block_hash);
        assert!(runner.orphan_blocks.is_empty());
        assert_eq!(
            runner
                .preferred_pending_child_for_certified_head()
                .map(|block| block.block_hash),
            Some(child.block_hash)
        );

        assert!(
            runner
                .import_proposal_vote(signed_proposal_vote(runner.crypto.as_ref(), 3, &child))
                .unwrap()
        );
        assert!(
            runner
                .import_proposal_vote(signed_proposal_vote(runner.crypto.as_ref(), 4, &child))
                .unwrap()
        );

        assert_eq!(runner.ledger.snapshot().tip_hash, child.block_hash);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn pending_certified_child_is_not_requeued_by_orphan_promotion() {
        let genesis = sample_genesis();
        let config = sample_node_config(
            1,
            reserve_local_address(),
            Vec::new(),
            "pending-certified-child-lane",
        );
        let mut runner = build_test_runner(1, config, genesis.clone()).await;
        let certified_root = first_valid_block(&genesis);
        let crypto = DeterministicCryptoBackend::from_genesis(&genesis);
        let consensus = ConsensusEngine::new(genesis.clone());

        assert_eq!(
            runner
                .accept_block(certified_root.clone(), true)
                .await
                .unwrap(),
            BlockAcceptance::Accepted
        );
        assert!(
            runner
                .import_proposal_vote(signed_proposal_vote(
                    runner.crypto.as_ref(),
                    2,
                    &certified_root
                ))
                .unwrap()
        );
        assert!(
            runner
                .import_proposal_vote(signed_proposal_vote(
                    runner.crypto.as_ref(),
                    3,
                    &certified_root
                ))
                .unwrap()
        );

        let mut root_ledger = LedgerState::from_genesis(&genesis);
        root_ledger.apply_block(&certified_root, &crypto).unwrap();
        let slot = ((certified_root.header.slot + 1)..(certified_root.header.slot + 51))
            .find(|slot| consensus.proposer_for_slot(*slot) == 2)
            .unwrap();
        let epoch = consensus.epoch_for_slot(slot);
        let transactions = Vec::new();
        let commitment: Option<TopologyCommitment> = None;
        let header = BlockHeader {
            block_number: 2,
            parent_hash: certified_root.block_hash,
            slot,
            epoch,
            proposer_id: 2,
            timestamp_unix_millis: now_unix_millis(),
            state_root: root_ledger.state_root(),
            transactions_root: canonical_hash(&transactions),
            topology_root: empty_hash(),
        };
        let block_hash = canonical_hash(&(header.clone(), &transactions, &commitment));
        let child = Block {
            header,
            transactions,
            commitment,
            commitment_receipts: Vec::new(),
            signature: crypto.sign(2, &block_hash).unwrap(),
            block_hash,
        };

        assert_eq!(
            runner.accept_block(child.clone(), false).await.unwrap(),
            BlockAcceptance::Orphan
        );
        assert!(runner.orphan_blocks.is_empty());
        assert_eq!(
            runner
                .preferred_pending_child_for_certified_head()
                .map(|block| block.block_hash),
            Some(child.block_hash)
        );

        runner.try_promote_orphans().await.unwrap();

        assert!(runner.orphan_blocks.is_empty());
        assert_eq!(
            runner
                .preferred_pending_child_for_certified_head()
                .map(|block| block.block_hash),
            Some(child.block_hash)
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn imported_qc_clears_descendant_votes_from_losing_branch() {
        let genesis = sample_genesis();
        let config = sample_node_config(
            1,
            reserve_local_address(),
            Vec::new(),
            "clear-losing-descendant-votes",
        );
        let mut runner = build_test_runner(1, config, genesis.clone()).await;
        let local_root = first_valid_block(&genesis);
        let crypto = DeterministicCryptoBackend::from_genesis(&genesis);
        let consensus = ConsensusEngine::new(genesis.clone());
        let mut local_root_ledger = LedgerState::from_genesis(&genesis);
        local_root_ledger.apply_block(&local_root, &crypto).unwrap();
        let local_child_slot = ((local_root.header.slot + 1)..(local_root.header.slot + 51))
            .find(|slot| consensus.proposer_for_slot(*slot) == 2)
            .unwrap();
        let local_child_epoch = consensus.epoch_for_slot(local_child_slot);
        let local_child_transactions = Vec::new();
        let local_child_commitment: Option<TopologyCommitment> = None;
        let local_child_header = BlockHeader {
            block_number: 2,
            parent_hash: local_root.block_hash,
            slot: local_child_slot,
            epoch: local_child_epoch,
            proposer_id: 2,
            timestamp_unix_millis: now_unix_millis(),
            state_root: local_root_ledger.state_root(),
            transactions_root: canonical_hash(&local_child_transactions),
            topology_root: empty_hash(),
        };
        let local_child_hash = canonical_hash(&(
            local_child_header.clone(),
            &local_child_transactions,
            &local_child_commitment,
        ));
        let local_child = Block {
            header: local_child_header,
            transactions: local_child_transactions,
            commitment: local_child_commitment,
            commitment_receipts: Vec::new(),
            signature: crypto.sign(2, &local_child_hash).unwrap(),
            block_hash: local_child_hash,
        };
        let certified_root =
            valid_empty_block_on_parent(&genesis, empty_hash(), 1, 3, local_root.header.slot + 1);

        assert_eq!(
            runner.accept_block(local_root.clone(), true).await.unwrap(),
            BlockAcceptance::Accepted
        );
        assert_eq!(
            runner
                .accept_block(local_child.clone(), true)
                .await
                .unwrap(),
            BlockAcceptance::Accepted
        );
        assert!(
            runner
                .proposal_votes
                .get(&local_child.block_hash)
                .and_then(|votes| votes.get(&1))
                .is_some()
        );

        assert_eq!(
            runner
                .accept_block(certified_root.clone(), false)
                .await
                .unwrap(),
            BlockAcceptance::Orphan
        );

        let qc_votes = vec![
            signed_proposal_vote(runner.crypto.as_ref(), 2, &certified_root),
            signed_proposal_vote(runner.crypto.as_ref(), 3, &certified_root),
            signed_proposal_vote(runner.crypto.as_ref(), 4, &certified_root),
        ];
        let qc = QuorumCertificate {
            block_hash: certified_root.block_hash,
            block_number: certified_root.header.block_number,
            epoch: certified_root.header.epoch,
            slot: certified_root.header.slot,
            vote_root: entangrid_types::quorum_certificate_vote_root(&qc_votes),
            votes: qc_votes,
        };

        assert!(runner.import_quorum_certificate(qc).unwrap());
        assert_eq!(runner.ledger.snapshot().tip_hash, certified_root.block_hash);

        let mut certified_root_ledger = LedgerState::from_genesis(&genesis);
        certified_root_ledger
            .apply_block(&certified_root, &crypto)
            .unwrap();
        let certified_child_slot = ((certified_root.header.slot + 1)
            ..(certified_root.header.slot + 51))
            .find(|slot| consensus.proposer_for_slot(*slot) == 4)
            .unwrap();
        let certified_child_epoch = consensus.epoch_for_slot(certified_child_slot);
        let certified_child_transactions = Vec::new();
        let certified_child_commitment: Option<TopologyCommitment> = None;
        let certified_child_header = BlockHeader {
            block_number: 2,
            parent_hash: certified_root.block_hash,
            slot: certified_child_slot,
            epoch: certified_child_epoch,
            proposer_id: 4,
            timestamp_unix_millis: now_unix_millis(),
            state_root: certified_root_ledger.state_root(),
            transactions_root: canonical_hash(&certified_child_transactions),
            topology_root: empty_hash(),
        };
        let certified_child_hash = canonical_hash(&(
            certified_child_header.clone(),
            &certified_child_transactions,
            &certified_child_commitment,
        ));
        let certified_child = Block {
            header: certified_child_header,
            transactions: certified_child_transactions,
            commitment: certified_child_commitment,
            commitment_receipts: Vec::new(),
            signature: crypto.sign(4, &certified_child_hash).unwrap(),
            block_hash: certified_child_hash,
        };
        assert_eq!(
            runner
                .accept_block(certified_child.clone(), false)
                .await
                .unwrap(),
            BlockAcceptance::Orphan
        );

        runner
            .maybe_broadcast_local_proposal_vote(&certified_child)
            .unwrap();

        assert!(
            runner
                .proposal_votes
                .get(&certified_child.block_hash)
                .and_then(|votes| votes.get(&1))
                .is_some()
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn earlier_slot_child_replaces_local_same_height_vote_under_certified_head() {
        let genesis = sample_genesis();
        let config = sample_node_config(
            1,
            reserve_local_address(),
            Vec::new(),
            "same-height-vote-replacement",
        );
        let mut runner = build_test_runner(1, config, genesis.clone()).await;
        let certified_root = first_valid_block(&genesis);
        let crypto = DeterministicCryptoBackend::from_genesis(&genesis);
        let consensus = ConsensusEngine::new(genesis.clone());

        assert_eq!(
            runner
                .accept_block(certified_root.clone(), true)
                .await
                .unwrap(),
            BlockAcceptance::Accepted
        );
        assert!(
            runner
                .import_proposal_vote(signed_proposal_vote(
                    runner.crypto.as_ref(),
                    2,
                    &certified_root
                ))
                .unwrap()
        );
        assert!(
            runner
                .import_proposal_vote(signed_proposal_vote(
                    runner.crypto.as_ref(),
                    3,
                    &certified_root
                ))
                .unwrap()
        );
        assert!(
            runner
                .quorum_certificates
                .contains_key(&certified_root.block_hash)
        );

        let mut root_ledger = LedgerState::from_genesis(&genesis);
        root_ledger.apply_block(&certified_root, &crypto).unwrap();
        let build_child = |proposer_id: ValidatorId, min_slot: u64| {
            let slot = (min_slot..(min_slot + 50))
                .find(|slot| consensus.proposer_for_slot(*slot) == proposer_id)
                .unwrap();
            let epoch = consensus.epoch_for_slot(slot);
            let transactions = Vec::new();
            let commitment: Option<TopologyCommitment> = None;
            let header = BlockHeader {
                block_number: 2,
                parent_hash: certified_root.block_hash,
                slot,
                epoch,
                proposer_id,
                timestamp_unix_millis: now_unix_millis(),
                state_root: root_ledger.state_root(),
                transactions_root: canonical_hash(&transactions),
                topology_root: empty_hash(),
            };
            let block_hash = canonical_hash(&(header.clone(), &transactions, &commitment));
            Block {
                header,
                transactions,
                commitment,
                commitment_receipts: Vec::new(),
                signature: crypto.sign(proposer_id, &block_hash).unwrap(),
                block_hash,
            }
        };

        let later_child = build_child(4, certified_root.header.slot + 3);
        let earlier_child = build_child(2, certified_root.header.slot + 1);
        assert!(earlier_child.header.slot < later_child.header.slot);

        assert_eq!(
            runner
                .accept_block(later_child.clone(), false)
                .await
                .unwrap(),
            BlockAcceptance::Orphan
        );
        assert!(
            runner
                .proposal_votes
                .get(&later_child.block_hash)
                .and_then(|votes| votes.get(&1))
                .is_some()
        );

        assert_eq!(
            runner
                .accept_block(earlier_child.clone(), false)
                .await
                .unwrap(),
            BlockAcceptance::Orphan
        );
        assert!(
            runner
                .proposal_votes
                .get(&earlier_child.block_hash)
                .and_then(|votes| votes.get(&1))
                .is_some()
        );
        assert!(
            runner
                .proposal_votes
                .get(&later_child.block_hash)
                .and_then(|votes| votes.get(&1))
                .is_none()
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn proposal_vote_for_unknown_block_is_buffered_until_block_arrives() {
        let genesis = sample_genesis();
        let config = sample_node_config(
            1,
            reserve_local_address(),
            Vec::new(),
            "buffer-unknown-vote",
        );
        let mut runner = build_test_runner(1, config, genesis.clone()).await;
        let block = first_valid_block(&genesis);

        assert!(
            runner
                .import_proposal_vote(signed_proposal_vote(runner.crypto.as_ref(), 2, &block))
                .unwrap()
        );
        assert!(
            runner
                .import_proposal_vote(signed_proposal_vote(runner.crypto.as_ref(), 3, &block))
                .unwrap()
        );

        assert_eq!(
            runner.accept_block(block.clone(), false).await.unwrap(),
            BlockAcceptance::Accepted
        );
        assert!(runner.quorum_certificates.contains_key(&block.block_hash));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn validator_cannot_vote_for_two_children_at_same_block_height() {
        let genesis = sample_genesis();
        let config = sample_node_config(
            1,
            reserve_local_address(),
            Vec::new(),
            "same-height-vote-conflict",
        );
        let mut runner = build_test_runner(1, config, genesis.clone()).await;
        let certified_root = first_valid_block(&genesis);
        let crypto = DeterministicCryptoBackend::from_genesis(&genesis);
        let consensus = ConsensusEngine::new(genesis.clone());

        assert_eq!(
            runner
                .accept_block(certified_root.clone(), true)
                .await
                .unwrap(),
            BlockAcceptance::Accepted
        );
        assert!(
            runner
                .import_proposal_vote(signed_proposal_vote(
                    runner.crypto.as_ref(),
                    2,
                    &certified_root
                ))
                .unwrap()
        );
        assert!(
            runner
                .import_proposal_vote(signed_proposal_vote(
                    runner.crypto.as_ref(),
                    3,
                    &certified_root
                ))
                .unwrap()
        );
        assert!(
            runner
                .quorum_certificates
                .contains_key(&certified_root.block_hash)
        );

        let mut root_ledger = LedgerState::from_genesis(&genesis);
        root_ledger.apply_block(&certified_root, &crypto).unwrap();
        let build_child = |proposer_id: ValidatorId, min_slot: u64| {
            let slot = (min_slot..(min_slot + 50))
                .find(|slot| consensus.proposer_for_slot(*slot) == proposer_id)
                .unwrap();
            let epoch = consensus.epoch_for_slot(slot);
            let transactions = Vec::new();
            let commitment: Option<TopologyCommitment> = None;
            let header = BlockHeader {
                block_number: 2,
                parent_hash: certified_root.block_hash,
                slot,
                epoch,
                proposer_id,
                timestamp_unix_millis: now_unix_millis(),
                state_root: root_ledger.state_root(),
                transactions_root: canonical_hash(&transactions),
                topology_root: empty_hash(),
            };
            let block_hash = canonical_hash(&(header.clone(), &transactions, &commitment));
            Block {
                header,
                transactions,
                commitment,
                commitment_receipts: Vec::new(),
                signature: crypto.sign(proposer_id, &block_hash).unwrap(),
                block_hash,
            }
        };

        let child_a = build_child(2, certified_root.header.slot + 1);
        let child_b = build_child(3, child_a.header.slot + 1);

        assert_eq!(
            runner.accept_block(child_a.clone(), false).await.unwrap(),
            BlockAcceptance::Orphan
        );
        assert_eq!(
            runner.accept_block(child_b.clone(), false).await.unwrap(),
            BlockAcceptance::Orphan
        );

        assert!(
            runner
                .import_proposal_vote(signed_proposal_vote(runner.crypto.as_ref(), 4, &child_a))
                .unwrap()
        );
        assert!(
            runner
                .import_proposal_vote(signed_proposal_vote(runner.crypto.as_ref(), 4, &child_b))
                .is_err()
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn replayable_branch_stays_orphan_until_certified_in_v2() {
        let genesis = sample_genesis();
        let config = sample_node_config(1, reserve_local_address(), Vec::new(), "prefer-branch");
        let mut runner = build_test_runner(1, config, genesis.clone()).await;
        let certified_root = first_valid_block(&genesis);
        let crypto = DeterministicCryptoBackend::from_genesis(&genesis);
        let consensus = ConsensusEngine::new(genesis.clone());

        assert_eq!(
            runner
                .accept_block(certified_root.clone(), true)
                .await
                .unwrap(),
            BlockAcceptance::Accepted
        );
        assert!(
            runner
                .import_proposal_vote(signed_proposal_vote(
                    runner.crypto.as_ref(),
                    2,
                    &certified_root
                ))
                .unwrap()
        );
        assert!(
            runner
                .import_proposal_vote(signed_proposal_vote(
                    runner.crypto.as_ref(),
                    3,
                    &certified_root
                ))
                .unwrap()
        );
        assert!(
            runner
                .quorum_certificates
                .contains_key(&certified_root.block_hash)
        );

        let mut root_ledger = LedgerState::from_genesis(&genesis);
        root_ledger.apply_block(&certified_root, &crypto).unwrap();
        let build_child = |proposer_id: ValidatorId, min_slot: u64| {
            let slot = (min_slot..(min_slot + 50))
                .find(|slot| consensus.proposer_for_slot(*slot) == proposer_id)
                .unwrap();
            let epoch = consensus.epoch_for_slot(slot);
            let transactions = Vec::new();
            let commitment: Option<TopologyCommitment> = None;
            let header = BlockHeader {
                block_number: 2,
                parent_hash: certified_root.block_hash,
                slot,
                epoch,
                proposer_id,
                timestamp_unix_millis: now_unix_millis(),
                state_root: root_ledger.state_root(),
                transactions_root: canonical_hash(&transactions),
                topology_root: empty_hash(),
            };
            let block_hash = canonical_hash(&(header.clone(), &transactions, &commitment));
            Block {
                header,
                transactions,
                commitment,
                commitment_receipts: Vec::new(),
                signature: crypto.sign(proposer_id, &block_hash).unwrap(),
                block_hash,
            }
        };
        let current_tip = build_child(2, certified_root.header.slot + 1);
        let competing_tip = build_child(3, current_tip.header.slot + 1);

        assert_eq!(
            runner
                .accept_block(current_tip.clone(), false)
                .await
                .unwrap(),
            BlockAcceptance::Orphan
        );
        assert_eq!(runner.ledger.snapshot().tip_hash, certified_root.block_hash);
        let mut competing_chain = runner
            .build_parent_chain_for_block(&competing_tip)
            .unwrap()
            .unwrap();
        competing_chain.push(competing_tip.clone());
        assert_eq!(
            runner.highest_locked_qc_hash(),
            Some(certified_root.block_hash)
        );
        assert_eq!(runner.proposal_votes[&current_tip.block_hash].len(), 1);
        assert!(
            runner
                .proposal_votes
                .get(&competing_tip.block_hash)
                .is_none()
        );

        assert_eq!(
            runner
                .accept_block(competing_tip.clone(), false)
                .await
                .unwrap(),
            BlockAcceptance::Orphan
        );
        assert_eq!(runner.ledger.snapshot().tip_hash, certified_root.block_hash);
        assert_eq!(
            runner.blocks.last().unwrap().block_hash,
            certified_root.block_hash
        );
        assert!(
            runner
                .pending_certified_children
                .iter()
                .any(|block| block.block_hash == competing_tip.block_hash)
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn equal_qc_height_keeps_current_branch_until_certified_suffix_is_better() {
        let genesis = sample_genesis();
        let config = sample_node_config(1, reserve_local_address(), Vec::new(), "vote-branch");
        let mut runner = build_test_runner(1, config, genesis.clone()).await;
        let certified_root = first_valid_block(&genesis);
        let crypto = DeterministicCryptoBackend::from_genesis(&genesis);
        let consensus = ConsensusEngine::new(genesis.clone());

        assert_eq!(
            runner
                .accept_block(certified_root.clone(), true)
                .await
                .unwrap(),
            BlockAcceptance::Accepted
        );
        assert!(
            runner
                .import_proposal_vote(signed_proposal_vote(
                    runner.crypto.as_ref(),
                    2,
                    &certified_root
                ))
                .unwrap()
        );
        assert!(
            runner
                .import_proposal_vote(signed_proposal_vote(
                    runner.crypto.as_ref(),
                    3,
                    &certified_root
                ))
                .unwrap()
        );
        assert!(
            runner
                .quorum_certificates
                .contains_key(&certified_root.block_hash)
        );

        let mut root_ledger = LedgerState::from_genesis(&genesis);
        root_ledger.apply_block(&certified_root, &crypto).unwrap();
        let build_child = |proposer_id: ValidatorId, min_slot: u64| {
            let slot = (min_slot..(min_slot + 50))
                .find(|slot| consensus.proposer_for_slot(*slot) == proposer_id)
                .unwrap();
            let epoch = consensus.epoch_for_slot(slot);
            let transactions = Vec::new();
            let commitment: Option<TopologyCommitment> = None;
            let header = BlockHeader {
                block_number: 2,
                parent_hash: certified_root.block_hash,
                slot,
                epoch,
                proposer_id,
                timestamp_unix_millis: now_unix_millis(),
                state_root: root_ledger.state_root(),
                transactions_root: canonical_hash(&transactions),
                topology_root: empty_hash(),
            };
            let block_hash = canonical_hash(&(header.clone(), &transactions, &commitment));
            Block {
                header,
                transactions,
                commitment,
                commitment_receipts: Vec::new(),
                signature: crypto.sign(proposer_id, &block_hash).unwrap(),
                block_hash,
            }
        };

        let competing_tip = build_child(2, certified_root.header.slot + 1);
        let current_tip = build_child(3, competing_tip.header.slot + 1);

        assert_eq!(
            runner
                .accept_block(current_tip.clone(), false)
                .await
                .unwrap(),
            BlockAcceptance::Orphan
        );
        assert_eq!(
            runner
                .accept_block(competing_tip.clone(), false)
                .await
                .unwrap(),
            BlockAcceptance::Orphan
        );
        assert_eq!(runner.ledger.snapshot().tip_hash, certified_root.block_hash);

        assert!(
            runner
                .import_proposal_vote(signed_proposal_vote(
                    runner.crypto.as_ref(),
                    2,
                    &competing_tip
                ))
                .unwrap()
        );
        assert_eq!(runner.ledger.snapshot().tip_hash, certified_root.block_hash);

        assert!(
            runner
                .import_proposal_vote(signed_proposal_vote(
                    runner.crypto.as_ref(),
                    3,
                    &competing_tip
                ))
                .unwrap()
        );
        assert!(
            runner
                .import_proposal_vote(signed_proposal_vote(
                    runner.crypto.as_ref(),
                    4,
                    &competing_tip
                ))
                .unwrap()
        );
        assert!(
            runner
                .quorum_certificates
                .contains_key(&competing_tip.block_hash)
        );
        assert_eq!(runner.ledger.snapshot().tip_hash, competing_tip.block_hash);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn replayable_orphan_block_skips_local_vote_if_validator_already_voted_at_height() {
        let genesis = sample_genesis();
        let config = sample_node_config(1, reserve_local_address(), Vec::new(), "orphan-vote");
        let mut runner = build_test_runner(1, config, genesis.clone()).await;
        let current_tip = first_valid_block(&genesis);
        let competing_tip =
            valid_empty_block_on_parent(&genesis, empty_hash(), 1, 2, current_tip.header.slot + 1);

        assert_eq!(
            runner
                .accept_block(current_tip.clone(), true)
                .await
                .unwrap(),
            BlockAcceptance::Accepted
        );
        assert_eq!(
            runner
                .accept_block(competing_tip.clone(), false)
                .await
                .unwrap(),
            BlockAcceptance::Orphan
        );

        assert!(
            runner
                .proposal_votes
                .get(&competing_tip.block_hash)
                .and_then(|votes| votes.get(&runner.config.validator_id))
                .is_none()
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn disconnected_orphan_block_does_not_record_local_vote() {
        let genesis = sample_genesis();
        let config = sample_node_config(1, reserve_local_address(), Vec::new(), "orphan-no-vote");
        let mut runner = build_test_runner(1, config, genesis.clone()).await;
        let disconnected_tip =
            valid_empty_block_on_parent(&genesis, [9u8; 32], 2, 2, genesis.slots_per_epoch + 1);

        assert_eq!(
            runner
                .accept_block(disconnected_tip.clone(), false)
                .await
                .unwrap(),
            BlockAcceptance::Orphan
        );

        assert!(
            runner
                .proposal_votes
                .get(&disconnected_tip.block_hash)
                .is_none()
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn competing_branch_without_locked_qc_does_not_receive_local_vote() {
        let genesis = sample_genesis();
        let config = sample_node_config(1, reserve_local_address(), Vec::new(), "qc-lock");
        let mut runner = build_test_runner(1, config, genesis.clone()).await;
        let locked_tip = first_valid_block(&genesis);
        let competing_tip =
            valid_empty_block_on_parent(&genesis, empty_hash(), 1, 2, locked_tip.header.slot + 1);

        assert_eq!(
            runner.accept_block(locked_tip.clone(), true).await.unwrap(),
            BlockAcceptance::Accepted
        );
        assert!(
            runner
                .import_proposal_vote(signed_proposal_vote(runner.crypto.as_ref(), 2, &locked_tip))
                .unwrap()
        );
        assert!(
            runner
                .import_proposal_vote(signed_proposal_vote(runner.crypto.as_ref(), 3, &locked_tip))
                .unwrap()
        );
        assert!(
            runner
                .quorum_certificates
                .contains_key(&locked_tip.block_hash)
        );

        assert_eq!(
            runner
                .accept_block(competing_tip.clone(), false)
                .await
                .unwrap(),
            BlockAcceptance::Orphan
        );
        assert!(
            runner
                .proposal_votes
                .get(&competing_tip.block_hash)
                .and_then(|votes| votes.get(&runner.config.validator_id))
                .is_none()
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn invalid_proposal_vote_signature_is_rejected() {
        let genesis = sample_genesis();
        let config = sample_node_config(1, reserve_local_address(), Vec::new(), "bad-vote");
        let mut runner = build_test_runner(1, config, genesis.clone()).await;
        let block = first_valid_block(&genesis);

        assert_eq!(
            runner.accept_block(block.clone(), true).await.unwrap(),
            BlockAcceptance::Accepted
        );

        let mut vote = signed_proposal_vote(runner.crypto.as_ref(), 2, &block);
        vote.signature = TypedSignature::single(SignatureScheme::DevDeterministic, vec![7, 7, 7]);

        let error = runner.import_proposal_vote(vote).unwrap_err();

        assert!(
            error
                .to_string()
                .contains("invalid proposal vote signature")
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn certified_sync_repairs_diverged_suffix_from_highest_shared_qc() {
        let genesis = sample_genesis();
        let responder_config = sample_node_config(
            1,
            reserve_local_address(),
            Vec::new(),
            "certified-sync-source",
        );
        let requester_config = sample_node_config(
            1,
            reserve_local_address(),
            Vec::new(),
            "certified-sync-target",
        );
        let mut responder = build_test_runner(1, responder_config, genesis.clone()).await;
        let mut requester = build_test_runner(1, requester_config, genesis.clone()).await;
        let shared_root = first_valid_block(&genesis);
        let crypto = DeterministicCryptoBackend::from_genesis(&genesis);
        let consensus = ConsensusEngine::new(genesis.clone());

        for runner in [&mut responder, &mut requester] {
            assert_eq!(
                runner
                    .accept_block(shared_root.clone(), true)
                    .await
                    .unwrap(),
                BlockAcceptance::Accepted
            );
            assert!(
                runner
                    .import_proposal_vote(signed_proposal_vote(
                        runner.crypto.as_ref(),
                        2,
                        &shared_root
                    ))
                    .unwrap()
            );
            assert!(
                runner
                    .import_proposal_vote(signed_proposal_vote(
                        runner.crypto.as_ref(),
                        3,
                        &shared_root
                    ))
                    .unwrap()
            );
            assert!(
                runner
                    .quorum_certificates
                    .contains_key(&shared_root.block_hash)
            );
        }

        let mut root_ledger = LedgerState::from_genesis(&genesis);
        root_ledger.apply_block(&shared_root, &crypto).unwrap();
        let build_child = |proposer_id: ValidatorId, min_slot: u64| {
            let slot = (min_slot..(min_slot + 50))
                .find(|slot| consensus.proposer_for_slot(*slot) == proposer_id)
                .unwrap();
            let epoch = consensus.epoch_for_slot(slot);
            let transactions = Vec::new();
            let commitment: Option<TopologyCommitment> = None;
            let header = BlockHeader {
                block_number: 2,
                parent_hash: shared_root.block_hash,
                slot,
                epoch,
                proposer_id,
                timestamp_unix_millis: now_unix_millis(),
                state_root: root_ledger.state_root(),
                transactions_root: canonical_hash(&transactions),
                topology_root: empty_hash(),
            };
            let block_hash = canonical_hash(&(header.clone(), &transactions, &commitment));
            Block {
                header,
                transactions,
                commitment,
                commitment_receipts: Vec::new(),
                signature: crypto.sign(proposer_id, &block_hash).unwrap(),
                block_hash,
            }
        };
        let local_tip = build_child(2, shared_root.header.slot + 1);
        let certified_peer_tip = build_child(3, local_tip.header.slot + 1);

        assert_eq!(
            requester
                .accept_block(local_tip.clone(), false)
                .await
                .unwrap(),
            BlockAcceptance::Orphan
        );
        assert_eq!(
            responder
                .accept_block(certified_peer_tip.clone(), false)
                .await
                .unwrap(),
            BlockAcceptance::Orphan
        );
        assert!(
            responder
                .import_proposal_vote(signed_proposal_vote(
                    responder.crypto.as_ref(),
                    2,
                    &certified_peer_tip
                ))
                .unwrap()
        );
        assert!(
            responder
                .import_proposal_vote(signed_proposal_vote(
                    responder.crypto.as_ref(),
                    4,
                    &certified_peer_tip
                ))
                .unwrap()
        );
        assert!(
            responder
                .quorum_certificates
                .contains_key(&certified_peer_tip.block_hash)
        );
        assert_eq!(requester.ledger.snapshot().tip_hash, shared_root.block_hash);

        let response = responder.build_certified_sync_response_for_peer(&[SyncQcAnchor {
            block_hash: shared_root.block_hash,
            block_number: shared_root.header.block_number,
        }]);
        assert!(matches!(response, ChunkedSyncResponse::Certified { .. }));
        assert!(requester.import_certified_sync_response(response).unwrap());
        assert_eq!(
            requester.ledger.snapshot().tip_hash,
            certified_peer_tip.block_hash
        );
    }

    #[test]
    fn certified_sync_adoption_prefers_higher_qc_height_over_taller_uncertified_local_tip() {
        let local = StateSnapshot {
            balances: BTreeMap::new(),
            nonces: BTreeMap::new(),
            tip_hash: canonical_hash(&"local-taller"),
            height: 36,
            last_slot: 100,
        };
        let candidate = StateSnapshot {
            balances: BTreeMap::new(),
            nonces: BTreeMap::new(),
            tip_hash: canonical_hash(&"candidate-certified"),
            height: 35,
            last_slot: 99,
        };

        assert!(should_adopt_certified_snapshot(&local, 28, &candidate, 35));
        assert!(!should_adopt_certified_snapshot(&candidate, 35, &local, 28));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn certified_sync_is_unavailable_without_shared_qc() {
        let genesis = sample_genesis();
        let config = sample_node_config(
            1,
            reserve_local_address(),
            Vec::new(),
            "certified-sync-miss",
        );
        let responder = build_test_runner(1, config, genesis).await;

        assert!(matches!(
            responder.build_certified_sync_response_for_peer(&[]),
            ChunkedSyncResponse::Unavailable { .. }
        ));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn incremental_sync_can_replace_divergent_uncertified_suffix_above_certified_frontier() {
        let genesis = sample_genesis();
        let config = sample_node_config(
            1,
            reserve_local_address(),
            Vec::new(),
            "incremental-sync-certified-anchor-repair",
        );
        let mut runner = build_test_runner(1, config, genesis.clone()).await;
        let shared_root = first_valid_block(&genesis);
        let crypto = DeterministicCryptoBackend::from_genesis(&genesis);
        let consensus = ConsensusEngine::new(genesis.clone());

        assert_eq!(
            runner
                .accept_block(shared_root.clone(), true)
                .await
                .unwrap(),
            BlockAcceptance::Accepted
        );
        assert!(
            runner
                .import_proposal_vote(signed_proposal_vote(
                    runner.crypto.as_ref(),
                    2,
                    &shared_root
                ))
                .unwrap()
        );
        assert!(
            runner
                .import_proposal_vote(signed_proposal_vote(
                    runner.crypto.as_ref(),
                    3,
                    &shared_root
                ))
                .unwrap()
        );

        let mut root_ledger = LedgerState::from_genesis(&genesis);
        root_ledger.apply_block(&shared_root, &crypto).unwrap();
        let build_child = |parent: &Block,
                           parent_ledger: &LedgerState,
                           block_number: u64,
                           proposer_id: ValidatorId,
                           min_slot: u64| {
            let slot = (min_slot..(min_slot + 100))
                .find(|slot| consensus.proposer_for_slot(*slot) == proposer_id)
                .unwrap();
            let epoch = consensus.epoch_for_slot(slot);
            let transactions = Vec::new();
            let commitment: Option<TopologyCommitment> = None;
            let header = BlockHeader {
                block_number,
                parent_hash: parent.block_hash,
                slot,
                epoch,
                proposer_id,
                timestamp_unix_millis: now_unix_millis(),
                state_root: parent_ledger.state_root(),
                transactions_root: canonical_hash(&transactions),
                topology_root: empty_hash(),
            };
            let block_hash = canonical_hash(&(header.clone(), &transactions, &commitment));
            Block {
                header,
                transactions,
                commitment,
                commitment_receipts: Vec::new(),
                signature: crypto.sign(proposer_id, &block_hash).unwrap(),
                block_hash,
            }
        };

        let local_divergent_child = build_child(
            &shared_root,
            &root_ledger,
            2,
            2,
            shared_root.header.slot + 1,
        );
        let remote_child = build_child(
            &shared_root,
            &root_ledger,
            2,
            3,
            local_divergent_child.header.slot + 1,
        );
        let mut remote_ledger = root_ledger.clone();
        remote_ledger.apply_block(&remote_child, &crypto).unwrap();
        let remote_tip = build_child(
            &remote_child,
            &remote_ledger,
            3,
            4,
            remote_child.header.slot + 1,
        );

        runner.ledger = LedgerState::replay_blocks(
            &genesis,
            &[shared_root.clone(), local_divergent_child],
            &crypto,
        )
        .unwrap();
        runner.blocks = vec![shared_root.clone()];
        runner.blocks.push(build_child(
            &shared_root,
            &root_ledger,
            2,
            2,
            shared_root.header.slot + 1,
        ));
        runner.rebuild_seen_sets();

        let target_snapshot = LedgerState::replay_blocks(
            &genesis,
            &[
                shared_root.clone(),
                remote_child.clone(),
                remote_tip.clone(),
            ],
            &crypto,
        )
        .unwrap()
        .snapshot()
        .clone();
        let segment = ChainSegment {
            base_height: shared_root.header.block_number,
            base_tip_hash: shared_root.block_hash,
            target_snapshot,
            blocks: vec![remote_child.clone(), remote_tip.clone()],
            receipts: Vec::new(),
            proposal_votes: Vec::new(),
        };

        runner.apply_chain_segment(segment).unwrap();

        assert_eq!(runner.ledger.snapshot().tip_hash, remote_tip.block_hash);
        assert_eq!(
            runner.ledger.snapshot().height,
            remote_tip.header.block_number
        );
        assert_eq!(runner.blocks.len(), 3);
        assert_eq!(runner.blocks[1].block_hash, remote_child.block_hash);
        assert_eq!(runner.blocks[2].block_hash, remote_tip.block_hash);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn sync_blocks_handler_applies_certified_anchor_repair_even_when_target_is_shorter() {
        let genesis = sample_genesis();
        let config = sample_node_config(
            1,
            reserve_local_address(),
            vec![PeerConfig {
                validator_id: 2,
                address: reserve_local_address(),
            }],
            "sync-blocks-certified-anchor-shorter-target",
        );
        let mut runner = build_test_runner(1, config, genesis.clone()).await;
        let shared_root = first_valid_block(&genesis);
        let crypto = DeterministicCryptoBackend::from_genesis(&genesis);
        let consensus = ConsensusEngine::new(genesis.clone());

        assert_eq!(
            runner
                .accept_block(shared_root.clone(), true)
                .await
                .unwrap(),
            BlockAcceptance::Accepted
        );
        assert!(
            runner
                .import_proposal_vote(signed_proposal_vote(
                    runner.crypto.as_ref(),
                    2,
                    &shared_root
                ))
                .unwrap()
        );
        assert!(
            runner
                .import_proposal_vote(signed_proposal_vote(
                    runner.crypto.as_ref(),
                    3,
                    &shared_root
                ))
                .unwrap()
        );

        let mut root_ledger = LedgerState::from_genesis(&genesis);
        root_ledger.apply_block(&shared_root, &crypto).unwrap();
        let build_child = |parent: &Block,
                           parent_ledger: &LedgerState,
                           block_number: u64,
                           proposer_id: ValidatorId,
                           min_slot: u64| {
            let slot = (min_slot..(min_slot + 100))
                .find(|slot| consensus.proposer_for_slot(*slot) == proposer_id)
                .unwrap();
            let epoch = consensus.epoch_for_slot(slot);
            let transactions = Vec::new();
            let commitment: Option<TopologyCommitment> = None;
            let header = BlockHeader {
                block_number,
                parent_hash: parent.block_hash,
                slot,
                epoch,
                proposer_id,
                timestamp_unix_millis: now_unix_millis(),
                state_root: parent_ledger.state_root(),
                transactions_root: canonical_hash(&transactions),
                topology_root: empty_hash(),
            };
            let block_hash = canonical_hash(&(header.clone(), &transactions, &commitment));
            Block {
                header,
                transactions,
                commitment,
                commitment_receipts: Vec::new(),
                signature: crypto.sign(proposer_id, &block_hash).unwrap(),
                block_hash,
            }
        };

        let local_child = build_child(
            &shared_root,
            &root_ledger,
            2,
            2,
            shared_root.header.slot + 1,
        );
        let mut local_ledger = root_ledger.clone();
        local_ledger.apply_block(&local_child, &crypto).unwrap();
        let local_tip = build_child(
            &local_child,
            &local_ledger,
            3,
            3,
            local_child.header.slot + 1,
        );
        let remote_child = build_child(&shared_root, &root_ledger, 2, 4, local_tip.header.slot + 1);

        runner.ledger = LedgerState::replay_blocks(
            &genesis,
            &[shared_root.clone(), local_child.clone(), local_tip.clone()],
            &crypto,
        )
        .unwrap();
        runner.blocks = vec![shared_root.clone(), local_child, local_tip];
        runner.rebuild_seen_sets();

        let target_snapshot = LedgerState::replay_blocks(
            &genesis,
            &[shared_root.clone(), remote_child.clone()],
            &crypto,
        )
        .unwrap()
        .snapshot()
        .clone();
        let segment = ChainSegment {
            base_height: shared_root.header.block_number,
            base_tip_hash: shared_root.block_hash,
            target_snapshot,
            blocks: vec![remote_child.clone()],
            receipts: Vec::new(),
            proposal_votes: Vec::new(),
        };

        runner
            .handle_network_event(NetworkEvent::Received {
                from_validator_id: 2,
                payload: ProtocolMessage::SyncBlocks {
                    responder_id: 2,
                    chain: segment,
                },
                bytes: 0,
            })
            .await
            .unwrap();

        assert_eq!(runner.ledger.snapshot().height, 2);
        assert_eq!(runner.ledger.snapshot().tip_hash, remote_child.block_hash);
        assert_eq!(runner.blocks.len(), 2);
        assert_eq!(runner.blocks[1].block_hash, remote_child.block_hash);
        assert_eq!(runner.snapshot_metrics().incremental_sync_applied, 1);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn v2_sync_request_hint_anchors_to_highest_qc_when_peer_is_ahead() {
        let genesis = sample_genesis();
        let config = sample_node_config(
            1,
            reserve_local_address(),
            vec![PeerConfig {
                validator_id: 2,
                address: reserve_local_address(),
            }],
            "v2-sync-request-certified-anchor",
        );
        let mut runner = build_test_runner(1, config, genesis.clone()).await;
        let shared_root = first_valid_block(&genesis);
        let crypto = DeterministicCryptoBackend::from_genesis(&genesis);
        let consensus = ConsensusEngine::new(genesis.clone());

        assert_eq!(
            runner
                .accept_block(shared_root.clone(), true)
                .await
                .unwrap(),
            BlockAcceptance::Accepted
        );
        assert!(
            runner
                .import_proposal_vote(signed_proposal_vote(
                    runner.crypto.as_ref(),
                    2,
                    &shared_root
                ))
                .unwrap()
        );
        assert!(
            runner
                .import_proposal_vote(signed_proposal_vote(
                    runner.crypto.as_ref(),
                    3,
                    &shared_root
                ))
                .unwrap()
        );

        let mut root_ledger = LedgerState::from_genesis(&genesis);
        root_ledger.apply_block(&shared_root, &crypto).unwrap();
        let slot = ((shared_root.header.slot + 1)..(shared_root.header.slot + 50))
            .find(|slot| consensus.proposer_for_slot(*slot) == 2)
            .unwrap();
        let epoch = consensus.epoch_for_slot(slot);
        let transactions = Vec::new();
        let commitment: Option<TopologyCommitment> = None;
        let header = BlockHeader {
            block_number: 2,
            parent_hash: shared_root.block_hash,
            slot,
            epoch,
            proposer_id: 2,
            timestamp_unix_millis: now_unix_millis(),
            state_root: root_ledger.state_root(),
            transactions_root: canonical_hash(&transactions),
            topology_root: empty_hash(),
        };
        let block_hash = canonical_hash(&(header.clone(), &transactions, &commitment));
        let local_child = Block {
            header,
            transactions,
            commitment,
            commitment_receipts: Vec::new(),
            signature: crypto.sign(2, &block_hash).unwrap(),
            block_hash,
        };
        runner.ledger = LedgerState::replay_blocks(
            &genesis,
            &[shared_root.clone(), local_child.clone()],
            &crypto,
        )
        .unwrap();
        runner.blocks = vec![shared_root.clone(), local_child];
        runner.rebuild_seen_sets();

        runner.record_peer_sync_status(
            2,
            3,
            canonical_hash(&"peer-ahead-tip"),
            Some(shared_root.block_hash),
            shared_root.header.block_number,
            vec![SyncQcAnchor {
                block_hash: shared_root.block_hash,
                block_number: shared_root.header.block_number,
            }],
        );

        let (known_height, known_tip_hash) = runner.sync_request_hint_for_peer(2);
        assert_eq!(known_height, shared_root.header.block_number);
        assert_eq!(known_tip_hash, shared_root.block_hash);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn certified_sync_uses_older_shared_qc_anchor_when_latest_is_unknown() {
        let genesis = sample_genesis();
        let responder_config = sample_node_config(
            1,
            reserve_local_address(),
            Vec::new(),
            "certified-sync-older-anchor",
        );
        let requester_config = sample_node_config(
            1,
            reserve_local_address(),
            Vec::new(),
            "certified-sync-requester-anchor",
        );
        let mut responder = build_test_runner(1, responder_config, genesis.clone()).await;
        let mut requester = build_test_runner(1, requester_config, genesis.clone()).await;
        let shared_root = first_valid_block(&genesis);
        let crypto = DeterministicCryptoBackend::from_genesis(&genesis);
        let consensus = ConsensusEngine::new(genesis.clone());

        for runner in [&mut responder, &mut requester] {
            assert_eq!(
                runner
                    .accept_block(shared_root.clone(), true)
                    .await
                    .unwrap(),
                BlockAcceptance::Accepted
            );
            assert!(
                runner
                    .import_proposal_vote(signed_proposal_vote(
                        runner.crypto.as_ref(),
                        2,
                        &shared_root
                    ))
                    .unwrap()
            );
            assert!(
                runner
                    .import_proposal_vote(signed_proposal_vote(
                        runner.crypto.as_ref(),
                        3,
                        &shared_root
                    ))
                    .unwrap()
            );
        }

        let mut root_ledger = LedgerState::from_genesis(&genesis);
        root_ledger.apply_block(&shared_root, &crypto).unwrap();
        let build_child = |proposer_id: ValidatorId, min_slot: u64| {
            let slot = (min_slot..(min_slot + 50))
                .find(|slot| consensus.proposer_for_slot(*slot) == proposer_id)
                .unwrap();
            let epoch = consensus.epoch_for_slot(slot);
            let transactions = Vec::new();
            let commitment: Option<TopologyCommitment> = None;
            let header = BlockHeader {
                block_number: 2,
                parent_hash: shared_root.block_hash,
                slot,
                epoch,
                proposer_id,
                timestamp_unix_millis: now_unix_millis(),
                state_root: root_ledger.state_root(),
                transactions_root: canonical_hash(&transactions),
                topology_root: empty_hash(),
            };
            let block_hash = canonical_hash(&(header.clone(), &transactions, &commitment));
            Block {
                header,
                transactions,
                commitment,
                commitment_receipts: Vec::new(),
                signature: crypto.sign(proposer_id, &block_hash).unwrap(),
                block_hash,
            }
        };

        let requester_tip = build_child(2, shared_root.header.slot + 1);
        let responder_tip = build_child(3, requester_tip.header.slot + 1);

        assert_eq!(
            requester
                .accept_block(requester_tip.clone(), false)
                .await
                .unwrap(),
            BlockAcceptance::Orphan
        );
        assert!(
            requester
                .import_proposal_vote(signed_proposal_vote(
                    requester.crypto.as_ref(),
                    3,
                    &requester_tip
                ))
                .unwrap()
        );
        assert!(
            requester
                .import_proposal_vote(signed_proposal_vote(
                    requester.crypto.as_ref(),
                    4,
                    &requester_tip
                ))
                .unwrap()
        );

        assert_eq!(
            responder
                .accept_block(responder_tip.clone(), false)
                .await
                .unwrap(),
            BlockAcceptance::Orphan
        );
        assert!(
            responder
                .import_proposal_vote(signed_proposal_vote(
                    responder.crypto.as_ref(),
                    2,
                    &responder_tip
                ))
                .unwrap()
        );
        assert!(
            responder
                .import_proposal_vote(signed_proposal_vote(
                    responder.crypto.as_ref(),
                    4,
                    &responder_tip
                ))
                .unwrap()
        );

        let response = responder.build_certified_sync_response(ChunkedSyncRequest {
            requester_id: 2,
            known_qc_hash: Some(requester_tip.block_hash),
            known_qc_height: requester_tip.header.block_number,
            known_qc_anchors: vec![
                SyncQcAnchor {
                    block_hash: requester_tip.block_hash,
                    block_number: requester_tip.header.block_number,
                },
                SyncQcAnchor {
                    block_hash: shared_root.block_hash,
                    block_number: shared_root.header.block_number,
                },
            ],
            from_height: requester_tip.header.block_number,
            want_certified_only: true,
        });

        match response {
            ChunkedSyncResponse::Certified {
                shared_qc_hash,
                shared_qc_height,
                blocks,
                ..
            } => {
                assert_eq!(shared_qc_hash, shared_root.block_hash);
                assert_eq!(shared_qc_height, shared_root.header.block_number);
                assert_eq!(blocks.len(), 1);
                assert_eq!(blocks[0].block_hash, responder_tip.block_hash);
            }
            other => panic!("expected certified response, got {other:?}"),
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn startup_barrier_sync_blocks_can_replace_divergent_uncertified_suffix_from_certified_frontier()
     {
        let genesis = sample_genesis();
        let local_address = reserve_local_address();
        let peer_address = reserve_local_address();
        let peer = PeerConfig {
            validator_id: 2,
            address: peer_address,
        };
        let config = sample_node_config(
            1,
            local_address,
            vec![peer],
            "startup-barrier-sync-blocks-anchor-repair",
        );
        let mut runner = build_test_runner(1, config, genesis.clone()).await;
        let shared_root = first_valid_block(&genesis);
        let crypto = DeterministicCryptoBackend::from_genesis(&genesis);
        let consensus = ConsensusEngine::new(genesis.clone());

        assert_eq!(
            runner
                .accept_block(shared_root.clone(), true)
                .await
                .unwrap(),
            BlockAcceptance::Accepted
        );
        assert!(
            runner
                .import_proposal_vote(signed_proposal_vote(
                    runner.crypto.as_ref(),
                    2,
                    &shared_root
                ))
                .unwrap()
        );
        assert!(
            runner
                .import_proposal_vote(signed_proposal_vote(
                    runner.crypto.as_ref(),
                    3,
                    &shared_root
                ))
                .unwrap()
        );

        let mut root_ledger = LedgerState::from_genesis(&genesis);
        root_ledger.apply_block(&shared_root, &crypto).unwrap();
        let build_child = |parent: &Block,
                           parent_ledger: &LedgerState,
                           block_number: u64,
                           proposer_id: ValidatorId,
                           min_slot: u64| {
            let slot = (min_slot..(min_slot + 100))
                .find(|slot| consensus.proposer_for_slot(*slot) == proposer_id)
                .unwrap();
            let epoch = consensus.epoch_for_slot(slot);
            let transactions = Vec::new();
            let commitment: Option<TopologyCommitment> = None;
            let header = BlockHeader {
                block_number,
                parent_hash: parent.block_hash,
                slot,
                epoch,
                proposer_id,
                timestamp_unix_millis: now_unix_millis(),
                state_root: parent_ledger.state_root(),
                transactions_root: canonical_hash(&transactions),
                topology_root: empty_hash(),
            };
            let block_hash = canonical_hash(&(header.clone(), &transactions, &commitment));
            Block {
                header,
                transactions,
                commitment,
                commitment_receipts: Vec::new(),
                signature: crypto.sign(proposer_id, &block_hash).unwrap(),
                block_hash,
            }
        };

        let local_divergent_child = build_child(
            &shared_root,
            &root_ledger,
            2,
            2,
            shared_root.header.slot + 1,
        );
        let remote_child = build_child(
            &shared_root,
            &root_ledger,
            2,
            3,
            local_divergent_child.header.slot + 1,
        );

        runner.ledger = LedgerState::replay_blocks(
            &genesis,
            &[shared_root.clone(), local_divergent_child],
            &crypto,
        )
        .unwrap();
        runner.blocks = vec![shared_root.clone()];
        runner.blocks.push(build_child(
            &shared_root,
            &root_ledger,
            2,
            2,
            shared_root.header.slot + 1,
        ));
        runner.startup_sync_barrier = true;
        runner.rebuild_seen_sets();

        let target_snapshot = LedgerState::replay_blocks(
            &genesis,
            &[shared_root.clone(), remote_child.clone()],
            &crypto,
        )
        .unwrap()
        .snapshot()
        .clone();
        let chain = ChainSegment {
            base_height: shared_root.header.block_number,
            base_tip_hash: shared_root.block_hash,
            target_snapshot,
            blocks: vec![remote_child.clone()],
            receipts: Vec::new(),
            proposal_votes: Vec::new(),
        };

        runner
            .handle_network_event(NetworkEvent::Received {
                from_validator_id: 2,
                payload: ProtocolMessage::SyncBlocks {
                    responder_id: 2,
                    chain,
                },
                bytes: 0,
            })
            .await
            .unwrap();

        assert_eq!(runner.ledger.snapshot().tip_hash, remote_child.block_hash);
        assert_eq!(
            runner.snapshot_metrics().incremental_sync_applied,
            1,
            "anchored suffix repair should apply through the live SyncBlocks handler"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn certified_sync_response_with_ahead_tip_requests_legacy_followup() {
        let genesis = sample_genesis();
        let local_address = reserve_local_address();
        let peer_address = reserve_local_address();
        let peer = PeerConfig {
            validator_id: 2,
            address: peer_address.clone(),
        };
        let config = sample_node_config(
            1,
            local_address,
            vec![peer],
            "certified-sync-legacy-followup",
        );
        let mut runner = build_test_runner(1, config, genesis.clone()).await;
        let (_, _, _, mut peer_events) =
            spawn_test_network(2, peer_address, Vec::new(), &genesis).await;
        let shared_root = first_valid_block(&genesis);
        let crypto = DeterministicCryptoBackend::from_genesis(&genesis);
        let consensus = ConsensusEngine::new(genesis.clone());

        assert_eq!(
            runner
                .accept_block(shared_root.clone(), true)
                .await
                .unwrap(),
            BlockAcceptance::Accepted
        );
        assert!(
            runner
                .import_proposal_vote(signed_proposal_vote(
                    runner.crypto.as_ref(),
                    2,
                    &shared_root
                ))
                .unwrap()
        );
        assert!(
            runner
                .import_proposal_vote(signed_proposal_vote(
                    runner.crypto.as_ref(),
                    3,
                    &shared_root
                ))
                .unwrap()
        );

        let mut root_ledger = LedgerState::from_genesis(&genesis);
        root_ledger.apply_block(&shared_root, &crypto).unwrap();
        let slot = ((shared_root.header.slot + 1)..(shared_root.header.slot + 50))
            .find(|slot| consensus.proposer_for_slot(*slot) == 3)
            .unwrap();
        let epoch = consensus.epoch_for_slot(slot);
        let transactions = Vec::new();
        let commitment: Option<TopologyCommitment> = None;
        let header = BlockHeader {
            block_number: 2,
            parent_hash: shared_root.block_hash,
            slot,
            epoch,
            proposer_id: 3,
            timestamp_unix_millis: now_unix_millis(),
            state_root: root_ledger.state_root(),
            transactions_root: canonical_hash(&transactions),
            topology_root: empty_hash(),
        };
        let block_hash = canonical_hash(&(header.clone(), &transactions, &commitment));
        let certified_tip = Block {
            header,
            transactions,
            commitment,
            commitment_receipts: Vec::new(),
            signature: crypto.sign(3, &block_hash).unwrap(),
            block_hash,
        };
        assert_eq!(
            runner
                .accept_block(certified_tip.clone(), false)
                .await
                .unwrap(),
            BlockAcceptance::Orphan
        );
        assert!(
            runner
                .import_proposal_vote(signed_proposal_vote(
                    runner.crypto.as_ref(),
                    2,
                    &certified_tip
                ))
                .unwrap()
        );
        assert!(
            runner
                .import_proposal_vote(signed_proposal_vote(
                    runner.crypto.as_ref(),
                    4,
                    &certified_tip
                ))
                .unwrap()
        );
        let certified_qc = runner
            .quorum_certificates
            .get(&certified_tip.block_hash)
            .unwrap()
            .clone();

        runner
            .handle_network_event(NetworkEvent::Received {
                from_validator_id: 2,
                payload: ProtocolMessage::CertifiedSyncResponse(ChunkedSyncResponse::Certified {
                    responder_id: 2,
                    responder_height: 3,
                    responder_tip_hash: canonical_hash(&"peer-ahead-uncertified-tip"),
                    shared_qc_hash: shared_root.block_hash,
                    shared_qc_height: shared_root.header.block_number,
                    headers: vec![CertifiedBlockHeader {
                        header: certified_tip.header.clone(),
                        block_hash: certified_tip.block_hash,
                        quorum_certificate: Some(certified_qc.clone()),
                        prior_service_aggregate: None,
                    }],
                    blocks: vec![certified_tip.clone()],
                    qcs: vec![certified_qc],
                    service_aggregates: Vec::new(),
                }),
                bytes: 0,
            })
            .await
            .unwrap();

        loop {
            match recv_protocol_message(&mut peer_events).await {
                ProtocolMessage::SyncRequest {
                    requester_id,
                    known_height,
                    known_tip_hash,
                } => {
                    assert_eq!(requester_id, 1);
                    assert_eq!(known_height, certified_tip.header.block_number);
                    assert_eq!(known_tip_hash, certified_tip.block_hash);
                    break;
                }
                ProtocolMessage::ProposalVote(_)
                | ProtocolMessage::QuorumCertificate(_)
                | ProtocolMessage::SyncStatus { .. }
                | ProtocolMessage::SyncBlocks { .. }
                | ProtocolMessage::SyncResponse { .. }
                | ProtocolMessage::HeartbeatPulse(_)
                | ProtocolMessage::ReceiptFetch { .. } => {}
                other => panic!("expected legacy sync follow-up request, got {other:?}"),
            }
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn skipped_certified_sync_response_with_ahead_peer_requests_legacy_followup() {
        let genesis = sample_genesis();
        let local_address = reserve_local_address();
        let peer_address = reserve_local_address();
        let peer = PeerConfig {
            validator_id: 2,
            address: peer_address.clone(),
        };
        let config = sample_node_config(
            1,
            local_address,
            vec![peer],
            "certified-sync-skipped-legacy-followup",
        );
        let mut runner = build_test_runner(1, config, genesis.clone()).await;
        let (_, _, _, mut peer_events) =
            spawn_test_network(2, peer_address, Vec::new(), &genesis).await;
        let shared_root = first_valid_block(&genesis);

        assert_eq!(
            runner
                .accept_block(shared_root.clone(), true)
                .await
                .unwrap(),
            BlockAcceptance::Accepted
        );
        assert!(
            runner
                .import_proposal_vote(signed_proposal_vote(
                    runner.crypto.as_ref(),
                    2,
                    &shared_root
                ))
                .unwrap()
        );
        assert!(
            runner
                .import_proposal_vote(signed_proposal_vote(
                    runner.crypto.as_ref(),
                    3,
                    &shared_root
                ))
                .unwrap()
        );

        runner.record_peer_sync_status(
            2,
            2,
            canonical_hash(&"peer-ahead-uncertified-tip"),
            Some(shared_root.block_hash),
            shared_root.header.block_number,
            vec![SyncQcAnchor {
                block_hash: shared_root.block_hash,
                block_number: shared_root.header.block_number,
            }],
        );

        runner
            .handle_network_event(NetworkEvent::Received {
                from_validator_id: 2,
                payload: ProtocolMessage::CertifiedSyncResponse(ChunkedSyncResponse::Certified {
                    responder_id: 2,
                    responder_height: shared_root.header.block_number,
                    responder_tip_hash: shared_root.block_hash,
                    shared_qc_hash: shared_root.block_hash,
                    shared_qc_height: shared_root.header.block_number,
                    headers: Vec::new(),
                    blocks: vec![shared_root.clone()],
                    qcs: vec![
                        runner
                            .quorum_certificates
                            .get(&shared_root.block_hash)
                            .unwrap()
                            .clone(),
                    ],
                    service_aggregates: Vec::new(),
                }),
                bytes: 0,
            })
            .await
            .unwrap();

        loop {
            match recv_protocol_message(&mut peer_events).await {
                ProtocolMessage::SyncRequest {
                    requester_id,
                    known_height,
                    known_tip_hash,
                } => {
                    assert_eq!(requester_id, 1);
                    assert_eq!(known_height, shared_root.header.block_number);
                    assert_eq!(known_tip_hash, shared_root.block_hash);
                    break;
                }
                ProtocolMessage::ProposalVote(_)
                | ProtocolMessage::QuorumCertificate(_)
                | ProtocolMessage::SyncStatus { .. }
                | ProtocolMessage::SyncBlocks { .. }
                | ProtocolMessage::SyncResponse { .. }
                | ProtocolMessage::HeartbeatPulse(_)
                | ProtocolMessage::ReceiptFetch { .. } => {}
                other => panic!(
                    "expected legacy sync request after skipped certified sync, got {other:?}"
                ),
            }
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn incremental_sync_apply_with_ahead_peer_requests_followup() {
        let genesis = sample_genesis();
        let local_address = reserve_local_address();
        let peer_address = reserve_local_address();
        let peer = PeerConfig {
            validator_id: 2,
            address: peer_address.clone(),
        };
        let config = sample_node_config(1, local_address, vec![peer], "sync-blocks-followup");
        let mut runner = build_test_runner(1, config, genesis.clone()).await;
        let (_, _, _, mut peer_events) =
            spawn_test_network(2, peer_address, Vec::new(), &genesis).await;
        let shared_root = first_valid_block(&genesis);

        assert_eq!(
            runner
                .accept_block(shared_root.clone(), true)
                .await
                .unwrap(),
            BlockAcceptance::Accepted
        );
        assert!(
            runner
                .import_proposal_vote(signed_proposal_vote(
                    runner.crypto.as_ref(),
                    2,
                    &shared_root
                ))
                .unwrap()
        );
        assert!(
            runner
                .import_proposal_vote(signed_proposal_vote(
                    runner.crypto.as_ref(),
                    3,
                    &shared_root
                ))
                .unwrap()
        );

        let crypto = DeterministicCryptoBackend::from_genesis(&genesis);
        let consensus = ConsensusEngine::new(genesis.clone());
        let mut root_ledger = LedgerState::from_genesis(&genesis);
        root_ledger.apply_block(&shared_root, &crypto).unwrap();
        let slot = ((shared_root.header.slot + 1)..(shared_root.header.slot + 50))
            .find(|slot| consensus.proposer_for_slot(*slot) == 2)
            .unwrap();
        let epoch = consensus.epoch_for_slot(slot);
        let transactions = Vec::new();
        let commitment: Option<TopologyCommitment> = None;
        let header = BlockHeader {
            block_number: 2,
            parent_hash: shared_root.block_hash,
            slot,
            epoch,
            proposer_id: 2,
            timestamp_unix_millis: now_unix_millis(),
            state_root: root_ledger.state_root(),
            transactions_root: canonical_hash(&transactions),
            topology_root: empty_hash(),
        };
        let block_hash = canonical_hash(&(header.clone(), &transactions, &commitment));
        let child = Block {
            header,
            transactions,
            commitment,
            commitment_receipts: Vec::new(),
            signature: crypto.sign(2, &block_hash).unwrap(),
            block_hash,
        };
        let target_snapshot =
            LedgerState::replay_blocks(&genesis, &[shared_root.clone(), child.clone()], &crypto)
                .unwrap()
                .snapshot()
                .clone();
        runner.record_peer_sync_status(
            2,
            3,
            canonical_hash(&"peer-ahead-tip"),
            Some(shared_root.block_hash),
            shared_root.header.block_number,
            vec![SyncQcAnchor {
                block_hash: shared_root.block_hash,
                block_number: shared_root.header.block_number,
            }],
        );

        runner
            .handle_network_event(NetworkEvent::Received {
                from_validator_id: 2,
                payload: ProtocolMessage::SyncBlocks {
                    responder_id: 2,
                    chain: ChainSegment {
                        base_height: shared_root.header.block_number,
                        base_tip_hash: shared_root.block_hash,
                        target_snapshot,
                        blocks: vec![child.clone()],
                        receipts: Vec::new(),
                        proposal_votes: Vec::new(),
                    },
                },
                bytes: 0,
            })
            .await
            .unwrap();

        loop {
            match recv_protocol_message(&mut peer_events).await {
                ProtocolMessage::SyncRequest {
                    requester_id,
                    known_height,
                    known_tip_hash: _,
                } => {
                    assert_eq!(requester_id, 1);
                    assert_eq!(known_height, shared_root.header.block_number);
                    break;
                }
                ProtocolMessage::ProposalVote(_)
                | ProtocolMessage::QuorumCertificate(_)
                | ProtocolMessage::SyncStatus { .. }
                | ProtocolMessage::SyncBlocks { .. }
                | ProtocolMessage::SyncResponse { .. }
                | ProtocolMessage::HeartbeatPulse(_)
                | ProtocolMessage::ReceiptFetch { .. } => {}
                other => panic!("expected follow-up sync request, got {other:?}"),
            }
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn incremental_sync_apply_pushes_tip_progress_to_other_stale_peer() {
        let genesis = sample_genesis();
        let local_address = reserve_local_address();
        let responder_address = reserve_local_address();
        let stale_peer_address = reserve_local_address();
        let config = sample_node_config(
            1,
            local_address,
            vec![
                PeerConfig {
                    validator_id: 2,
                    address: responder_address.clone(),
                },
                PeerConfig {
                    validator_id: 3,
                    address: stale_peer_address.clone(),
                },
            ],
            "sync-blocks-push-stale-peer",
        );
        let mut runner = build_test_runner(1, config, genesis.clone()).await;
        let (_, _, _, _responder_events) =
            spawn_test_network(2, responder_address, Vec::new(), &genesis).await;
        let (_, _, _, mut stale_peer_events) =
            spawn_test_network(3, stale_peer_address, Vec::new(), &genesis).await;
        let shared_root = first_valid_block(&genesis);

        assert_eq!(
            runner
                .accept_block(shared_root.clone(), true)
                .await
                .unwrap(),
            BlockAcceptance::Accepted
        );
        assert!(
            runner
                .import_proposal_vote(signed_proposal_vote(
                    runner.crypto.as_ref(),
                    2,
                    &shared_root
                ))
                .unwrap()
        );
        assert!(
            runner
                .import_proposal_vote(signed_proposal_vote(
                    runner.crypto.as_ref(),
                    3,
                    &shared_root
                ))
                .unwrap()
        );

        let crypto = DeterministicCryptoBackend::from_genesis(&genesis);
        let consensus = ConsensusEngine::new(genesis.clone());
        let mut root_ledger = LedgerState::from_genesis(&genesis);
        root_ledger.apply_block(&shared_root, &crypto).unwrap();
        let slot = ((shared_root.header.slot + 1)..(shared_root.header.slot + 50))
            .find(|slot| consensus.proposer_for_slot(*slot) == 2)
            .unwrap();
        let epoch = consensus.epoch_for_slot(slot);
        let transactions = Vec::new();
        let commitment: Option<TopologyCommitment> = None;
        let header = BlockHeader {
            block_number: 2,
            parent_hash: shared_root.block_hash,
            slot,
            epoch,
            proposer_id: 2,
            timestamp_unix_millis: now_unix_millis(),
            state_root: root_ledger.state_root(),
            transactions_root: canonical_hash(&transactions),
            topology_root: empty_hash(),
        };
        let block_hash = canonical_hash(&(header.clone(), &transactions, &commitment));
        let child = Block {
            header,
            transactions,
            commitment,
            commitment_receipts: Vec::new(),
            signature: crypto.sign(2, &block_hash).unwrap(),
            block_hash,
        };
        let target_snapshot =
            LedgerState::replay_blocks(&genesis, &[shared_root.clone(), child.clone()], &crypto)
                .unwrap()
                .snapshot()
                .clone();
        let shared_anchor = SyncQcAnchor {
            block_hash: shared_root.block_hash,
            block_number: shared_root.header.block_number,
        };
        runner.record_peer_sync_status(
            2,
            3,
            canonical_hash(&"peer-ahead-tip"),
            Some(shared_root.block_hash),
            shared_root.header.block_number,
            vec![shared_anchor.clone()],
        );
        runner.record_peer_sync_status(
            3,
            shared_root.header.block_number,
            shared_root.block_hash,
            Some(shared_root.block_hash),
            shared_root.header.block_number,
            vec![shared_anchor],
        );

        runner
            .handle_network_event(NetworkEvent::Received {
                from_validator_id: 2,
                payload: ProtocolMessage::SyncBlocks {
                    responder_id: 2,
                    chain: ChainSegment {
                        base_height: shared_root.header.block_number,
                        base_tip_hash: shared_root.block_hash,
                        target_snapshot,
                        blocks: vec![child.clone()],
                        receipts: Vec::new(),
                        proposal_votes: Vec::new(),
                    },
                },
                bytes: 0,
            })
            .await
            .unwrap();

        let mut saw_status = false;
        let mut saw_push = false;
        let deadline = tokio::time::Instant::now() + Duration::from_millis(250);
        while !(saw_status && saw_push) {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            assert!(
                !remaining.is_zero(),
                "expected repaired node to notify stale peer after incremental sync apply"
            );
            match timeout(remaining, recv_protocol_message(&mut stale_peer_events))
                .await
                .unwrap()
            {
                ProtocolMessage::SyncStatus { validator_id, .. } => {
                    assert_eq!(validator_id, 1);
                    saw_status = true;
                }
                ProtocolMessage::SyncBlocks { .. }
                | ProtocolMessage::SyncResponse { .. }
                | ProtocolMessage::CertifiedSyncResponse(_) => {
                    saw_push = true;
                }
                ProtocolMessage::ProposalVote(_)
                | ProtocolMessage::QuorumCertificate(_)
                | ProtocolMessage::HeartbeatPulse(_)
                | ProtocolMessage::ReceiptFetch { .. } => {}
                other => panic!("unexpected message after incremental sync apply: {other:?}"),
            }
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn full_sync_apply_with_ahead_peer_requests_followup() {
        let genesis = sample_genesis();
        let local_address = reserve_local_address();
        let peer_address = reserve_local_address();
        let peer = PeerConfig {
            validator_id: 2,
            address: peer_address.clone(),
        };
        let config = sample_node_config(1, local_address, vec![peer], "sync-snapshot-followup");
        let mut runner = build_test_runner(1, config, genesis.clone()).await;
        let (_, _, _, mut peer_events) =
            spawn_test_network(2, peer_address, Vec::new(), &genesis).await;
        let shared_root = first_valid_block(&genesis);

        runner.record_peer_sync_status(
            2,
            3,
            canonical_hash(&"peer-ahead-tip"),
            Some(shared_root.block_hash),
            shared_root.header.block_number,
            vec![SyncQcAnchor {
                block_hash: shared_root.block_hash,
                block_number: shared_root.header.block_number,
            }],
        );

        runner
            .handle_network_event(NetworkEvent::Received {
                from_validator_id: 2,
                payload: ProtocolMessage::SyncResponse {
                    responder_id: 2,
                    chain: ChainSnapshot {
                        snapshot: LedgerState::replay_blocks(
                            &genesis,
                            std::slice::from_ref(&shared_root),
                            &DeterministicCryptoBackend::from_genesis(&genesis),
                        )
                        .unwrap()
                        .snapshot()
                        .clone(),
                        blocks: vec![shared_root.clone()],
                        receipts: Vec::new(),
                    },
                },
                bytes: 0,
            })
            .await
            .unwrap();

        loop {
            match recv_protocol_message(&mut peer_events).await {
                ProtocolMessage::SyncRequest {
                    requester_id,
                    known_height,
                    known_tip_hash: _,
                } => {
                    assert_eq!(requester_id, 1);
                    assert_eq!(known_height, shared_root.header.block_number);
                    break;
                }
                ProtocolMessage::CertifiedSyncRequest(ChunkedSyncRequest {
                    requester_id,
                    from_height,
                    ..
                }) => {
                    assert_eq!(requester_id, 1);
                    assert_eq!(from_height, shared_root.header.block_number);
                    break;
                }
                ProtocolMessage::ProposalVote(_)
                | ProtocolMessage::QuorumCertificate(_)
                | ProtocolMessage::SyncStatus { .. }
                | ProtocolMessage::SyncBlocks { .. }
                | ProtocolMessage::SyncResponse { .. }
                | ProtocolMessage::HeartbeatPulse(_)
                | ProtocolMessage::ReceiptFetch { .. } => {}
                other => panic!("expected follow-up sync request, got {other:?}"),
            }
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn full_sync_apply_pushes_tip_progress_to_other_stale_peer() {
        let genesis = sample_genesis();
        let local_address = reserve_local_address();
        let responder_address = reserve_local_address();
        let stale_peer_address = reserve_local_address();
        let config = sample_node_config(
            1,
            local_address,
            vec![
                PeerConfig {
                    validator_id: 2,
                    address: responder_address.clone(),
                },
                PeerConfig {
                    validator_id: 3,
                    address: stale_peer_address.clone(),
                },
            ],
            "sync-snapshot-push-stale-peer",
        );
        let mut runner = build_test_runner(1, config, genesis.clone()).await;
        let (_, _, _, _responder_events) =
            spawn_test_network(2, responder_address, Vec::new(), &genesis).await;
        let (_, _, _, mut stale_peer_events) =
            spawn_test_network(3, stale_peer_address, Vec::new(), &genesis).await;

        let shared_root = first_valid_block(&genesis);
        let crypto = DeterministicCryptoBackend::from_genesis(&genesis);
        let consensus = ConsensusEngine::new(genesis.clone());
        let mut root_ledger = LedgerState::from_genesis(&genesis);
        root_ledger.apply_block(&shared_root, &crypto).unwrap();
        let slot = ((shared_root.header.slot + 1)..(shared_root.header.slot + 50))
            .find(|slot| consensus.proposer_for_slot(*slot) == 2)
            .unwrap();
        let epoch = consensus.epoch_for_slot(slot);
        let transactions = Vec::new();
        let commitment: Option<TopologyCommitment> = None;
        let header = BlockHeader {
            block_number: 2,
            parent_hash: shared_root.block_hash,
            slot,
            epoch,
            proposer_id: 2,
            timestamp_unix_millis: now_unix_millis(),
            state_root: root_ledger.state_root(),
            transactions_root: canonical_hash(&transactions),
            topology_root: empty_hash(),
        };
        let block_hash = canonical_hash(&(header.clone(), &transactions, &commitment));
        let second_block = Block {
            header,
            transactions,
            commitment,
            commitment_receipts: Vec::new(),
            signature: crypto.sign(2, &block_hash).unwrap(),
            block_hash,
        };
        let mut snapshot_ledger = LedgerState::from_genesis(&genesis);
        snapshot_ledger
            .apply_block(&shared_root, runner.crypto.as_ref())
            .unwrap();
        snapshot_ledger
            .apply_block(&second_block, runner.crypto.as_ref())
            .unwrap();
        let snapshot = snapshot_ledger.snapshot().clone();

        runner.record_peer_sync_status(2, 3, canonical_hash(&"ahead-tip"), None, 0, Vec::new());
        runner.record_peer_sync_status(3, 0, empty_hash(), None, 0, Vec::new());

        runner
            .handle_network_event(NetworkEvent::Received {
                from_validator_id: 2,
                payload: ProtocolMessage::SyncResponse {
                    responder_id: 2,
                    chain: ChainSnapshot {
                        snapshot,
                        blocks: vec![shared_root.clone(), second_block.clone()],
                        receipts: Vec::new(),
                    },
                },
                bytes: 0,
            })
            .await
            .unwrap();

        let mut saw_status = false;
        let mut saw_push = false;
        let deadline = tokio::time::Instant::now() + Duration::from_millis(250);
        while !(saw_status && saw_push) {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            assert!(
                !remaining.is_zero(),
                "expected repaired node to notify stale peer after full sync apply"
            );
            match timeout(remaining, recv_protocol_message(&mut stale_peer_events))
                .await
                .unwrap()
            {
                ProtocolMessage::SyncStatus { validator_id, .. } => {
                    assert_eq!(validator_id, 1);
                    saw_status = true;
                }
                ProtocolMessage::SyncBlocks { .. }
                | ProtocolMessage::SyncResponse { .. }
                | ProtocolMessage::CertifiedSyncResponse(_) => {
                    saw_push = true;
                }
                ProtocolMessage::ProposalVote(_)
                | ProtocolMessage::QuorumCertificate(_)
                | ProtocolMessage::HeartbeatPulse(_)
                | ProtocolMessage::ReceiptFetch { .. } => {}
                other => panic!("unexpected message after full sync apply: {other:?}"),
            }
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn skipped_certified_sync_with_pending_child_requests_legacy_followup() {
        let genesis = sample_genesis();
        let local_address = reserve_local_address();
        let peer_address = reserve_local_address();
        let peer = PeerConfig {
            validator_id: 2,
            address: peer_address.clone(),
        };
        let config = sample_node_config(
            1,
            local_address,
            vec![peer],
            "certified-sync-skipped-pending-child-followup",
        );
        let mut runner = build_test_runner(1, config, genesis.clone()).await;
        let (_, _, _, mut peer_events) =
            spawn_test_network(2, peer_address, Vec::new(), &genesis).await;
        let shared_root = first_valid_block(&genesis);
        let crypto = DeterministicCryptoBackend::from_genesis(&genesis);
        let consensus = ConsensusEngine::new(genesis.clone());

        assert_eq!(
            runner
                .accept_block(shared_root.clone(), true)
                .await
                .unwrap(),
            BlockAcceptance::Accepted
        );
        assert!(
            runner
                .import_proposal_vote(signed_proposal_vote(
                    runner.crypto.as_ref(),
                    2,
                    &shared_root
                ))
                .unwrap()
        );
        assert!(
            runner
                .import_proposal_vote(signed_proposal_vote(
                    runner.crypto.as_ref(),
                    3,
                    &shared_root
                ))
                .unwrap()
        );

        let mut root_ledger = LedgerState::from_genesis(&genesis);
        root_ledger.apply_block(&shared_root, &crypto).unwrap();
        let slot = ((shared_root.header.slot + 1)..(shared_root.header.slot + 50))
            .find(|slot| consensus.proposer_for_slot(*slot) == 2)
            .unwrap();
        let epoch = consensus.epoch_for_slot(slot);
        let transactions = Vec::new();
        let commitment: Option<TopologyCommitment> = None;
        let header = BlockHeader {
            block_number: 2,
            parent_hash: shared_root.block_hash,
            slot,
            epoch,
            proposer_id: 2,
            timestamp_unix_millis: now_unix_millis(),
            state_root: root_ledger.state_root(),
            transactions_root: canonical_hash(&transactions),
            topology_root: empty_hash(),
        };
        let block_hash = canonical_hash(&(header.clone(), &transactions, &commitment));
        let pending_child = Block {
            header,
            transactions,
            commitment,
            commitment_receipts: Vec::new(),
            signature: crypto.sign(2, &block_hash).unwrap(),
            block_hash,
        };
        assert_eq!(
            runner.accept_block(pending_child, false).await.unwrap(),
            BlockAcceptance::Orphan
        );
        assert!(!runner.pending_certified_children.is_empty());

        runner.record_peer_sync_status(
            2,
            shared_root.header.block_number,
            shared_root.block_hash,
            Some(shared_root.block_hash),
            shared_root.header.block_number,
            vec![SyncQcAnchor {
                block_hash: shared_root.block_hash,
                block_number: shared_root.header.block_number,
            }],
        );

        while timeout(Duration::from_millis(25), peer_events.recv())
            .await
            .ok()
            .flatten()
            .is_some()
        {}

        runner
            .handle_network_event(NetworkEvent::Received {
                from_validator_id: 2,
                payload: ProtocolMessage::CertifiedSyncResponse(ChunkedSyncResponse::Certified {
                    responder_id: 2,
                    responder_height: shared_root.header.block_number,
                    responder_tip_hash: shared_root.block_hash,
                    shared_qc_hash: shared_root.block_hash,
                    shared_qc_height: shared_root.header.block_number,
                    headers: vec![CertifiedBlockHeader {
                        header: shared_root.header.clone(),
                        block_hash: shared_root.block_hash,
                        quorum_certificate: runner
                            .quorum_certificates
                            .get(&shared_root.block_hash)
                            .cloned(),
                        prior_service_aggregate: None,
                    }],
                    blocks: vec![shared_root.clone()],
                    qcs: vec![
                        runner
                            .quorum_certificates
                            .get(&shared_root.block_hash)
                            .unwrap()
                            .clone(),
                    ],
                    service_aggregates: Vec::new(),
                }),
                bytes: 0,
            })
            .await
            .unwrap();

        loop {
            match recv_protocol_message(&mut peer_events).await {
                ProtocolMessage::SyncRequest {
                    requester_id,
                    known_height,
                    known_tip_hash,
                } => {
                    assert_eq!(requester_id, 1);
                    assert_eq!(known_height, shared_root.header.block_number);
                    assert_eq!(known_tip_hash, shared_root.block_hash);
                    break;
                }
                ProtocolMessage::ProposalVote(_)
                | ProtocolMessage::QuorumCertificate(_)
                | ProtocolMessage::SyncStatus { .. }
                | ProtocolMessage::SyncBlocks { .. }
                | ProtocolMessage::SyncResponse { .. }
                | ProtocolMessage::HeartbeatPulse(_)
                | ProtocolMessage::ReceiptFetch { .. } => {}
                other => panic!(
                    "expected legacy sync request after skipped certified sync with pending child, got {other:?}"
                ),
            }
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn stale_certified_sync_response_does_not_replace_newer_local_certified_tip() {
        let genesis = sample_genesis();
        let responder_config = sample_node_config(
            1,
            reserve_local_address(),
            Vec::new(),
            "certified-sync-stale-responder",
        );
        let requester_config = sample_node_config(
            1,
            reserve_local_address(),
            Vec::new(),
            "certified-sync-stale-requester",
        );
        let mut responder = build_test_runner(1, responder_config, genesis.clone()).await;
        let mut requester = build_test_runner(1, requester_config, genesis.clone()).await;
        let shared_root = first_valid_block(&genesis);
        let crypto = DeterministicCryptoBackend::from_genesis(&genesis);
        let consensus = ConsensusEngine::new(genesis.clone());

        for runner in [&mut responder, &mut requester] {
            assert_eq!(
                runner
                    .accept_block(shared_root.clone(), true)
                    .await
                    .unwrap(),
                BlockAcceptance::Accepted
            );
            assert!(
                runner
                    .import_proposal_vote(signed_proposal_vote(
                        runner.crypto.as_ref(),
                        2,
                        &shared_root
                    ))
                    .unwrap()
            );
            assert!(
                runner
                    .import_proposal_vote(signed_proposal_vote(
                        runner.crypto.as_ref(),
                        3,
                        &shared_root
                    ))
                    .unwrap()
            );
        }

        let mut root_ledger = LedgerState::from_genesis(&genesis);
        root_ledger.apply_block(&shared_root, &crypto).unwrap();
        let build_child = |parent: &Block,
                           parent_ledger: &LedgerState,
                           proposer_id: ValidatorId,
                           min_slot: u64| {
            let slot = (min_slot..(min_slot + 50))
                .find(|slot| consensus.proposer_for_slot(*slot) == proposer_id)
                .unwrap();
            let epoch = consensus.epoch_for_slot(slot);
            let transactions = Vec::new();
            let commitment: Option<TopologyCommitment> = None;
            let header = BlockHeader {
                block_number: parent.header.block_number + 1,
                parent_hash: parent.block_hash,
                slot,
                epoch,
                proposer_id,
                timestamp_unix_millis: now_unix_millis(),
                state_root: parent_ledger.state_root(),
                transactions_root: canonical_hash(&transactions),
                topology_root: empty_hash(),
            };
            let block_hash = canonical_hash(&(header.clone(), &transactions, &commitment));
            Block {
                header,
                transactions,
                commitment,
                commitment_receipts: Vec::new(),
                signature: crypto.sign(proposer_id, &block_hash).unwrap(),
                block_hash,
            }
        };

        let certified_child =
            build_child(&shared_root, &root_ledger, 4, shared_root.header.slot + 1);
        for runner in [&mut responder, &mut requester] {
            assert_eq!(
                runner
                    .accept_block(certified_child.clone(), false)
                    .await
                    .unwrap(),
                BlockAcceptance::Orphan
            );
            assert!(
                runner
                    .import_proposal_vote(signed_proposal_vote(
                        runner.crypto.as_ref(),
                        2,
                        &certified_child
                    ))
                    .unwrap()
            );
            assert!(
                runner
                    .import_proposal_vote(signed_proposal_vote(
                        runner.crypto.as_ref(),
                        3,
                        &certified_child
                    ))
                    .unwrap()
            );
            assert_eq!(
                runner.ledger.snapshot().tip_hash,
                certified_child.block_hash
            );
        }

        let stale_response = responder.build_certified_sync_response_for_peer(&[SyncQcAnchor {
            block_hash: shared_root.block_hash,
            block_number: shared_root.header.block_number,
        }]);
        assert!(matches!(
            stale_response,
            ChunkedSyncResponse::Certified { .. }
        ));

        let mut child_ledger = root_ledger.clone();
        child_ledger.apply_block(&certified_child, &crypto).unwrap();
        let newer_certified_tip = build_child(
            &certified_child,
            &child_ledger,
            2,
            certified_child.header.slot + 1,
        );
        assert_eq!(
            requester
                .accept_block(newer_certified_tip.clone(), false)
                .await
                .unwrap(),
            BlockAcceptance::Orphan
        );
        assert!(
            requester
                .import_proposal_vote(signed_proposal_vote(
                    requester.crypto.as_ref(),
                    3,
                    &newer_certified_tip
                ))
                .unwrap()
        );
        assert!(
            requester
                .import_proposal_vote(signed_proposal_vote(
                    requester.crypto.as_ref(),
                    4,
                    &newer_certified_tip
                ))
                .unwrap()
        );
        assert_eq!(
            requester.ledger.snapshot().tip_hash,
            newer_certified_tip.block_hash
        );
        assert_eq!(requester.ledger.snapshot().height, 3);

        assert!(
            !requester
                .import_certified_sync_response(stale_response)
                .unwrap()
        );
        assert_eq!(
            requester.ledger.snapshot().tip_hash,
            newer_certified_tip.block_hash
        );
        assert_eq!(requester.ledger.snapshot().height, 3);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn sync_status_with_shared_qc_prefers_certified_sync_push() {
        let genesis = sample_genesis();
        let local_address = reserve_local_address();
        let peer_address = reserve_local_address();
        let peer = PeerConfig {
            validator_id: 2,
            address: peer_address.clone(),
        };
        let config = sample_node_config(
            1,
            local_address,
            vec![peer.clone()],
            "sync-status-certified",
        );
        let mut runner = build_test_runner(1, config, genesis.clone()).await;
        let (_, _, _, mut peer_events) =
            spawn_test_network(2, peer_address, Vec::new(), &genesis).await;
        let shared_root = first_valid_block(&genesis);

        assert_eq!(
            runner
                .accept_block(shared_root.clone(), true)
                .await
                .unwrap(),
            BlockAcceptance::Accepted
        );
        assert!(
            runner
                .import_proposal_vote(signed_proposal_vote(
                    runner.crypto.as_ref(),
                    2,
                    &shared_root
                ))
                .unwrap()
        );
        assert!(
            runner
                .import_proposal_vote(signed_proposal_vote(
                    runner.crypto.as_ref(),
                    3,
                    &shared_root
                ))
                .unwrap()
        );

        let crypto = DeterministicCryptoBackend::from_genesis(&genesis);
        let consensus = ConsensusEngine::new(genesis.clone());
        let mut root_ledger = LedgerState::from_genesis(&genesis);
        root_ledger.apply_block(&shared_root, &crypto).unwrap();
        let build_child = |proposer_id: ValidatorId, min_slot: u64| {
            let slot = (min_slot..(min_slot + 50))
                .find(|slot| consensus.proposer_for_slot(*slot) == proposer_id)
                .unwrap();
            let epoch = consensus.epoch_for_slot(slot);
            let transactions = Vec::new();
            let commitment: Option<TopologyCommitment> = None;
            let header = BlockHeader {
                block_number: 2,
                parent_hash: shared_root.block_hash,
                slot,
                epoch,
                proposer_id,
                timestamp_unix_millis: now_unix_millis(),
                state_root: root_ledger.state_root(),
                transactions_root: canonical_hash(&transactions),
                topology_root: empty_hash(),
            };
            let block_hash = canonical_hash(&(header.clone(), &transactions, &commitment));
            Block {
                header,
                transactions,
                commitment,
                commitment_receipts: Vec::new(),
                signature: crypto.sign(proposer_id, &block_hash).unwrap(),
                block_hash,
            }
        };
        let certified_tip = build_child(3, shared_root.header.slot + 1);

        assert_eq!(
            runner
                .accept_block(certified_tip.clone(), false)
                .await
                .unwrap(),
            BlockAcceptance::Orphan
        );
        assert!(
            runner
                .import_proposal_vote(signed_proposal_vote(
                    runner.crypto.as_ref(),
                    2,
                    &certified_tip
                ))
                .unwrap()
        );
        assert!(
            runner
                .import_proposal_vote(signed_proposal_vote(
                    runner.crypto.as_ref(),
                    4,
                    &certified_tip
                ))
                .unwrap()
        );

        runner
            .handle_network_event(NetworkEvent::Received {
                from_validator_id: 2,
                payload: ProtocolMessage::SyncStatus {
                    validator_id: 2,
                    height: 1,
                    tip_hash: shared_root.block_hash,
                    highest_qc_hash: Some(shared_root.block_hash),
                    highest_qc_height: 1,
                    recent_qc_anchors: vec![SyncQcAnchor {
                        block_hash: shared_root.block_hash,
                        block_number: 1,
                    }],
                },
                bytes: 0,
            })
            .await
            .unwrap();

        loop {
            match recv_protocol_message(&mut peer_events).await {
                ProtocolMessage::CertifiedSyncResponse(ChunkedSyncResponse::Certified {
                    shared_qc_hash,
                    shared_qc_height,
                    blocks,
                    qcs,
                    ..
                }) => {
                    assert_eq!(shared_qc_hash, shared_root.block_hash);
                    assert_eq!(shared_qc_height, 1);
                    assert_eq!(blocks.len(), 1);
                    assert_eq!(qcs.len(), 1);
                    assert_eq!(blocks[0].block_hash, certified_tip.block_hash);
                    break;
                }
                ProtocolMessage::ProposalVote(_)
                | ProtocolMessage::QuorumCertificate(_)
                | ProtocolMessage::SyncStatus { .. }
                | ProtocolMessage::SyncResponse { .. }
                | ProtocolMessage::HeartbeatPulse(_)
                | ProtocolMessage::ReceiptFetch { .. } => {}
                other => panic!("expected certified sync response, got {other:?}"),
            }
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn repeated_proactive_sync_push_is_suppressed_when_peer_state_is_unchanged() {
        let genesis = sample_genesis();
        let local_address = reserve_local_address();
        let peer_address = reserve_local_address();
        let peer = PeerConfig {
            validator_id: 2,
            address: peer_address.clone(),
        };
        let config = sample_node_config(1, local_address, vec![peer], "sync-push-dedup");
        let mut runner = build_test_runner(1, config, genesis.clone()).await;
        let (_, _, _, mut peer_events) =
            spawn_test_network(2, peer_address, Vec::new(), &genesis).await;
        let shared_root = first_valid_block(&genesis);
        let crypto = DeterministicCryptoBackend::from_genesis(&genesis);
        let consensus = ConsensusEngine::new(genesis.clone());

        assert_eq!(
            runner
                .accept_block(shared_root.clone(), true)
                .await
                .unwrap(),
            BlockAcceptance::Accepted
        );
        assert!(
            runner
                .import_proposal_vote(signed_proposal_vote(
                    runner.crypto.as_ref(),
                    2,
                    &shared_root
                ))
                .unwrap()
        );
        assert!(
            runner
                .import_proposal_vote(signed_proposal_vote(
                    runner.crypto.as_ref(),
                    3,
                    &shared_root
                ))
                .unwrap()
        );

        let mut root_ledger = LedgerState::from_genesis(&genesis);
        root_ledger.apply_block(&shared_root, &crypto).unwrap();
        let slot = ((shared_root.header.slot + 1)..(shared_root.header.slot + 51))
            .find(|slot| consensus.proposer_for_slot(*slot) == 3)
            .unwrap();
        let epoch = consensus.epoch_for_slot(slot);
        let transactions = Vec::new();
        let commitment: Option<TopologyCommitment> = None;
        let header = BlockHeader {
            block_number: 2,
            parent_hash: shared_root.block_hash,
            slot,
            epoch,
            proposer_id: 3,
            timestamp_unix_millis: now_unix_millis(),
            state_root: root_ledger.state_root(),
            transactions_root: canonical_hash(&transactions),
            topology_root: empty_hash(),
        };
        let block_hash = canonical_hash(&(header.clone(), &transactions, &commitment));
        let certified_tip = Block {
            header,
            transactions,
            commitment,
            commitment_receipts: Vec::new(),
            signature: crypto.sign(3, &block_hash).unwrap(),
            block_hash,
        };
        assert_eq!(
            runner
                .accept_block(certified_tip.clone(), false)
                .await
                .unwrap(),
            BlockAcceptance::Orphan
        );
        assert!(
            runner
                .import_proposal_vote(signed_proposal_vote(
                    runner.crypto.as_ref(),
                    2,
                    &certified_tip
                ))
                .unwrap()
        );
        assert!(
            runner
                .import_proposal_vote(signed_proposal_vote(
                    runner.crypto.as_ref(),
                    4,
                    &certified_tip
                ))
                .unwrap()
        );

        runner.record_peer_sync_status(
            2,
            1,
            shared_root.block_hash,
            Some(shared_root.block_hash),
            shared_root.header.block_number,
            vec![SyncQcAnchor {
                block_hash: shared_root.block_hash,
                block_number: shared_root.header.block_number,
            }],
        );

        runner
            .push_best_sync_to(2, 1, shared_root.block_hash)
            .unwrap();

        let mut saw_certified = false;
        let mut saw_sync_blocks = false;
        let deadline = tokio::time::Instant::now() + Duration::from_millis(250);
        while !(saw_certified && saw_sync_blocks) {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            assert!(
                !remaining.is_zero(),
                "expected first proactive push to send certified and anchored suffix messages"
            );
            match timeout(remaining, recv_protocol_message(&mut peer_events))
                .await
                .unwrap()
            {
                ProtocolMessage::CertifiedSyncResponse(ChunkedSyncResponse::Certified {
                    ..
                }) => {
                    saw_certified = true;
                }
                ProtocolMessage::SyncBlocks { .. } => {
                    saw_sync_blocks = true;
                }
                ProtocolMessage::ProposalVote(_)
                | ProtocolMessage::QuorumCertificate(_)
                | ProtocolMessage::SyncStatus { .. }
                | ProtocolMessage::SyncResponse { .. }
                | ProtocolMessage::HeartbeatPulse(_)
                | ProtocolMessage::ReceiptFetch { .. } => {}
                other => panic!("expected first proactive sync push, got {other:?}"),
            }
        }

        runner
            .push_best_sync_to(2, 1, shared_root.block_hash)
            .unwrap();

        assert!(
            timeout(Duration::from_millis(25), peer_events.recv())
                .await
                .is_err(),
            "expected duplicate proactive sync push to be suppressed"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn new_qc_immediately_pushes_certified_sync_to_stale_peer() {
        let genesis = sample_genesis();
        let local_address = reserve_local_address();
        let peer_address = reserve_local_address();
        let peer = PeerConfig {
            validator_id: 2,
            address: peer_address.clone(),
        };
        let config = sample_node_config(1, local_address, vec![peer.clone()], "qc-progress-push");
        let mut runner = build_test_runner(1, config, genesis.clone()).await;
        let (_, _, _, mut peer_events) =
            spawn_test_network(2, peer_address, Vec::new(), &genesis).await;
        let shared_root = first_valid_block(&genesis);

        assert_eq!(
            runner
                .accept_block(shared_root.clone(), true)
                .await
                .unwrap(),
            BlockAcceptance::Accepted
        );
        assert!(
            runner
                .import_proposal_vote(signed_proposal_vote(
                    runner.crypto.as_ref(),
                    2,
                    &shared_root
                ))
                .unwrap()
        );
        assert!(
            runner
                .import_proposal_vote(signed_proposal_vote(
                    runner.crypto.as_ref(),
                    3,
                    &shared_root
                ))
                .unwrap()
        );

        runner
            .handle_network_event(NetworkEvent::Received {
                from_validator_id: 2,
                payload: ProtocolMessage::SyncStatus {
                    validator_id: 2,
                    height: 1,
                    tip_hash: shared_root.block_hash,
                    highest_qc_hash: Some(shared_root.block_hash),
                    highest_qc_height: 1,
                    recent_qc_anchors: vec![SyncQcAnchor {
                        block_hash: shared_root.block_hash,
                        block_number: 1,
                    }],
                },
                bytes: 0,
            })
            .await
            .unwrap();

        let crypto = DeterministicCryptoBackend::from_genesis(&genesis);
        let consensus = ConsensusEngine::new(genesis.clone());
        let mut root_ledger = LedgerState::from_genesis(&genesis);
        root_ledger.apply_block(&shared_root, &crypto).unwrap();
        let slot = ((shared_root.header.slot + 1)..(shared_root.header.slot + 50))
            .find(|slot| consensus.proposer_for_slot(*slot) == 3)
            .unwrap();
        let epoch = consensus.epoch_for_slot(slot);
        let transactions = Vec::new();
        let commitment: Option<TopologyCommitment> = None;
        let header = BlockHeader {
            block_number: 2,
            parent_hash: shared_root.block_hash,
            slot,
            epoch,
            proposer_id: 3,
            timestamp_unix_millis: now_unix_millis(),
            state_root: root_ledger.state_root(),
            transactions_root: canonical_hash(&transactions),
            topology_root: empty_hash(),
        };
        let block_hash = canonical_hash(&(header.clone(), &transactions, &commitment));
        let child = Block {
            header,
            transactions,
            commitment,
            commitment_receipts: Vec::new(),
            signature: crypto.sign(3, &block_hash).unwrap(),
            block_hash,
        };

        assert_eq!(
            runner.accept_block(child.clone(), false).await.unwrap(),
            BlockAcceptance::Orphan
        );
        assert!(
            runner
                .import_proposal_vote(signed_proposal_vote(runner.crypto.as_ref(), 2, &child))
                .unwrap()
        );
        assert!(
            runner
                .import_proposal_vote(signed_proposal_vote(runner.crypto.as_ref(), 4, &child))
                .unwrap()
        );

        loop {
            match recv_protocol_message(&mut peer_events).await {
                ProtocolMessage::CertifiedSyncResponse(ChunkedSyncResponse::Certified {
                    shared_qc_hash,
                    shared_qc_height,
                    blocks,
                    qcs,
                    ..
                }) => {
                    assert_eq!(shared_qc_hash, shared_root.block_hash);
                    assert_eq!(shared_qc_height, 1);
                    assert_eq!(blocks.len(), 1);
                    assert_eq!(qcs.len(), 1);
                    assert_eq!(blocks[0].block_hash, child.block_hash);
                    break;
                }
                ProtocolMessage::ProposalVote(_)
                | ProtocolMessage::QuorumCertificate(_)
                | ProtocolMessage::SyncStatus { .. }
                | ProtocolMessage::SyncResponse { .. }
                | ProtocolMessage::HeartbeatPulse(_)
                | ProtocolMessage::ReceiptFetch { .. } => {}
                other => panic!("expected certified sync response after new QC, got {other:?}"),
            }
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn sync_status_preserves_qc_anchors_across_tip_only_updates() {
        let genesis = sample_genesis();
        let local_address = reserve_local_address();
        let peer_address = reserve_local_address();
        let peer = PeerConfig {
            validator_id: 2,
            address: peer_address.clone(),
        };
        let config = sample_node_config(1, local_address, vec![peer], "sync-status-preserve-qc");
        let mut runner = build_test_runner(1, config, genesis.clone()).await;
        let (_, _, _, mut peer_events) =
            spawn_test_network(2, peer_address, Vec::new(), &genesis).await;
        let shared_root = first_valid_block(&genesis);

        assert_eq!(
            runner
                .accept_block(shared_root.clone(), true)
                .await
                .unwrap(),
            BlockAcceptance::Accepted
        );
        assert!(
            runner
                .import_proposal_vote(signed_proposal_vote(
                    runner.crypto.as_ref(),
                    2,
                    &shared_root
                ))
                .unwrap()
        );
        assert!(
            runner
                .import_proposal_vote(signed_proposal_vote(
                    runner.crypto.as_ref(),
                    3,
                    &shared_root
                ))
                .unwrap()
        );

        let crypto = DeterministicCryptoBackend::from_genesis(&genesis);
        let consensus = ConsensusEngine::new(genesis.clone());
        let mut root_ledger = LedgerState::from_genesis(&genesis);
        root_ledger.apply_block(&shared_root, &crypto).unwrap();
        let slot = ((shared_root.header.slot + 1)..(shared_root.header.slot + 51))
            .find(|slot| consensus.proposer_for_slot(*slot) == 3)
            .unwrap();
        let epoch = consensus.epoch_for_slot(slot);
        let transactions = Vec::new();
        let commitment: Option<TopologyCommitment> = None;
        let header = BlockHeader {
            block_number: 2,
            parent_hash: shared_root.block_hash,
            slot,
            epoch,
            proposer_id: 3,
            timestamp_unix_millis: now_unix_millis(),
            state_root: root_ledger.state_root(),
            transactions_root: canonical_hash(&transactions),
            topology_root: empty_hash(),
        };
        let block_hash = canonical_hash(&(header.clone(), &transactions, &commitment));
        let certified_tip = Block {
            header,
            transactions,
            commitment,
            commitment_receipts: Vec::new(),
            signature: crypto.sign(3, &block_hash).unwrap(),
            block_hash,
        };
        assert_eq!(
            runner
                .accept_block(certified_tip.clone(), false)
                .await
                .unwrap(),
            BlockAcceptance::Orphan
        );
        assert!(
            runner
                .import_proposal_vote(signed_proposal_vote(
                    runner.crypto.as_ref(),
                    2,
                    &certified_tip
                ))
                .unwrap()
        );
        assert!(
            runner
                .import_proposal_vote(signed_proposal_vote(
                    runner.crypto.as_ref(),
                    4,
                    &certified_tip
                ))
                .unwrap()
        );

        runner.record_peer_sync_status(
            2,
            1,
            shared_root.block_hash,
            Some(shared_root.block_hash),
            shared_root.header.block_number,
            vec![SyncQcAnchor {
                block_hash: shared_root.block_hash,
                block_number: shared_root.header.block_number,
            }],
        );
        runner.record_peer_sync_status(2, 2, canonical_hash(&"peer-tip"), None, 0, Vec::new());

        let status = runner.peer_sync_status.get(&2).unwrap();
        assert_eq!(status.highest_qc_hash, Some(shared_root.block_hash));
        assert_eq!(status.highest_qc_height, shared_root.header.block_number);
        assert_eq!(status.recent_qc_anchors.len(), 1);
        assert_eq!(
            status.recent_qc_anchors[0].block_hash,
            shared_root.block_hash
        );

        runner
            .push_best_sync_to(2, 1, shared_root.block_hash)
            .unwrap();

        loop {
            match recv_protocol_message(&mut peer_events).await {
                ProtocolMessage::CertifiedSyncResponse(ChunkedSyncResponse::Certified {
                    shared_qc_hash,
                    ..
                }) => {
                    assert_eq!(shared_qc_hash, shared_root.block_hash);
                    break;
                }
                ProtocolMessage::ProposalVote(_)
                | ProtocolMessage::QuorumCertificate(_)
                | ProtocolMessage::SyncStatus { .. }
                | ProtocolMessage::SyncResponse { .. }
                | ProtocolMessage::HeartbeatPulse(_)
                | ProtocolMessage::ReceiptFetch { .. } => {}
                other => panic!("expected certified sync response, got {other:?}"),
            }
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn sync_status_accepts_lower_certified_frontier_after_restart() {
        let genesis = sample_genesis();
        let config = sample_node_config(
            1,
            reserve_local_address(),
            vec![PeerConfig {
                validator_id: 2,
                address: reserve_local_address(),
            }],
            "sync-status-restart-frontier",
        );
        let mut runner = build_test_runner(1, config, genesis).await;
        let ahead_tip = canonical_hash(&"ahead-tip");
        let stale_tip = canonical_hash(&"stale-tip");
        let shared_qc_hash = canonical_hash(&"shared-qc");

        runner.record_peer_sync_status(
            2,
            7,
            ahead_tip,
            Some(shared_qc_hash),
            6,
            vec![SyncQcAnchor {
                block_hash: shared_qc_hash,
                block_number: 6,
            }],
        );
        runner.record_peer_sync_status(
            2,
            5,
            stale_tip,
            Some(stale_tip),
            5,
            vec![SyncQcAnchor {
                block_hash: stale_tip,
                block_number: 5,
            }],
        );

        let status = runner.peer_sync_status.get(&2).unwrap();
        assert_eq!(status.height, 5);
        assert_eq!(status.tip_hash, stale_tip);
        assert_eq!(status.highest_qc_hash, Some(stale_tip));
        assert_eq!(status.highest_qc_height, 5);
        assert_eq!(
            status.recent_qc_anchors,
            vec![SyncQcAnchor {
                block_hash: stale_tip,
                block_number: 5,
            }]
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn certified_sync_unavailable_fails_over_to_another_ahead_peer() {
        let genesis = sample_genesis();
        let local_address = reserve_local_address();
        let peer2_address = reserve_local_address();
        let peer3_address = reserve_local_address();
        let config = sample_node_config(
            1,
            local_address,
            vec![
                PeerConfig {
                    validator_id: 2,
                    address: peer2_address.clone(),
                },
                PeerConfig {
                    validator_id: 3,
                    address: peer3_address.clone(),
                },
            ],
            "sync-unavailable-failover",
        );
        let mut runner = build_test_runner(1, config, genesis.clone()).await;
        let (_, _, _, mut peer2_events) =
            spawn_test_network(2, peer2_address, Vec::new(), &genesis).await;
        let (_, _, _, mut peer3_events) =
            spawn_test_network(3, peer3_address, Vec::new(), &genesis).await;
        let shared_root = first_valid_block(&genesis);

        assert_eq!(
            runner
                .accept_block(shared_root.clone(), true)
                .await
                .unwrap(),
            BlockAcceptance::Accepted
        );
        assert!(
            runner
                .import_proposal_vote(signed_proposal_vote(
                    runner.crypto.as_ref(),
                    2,
                    &shared_root
                ))
                .unwrap()
        );
        assert!(
            runner
                .import_proposal_vote(signed_proposal_vote(
                    runner.crypto.as_ref(),
                    3,
                    &shared_root
                ))
                .unwrap()
        );

        runner.record_peer_sync_status(
            2,
            4,
            canonical_hash(&"peer-two-tip"),
            Some(shared_root.block_hash),
            shared_root.header.block_number,
            vec![SyncQcAnchor {
                block_hash: shared_root.block_hash,
                block_number: shared_root.header.block_number,
            }],
        );
        runner.record_peer_sync_status(
            3,
            5,
            canonical_hash(&"peer-three-tip"),
            Some(shared_root.block_hash),
            shared_root.header.block_number,
            vec![SyncQcAnchor {
                block_hash: shared_root.block_hash,
                block_number: shared_root.header.block_number,
            }],
        );

        runner
            .handle_network_event(NetworkEvent::Received {
                from_validator_id: 2,
                payload: ProtocolMessage::CertifiedSyncResponse(ChunkedSyncResponse::Unavailable {
                    responder_id: 2,
                }),
                bytes: 0,
            })
            .await
            .unwrap();

        loop {
            match recv_protocol_message(&mut peer3_events).await {
                ProtocolMessage::SyncRequest {
                    requester_id,
                    known_height,
                    known_tip_hash,
                } => {
                    assert_eq!(requester_id, 1);
                    assert_eq!(known_height, shared_root.header.block_number);
                    assert_eq!(known_tip_hash, shared_root.block_hash);
                    break;
                }
                ProtocolMessage::SyncStatus { .. }
                | ProtocolMessage::SyncResponse { .. }
                | ProtocolMessage::ProposalVote(_)
                | ProtocolMessage::QuorumCertificate(_)
                | ProtocolMessage::CertifiedSyncRequest(_)
                | ProtocolMessage::CertifiedSyncResponse(_)
                | ProtocolMessage::SyncBlocks { .. }
                | ProtocolMessage::HeartbeatPulse(_)
                | ProtocolMessage::ReceiptFetch { .. } => {}
                other => panic!("expected failover sync request, got {other:?}"),
            }
        }

        while timeout(Duration::from_millis(25), peer2_events.recv())
            .await
            .ok()
            .flatten()
            .is_some()
        {}
    }

    #[tokio::test(flavor = "current_thread")]
    async fn sync_status_merges_qc_anchor_history_across_updates() {
        let genesis = sample_genesis();
        let config = sample_node_config(
            1,
            reserve_local_address(),
            Vec::new(),
            "sync-status-merge-qc-history",
        );
        let mut runner = build_test_runner(1, config, genesis).await;
        let anchor_one = SyncQcAnchor {
            block_hash: canonical_hash(&"anchor-one"),
            block_number: 10,
        };
        let anchor_two = SyncQcAnchor {
            block_hash: canonical_hash(&"anchor-two"),
            block_number: 8,
        };
        let anchor_three = SyncQcAnchor {
            block_hash: canonical_hash(&"anchor-three"),
            block_number: 12,
        };

        runner.record_peer_sync_status(
            2,
            10,
            canonical_hash(&"tip-one"),
            Some(anchor_one.block_hash),
            anchor_one.block_number,
            vec![anchor_one.clone(), anchor_two.clone()],
        );
        runner.record_peer_sync_status(
            2,
            12,
            canonical_hash(&"tip-two"),
            Some(anchor_three.block_hash),
            anchor_three.block_number,
            vec![anchor_three.clone()],
        );

        let status = runner.peer_sync_status.get(&2).unwrap();
        assert_eq!(status.highest_qc_hash, Some(anchor_three.block_hash));
        assert_eq!(status.highest_qc_height, anchor_three.block_number);
        assert_eq!(
            status
                .recent_qc_anchors
                .iter()
                .map(|anchor| anchor.block_hash)
                .collect::<Vec<_>>(),
            vec![
                anchor_three.block_hash,
                anchor_one.block_hash,
                anchor_two.block_hash,
            ]
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn sync_status_without_shared_qc_falls_back_to_legacy_sync_push() {
        let genesis = sample_genesis();
        let local_address = reserve_local_address();
        let peer_address = reserve_local_address();
        let peer = PeerConfig {
            validator_id: 2,
            address: peer_address.clone(),
        };
        let config = sample_node_config(1, local_address, vec![peer], "sync-status-legacy");
        let mut runner = build_test_runner(1, config, genesis.clone()).await;
        let (_, _, _, mut peer_events) =
            spawn_test_network(2, peer_address, Vec::new(), &genesis).await;
        let shared_root = first_valid_block(&genesis);

        assert_eq!(
            runner
                .accept_block(shared_root.clone(), true)
                .await
                .unwrap(),
            BlockAcceptance::Accepted
        );

        runner
            .handle_network_event(NetworkEvent::Received {
                from_validator_id: 2,
                payload: ProtocolMessage::SyncStatus {
                    validator_id: 2,
                    height: 0,
                    tip_hash: empty_hash(),
                    highest_qc_hash: None,
                    highest_qc_height: 0,
                    recent_qc_anchors: Vec::new(),
                },
                bytes: 0,
            })
            .await
            .unwrap();

        loop {
            match recv_protocol_message(&mut peer_events).await {
                ProtocolMessage::SyncBlocks { .. } | ProtocolMessage::SyncResponse { .. } => break,
                ProtocolMessage::ProposalVote(_)
                | ProtocolMessage::QuorumCertificate(_)
                | ProtocolMessage::HeartbeatPulse(_)
                | ProtocolMessage::ReceiptFetch { .. } => {}
                other => panic!("expected legacy sync push, got {other:?}"),
            }
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn sync_status_with_same_qc_and_ahead_tip_prefers_legacy_sync_push_in_v2() {
        let genesis = sample_genesis();
        let local_address = reserve_local_address();
        let peer_address = reserve_local_address();
        let peer = PeerConfig {
            validator_id: 2,
            address: peer_address.clone(),
        };
        let config = sample_node_config(
            1,
            local_address,
            vec![peer],
            "sync-status-same-qc-legacy-push",
        );
        let mut runner = build_test_runner(1, config, genesis.clone()).await;
        let (_, _, _, mut peer_events) =
            spawn_test_network(2, peer_address, Vec::new(), &genesis).await;
        let shared_root = first_valid_block(&genesis);

        assert_eq!(
            runner
                .accept_block(shared_root.clone(), true)
                .await
                .unwrap(),
            BlockAcceptance::Accepted
        );
        assert!(
            runner
                .import_proposal_vote(signed_proposal_vote(
                    runner.crypto.as_ref(),
                    2,
                    &shared_root
                ))
                .unwrap()
        );
        assert!(
            runner
                .import_proposal_vote(signed_proposal_vote(
                    runner.crypto.as_ref(),
                    3,
                    &shared_root
                ))
                .unwrap()
        );

        runner.record_peer_sync_status(
            2,
            2,
            canonical_hash(&"peer-ahead-uncertified"),
            Some(shared_root.block_hash),
            shared_root.header.block_number,
            vec![SyncQcAnchor {
                block_hash: shared_root.block_hash,
                block_number: shared_root.header.block_number,
            }],
        );

        runner
            .push_best_sync_to(2, 1, shared_root.block_hash)
            .unwrap();

        loop {
            match recv_protocol_message(&mut peer_events).await {
                ProtocolMessage::SyncBlocks { .. } | ProtocolMessage::SyncResponse { .. } => break,
                ProtocolMessage::ProposalVote(_)
                | ProtocolMessage::QuorumCertificate(_)
                | ProtocolMessage::SyncStatus { .. }
                | ProtocolMessage::HeartbeatPulse(_)
                | ProtocolMessage::ReceiptFetch { .. } => {}
                other => panic!("expected legacy sync push, got {other:?}"),
            }
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn sync_status_with_same_qc_and_ahead_tip_prefers_legacy_sync_request_in_v2() {
        let genesis = sample_genesis();
        let local_address = reserve_local_address();
        let peer_address = reserve_local_address();
        let peer = PeerConfig {
            validator_id: 2,
            address: peer_address.clone(),
        };
        let config = sample_node_config(1, local_address, vec![peer], "sync-status-same-qc-legacy");
        let mut runner = build_test_runner(1, config, genesis.clone()).await;
        let (_, _, _, mut peer_events) =
            spawn_test_network(2, peer_address, Vec::new(), &genesis).await;
        let shared_root = first_valid_block(&genesis);

        assert_eq!(
            runner
                .accept_block(shared_root.clone(), true)
                .await
                .unwrap(),
            BlockAcceptance::Accepted
        );
        assert!(
            runner
                .import_proposal_vote(signed_proposal_vote(
                    runner.crypto.as_ref(),
                    2,
                    &shared_root
                ))
                .unwrap()
        );
        assert!(
            runner
                .import_proposal_vote(signed_proposal_vote(
                    runner.crypto.as_ref(),
                    3,
                    &shared_root
                ))
                .unwrap()
        );

        runner
            .handle_network_event(NetworkEvent::Received {
                from_validator_id: 2,
                payload: ProtocolMessage::SyncStatus {
                    validator_id: 2,
                    height: 2,
                    tip_hash: canonical_hash(&"peer-ahead-uncertified"),
                    highest_qc_hash: Some(shared_root.block_hash),
                    highest_qc_height: shared_root.header.block_number,
                    recent_qc_anchors: vec![SyncQcAnchor {
                        block_hash: shared_root.block_hash,
                        block_number: shared_root.header.block_number,
                    }],
                },
                bytes: 0,
            })
            .await
            .unwrap();

        loop {
            match recv_protocol_message(&mut peer_events).await {
                ProtocolMessage::SyncRequest {
                    requester_id,
                    known_height,
                    known_tip_hash,
                } => {
                    assert_eq!(requester_id, 1);
                    assert_eq!(known_height, shared_root.header.block_number);
                    assert_eq!(known_tip_hash, shared_root.block_hash);
                    break;
                }
                ProtocolMessage::ProposalVote(_)
                | ProtocolMessage::QuorumCertificate(_)
                | ProtocolMessage::SyncStatus { .. }
                | ProtocolMessage::SyncBlocks { .. }
                | ProtocolMessage::SyncResponse { .. }
                | ProtocolMessage::HeartbeatPulse(_)
                | ProtocolMessage::ReceiptFetch { .. } => {}
                other => panic!("expected legacy sync request, got {other:?}"),
            }
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn best_available_v2_sync_request_pulls_same_qc_ahead_peer() {
        let genesis = sample_genesis();
        let local_address = reserve_local_address();
        let peer_address = reserve_local_address();
        let peer = PeerConfig {
            validator_id: 2,
            address: peer_address.clone(),
        };
        let config = sample_node_config(1, local_address, vec![peer], "best-v2-sync-same-qc-ahead");
        let mut runner = build_test_runner(1, config, genesis.clone()).await;
        let (_, _, _, mut peer_events) =
            spawn_test_network(2, peer_address, Vec::new(), &genesis).await;
        let shared_root = first_valid_block(&genesis);

        assert_eq!(
            runner
                .accept_block(shared_root.clone(), true)
                .await
                .unwrap(),
            BlockAcceptance::Accepted
        );
        assert!(
            runner
                .import_proposal_vote(signed_proposal_vote(
                    runner.crypto.as_ref(),
                    2,
                    &shared_root
                ))
                .unwrap()
        );
        assert!(
            runner
                .import_proposal_vote(signed_proposal_vote(
                    runner.crypto.as_ref(),
                    3,
                    &shared_root
                ))
                .unwrap()
        );
        runner.record_peer_sync_status(
            2,
            2,
            canonical_hash(&"peer-ahead-uncertified"),
            Some(shared_root.block_hash),
            shared_root.header.block_number,
            vec![SyncQcAnchor {
                block_hash: shared_root.block_hash,
                block_number: shared_root.header.block_number,
            }],
        );

        assert!(
            runner
                .request_best_available_sync_from_ahead_peers()
                .unwrap()
        );

        loop {
            match recv_protocol_message(&mut peer_events).await {
                ProtocolMessage::SyncRequest { requester_id, .. } => {
                    assert_eq!(requester_id, 1);
                    break;
                }
                ProtocolMessage::ProposalVote(_)
                | ProtocolMessage::QuorumCertificate(_)
                | ProtocolMessage::SyncStatus { .. }
                | ProtocolMessage::SyncBlocks { .. }
                | ProtocolMessage::SyncResponse { .. }
                | ProtocolMessage::HeartbeatPulse(_)
                | ProtocolMessage::ReceiptFetch { .. } => {}
                other => panic!("expected best available legacy sync request, got {other:?}"),
            }
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn pending_certified_child_from_peer_triggers_followup_best_available_sync_request() {
        let genesis = sample_genesis();
        let local_address = reserve_local_address();
        let peer_address = reserve_local_address();
        let peer = PeerConfig {
            validator_id: 2,
            address: peer_address.clone(),
        };
        let config = sample_node_config(
            1,
            local_address,
            vec![peer],
            "pending-certified-child-followup-sync",
        );
        let mut runner = build_test_runner(1, config, genesis.clone()).await;
        let (_, _, _, mut peer_events) =
            spawn_test_network(2, peer_address, Vec::new(), &genesis).await;
        let shared_root = first_valid_block(&genesis);

        assert_eq!(
            runner
                .accept_block(shared_root.clone(), true)
                .await
                .unwrap(),
            BlockAcceptance::Accepted
        );
        assert!(
            runner
                .import_proposal_vote(signed_proposal_vote(
                    runner.crypto.as_ref(),
                    2,
                    &shared_root
                ))
                .unwrap()
        );
        assert!(
            runner
                .import_proposal_vote(signed_proposal_vote(
                    runner.crypto.as_ref(),
                    3,
                    &shared_root
                ))
                .unwrap()
        );

        let crypto = DeterministicCryptoBackend::from_genesis(&genesis);
        let consensus = ConsensusEngine::new(genesis.clone());
        let mut root_ledger = LedgerState::from_genesis(&genesis);
        root_ledger.apply_block(&shared_root, &crypto).unwrap();
        let child_slot = ((shared_root.header.slot + 1)..(shared_root.header.slot + 50))
            .find(|slot| consensus.proposer_for_slot(*slot) == 2)
            .unwrap();
        let child_epoch = consensus.epoch_for_slot(child_slot);
        let child_transactions = Vec::new();
        let child_commitment: Option<TopologyCommitment> = None;
        let child_header = BlockHeader {
            block_number: 2,
            parent_hash: shared_root.block_hash,
            slot: child_slot,
            epoch: child_epoch,
            proposer_id: 2,
            timestamp_unix_millis: now_unix_millis(),
            state_root: root_ledger.state_root(),
            transactions_root: canonical_hash(&child_transactions),
            topology_root: empty_hash(),
        };
        let child_hash =
            canonical_hash(&(child_header.clone(), &child_transactions, &child_commitment));
        let pending_child = Block {
            header: child_header,
            transactions: child_transactions,
            commitment: child_commitment,
            commitment_receipts: Vec::new(),
            signature: crypto.sign(2, &child_hash).unwrap(),
            block_hash: child_hash,
        };

        runner
            .handle_network_event(NetworkEvent::Received {
                from_validator_id: 2,
                payload: ProtocolMessage::BlockProposal(pending_child),
                bytes: 0,
            })
            .await
            .unwrap();

        loop {
            match recv_protocol_message(&mut peer_events).await {
                ProtocolMessage::SyncRequest {
                    requester_id,
                    known_height,
                    known_tip_hash,
                } => {
                    assert_eq!(requester_id, 1);
                    assert_eq!(known_height, shared_root.header.block_number);
                    assert_eq!(known_tip_hash, shared_root.block_hash);
                    break;
                }
                ProtocolMessage::ProposalVote(_)
                | ProtocolMessage::QuorumCertificate(_)
                | ProtocolMessage::SyncStatus { .. }
                | ProtocolMessage::SyncBlocks { .. }
                | ProtocolMessage::SyncResponse { .. }
                | ProtocolMessage::CertifiedSyncResponse(_)
                | ProtocolMessage::CertifiedSyncRequest(_)
                | ProtocolMessage::RelayReceipt(_)
                | ProtocolMessage::HeartbeatPulse(_)
                | ProtocolMessage::ReceiptFetch { .. }
                | ProtocolMessage::ReceiptResponse { .. } => {}
                other => panic!("expected follow-up sync request, got {other:?}"),
            }
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn pending_certified_child_from_peer_uses_legacy_sync_even_without_peer_status_qc() {
        let genesis = sample_genesis();
        let local_address = reserve_local_address();
        let peer_address = reserve_local_address();
        let peer = PeerConfig {
            validator_id: 2,
            address: peer_address.clone(),
        };
        let config = sample_node_config(
            1,
            local_address,
            vec![peer],
            "pending-certified-child-direct-legacy",
        );
        let mut runner = build_test_runner(1, config, genesis.clone()).await;
        let (_, _, _, mut peer_events) =
            spawn_test_network(2, peer_address, Vec::new(), &genesis).await;
        let shared_root = first_valid_block(&genesis);

        assert_eq!(
            runner
                .accept_block(shared_root.clone(), true)
                .await
                .unwrap(),
            BlockAcceptance::Accepted
        );
        assert!(
            runner
                .import_proposal_vote(signed_proposal_vote(
                    runner.crypto.as_ref(),
                    2,
                    &shared_root
                ))
                .unwrap()
        );
        assert!(
            runner
                .import_proposal_vote(signed_proposal_vote(
                    runner.crypto.as_ref(),
                    3,
                    &shared_root
                ))
                .unwrap()
        );

        let crypto = DeterministicCryptoBackend::from_genesis(&genesis);
        let consensus = ConsensusEngine::new(genesis.clone());
        let mut root_ledger = LedgerState::from_genesis(&genesis);
        root_ledger.apply_block(&shared_root, &crypto).unwrap();
        let child_slot = ((shared_root.header.slot + 1)..(shared_root.header.slot + 50))
            .find(|slot| consensus.proposer_for_slot(*slot) == 2)
            .unwrap();
        let child_epoch = consensus.epoch_for_slot(child_slot);
        let child_transactions = Vec::new();
        let child_commitment: Option<TopologyCommitment> = None;
        let child_header = BlockHeader {
            block_number: 2,
            parent_hash: shared_root.block_hash,
            slot: child_slot,
            epoch: child_epoch,
            proposer_id: 2,
            timestamp_unix_millis: now_unix_millis(),
            state_root: root_ledger.state_root(),
            transactions_root: canonical_hash(&child_transactions),
            topology_root: empty_hash(),
        };
        let child_hash =
            canonical_hash(&(child_header.clone(), &child_transactions, &child_commitment));
        let pending_child = Block {
            header: child_header,
            transactions: child_transactions,
            commitment: child_commitment,
            commitment_receipts: Vec::new(),
            signature: crypto.sign(2, &child_hash).unwrap(),
            block_hash: child_hash,
        };

        runner
            .handle_network_event(NetworkEvent::Received {
                from_validator_id: 2,
                payload: ProtocolMessage::BlockProposal(pending_child),
                bytes: 0,
            })
            .await
            .unwrap();

        loop {
            match recv_protocol_message(&mut peer_events).await {
                ProtocolMessage::SyncRequest {
                    requester_id,
                    known_height,
                    known_tip_hash,
                } => {
                    assert_eq!(requester_id, 1);
                    assert_eq!(known_height, shared_root.header.block_number);
                    assert_eq!(known_tip_hash, shared_root.block_hash);
                    break;
                }
                ProtocolMessage::ProposalVote(_)
                | ProtocolMessage::QuorumCertificate(_)
                | ProtocolMessage::SyncStatus { .. }
                | ProtocolMessage::SyncBlocks { .. }
                | ProtocolMessage::SyncResponse { .. }
                | ProtocolMessage::CertifiedSyncResponse(_)
                | ProtocolMessage::CertifiedSyncRequest(_)
                | ProtocolMessage::RelayReceipt(_)
                | ProtocolMessage::HeartbeatPulse(_)
                | ProtocolMessage::ReceiptFetch { .. }
                | ProtocolMessage::ReceiptResponse { .. } => {}
                other => panic!("expected direct legacy sync request, got {other:?}"),
            }
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn stale_pending_certified_child_retries_best_available_sync_on_later_slot() {
        let genesis = sample_genesis();
        let local_address = reserve_local_address();
        let peer_address = reserve_local_address();
        let peer = PeerConfig {
            validator_id: 2,
            address: peer_address.clone(),
        };
        let config = sample_node_config(
            1,
            local_address,
            vec![peer],
            "stale-pending-certified-retry",
        );
        let mut runner = build_test_runner(1, config, genesis.clone()).await;
        let (_, _, _, mut peer_events) =
            spawn_test_network(2, peer_address, Vec::new(), &genesis).await;
        let shared_root = first_valid_block(&genesis);
        let crypto = DeterministicCryptoBackend::from_genesis(&genesis);
        let consensus = ConsensusEngine::new(genesis.clone());

        assert_eq!(
            runner
                .accept_block(shared_root.clone(), true)
                .await
                .unwrap(),
            BlockAcceptance::Accepted
        );
        assert!(
            runner
                .import_proposal_vote(signed_proposal_vote(
                    runner.crypto.as_ref(),
                    2,
                    &shared_root
                ))
                .unwrap()
        );
        assert!(
            runner
                .import_proposal_vote(signed_proposal_vote(
                    runner.crypto.as_ref(),
                    3,
                    &shared_root
                ))
                .unwrap()
        );

        let mut root_ledger = LedgerState::from_genesis(&genesis);
        root_ledger.apply_block(&shared_root, &crypto).unwrap();
        let slot = ((shared_root.header.slot + 1)..(shared_root.header.slot + 50))
            .find(|slot| consensus.proposer_for_slot(*slot) == 2)
            .unwrap();
        let epoch = consensus.epoch_for_slot(slot);
        let transactions = Vec::new();
        let commitment: Option<TopologyCommitment> = None;
        let header = BlockHeader {
            block_number: 2,
            parent_hash: shared_root.block_hash,
            slot,
            epoch,
            proposer_id: 2,
            timestamp_unix_millis: now_unix_millis(),
            state_root: root_ledger.state_root(),
            transactions_root: canonical_hash(&transactions),
            topology_root: empty_hash(),
        };
        let block_hash = canonical_hash(&(header.clone(), &transactions, &commitment));
        let pending_child = Block {
            header,
            transactions,
            commitment,
            commitment_receipts: Vec::new(),
            signature: crypto.sign(2, &block_hash).unwrap(),
            block_hash,
        };
        assert_eq!(
            runner
                .accept_block(pending_child.clone(), false)
                .await
                .unwrap(),
            BlockAcceptance::Orphan
        );

        while timeout(Duration::from_millis(25), peer_events.recv())
            .await
            .ok()
            .flatten()
            .is_some()
        {}

        assert!(
            runner
                .maybe_repair_stale_pending_certified_child(pending_child.header.slot + 1)
                .unwrap()
        );

        loop {
            match recv_protocol_message(&mut peer_events).await {
                ProtocolMessage::SyncRequest {
                    requester_id,
                    known_height,
                    known_tip_hash,
                } => {
                    assert_eq!(requester_id, 1);
                    assert_eq!(known_height, shared_root.header.block_number);
                    assert_eq!(known_tip_hash, shared_root.block_hash);
                    break;
                }
                ProtocolMessage::ProposalVote(_)
                | ProtocolMessage::QuorumCertificate(_)
                | ProtocolMessage::SyncStatus { .. }
                | ProtocolMessage::SyncBlocks { .. }
                | ProtocolMessage::SyncResponse { .. }
                | ProtocolMessage::CertifiedSyncResponse(_)
                | ProtocolMessage::CertifiedSyncRequest(_)
                | ProtocolMessage::RelayReceipt(_)
                | ProtocolMessage::HeartbeatPulse(_)
                | ProtocolMessage::ReceiptFetch { .. }
                | ProtocolMessage::ReceiptResponse { .. } => {}
                other => {
                    panic!(
                        "expected best-available sync retry for stale pending child, got {other:?}"
                    )
                }
            }
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn stale_pending_certified_child_prefers_legacy_suffix_sync_for_same_qc_ahead_peer() {
        let genesis = sample_genesis();
        let local_address = reserve_local_address();
        let peer_address = reserve_local_address();
        let peer = PeerConfig {
            validator_id: 2,
            address: peer_address.clone(),
        };
        let config = sample_node_config(
            1,
            local_address,
            vec![peer],
            "stale-pending-certified-legacy-followup",
        );
        let mut runner = build_test_runner(1, config, genesis.clone()).await;
        let (_, _, _, mut peer_events) =
            spawn_test_network(2, peer_address, Vec::new(), &genesis).await;
        let shared_root = first_valid_block(&genesis);
        let crypto = DeterministicCryptoBackend::from_genesis(&genesis);
        let consensus = ConsensusEngine::new(genesis.clone());

        assert_eq!(
            runner
                .accept_block(shared_root.clone(), true)
                .await
                .unwrap(),
            BlockAcceptance::Accepted
        );
        assert!(
            runner
                .import_proposal_vote(signed_proposal_vote(
                    runner.crypto.as_ref(),
                    2,
                    &shared_root
                ))
                .unwrap()
        );
        assert!(
            runner
                .import_proposal_vote(signed_proposal_vote(
                    runner.crypto.as_ref(),
                    3,
                    &shared_root
                ))
                .unwrap()
        );

        let mut root_ledger = LedgerState::from_genesis(&genesis);
        root_ledger.apply_block(&shared_root, &crypto).unwrap();
        let slot = ((shared_root.header.slot + 1)..(shared_root.header.slot + 50))
            .find(|slot| consensus.proposer_for_slot(*slot) == 2)
            .unwrap();
        let epoch = consensus.epoch_for_slot(slot);
        let transactions = Vec::new();
        let commitment: Option<TopologyCommitment> = None;
        let header = BlockHeader {
            block_number: 2,
            parent_hash: shared_root.block_hash,
            slot,
            epoch,
            proposer_id: 2,
            timestamp_unix_millis: now_unix_millis(),
            state_root: root_ledger.state_root(),
            transactions_root: canonical_hash(&transactions),
            topology_root: empty_hash(),
        };
        let block_hash = canonical_hash(&(header.clone(), &transactions, &commitment));
        let pending_child = Block {
            header,
            transactions,
            commitment,
            commitment_receipts: Vec::new(),
            signature: crypto.sign(2, &block_hash).unwrap(),
            block_hash,
        };
        assert_eq!(
            runner
                .accept_block(pending_child.clone(), false)
                .await
                .unwrap(),
            BlockAcceptance::Orphan
        );
        runner.record_peer_sync_status(
            2,
            pending_child.header.block_number + 1,
            canonical_hash(&"peer-ahead-tip"),
            Some(shared_root.block_hash),
            shared_root.header.block_number,
            vec![SyncQcAnchor {
                block_hash: shared_root.block_hash,
                block_number: shared_root.header.block_number,
            }],
        );

        while timeout(Duration::from_millis(25), peer_events.recv())
            .await
            .ok()
            .flatten()
            .is_some()
        {}

        assert!(
            runner
                .maybe_repair_stale_pending_certified_child(pending_child.header.slot + 1)
                .unwrap()
        );

        loop {
            match recv_protocol_message(&mut peer_events).await {
                ProtocolMessage::SyncRequest {
                    requester_id,
                    known_height,
                    known_tip_hash,
                } => {
                    assert_eq!(requester_id, 1);
                    assert_eq!(known_height, shared_root.header.block_number);
                    assert_eq!(known_tip_hash, shared_root.block_hash);
                    break;
                }
                ProtocolMessage::ProposalVote(_)
                | ProtocolMessage::QuorumCertificate(_)
                | ProtocolMessage::SyncStatus { .. }
                | ProtocolMessage::SyncBlocks { .. }
                | ProtocolMessage::SyncResponse { .. }
                | ProtocolMessage::CertifiedSyncResponse(_)
                | ProtocolMessage::CertifiedSyncRequest(_)
                | ProtocolMessage::RelayReceipt(_)
                | ProtocolMessage::HeartbeatPulse(_)
                | ProtocolMessage::ReceiptFetch { .. }
                | ProtocolMessage::ReceiptResponse { .. } => {}
                other => panic!("expected legacy suffix sync request, got {other:?}"),
            }
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn stale_pending_certified_child_uses_legacy_sync_from_proposer_when_status_is_stale() {
        let genesis = sample_genesis();
        let local_address = reserve_local_address();
        let peer_address = reserve_local_address();
        let peer = PeerConfig {
            validator_id: 2,
            address: peer_address.clone(),
        };
        let config = sample_node_config(
            1,
            local_address,
            vec![peer],
            "stale-pending-certified-stale-status",
        );
        let mut runner = build_test_runner(1, config, genesis.clone()).await;
        let (_, _, _, mut peer_events) =
            spawn_test_network(2, peer_address, Vec::new(), &genesis).await;
        let shared_root = first_valid_block(&genesis);
        let crypto = DeterministicCryptoBackend::from_genesis(&genesis);
        let consensus = ConsensusEngine::new(genesis.clone());

        assert_eq!(
            runner
                .accept_block(shared_root.clone(), true)
                .await
                .unwrap(),
            BlockAcceptance::Accepted
        );
        assert!(
            runner
                .import_proposal_vote(signed_proposal_vote(
                    runner.crypto.as_ref(),
                    2,
                    &shared_root
                ))
                .unwrap()
        );
        assert!(
            runner
                .import_proposal_vote(signed_proposal_vote(
                    runner.crypto.as_ref(),
                    3,
                    &shared_root
                ))
                .unwrap()
        );

        let mut root_ledger = LedgerState::from_genesis(&genesis);
        root_ledger.apply_block(&shared_root, &crypto).unwrap();
        let slot = ((shared_root.header.slot + 1)..(shared_root.header.slot + 50))
            .find(|slot| consensus.proposer_for_slot(*slot) == 2)
            .unwrap();
        let epoch = consensus.epoch_for_slot(slot);
        let transactions = Vec::new();
        let commitment: Option<TopologyCommitment> = None;
        let header = BlockHeader {
            block_number: 2,
            parent_hash: shared_root.block_hash,
            slot,
            epoch,
            proposer_id: 2,
            timestamp_unix_millis: now_unix_millis(),
            state_root: root_ledger.state_root(),
            transactions_root: canonical_hash(&transactions),
            topology_root: empty_hash(),
        };
        let block_hash = canonical_hash(&(header.clone(), &transactions, &commitment));
        let pending_child = Block {
            header,
            transactions,
            commitment,
            commitment_receipts: Vec::new(),
            signature: crypto.sign(2, &block_hash).unwrap(),
            block_hash,
        };
        assert_eq!(
            runner
                .accept_block(pending_child.clone(), false)
                .await
                .unwrap(),
            BlockAcceptance::Orphan
        );

        runner.record_peer_sync_status(
            2,
            shared_root.header.block_number,
            shared_root.block_hash,
            Some(shared_root.block_hash),
            shared_root.header.block_number,
            vec![SyncQcAnchor {
                block_hash: shared_root.block_hash,
                block_number: shared_root.header.block_number,
            }],
        );

        while timeout(Duration::from_millis(25), peer_events.recv())
            .await
            .ok()
            .flatten()
            .is_some()
        {}

        assert!(
            runner
                .maybe_repair_stale_pending_certified_child(pending_child.header.slot + 1)
                .unwrap()
        );

        loop {
            match recv_protocol_message(&mut peer_events).await {
                ProtocolMessage::SyncRequest {
                    requester_id,
                    known_height,
                    known_tip_hash,
                } => {
                    assert_eq!(requester_id, 1);
                    assert_eq!(known_height, shared_root.header.block_number);
                    assert_eq!(known_tip_hash, shared_root.block_hash);
                    break;
                }
                ProtocolMessage::ProposalVote(_)
                | ProtocolMessage::QuorumCertificate(_)
                | ProtocolMessage::SyncStatus { .. }
                | ProtocolMessage::SyncBlocks { .. }
                | ProtocolMessage::SyncResponse { .. }
                | ProtocolMessage::CertifiedSyncResponse(_)
                | ProtocolMessage::CertifiedSyncRequest(_)
                | ProtocolMessage::RelayReceipt(_)
                | ProtocolMessage::HeartbeatPulse(_)
                | ProtocolMessage::ReceiptFetch { .. }
                | ProtocolMessage::ReceiptResponse { .. } => {}
                other => {
                    panic!("expected legacy sync request from stale pending repair, got {other:?}")
                }
            }
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn certified_sync_unavailable_prefers_same_ahead_peer_followup() {
        let genesis = sample_genesis();
        let local_address = reserve_local_address();
        let peer2_address = reserve_local_address();
        let config = sample_node_config(
            1,
            local_address,
            vec![PeerConfig {
                validator_id: 2,
                address: peer2_address.clone(),
            }],
            "certified-sync-unavailable-alternate",
        );
        let mut runner = build_test_runner(1, config, genesis.clone()).await;
        let (_, _, _, mut peer2_events) =
            spawn_test_network(2, peer2_address, Vec::new(), &genesis).await;
        let shared_root = first_valid_block(&genesis);

        assert_eq!(
            runner
                .accept_block(shared_root.clone(), true)
                .await
                .unwrap(),
            BlockAcceptance::Accepted
        );
        assert!(
            runner
                .import_proposal_vote(signed_proposal_vote(
                    runner.crypto.as_ref(),
                    2,
                    &shared_root
                ))
                .unwrap()
        );
        assert!(
            runner
                .import_proposal_vote(signed_proposal_vote(
                    runner.crypto.as_ref(),
                    3,
                    &shared_root
                ))
                .unwrap()
        );

        runner.record_peer_sync_status(
            2,
            5,
            canonical_hash(&"peer-2-tip"),
            Some(shared_root.block_hash),
            shared_root.header.block_number,
            vec![SyncQcAnchor {
                block_hash: shared_root.block_hash,
                block_number: shared_root.header.block_number,
            }],
        );
        runner
            .handle_network_event(NetworkEvent::Received {
                from_validator_id: 2,
                payload: ProtocolMessage::CertifiedSyncResponse(ChunkedSyncResponse::Unavailable {
                    responder_id: 2,
                }),
                bytes: 0,
            })
            .await
            .unwrap();

        async fn drain_best_sync_requests(
            events: &mut mpsc::UnboundedReceiver<NetworkEvent>,
        ) -> usize {
            let deadline = tokio::time::Instant::now() + Duration::from_millis(250);
            let mut count = 0;
            loop {
                let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
                if remaining.is_zero() {
                    break;
                }
                let Ok(message) =
                    tokio::time::timeout(remaining, recv_protocol_message(events)).await
                else {
                    break;
                };
                if matches!(
                    message,
                    ProtocolMessage::CertifiedSyncRequest(_) | ProtocolMessage::SyncRequest { .. }
                ) {
                    count += 1;
                }
            }
            count
        }

        let requests_on_2 = drain_best_sync_requests(&mut peer2_events).await;
        assert_eq!(requests_on_2, 1);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn startup_sync_barrier_advertises_certified_frontier_not_uncertified_tip() {
        let genesis = sample_genesis();
        let local_address = reserve_local_address();
        let peer_address = reserve_local_address();
        let peer = PeerConfig {
            validator_id: 2,
            address: peer_address.clone(),
        };
        let config = sample_node_config(1, local_address, vec![peer], "startup-sync-status");
        let mut runner = build_test_runner(1, config, genesis.clone()).await;
        let (_, _, _, mut peer_events) =
            spawn_test_network(2, peer_address, Vec::new(), &genesis).await;
        let shared_root = first_valid_block(&genesis);

        assert_eq!(
            runner
                .accept_block(shared_root.clone(), true)
                .await
                .unwrap(),
            BlockAcceptance::Accepted
        );
        assert!(
            runner
                .import_proposal_vote(signed_proposal_vote(
                    runner.crypto.as_ref(),
                    2,
                    &shared_root
                ))
                .unwrap()
        );
        assert!(
            runner
                .import_proposal_vote(signed_proposal_vote(
                    runner.crypto.as_ref(),
                    3,
                    &shared_root
                ))
                .unwrap()
        );

        let crypto = DeterministicCryptoBackend::from_genesis(&genesis);
        let consensus = ConsensusEngine::new(genesis.clone());
        let mut root_ledger = LedgerState::from_genesis(&genesis);
        root_ledger.apply_block(&shared_root, &crypto).unwrap();
        let child_slot = ((shared_root.header.slot + 1)..(shared_root.header.slot + 50))
            .find(|slot| consensus.proposer_for_slot(*slot) == 2)
            .unwrap();
        let child_epoch = consensus.epoch_for_slot(child_slot);
        let child_transactions = Vec::new();
        let child_commitment: Option<TopologyCommitment> = None;
        let child_header = BlockHeader {
            block_number: 2,
            parent_hash: shared_root.block_hash,
            slot: child_slot,
            epoch: child_epoch,
            proposer_id: 2,
            timestamp_unix_millis: now_unix_millis(),
            state_root: root_ledger.state_root(),
            transactions_root: canonical_hash(&child_transactions),
            topology_root: empty_hash(),
        };
        let child_hash =
            canonical_hash(&(child_header.clone(), &child_transactions, &child_commitment));
        let uncertified_child = Block {
            header: child_header,
            transactions: child_transactions,
            commitment: child_commitment,
            commitment_receipts: Vec::new(),
            signature: crypto.sign(2, &child_hash).unwrap(),
            block_hash: child_hash,
        };
        runner.blocks.push(uncertified_child.clone());
        runner
            .ledger
            .apply_block(&uncertified_child, runner.crypto.as_ref())
            .unwrap();
        runner.startup_sync_barrier = true;

        runner.broadcast_sync_status().unwrap();

        loop {
            match recv_protocol_message(&mut peer_events).await {
                ProtocolMessage::SyncStatus {
                    height,
                    tip_hash,
                    highest_qc_hash,
                    highest_qc_height,
                    ..
                } => {
                    assert_eq!(height, shared_root.header.block_number);
                    assert_eq!(tip_hash, shared_root.block_hash);
                    assert_eq!(highest_qc_hash, Some(shared_root.block_hash));
                    assert_eq!(highest_qc_height, shared_root.header.block_number);
                    break;
                }
                ProtocolMessage::ProposalVote(_)
                | ProtocolMessage::QuorumCertificate(_)
                | ProtocolMessage::HeartbeatPulse(_)
                | ProtocolMessage::ReceiptFetch { .. } => {}
                other => panic!("expected sync status, got {other:?}"),
            }
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn startup_sync_barrier_broadcast_sync_request_uses_certified_frontier() {
        let genesis = sample_genesis();
        let local_address = reserve_local_address();
        let peer_address = reserve_local_address();
        let peer = PeerConfig {
            validator_id: 2,
            address: peer_address.clone(),
        };
        let config = sample_node_config(1, local_address, vec![peer], "startup-sync-request");
        let mut runner = build_test_runner(1, config, genesis.clone()).await;
        let (_, _, _, mut peer_events) =
            spawn_test_network(2, peer_address, Vec::new(), &genesis).await;
        let shared_root = first_valid_block(&genesis);

        assert_eq!(
            runner
                .accept_block(shared_root.clone(), true)
                .await
                .unwrap(),
            BlockAcceptance::Accepted
        );
        assert!(
            runner
                .import_proposal_vote(signed_proposal_vote(
                    runner.crypto.as_ref(),
                    2,
                    &shared_root
                ))
                .unwrap()
        );
        assert!(
            runner
                .import_proposal_vote(signed_proposal_vote(
                    runner.crypto.as_ref(),
                    3,
                    &shared_root
                ))
                .unwrap()
        );

        let crypto = DeterministicCryptoBackend::from_genesis(&genesis);
        let consensus = ConsensusEngine::new(genesis.clone());
        let mut root_ledger = LedgerState::from_genesis(&genesis);
        root_ledger.apply_block(&shared_root, &crypto).unwrap();
        let child_slot = ((shared_root.header.slot + 1)..(shared_root.header.slot + 50))
            .find(|slot| consensus.proposer_for_slot(*slot) == 2)
            .unwrap();
        let child_epoch = consensus.epoch_for_slot(child_slot);
        let child_transactions = Vec::new();
        let child_commitment: Option<TopologyCommitment> = None;
        let child_header = BlockHeader {
            block_number: 2,
            parent_hash: shared_root.block_hash,
            slot: child_slot,
            epoch: child_epoch,
            proposer_id: 2,
            timestamp_unix_millis: now_unix_millis(),
            state_root: root_ledger.state_root(),
            transactions_root: canonical_hash(&child_transactions),
            topology_root: empty_hash(),
        };
        let child_hash =
            canonical_hash(&(child_header.clone(), &child_transactions, &child_commitment));
        let uncertified_child = Block {
            header: child_header,
            transactions: child_transactions,
            commitment: child_commitment,
            commitment_receipts: Vec::new(),
            signature: crypto.sign(2, &child_hash).unwrap(),
            block_hash: child_hash,
        };
        runner.blocks.push(uncertified_child.clone());
        runner
            .ledger
            .apply_block(&uncertified_child, runner.crypto.as_ref())
            .unwrap();
        runner.startup_sync_barrier = true;

        runner.broadcast_sync_request().unwrap();

        loop {
            match recv_protocol_message(&mut peer_events).await {
                ProtocolMessage::SyncRequest {
                    known_height,
                    known_tip_hash,
                    ..
                } => {
                    assert_eq!(known_height, shared_root.header.block_number);
                    assert_eq!(known_tip_hash, shared_root.block_hash);
                    break;
                }
                ProtocolMessage::SyncStatus { .. }
                | ProtocolMessage::SyncBlocks { .. }
                | ProtocolMessage::SyncResponse { .. }
                | ProtocolMessage::CertifiedSyncResponse(_)
                | ProtocolMessage::ProposalVote(_)
                | ProtocolMessage::QuorumCertificate(_)
                | ProtocolMessage::HeartbeatPulse(_)
                | ProtocolMessage::ReceiptFetch { .. } => {}
                other => panic!("expected sync request, got {other:?}"),
            }
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn startup_sync_barrier_does_not_push_uncertified_local_suffix() {
        let genesis = sample_genesis();
        let local_address = reserve_local_address();
        let peer_address = reserve_local_address();
        let peer = PeerConfig {
            validator_id: 2,
            address: peer_address.clone(),
        };
        let config = sample_node_config(1, local_address, vec![peer], "startup-sync-push");
        let mut runner = build_test_runner(1, config, genesis.clone()).await;
        let (_, _, _, mut peer_events) =
            spawn_test_network(2, peer_address, Vec::new(), &genesis).await;
        let shared_root = first_valid_block(&genesis);

        assert_eq!(
            runner
                .accept_block(shared_root.clone(), true)
                .await
                .unwrap(),
            BlockAcceptance::Accepted
        );
        assert!(
            runner
                .import_proposal_vote(signed_proposal_vote(
                    runner.crypto.as_ref(),
                    2,
                    &shared_root
                ))
                .unwrap()
        );
        assert!(
            runner
                .import_proposal_vote(signed_proposal_vote(
                    runner.crypto.as_ref(),
                    3,
                    &shared_root
                ))
                .unwrap()
        );

        let crypto = DeterministicCryptoBackend::from_genesis(&genesis);
        let consensus = ConsensusEngine::new(genesis.clone());
        let mut root_ledger = LedgerState::from_genesis(&genesis);
        root_ledger.apply_block(&shared_root, &crypto).unwrap();
        let child_slot = ((shared_root.header.slot + 1)..(shared_root.header.slot + 50))
            .find(|slot| consensus.proposer_for_slot(*slot) == 2)
            .unwrap();
        let child_epoch = consensus.epoch_for_slot(child_slot);
        let child_transactions = Vec::new();
        let child_commitment: Option<TopologyCommitment> = None;
        let child_header = BlockHeader {
            block_number: 2,
            parent_hash: shared_root.block_hash,
            slot: child_slot,
            epoch: child_epoch,
            proposer_id: 2,
            timestamp_unix_millis: now_unix_millis(),
            state_root: root_ledger.state_root(),
            transactions_root: canonical_hash(&child_transactions),
            topology_root: empty_hash(),
        };
        let child_hash =
            canonical_hash(&(child_header.clone(), &child_transactions, &child_commitment));
        let uncertified_child = Block {
            header: child_header,
            transactions: child_transactions,
            commitment: child_commitment,
            commitment_receipts: Vec::new(),
            signature: crypto.sign(2, &child_hash).unwrap(),
            block_hash: child_hash,
        };
        runner.blocks.push(uncertified_child.clone());
        runner
            .ledger
            .apply_block(&uncertified_child, runner.crypto.as_ref())
            .unwrap();
        runner.startup_sync_barrier = true;
        runner.record_peer_sync_status(
            2,
            shared_root.header.block_number,
            shared_root.block_hash,
            Some(shared_root.block_hash),
            shared_root.header.block_number,
            vec![SyncQcAnchor {
                block_hash: shared_root.block_hash,
                block_number: shared_root.header.block_number,
            }],
        );

        let drain_deadline = tokio::time::Instant::now() + Duration::from_millis(100);
        loop {
            let remaining = drain_deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                break;
            }
            let Ok(_) =
                tokio::time::timeout(remaining, recv_protocol_message(&mut peer_events)).await
            else {
                break;
            };
        }

        runner.push_sync_to_stale_peers().unwrap();

        let deadline = tokio::time::Instant::now() + Duration::from_millis(250);
        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                break;
            }
            let Ok(message) =
                tokio::time::timeout(remaining, recv_protocol_message(&mut peer_events)).await
            else {
                break;
            };
            match message {
                ProtocolMessage::SyncBlocks { .. }
                | ProtocolMessage::SyncResponse { .. }
                | ProtocolMessage::CertifiedSyncResponse(_) => {
                    panic!("unexpected sync push while startup barrier is active: {message:?}");
                }
                ProtocolMessage::SyncStatus { .. }
                | ProtocolMessage::ProposalVote(_)
                | ProtocolMessage::QuorumCertificate(_)
                | ProtocolMessage::HeartbeatPulse(_)
                | ProtocolMessage::ReceiptFetch { .. } => {}
                other => {
                    panic!("unexpected message while checking startup sync barrier: {other:?}")
                }
            }
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn startup_sync_barrier_defers_pending_child_votes_while_peer_is_ahead() {
        let genesis = sample_genesis();
        let local_address = reserve_local_address();
        let peer_address = reserve_local_address();
        let peer = PeerConfig {
            validator_id: 2,
            address: peer_address.clone(),
        };
        let config = sample_node_config(1, local_address, vec![peer], "startup-sync-vote-defer");
        let mut runner = build_test_runner(1, config, genesis.clone()).await;
        let (_, _, _, mut peer_events) =
            spawn_test_network(2, peer_address, Vec::new(), &genesis).await;
        let shared_root = first_valid_block(&genesis);

        assert_eq!(
            runner
                .accept_block(shared_root.clone(), true)
                .await
                .unwrap(),
            BlockAcceptance::Accepted
        );
        assert!(
            runner
                .import_proposal_vote(signed_proposal_vote(
                    runner.crypto.as_ref(),
                    2,
                    &shared_root
                ))
                .unwrap()
        );
        assert!(
            runner
                .import_proposal_vote(signed_proposal_vote(
                    runner.crypto.as_ref(),
                    3,
                    &shared_root
                ))
                .unwrap()
        );

        let crypto = DeterministicCryptoBackend::from_genesis(&genesis);
        let consensus = ConsensusEngine::new(genesis.clone());
        let mut root_ledger = LedgerState::from_genesis(&genesis);
        root_ledger.apply_block(&shared_root, &crypto).unwrap();
        let child_slot = ((shared_root.header.slot + 1)..(shared_root.header.slot + 50))
            .find(|slot| consensus.proposer_for_slot(*slot) == 2)
            .unwrap();
        let child_epoch = consensus.epoch_for_slot(child_slot);
        let child_transactions = Vec::new();
        let child_commitment: Option<TopologyCommitment> = None;
        let child_header = BlockHeader {
            block_number: 2,
            parent_hash: shared_root.block_hash,
            slot: child_slot,
            epoch: child_epoch,
            proposer_id: 2,
            timestamp_unix_millis: now_unix_millis(),
            state_root: root_ledger.state_root(),
            transactions_root: canonical_hash(&child_transactions),
            topology_root: empty_hash(),
        };
        let child_hash =
            canonical_hash(&(child_header.clone(), &child_transactions, &child_commitment));
        let child = Block {
            header: child_header,
            transactions: child_transactions,
            commitment: child_commitment,
            commitment_receipts: Vec::new(),
            signature: crypto.sign(2, &child_hash).unwrap(),
            block_hash: child_hash,
        };

        runner.startup_sync_barrier = true;
        runner.record_peer_sync_status(
            2,
            3,
            canonical_hash(&"peer-ahead-tip"),
            Some(shared_root.block_hash),
            shared_root.header.block_number,
            vec![SyncQcAnchor {
                block_hash: shared_root.block_hash,
                block_number: shared_root.header.block_number,
            }],
        );

        let drain_deadline = tokio::time::Instant::now() + Duration::from_millis(100);
        loop {
            let remaining = drain_deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                break;
            }
            let Ok(_) =
                tokio::time::timeout(remaining, recv_protocol_message(&mut peer_events)).await
            else {
                break;
            };
        }

        assert_eq!(
            runner.accept_block(child.clone(), false).await.unwrap(),
            BlockAcceptance::Orphan
        );
        assert!(
            runner
                .pending_certified_children
                .iter()
                .any(|pending| pending.block_hash == child.block_hash)
        );
        assert!(
            timeout(Duration::from_millis(25), peer_events.recv())
                .await
                .is_err(),
            "expected recovery node not to broadcast a proposal vote for pending child"
        );

        assert!(
            !runner
                .import_proposal_vote(signed_proposal_vote(runner.crypto.as_ref(), 3, &child))
                .unwrap()
        );
        assert!(
            !runner
                .import_proposal_vote(signed_proposal_vote(runner.crypto.as_ref(), 4, &child))
                .unwrap()
        );
        assert!(
            !runner.quorum_certificates.contains_key(&child.block_hash),
            "expected recovery node to defer QC assembly while a peer is still ahead"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn certified_sync_push_also_sends_anchored_suffix_tail() {
        let genesis = sample_genesis();
        let local_address = reserve_local_address();
        let peer_address = reserve_local_address();
        let peer = PeerConfig {
            validator_id: 2,
            address: peer_address.clone(),
        };
        let config = sample_node_config(1, local_address, vec![peer], "certified-push-tail");
        let mut runner = build_test_runner(1, config, genesis.clone()).await;
        let (_, _, _, mut peer_events) =
            spawn_test_network(2, peer_address, Vec::new(), &genesis).await;
        let shared_root = first_valid_block(&genesis);

        assert_eq!(
            runner
                .accept_block(shared_root.clone(), true)
                .await
                .unwrap(),
            BlockAcceptance::Accepted
        );
        assert!(
            runner
                .import_proposal_vote(signed_proposal_vote(
                    runner.crypto.as_ref(),
                    2,
                    &shared_root
                ))
                .unwrap()
        );
        assert!(
            runner
                .import_proposal_vote(signed_proposal_vote(
                    runner.crypto.as_ref(),
                    3,
                    &shared_root
                ))
                .unwrap()
        );

        let crypto = DeterministicCryptoBackend::from_genesis(&genesis);
        let consensus = ConsensusEngine::new(genesis.clone());
        let mut root_ledger = LedgerState::from_genesis(&genesis);
        root_ledger.apply_block(&shared_root, &crypto).unwrap();
        let child_slot = ((shared_root.header.slot + 1)..(shared_root.header.slot + 50))
            .find(|slot| consensus.proposer_for_slot(*slot) == 2)
            .unwrap();
        let child_epoch = consensus.epoch_for_slot(child_slot);
        let child_transactions = Vec::new();
        let child_commitment: Option<TopologyCommitment> = None;
        let child_header = BlockHeader {
            block_number: 2,
            parent_hash: shared_root.block_hash,
            slot: child_slot,
            epoch: child_epoch,
            proposer_id: 2,
            timestamp_unix_millis: now_unix_millis(),
            state_root: root_ledger.state_root(),
            transactions_root: canonical_hash(&child_transactions),
            topology_root: empty_hash(),
        };
        let child_hash =
            canonical_hash(&(child_header.clone(), &child_transactions, &child_commitment));
        let child = Block {
            header: child_header,
            transactions: child_transactions,
            commitment: child_commitment,
            commitment_receipts: Vec::new(),
            signature: crypto.sign(2, &child_hash).unwrap(),
            block_hash: child_hash,
        };
        assert_eq!(
            runner.accept_block(child.clone(), false).await.unwrap(),
            BlockAcceptance::Orphan
        );
        assert!(
            runner
                .import_proposal_vote(signed_proposal_vote(runner.crypto.as_ref(), 3, &child))
                .unwrap()
        );
        assert!(
            runner
                .import_proposal_vote(signed_proposal_vote(runner.crypto.as_ref(), 4, &child))
                .unwrap()
        );
        assert!(runner.quorum_certificates.contains_key(&child.block_hash));

        runner.record_peer_sync_status(
            2,
            shared_root.header.block_number,
            shared_root.block_hash,
            Some(shared_root.block_hash),
            shared_root.header.block_number,
            vec![SyncQcAnchor {
                block_hash: shared_root.block_hash,
                block_number: shared_root.header.block_number,
            }],
        );

        let drain_deadline = tokio::time::Instant::now() + Duration::from_millis(100);
        loop {
            let remaining = drain_deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                break;
            }
            let Ok(_) =
                tokio::time::timeout(remaining, recv_protocol_message(&mut peer_events)).await
            else {
                break;
            };
        }

        runner
            .push_best_sync_to(2, shared_root.header.block_number, shared_root.block_hash)
            .unwrap();

        let deadline = tokio::time::Instant::now() + Duration::from_millis(500);
        let mut saw_certified = false;
        let mut saw_sync_blocks = false;
        while !(saw_certified && saw_sync_blocks) {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            assert!(
                !remaining.is_zero(),
                "expected both certified sync and anchored suffix sync"
            );
            let message = tokio::time::timeout(remaining, recv_protocol_message(&mut peer_events))
                .await
                .unwrap();
            match message {
                ProtocolMessage::CertifiedSyncResponse(ChunkedSyncResponse::Certified {
                    shared_qc_hash,
                    shared_qc_height,
                    responder_height,
                    ..
                }) => {
                    assert_eq!(shared_qc_hash, shared_root.block_hash);
                    assert_eq!(shared_qc_height, shared_root.header.block_number);
                    assert_eq!(responder_height, child.header.block_number);
                    saw_certified = true;
                }
                ProtocolMessage::SyncBlocks {
                    responder_id,
                    chain,
                } => {
                    assert_eq!(responder_id, 1);
                    assert_eq!(chain.base_height, shared_root.header.block_number);
                    assert_eq!(chain.base_tip_hash, shared_root.block_hash);
                    assert_eq!(chain.target_snapshot.height, child.header.block_number);
                    assert_eq!(chain.blocks.len(), 1);
                    assert_eq!(chain.blocks[0].block_hash, child.block_hash);
                    saw_sync_blocks = true;
                }
                ProtocolMessage::SyncStatus { .. }
                | ProtocolMessage::ProposalVote(_)
                | ProtocolMessage::QuorumCertificate(_)
                | ProtocolMessage::HeartbeatPulse(_)
                | ProtocolMessage::ReceiptFetch { .. } => {}
                other => panic!("unexpected message while checking certified push tail: {other:?}"),
            }
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn certified_sync_request_also_sends_anchored_suffix_tail() {
        let genesis = sample_genesis();
        let local_address = reserve_local_address();
        let peer_address = reserve_local_address();
        let peer = PeerConfig {
            validator_id: 2,
            address: peer_address.clone(),
        };
        let config = sample_node_config(1, local_address, vec![peer], "certified-request-tail");
        let mut runner = build_test_runner(1, config, genesis.clone()).await;
        let (_, _, _, mut peer_events) =
            spawn_test_network(2, peer_address, Vec::new(), &genesis).await;
        let shared_root = first_valid_block(&genesis);

        assert_eq!(
            runner
                .accept_block(shared_root.clone(), true)
                .await
                .unwrap(),
            BlockAcceptance::Accepted
        );
        assert!(
            runner
                .import_proposal_vote(signed_proposal_vote(
                    runner.crypto.as_ref(),
                    2,
                    &shared_root
                ))
                .unwrap()
        );
        assert!(
            runner
                .import_proposal_vote(signed_proposal_vote(
                    runner.crypto.as_ref(),
                    3,
                    &shared_root
                ))
                .unwrap()
        );

        let crypto = DeterministicCryptoBackend::from_genesis(&genesis);
        let consensus = ConsensusEngine::new(genesis.clone());
        let mut root_ledger = LedgerState::from_genesis(&genesis);
        root_ledger.apply_block(&shared_root, &crypto).unwrap();
        let child_slot = ((shared_root.header.slot + 1)..(shared_root.header.slot + 50))
            .find(|slot| consensus.proposer_for_slot(*slot) == 2)
            .unwrap();
        let child_epoch = consensus.epoch_for_slot(child_slot);
        let child_transactions = Vec::new();
        let child_commitment: Option<TopologyCommitment> = None;
        let child_header = BlockHeader {
            block_number: 2,
            parent_hash: shared_root.block_hash,
            slot: child_slot,
            epoch: child_epoch,
            proposer_id: 2,
            timestamp_unix_millis: now_unix_millis(),
            state_root: root_ledger.state_root(),
            transactions_root: canonical_hash(&child_transactions),
            topology_root: empty_hash(),
        };
        let child_hash =
            canonical_hash(&(child_header.clone(), &child_transactions, &child_commitment));
        let child = Block {
            header: child_header,
            transactions: child_transactions,
            commitment: child_commitment,
            commitment_receipts: Vec::new(),
            signature: crypto.sign(2, &child_hash).unwrap(),
            block_hash: child_hash,
        };
        assert_eq!(
            runner.accept_block(child.clone(), false).await.unwrap(),
            BlockAcceptance::Orphan
        );
        assert!(
            runner
                .import_proposal_vote(signed_proposal_vote(runner.crypto.as_ref(), 3, &child))
                .unwrap()
        );
        assert!(
            runner
                .import_proposal_vote(signed_proposal_vote(runner.crypto.as_ref(), 4, &child))
                .unwrap()
        );
        assert!(runner.quorum_certificates.contains_key(&child.block_hash));

        let drain_deadline = tokio::time::Instant::now() + Duration::from_millis(100);
        loop {
            let remaining = drain_deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                break;
            }
            let Ok(_) =
                tokio::time::timeout(remaining, recv_protocol_message(&mut peer_events)).await
            else {
                break;
            };
        }

        runner
            .handle_network_event(NetworkEvent::Received {
                from_validator_id: 2,
                payload: ProtocolMessage::CertifiedSyncRequest(ChunkedSyncRequest {
                    requester_id: 2,
                    known_qc_hash: Some(shared_root.block_hash),
                    known_qc_height: shared_root.header.block_number,
                    known_qc_anchors: vec![SyncQcAnchor {
                        block_hash: shared_root.block_hash,
                        block_number: shared_root.header.block_number,
                    }],
                    from_height: shared_root.header.block_number,
                    want_certified_only: true,
                }),
                bytes: 0,
            })
            .await
            .unwrap();

        let deadline = tokio::time::Instant::now() + Duration::from_millis(500);
        let mut saw_certified = false;
        let mut saw_sync_blocks = false;
        while !(saw_certified && saw_sync_blocks) {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            assert!(
                !remaining.is_zero(),
                "expected both certified sync and anchored suffix sync"
            );
            let message = tokio::time::timeout(remaining, recv_protocol_message(&mut peer_events))
                .await
                .unwrap();
            match message {
                ProtocolMessage::CertifiedSyncResponse(ChunkedSyncResponse::Certified {
                    shared_qc_hash,
                    shared_qc_height,
                    responder_height,
                    ..
                }) => {
                    assert_eq!(shared_qc_hash, shared_root.block_hash);
                    assert_eq!(shared_qc_height, shared_root.header.block_number);
                    assert_eq!(responder_height, child.header.block_number);
                    saw_certified = true;
                }
                ProtocolMessage::SyncBlocks {
                    responder_id,
                    chain,
                } => {
                    assert_eq!(responder_id, 1);
                    assert_eq!(chain.base_height, shared_root.header.block_number);
                    assert_eq!(chain.base_tip_hash, shared_root.block_hash);
                    assert_eq!(chain.target_snapshot.height, child.header.block_number);
                    assert_eq!(chain.blocks.len(), 1);
                    assert_eq!(chain.blocks[0].block_hash, child.block_hash);
                    saw_sync_blocks = true;
                }
                ProtocolMessage::SyncStatus { .. }
                | ProtocolMessage::ProposalVote(_)
                | ProtocolMessage::QuorumCertificate(_)
                | ProtocolMessage::HeartbeatPulse(_)
                | ProtocolMessage::ReceiptFetch { .. } => {}
                other => {
                    panic!("unexpected message while checking certified request tail: {other:?}")
                }
            }
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn explicit_legacy_sync_request_is_not_upgraded_to_certified_response() {
        let genesis = sample_genesis();
        let local_address = reserve_local_address();
        let peer_address = reserve_local_address();
        let peer = PeerConfig {
            validator_id: 2,
            address: peer_address.clone(),
        };
        let config = sample_node_config(
            1,
            local_address,
            vec![peer],
            "explicit-legacy-sync-response",
        );
        let mut runner = build_test_runner(1, config, genesis.clone()).await;
        let (_, _, _, mut peer_events) =
            spawn_test_network(2, peer_address, Vec::new(), &genesis).await;
        let shared_root = first_valid_block(&genesis);

        assert_eq!(
            runner
                .accept_block(shared_root.clone(), true)
                .await
                .unwrap(),
            BlockAcceptance::Accepted
        );
        assert!(
            runner
                .import_proposal_vote(signed_proposal_vote(
                    runner.crypto.as_ref(),
                    2,
                    &shared_root
                ))
                .unwrap()
        );
        assert!(
            runner
                .import_proposal_vote(signed_proposal_vote(
                    runner.crypto.as_ref(),
                    3,
                    &shared_root
                ))
                .unwrap()
        );

        let crypto = DeterministicCryptoBackend::from_genesis(&genesis);
        let consensus = ConsensusEngine::new(genesis.clone());
        let mut root_ledger = LedgerState::from_genesis(&genesis);
        root_ledger.apply_block(&shared_root, &crypto).unwrap();
        let slot = ((shared_root.header.slot + 1)..(shared_root.header.slot + 50))
            .find(|slot| consensus.proposer_for_slot(*slot) == 2)
            .unwrap();
        let epoch = consensus.epoch_for_slot(slot);
        let transactions = Vec::new();
        let commitment: Option<TopologyCommitment> = None;
        let header = BlockHeader {
            block_number: 2,
            parent_hash: shared_root.block_hash,
            slot,
            epoch,
            proposer_id: 2,
            timestamp_unix_millis: now_unix_millis(),
            state_root: root_ledger.state_root(),
            transactions_root: canonical_hash(&transactions),
            topology_root: empty_hash(),
        };
        let block_hash = canonical_hash(&(header.clone(), &transactions, &commitment));
        let child = Block {
            header,
            transactions,
            commitment,
            commitment_receipts: Vec::new(),
            signature: crypto.sign(2, &block_hash).unwrap(),
            block_hash,
        };
        assert_eq!(
            runner.accept_block(child.clone(), false).await.unwrap(),
            BlockAcceptance::Orphan
        );
        assert!(
            runner
                .import_proposal_vote(signed_proposal_vote(runner.crypto.as_ref(), 3, &child))
                .unwrap()
        );
        assert!(
            runner
                .import_proposal_vote(signed_proposal_vote(runner.crypto.as_ref(), 4, &child))
                .unwrap()
        );

        runner
            .handle_network_event(NetworkEvent::Received {
                from_validator_id: 2,
                payload: ProtocolMessage::SyncRequest {
                    requester_id: 2,
                    known_height: shared_root.header.block_number,
                    known_tip_hash: shared_root.block_hash,
                },
                bytes: 0,
            })
            .await
            .unwrap();

        loop {
            match recv_protocol_message(&mut peer_events).await {
                ProtocolMessage::SyncBlocks { .. } | ProtocolMessage::SyncResponse { .. } => break,
                ProtocolMessage::ProposalVote(_)
                | ProtocolMessage::QuorumCertificate(_)
                | ProtocolMessage::SyncStatus { .. }
                | ProtocolMessage::HeartbeatPulse(_)
                | ProtocolMessage::ReceiptFetch { .. } => {}
                other => panic!("expected explicit legacy sync response, got {other:?}"),
            }
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn v2_sync_request_uses_certified_push_when_peer_qc_state_is_known() {
        let genesis = sample_genesis();
        let local_address = reserve_local_address();
        let peer_address = reserve_local_address();
        let peer = PeerConfig {
            validator_id: 2,
            address: peer_address.clone(),
        };
        let config = sample_node_config(1, local_address, vec![peer], "v2-sync-request-push");
        let mut runner = build_test_runner(1, config, genesis.clone()).await;
        let (_, _, _, mut peer_events) =
            spawn_test_network(2, peer_address, Vec::new(), &genesis).await;
        let shared_root = first_valid_block(&genesis);

        assert_eq!(
            runner
                .accept_block(shared_root.clone(), true)
                .await
                .unwrap(),
            BlockAcceptance::Accepted
        );
        assert!(
            runner
                .import_proposal_vote(signed_proposal_vote(
                    runner.crypto.as_ref(),
                    2,
                    &shared_root
                ))
                .unwrap()
        );
        assert!(
            runner
                .import_proposal_vote(signed_proposal_vote(
                    runner.crypto.as_ref(),
                    3,
                    &shared_root
                ))
                .unwrap()
        );

        let crypto = DeterministicCryptoBackend::from_genesis(&genesis);
        let consensus = ConsensusEngine::new(genesis.clone());
        let mut root_ledger = LedgerState::from_genesis(&genesis);
        root_ledger.apply_block(&shared_root, &crypto).unwrap();
        let slot = ((shared_root.header.slot + 1)..(shared_root.header.slot + 50))
            .find(|slot| consensus.proposer_for_slot(*slot) == 2)
            .unwrap();
        let epoch = consensus.epoch_for_slot(slot);
        let transactions = Vec::new();
        let commitment: Option<TopologyCommitment> = None;
        let header = BlockHeader {
            block_number: 2,
            parent_hash: shared_root.block_hash,
            slot,
            epoch,
            proposer_id: 2,
            timestamp_unix_millis: now_unix_millis(),
            state_root: root_ledger.state_root(),
            transactions_root: canonical_hash(&transactions),
            topology_root: empty_hash(),
        };
        let block_hash = canonical_hash(&(header.clone(), &transactions, &commitment));
        let child = Block {
            header,
            transactions,
            commitment,
            commitment_receipts: Vec::new(),
            signature: crypto.sign(2, &block_hash).unwrap(),
            block_hash,
        };
        assert_eq!(
            runner.accept_block(child.clone(), false).await.unwrap(),
            BlockAcceptance::Orphan
        );
        assert!(
            runner
                .import_proposal_vote(signed_proposal_vote(runner.crypto.as_ref(), 3, &child))
                .unwrap()
        );
        assert!(
            runner
                .import_proposal_vote(signed_proposal_vote(runner.crypto.as_ref(), 4, &child))
                .unwrap()
        );
        assert!(runner.quorum_certificates.contains_key(&child.block_hash));

        runner.record_peer_sync_status(
            2,
            shared_root.header.block_number,
            shared_root.block_hash,
            Some(shared_root.block_hash),
            shared_root.header.block_number,
            vec![SyncQcAnchor {
                block_hash: shared_root.block_hash,
                block_number: shared_root.header.block_number,
            }],
        );

        let drain_deadline = tokio::time::Instant::now() + Duration::from_millis(100);
        loop {
            let remaining = drain_deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                break;
            }
            let Ok(_) =
                tokio::time::timeout(remaining, recv_protocol_message(&mut peer_events)).await
            else {
                break;
            };
        }

        runner
            .handle_network_event(NetworkEvent::Received {
                from_validator_id: 2,
                payload: ProtocolMessage::SyncRequest {
                    requester_id: 2,
                    known_height: shared_root.header.block_number,
                    known_tip_hash: shared_root.block_hash,
                },
                bytes: 0,
            })
            .await
            .unwrap();

        let deadline = tokio::time::Instant::now() + Duration::from_millis(500);
        let mut saw_certified = false;
        let mut saw_sync_blocks = false;
        while !(saw_certified && saw_sync_blocks) {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            assert!(
                !remaining.is_zero(),
                "expected both certified sync and anchored suffix sync"
            );
            let message = tokio::time::timeout(remaining, recv_protocol_message(&mut peer_events))
                .await
                .unwrap();
            match message {
                ProtocolMessage::CertifiedSyncResponse(ChunkedSyncResponse::Certified {
                    shared_qc_hash,
                    shared_qc_height,
                    responder_height,
                    ..
                }) => {
                    assert_eq!(shared_qc_hash, shared_root.block_hash);
                    assert_eq!(shared_qc_height, shared_root.header.block_number);
                    assert_eq!(responder_height, child.header.block_number);
                    saw_certified = true;
                }
                ProtocolMessage::SyncBlocks {
                    responder_id,
                    chain,
                } => {
                    assert_eq!(responder_id, 1);
                    assert_eq!(chain.base_height, shared_root.header.block_number);
                    assert_eq!(chain.base_tip_hash, shared_root.block_hash);
                    assert_eq!(chain.target_snapshot.height, child.header.block_number);
                    assert_eq!(chain.blocks.len(), 1);
                    assert_eq!(chain.blocks[0].block_hash, child.block_hash);
                    saw_sync_blocks = true;
                }
                ProtocolMessage::SyncStatus { .. }
                | ProtocolMessage::ProposalVote(_)
                | ProtocolMessage::QuorumCertificate(_)
                | ProtocolMessage::HeartbeatPulse(_)
                | ProtocolMessage::ReceiptFetch { .. } => {}
                other => {
                    panic!("unexpected message while checking v2 sync request push: {other:?}")
                }
            }
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn improved_service_attestation_supersedes_prior_observation() {
        let genesis = sample_genesis();
        let config =
            sample_node_config(1, reserve_local_address(), Vec::new(), "attestation-update");
        let mut runner = build_test_runner(1, config, genesis).await;
        let original = signed_service_attestation(
            runner.crypto.as_ref(),
            1,
            2,
            1,
            ServiceCounters {
                uptime_windows: 0,
                total_windows: 6,
                timely_deliveries: 0,
                expected_deliveries: 6,
                distinct_peers: 0,
                expected_peers: 1,
                failed_sessions: 0,
                invalid_receipts: 0,
            },
        );
        let improved = signed_service_attestation(
            runner.crypto.as_ref(),
            1,
            2,
            1,
            ServiceCounters {
                uptime_windows: 5,
                total_windows: 6,
                timely_deliveries: 5,
                expected_deliveries: 6,
                distinct_peers: 1,
                expected_peers: 1,
                failed_sessions: 0,
                invalid_receipts: 0,
            },
        );

        assert!(runner.import_service_attestation(original).unwrap());
        assert!(runner.import_service_attestation(improved.clone()).unwrap());
        assert_eq!(
            runner
                .service_attestations
                .get(&(1, 1))
                .and_then(|by_member| by_member.get(&2))
                .unwrap()
                .counters,
            improved.counters
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn improved_service_aggregate_supersedes_prior_version() {
        let genesis = sample_genesis();
        let config = sample_node_config(1, reserve_local_address(), Vec::new(), "aggregate-update");
        let mut runner = build_test_runner(1, config, genesis).await;
        let original = service_aggregate_for_subject(
            runner.crypto.as_ref(),
            &runner.consensus,
            1,
            1,
            ServiceCounters {
                uptime_windows: 0,
                total_windows: 6,
                timely_deliveries: 0,
                expected_deliveries: 6,
                distinct_peers: 0,
                expected_peers: 1,
                failed_sessions: 0,
                invalid_receipts: 0,
            },
        );
        let improved = service_aggregate_for_subject(
            runner.crypto.as_ref(),
            &runner.consensus,
            1,
            1,
            ServiceCounters {
                uptime_windows: 5,
                total_windows: 6,
                timely_deliveries: 5,
                expected_deliveries: 6,
                distinct_peers: 1,
                expected_peers: 1,
                failed_sessions: 0,
                invalid_receipts: 0,
            },
        );

        assert!(runner.import_service_aggregate(original).unwrap());
        assert!(runner.import_service_aggregate(improved.clone()).unwrap());
        assert_eq!(
            runner
                .service_aggregates
                .get(&(1, 1))
                .unwrap()
                .aggregate_counters,
            improved.aggregate_counters
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn conflicting_service_aggregates_merge_at_attestation_layer() {
        let genesis = sample_genesis();
        let config = sample_node_config(1, reserve_local_address(), Vec::new(), "aggregate-merge");
        let mut runner = build_test_runner(1, config, genesis).await;

        let good = ServiceCounters {
            uptime_windows: 6,
            total_windows: 6,
            timely_deliveries: 6,
            expected_deliveries: 6,
            distinct_peers: 1,
            expected_peers: 1,
            failed_sessions: 0,
            invalid_receipts: 0,
        };
        let zero = ServiceCounters {
            uptime_windows: 0,
            total_windows: 6,
            timely_deliveries: 0,
            expected_deliveries: 6,
            distinct_peers: 0,
            expected_peers: 1,
            failed_sessions: 0,
            invalid_receipts: 0,
        };

        let aggregate_a = service_aggregate_from_attestations(
            1,
            1,
            vec![
                signed_service_attestation(runner.crypto.as_ref(), 1, 2, 1, good.clone()),
                signed_service_attestation(runner.crypto.as_ref(), 1, 3, 1, zero.clone()),
            ],
        );
        let aggregate_b = service_aggregate_from_attestations(
            1,
            1,
            vec![
                signed_service_attestation(runner.crypto.as_ref(), 1, 2, 1, zero),
                signed_service_attestation(runner.crypto.as_ref(), 1, 3, 1, good.clone()),
            ],
        );

        assert!(runner.import_service_aggregate(aggregate_a).unwrap());
        assert!(runner.import_service_aggregate(aggregate_b).unwrap());

        let by_member = runner.service_attestations.get(&(1, 1)).unwrap();
        assert_eq!(by_member.get(&2).unwrap().counters, good);
        assert_eq!(by_member.get(&3).unwrap().counters, good);
        assert_eq!(
            runner
                .service_aggregates
                .get(&(1, 1))
                .unwrap()
                .aggregate_counters,
            entangrid_types::aggregate_service_counters(&[
                by_member.get(&2).unwrap().clone(),
                by_member.get(&3).unwrap().clone(),
            ])
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn later_epoch_aggregate_with_more_attestations_replaces_first_threshold_snapshot() {
        let mut balances = BTreeMap::new();
        for validator_id in 1..=8 {
            balances.insert(validator_account(validator_id), 1_000);
        }
        let genesis = GenesisConfig {
            chain_id: "test".into(),
            epoch_seed: empty_hash(),
            genesis_time_unix_millis: 0,
            slot_duration_millis: 1_000,
            slots_per_epoch: 6,
            max_txs_per_block: 16,
            witness_count: 7,
            validators: (1..=8)
                .map(|validator_id| ValidatorConfig {
                    validator_id,
                    stake: 100,
                    address: format!("127.0.0.1:{}", 4100 + validator_id),
                    dev_secret: format!("secret-{validator_id}"),
                    public_identity: PublicIdentity::default(),
                })
                .collect(),
            initial_balances: balances,
        };
        let config = sample_node_config(
            1,
            reserve_local_address(),
            Vec::new(),
            "aggregate-upsert-window",
        );
        let mut runner = build_test_runner(1, config, genesis).await;
        let subject_validator_id = 3;
        let epoch = 2;
        let committee = runner
            .consensus
            .service_committee_for(epoch, subject_validator_id);
        assert_eq!(committee.len(), 7);

        let weak_positive = ServiceCounters {
            uptime_windows: 2,
            total_windows: 6,
            timely_deliveries: 2,
            expected_deliveries: 6,
            distinct_peers: 1,
            expected_peers: 1,
            failed_sessions: 0,
            invalid_receipts: 0,
        };
        let zero = ServiceCounters {
            uptime_windows: 0,
            total_windows: 6,
            timely_deliveries: 0,
            expected_deliveries: 6,
            distinct_peers: 0,
            expected_peers: 1,
            failed_sessions: 0,
            invalid_receipts: 0,
        };

        let first_threshold = service_aggregate_from_attestations(
            subject_validator_id,
            epoch,
            committee[..5]
                .iter()
                .map(|committee_member_id| {
                    signed_service_attestation(
                        runner.crypto.as_ref(),
                        subject_validator_id,
                        *committee_member_id,
                        epoch,
                        weak_positive.clone(),
                    )
                })
                .collect(),
        );
        assert!(runner.import_service_aggregate(first_threshold).unwrap());
        runner.refresh_service_scores();
        assert!(matches!(
            runner.v2_gating_state(subject_validator_id, epoch + 1),
            V2GatingState::AllowScore(score) if score > runner.service_gating_threshold()
        ));

        let expanded_committee = service_aggregate_from_attestations(
            subject_validator_id,
            epoch,
            committee
                .iter()
                .enumerate()
                .map(|(index, committee_member_id)| {
                    signed_service_attestation(
                        runner.crypto.as_ref(),
                        subject_validator_id,
                        *committee_member_id,
                        epoch,
                        if index < 5 {
                            weak_positive.clone()
                        } else {
                            zero.clone()
                        },
                    )
                })
                .collect(),
        );
        assert!(runner.import_service_aggregate(expanded_committee).unwrap());
        runner.refresh_service_scores();

        let aggregate = runner
            .service_aggregates
            .get(&(subject_validator_id, epoch))
            .expect("expanded aggregate should be stored");
        assert_eq!(aggregate.attestations.len(), 7);
        assert_eq!(aggregate.aggregate_counters.expected_peers, 7);
        assert!(matches!(
            runner.v2_gating_state(subject_validator_id, epoch + 1),
            V2GatingState::RejectScore(score) if score < runner.service_gating_threshold()
        ));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn reemitting_completed_epoch_attestations_refreshes_improved_receipts() {
        let genesis = sample_genesis();
        let config = sample_node_config(
            2,
            reserve_local_address(),
            Vec::new(),
            "attestation-backfill",
        );
        let mut runner = build_test_runner(2, config, genesis).await;

        runner
            .emit_service_attestations_for_completed_epoch(1)
            .unwrap();
        let initial = runner
            .service_attestations
            .get(&(1, 1))
            .and_then(|by_member| by_member.get(&2))
            .unwrap()
            .counters
            .clone();
        assert_eq!(initial.uptime_windows, 0);

        let mut receipt = sample_receipt();
        receipt.epoch = 1;
        receipt.slot = 11;
        receipt.source_validator_id = 1;
        receipt.destination_validator_id = 2;
        receipt.witness_validator_id = 2;
        receipt.message_class = MessageClass::Heartbeat;
        runner.persist_receipt(receipt).unwrap();

        runner
            .emit_service_attestations_for_completed_epoch(1)
            .unwrap();
        let refreshed = runner
            .service_attestations
            .get(&(1, 1))
            .and_then(|by_member| by_member.get(&2))
            .unwrap()
            .counters
            .clone();
        assert!(refreshed.uptime_windows > initial.uptime_windows);
        assert!(refreshed.timely_deliveries > initial.timely_deliveries);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn v2_attestations_only_score_observable_witness_obligations() {
        let genesis = sample_genesis();
        let config = sample_node_config(2, reserve_local_address(), Vec::new(), "witness-v2");
        let runner = build_test_runner(2, config, genesis.clone()).await;

        let attestation = runner.build_local_service_attestation(1, 1).unwrap();

        assert_eq!(attestation.committee_member_id, 2);
        assert_eq!(attestation.counters.total_windows, genesis.slots_per_epoch);
        assert_eq!(
            attestation.counters.expected_deliveries,
            genesis.slots_per_epoch
        );
        assert_eq!(attestation.counters.expected_peers, 1);
        assert_eq!(attestation.counters.failed_sessions, 0);
        assert_eq!(attestation.counters.invalid_receipts, 0);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn v2_scores_fall_back_to_latest_available_prior_aggregate() {
        let genesis = sample_genesis();
        let config =
            sample_node_config(1, reserve_local_address(), Vec::new(), "aggregate-fallback");
        let mut runner = build_test_runner(1, config, genesis.clone()).await;
        runner.last_processed_slot = Some(genesis.slots_per_epoch * 3);

        let aggregate = service_aggregate_for_subject(
            runner.crypto.as_ref(),
            &runner.consensus,
            1,
            1,
            ServiceCounters {
                uptime_windows: 5,
                total_windows: 5,
                timely_deliveries: 5,
                expected_deliveries: 5,
                distinct_peers: 2,
                expected_peers: 2,
                failed_sessions: 0,
                invalid_receipts: 0,
            },
        );

        runner.import_service_aggregate(aggregate.clone()).unwrap();
        runner.refresh_service_scores();

        let expected_score = runner.consensus.compute_service_score_from_aggregate(
            &aggregate,
            &runner.config.feature_flags.service_score_weights,
        );
        assert_eq!(runner.service_score_for_validator(1), expected_score);
        assert_eq!(runner.snapshot_metrics().last_completed_service_epoch, 1);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn v2_scores_preserve_last_known_value_when_no_aggregate_is_available() {
        let genesis = sample_genesis();
        let config =
            sample_node_config(1, reserve_local_address(), Vec::new(), "aggregate-preserve");
        let mut runner = build_test_runner(1, config, genesis.clone()).await;
        runner.last_processed_slot = Some(genesis.slots_per_epoch * 4);
        let prior_counters = ServiceCounters {
            uptime_windows: 3,
            total_windows: 5,
            timely_deliveries: 4,
            expected_deliveries: 5,
            distinct_peers: 2,
            expected_peers: 2,
            failed_sessions: 0,
            invalid_receipts: 0,
        };
        runner.latest_service_scores.insert(1, 0.73);
        runner
            .latest_service_counters
            .insert(1, prior_counters.clone());

        runner.refresh_service_scores();

        assert_eq!(runner.service_score_for_validator(1), 0.73);
        assert_eq!(runner.local_service_counters(), prior_counters);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn v2_local_proposer_gating_uses_prior_epoch_aggregate_instead_of_legacy_rolling_score() {
        let genesis = sample_genesis();
        let config = sample_node_config(1, reserve_local_address(), Vec::new(), "v2-gating");
        let mut runner = build_test_runner(1, config, genesis.clone()).await;
        let (proposal_epoch, slot) = (2..8)
            .find_map(|epoch| {
                let start = genesis.slots_per_epoch * epoch;
                let end = genesis.slots_per_epoch * (epoch + 1);
                (start..end)
                    .find(|slot| runner.consensus.proposer_for_slot(*slot) == 1)
                    .map(|slot| (epoch, slot))
            })
            .unwrap();
        runner.last_processed_slot = Some(genesis.slots_per_epoch * proposal_epoch);
        runner
            .observed_invalid_receipts
            .insert((proposal_epoch - 1, 1), 100);
        runner
            .observed_failed_sessions
            .insert((proposal_epoch - 1, 1, 2));
        runner.known_live_peers.insert(2);

        let aggregate = service_aggregate_for_subject(
            runner.crypto.as_ref(),
            &runner.consensus,
            1,
            proposal_epoch - 1,
            ServiceCounters {
                uptime_windows: 5,
                total_windows: 5,
                timely_deliveries: 5,
                expected_deliveries: 5,
                distinct_peers: 2,
                expected_peers: 2,
                failed_sessions: 0,
                invalid_receipts: 0,
            },
        );
        runner.import_service_aggregate(aggregate).unwrap();
        runner.refresh_service_scores();

        runner
            .maybe_propose_block(slot, proposal_epoch)
            .await
            .unwrap();

        assert_eq!(runner.last_proposed_slot, Some(slot));
        assert_eq!(runner.blocks.len(), 1);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn local_v2_proposer_is_rejected_when_confirmed_aggregate_score_is_below_threshold() {
        let genesis = sample_genesis();
        let config = sample_node_config(
            1,
            reserve_local_address(),
            Vec::new(),
            "v2-confirmed-reject",
        );
        let mut runner = build_test_runner(1, config, genesis.clone()).await;
        let (proposal_epoch, slot) = (2..8)
            .find_map(|epoch| {
                let start = genesis.slots_per_epoch * epoch;
                let end = genesis.slots_per_epoch * (epoch + 1);
                (start..end)
                    .find(|slot| runner.consensus.proposer_for_slot(*slot) == 1)
                    .map(|slot| (epoch, slot))
            })
            .unwrap();
        runner.last_processed_slot = Some(genesis.slots_per_epoch * proposal_epoch);

        let aggregate = service_aggregate_for_subject(
            runner.crypto.as_ref(),
            &runner.consensus,
            1,
            proposal_epoch - 1,
            ServiceCounters {
                uptime_windows: 0,
                total_windows: 5,
                timely_deliveries: 0,
                expected_deliveries: 5,
                distinct_peers: 0,
                expected_peers: 2,
                failed_sessions: 0,
                invalid_receipts: 0,
            },
        );
        runner.import_service_aggregate(aggregate).unwrap();
        runner.refresh_service_scores();

        let aggregate = runner
            .service_aggregates
            .get(&(1, proposal_epoch - 1))
            .unwrap();
        let score = runner.consensus.compute_service_score_from_aggregate(
            aggregate,
            &runner.config.feature_flags.service_score_weights,
        );
        assert!(
            score < runner.service_gating_threshold(),
            "expected low score, got score={score:?} counters={:?}",
            aggregate.aggregate_counters
        );

        assert!(matches!(
            runner.v2_gating_state(1, proposal_epoch),
            V2GatingState::RejectScore(score) if score < runner.service_gating_threshold()
        ));

        runner
            .maybe_propose_block(slot, proposal_epoch)
            .await
            .unwrap();

        let metrics = runner.snapshot_metrics();
        assert_eq!(runner.last_proposed_slot, None);
        assert!(runner.blocks.is_empty());
        assert_eq!(metrics.service_gating_rejections, 1);
        assert_eq!(metrics.service_gating_enforcement_skips, 0);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn recent_v2_low_score_within_window_rejects_without_exact_prior_epoch_match() {
        let genesis = sample_genesis();
        let config = sample_node_config(1, reserve_local_address(), Vec::new(), "v2-gating-skip");
        let mut runner = build_test_runner(1, config, genesis.clone()).await;
        let (proposal_epoch, slot) = (4..9)
            .find_map(|epoch| {
                let start = genesis.slots_per_epoch * epoch;
                let end = genesis.slots_per_epoch * (epoch + 1);
                (start..end)
                    .find(|slot| runner.consensus.proposer_for_slot(*slot) == 1)
                    .map(|slot| (epoch, slot))
            })
            .unwrap();
        runner.last_processed_slot = Some(genesis.slots_per_epoch * proposal_epoch);

        let recent_low_aggregate = service_aggregate_for_subject(
            runner.crypto.as_ref(),
            &runner.consensus,
            1,
            proposal_epoch - 2,
            ServiceCounters {
                uptime_windows: 0,
                total_windows: 5,
                timely_deliveries: 0,
                expected_deliveries: 5,
                distinct_peers: 0,
                expected_peers: 2,
                failed_sessions: 0,
                invalid_receipts: 0,
            },
        );
        runner
            .import_service_aggregate(recent_low_aggregate)
            .unwrap();
        runner.refresh_service_scores();

        runner
            .maybe_propose_block(slot, proposal_epoch)
            .await
            .unwrap();

        let metrics = runner.snapshot_metrics();
        assert_eq!(runner.last_proposed_slot, None);
        assert!(runner.blocks.is_empty());
        assert_eq!(metrics.service_gating_rejections, 1);
        assert_eq!(metrics.service_gating_enforcement_skips, 0);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn v2_gating_uses_weighted_recent_confirmed_window_instead_of_latest_aggregate_only() {
        let genesis = sample_genesis();
        let config =
            sample_node_config(1, reserve_local_address(), Vec::new(), "v2-windowed-gating");
        let mut runner = build_test_runner(1, config, genesis.clone()).await;
        let (proposal_epoch, slot) = (4..9)
            .find_map(|epoch| {
                let start = genesis.slots_per_epoch * epoch;
                let end = genesis.slots_per_epoch * (epoch + 1);
                (start..end)
                    .find(|slot| runner.consensus.proposer_for_slot(*slot) == 1)
                    .map(|slot| (epoch, slot))
            })
            .unwrap();
        runner.last_processed_slot = Some(genesis.slots_per_epoch * proposal_epoch);

        let older_low_aggregate = service_aggregate_for_subject(
            runner.crypto.as_ref(),
            &runner.consensus,
            1,
            proposal_epoch - 2,
            ServiceCounters {
                uptime_windows: 0,
                total_windows: 5,
                timely_deliveries: 0,
                expected_deliveries: 5,
                distinct_peers: 0,
                expected_peers: 2,
                failed_sessions: 0,
                invalid_receipts: 0,
            },
        );
        let latest_medium_aggregate = service_aggregate_for_subject(
            runner.crypto.as_ref(),
            &runner.consensus,
            1,
            proposal_epoch - 1,
            ServiceCounters {
                uptime_windows: 0,
                total_windows: 5,
                timely_deliveries: 5,
                expected_deliveries: 5,
                distinct_peers: 0,
                expected_peers: 2,
                failed_sessions: 0,
                invalid_receipts: 0,
            },
        );
        runner
            .import_service_aggregate(older_low_aggregate)
            .unwrap();
        runner
            .import_service_aggregate(latest_medium_aggregate.clone())
            .unwrap();
        runner.refresh_service_scores();

        let latest_only_score = runner.consensus.compute_service_score_from_aggregate(
            &latest_medium_aggregate,
            &runner.config.feature_flags.service_score_weights,
        );
        assert!(
            latest_only_score > runner.service_gating_threshold(),
            "expected the latest aggregate alone to clear the threshold, got {latest_only_score}"
        );

        assert!(matches!(
            runner.v2_gating_state(1, proposal_epoch),
            V2GatingState::RejectScore(score) if score < runner.service_gating_threshold()
        ));

        runner
            .maybe_propose_block(slot, proposal_epoch)
            .await
            .unwrap();

        let metrics = runner.snapshot_metrics();
        assert_eq!(runner.last_proposed_slot, None);
        assert!(runner.blocks.is_empty());
        assert_eq!(metrics.service_gating_rejections, 1);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn very_stale_v2_low_score_outside_window_skips_gating() {
        let genesis = sample_genesis();
        let config = sample_node_config(1, reserve_local_address(), Vec::new(), "v2-gating-old");
        let mut runner = build_test_runner(1, config, genesis.clone()).await;
        let window = runner.config.feature_flags.service_score_window_epochs;
        let (proposal_epoch, slot) = ((window + 3)..=(window + 8))
            .find_map(|epoch| {
                let start = genesis.slots_per_epoch * epoch;
                let end = genesis.slots_per_epoch * (epoch + 1);
                (start..end)
                    .find(|slot| runner.consensus.proposer_for_slot(*slot) == 1)
                    .map(|slot| (epoch, slot))
            })
            .unwrap();
        runner.last_processed_slot = Some(genesis.slots_per_epoch * proposal_epoch);

        let old_low_aggregate = service_aggregate_for_subject(
            runner.crypto.as_ref(),
            &runner.consensus,
            1,
            1,
            ServiceCounters {
                uptime_windows: 0,
                total_windows: 5,
                timely_deliveries: 0,
                expected_deliveries: 5,
                distinct_peers: 0,
                expected_peers: 2,
                failed_sessions: 0,
                invalid_receipts: 0,
            },
        );
        runner.import_service_aggregate(old_low_aggregate).unwrap();
        runner.refresh_service_scores();

        runner
            .maybe_propose_block(slot, proposal_epoch)
            .await
            .unwrap();

        let metrics = runner.snapshot_metrics();
        assert_eq!(runner.last_proposed_slot, Some(slot));
        assert_eq!(runner.blocks.len(), 1);
        assert_eq!(metrics.service_gating_rejections, 0);
        assert_eq!(metrics.service_gating_enforcement_skips, 1);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn recent_v2_low_score_one_epoch_outside_window_still_rejects() {
        let genesis = sample_genesis();
        let mut config = sample_node_config(
            1,
            reserve_local_address(),
            Vec::new(),
            "v2-gating-window-fallback",
        );
        config.feature_flags.service_score_window_epochs = 1;
        let mut runner = build_test_runner(1, config, genesis.clone()).await;
        let (proposal_epoch, slot) = (4..9)
            .find_map(|epoch| {
                let start = genesis.slots_per_epoch * epoch;
                let end = genesis.slots_per_epoch * (epoch + 1);
                (start..end)
                    .find(|slot| runner.consensus.proposer_for_slot(*slot) == 1)
                    .map(|slot| (epoch, slot))
            })
            .unwrap();
        runner.last_processed_slot = Some(genesis.slots_per_epoch * proposal_epoch);

        let recent_low_aggregate = service_aggregate_for_subject(
            runner.crypto.as_ref(),
            &runner.consensus,
            1,
            proposal_epoch - 2,
            ServiceCounters {
                uptime_windows: 0,
                total_windows: 5,
                timely_deliveries: 0,
                expected_deliveries: 5,
                distinct_peers: 0,
                expected_peers: 2,
                failed_sessions: 0,
                invalid_receipts: 0,
            },
        );
        runner
            .import_service_aggregate(recent_low_aggregate)
            .unwrap();
        runner.refresh_service_scores();

        assert!(matches!(
            runner.v2_gating_state(1, proposal_epoch),
            V2GatingState::RejectScore(score) if score < runner.service_gating_threshold()
        ));

        runner
            .maybe_propose_block(slot, proposal_epoch)
            .await
            .unwrap();

        let metrics = runner.snapshot_metrics();
        assert_eq!(runner.last_proposed_slot, None);
        assert!(runner.blocks.is_empty());
        assert_eq!(metrics.service_gating_rejections, 1);
        assert_eq!(metrics.service_gating_enforcement_skips, 0);
    }

    #[test]
    fn pending_slots_include_all_missed_slots() {
        assert_eq!(pending_slots_to_process(None, 3), vec![0, 1, 2, 3]);
        assert_eq!(pending_slots_to_process(Some(5), 5), vec![5]);
        assert_eq!(pending_slots_to_process(Some(5), 8), vec![6, 7, 8]);
    }

    #[test]
    fn initial_last_processed_slot_skips_historical_replay_for_late_start() {
        let genesis = sample_genesis();
        let consensus = ConsensusEngine::new(genesis.clone());
        let live_now = genesis.genesis_time_unix_millis + genesis.slot_duration_millis * 5;

        assert_eq!(
            initial_last_processed_slot(&consensus, None, live_now),
            Some(4)
        );
        assert_eq!(
            initial_last_processed_slot(&consensus, Some(2), live_now),
            Some(4)
        );
        assert_eq!(
            initial_last_processed_slot(&consensus, Some(5), live_now),
            Some(5)
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn process_slot_tick_does_not_propose_for_historical_missed_slots() {
        let mut genesis = sample_genesis();
        genesis.slot_duration_millis = 10_000;
        genesis.genesis_time_unix_millis =
            now_unix_millis().saturating_sub(genesis.slot_duration_millis * 4);
        let consensus = ConsensusEngine::new(genesis.clone());
        let current_slot = consensus.slot_at(now_unix_millis());
        assert!(current_slot >= 2);
        let historical_slot = current_slot - 1;
        let validator_id = consensus.proposer_for_slot(historical_slot);
        assert_ne!(validator_id, consensus.proposer_for_slot(current_slot));

        let config = sample_node_config(
            validator_id,
            reserve_local_address(),
            Vec::new(),
            "historical-slot-replay",
        );
        let mut runner = build_test_runner(validator_id, config, genesis).await;
        runner.last_processed_slot = Some(historical_slot - 1);

        runner.process_slot_tick().await.unwrap();

        assert!(runner.blocks.is_empty());
        assert_eq!(runner.last_proposed_slot, None);
        assert_eq!(runner.last_processed_slot, Some(current_slot));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn restarting_node_does_not_propose_while_known_peer_is_ahead() {
        let genesis = sample_genesis();
        let peer = PeerConfig {
            validator_id: 2,
            address: reserve_local_address(),
        };
        let config = sample_node_config(
            1,
            reserve_local_address(),
            vec![peer],
            "startup-sync-proposal-hold",
        );
        let mut runner = build_test_runner(1, config, genesis.clone()).await;
        runner.startup_sync_barrier = true;
        runner.record_peer_sync_status(2, 5, canonical_hash(&"ahead-tip"), None, 0, Vec::new());

        let slot = (0..50)
            .find(|slot| runner.consensus.proposer_for_slot(*slot) == 1)
            .unwrap();
        let epoch = runner.consensus.epoch_for_slot(slot);
        runner.last_processed_slot = Some(genesis.slots_per_epoch * epoch);

        runner.maybe_propose_block(slot, epoch).await.unwrap();

        assert!(runner.blocks.is_empty());
        assert_eq!(runner.last_proposed_slot, None);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn startup_sync_barrier_clears_once_known_peers_are_caught_up() {
        let genesis = sample_genesis();
        let peer = PeerConfig {
            validator_id: 2,
            address: reserve_local_address(),
        };
        let config = sample_node_config(
            1,
            reserve_local_address(),
            vec![peer],
            "startup-sync-proposal-release",
        );
        let mut runner = build_test_runner(1, config, genesis.clone()).await;
        runner.startup_sync_barrier = true;
        let local_snapshot = runner.ledger.snapshot().clone();
        runner.record_peer_sync_status(
            2,
            local_snapshot.height,
            local_snapshot.tip_hash,
            None,
            0,
            Vec::new(),
        );

        let slot = (0..50)
            .find(|slot| runner.consensus.proposer_for_slot(*slot) == 1)
            .unwrap();
        let epoch = runner.consensus.epoch_for_slot(slot);
        runner.last_processed_slot = Some(genesis.slots_per_epoch * epoch);

        runner.maybe_propose_block(slot, epoch).await.unwrap();

        assert_eq!(runner.blocks.len(), 1);
        assert_eq!(runner.last_proposed_slot, Some(slot));
        assert!(!runner.startup_sync_barrier);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn startup_sync_barrier_live_slot_proactively_requests_sync_from_ahead_peer() {
        let mut genesis = sample_genesis();
        genesis.slot_duration_millis = 10_000;
        genesis.genesis_time_unix_millis =
            now_unix_millis().saturating_sub(genesis.slot_duration_millis * 3);
        let peer_address = reserve_local_address();
        let peer = PeerConfig {
            validator_id: 2,
            address: peer_address.clone(),
        };
        let mut config = sample_node_config(
            1,
            reserve_local_address(),
            vec![peer],
            "startup-sync-live-request",
        );
        config.sync_on_startup = true;
        let mut runner = build_test_runner(1, config, genesis.clone()).await;
        let (_, _, _, mut peer_events) =
            spawn_test_network(2, peer_address, Vec::new(), &genesis).await;
        let current_slot = runner.consensus.slot_at(now_unix_millis());
        runner.last_processed_slot = current_slot.checked_sub(1);
        runner.startup_sync_barrier = true;
        runner.record_peer_sync_status(
            2,
            current_slot + 1,
            canonical_hash(&"ahead-tip"),
            None,
            0,
            Vec::new(),
        );

        runner.process_slot_tick().await.unwrap();

        let deadline = tokio::time::Instant::now() + Duration::from_millis(250);
        let mut saw_sync_request = false;
        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                break;
            }
            let Ok(message) =
                tokio::time::timeout(remaining, recv_protocol_message(&mut peer_events)).await
            else {
                break;
            };
            match message {
                ProtocolMessage::SyncRequest { requester_id, .. } => {
                    assert_eq!(requester_id, 1);
                    saw_sync_request = true;
                    break;
                }
                ProtocolMessage::SyncStatus { .. }
                | ProtocolMessage::ProposalVote(_)
                | ProtocolMessage::QuorumCertificate(_)
                | ProtocolMessage::HeartbeatPulse(_)
                | ProtocolMessage::ReceiptFetch { .. } => {}
                other => {
                    panic!("unexpected message while waiting for proactive startup sync: {other:?}")
                }
            }
        }

        assert!(
            saw_sync_request,
            "expected proactive sync request during startup barrier"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn startup_sync_barrier_targets_single_best_ahead_peer() {
        let mut genesis = sample_genesis();
        genesis.slot_duration_millis = 10_000;
        genesis.genesis_time_unix_millis =
            now_unix_millis().saturating_sub(genesis.slot_duration_millis * 3);
        let peer2_address = reserve_local_address();
        let peer3_address = reserve_local_address();
        let config = sample_node_config(
            1,
            reserve_local_address(),
            vec![
                PeerConfig {
                    validator_id: 2,
                    address: peer2_address.clone(),
                },
                PeerConfig {
                    validator_id: 3,
                    address: peer3_address.clone(),
                },
            ],
            "startup-sync-single-target",
        );
        let mut runner = build_test_runner(1, config, genesis.clone()).await;
        let (_, _, _, mut peer2_events) =
            spawn_test_network(2, peer2_address, Vec::new(), &genesis).await;
        let (_, _, _, mut peer3_events) =
            spawn_test_network(3, peer3_address, Vec::new(), &genesis).await;
        runner.startup_sync_barrier = true;
        runner.record_peer_sync_status(2, 5, canonical_hash(&"ahead-tip-2"), None, 0, Vec::new());
        runner.record_peer_sync_status(3, 7, canonical_hash(&"ahead-tip-3"), None, 0, Vec::new());

        runner.run_sync_maintenance().unwrap();

        let deadline = tokio::time::Instant::now() + Duration::from_millis(250);
        let mut peer2_sync_requests = 0usize;
        let mut peer3_sync_requests = 0usize;
        while tokio::time::Instant::now() < deadline {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if let Ok(message) =
                tokio::time::timeout(remaining, recv_protocol_message(&mut peer2_events)).await
            {
                if matches!(
                    message,
                    ProtocolMessage::SyncRequest {
                        requester_id: 1,
                        ..
                    }
                ) {
                    peer2_sync_requests += 1;
                }
            } else {
                break;
            }
        }
        let deadline = tokio::time::Instant::now() + Duration::from_millis(250);
        while tokio::time::Instant::now() < deadline {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if let Ok(message) =
                tokio::time::timeout(remaining, recv_protocol_message(&mut peer3_events)).await
            {
                if matches!(
                    message,
                    ProtocolMessage::SyncRequest {
                        requester_id: 1,
                        ..
                    }
                ) {
                    peer3_sync_requests += 1;
                }
            } else {
                break;
            }
        }

        assert_eq!(peer2_sync_requests, 0);
        assert_eq!(peer3_sync_requests, 1);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn certified_progress_proactively_requests_sync_from_ahead_peer() {
        let genesis = sample_genesis();
        let local_address = reserve_local_address();
        let peer_address = reserve_local_address();
        let peer = PeerConfig {
            validator_id: 2,
            address: peer_address.clone(),
        };
        let config = sample_node_config(1, local_address, vec![peer], "qc-progress-pull");
        let mut runner = build_test_runner(1, config, genesis.clone()).await;
        let (_, _, _, mut peer_events) =
            spawn_test_network(2, peer_address, Vec::new(), &genesis).await;
        let certified_root = first_valid_block(&genesis);

        assert_eq!(
            runner
                .accept_block(certified_root.clone(), true)
                .await
                .unwrap(),
            BlockAcceptance::Accepted
        );
        assert!(
            runner
                .import_proposal_vote(signed_proposal_vote(
                    runner.crypto.as_ref(),
                    2,
                    &certified_root
                ))
                .unwrap()
        );
        assert!(
            runner
                .import_proposal_vote(signed_proposal_vote(
                    runner.crypto.as_ref(),
                    3,
                    &certified_root
                ))
                .unwrap()
        );
        assert_eq!(
            runner.highest_local_qc().map(|qc| qc.block_number),
            Some(certified_root.header.block_number)
        );
        runner.record_peer_sync_status(
            2,
            certified_root.header.block_number + 2,
            canonical_hash(&"ahead-tip"),
            Some(certified_root.block_hash),
            certified_root.header.block_number,
            vec![SyncQcAnchor {
                block_hash: certified_root.block_hash,
                block_number: certified_root.header.block_number,
            }],
        );

        runner.announce_certified_progress().unwrap();

        let deadline = tokio::time::Instant::now() + Duration::from_millis(250);
        let mut saw_sync_request = false;
        while tokio::time::Instant::now() < deadline {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if let Ok(message) =
                tokio::time::timeout(remaining, recv_protocol_message(&mut peer_events)).await
            {
                match message {
                    ProtocolMessage::SyncRequest {
                        requester_id,
                        known_height,
                        known_tip_hash,
                    } => {
                        assert_eq!(requester_id, 1);
                        assert_eq!(known_height, certified_root.header.block_number);
                        assert_eq!(known_tip_hash, certified_root.block_hash);
                        saw_sync_request = true;
                        break;
                    }
                    ProtocolMessage::SyncStatus { .. }
                    | ProtocolMessage::SyncBlocks { .. }
                    | ProtocolMessage::SyncResponse { .. }
                    | ProtocolMessage::CertifiedSyncRequest(_)
                    | ProtocolMessage::CertifiedSyncResponse(_)
                    | ProtocolMessage::BlockProposal(_)
                    | ProtocolMessage::ProposalVote(_)
                    | ProtocolMessage::QuorumCertificate(_)
                    | ProtocolMessage::HeartbeatPulse(_)
                    | ProtocolMessage::ReceiptFetch { .. } => {}
                    other => panic!(
                        "unexpected message while waiting for certified-progress sync request: {other:?}"
                    ),
                }
            } else {
                break;
            }
        }

        assert!(
            saw_sync_request,
            "expected proactive sync request after certified progress"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn pending_local_v2_proposal_is_broadcast_after_qc_lock() {
        let genesis = sample_genesis();
        let local_address = reserve_local_address();
        let peer_address = reserve_local_address();
        let peer = PeerConfig {
            validator_id: 2,
            address: peer_address.clone(),
        };
        let config = sample_node_config(1, local_address, vec![peer], "pending-proposal-broadcast");
        let mut runner = build_test_runner(1, config, genesis.clone()).await;
        let (_, _, _, mut peer_events) =
            spawn_test_network(2, peer_address, Vec::new(), &genesis).await;
        let certified_root = first_valid_block(&genesis);

        assert_eq!(
            runner
                .accept_block(certified_root.clone(), true)
                .await
                .unwrap(),
            BlockAcceptance::Accepted
        );
        assert!(
            runner
                .import_proposal_vote(signed_proposal_vote(
                    runner.crypto.as_ref(),
                    2,
                    &certified_root
                ))
                .unwrap()
        );
        assert!(
            runner
                .import_proposal_vote(signed_proposal_vote(
                    runner.crypto.as_ref(),
                    3,
                    &certified_root
                ))
                .unwrap()
        );

        let slot = ((certified_root.header.slot + 1)..(certified_root.header.slot + 50))
            .find(|slot| runner.consensus.proposer_for_slot(*slot) == 1)
            .unwrap();
        let proposal_epoch = runner.consensus.epoch_for_slot(slot);
        runner.last_processed_slot = Some(genesis.slots_per_epoch * proposal_epoch);

        runner
            .maybe_propose_block(slot, proposal_epoch)
            .await
            .unwrap();

        assert_eq!(runner.blocks.len(), 1);
        assert_eq!(runner.ledger.snapshot().tip_hash, certified_root.block_hash);
        let pending = runner
            .preferred_pending_child_for_certified_head()
            .expect("pending child should stay pending until certified");

        loop {
            match recv_protocol_message(&mut peer_events).await {
                ProtocolMessage::BlockProposal(block) => {
                    assert_eq!(block.block_hash, pending.block_hash);
                    break;
                }
                ProtocolMessage::ProposalVote(_)
                | ProtocolMessage::QuorumCertificate(_)
                | ProtocolMessage::SyncStatus { .. }
                | ProtocolMessage::SyncResponse { .. }
                | ProtocolMessage::HeartbeatPulse(_)
                | ProtocolMessage::ReceiptFetch { .. } => {}
                other => panic!("expected pending block proposal broadcast, got {other:?}"),
            }
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn proposer_rebroadcasts_existing_pending_child_instead_of_creating_sibling() {
        let genesis = sample_genesis();
        let local_address = reserve_local_address();
        let peer_address = reserve_local_address();
        let peer = PeerConfig {
            validator_id: 2,
            address: peer_address.clone(),
        };
        let config = sample_node_config(1, local_address, vec![peer], "pending-child-rebroadcast");
        let mut runner = build_test_runner(1, config, genesis.clone()).await;
        let (_, _, _, mut peer_events) =
            spawn_test_network(2, peer_address, Vec::new(), &genesis).await;
        let certified_root = first_valid_block(&genesis);
        let crypto = DeterministicCryptoBackend::from_genesis(&genesis);
        let consensus = ConsensusEngine::new(genesis.clone());

        assert_eq!(
            runner
                .accept_block(certified_root.clone(), true)
                .await
                .unwrap(),
            BlockAcceptance::Accepted
        );
        assert!(
            runner
                .import_proposal_vote(signed_proposal_vote(
                    runner.crypto.as_ref(),
                    2,
                    &certified_root
                ))
                .unwrap()
        );
        assert!(
            runner
                .import_proposal_vote(signed_proposal_vote(
                    runner.crypto.as_ref(),
                    3,
                    &certified_root
                ))
                .unwrap()
        );
        let mut root_ledger = LedgerState::from_genesis(&genesis);
        root_ledger.apply_block(&certified_root, &crypto).unwrap();
        let slot = ((certified_root.header.slot + 1)..(certified_root.header.slot + 50))
            .find(|slot| consensus.proposer_for_slot(*slot) == 3)
            .unwrap();
        let epoch = consensus.epoch_for_slot(slot);
        let transactions = Vec::new();
        let commitment: Option<TopologyCommitment> = None;
        let header = BlockHeader {
            block_number: 2,
            parent_hash: certified_root.block_hash,
            slot,
            epoch,
            proposer_id: 3,
            timestamp_unix_millis: now_unix_millis(),
            state_root: root_ledger.state_root(),
            transactions_root: canonical_hash(&transactions),
            topology_root: empty_hash(),
        };
        let block_hash = canonical_hash(&(header.clone(), &transactions, &commitment));
        let pending_child = Block {
            header,
            transactions,
            commitment,
            commitment_receipts: Vec::new(),
            signature: crypto.sign(3, &block_hash).unwrap(),
            block_hash,
        };
        assert_eq!(
            runner
                .accept_block(pending_child.clone(), false)
                .await
                .unwrap(),
            BlockAcceptance::Orphan
        );
        assert_eq!(runner.orphan_blocks.len(), 0);
        assert_eq!(
            runner
                .preferred_pending_child_for_certified_head()
                .map(|block| block.block_hash),
            Some(pending_child.block_hash)
        );

        let slot = ((pending_child.header.slot + 1)..(pending_child.header.slot + 50))
            .find(|slot| runner.consensus.proposer_for_slot(*slot) == 1)
            .unwrap();
        let proposal_epoch = runner.consensus.epoch_for_slot(slot);
        runner.last_processed_slot = Some(genesis.slots_per_epoch * proposal_epoch);

        runner
            .maybe_propose_block(slot, proposal_epoch)
            .await
            .unwrap();

        assert_eq!(runner.orphan_blocks.len(), 0);
        assert_eq!(
            runner
                .preferred_pending_child_for_certified_head()
                .map(|block| block.block_hash),
            Some(pending_child.block_hash)
        );

        loop {
            match recv_protocol_message(&mut peer_events).await {
                ProtocolMessage::BlockProposal(block) => {
                    assert_eq!(block.block_hash, pending_child.block_hash);
                    break;
                }
                ProtocolMessage::ProposalVote(_)
                | ProtocolMessage::QuorumCertificate(_)
                | ProtocolMessage::SyncStatus { .. }
                | ProtocolMessage::SyncResponse { .. }
                | ProtocolMessage::HeartbeatPulse(_)
                | ProtocolMessage::ReceiptFetch { .. } => {}
                other => panic!("expected pending child rebroadcast, got {other:?}"),
            }
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn invalid_service_attestation_signature_is_rejected() {
        let genesis = sample_genesis();
        let config = sample_node_config(1, reserve_local_address(), Vec::new(), "bad-attestation");
        let mut runner = build_test_runner(1, config, genesis).await;
        let mut attestation =
            signed_service_attestation(runner.crypto.as_ref(), 1, 2, 1, ServiceCounters::default());
        attestation.signature =
            TypedSignature::single(SignatureScheme::DevDeterministic, vec![9, 9, 9]);

        let error = runner.import_service_attestation(attestation).unwrap_err();

        assert!(
            error
                .to_string()
                .contains("invalid service attestation signature")
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn invalid_service_aggregate_payload_is_rejected() {
        let genesis = sample_genesis();
        let config = sample_node_config(1, reserve_local_address(), Vec::new(), "bad-aggregate");
        let mut runner = build_test_runner(1, config, genesis).await;
        let mut aggregate = service_aggregate_for_subject(
            runner.crypto.as_ref(),
            &runner.consensus,
            1,
            1,
            ServiceCounters::default(),
        );
        aggregate.attestation_root = [7u8; 32];

        let error = runner.import_service_aggregate(aggregate).unwrap_err();

        assert!(error.to_string().contains("aggregate payload is malformed"));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn legacy_mode_ignores_and_rejects_v2_service_evidence() {
        let genesis = sample_genesis();
        let mut config =
            sample_node_config(1, reserve_local_address(), Vec::new(), "legacy-v2-ignore");
        config.feature_flags.consensus_v2 = false;
        let mut runner = build_test_runner(1, config, genesis).await;
        let committee_member_id = runner.consensus.service_committee_for(1, 1)[0];
        let attestation = signed_service_attestation(
            runner.crypto.as_ref(),
            1,
            committee_member_id,
            1,
            ServiceCounters::default(),
        );
        let aggregate = service_aggregate_for_subject(
            runner.crypto.as_ref(),
            &runner.consensus,
            1,
            1,
            ServiceCounters::default(),
        );

        runner
            .handle_network_event(NetworkEvent::Received {
                from_validator_id: committee_member_id,
                payload: ProtocolMessage::ServiceAttestation(attestation.clone()),
                bytes: 0,
            })
            .await
            .unwrap();
        runner
            .handle_network_event(NetworkEvent::Received {
                from_validator_id: committee_member_id,
                payload: ProtocolMessage::ServiceAggregate(aggregate.clone()),
                bytes: 0,
            })
            .await
            .unwrap();

        assert!(runner.service_attestations.is_empty());
        assert!(runner.service_aggregates.is_empty());
        assert!(
            runner
                .import_service_attestation(attestation)
                .unwrap_err()
                .to_string()
                .contains("require consensus_v2")
        );
        assert!(
            runner
                .import_service_aggregate(aggregate)
                .unwrap_err()
                .to_string()
                .contains("require consensus_v2")
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn restore_service_evidence_rebuilds_state_without_reappending_files() {
        let genesis = sample_genesis();
        let config = sample_node_config(1, reserve_local_address(), Vec::new(), "restore-v2");
        let mut runner = build_test_runner(1, config, genesis).await;
        let aggregate = service_aggregate_for_subject(
            runner.crypto.as_ref(),
            &runner.consensus,
            1,
            1,
            ServiceCounters {
                uptime_windows: 4,
                total_windows: 4,
                timely_deliveries: 4,
                expected_deliveries: 4,
                distinct_peers: 2,
                expected_peers: 2,
                failed_sessions: 0,
                invalid_receipts: 0,
            },
        );
        for attestation in aggregate.attestations.clone() {
            runner.import_service_attestation(attestation).unwrap();
        }
        runner.import_service_aggregate(aggregate.clone()).unwrap();

        let loaded_attestations = runner.storage.load_service_attestations().unwrap();
        let loaded_aggregates = runner.storage.load_service_aggregates().unwrap();
        let attestation_lines_before =
            fs::read_to_string(&runner.storage.service_attestations_path)
                .unwrap()
                .lines()
                .count();
        let aggregate_lines_before = fs::read_to_string(&runner.storage.service_aggregates_path)
            .unwrap()
            .lines()
            .count();

        runner.service_attestations.clear();
        runner.service_aggregates.clear();
        runner.seen_service_attestations.clear();
        runner.seen_service_aggregates.clear();

        runner
            .restore_service_evidence(loaded_attestations, loaded_aggregates)
            .unwrap();

        assert_eq!(
            fs::read_to_string(&runner.storage.service_attestations_path)
                .unwrap()
                .lines()
                .count(),
            attestation_lines_before
        );
        assert_eq!(
            fs::read_to_string(&runner.storage.service_aggregates_path)
                .unwrap()
                .lines()
                .count(),
            aggregate_lines_before
        );
        assert_eq!(
            runner.service_attestations.get(&(1, 1)).unwrap().len(),
            aggregate.attestations.len()
        );
        assert_eq!(runner.service_aggregates.get(&(1, 1)).unwrap(), &aggregate);
    }

    #[test]
    fn receipt_event_hash_ignores_non_identity_fields() {
        let mut first = sample_receipt();
        let mut second = sample_receipt();
        second.signature = TypedSignature::single(SignatureScheme::DevDeterministic, vec![9, 9, 9]);
        second.transcript_digest = [9u8; 32];
        second.latency_bucket_ms = 900;
        second.byte_count_bucket = 42;

        assert_eq!(receipt_event_hash(&first), receipt_event_hash(&second));
        assert_ne!(canonical_hash(&first), canonical_hash(&second));

        first.sequence_number += 1;
        assert_ne!(receipt_event_hash(&first), receipt_event_hash(&second));
    }

    #[test]
    fn storage_ignores_truncated_trailing_jsonl_entry() {
        let unique_dir =
            std::env::temp_dir().join(format!("entangrid-node-test-{}", now_unix_millis()));
        let storage = Storage {
            data_dir: unique_dir.clone(),
            inbox_dir: unique_dir.join("inbox"),
            processed_dir: unique_dir.join("processed"),
            blocks_path: unique_dir.join(BLOCKS_FILE),
            receipts_path: unique_dir.join(RECEIPTS_FILE),
            service_attestations_path: unique_dir.join(SERVICE_ATTESTATIONS_FILE),
            service_aggregates_path: unique_dir.join(SERVICE_AGGREGATES_FILE),
            orphan_path: unique_dir.join(ORPHANS_FILE),
            snapshot_path: unique_dir.join(SNAPSHOT_FILE),
            log_path: unique_dir.join("events.log"),
            metrics_path: unique_dir.join("metrics.json"),
        };
        storage.init().unwrap();

        let valid = serde_json::to_string(&sample_receipt()).unwrap();
        let truncated = "{\"epoch\":1,\"slot\":2,\"source_validator_id\":1,\"destination_validator_id\":2,\"witness_validator_id\":3,\"message_class\":\"Transaction\",\"transcript_digest\":[1,2";
        fs::write(&storage.receipts_path, format!("{valid}\n{truncated}")).unwrap();

        let receipts = storage
            .load_json_lines::<RelayReceipt>(&storage.receipts_path)
            .unwrap();
        assert_eq!(receipts.len(), 1);
    }

    #[test]
    fn validate_chain_snapshot_rejects_invalid_receipts() {
        let genesis = sample_genesis();
        let consensus = ConsensusEngine::new(genesis.clone());
        let crypto = DeterministicCryptoBackend::from_genesis(&genesis);
        let mut receipt = sample_receipt();
        receipt.epoch = 0;
        receipt.slot = 0;
        receipt.source_validator_id = 1;
        receipt.destination_validator_id = 2;
        receipt.witness_validator_id = 2;
        receipt.signature =
            TypedSignature::single(SignatureScheme::DevDeterministic, vec![9, 9, 9]);
        let chain = ChainSnapshot {
            snapshot: LedgerState::from_genesis(&genesis).snapshot().clone(),
            blocks: Vec::new(),
            receipts: vec![receipt],
        };

        let error = validate_chain_snapshot(&genesis, &consensus, &crypto, &chain).unwrap_err();
        assert!(error.to_string().contains("invalid receipt signature"));
    }

    #[test]
    fn validate_chain_snapshot_rejects_invalid_block_schedule() {
        let genesis = sample_genesis();
        let consensus = ConsensusEngine::new(genesis.clone());
        let crypto = DeterministicCryptoBackend::from_genesis(&genesis);
        let mut block = first_valid_block(&genesis);
        let wrong_proposer = 2;
        block.header.proposer_id = wrong_proposer;
        block.block_hash =
            canonical_hash(&(block.header.clone(), &block.transactions, &block.commitment));
        block.signature = crypto.sign(wrong_proposer, &block.block_hash).unwrap();
        let chain = ChainSnapshot {
            snapshot: LedgerState::from_genesis(&genesis).snapshot().clone(),
            blocks: vec![block],
            receipts: Vec::new(),
        };

        let error = validate_chain_snapshot(&genesis, &consensus, &crypto, &chain).unwrap_err();
        assert!(error.to_string().contains("unexpected proposer"));
    }

    #[test]
    fn build_chain_segment_returns_missing_blocks_for_matching_tip() {
        let genesis = sample_genesis();
        let block = first_valid_block(&genesis);
        let snapshot = LedgerState::replay_blocks(
            &genesis,
            &[block.clone()],
            &DeterministicCryptoBackend::from_genesis(&genesis),
        )
        .unwrap()
        .snapshot()
        .clone();

        let segment = build_chain_segment_from_chain(
            &snapshot,
            &[block.clone()],
            &[],
            4,
            0,
            empty_hash(),
            MAX_PREFERRED_INCREMENTAL_SYNC_BLOCKS,
        )
        .expect("matching genesis tip should produce a segment");

        assert_eq!(segment.base_height, 0);
        assert_eq!(segment.base_tip_hash, empty_hash());
        assert_eq!(segment.blocks, vec![block]);
        assert!(segment.receipts.is_empty());
        assert_eq!(segment.target_snapshot, snapshot);
    }

    #[test]
    fn build_chain_segment_rejects_mismatched_tip() {
        let genesis = sample_genesis();
        let block = first_valid_block(&genesis);
        let snapshot = LedgerState::replay_blocks(
            &genesis,
            &[block.clone()],
            &DeterministicCryptoBackend::from_genesis(&genesis),
        )
        .unwrap()
        .snapshot()
        .clone();

        let segment = build_chain_segment_from_chain(
            &snapshot,
            &[block],
            &[],
            4,
            0,
            [9u8; 32],
            MAX_PREFERRED_INCREMENTAL_SYNC_BLOCKS,
        );
        assert!(segment.is_none());
    }

    #[test]
    fn build_chain_segment_prefers_full_snapshot_for_large_gap() {
        let genesis = sample_genesis();
        let block = first_valid_block(&genesis);
        let snapshot = LedgerState::replay_blocks(
            &genesis,
            std::slice::from_ref(&block),
            &DeterministicCryptoBackend::from_genesis(&genesis),
        )
        .unwrap()
        .snapshot()
        .clone();
        let blocks = vec![block; MAX_PREFERRED_INCREMENTAL_SYNC_BLOCKS + 1];

        let segment = build_chain_segment_from_chain(
            &snapshot,
            &blocks,
            &[],
            4,
            0,
            empty_hash(),
            MAX_PREFERRED_INCREMENTAL_SYNC_BLOCKS,
        );
        assert!(segment.is_none());
    }

    #[test]
    fn build_chain_segment_allows_large_gap_when_anchor_requests_full_incremental_limit() {
        let genesis = sample_genesis();
        let block = first_valid_block(&genesis);
        let snapshot = LedgerState::replay_blocks(
            &genesis,
            std::slice::from_ref(&block),
            &DeterministicCryptoBackend::from_genesis(&genesis),
        )
        .unwrap()
        .snapshot()
        .clone();
        let blocks = vec![block; MAX_PREFERRED_INCREMENTAL_SYNC_BLOCKS + 1];

        let segment = build_chain_segment_from_chain(
            &snapshot,
            &blocks,
            &[],
            4,
            0,
            empty_hash(),
            MAX_INCREMENTAL_SYNC_BLOCKS,
        );
        assert!(segment.is_some());
    }

    #[test]
    fn sync_repair_escalates_to_full_snapshot_after_repeated_incremental_failures() {
        let tip_hash = [7u8; 32];

        assert!(!sync_repair_should_force_full_snapshot(0));
        assert!(!sync_repair_should_force_full_snapshot(
            INCREMENTAL_SYNC_FAILURES_BEFORE_FULL_SNAPSHOT - 1
        ));
        assert!(sync_repair_should_force_full_snapshot(
            INCREMENTAL_SYNC_FAILURES_BEFORE_FULL_SNAPSHOT
        ));

        assert_eq!(
            sync_request_known_state(42, tip_hash, false),
            (42, tip_hash)
        );
        assert_eq!(
            sync_request_known_state(42, tip_hash, true),
            (u64::MAX, empty_hash())
        );
        assert!(is_force_full_snapshot_hint(u64::MAX, empty_hash()));
        assert!(!is_force_full_snapshot_hint(42, tip_hash));
        assert!(!should_record_peer_sync_status(u64::MAX, empty_hash()));
        assert!(should_record_peer_sync_status(42, tip_hash));
    }

    #[test]
    fn receipt_fetch_plan_scales_with_peer_and_validator_count() {
        let peer_ids = BTreeSet::from([2, 3, 4]);
        let plan = build_receipt_fetch_plan(1, &peer_ids);

        assert_eq!(plan, vec![2, 3, 4]);
    }

    #[test]
    fn snapshot_fork_choice_prefers_height_then_slot_then_tip_hash() {
        let local = StateSnapshot {
            balances: BTreeMap::new(),
            nonces: BTreeMap::new(),
            tip_hash: [1u8; 32],
            height: 10,
            last_slot: 20,
        };
        let higher = StateSnapshot {
            height: 11,
            ..local.clone()
        };
        let later_slot = StateSnapshot {
            tip_hash: [2u8; 32],
            last_slot: 21,
            ..local.clone()
        };
        let higher_tip_hash = StateSnapshot {
            tip_hash: [3u8; 32],
            ..local.clone()
        };

        assert!(should_adopt_snapshot(&local, &higher));
        assert!(should_adopt_snapshot(&local, &later_slot));
        assert!(should_adopt_snapshot(&local, &higher_tip_hash));
        assert!(!should_adopt_snapshot(&later_slot, &local));
    }

    #[test]
    fn sync_request_throttle_only_blocks_repeated_identical_known_state() {
        let now = now_unix_millis();
        let repeated = ServedSyncRequest {
            served_at_unix_millis: now,
            known_height: 10,
            known_tip_hash: [1u8; 32],
            served_local_height: 20,
            served_local_tip_hash: [3u8; 32],
            served_local_highest_qc_height: 20,
            served_local_highest_qc_hash: Some([3u8; 32]),
        };
        let fresh_height = ServedSyncRequest {
            known_height: 11,
            ..repeated
        };
        let fresh_tip = ServedSyncRequest {
            known_tip_hash: [2u8; 32],
            ..repeated
        };

        assert!(sync_request_should_throttle(
            repeated,
            now + 10,
            repeated.known_height,
            repeated.known_tip_hash,
            repeated.served_local_height,
            repeated.served_local_tip_hash,
            repeated.served_local_highest_qc_height,
            repeated.served_local_highest_qc_hash,
        ));
        assert!(!sync_request_should_throttle(
            fresh_height,
            now + 10,
            repeated.known_height,
            repeated.known_tip_hash,
            repeated.served_local_height,
            repeated.served_local_tip_hash,
            repeated.served_local_highest_qc_height,
            repeated.served_local_highest_qc_hash,
        ));
        assert!(!sync_request_should_throttle(
            fresh_tip,
            now + 10,
            repeated.known_height,
            repeated.known_tip_hash,
            repeated.served_local_height,
            repeated.served_local_tip_hash,
            repeated.served_local_highest_qc_height,
            repeated.served_local_highest_qc_hash,
        ));
        assert!(!sync_request_should_throttle(
            repeated,
            now + SYNC_REQUEST_COOLDOWN_MILLIS + 1,
            repeated.known_height,
            repeated.known_tip_hash,
            repeated.served_local_height,
            repeated.served_local_tip_hash,
            repeated.served_local_highest_qc_height,
            repeated.served_local_highest_qc_hash,
        ));
        assert!(!sync_request_should_throttle(
            repeated,
            now + 10,
            u64::MAX,
            empty_hash(),
            repeated.served_local_height,
            repeated.served_local_tip_hash,
            repeated.served_local_highest_qc_height,
            repeated.served_local_highest_qc_hash,
        ));
        assert!(!sync_request_should_throttle(
            repeated,
            now + 10,
            repeated.known_height,
            repeated.known_tip_hash,
            21,
            [4u8; 32],
            21,
            Some([4u8; 32]),
        ));
        assert!(!sync_request_should_throttle(
            repeated,
            now + 10,
            repeated.served_local_highest_qc_height,
            repeated.served_local_highest_qc_hash.unwrap(),
            21,
            [4u8; 32],
            repeated.served_local_highest_qc_height,
            repeated.served_local_highest_qc_hash,
        ));
    }

    fn valid_block_chain_for_slots(genesis: &GenesisConfig, slots: &[u64]) -> Vec<Block> {
        let crypto = DeterministicCryptoBackend::from_genesis(genesis);
        let consensus = ConsensusEngine::new(genesis.clone());
        let mut ledger = LedgerState::from_genesis(genesis);
        let mut blocks = Vec::with_capacity(slots.len());
        let mut parent_hash = empty_hash();
        for (index, slot) in slots.iter().copied().enumerate() {
            let proposer_id = consensus.proposer_for_slot(slot);
            let epoch = consensus.epoch_for_slot(slot);
            let transactions: Vec<SignedTransaction> = Vec::new();
            let commitment: Option<TopologyCommitment> = None;
            let header = BlockHeader {
                block_number: (index + 1) as u64,
                parent_hash,
                slot,
                epoch,
                proposer_id,
                timestamp_unix_millis: now_unix_millis(),
                state_root: ledger.state_root(),
                transactions_root: canonical_hash(&transactions),
                topology_root: empty_hash(),
            };
            let block_hash = canonical_hash(&(header.clone(), &transactions, &commitment));
            let block = Block {
                header,
                transactions,
                commitment,
                commitment_receipts: Vec::new(),
                signature: crypto.sign(proposer_id, &block_hash).unwrap(),
                block_hash,
            };
            let mut next_ledger = ledger.clone();
            next_ledger.apply_block(&block, &crypto).unwrap();
            ledger = next_ledger;
            parent_hash = block.block_hash;
            blocks.push(block);
        }
        blocks
    }

    #[test]
    fn align_chain_segment_trims_already_applied_prefix() {
        let genesis = sample_genesis();
        let blocks = valid_block_chain_for_slots(&genesis, &[0, 1, 2, 3]);
        let local_blocks = blocks[..3].to_vec();
        let local_snapshot = LedgerState::replay_blocks(
            &genesis,
            &local_blocks,
            &DeterministicCryptoBackend::from_genesis(&genesis),
        )
        .unwrap()
        .snapshot()
        .clone();
        let target_snapshot = LedgerState::replay_blocks(
            &genesis,
            &blocks,
            &DeterministicCryptoBackend::from_genesis(&genesis),
        )
        .unwrap()
        .snapshot()
        .clone();
        let segment = ChainSegment {
            base_height: 2,
            base_tip_hash: blocks[1].block_hash,
            target_snapshot,
            blocks: blocks[2..].to_vec(),
            receipts: Vec::new(),
            proposal_votes: Vec::new(),
        };

        let aligned =
            align_chain_segment_to_local_chain(&local_blocks, &local_snapshot, segment).unwrap();
        assert_eq!(aligned.base_height, 3);
        assert_eq!(aligned.base_tip_hash, blocks[2].block_hash);
        assert_eq!(aligned.blocks, vec![blocks[3].clone()]);
    }

    #[test]
    fn align_chain_segment_rejects_divergent_local_suffix() {
        let genesis = sample_genesis();
        let local_blocks = valid_block_chain_for_slots(&genesis, &[0, 1, 4]);
        let remote_blocks = valid_block_chain_for_slots(&genesis, &[0, 1, 2, 3]);
        let local_snapshot = LedgerState::replay_blocks(
            &genesis,
            &local_blocks,
            &DeterministicCryptoBackend::from_genesis(&genesis),
        )
        .unwrap()
        .snapshot()
        .clone();
        let target_snapshot = LedgerState::replay_blocks(
            &genesis,
            &remote_blocks,
            &DeterministicCryptoBackend::from_genesis(&genesis),
        )
        .unwrap()
        .snapshot()
        .clone();
        let segment = ChainSegment {
            base_height: 2,
            base_tip_hash: remote_blocks[1].block_hash,
            target_snapshot,
            blocks: remote_blocks[2..].to_vec(),
            receipts: Vec::new(),
            proposal_votes: Vec::new(),
        };

        let error = align_chain_segment_to_local_chain(&local_blocks, &local_snapshot, segment)
            .unwrap_err();
        assert!(error.to_string().contains("incremental sync"));
    }

    #[test]
    fn failed_session_penalty_requires_peer_to_be_known_live() {
        let consensus = ConsensusEngine::new(sample_genesis());
        let known_live_peers = BTreeSet::new();
        let observed_successful_sessions = BTreeSet::new();

        assert!(!should_record_failed_session(
            &consensus,
            &known_live_peers,
            &observed_successful_sessions,
            0,
            1,
            2,
        ));
    }

    #[test]
    fn failed_session_penalty_clears_after_successful_session() {
        let consensus = ConsensusEngine::new(sample_genesis());
        let known_live_peers = BTreeSet::from([2]);
        let observed_successful_sessions = BTreeSet::from([(0, 1, 2)]);

        assert!(!should_record_failed_session(
            &consensus,
            &known_live_peers,
            &observed_successful_sessions,
            0,
            1,
            2,
        ));
    }

    #[test]
    fn failed_session_penalty_records_for_live_assigned_peer_without_success() {
        let consensus = ConsensusEngine::new(sample_genesis());
        let known_live_peers = BTreeSet::from([2]);
        let observed_successful_sessions = BTreeSet::new();

        assert!(should_record_failed_session(
            &consensus,
            &known_live_peers,
            &observed_successful_sessions,
            0,
            1,
            2,
        ));
    }

    #[test]
    fn remote_blocks_do_not_enforce_local_service_gating() {
        assert!(should_enforce_service_gating_for_block(true, true, 7, 3));
        assert!(!should_enforce_service_gating_for_block(true, false, 7, 3));
        assert!(!should_enforce_service_gating_for_block(true, true, 2, 3));
        assert!(!should_enforce_service_gating_for_block(false, true, 7, 3));
    }

    #[test]
    fn control_plane_sessions_do_not_count_toward_service_penalties() {
        assert!(should_track_outbound_service_session(true, true));
        assert!(!should_track_outbound_service_session(true, false));
        assert!(!should_track_outbound_service_session(false, true));
        assert!(!should_track_outbound_service_session(false, false));
        assert!(should_record_failed_service_session(
            true,
            true,
            NetworkFailureKind::FaultInjected
        ));
        assert!(!should_record_failed_service_session(
            true,
            true,
            NetworkFailureKind::Transport
        ));
        assert!(!should_record_failed_service_session(
            true,
            false,
            NetworkFailureKind::FaultInjected
        ));
    }

    #[test]
    fn peer_message_window_limits_and_resets_sync_control() {
        let mut window = PeerMessageWindow::default();
        for _ in 0..MAX_SYNC_CONTROL_MESSAGES_PER_WINDOW {
            assert!(allow_peer_message_in_window(
                &mut window,
                PeerMessageClass::SyncControl,
                1_000,
            ));
        }
        assert!(!allow_peer_message_in_window(
            &mut window,
            PeerMessageClass::SyncControl,
            1_000,
        ));
        assert!(allow_peer_message_in_window(
            &mut window,
            PeerMessageClass::SyncControl,
            1_000 + PEER_MESSAGE_WINDOW_MILLIS,
        ));
    }

    #[test]
    fn classify_peer_message_only_limits_spammable_gossip() {
        let tx = first_valid_block(&sample_genesis()).transactions[0].clone();
        assert_eq!(
            classify_peer_message(&ProtocolMessage::TransactionBroadcast(tx)),
            Some(PeerMessageClass::TransactionGossip)
        );
        assert_eq!(
            classify_peer_message(&ProtocolMessage::SyncRequest {
                requester_id: 1,
                known_height: 0,
                known_tip_hash: empty_hash(),
            }),
            Some(PeerMessageClass::SyncControl)
        );
        assert_eq!(
            classify_peer_message(&ProtocolMessage::BlockProposal(first_valid_block(
                &sample_genesis()
            ))),
            None
        );
    }
}
