use std::collections::{BTreeMap, BTreeSet};

use entangrid_types::{
    Block, CommitmentSummary, Epoch, EpochAssignment, GenesisConfig, HashBytes, MessageClass,
    RelayReceipt, Slot, TopologyCommitment, ValidatorId, canonical_hash, hash_many,
    now_unix_millis,
};

#[derive(Clone, Debug)]
pub struct ConsensusEngine {
    genesis: GenesisConfig,
    validator_ids: Vec<ValidatorId>,
}

#[derive(Clone, Debug, Default)]
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
            let max_relations = self
                .genesis
                .witness_count
                .min(rotated.len().saturating_sub(1));
            for offset in 1..=max_relations {
                let target = rotated[(position + offset) % rotated.len()];
                let witness = rotated[(position + rotated.len() - offset) % rotated.len()];
                if target != validator_id {
                    relay_targets.push(target);
                }
                if witness != validator_id {
                    witnesses.push(witness);
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

    pub fn compute_receipt_root(receipts: &[RelayReceipt]) -> HashBytes {
        let mut hashes: Vec<_> = receipts.iter().map(canonical_hash).collect();
        hashes.sort_unstable();
        canonical_hash(&hashes)
    }

    pub fn compute_service_score(&self, counters: &ServiceCounters) -> f64 {
        let uptime_ratio = ratio(counters.uptime_windows, counters.total_windows);
        let delivery_ratio = ratio(counters.timely_deliveries, counters.expected_deliveries);
        let diversity_ratio = ratio(counters.distinct_peers, counters.expected_peers);
        let penalty_ratio = ratio(
            counters.failed_sessions + counters.invalid_receipts,
            counters.total_windows.max(1),
        );
        (0.25 * uptime_ratio + 0.50 * delivery_ratio + 0.25 * diversity_ratio - penalty_ratio)
            .clamp(0.0, 1.0)
    }

    pub fn counters_for_validator(
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
        let relevant: Vec<_> = receipts
            .iter()
            .filter(|receipt| receipt.source_validator_id == validator_id && receipt.epoch == epoch)
            .collect();
        let mut heartbeat_slots = BTreeSet::new();
        let mut distinct_peers = BTreeSet::new();
        let mut timely_deliveries = 0;
        let expected_deliveries = self.genesis.slots_per_epoch.max(1);

        for receipt in &relevant {
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
        let relevant: Vec<RelayReceipt> = receipts
            .iter()
            .filter(|receipt| receipt.source_validator_id == validator_id && receipt.epoch == epoch)
            .cloned()
            .collect();

        let counters = self.counters_for_validator(
            validator_id,
            epoch,
            &relevant,
            failed_sessions,
            invalid_receipts,
        );
        let relay_score = self.compute_service_score(&counters);
        let mut by_message_class = BTreeMap::new();
        let mut peers = BTreeSet::new();
        for receipt in &relevant {
            *by_message_class
                .entry(receipt.message_class.clone())
                .or_insert(0) += 1;
            peers.insert(receipt.destination_validator_id);
        }
        TopologyCommitment {
            epoch,
            validator_id,
            receipt_root: Self::compute_receipt_root(&relevant),
            receipt_count: relevant.len() as u64,
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
    ) -> Result<(), String> {
        let expected_proposer = self.proposer_for_slot(block.header.slot);
        if block.header.proposer_id != expected_proposer {
            return Err("unexpected proposer".into());
        }
        if block.header.parent_hash != expected_parent_hash {
            return Err("unexpected parent hash".into());
        }
        if enable_service_gating && service_score.unwrap_or_default() < 0.40 {
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

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use entangrid_types::{GenesisConfig, ValidatorConfig, empty_hash};

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
}
