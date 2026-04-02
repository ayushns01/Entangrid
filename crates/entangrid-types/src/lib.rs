use std::{collections::BTreeMap, fmt};

use bincode::config::standard;
use serde::{
    Deserialize, Deserializer, Serialize, Serializer,
    de::{self, MapAccess, SeqAccess, Visitor},
    ser::SerializeStruct,
};

pub type ValidatorId = u64;
pub type Slot = u64;
pub type Epoch = u64;
pub type HashBytes = [u8; 32];
pub type AccountId = String;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum SignatureScheme {
    DevDeterministic,
    Ed25519,
    MlDsa,
    Hybrid,
}

impl Default for SignatureScheme {
    fn default() -> Self {
        Self::DevDeterministic
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum PublicKeyScheme {
    DevDeterministic,
    Ed25519,
    MlDsa,
    Hybrid,
}

impl Default for PublicKeyScheme {
    fn default() -> Self {
        Self::DevDeterministic
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[serde(rename_all = "snake_case")]
pub enum SigningBackendKind {
    DevDeterministic,
    MlDsa65Experimental,
    HybridDeterministicMlDsaExperimental,
}

impl Default for SigningBackendKind {
    fn default() -> Self {
        Self::DevDeterministic
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SignatureComponent {
    pub scheme: SignatureScheme,
    pub bytes: Vec<u8>,
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TypedSignature {
    pub scheme: SignatureScheme,
    pub bytes: Vec<u8>,
    pub components: Vec<SignatureComponent>,
}

impl Default for TypedSignature {
    fn default() -> Self {
        Self::single(SignatureScheme::DevDeterministic, Vec::new())
    }
}

impl TypedSignature {
    pub fn single(scheme: SignatureScheme, bytes: Vec<u8>) -> Self {
        Self {
            scheme,
            bytes,
            components: Vec::new(),
        }
    }

    pub fn try_hybrid(components: Vec<SignatureComponent>) -> Result<Self, String> {
        if components.is_empty() {
            return Err("hybrid signature requires at least one component".into());
        }
        let mut seen = Vec::new();
        for component in &components {
            if component.scheme == SignatureScheme::Hybrid {
                return Err("hybrid signature cannot contain a nested hybrid component".into());
            }
            if seen.contains(&component.scheme) {
                return Err("hybrid signature cannot contain duplicate component schemes".into());
            }
            seen.push(component.scheme.clone());
        }
        Ok(Self {
            scheme: SignatureScheme::Hybrid,
            bytes: Vec::new(),
            components,
        })
    }

    pub fn scheme(&self) -> SignatureScheme {
        self.scheme.clone()
    }

    pub fn as_single_bytes(&self) -> Option<&[u8]> {
        if self.scheme == SignatureScheme::Hybrid {
            None
        } else {
            Some(self.bytes.as_slice())
        }
    }

    pub fn components(&self) -> &[SignatureComponent] {
        self.components.as_slice()
    }

    pub fn component_bytes(&self, scheme: SignatureScheme) -> Option<&[u8]> {
        if self.scheme == scheme && self.scheme != SignatureScheme::Hybrid {
            Some(self.bytes.as_slice())
        } else {
            self.components
                .iter()
                .find(|component| component.scheme == scheme)
                .map(|component| component.bytes.as_slice())
        }
    }
}

impl Serialize for TypedSignature {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        if serializer.is_human_readable() {
            let field_count = if self.components.is_empty() { 2 } else { 3 };
            let mut state = serializer.serialize_struct("TypedSignature", field_count)?;
            state.serialize_field("scheme", &self.scheme)?;
            state.serialize_field("bytes", &self.bytes)?;
            if !self.components.is_empty() {
                state.serialize_field("components", &self.components)?;
            }
            state.end()
        } else {
            let payload = if self.scheme == SignatureScheme::Hybrid {
                bincode::serde::encode_to_vec(&self.components, standard())
                    .map_err(serde::ser::Error::custom)?
            } else {
                self.bytes.clone()
            };
            let mut state = serializer.serialize_struct("TypedSignature", 2)?;
            state.serialize_field("scheme", &self.scheme)?;
            state.serialize_field("bytes", &payload)?;
            state.end()
        }
    }
}

impl<'de> Deserialize<'de> for TypedSignature {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        const FIELDS: &[&str] = &["scheme", "bytes", "components"];

        enum Field {
            Scheme,
            Bytes,
            Components,
        }

        impl<'de> Deserialize<'de> for Field {
            fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
            where
                D: Deserializer<'de>,
            {
                struct FieldVisitor;

                impl<'de> Visitor<'de> for FieldVisitor {
                    type Value = Field;

                    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                        formatter.write_str("`scheme`, `bytes`, or `components`")
                    }

                    fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
                    where
                        E: de::Error,
                    {
                        match value {
                            "scheme" => Ok(Field::Scheme),
                            "bytes" => Ok(Field::Bytes),
                            "components" => Ok(Field::Components),
                            _ => Err(de::Error::unknown_field(value, FIELDS)),
                        }
                    }
                }

                deserializer.deserialize_identifier(FieldVisitor)
            }
        }

        struct TypedSignatureVisitor;

        impl<'de> Visitor<'de> for TypedSignatureVisitor {
            type Value = TypedSignature;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("a typed signature")
            }

            fn visit_seq<A>(self, mut seq: A) -> Result<Self::Value, A::Error>
            where
                A: SeqAccess<'de>,
            {
                let scheme = seq
                    .next_element()?
                    .ok_or_else(|| de::Error::invalid_length(0, &self))?;
                let bytes = seq
                    .next_element()?
                    .ok_or_else(|| de::Error::invalid_length(1, &self))?;
                let components = seq.next_element()?.unwrap_or_default();
                Ok(TypedSignature {
                    scheme,
                    bytes,
                    components,
                })
            }

            fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
            where
                A: MapAccess<'de>,
            {
                let mut scheme = None;
                let mut bytes = None;
                let mut components = None;
                while let Some(key) = map.next_key()? {
                    match key {
                        Field::Scheme => {
                            if scheme.is_some() {
                                return Err(de::Error::duplicate_field("scheme"));
                            }
                            scheme = Some(map.next_value()?);
                        }
                        Field::Bytes => {
                            if bytes.is_some() {
                                return Err(de::Error::duplicate_field("bytes"));
                            }
                            bytes = Some(map.next_value()?);
                        }
                        Field::Components => {
                            if components.is_some() {
                                return Err(de::Error::duplicate_field("components"));
                            }
                            components = Some(map.next_value()?);
                        }
                    }
                }
                Ok(TypedSignature {
                    scheme: scheme.ok_or_else(|| de::Error::missing_field("scheme"))?,
                    bytes: bytes.ok_or_else(|| de::Error::missing_field("bytes"))?,
                    components: components.unwrap_or_default(),
                })
            }
        }

        if deserializer.is_human_readable() {
            deserializer.deserialize_struct("TypedSignature", FIELDS, TypedSignatureVisitor)
        } else {
            #[derive(Deserialize)]
            struct BinaryTypedSignature {
                scheme: SignatureScheme,
                bytes: Vec<u8>,
            }

            let raw = BinaryTypedSignature::deserialize(deserializer)?;
            if raw.scheme == SignatureScheme::Hybrid {
                let (components, consumed): (Vec<SignatureComponent>, usize) =
                    bincode::serde::decode_from_slice(&raw.bytes, standard())
                        .map_err(de::Error::custom)?;
                if consumed != raw.bytes.len() {
                    return Err(de::Error::custom(
                        "hybrid signature payload contained trailing bytes",
                    ));
                }
                Ok(TypedSignature {
                    scheme: SignatureScheme::Hybrid,
                    bytes: Vec::new(),
                    components,
                })
            } else {
                Ok(TypedSignature {
                    scheme: raw.scheme,
                    bytes: raw.bytes,
                    components: Vec::new(),
                })
            }
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PublicIdentityComponent {
    pub scheme: PublicKeyScheme,
    pub bytes: Vec<u8>,
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PublicIdentity {
    pub scheme: PublicKeyScheme,
    pub bytes: Vec<u8>,
    pub components: Vec<PublicIdentityComponent>,
}

impl Default for PublicIdentity {
    fn default() -> Self {
        Self::single(PublicKeyScheme::DevDeterministic, Vec::new())
    }
}

impl Serialize for PublicIdentity {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        if serializer.is_human_readable() {
            let field_count = if self.components.is_empty() { 2 } else { 3 };
            let mut state = serializer.serialize_struct("PublicIdentity", field_count)?;
            state.serialize_field("scheme", &self.scheme)?;
            state.serialize_field("bytes", &self.bytes)?;
            if !self.components.is_empty() {
                state.serialize_field("components", &self.components)?;
            }
            state.end()
        } else {
            let payload = if self.scheme == PublicKeyScheme::Hybrid {
                bincode::serde::encode_to_vec(&self.components, standard())
                    .map_err(serde::ser::Error::custom)?
            } else {
                self.bytes.clone()
            };
            let mut state = serializer.serialize_struct("PublicIdentity", 2)?;
            state.serialize_field("scheme", &self.scheme)?;
            state.serialize_field("bytes", &payload)?;
            state.end()
        }
    }
}

impl<'de> Deserialize<'de> for PublicIdentity {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        const FIELDS: &[&str] = &["scheme", "bytes", "components"];

        enum Field {
            Scheme,
            Bytes,
            Components,
        }

        impl<'de> Deserialize<'de> for Field {
            fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
            where
                D: Deserializer<'de>,
            {
                struct FieldVisitor;

                impl<'de> Visitor<'de> for FieldVisitor {
                    type Value = Field;

                    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                        formatter.write_str("`scheme`, `bytes`, or `components`")
                    }

                    fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
                    where
                        E: de::Error,
                    {
                        match value {
                            "scheme" => Ok(Field::Scheme),
                            "bytes" => Ok(Field::Bytes),
                            "components" => Ok(Field::Components),
                            _ => Err(de::Error::unknown_field(value, FIELDS)),
                        }
                    }
                }

                deserializer.deserialize_identifier(FieldVisitor)
            }
        }

        struct PublicIdentityVisitor;

        impl<'de> Visitor<'de> for PublicIdentityVisitor {
            type Value = PublicIdentity;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("a public identity")
            }

            fn visit_seq<A>(self, mut seq: A) -> Result<Self::Value, A::Error>
            where
                A: SeqAccess<'de>,
            {
                let scheme = seq
                    .next_element()?
                    .ok_or_else(|| de::Error::invalid_length(0, &self))?;
                let bytes = seq
                    .next_element()?
                    .ok_or_else(|| de::Error::invalid_length(1, &self))?;
                let components = seq.next_element()?.unwrap_or_default();
                Ok(PublicIdentity {
                    scheme,
                    bytes,
                    components,
                })
            }

            fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
            where
                A: MapAccess<'de>,
            {
                let mut scheme = None;
                let mut bytes = None;
                let mut components = None;
                while let Some(key) = map.next_key()? {
                    match key {
                        Field::Scheme => {
                            if scheme.is_some() {
                                return Err(de::Error::duplicate_field("scheme"));
                            }
                            scheme = Some(map.next_value()?);
                        }
                        Field::Bytes => {
                            if bytes.is_some() {
                                return Err(de::Error::duplicate_field("bytes"));
                            }
                            bytes = Some(map.next_value()?);
                        }
                        Field::Components => {
                            if components.is_some() {
                                return Err(de::Error::duplicate_field("components"));
                            }
                            components = Some(map.next_value()?);
                        }
                    }
                }
                Ok(PublicIdentity {
                    scheme: scheme.ok_or_else(|| de::Error::missing_field("scheme"))?,
                    bytes: bytes.ok_or_else(|| de::Error::missing_field("bytes"))?,
                    components: components.unwrap_or_default(),
                })
            }
        }

        if deserializer.is_human_readable() {
            deserializer.deserialize_struct("PublicIdentity", FIELDS, PublicIdentityVisitor)
        } else {
            #[derive(Deserialize)]
            struct BinaryPublicIdentity {
                scheme: PublicKeyScheme,
                bytes: Vec<u8>,
            }

            let raw = BinaryPublicIdentity::deserialize(deserializer)?;
            if raw.scheme == PublicKeyScheme::Hybrid {
                let (components, consumed): (Vec<PublicIdentityComponent>, usize) =
                    bincode::serde::decode_from_slice(&raw.bytes, standard())
                        .map_err(de::Error::custom)?;
                if consumed != raw.bytes.len() {
                    return Err(de::Error::custom(
                        "hybrid public identity payload contained trailing bytes",
                    ));
                }
                Ok(PublicIdentity {
                    scheme: PublicKeyScheme::Hybrid,
                    bytes: Vec::new(),
                    components,
                })
            } else {
                Ok(PublicIdentity {
                    scheme: raw.scheme,
                    bytes: raw.bytes,
                    components: Vec::new(),
                })
            }
        }
    }
}

impl PublicIdentity {
    pub fn single(scheme: PublicKeyScheme, bytes: Vec<u8>) -> Self {
        Self {
            scheme,
            bytes,
            components: Vec::new(),
        }
    }

    pub fn try_hybrid(components: Vec<PublicIdentityComponent>) -> Result<Self, String> {
        if components.is_empty() {
            return Err("hybrid public identity requires at least one component".into());
        }
        let mut seen = Vec::new();
        for component in &components {
            if component.scheme == PublicKeyScheme::Hybrid {
                return Err(
                    "hybrid public identity cannot contain a nested hybrid component".into(),
                );
            }
            if seen.contains(&component.scheme) {
                return Err(
                    "hybrid public identity cannot contain duplicate component schemes".into(),
                );
            }
            seen.push(component.scheme.clone());
        }
        Ok(Self {
            scheme: PublicKeyScheme::Hybrid,
            bytes: Vec::new(),
            components,
        })
    }

    pub fn scheme(&self) -> PublicKeyScheme {
        self.scheme.clone()
    }

    pub fn as_single_bytes(&self) -> Option<&[u8]> {
        if self.scheme == PublicKeyScheme::Hybrid {
            None
        } else {
            Some(self.bytes.as_slice())
        }
    }

    pub fn components(&self) -> &[PublicIdentityComponent] {
        self.components.as_slice()
    }

    pub fn component_bytes(&self, scheme: PublicKeyScheme) -> Option<&[u8]> {
        if self.scheme == scheme && self.scheme != PublicKeyScheme::Hybrid {
            Some(self.bytes.as_slice())
        } else {
            self.components
                .iter()
                .find(|component| component.scheme == scheme)
                .map(|component| component.bytes.as_slice())
        }
    }
}

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
    pub public_identity: PublicIdentity,
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
    #[serde(default)]
    pub signing_backend: SigningBackendKind,
    #[serde(default)]
    pub signing_key_path: Option<String>,
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
    pub signature: TypedSignature,
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
    pub signature: TypedSignature,
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
    pub signature: TypedSignature,
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
    #[serde(default)]
    pub proposal_votes: Vec<ProposalVote>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct ProposalVote {
    pub validator_id: ValidatorId,
    pub block_hash: HashBytes,
    pub block_number: u64,
    pub epoch: Epoch,
    pub slot: Slot,
    pub signature: TypedSignature,
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
    pub signature: TypedSignature,
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
    pub signature: TypedSignature,
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
            signature: TypedSignature::single(SignatureScheme::DevDeterministic, vec![1, 2, 3]),
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
            signature: TypedSignature::single(SignatureScheme::DevDeterministic, vec![4, 5, 6]),
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

    #[test]
    fn typed_signature_round_trips_with_scheme_metadata() {
        let signature = TypedSignature::single(SignatureScheme::DevDeterministic, vec![1, 2, 3, 4]);
        let bytes = bincode::serde::encode_to_vec(&signature, bincode::config::standard()).unwrap();
        let (decoded, _): (TypedSignature, usize) =
            bincode::serde::decode_from_slice(&bytes, bincode::config::standard()).unwrap();
        assert_eq!(decoded, signature);
    }

    #[test]
    fn public_identity_round_trips_with_scheme_metadata() {
        let identity = PublicIdentity::single(PublicKeyScheme::DevDeterministic, vec![9, 8, 7, 6]);
        let bytes = bincode::serde::encode_to_vec(&identity, bincode::config::standard()).unwrap();
        let (decoded, _): (PublicIdentity, usize) =
            bincode::serde::decode_from_slice(&bytes, bincode::config::standard()).unwrap();
        assert_eq!(decoded, identity);
    }

    #[test]
    fn legacy_single_signature_encoding_still_round_trips_through_typed_signature() {
        #[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
        struct LegacyTypedSignature {
            scheme: SignatureScheme,
            bytes: Vec<u8>,
        }

        let legacy = LegacyTypedSignature {
            scheme: SignatureScheme::DevDeterministic,
            bytes: vec![1, 2, 3, 4],
        };
        let bytes = bincode::serde::encode_to_vec(&legacy, bincode::config::standard()).unwrap();
        let (decoded, _): (TypedSignature, usize) =
            bincode::serde::decode_from_slice(&bytes, bincode::config::standard()).unwrap();
        assert_eq!(
            decoded,
            TypedSignature::single(SignatureScheme::DevDeterministic, vec![1, 2, 3, 4])
        );
    }

    #[test]
    fn single_signature_encoding_matches_legacy_form_exactly() {
        #[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
        struct LegacyTypedSignature {
            scheme: SignatureScheme,
            bytes: Vec<u8>,
        }

        let legacy = LegacyTypedSignature {
            scheme: SignatureScheme::DevDeterministic,
            bytes: vec![1, 2, 3, 4],
        };
        let current = TypedSignature::single(SignatureScheme::DevDeterministic, vec![1, 2, 3, 4]);
        let legacy_bytes =
            bincode::serde::encode_to_vec(&legacy, bincode::config::standard()).unwrap();
        let current_bytes =
            bincode::serde::encode_to_vec(&current, bincode::config::standard()).unwrap();
        assert_eq!(current_bytes, legacy_bytes);
    }

    #[test]
    fn hybrid_signature_round_trips_with_multiple_components() {
        let signature = TypedSignature::try_hybrid(vec![
            SignatureComponent {
                scheme: SignatureScheme::DevDeterministic,
                bytes: vec![1, 2, 3],
            },
            SignatureComponent {
                scheme: SignatureScheme::MlDsa,
                bytes: vec![4, 5, 6],
            },
        ])
        .unwrap();
        let bytes = bincode::serde::encode_to_vec(&signature, bincode::config::standard()).unwrap();
        let (decoded, _): (TypedSignature, usize) =
            bincode::serde::decode_from_slice(&bytes, bincode::config::standard()).unwrap();
        assert_eq!(decoded, signature);
        assert_eq!(decoded.scheme(), SignatureScheme::Hybrid);
        assert_eq!(
            decoded.component_bytes(SignatureScheme::MlDsa),
            Some([4, 5, 6].as_slice())
        );
    }

    #[test]
    fn duplicate_hybrid_signature_schemes_are_rejected() {
        let error = TypedSignature::try_hybrid(vec![
            SignatureComponent {
                scheme: SignatureScheme::DevDeterministic,
                bytes: vec![1],
            },
            SignatureComponent {
                scheme: SignatureScheme::DevDeterministic,
                bytes: vec![2],
            },
        ])
        .unwrap_err();
        assert!(error.contains("duplicate"));
    }

    #[test]
    fn legacy_single_identity_encoding_still_round_trips_through_public_identity() {
        #[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
        struct LegacyPublicIdentity {
            scheme: PublicKeyScheme,
            bytes: Vec<u8>,
        }

        let legacy = LegacyPublicIdentity {
            scheme: PublicKeyScheme::DevDeterministic,
            bytes: vec![9, 8, 7, 6],
        };
        let bytes = bincode::serde::encode_to_vec(&legacy, bincode::config::standard()).unwrap();
        let (decoded, _): (PublicIdentity, usize) =
            bincode::serde::decode_from_slice(&bytes, bincode::config::standard()).unwrap();
        assert_eq!(
            decoded,
            PublicIdentity::single(PublicKeyScheme::DevDeterministic, vec![9, 8, 7, 6])
        );
    }

    #[test]
    fn single_public_identity_encoding_matches_legacy_form_exactly() {
        #[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
        struct LegacyPublicIdentity {
            scheme: PublicKeyScheme,
            bytes: Vec<u8>,
        }

        let legacy = LegacyPublicIdentity {
            scheme: PublicKeyScheme::DevDeterministic,
            bytes: vec![9, 8, 7, 6],
        };
        let current = PublicIdentity::single(PublicKeyScheme::DevDeterministic, vec![9, 8, 7, 6]);
        let legacy_bytes =
            bincode::serde::encode_to_vec(&legacy, bincode::config::standard()).unwrap();
        let current_bytes =
            bincode::serde::encode_to_vec(&current, bincode::config::standard()).unwrap();
        assert_eq!(current_bytes, legacy_bytes);
    }

    #[test]
    fn hybrid_public_identity_round_trips_with_multiple_components() {
        let identity = PublicIdentity::try_hybrid(vec![
            PublicIdentityComponent {
                scheme: PublicKeyScheme::DevDeterministic,
                bytes: vec![7, 7],
            },
            PublicIdentityComponent {
                scheme: PublicKeyScheme::MlDsa,
                bytes: vec![8, 8, 8],
            },
        ])
        .unwrap();
        let bytes = bincode::serde::encode_to_vec(&identity, bincode::config::standard()).unwrap();
        let (decoded, _): (PublicIdentity, usize) =
            bincode::serde::decode_from_slice(&bytes, bincode::config::standard()).unwrap();
        assert_eq!(decoded, identity);
        assert_eq!(decoded.scheme(), PublicKeyScheme::Hybrid);
        assert_eq!(
            decoded.component_bytes(PublicKeyScheme::MlDsa),
            Some([8, 8, 8].as_slice())
        );
    }

    #[test]
    fn signing_backend_defaults_to_deterministic_when_omitted_from_node_config() {
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
        assert_eq!(parsed.signing_backend, SigningBackendKind::DevDeterministic);
        assert_eq!(parsed.signing_key_path, None);
    }

    #[test]
    fn signing_backend_round_trips_through_toml() {
        let config = NodeConfig {
            validator_id: 1,
            data_dir: "/tmp/node-1".into(),
            genesis_path: "/tmp/genesis.toml".into(),
            listen_address: "127.0.0.1:3001".into(),
            peers: Vec::new(),
            log_path: "/tmp/events.log".into(),
            metrics_path: "/tmp/metrics.json".into(),
            feature_flags: FeatureFlags::default(),
            fault_profile: FaultProfile::default(),
            sync_on_startup: true,
            signing_backend: SigningBackendKind::MlDsa65Experimental,
            signing_key_path: Some("/tmp/ml-dsa.sk".into()),
        };
        let serialized = toml::to_string(&config).unwrap();
        let decoded: NodeConfig = toml::from_str(&serialized).unwrap();
        assert_eq!(
            decoded.signing_backend,
            SigningBackendKind::MlDsa65Experimental
        );
        assert_eq!(decoded.signing_key_path.as_deref(), Some("/tmp/ml-dsa.sk"));
    }
}
