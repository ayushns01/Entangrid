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
use entangrid_network::{NetworkEvent, NetworkHandle, spawn_network};
use entangrid_types::{
    Block, BlockHeader, ChainSnapshot, Epoch, EventLogEntry, GenesisConfig, HashBytes,
    HeartbeatPulse, MessageClass, NodeConfig, NodeMetrics, PeerConfig, ProtocolMessage,
    RelayReceipt, ServiceCounters, SignedTransaction, StateSnapshot, TopologyCommitment,
    ValidatorId, canonical_hash, empty_hash, now_unix_millis,
};
use tokio::{sync::mpsc, time::MissedTickBehavior};
use tracing::info;

const SNAPSHOT_FILE: &str = "state_snapshot.json";
const BLOCKS_FILE: &str = "blocks.jsonl";
const RECEIPTS_FILE: &str = "receipts.jsonl";
const ORPHANS_FILE: &str = "orphans.jsonl";

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
        observed_invalid_receipts: BTreeMap::new(),
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
    observed_invalid_receipts: BTreeMap<(Epoch, ValidatorId), u64>,
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
                        self.broadcast_chain_snapshot()?;
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
                        "slot {slot} local_score {score:.3} threshold {:.3} window {} uptime {}/{} timely {}/{} peers {}/{} failed_sessions {} invalid_receipts {}",
                        self.service_gating_threshold(),
                        self.config.feature_flags.service_score_window_epochs,
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
                            "slot {slot} missed due to service score {score:.3} below threshold {:.3} with window {} uptime {}/{} timely {}/{} peers {}/{} failed_sessions {} invalid_receipts {}",
                            self.service_gating_threshold(),
                            self.config.feature_flags.service_score_window_epochs,
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
                let commitment = self.consensus.commitment_from_receipts(
                    self.config.validator_id,
                    epoch - 1,
                    &commitment_receipts,
                    0,
                    0,
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

        self.accept_block(block.clone(), true).await?;
        self.network
            .broadcast(&self.config.peers, ProtocolMessage::BlockProposal(block))?;
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
            } => {
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
            } => {
                self.failed_sessions += 1;
                if let Some(peer_validator_id) = peer_validator_id {
                    self.record_failed_session(
                        self.current_epoch(),
                        self.config.validator_id,
                        peer_validator_id,
                    );
                }
                self.update_metrics(|metrics| {
                    metrics.handshake_failures += 1;
                });
                self.log_event(
                    "disconnect",
                    format!("peer {:?} detail {detail}", peer_validator_id),
                )?;
            }
        }
        Ok(())
    }

    async fn handle_protocol_message(
        &mut self,
        from_validator_id: ValidatorId,
        payload: ProtocolMessage,
    ) -> Result<()> {
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
                    Ok(true) => {
                        self.network
                            .broadcast(&self.config.peers, ProtocolMessage::BlockProposal(block))?;
                    }
                    Ok(false) => {
                        if let Err(error) = self.push_chain_to(from_validator_id) {
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
            ProtocolMessage::SyncRequest { requester_id } => {
                let chain = ChainSnapshot {
                    snapshot: self.ledger.snapshot().clone(),
                    blocks: self.blocks.clone(),
                    receipts: self.receipts.clone(),
                };
                let peer = self.find_peer(requester_id)?;
                self.network.send_to(
                    peer,
                    ProtocolMessage::SyncResponse {
                        responder_id: self.config.validator_id,
                        chain,
                    },
                )?;
            }
            ProtocolMessage::SyncResponse {
                responder_id,
                chain,
            } => {
                if chain.snapshot.height > self.ledger.block_height() {
                    match self.apply_chain_snapshot(chain) {
                        Ok(()) => {
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
                } else if chain.snapshot.height < self.ledger.block_height() {
                    if let Err(error) = self.push_chain_to(responder_id) {
                        self.log_event(
                            "sync-push-failed",
                            format!("to {responder_id} detail {error}"),
                        )?;
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
                self.network.send_to(
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

    async fn accept_block(&mut self, block: Block, local_proposal: bool) -> Result<bool> {
        if self.seen_blocks.contains(&block.block_hash) {
            return Ok(false);
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
            return Ok(false);
        }

        let service_score = if self.config.feature_flags.enable_service_gating
            && self.service_gating_active(block.header.epoch)
        {
            Some(self.service_score_for_validator(block.header.proposer_id))
        } else {
            None
        };
        self.consensus
            .validate_block_basic(
                &block,
                self.ledger.snapshot().tip_hash,
                service_score,
                self.config.feature_flags.enable_service_gating
                    && self.service_gating_active(block.header.epoch),
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
        Ok(true)
    }

    fn validate_commitment(
        &mut self,
        commitment: &TopologyCommitment,
        commitment_receipts: &[RelayReceipt],
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
            self.validate_receipt(receipt)?;
            let receipt_hash = canonical_hash(receipt);
            if !exact_hashes.insert(receipt_hash) {
                return Err(anyhow!("duplicate receipt in commitment"));
            }
            let event_hash = receipt_event_hash(receipt);
            if !event_hashes.insert(event_hash) {
                return Err(anyhow!("duplicate receipt event in commitment"));
            }
        }
        let expected = self.consensus.commitment_from_receipts(
            commitment.validator_id,
            commitment.epoch,
            commitment_receipts,
            0,
            0,
        );
        if expected != *commitment {
            return Err(anyhow!("receipt root mismatch"));
        }
        Ok(())
    }

    fn import_commitment_receipts(&mut self, receipts: &[RelayReceipt]) -> Result<()> {
        for receipt in receipts {
            let _ = self.persist_receipt(receipt.clone())?;
        }
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
                if let Err(error) = self.accept_block(orphan.clone(), false).await {
                    self.log_event(
                        "orphan-rejected",
                        format!(
                            "height {} slot {} detail {}",
                            orphan.header.block_number, orphan.header.slot, error
                        ),
                    )?;
                }
                progress = true;
            }
        }
        Ok(())
    }

    fn apply_chain_snapshot(&mut self, chain: ChainSnapshot) -> Result<()> {
        let ledger =
            LedgerState::replay_blocks(&self.genesis, &chain.blocks, self.crypto.as_ref())?;
        if ledger.snapshot() != &chain.snapshot {
            return Err(anyhow!("snapshot replay mismatch"));
        }
        self.ledger = ledger;
        self.blocks = chain.blocks;
        self.receipts = chain.receipts;
        self.rebuild_seen_sets();
        self.storage.write_snapshot(self.ledger.snapshot())?;
        self.storage
            .overwrite_json_lines(&self.storage.blocks_path, &self.blocks)?;
        self.storage
            .overwrite_json_lines(&self.storage.receipts_path, &self.receipts)?;
        Ok(())
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
            let score = self.consensus.compute_service_score(&aggregate);
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
        self.network.broadcast(
            &self.config.peers,
            ProtocolMessage::SyncRequest {
                requester_id: self.config.validator_id,
            },
        )
    }

    fn broadcast_chain_snapshot(&self) -> Result<()> {
        let chain = ChainSnapshot {
            snapshot: self.ledger.snapshot().clone(),
            blocks: self.blocks.clone(),
            receipts: self.receipts.clone(),
        };
        self.network.broadcast(
            &self.config.peers,
            ProtocolMessage::SyncResponse {
                responder_id: self.config.validator_id,
                chain,
            },
        )
    }

    fn request_sync_from(&self, validator_id: ValidatorId) -> Result<()> {
        self.network.send_to(
            self.find_peer(validator_id)?,
            ProtocolMessage::SyncRequest {
                requester_id: self.config.validator_id,
            },
        )
    }

    fn push_chain_to(&self, validator_id: ValidatorId) -> Result<()> {
        let chain = ChainSnapshot {
            snapshot: self.ledger.snapshot().clone(),
            blocks: self.blocks.clone(),
            receipts: self.receipts.clone(),
        };
        self.network.send_to(
            self.find_peer(validator_id)?,
            ProtocolMessage::SyncResponse {
                responder_id: self.config.validator_id,
                chain,
            },
        )
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
        if !self
            .consensus
            .is_relay_target_for(validator_id, peer_validator_id, epoch)
        {
            return;
        }
        self.observed_failed_sessions
            .insert((epoch, validator_id, peer_validator_id));
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
                    "epoch {epoch} warmup until epoch {} local_score {:.3} threshold {:.3} window {} uptime {}/{} timely {}/{} peers {}/{} failed_sessions {} invalid_receipts {}",
                    self.config.feature_flags.service_gating_start_epoch,
                    self.service_score_for_validator(self.config.validator_id),
                    self.service_gating_threshold(),
                    self.config.feature_flags.service_score_window_epochs,
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
                "epoch {epoch} completed_epoch {} local_score {:.3} threshold {:.3} window {} uptime {}/{} timely {}/{} peers {}/{} failed_sessions {} invalid_receipts {}",
                epoch.saturating_sub(1),
                self.service_score_for_validator(self.config.validator_id),
                self.service_gating_threshold(),
                self.config.feature_flags.service_score_window_epochs,
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
    use super::*;

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
}
