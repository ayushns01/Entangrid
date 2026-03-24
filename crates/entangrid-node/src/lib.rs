use std::{
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
use entangrid_crypto::{CryptoBackend, DeterministicCryptoBackend};
use entangrid_ledger::LedgerState;
use entangrid_network::{NetworkEvent, NetworkFailureKind, NetworkHandle, spawn_network};
use entangrid_types::{
    Block, BlockHeader, ChainSegment, ChainSnapshot, Epoch, EventLogEntry, GenesisConfig,
    HashBytes, HeartbeatPulse, MessageClass, NodeConfig, NodeMetrics, PeerConfig, ProtocolMessage,
    RelayReceipt, ServiceCounters, SignedTransaction, StateSnapshot, TopologyCommitment,
    ValidatorId, canonical_hash, empty_hash, now_unix_millis,
};
use tokio::{sync::mpsc, time::MissedTickBehavior};
use tracing::info;

const SNAPSHOT_FILE: &str = "state_snapshot.json";
const BLOCKS_FILE: &str = "blocks.jsonl";
const RECEIPTS_FILE: &str = "receipts.jsonl";
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
    let crypto: Arc<dyn CryptoBackend> =
        Arc::new(DeterministicCryptoBackend::from_genesis(&genesis));
    let consensus = ConsensusEngine::new(genesis.clone());
    let storage = Storage::new(&config)?;
    storage.init()?;
    let loaded_blocks = storage.load_blocks()?;
    let loaded_receipts = storage.load_receipts()?;
    let snapshot = storage.load_snapshot()?;
    let ledger = match snapshot {
        Some(snapshot) => LedgerState::from_snapshot(snapshot),
        None => LedgerState::replay_blocks(&genesis, &loaded_blocks, crypto.as_ref())
            .unwrap_or_else(|_| LedgerState::from_genesis(&genesis)),
    };

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
        mempool: BTreeMap::new(),
        seen_transactions: BTreeSet::new(),
        seen_blocks: BTreeSet::new(),
        seen_receipts: BTreeSet::new(),
        seen_receipt_events: BTreeSet::new(),
        last_processed_slot: None,
        last_logged_epoch: None,
        last_heartbeat_slot: None,
        last_proposed_slot: None,
        latest_service_scores: BTreeMap::new(),
        latest_service_counters: BTreeMap::new(),
        failed_sessions: 0,
        invalid_receipts: 0,
        observed_failed_sessions: BTreeSet::new(),
        observed_successful_sessions: BTreeSet::new(),
        observed_invalid_receipts: BTreeMap::new(),
        known_live_peers: BTreeSet::new(),
        last_sync_request_served: BTreeMap::new(),
        peer_sync_status: BTreeMap::new(),
        peer_sync_repair_failures: BTreeMap::new(),
        peer_message_windows: BTreeMap::new(),
    };
    runner.rebuild_seen_sets();
    runner.run().await
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
    mempool: BTreeMap<HashBytes, SignedTransaction>,
    seen_transactions: BTreeSet<HashBytes>,
    seen_blocks: BTreeSet<HashBytes>,
    seen_receipts: BTreeSet<HashBytes>,
    seen_receipt_events: BTreeSet<HashBytes>,
    last_processed_slot: Option<u64>,
    last_logged_epoch: Option<Epoch>,
    last_heartbeat_slot: Option<u64>,
    last_proposed_slot: Option<u64>,
    latest_service_scores: BTreeMap<ValidatorId, f64>,
    latest_service_counters: BTreeMap<ValidatorId, ServiceCounters>,
    failed_sessions: u64,
    invalid_receipts: u64,
    observed_failed_sessions: BTreeSet<(Epoch, ValidatorId, ValidatorId)>,
    observed_successful_sessions: BTreeSet<(Epoch, ValidatorId, ValidatorId)>,
    observed_invalid_receipts: BTreeMap<(Epoch, ValidatorId), u64>,
    known_live_peers: BTreeSet<ValidatorId>,
    last_sync_request_served: BTreeMap<ValidatorId, ServedSyncRequest>,
    peer_sync_status: BTreeMap<ValidatorId, (u64, HashBytes)>,
    peer_sync_repair_failures: BTreeMap<ValidatorId, u64>,
    peer_message_windows: BTreeMap<ValidatorId, PeerMessageWindow>,
}

