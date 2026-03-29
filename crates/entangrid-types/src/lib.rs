use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

pub type ValidatorId = u64;
pub type Slot = u64;
pub type Epoch = u64;
pub type HashBytes = [u8; 32];
pub type AccountId = String;

pub const RECOMMENDED_SERVICE_GATING_START_EPOCH: Epoch = 3;
pub const RECOMMENDED_SERVICE_GATING_THRESHOLD: f64 = 0.40;
pub const RECOMMENDED_SERVICE_SCORE_WINDOW_EPOCHS: u64 = 4;
pub const RECOMMENDED_SERVICE_UPTIME_WEIGHT: f64 = 0.25;
pub const RECOMMENDED_SERVICE_DELIVERY_WEIGHT: f64 = 0.50;
pub const RECOMMENDED_SERVICE_DIVERSITY_WEIGHT: f64 = 0.25;
pub const RECOMMENDED_SERVICE_PENALTY_WEIGHT: f64 = 1.0;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum MessageClass {
    Heartbeat,
    Transaction,
    Block,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct FeatureFlags {
    pub enable_receipts: bool,
    pub enable_service_gating: bool,
    #[serde(default = "default_consensus_v2")]
    pub consensus_v2: bool,
    #[serde(default = "default_service_gating_start_epoch")]
    pub service_gating_start_epoch: Epoch,
    #[serde(default = "default_service_gating_threshold")]
    pub service_gating_threshold: f64,
    #[serde(default = "default_service_score_window_epochs")]
    pub service_score_window_epochs: u64,
    #[serde(default = "default_service_score_weights")]
    pub service_score_weights: ServiceScoreWeights,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct FaultProfile {
    pub artificial_delay_ms: u64,
    pub outbound_drop_probability: f64,
    pub pause_slot_production: bool,
    pub disable_outbound: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct PeerConfig {
    pub validator_id: ValidatorId,
    pub address: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct ValidatorConfig {
    pub validator_id: ValidatorId,
    pub stake: u64,
    pub address: String,
    pub dev_secret: String,
    pub public_identity: Vec<u8>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct GenesisConfig {
    pub chain_id: String,
    pub epoch_seed: HashBytes,
    pub genesis_time_unix_millis: u64,
    pub slot_duration_millis: u64,
    pub slots_per_epoch: u64,
    pub max_txs_per_block: usize,
    pub witness_count: usize,
    pub validators: Vec<ValidatorConfig>,
    pub initial_balances: BTreeMap<AccountId, u64>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct NodeConfig {
    pub validator_id: ValidatorId,
    pub data_dir: String,
    pub genesis_path: String,
    pub listen_address: String,
    pub peers: Vec<PeerConfig>,
    pub log_path: String,
    pub metrics_path: String,
    pub feature_flags: FeatureFlags,
    pub fault_profile: FaultProfile,
    pub sync_on_startup: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Transaction {
    pub from: AccountId,
    pub to: AccountId,
    pub amount: u64,
    pub nonce: u64,
    pub memo: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct SignedTransaction {
    pub transaction: Transaction,
    pub signer_id: ValidatorId,
    pub signature: Vec<u8>,
    pub tx_hash: HashBytes,
    pub submitted_at_unix_millis: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct HeartbeatPulse {
    pub epoch: Epoch,
    pub slot: Slot,
    pub source_validator_id: ValidatorId,
    pub sequence_number: u64,
    pub emitted_at_unix_millis: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct RelayReceipt {
    pub epoch: Epoch,
    pub slot: Slot,
    pub source_validator_id: ValidatorId,
    pub destination_validator_id: ValidatorId,
    pub witness_validator_id: ValidatorId,
    pub message_class: MessageClass,
    pub transcript_digest: HashBytes,
    pub latency_bucket_ms: u64,
    pub byte_count_bucket: u64,
    pub sequence_number: u64,
    pub signature: Vec<u8>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct CommitmentSummary {
    pub by_message_class: BTreeMap<MessageClass, u64>,
    pub distinct_peers: u64,
    pub relay_score: f64,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct TopologyCommitment {
    pub epoch: Epoch,
    pub validator_id: ValidatorId,
    pub receipt_root: HashBytes,
    pub receipt_count: u64,
    pub summary: CommitmentSummary,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct EpochAssignment {
    pub epoch: Epoch,
    pub validator_id: ValidatorId,
    pub witnesses: Vec<ValidatorId>,
    pub relay_targets: Vec<ValidatorId>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct BlockHeader {
    pub block_number: u64,
    pub parent_hash: HashBytes,
    pub slot: Slot,
    pub epoch: Epoch,
    pub proposer_id: ValidatorId,
    pub timestamp_unix_millis: u64,
    pub state_root: HashBytes,
    pub transactions_root: HashBytes,
    pub topology_root: HashBytes,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Block {
    pub header: BlockHeader,
    pub transactions: Vec<SignedTransaction>,
    pub commitment: Option<TopologyCommitment>,
    #[serde(default)]
    pub commitment_receipts: Vec<RelayReceipt>,
    pub signature: Vec<u8>,
    pub block_hash: HashBytes,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct StateSnapshot {
    pub balances: BTreeMap<AccountId, u64>,
    pub nonces: BTreeMap<AccountId, u64>,
    pub tip_hash: HashBytes,
    pub height: u64,
    pub last_slot: Slot,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct ChainSnapshot {
    pub snapshot: StateSnapshot,
    pub blocks: Vec<Block>,
    pub receipts: Vec<RelayReceipt>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct ChainSegment {
    pub base_height: u64,
    pub base_tip_hash: HashBytes,
    pub target_snapshot: StateSnapshot,
    pub blocks: Vec<Block>,
    pub receipts: Vec<RelayReceipt>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct ProposalVote {
    pub validator_id: ValidatorId,
    pub block_hash: HashBytes,
    pub block_number: u64,
    pub epoch: Epoch,
    pub slot: Slot,
    pub signature: Vec<u8>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct QuorumCertificate {
    pub block_hash: HashBytes,
    pub block_number: u64,
    pub epoch: Epoch,
    pub slot: Slot,
    pub vote_root: HashBytes,
    pub votes: Vec<ProposalVote>,
}

impl QuorumCertificate {
    pub fn is_well_formed(&self) -> bool {
        !self.votes.is_empty()
            && self.vote_root == quorum_certificate_vote_root(&self.votes)
            && self.votes.iter().all(|vote| {
                vote.block_hash == self.block_hash
                    && vote.block_number == self.block_number
                    && vote.epoch == self.epoch
                    && vote.slot == self.slot
            })
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct ServiceAttestation {
    pub subject_validator_id: ValidatorId,
    pub committee_member_id: ValidatorId,
    pub epoch: Epoch,
    pub counters: ServiceCounters,
    pub signature: Vec<u8>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct ServiceAggregate {
    pub subject_validator_id: ValidatorId,
    pub epoch: Epoch,
    pub attestation_root: HashBytes,
    pub attestations: Vec<ServiceAttestation>,
    pub aggregate_counters: ServiceCounters,
}

impl ServiceAggregate {
    pub fn is_well_formed(&self) -> bool {
        !self.attestations.is_empty()
            && self.attestation_root == service_attestation_root(&self.attestations)
            && self.aggregate_counters == aggregate_service_counters(&self.attestations)
            && self.attestations.iter().all(|attestation| {
                attestation.subject_validator_id == self.subject_validator_id
                    && attestation.epoch == self.epoch
            })
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct CertifiedBlockHeader {
    pub header: BlockHeader,
    pub block_hash: HashBytes,
    pub quorum_certificate: Option<QuorumCertificate>,
    pub prior_service_aggregate: Option<ServiceAggregate>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct SyncQcAnchor {
    pub block_hash: HashBytes,
    pub block_number: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct ChunkedSyncRequest {
    pub requester_id: ValidatorId,
    pub known_qc_hash: Option<HashBytes>,
    pub known_qc_height: u64,
    pub known_qc_anchors: Vec<SyncQcAnchor>,
    pub from_height: u64,
    pub want_certified_only: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub enum ChunkedSyncResponse {
    Certified {
        responder_id: ValidatorId,
        responder_height: u64,
        responder_tip_hash: HashBytes,
        shared_qc_hash: HashBytes,
        shared_qc_height: u64,
        headers: Vec<CertifiedBlockHeader>,
        blocks: Vec<Block>,
        qcs: Vec<QuorumCertificate>,
        service_aggregates: Vec<ServiceAggregate>,
    },
    Unavailable {
        responder_id: ValidatorId,
    },
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub enum ProtocolMessage {
    TransactionBroadcast(SignedTransaction),
    BlockProposal(Block),
    ProposalVote(ProposalVote),
    QuorumCertificate(QuorumCertificate),
    SyncStatus {
        validator_id: ValidatorId,
        height: u64,
        tip_hash: HashBytes,
        highest_qc_hash: Option<HashBytes>,
        highest_qc_height: u64,
        recent_qc_anchors: Vec<SyncQcAnchor>,
    },
    SyncRequest {
        requester_id: ValidatorId,
        known_height: u64,
        known_tip_hash: HashBytes,
    },
    SyncResponse {
        responder_id: ValidatorId,
        chain: ChainSnapshot,
    },
    SyncBlocks {
        responder_id: ValidatorId,
        chain: ChainSegment,
    },
    CertifiedSyncRequest(ChunkedSyncRequest),
    CertifiedSyncResponse(ChunkedSyncResponse),
    HeartbeatPulse(HeartbeatPulse),
    RelayReceipt(RelayReceipt),
    ServiceAttestation(ServiceAttestation),
    ServiceAggregate(ServiceAggregate),
    ReceiptFetch {
        requester_id: ValidatorId,
        epoch: Epoch,
        validator_id: ValidatorId,
    },
    ReceiptResponse {
        responder_id: ValidatorId,
        epoch: Epoch,
        validator_id: ValidatorId,
        receipts: Vec<RelayReceipt>,
    },
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct SignedEnvelope {
    pub from_validator_id: ValidatorId,
    pub message_hash: HashBytes,
    pub signature: Vec<u8>,
    pub payload: ProtocolMessage,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct ServiceCounters {
    pub uptime_windows: u64,
    pub total_windows: u64,
    pub timely_deliveries: u64,
    pub expected_deliveries: u64,
    pub distinct_peers: u64,
    pub expected_peers: u64,
    pub failed_sessions: u64,
    pub invalid_receipts: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct ServiceScoreWeights {
    #[serde(default = "default_service_uptime_weight")]
    pub uptime_weight: f64,
    #[serde(default = "default_service_delivery_weight")]
    pub delivery_weight: f64,
    #[serde(default = "default_service_diversity_weight")]
    pub diversity_weight: f64,
    #[serde(default = "default_service_penalty_weight")]
    pub penalty_weight: f64,
}

impl Default for ServiceScoreWeights {
    fn default() -> Self {
        default_service_score_weights()
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct NodeMetrics {
    pub validator_id: ValidatorId,
    pub current_slot: Slot,
    pub current_epoch: Epoch,
    pub last_completed_service_epoch: Epoch,
    pub service_gating_start_epoch: Epoch,
    pub service_score_window_epochs: u64,
    pub service_score_weights: ServiceScoreWeights,
    pub active_sessions: u64,
    pub handshake_attempts: u64,
    pub handshake_failures: u64,
    pub blocks_proposed: u64,
    pub blocks_validated: u64,
    pub missed_proposer_slots: u64,
    pub service_gating_rejections: u64,
    pub service_gating_enforcement_skips: u64,
    pub duplicate_receipts_ignored: u64,
    pub tx_ingress: u64,
    pub tx_propagated: u64,
    pub receipts_created: u64,
    pub receipts_verified: u64,
    pub service_attestations_emitted: u64,
    pub service_attestations_imported: u64,
    pub service_aggregates_published: u64,
    pub service_aggregates_imported: u64,
    pub bytes_sent: u64,
    pub bytes_received: u64,
    pub last_local_service_score: f64,
    pub service_gating_threshold: f64,
    pub last_local_service_counters: ServiceCounters,
    pub relay_scores: BTreeMap<ValidatorId, f64>,
    pub sync_requests_throttled: u64,
    pub peer_rate_limit_drops: u64,
    pub inbound_session_drops: u64,
    pub incremental_sync_served: u64,
    pub incremental_sync_applied: u64,
    pub full_sync_served: u64,
    pub full_sync_applied: u64,
    pub certified_sync_served: u64,
    pub certified_sync_applied: u64,
    pub last_updated_unix_millis: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct EventLogEntry {
    pub timestamp_unix_millis: u64,
    pub event: String,
    pub detail: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct LocalnetManifest {
    pub base_dir: String,
    pub genesis_path: String,
    pub node_configs: Vec<String>,
}

pub fn canonical_hash<T: Serialize + ?Sized>(value: &T) -> HashBytes {
    let bytes = bincode::serde::encode_to_vec(value, bincode::config::standard())
        .expect("canonical serialization should succeed");
    blake3::hash(&bytes).into()
}

pub fn hash_many(parts: &[&[u8]]) -> HashBytes {
    let mut hasher = blake3::Hasher::new();
    for part in parts {
        hasher.update(part);
    }
    hasher.finalize().into()
}

pub fn empty_hash() -> HashBytes {
    [0u8; 32]
}

pub fn default_service_gating_start_epoch() -> Epoch {
    RECOMMENDED_SERVICE_GATING_START_EPOCH
}

pub fn default_consensus_v2() -> bool {
    false
}

pub fn default_service_gating_threshold() -> f64 {
    RECOMMENDED_SERVICE_GATING_THRESHOLD
}

pub fn default_service_score_window_epochs() -> u64 {
    RECOMMENDED_SERVICE_SCORE_WINDOW_EPOCHS
}

pub fn default_service_uptime_weight() -> f64 {
    RECOMMENDED_SERVICE_UPTIME_WEIGHT
}

pub fn default_service_delivery_weight() -> f64 {
    RECOMMENDED_SERVICE_DELIVERY_WEIGHT
}

pub fn default_service_diversity_weight() -> f64 {
    RECOMMENDED_SERVICE_DIVERSITY_WEIGHT
}

pub fn default_service_penalty_weight() -> f64 {
    RECOMMENDED_SERVICE_PENALTY_WEIGHT
}

pub fn default_service_score_weights() -> ServiceScoreWeights {
    ServiceScoreWeights {
        uptime_weight: default_service_uptime_weight(),
        delivery_weight: default_service_delivery_weight(),
        diversity_weight: default_service_diversity_weight(),
        penalty_weight: default_service_penalty_weight(),
    }
}

pub fn validator_account(validator_id: ValidatorId) -> AccountId {
    format!("validator-{validator_id}")
}

pub fn quorum_certificate_vote_root(votes: &[ProposalVote]) -> HashBytes {
    canonical_hash(votes)
}

pub fn service_attestation_root(attestations: &[ServiceAttestation]) -> HashBytes {
    canonical_hash(attestations)
}

pub fn aggregate_service_counters(attestations: &[ServiceAttestation]) -> ServiceCounters {
    let mut counters = ServiceCounters::default();
    for attestation in attestations {
        counters.uptime_windows += attestation.counters.uptime_windows;
        counters.total_windows += attestation.counters.total_windows;
        counters.timely_deliveries += attestation.counters.timely_deliveries;
        counters.expected_deliveries += attestation.counters.expected_deliveries;
        counters.distinct_peers += attestation.counters.distinct_peers;
        counters.expected_peers += attestation.counters.expected_peers;
        counters.failed_sessions += attestation.counters.failed_sessions;
        counters.invalid_receipts += attestation.counters.invalid_receipts;
    }
    counters
}

pub fn now_unix_millis() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};

    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonical_hash_is_deterministic() {
        let tx = Transaction {
            from: "alice".into(),
            to: "bob".into(),
            amount: 5,
            nonce: 1,
            memo: None,
        };
        assert_eq!(canonical_hash(&tx), canonical_hash(&tx));
    }

    #[test]
    fn default_service_policy_matches_recommended_profile() {
        assert_eq!(
            default_service_gating_start_epoch(),
            RECOMMENDED_SERVICE_GATING_START_EPOCH
        );
        assert!(
            (default_service_gating_threshold() - RECOMMENDED_SERVICE_GATING_THRESHOLD).abs()
                < f64::EPSILON
        );
        assert_eq!(
            default_service_score_window_epochs(),
            RECOMMENDED_SERVICE_SCORE_WINDOW_EPOCHS
        );
        assert_eq!(
            default_service_score_weights(),
            ServiceScoreWeights {
                uptime_weight: RECOMMENDED_SERVICE_UPTIME_WEIGHT,
                delivery_weight: RECOMMENDED_SERVICE_DELIVERY_WEIGHT,
                diversity_weight: RECOMMENDED_SERVICE_DIVERSITY_WEIGHT,
                penalty_weight: RECOMMENDED_SERVICE_PENALTY_WEIGHT,
            }
        );
    }

    #[test]
    fn feature_flags_default_consensus_v2_is_disabled() {
        assert!(!FeatureFlags::default().consensus_v2);
    }

    #[test]
    fn consensus_v2_objects_are_transportable_and_self_validating() {
        let vote = ProposalVote {
            validator_id: 1,
            block_hash: [9; 32],
            block_number: 7,
            epoch: 3,
            slot: 19,
            signature: vec![1, 2, 3],
        };
        let qc = QuorumCertificate {
            block_hash: vote.block_hash,
            block_number: vote.block_number,
            epoch: vote.epoch,
            slot: vote.slot,
            vote_root: quorum_certificate_vote_root(std::slice::from_ref(&vote)),
            votes: vec![vote.clone()],
        };
        assert!(qc.is_well_formed());

        let attestation = ServiceAttestation {
            subject_validator_id: 3,
            committee_member_id: 2,
            epoch: 2,
            counters: ServiceCounters {
                uptime_windows: 4,
                total_windows: 4,
                timely_deliveries: 6,
                expected_deliveries: 8,
                distinct_peers: 2,
                expected_peers: 3,
                failed_sessions: 1,
                invalid_receipts: 0,
            },
            signature: vec![4, 5, 6],
        };
        let aggregate = ServiceAggregate {
            subject_validator_id: 3,
            epoch: 2,
            attestation_root: service_attestation_root(std::slice::from_ref(&attestation)),
            attestations: vec![attestation.clone()],
            aggregate_counters: aggregate_service_counters(std::slice::from_ref(&attestation)),
        };
        assert!(aggregate.is_well_formed());

        let certified = CertifiedBlockHeader {
            header: BlockHeader {
                block_number: 7,
                parent_hash: [8; 32],
                slot: 19,
                epoch: 3,
                proposer_id: 1,
                timestamp_unix_millis: 42,
                state_root: [1; 32],
                transactions_root: [2; 32],
                topology_root: [3; 32],
            },
            block_hash: [9; 32],
            quorum_certificate: Some(qc),
            prior_service_aggregate: Some(aggregate),
        };
        let bytes = bincode::serde::encode_to_vec(&certified, bincode::config::standard()).unwrap();
        let (decoded, _): (CertifiedBlockHeader, usize) =
            bincode::serde::decode_from_slice(&bytes, bincode::config::standard()).unwrap();
        assert_eq!(decoded, certified);
    }
}
