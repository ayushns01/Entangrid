use std::collections::{BTreeMap, BTreeSet};

use entangrid_types::{
    Block, CommitmentSummary, Epoch, EpochAssignment, GenesisConfig, HashBytes, MessageClass,
    RelayReceipt, ServiceAggregate, ServiceCounters, ServiceScoreWeights, Slot, TopologyCommitment,
    ValidatorId, canonical_hash, default_service_score_weights, hash_many, now_unix_millis,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ServiceEvidenceStatus {
    Confirmed,
    InsufficientEvidence,
}

pub fn derived_service_committee_size(validator_count: usize) -> usize {
    if validator_count <= 1 {
        return 0;
    }
    let log2 = validator_count.ilog2() as usize;
    let target = (log2 + 1).max(3);
    target.min(validator_count - 1)
}

#[derive(Clone, Debug)]
pub struct ConsensusEngine {
    genesis: GenesisConfig,
    validator_ids: Vec<ValidatorId>,
}

impl ConsensusEngine {
    pub fn new(genesis: GenesisConfig) -> Self {
        let mut validator_ids: Vec<_> = genesis.validators.iter().map(|v| v.validator_id).collect();
        validator_ids.sort_unstable();
        Self {
            genesis,
            validator_ids,
        }
    }

    pub fn genesis(&self) -> &GenesisConfig {
        &self.genesis
    }

    pub fn current_slot(&self) -> Slot {
        self.slot_at(now_unix_millis())
    }

    pub fn slot_at(&self, timestamp_unix_millis: u64) -> Slot {
        let offset = timestamp_unix_millis.saturating_sub(self.genesis.genesis_time_unix_millis);
        offset / self.genesis.slot_duration_millis
    }

    pub fn epoch_for_slot(&self, slot: Slot) -> Epoch {
        if self.genesis.slots_per_epoch == 0 {
            return 0;
        }
        slot / self.genesis.slots_per_epoch
    }

    pub fn epoch_seed(&self, epoch: Epoch) -> HashBytes {
        hash_many(&[&self.genesis.epoch_seed, &epoch.to_le_bytes()])
    }

    pub fn proposer_for_slot(&self, slot: Slot) -> ValidatorId {
        let epoch = self.epoch_for_slot(slot);
        let seed = self.epoch_seed(epoch);
        let slot_bytes = slot.to_le_bytes();
        let selection = hash_many(&[&seed, &slot_bytes]);
        let mut number_bytes = [0u8; 8];
        number_bytes.copy_from_slice(&selection[..8]);
        let idx = u64::from_le_bytes(number_bytes) as usize % self.validator_ids.len();
        self.validator_ids[idx]
    }

    pub fn assignments_for_epoch(&self, epoch: Epoch) -> BTreeMap<ValidatorId, EpochAssignment> {
        let mut rotated = self.validator_ids.clone();
        let seed = self.epoch_seed(epoch);
        let mut rotation_bytes = [0u8; 8];
        rotation_bytes.copy_from_slice(&seed[..8]);
        let rotation = (u64::from_le_bytes(rotation_bytes) as usize) % rotated.len().max(1);
        rotated.rotate_left(rotation);

        let mut assignments = BTreeMap::new();
        for (position, validator_id) in rotated.iter().copied().enumerate() {
            let mut witnesses = Vec::new();
            let mut relay_targets = Vec::new();
            let max_relations = derived_service_committee_size(rotated.len());
            for offset in 1..=max_relations {
                let target = rotated[(position + offset) % rotated.len()];
                if target != validator_id {
                    relay_targets.push(target);
                    // In the current direct-delivery prototype, only the receiving relay target
                    // can actually observe and receipt the event, so witness assignments need to
                    // line up with relay targets to avoid penalizing validators for missing
                    // third-party observations the network does not yet model.
                    witnesses.push(target);
                }
            }
            assignments.insert(
                validator_id,
                EpochAssignment {
                    epoch,
                    validator_id,
                    witnesses,
                    relay_targets,
                },
            );
        }
        assignments
    }

    pub fn assignment_for(
        &self,
        epoch: Epoch,
        validator_id: ValidatorId,
    ) -> Option<EpochAssignment> {
        self.assignments_for_epoch(epoch).remove(&validator_id)
    }

    pub fn is_witness_for(
        &self,
        witness_validator_id: ValidatorId,
        source_validator_id: ValidatorId,
        epoch: Epoch,
    ) -> bool {
        self.assignment_for(epoch, source_validator_id)
            .map(|assignment| assignment.witnesses.contains(&witness_validator_id))
            .unwrap_or(false)
    }

    pub fn is_relay_target_for(
        &self,
        source_validator_id: ValidatorId,
        destination_validator_id: ValidatorId,
        epoch: Epoch,
    ) -> bool {
        self.assignment_for(epoch, source_validator_id)
            .map(|assignment| assignment.relay_targets.contains(&destination_validator_id))
            .unwrap_or(false)
    }

    pub fn validate_receipt_assignment(&self, receipt: &RelayReceipt) -> Result<(), String> {
        if self.epoch_for_slot(receipt.slot) != receipt.epoch {
            return Err("receipt epoch does not match slot".into());
        }
        if !self.is_witness_for(
            receipt.witness_validator_id,
            receipt.source_validator_id,
            receipt.epoch,
        ) {
            return Err("receipt witness is not assigned for source validator".into());
        }
        if !self.is_relay_target_for(
            receipt.source_validator_id,
            receipt.destination_validator_id,
            receipt.epoch,
        ) {
            return Err("receipt destination is not an assigned relay target".into());
        }
        Ok(())
    }

    pub fn receipts_for_validator(
        &self,
        validator_id: ValidatorId,
        epoch: Epoch,
        receipts: &[RelayReceipt],
    ) -> Vec<RelayReceipt> {
        receipts
            .iter()
            .filter(|receipt| receipt.source_validator_id == validator_id && receipt.epoch == epoch)
            .cloned()
            .collect()
    }

    pub fn compute_receipt_root(receipts: &[RelayReceipt]) -> HashBytes {
        let mut hashes: Vec<_> = receipts.iter().map(canonical_hash).collect();
        hashes.sort_unstable();
        canonical_hash(&hashes)
    }

    pub fn compute_service_score(&self, counters: &ServiceCounters) -> f64 {
        self.compute_service_score_with_weights(counters, &default_service_score_weights())
    }

    pub fn compute_service_score_with_weights(
        &self,
        counters: &ServiceCounters,
        weights: &ServiceScoreWeights,
    ) -> f64 {
        let uptime_ratio = capped_ratio(counters.uptime_windows, counters.total_windows);
        let delivery_ratio = capped_ratio(counters.timely_deliveries, counters.expected_deliveries);
        let diversity_ratio = capped_ratio(counters.distinct_peers, counters.expected_peers);
        let penalty_ratio = ratio(
            (counters.failed_sessions + counters.invalid_receipts)
                .min(counters.total_windows.max(1)),
            counters.total_windows.max(1),
        );
        (weights.uptime_weight * uptime_ratio
            + weights.delivery_weight * delivery_ratio
            + weights.diversity_weight * diversity_ratio
            - weights.penalty_weight * penalty_ratio)
            .clamp(0.0, 1.0)
    }

    pub fn service_committee_for(
        &self,
        epoch: Epoch,
        subject_validator_id: ValidatorId,
    ) -> Vec<ValidatorId> {
        self.assignment_for(epoch, subject_validator_id)
            .map(|assignment| assignment.witnesses)
            .unwrap_or_default()
    }

    pub fn validate_service_aggregate(&self, aggregate: &ServiceAggregate) -> Result<(), String> {
        if !aggregate.is_well_formed() {
            return Err("aggregate payload is malformed".into());
        }
        let committee = self.service_committee_for(aggregate.epoch, aggregate.subject_validator_id);
        let threshold = service_committee_threshold(committee.len());
        if aggregate.committee_size != 0 && aggregate.committee_size != committee.len() {
            return Err("aggregate committee size does not match derived committee".into());
        }
        if aggregate.quorum_threshold != 0 && aggregate.quorum_threshold != threshold {
            return Err("aggregate quorum threshold does not match derived threshold".into());
        }
        let committee_members: BTreeSet<_> = committee.into_iter().collect();
        let mut attested_members = BTreeSet::new();
        for attestation in &aggregate.attestations {
            if !committee_members.contains(&attestation.committee_member_id) {
                return Err("attestation signer is not in the service committee".into());
            }
            if !attested_members.insert(attestation.committee_member_id) {
                return Err("duplicate committee attestation".into());
            }
        }
        if aggregate.coverage_count != 0 && aggregate.coverage_count != attested_members.len() {
            return Err("aggregate coverage count does not match attestations".into());
        }
        Ok(())
    }

    pub fn service_evidence_status(
        &self,
        aggregate: &ServiceAggregate,
    ) -> Result<ServiceEvidenceStatus, String> {
        self.validate_service_aggregate(aggregate)?;
        let committee = self.service_committee_for(aggregate.epoch, aggregate.subject_validator_id);
        let threshold = service_committee_threshold(committee.len());
        if aggregate.attestations.len() >= threshold {
            Ok(ServiceEvidenceStatus::Confirmed)
        } else {
            Ok(ServiceEvidenceStatus::InsufficientEvidence)
        }
    }

    pub fn compute_service_score_from_aggregate(
        &self,
        aggregate: &ServiceAggregate,
        weights: &ServiceScoreWeights,
    ) -> f64 {
        self.compute_service_score_with_weights(&aggregate.aggregate_counters, weights)
    }

    pub fn proposer_is_service_eligible(
        &self,
        aggregate: Option<&ServiceAggregate>,
        threshold: f64,
        current_epoch: Epoch,
    ) -> bool {
        let Some(aggregate) = aggregate else {
            return false;
        };
        if aggregate.epoch >= current_epoch {
            return false;
        }
        if self.service_evidence_status(aggregate).ok() != Some(ServiceEvidenceStatus::Confirmed) {
            return false;
        }
        self.compute_service_score_from_aggregate(aggregate, &default_service_score_weights())
            >= threshold
    }

    pub fn counters_for_validator(
        &self,
        validator_id: ValidatorId,
        epoch: Epoch,
        receipts: &[RelayReceipt],
        failed_sessions: u64,
        invalid_receipts: u64,
    ) -> ServiceCounters {
        let relevant = self.receipts_for_validator(validator_id, epoch, receipts);
        self.counters_from_receipts(
            validator_id,
            epoch,
            &relevant,
            failed_sessions,
            invalid_receipts,
        )
    }

    pub fn counters_for_validator_from_observer(
        &self,
        validator_id: ValidatorId,
        observer_validator_id: ValidatorId,
        epoch: Epoch,
        receipts: &[RelayReceipt],
    ) -> ServiceCounters {
        let assignment = self
            .assignment_for(epoch, validator_id)
            .unwrap_or(EpochAssignment {
                epoch,
                validator_id,
                witnesses: Vec::new(),
                relay_targets: Vec::new(),
            });
        if !assignment.witnesses.contains(&observer_validator_id) {
            return ServiceCounters::default();
        }

        let mut heartbeat_slots = BTreeSet::new();
        let mut timely_deliveries = 0;
        let observed_receipts: Vec<_> = receipts
            .iter()
            .filter(|receipt| {
                receipt.source_validator_id == validator_id
                    && receipt.epoch == epoch
                    && receipt.witness_validator_id == observer_validator_id
            })
            .collect();

        for receipt in &observed_receipts {
            if receipt.message_class == MessageClass::Heartbeat {
                heartbeat_slots.insert(receipt.slot);
            }
            if receipt.latency_bucket_ms <= self.genesis.slot_duration_millis {
                timely_deliveries += 1;
            }
        }

        ServiceCounters {
            uptime_windows: heartbeat_slots.len() as u64,
            total_windows: self.genesis.slots_per_epoch.max(1),
            timely_deliveries,
            expected_deliveries: self.genesis.slots_per_epoch.max(1),
            distinct_peers: u64::from(!observed_receipts.is_empty()),
            expected_peers: 1,
            failed_sessions: 0,
            invalid_receipts: 0,
        }
    }

    pub fn counters_from_receipts(
        &self,
        validator_id: ValidatorId,
        epoch: Epoch,
        receipts: &[RelayReceipt],
        failed_sessions: u64,
        invalid_receipts: u64,
    ) -> ServiceCounters {
        let assignment = self
            .assignment_for(epoch, validator_id)
            .unwrap_or(EpochAssignment {
                epoch,
                validator_id,
                witnesses: Vec::new(),
                relay_targets: Vec::new(),
            });
        let mut heartbeat_slots = BTreeSet::new();
        let mut distinct_peers = BTreeSet::new();
        let mut timely_deliveries = 0;
        let expected_deliveries = self.genesis.slots_per_epoch.max(1);

        for receipt in receipts {
            if receipt.message_class == MessageClass::Heartbeat {
                heartbeat_slots.insert(receipt.slot);
            }
            if receipt.latency_bucket_ms <= self.genesis.slot_duration_millis {
                timely_deliveries += 1;
            }
            distinct_peers.insert(receipt.destination_validator_id);
        }

        ServiceCounters {
            uptime_windows: heartbeat_slots.len() as u64,
            total_windows: self.genesis.slots_per_epoch.max(1),
            timely_deliveries,
            expected_deliveries,
            distinct_peers: distinct_peers.len() as u64,
            expected_peers: assignment.relay_targets.len().max(1) as u64,
            failed_sessions,
            invalid_receipts,
        }
    }

    pub fn commitment_for_validator(
        &self,
        validator_id: ValidatorId,
        epoch: Epoch,
        receipts: &[RelayReceipt],
        failed_sessions: u64,
        invalid_receipts: u64,
    ) -> TopologyCommitment {
        self.commitment_for_validator_with_weights(
            validator_id,
            epoch,
            receipts,
            failed_sessions,
            invalid_receipts,
            &default_service_score_weights(),
        )
    }

    pub fn commitment_for_validator_with_weights(
        &self,
        validator_id: ValidatorId,
        epoch: Epoch,
        receipts: &[RelayReceipt],
        failed_sessions: u64,
        invalid_receipts: u64,
        weights: &ServiceScoreWeights,
    ) -> TopologyCommitment {
        let relevant = self.receipts_for_validator(validator_id, epoch, receipts);
        self.commitment_from_receipts_with_weights(
            validator_id,
            epoch,
            &relevant,
            failed_sessions,
            invalid_receipts,
            weights,
        )
    }

    pub fn commitment_from_receipts(
        &self,
        validator_id: ValidatorId,
        epoch: Epoch,
        receipts: &[RelayReceipt],
        failed_sessions: u64,
        invalid_receipts: u64,
    ) -> TopologyCommitment {
        self.commitment_from_receipts_with_weights(
            validator_id,
            epoch,
            receipts,
            failed_sessions,
            invalid_receipts,
            &default_service_score_weights(),
        )
    }

    pub fn commitment_from_receipts_with_weights(
        &self,
        validator_id: ValidatorId,
        epoch: Epoch,
        receipts: &[RelayReceipt],
        failed_sessions: u64,
        invalid_receipts: u64,
        weights: &ServiceScoreWeights,
    ) -> TopologyCommitment {
        let counters = self.counters_from_receipts(
            validator_id,
            epoch,
            receipts,
            failed_sessions,
            invalid_receipts,
        );
        let relay_score = self.compute_service_score_with_weights(&counters, weights);
        let mut by_message_class = BTreeMap::new();
        let mut peers = BTreeSet::new();
        for receipt in receipts {
            *by_message_class
                .entry(receipt.message_class.clone())
                .or_insert(0) += 1;
            peers.insert(receipt.destination_validator_id);
        }
        TopologyCommitment {
            epoch,
            validator_id,
            receipt_root: Self::compute_receipt_root(receipts),
            receipt_count: receipts.len() as u64,
            summary: CommitmentSummary {
                by_message_class,
                distinct_peers: peers.len() as u64,
                relay_score,
            },
        }
    }

    pub fn validate_block_basic(
        &self,
        block: &Block,
        expected_parent_hash: HashBytes,
        service_score: Option<f64>,
        enable_service_gating: bool,
        service_gating_threshold: f64,
    ) -> Result<(), String> {
        let expected_proposer = self.proposer_for_slot(block.header.slot);
        if block.header.proposer_id != expected_proposer {
            return Err("unexpected proposer".into());
        }
        if block.header.parent_hash != expected_parent_hash {
            return Err("unexpected parent hash".into());
        }
        if enable_service_gating && service_score.unwrap_or_default() < service_gating_threshold {
            return Err("service gating rejected proposer".into());
        }
        Ok(())
    }
}

fn ratio(numerator: u64, denominator: u64) -> f64 {
    if denominator == 0 {
        return 0.0;
    }
    (numerator as f64 / denominator as f64).clamp(0.0, 1.0)
}

fn capped_ratio(numerator: u64, denominator: u64) -> f64 {
    ratio(numerator.min(denominator), denominator)
}

fn service_committee_threshold(committee_size: usize) -> usize {
    if committee_size == 0 {
        return 0;
    }
    ((committee_size * 2) / 3) + 1
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use entangrid_types::{
        GenesisConfig, MessageClass, RelayReceipt, ServiceAggregate, ServiceAttestation,
        ServiceScoreWeights, ValidatorConfig, aggregate_service_counters, empty_hash,
        service_attestation_root,
    };

    use super::*;

    fn sample_genesis() -> GenesisConfig {
        GenesisConfig {
            chain_id: "test".into(),
            epoch_seed: empty_hash(),
            genesis_time_unix_millis: 0,
            slot_duration_millis: 1_000,
            slots_per_epoch: 10,
            max_txs_per_block: 16,
            witness_count: 2,
            validators: vec![
                ValidatorConfig {
                    validator_id: 1,
                    stake: 100,
                    address: "127.0.0.1:3001".into(),
                    dev_secret: "one".into(),
                    public_identity: vec![],
                },
                ValidatorConfig {
                    validator_id: 2,
                    stake: 100,
                    address: "127.0.0.1:3002".into(),
                    dev_secret: "two".into(),
                    public_identity: vec![],
                },
                ValidatorConfig {
                    validator_id: 3,
                    stake: 100,
                    address: "127.0.0.1:3003".into(),
                    dev_secret: "three".into(),
                    public_identity: vec![],
                },
                ValidatorConfig {
                    validator_id: 4,
                    stake: 100,
                    address: "127.0.0.1:3004".into(),
                    dev_secret: "four".into(),
                    public_identity: vec![],
                },
            ],
            initial_balances: BTreeMap::new(),
        }
    }

    fn sample_genesis_with_validators(count: u64) -> GenesisConfig {
        GenesisConfig {
            chain_id: "test".into(),
            epoch_seed: empty_hash(),
            genesis_time_unix_millis: 0,
            slot_duration_millis: 1_000,
            slots_per_epoch: 10,
            max_txs_per_block: 16,
            witness_count: 2,
            validators: (1..=count)
                .map(|validator_id| ValidatorConfig {
                    validator_id,
                    stake: 100,
                    address: format!("127.0.0.1:{}", 3000 + validator_id),
                    dev_secret: format!("secret-{validator_id}"),
                    public_identity: vec![],
                })
                .collect(),
            initial_balances: BTreeMap::new(),
        }
    }

    #[test]
    fn proposer_selection_is_deterministic() {
        let engine = ConsensusEngine::new(sample_genesis());
        assert_eq!(engine.proposer_for_slot(5), engine.proposer_for_slot(5));
    }

    #[test]
    fn assignments_never_include_self() {
        let engine = ConsensusEngine::new(sample_genesis());
        let assignments = engine.assignments_for_epoch(0);
        for (validator_id, assignment) in assignments {
            assert!(!assignment.witnesses.contains(&validator_id));
            assert!(!assignment.relay_targets.contains(&validator_id));
        }
    }

    #[test]
    fn direct_delivery_witnesses_match_relay_targets() {
        let engine = ConsensusEngine::new(sample_genesis());
        let assignments = engine.assignments_for_epoch(2);
        for assignment in assignments.values() {
            assert_eq!(assignment.witnesses, assignment.relay_targets);
        }
    }

    #[test]
    fn service_score_caps_duplicate_credit() {
        let engine = ConsensusEngine::new(sample_genesis());
        let score = engine.compute_service_score(&ServiceCounters {
            uptime_windows: 1,
            total_windows: 5,
            timely_deliveries: 10,
            expected_deliveries: 5,
            distinct_peers: 1,
            expected_peers: 2,
            failed_sessions: 0,
            invalid_receipts: 0,
        });
        assert!((score - 0.675).abs() < f64::EPSILON);
    }

    #[test]
    fn service_score_respects_penalty_weight() {
        let engine = ConsensusEngine::new(sample_genesis());
        let counters = ServiceCounters {
            uptime_windows: 5,
            total_windows: 5,
            timely_deliveries: 5,
            expected_deliveries: 5,
            distinct_peers: 2,
            expected_peers: 2,
            failed_sessions: 1,
            invalid_receipts: 0,
        };
        let lower_penalty = engine.compute_service_score_with_weights(
            &counters,
            &ServiceScoreWeights {
                penalty_weight: 0.5,
                ..Default::default()
            },
        );
        let higher_penalty = engine.compute_service_score_with_weights(
            &counters,
            &ServiceScoreWeights {
                penalty_weight: 1.5,
                ..Default::default()
            },
        );
        assert!(lower_penalty > higher_penalty);
    }

    #[test]
    fn receipt_assignment_validation_requires_assigned_witness_and_target() {
        let engine = ConsensusEngine::new(sample_genesis());
        let assignment = engine.assignment_for(0, 1).unwrap();
        let receipt = RelayReceipt {
            epoch: 0,
            slot: 0,
            source_validator_id: 1,
            destination_validator_id: assignment.relay_targets[0],
            witness_validator_id: assignment.witnesses[0],
            message_class: MessageClass::Heartbeat,
            transcript_digest: [0u8; 32],
            latency_bucket_ms: 100,
            byte_count_bucket: 1,
            sequence_number: 0,
            signature: vec![],
        };
        assert!(engine.validate_receipt_assignment(&receipt).is_ok());

        let mut wrong_target = receipt.clone();
        wrong_target.destination_validator_id = 1;
        assert!(engine.validate_receipt_assignment(&wrong_target).is_err());

        let mut wrong_witness = receipt;
        wrong_witness.witness_validator_id = 1;
        assert!(engine.validate_receipt_assignment(&wrong_witness).is_err());
    }

    #[test]
    fn service_committee_matches_observable_witness_assignments() {
        let four = ConsensusEngine::new(sample_genesis_with_validators(4));
        let eight = ConsensusEngine::new(sample_genesis_with_validators(8));
        let four_assignment = four.assignment_for(2, 1).unwrap();
        let eight_assignment = eight.assignment_for(2, 1).unwrap();
        assert_eq!(four.service_committee_for(2, 1), four_assignment.witnesses);
        assert_eq!(
            eight.service_committee_for(2, 1),
            eight_assignment.witnesses
        );
        assert_eq!(
            four.service_committee_for(2, 1).len(),
            derived_service_committee_size(4)
        );
        assert_eq!(
            eight.service_committee_for(2, 1).len(),
            derived_service_committee_size(8)
        );
        assert_eq!(four.service_committee_for(2, 1).len(), derived_service_committee_size(4));
    }

    #[test]
    fn aggregate_validation_requires_matching_committee_and_counters() {
        let engine = ConsensusEngine::new(sample_genesis_with_validators(8));
        let committee = engine.service_committee_for(4, 3);
        let attestations: Vec<_> = committee
            .iter()
            .take(3)
            .map(|committee_member_id| ServiceAttestation {
                subject_validator_id: 3,
                committee_member_id: *committee_member_id,
                epoch: 4,
                counters: ServiceCounters {
                    uptime_windows: 1,
                    total_windows: 1,
                    timely_deliveries: 2,
                    expected_deliveries: 2,
                    distinct_peers: 1,
                    expected_peers: 1,
                    failed_sessions: 0,
                    invalid_receipts: 0,
                },
                signature: vec![],
            })
            .collect();
        let aggregate = ServiceAggregate {
            subject_validator_id: 3,
            epoch: 4,
            attestation_root: service_attestation_root(&attestations),
            aggregate_counters: aggregate_service_counters(&attestations),
            attestations: attestations.clone(),
            committee_size: committee.len(),
            quorum_threshold: service_committee_threshold(committee.len()),
            coverage_count: attestations.len(),
        };
        assert!(engine.validate_service_aggregate(&aggregate).is_ok());

        let mut wrong_counters = aggregate.clone();
        wrong_counters.aggregate_counters.failed_sessions = 1;
        assert!(engine.validate_service_aggregate(&wrong_counters).is_err());
    }

    #[test]
    fn below_quorum_aggregate_is_structurally_valid_but_insufficient() {
        let engine = ConsensusEngine::new(sample_genesis_with_validators(8));
        let committee = engine.service_committee_for(4, 3);
        let attestations: Vec<_> = committee
            .iter()
            .take(service_committee_threshold(committee.len()) - 1)
            .map(|committee_member_id| ServiceAttestation {
                subject_validator_id: 3,
                committee_member_id: *committee_member_id,
                epoch: 4,
                counters: ServiceCounters {
                    uptime_windows: 1,
                    total_windows: 1,
                    timely_deliveries: 2,
                    expected_deliveries: 2,
                    distinct_peers: 1,
                    expected_peers: 1,
                    failed_sessions: 0,
                    invalid_receipts: 0,
                },
                signature: vec![],
            })
            .collect();
        let aggregate = ServiceAggregate {
            subject_validator_id: 3,
            epoch: 4,
            attestation_root: service_attestation_root(&attestations),
            aggregate_counters: aggregate_service_counters(&attestations),
            attestations: attestations.clone(),
            committee_size: committee.len(),
            quorum_threshold: service_committee_threshold(committee.len()),
            coverage_count: attestations.len(),
        };
        assert!(engine.validate_service_aggregate(&aggregate).is_ok());
        assert_eq!(
            engine.service_evidence_status(&aggregate).unwrap(),
            ServiceEvidenceStatus::InsufficientEvidence
        );
    }

    #[test]
    fn proposer_eligibility_requires_prior_epoch_aggregate() {
        let engine = ConsensusEngine::new(sample_genesis());
        let committee = engine.service_committee_for(2, 1);
        let attestations: Vec<_> = committee
            .iter()
            .map(|committee_member_id| ServiceAttestation {
                subject_validator_id: 1,
                committee_member_id: *committee_member_id,
                epoch: 2,
                counters: ServiceCounters {
                    uptime_windows: 10,
                    total_windows: 10,
                    timely_deliveries: 10,
                    expected_deliveries: 10,
                    distinct_peers: 2,
                    expected_peers: 2,
                    failed_sessions: 0,
                    invalid_receipts: 0,
                },
                signature: vec![],
            })
            .collect();
        let aggregate = ServiceAggregate {
            subject_validator_id: 1,
            epoch: 2,
            attestation_root: service_attestation_root(&attestations),
            aggregate_counters: aggregate_service_counters(&attestations),
            attestations,
            committee_size: committee.len(),
            quorum_threshold: service_committee_threshold(committee.len()),
            coverage_count: committee.len(),
        };
        assert!(engine.proposer_is_service_eligible(Some(&aggregate), 0.4, 3));
        assert!(!engine.proposer_is_service_eligible(Some(&aggregate), 0.4, 2));
        assert!(!engine.proposer_is_service_eligible(None, 0.4, 3));
    }
}