#[derive(Clone, Copy, Debug)]
struct ServedSyncRequest {
    served_at_unix_millis: u64,
    known_height: u64,
    known_tip_hash: HashBytes,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum BlockAcceptance {
    Accepted,
    Duplicate,
    Orphan,
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
                        self.broadcast_sync_request()?;
                        self.broadcast_sync_status()?;
                        self.push_sync_to_stale_peers()?;
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
        let slot = self.consensus.slot_at(now);
        if self.last_processed_slot == Some(slot) {
            return Ok(());
        }
        self.last_processed_slot = Some(slot);
        let epoch = self.consensus.epoch_for_slot(slot);
        self.refresh_service_scores();
        self.update_metrics(|metrics| {
            metrics.current_slot = slot;
            metrics.current_epoch = epoch;
        });

        if self.last_logged_epoch != Some(epoch) {
            self.last_logged_epoch = Some(epoch);
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
        }

        self.broadcast_heartbeat(slot, epoch)?;
        self.maybe_propose_block(slot, epoch).await?;
        self.try_promote_orphans().await?;
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
                let score = self.service_score_for_validator(self.config.validator_id);
                let counters = self.local_service_counters();
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

        self.last_proposed_slot = Some(slot);
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

        if matches!(
            self.accept_block(block.clone(), true).await?,
            BlockAcceptance::Accepted
        ) {
            self.try_promote_orphans().await?;
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
            ProtocolMessage::SyncStatus {
                validator_id,
                height,
                tip_hash,
            } => {
                self.record_peer_sync_status(validator_id, height, tip_hash);
                let local_height = self.ledger.block_height();
                let local_tip_hash = self.ledger.snapshot().tip_hash;
                if height > local_height {
                    if let Err(error) = self.request_sync_from(validator_id) {
                        self.log_event(
                            "sync-request-failed",
                            format!("to {validator_id} detail {error}"),
                        )?;
                    }
                } else if height < local_height || tip_hash != local_tip_hash {
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
                self.record_peer_sync_status(requester_id, known_height, known_tip_hash);
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
                    self.send_best_sync_to(requester_id, known_height, known_tip_hash)?;
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
                );
                let local_snapshot = self.ledger.snapshot().clone();
                if !should_adopt_snapshot(&local_snapshot, &chain.target_snapshot) {
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
            ProtocolMessage::RelayReceipt(receipt) => {
                if self.store_receipt(receipt.clone())? {
                    self.network
                        .broadcast(&self.config.peers, ProtocolMessage::RelayReceipt(receipt))?;
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
            signature: Vec::new(),
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
        if self.seen_blocks.contains(&block.block_hash) {
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
            return Ok(BlockAcceptance::Orphan);
        }

        let service_gating_enforced = should_enforce_service_gating_for_block(
            self.config.feature_flags.enable_service_gating,
            local_proposal,
            block.header.epoch,
            self.config.feature_flags.service_gating_start_epoch,
        );
        let service_score = if service_gating_enforced {
            Some(self.service_score_for_validator(block.header.proposer_id))
        } else {
            None
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

        let mut next_ledger = self.ledger.clone();
        next_ledger.apply_block(&block, self.crypto.as_ref())?;
        self.ledger = next_ledger;
        self.seen_blocks.insert(block.block_hash);
        self.blocks.push(block.clone());
        self.storage
            .append_json_line(&self.storage.blocks_path, &block)?;
        self.storage.write_snapshot(self.ledger.snapshot())?;
        self.import_commitment_receipts(&block.commitment_receipts)?;
        for transaction in &block.transactions {
            self.mempool.remove(&transaction.tx_hash);
        }
        self.update_metrics(|metrics| {
            metrics.blocks_validated += 1;
            if local_proposal {
                metrics.blocks_proposed += 1;
            }
        });
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
        self.persist_metrics()?;
        Ok(BlockAcceptance::Accepted)
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
        self.rebuild_seen_sets();
        self.storage.write_snapshot(self.ledger.snapshot())?;
        self.storage
            .overwrite_json_lines(&self.storage.blocks_path, &self.blocks)?;
        self.storage
            .overwrite_json_lines(&self.storage.receipts_path, &self.receipts)?;
        Ok(())
    }

    fn apply_chain_segment(&mut self, chain: ChainSegment) -> Result<()> {
        let chain =
            align_chain_segment_to_local_chain(&self.blocks, self.ledger.snapshot(), chain)?;

        let receipts =
            validate_snapshot_receipts(&self.consensus, self.crypto.as_ref(), &chain.receipts)?;
        let mut ledger = self.ledger.clone();
        let mut expected_parent_hash = self.ledger.snapshot().tip_hash;
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
        self.blocks.extend(chain.blocks);
        self.rebuild_seen_sets();
        self.storage.write_snapshot(self.ledger.snapshot())?;
        self.storage
            .overwrite_json_lines(&self.storage.blocks_path, &self.blocks)?;
        for receipt in receipts {
            self.store_receipt(receipt)?;
        }
        Ok(())
    }

    fn sync_request_is_throttled(
        &self,
        validator_id: ValidatorId,
        known_height: u64,
        known_tip_hash: HashBytes,
    ) -> bool {
        self.last_sync_request_served
            .get(&validator_id)
            .map(|served| {
                sync_request_should_throttle(
                    *served,
                    now_unix_millis(),
                    known_height,
                    known_tip_hash,
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
        self.last_sync_request_served.insert(
            validator_id,
            ServedSyncRequest {
                served_at_unix_millis: now_unix_millis(),
                known_height,
                known_tip_hash,
            },
        );
    }

    fn sync_request_hint_for_peer(&self, validator_id: ValidatorId) -> (u64, HashBytes) {
        sync_request_known_state(
            self.ledger.block_height(),
            self.ledger.snapshot().tip_hash,
            self.should_force_full_snapshot_for_peer(validator_id),
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

    fn broadcast_sync_request(&self) -> Result<()> {
        self.network.broadcast_control(
            &self.config.peers,
            ProtocolMessage::SyncRequest {
                requester_id: self.config.validator_id,
                known_height: self.ledger.block_height(),
                known_tip_hash: self.ledger.snapshot().tip_hash,
            },
        )
    }

    fn broadcast_sync_status(&self) -> Result<()> {
        self.network.broadcast_control(
            &self.config.peers,
            ProtocolMessage::SyncStatus {
                validator_id: self.config.validator_id,
                height: self.ledger.block_height(),
                tip_hash: self.ledger.snapshot().tip_hash,
            },
        )
    }

    fn request_sync_from(&self, validator_id: ValidatorId) -> Result<()> {
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

    fn push_best_sync_to(
        &self,
        validator_id: ValidatorId,
        known_height: u64,
        known_tip_hash: HashBytes,
    ) -> Result<()> {
        self.send_best_sync_to(validator_id, known_height, known_tip_hash)
    }

    fn send_best_sync_to(
        &self,
        validator_id: ValidatorId,
        known_height: u64,
        known_tip_hash: HashBytes,
    ) -> Result<()> {
        let peer = self.find_peer(validator_id)?;
        if let Some(chain) = self.build_chain_segment(known_height, known_tip_hash) {
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
        let chain = self.build_chain_snapshot();
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

    fn build_chain_snapshot(&self) -> ChainSnapshot {
        ChainSnapshot {
            snapshot: self.ledger.snapshot().clone(),
            blocks: self.blocks.clone(),
            receipts: self.receipts.clone(),
        }
    }

    fn build_chain_segment(
        &self,
        known_height: u64,
        known_tip_hash: HashBytes,
    ) -> Option<ChainSegment> {
        build_chain_segment_from_chain(
            self.ledger.snapshot(),
            &self.blocks,
            &self.receipts,
            self.config.feature_flags.service_score_window_epochs,
            known_height,
            known_tip_hash,
        )
    }

    fn push_sync_to_stale_peers(&self) -> Result<()> {
        let local_height = self.ledger.block_height();
        let local_tip_hash = self.ledger.snapshot().tip_hash;
        for peer in &self.config.peers {
            if let Some((known_height, known_tip_hash)) =
                self.peer_sync_status.get(&peer.validator_id).copied()
            {
                if local_height > known_height
                    || (local_height == known_height && local_tip_hash != known_tip_hash)
                {
                    self.send_best_sync_to(peer.validator_id, known_height, known_tip_hash)?;
                }
            } else if local_height > 0 {
                self.send_full_snapshot_to(peer.clone())?;
            }
        }
        Ok(())
    }

    fn record_peer_sync_status(
        &mut self,
        validator_id: ValidatorId,
        height: u64,
        tip_hash: HashBytes,
    ) {
        if !should_record_peer_sync_status(height, tip_hash) {
            return;
        }
        self.peer_sync_status
            .insert(validator_id, (height, tip_hash));
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
        for block in &self.blocks {
            for transaction in &block.transactions {
                self.seen_transactions.insert(transaction.tx_hash);
            }
        }
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
                epoch.saturating_sub(1),
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
    unsigned.signature.clear();
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

fn classify_peer_message(payload: &ProtocolMessage) -> Option<PeerMessageClass> {
    match payload {
        ProtocolMessage::SyncStatus { .. }
        | ProtocolMessage::SyncRequest { .. }
        | ProtocolMessage::ReceiptFetch { .. } => Some(PeerMessageClass::SyncControl),
        ProtocolMessage::TransactionBroadcast(_) => Some(PeerMessageClass::TransactionGossip),
        ProtocolMessage::RelayReceipt(_) => Some(PeerMessageClass::ReceiptGossip),
        ProtocolMessage::BlockProposal(_)
        | ProtocolMessage::SyncResponse { .. }
        | ProtocolMessage::SyncBlocks { .. }
        | ProtocolMessage::HeartbeatPulse(_)
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
        || missing_blocks.len() > MAX_PREFERRED_INCREMENTAL_SYNC_BLOCKS
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
    })
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

fn sync_repair_should_force_full_snapshot(failure_count: u64) -> bool {
    failure_count >= INCREMENTAL_SYNC_FAILURES_BEFORE_FULL_SNAPSHOT
}

fn sync_request_should_throttle(
    served: ServedSyncRequest,
    now_unix_millis: u64,
    known_height: u64,
    known_tip_hash: HashBytes,
) -> bool {
    if is_force_full_snapshot_hint(known_height, known_tip_hash) {
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

#[derive(Clone, Debug)]
struct Storage {
    data_dir: PathBuf,
    inbox_dir: PathBuf,
    processed_dir: PathBuf,
    blocks_path: PathBuf,
    receipts_path: PathBuf,
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
    use std::collections::BTreeMap;

    use entangrid_crypto::{DeterministicCryptoBackend, Signer};
    use entangrid_types::{
        GenesisConfig, SignedTransaction, Transaction, ValidatorConfig, empty_hash,
        validator_account,
    };

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
                    public_identity: vec![],
                })
                .collect(),
            initial_balances: balances,
        }
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
            signature: vec![1, 2, 3],
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

    #[test]
    fn receipt_event_hash_ignores_non_identity_fields() {
        let mut first = sample_receipt();
        let mut second = sample_receipt();
        second.signature = vec![9, 9, 9];
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
        receipt.signature = vec![9, 9, 9];
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

        let segment =
            build_chain_segment_from_chain(&snapshot, &[block.clone()], &[], 4, 0, empty_hash())
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

        let segment = build_chain_segment_from_chain(&snapshot, &[block], &[], 4, 0, [9u8; 32]);
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

        let segment = build_chain_segment_from_chain(&snapshot, &blocks, &[], 4, 0, empty_hash());
        assert!(segment.is_none());
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
        ));
        assert!(!sync_request_should_throttle(
            fresh_height,
            now + 10,
            repeated.known_height,
            repeated.known_tip_hash,
        ));
        assert!(!sync_request_should_throttle(
            fresh_tip,
            now + 10,
            repeated.known_height,
            repeated.known_tip_hash,
        ));
        assert!(!sync_request_should_throttle(
            repeated,
            now + SYNC_REQUEST_COOLDOWN_MILLIS + 1,
            repeated.known_height,
            repeated.known_tip_hash,
        ));
        assert!(!sync_request_should_throttle(
            repeated,
            now + 10,
            u64::MAX,
            empty_hash(),
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
