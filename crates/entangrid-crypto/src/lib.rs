use std::{collections::BTreeMap, sync::Arc, time::Instant};

use anyhow::{Result, anyhow};
use entangrid_types::{
    GenesisConfig, HashBytes, NodeConfig, PublicIdentity, PublicKeyScheme, SignatureScheme,
    SigningBackendKind, TypedSignature, ValidatorConfig, ValidatorId, hash_many,
};
#[cfg(feature = "pq-ml-dsa")]
use ml_dsa::{
    EncodedSigningKey, EncodedVerifyingKey, MlDsa65, Signature as MlDsaSignature,
    SigningKey as MlDsaSigningKey, VerifyingKey as MlDsaVerifyingKey,
    signature::{Signer as _, Verifier as _},
};
#[cfg(feature = "pq-ml-dsa")]
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionMaterial {
    pub session_key: HashBytes,
    pub transcript_hash: HashBytes,
}

pub trait Signer: Send + Sync {
    fn sign(&self, validator_id: ValidatorId, message: &[u8]) -> Result<TypedSignature>;
}

pub trait Verifier: Send + Sync {
    fn verify(
        &self,
        validator_id: ValidatorId,
        message: &[u8],
        signature: &TypedSignature,
    ) -> Result<bool>;
}

pub trait HandshakeProvider: Send + Sync {
    fn open_session(
        &self,
        local_validator_id: ValidatorId,
        peer_validator_id: ValidatorId,
        nonce: &[u8],
    ) -> Result<SessionMaterial>;
}

pub trait TranscriptHasher: Send + Sync {
    fn transcript_hash(&self, parts: &[&[u8]]) -> HashBytes;
}

pub trait CryptoBackend:
    Signer + Verifier + HandshakeProvider + TranscriptHasher + Send + Sync
{
}

impl<T> CryptoBackend for T where
    T: Signer + Verifier + HandshakeProvider + TranscriptHasher + Send + Sync
{
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SigningMeasurementReport {
    pub backend: String,
    pub signature_scheme: SignatureScheme,
    pub public_identity_size_bytes: usize,
    pub signature_size_bytes: usize,
    pub median_sign_latency_nanos: u128,
    pub median_verify_latency_nanos: u128,
    pub iterations: usize,
}

fn median_nanos(samples: &mut [u128]) -> u128 {
    if samples.is_empty() {
        return 0;
    }
    samples.sort_unstable();
    samples[samples.len() / 2]
}

fn backend_display_name(backend: &str, scheme: &SignatureScheme) -> String {
    match scheme {
        SignatureScheme::DevDeterministic => "Deterministic".into(),
        SignatureScheme::MlDsa => "ML-DSA".into(),
        _ => backend.to_string(),
    }
}

pub fn measure_signing_backend(
    backend: &str,
    genesis: &GenesisConfig,
    config: &NodeConfig,
    validator_id: ValidatorId,
    message: &[u8],
    iterations: usize,
) -> Result<SigningMeasurementReport> {
    let backend_impl = build_crypto_backend(genesis, config)?;
    let validator = validator_config(genesis, validator_id)?;
    let iterations = iterations.max(1);
    let mut sign_latencies = Vec::with_capacity(iterations);
    let mut verify_latencies = Vec::with_capacity(iterations);
    let mut signature = None;
    for _ in 0..iterations {
        let started = Instant::now();
        let signed = backend_impl.sign(validator_id, message)?;
        sign_latencies.push(started.elapsed().as_nanos());

        let started = Instant::now();
        let verified = backend_impl.verify(validator_id, message, &signed)?;
        verify_latencies.push(started.elapsed().as_nanos());
        if !verified {
            return Err(anyhow!(
                "signature verification failed while measuring backend {backend}"
            ));
        }
        signature = Some(signed);
    }
    let signature = signature.ok_or_else(|| anyhow!("missing measured signature"))?;
    Ok(SigningMeasurementReport {
        backend: backend.to_string(),
        signature_scheme: signature.scheme,
        public_identity_size_bytes: validator.public_identity.bytes.len(),
        signature_size_bytes: signature.bytes.len(),
        median_sign_latency_nanos: median_nanos(&mut sign_latencies),
        median_verify_latency_nanos: median_nanos(&mut verify_latencies),
        iterations,
    })
}

pub fn render_signing_measurement_report(reports: &[SigningMeasurementReport]) -> String {
    let mut markdown = String::from("# PQ Signing Measurements\n\n");
    for report in reports {
        let display_name = backend_display_name(&report.backend, &report.signature_scheme);
        markdown.push_str(&format!("## {display_name}\n\n"));
        markdown.push_str(&format!(
            "- Backend id: `{}`\n- Signature scheme: `{:?}`\n- Public identity size: `{}` bytes\n- Signature size: `{}` bytes\n- Median sign latency: `{}` ns\n- Median verify latency: `{}` ns\n- Iterations: `{}`\n\n",
            report.backend,
            report.signature_scheme,
            report.public_identity_size_bytes,
            report.signature_size_bytes,
            report.median_sign_latency_nanos,
            report.median_verify_latency_nanos,
            report.iterations,
        ));
    }
    markdown
}

pub fn deterministic_public_identity(validator_id: ValidatorId) -> PublicIdentity {
    PublicIdentity {
        scheme: PublicKeyScheme::DevDeterministic,
        bytes: format!("validator-{validator_id}").into_bytes(),
    }
}

fn validator_config<'a>(
    genesis: &'a GenesisConfig,
    validator_id: ValidatorId,
) -> Result<&'a ValidatorConfig> {
    genesis
        .validators
        .iter()
        .find(|validator| validator.validator_id == validator_id)
        .ok_or_else(|| anyhow!("unknown validator id {validator_id}"))
}

fn validate_deterministic_identity(validator: &ValidatorConfig) -> Result<()> {
    if validator.public_identity == PublicIdentity::default() {
        return Ok(());
    }
    let expected = deterministic_public_identity(validator.validator_id);
    if validator.public_identity != expected {
        return Err(anyhow!(
            "validator {} public identity does not match deterministic signer",
            validator.validator_id
        ));
    }
    Ok(())
}

#[cfg(feature = "pq-ml-dsa")]
#[derive(Clone, Debug, Serialize, Deserialize)]
struct MlDsa65KeyFile {
    signing_key: Vec<u8>,
    verifying_key: Vec<u8>,
}

#[derive(Clone)]
enum LocalSigningBackend {
    Deterministic,
    #[cfg(feature = "pq-ml-dsa")]
    MlDsa65(Arc<MlDsaSigningKey<MlDsa65>>),
}

#[derive(Clone)]
struct ConfiguredCryptoBackend {
    #[cfg(feature = "pq-ml-dsa")]
    local_validator_id: ValidatorId,
    local_signing_backend: LocalSigningBackend,
    deterministic: DeterministicCryptoBackend,
    identities: Arc<BTreeMap<ValidatorId, PublicIdentity>>,
    #[cfg(feature = "pq-ml-dsa")]
    ml_dsa_verifying_keys: Arc<BTreeMap<ValidatorId, MlDsaVerifyingKey<MlDsa65>>>,
}

#[cfg(feature = "pq-ml-dsa")]
fn decode_ml_dsa_signing_key(bytes: &[u8]) -> Result<MlDsaSigningKey<MlDsa65>> {
    let encoded = EncodedSigningKey::<MlDsa65>::try_from(bytes)
        .map_err(|_| anyhow!("invalid ML-DSA signing key encoding"))?;
    Ok(MlDsaSigningKey::<MlDsa65>::decode(&encoded))
}

#[cfg(feature = "pq-ml-dsa")]
fn decode_ml_dsa_verifying_key(bytes: &[u8]) -> Result<MlDsaVerifyingKey<MlDsa65>> {
    let encoded = EncodedVerifyingKey::<MlDsa65>::try_from(bytes)
        .map_err(|_| anyhow!("invalid ML-DSA verifying key encoding"))?;
    Ok(MlDsaVerifyingKey::<MlDsa65>::decode(&encoded))
}

#[cfg(feature = "pq-ml-dsa")]
fn load_ml_dsa_signing_material(
    validator: &ValidatorConfig,
    config: &NodeConfig,
) -> Result<Arc<MlDsaSigningKey<MlDsa65>>> {
    let key_path = config
        .signing_key_path
        .as_deref()
        .ok_or_else(|| anyhow!("ML-DSA backend requires signing_key_path"))?;
    let contents = std::fs::read(key_path)
        .map_err(|error| anyhow!("failed to read ML-DSA key file {key_path}: {error}"))?;
    let key_file: MlDsa65KeyFile = serde_json::from_slice(&contents)
        .map_err(|error| anyhow!("failed to parse ML-DSA key file {key_path}: {error}"))?;
    let signing_key = decode_ml_dsa_signing_key(&key_file.signing_key)?;
    let verifying_key = decode_ml_dsa_verifying_key(&key_file.verifying_key)?;
    let encoded_verifying_key = verifying_key.encode().as_slice().to_vec();
    if validator.public_identity.scheme != PublicKeyScheme::MlDsa
        || validator.public_identity.bytes != encoded_verifying_key
    {
        return Err(anyhow!(
            "validator {} public identity does not match ML-DSA signing key",
            validator.validator_id
        ));
    }
    Ok(Arc::new(signing_key))
}

fn build_identity_map(genesis: &GenesisConfig) -> Arc<BTreeMap<ValidatorId, PublicIdentity>> {
    Arc::new(
        genesis
            .validators
            .iter()
            .map(|validator| (validator.validator_id, validator.public_identity.clone()))
            .collect(),
    )
}

#[cfg(feature = "pq-ml-dsa")]
fn build_ml_dsa_verifying_key_map(
    genesis: &GenesisConfig,
) -> Result<Arc<BTreeMap<ValidatorId, MlDsaVerifyingKey<MlDsa65>>>> {
    let mut verifying_keys = BTreeMap::new();
    for validator in &genesis.validators {
        if validator.public_identity.scheme == PublicKeyScheme::MlDsa {
            verifying_keys.insert(
                validator.validator_id,
                decode_ml_dsa_verifying_key(&validator.public_identity.bytes)?,
            );
        }
    }
    Ok(Arc::new(verifying_keys))
}

pub fn build_crypto_backend(
    genesis: &GenesisConfig,
    config: &NodeConfig,
) -> Result<Arc<dyn CryptoBackend>> {
    let validator = validator_config(genesis, config.validator_id)?;
    let deterministic = DeterministicCryptoBackend::from_genesis(genesis);
    let identities = build_identity_map(genesis);
    match config.signing_backend {
        SigningBackendKind::DevDeterministic => {
            validate_deterministic_identity(validator)?;
            Ok(Arc::new(ConfiguredCryptoBackend {
                #[cfg(feature = "pq-ml-dsa")]
                local_validator_id: config.validator_id,
                local_signing_backend: LocalSigningBackend::Deterministic,
                deterministic,
                identities,
                #[cfg(feature = "pq-ml-dsa")]
                ml_dsa_verifying_keys: build_ml_dsa_verifying_key_map(genesis)?,
            }))
        }
        SigningBackendKind::MlDsa65Experimental => {
            #[cfg(feature = "pq-ml-dsa")]
            {
                let signing_key = load_ml_dsa_signing_material(validator, config)?;
                return Ok(Arc::new(ConfiguredCryptoBackend {
                    #[cfg(feature = "pq-ml-dsa")]
                    local_validator_id: config.validator_id,
                    local_signing_backend: LocalSigningBackend::MlDsa65(signing_key),
                    deterministic,
                    identities,
                    ml_dsa_verifying_keys: build_ml_dsa_verifying_key_map(genesis)?,
                }));
            }
            #[cfg(not(feature = "pq-ml-dsa"))]
            {
                Err(anyhow!(
                    "ML-DSA backend requested but entangrid-crypto was built without pq-ml-dsa"
                ))
            }
        }
    }
}

#[derive(Clone, Debug)]
pub struct DeterministicCryptoBackend {
    secrets: Arc<BTreeMap<ValidatorId, String>>,
}

impl DeterministicCryptoBackend {
    pub fn from_genesis(genesis: &GenesisConfig) -> Self {
        Self::from_validators(&genesis.validators)
    }

    pub fn from_validators(validators: &[ValidatorConfig]) -> Self {
        let secrets = validators
            .iter()
            .map(|validator| (validator.validator_id, validator.dev_secret.clone()))
            .collect();
        Self {
            secrets: Arc::new(secrets),
        }
    }

    fn secret(&self, validator_id: ValidatorId) -> Result<&str> {
        self.secrets
            .get(&validator_id)
            .map(|secret| secret.as_str())
            .ok_or_else(|| anyhow!("unknown validator id {validator_id}"))
    }
}

impl Signer for DeterministicCryptoBackend {
    fn sign(&self, validator_id: ValidatorId, message: &[u8]) -> Result<TypedSignature> {
        let secret = self.secret(validator_id)?;
        let hash = hash_many(&[secret.as_bytes(), message]);
        Ok(TypedSignature {
            scheme: SignatureScheme::DevDeterministic,
            bytes: hash.to_vec(),
        })
    }
}

impl Verifier for DeterministicCryptoBackend {
    fn verify(
        &self,
        validator_id: ValidatorId,
        message: &[u8],
        signature: &TypedSignature,
    ) -> Result<bool> {
        if signature.scheme != SignatureScheme::DevDeterministic {
            return Ok(false);
        }
        let expected = self.sign(validator_id, message)?;
        Ok(expected == *signature)
    }
}

impl HandshakeProvider for DeterministicCryptoBackend {
    fn open_session(
        &self,
        local_validator_id: ValidatorId,
        peer_validator_id: ValidatorId,
        nonce: &[u8],
    ) -> Result<SessionMaterial> {
        let local_secret = self.secret(local_validator_id)?;
        let peer_secret = self.secret(peer_validator_id)?;
        let (first_id, first_secret, second_id, second_secret) =
            if local_validator_id <= peer_validator_id {
                (
                    local_validator_id,
                    local_secret.as_bytes(),
                    peer_validator_id,
                    peer_secret.as_bytes(),
                )
            } else {
                (
                    peer_validator_id,
                    peer_secret.as_bytes(),
                    local_validator_id,
                    local_secret.as_bytes(),
                )
            };
        let first_id_bytes = first_id.to_le_bytes();
        let second_id_bytes = second_id.to_le_bytes();
        let transcript_hash = self.transcript_hash(&[
            b"entangrid-session",
            &first_id_bytes,
            first_secret,
            &second_id_bytes,
            second_secret,
            nonce,
        ]);
        let session_key = hash_many(&[b"session-key", &transcript_hash]);
        Ok(SessionMaterial {
            session_key,
            transcript_hash,
        })
    }
}

impl TranscriptHasher for DeterministicCryptoBackend {
    fn transcript_hash(&self, parts: &[&[u8]]) -> HashBytes {
        hash_many(parts)
    }
}

impl Signer for ConfiguredCryptoBackend {
    fn sign(&self, validator_id: ValidatorId, message: &[u8]) -> Result<TypedSignature> {
        match &self.local_signing_backend {
            LocalSigningBackend::Deterministic => self.deterministic.sign(validator_id, message),
            #[cfg(feature = "pq-ml-dsa")]
            LocalSigningBackend::MlDsa65(signing_key) => {
                if validator_id != self.local_validator_id {
                    return Err(anyhow!(
                        "ML-DSA signing material is only available for validator {}",
                        self.local_validator_id
                    ));
                }
                let signature: MlDsaSignature<MlDsa65> = signing_key.sign(message);
                Ok(TypedSignature {
                    scheme: SignatureScheme::MlDsa,
                    bytes: signature.encode().as_slice().to_vec(),
                })
            }
        }
    }
}

impl Verifier for ConfiguredCryptoBackend {
    fn verify(
        &self,
        validator_id: ValidatorId,
        message: &[u8],
        signature: &TypedSignature,
    ) -> Result<bool> {
        let identity = self
            .identities
            .get(&validator_id)
            .ok_or_else(|| anyhow!("unknown validator id {validator_id}"))?;
        match identity.scheme {
            PublicKeyScheme::DevDeterministic => {
                self.deterministic.verify(validator_id, message, signature)
            }
            PublicKeyScheme::MlDsa => {
                if signature.scheme != SignatureScheme::MlDsa {
                    return Ok(false);
                }
                #[cfg(feature = "pq-ml-dsa")]
                {
                    let verifying_key =
                        self.ml_dsa_verifying_keys
                            .get(&validator_id)
                            .ok_or_else(|| {
                                anyhow!("missing ML-DSA verifying key for validator {validator_id}")
                            })?;
                    let signature = MlDsaSignature::<MlDsa65>::try_from(signature.bytes.as_slice())
                        .map_err(|_| anyhow!("invalid ML-DSA signature encoding"))?;
                    return Ok(verifying_key.verify(message, &signature).is_ok());
                }
                #[cfg(not(feature = "pq-ml-dsa"))]
                {
                    Ok(false)
                }
            }
            _ => Ok(false),
        }
    }
}

impl HandshakeProvider for ConfiguredCryptoBackend {
    fn open_session(
        &self,
        local_validator_id: ValidatorId,
        peer_validator_id: ValidatorId,
        nonce: &[u8],
    ) -> Result<SessionMaterial> {
        self.deterministic
            .open_session(local_validator_id, peer_validator_id, nonce)
    }
}

impl TranscriptHasher for ConfiguredCryptoBackend {
    fn transcript_hash(&self, parts: &[&[u8]]) -> HashBytes {
        self.deterministic.transcript_hash(parts)
    }
}

#[cfg(test)]
mod tests {
    use entangrid_types::{
        FaultProfile, FeatureFlags, GenesisConfig, NodeConfig, PublicIdentity, PublicKeyScheme,
        SignatureScheme, SigningBackendKind, TypedSignature, ValidatorConfig, empty_hash,
    };

    use super::*;

    #[test]
    fn signatures_round_trip() {
        let genesis = GenesisConfig {
            chain_id: "entangrid-test".into(),
            epoch_seed: empty_hash(),
            genesis_time_unix_millis: 0,
            slot_duration_millis: 1000,
            slots_per_epoch: 10,
            max_txs_per_block: 16,
            witness_count: 2,
            validators: vec![ValidatorConfig {
                validator_id: 1,
                stake: 100,
                address: "127.0.0.1:3001".into(),
                dev_secret: "secret-1".into(),
                public_identity: PublicIdentity::default(),
            }],
            initial_balances: Default::default(),
        };
        let backend = DeterministicCryptoBackend::from_genesis(&genesis);
        let message = b"hello";
        let signature = backend.sign(1, message).unwrap();
        assert!(backend.verify(1, message, &signature).unwrap());
    }

    #[test]
    fn deterministic_backend_signs_with_dev_deterministic_scheme() {
        let genesis = GenesisConfig {
            chain_id: "entangrid-test".into(),
            epoch_seed: empty_hash(),
            genesis_time_unix_millis: 0,
            slot_duration_millis: 1000,
            slots_per_epoch: 10,
            max_txs_per_block: 16,
            witness_count: 2,
            validators: vec![ValidatorConfig {
                validator_id: 1,
                stake: 100,
                address: "127.0.0.1:3001".into(),
                dev_secret: "secret-1".into(),
                public_identity: PublicIdentity::default(),
            }],
            initial_balances: Default::default(),
        };
        let backend = DeterministicCryptoBackend::from_genesis(&genesis);
        let message = b"typed-signature";
        let signature: TypedSignature = backend.sign(1, message).unwrap();
        assert_eq!(signature.scheme, SignatureScheme::DevDeterministic);
        assert!(backend.verify(1, message, &signature).unwrap());
    }

    #[test]
    fn backend_factory_selects_deterministic_backend_by_default() {
        let genesis = GenesisConfig {
            chain_id: "entangrid-test".into(),
            epoch_seed: empty_hash(),
            genesis_time_unix_millis: 0,
            slot_duration_millis: 1000,
            slots_per_epoch: 10,
            max_txs_per_block: 16,
            witness_count: 2,
            validators: vec![ValidatorConfig {
                validator_id: 1,
                stake: 100,
                address: "127.0.0.1:3001".into(),
                dev_secret: "secret-1".into(),
                public_identity: PublicIdentity {
                    scheme: PublicKeyScheme::DevDeterministic,
                    bytes: b"validator-1".to_vec(),
                },
            }],
            initial_balances: Default::default(),
        };
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
            signing_backend: SigningBackendKind::DevDeterministic,
            signing_key_path: None,
        };
        let backend = build_crypto_backend(&genesis, &config).unwrap();
        let signature = backend.sign(1, b"factory").unwrap();
        assert_eq!(signature.scheme, SignatureScheme::DevDeterministic);
    }

    #[test]
    fn backend_factory_rejects_mismatched_deterministic_public_identity() {
        let genesis = GenesisConfig {
            chain_id: "entangrid-test".into(),
            epoch_seed: empty_hash(),
            genesis_time_unix_millis: 0,
            slot_duration_millis: 1000,
            slots_per_epoch: 10,
            max_txs_per_block: 16,
            witness_count: 2,
            validators: vec![ValidatorConfig {
                validator_id: 1,
                stake: 100,
                address: "127.0.0.1:3001".into(),
                dev_secret: "secret-1".into(),
                public_identity: PublicIdentity {
                    scheme: PublicKeyScheme::DevDeterministic,
                    bytes: b"wrong-validator".to_vec(),
                },
            }],
            initial_balances: Default::default(),
        };
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
            signing_backend: SigningBackendKind::DevDeterministic,
            signing_key_path: None,
        };
        let error = match build_crypto_backend(&genesis, &config) {
            Ok(_) => panic!("expected deterministic identity mismatch"),
            Err(error) => error,
        };
        assert!(
            error
                .to_string()
                .contains("public identity does not match deterministic signer"),
            "unexpected error: {error:?}"
        );
    }

    #[test]
    fn measurement_report_includes_deterministic_sizes_and_latency_sections() {
        let genesis = GenesisConfig {
            chain_id: "entangrid-test".into(),
            epoch_seed: empty_hash(),
            genesis_time_unix_millis: 0,
            slot_duration_millis: 1000,
            slots_per_epoch: 10,
            max_txs_per_block: 16,
            witness_count: 2,
            validators: vec![ValidatorConfig {
                validator_id: 1,
                stake: 100,
                address: "127.0.0.1:3001".into(),
                dev_secret: "secret-1".into(),
                public_identity: deterministic_public_identity(1),
            }],
            initial_balances: Default::default(),
        };
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
            signing_backend: SigningBackendKind::DevDeterministic,
            signing_key_path: None,
        };
        let report = measure_signing_backend(
            "deterministic",
            &genesis,
            &config,
            1,
            b"stage1c-measurement",
            8,
        )
        .unwrap();
        let markdown = render_signing_measurement_report(&[report.clone()]);
        assert_eq!(report.backend, "deterministic");
        assert_eq!(report.signature_scheme, SignatureScheme::DevDeterministic);
        assert!(report.public_identity_size_bytes > 0);
        assert!(report.signature_size_bytes > 0);
        assert!(markdown.contains("Deterministic"));
        assert!(markdown.contains("Public identity size"));
        assert!(markdown.contains("Median sign latency"));
        assert!(markdown.contains("Median verify latency"));
    }

    #[cfg(not(feature = "pq-ml-dsa"))]
    #[test]
    fn backend_factory_rejects_ml_dsa_selection_without_feature() {
        let genesis = GenesisConfig {
            chain_id: "entangrid-test".into(),
            epoch_seed: empty_hash(),
            genesis_time_unix_millis: 0,
            slot_duration_millis: 1000,
            slots_per_epoch: 10,
            max_txs_per_block: 16,
            witness_count: 2,
            validators: vec![ValidatorConfig {
                validator_id: 1,
                stake: 100,
                address: "127.0.0.1:3001".into(),
                dev_secret: "secret-1".into(),
                public_identity: PublicIdentity {
                    scheme: PublicKeyScheme::MlDsa,
                    bytes: vec![7, 7, 7],
                },
            }],
            initial_balances: Default::default(),
        };
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
        let error = match build_crypto_backend(&genesis, &config) {
            Ok(_) => panic!("expected ML-DSA feature gate failure"),
            Err(error) => error,
        };
        assert!(
            error.to_string().contains("built without pq-ml-dsa"),
            "unexpected error: {error:?}"
        );
    }

    #[cfg(feature = "pq-ml-dsa")]
    #[test]
    fn ml_dsa_backend_signs_with_ml_dsa_scheme() {
        use ml_dsa::{KeyGen, MlDsa65};
        use rand_core::OsRng;

        let mut rng = OsRng;
        let keypair = MlDsa65::key_gen(&mut rng);
        let signing_key = keypair.signing_key().clone();
        let verifying_key = keypair.verifying_key().clone();

        let key_path =
            std::env::temp_dir().join(format!("entangrid-ml-dsa-test-{}.key", std::process::id()));
        let key_file = MlDsa65KeyFile {
            signing_key: signing_key.encode().as_slice().to_vec(),
            verifying_key: verifying_key.encode().as_slice().to_vec(),
        };
        std::fs::write(&key_path, serde_json::to_vec(&key_file).unwrap()).unwrap();

        let genesis = GenesisConfig {
            chain_id: "entangrid-test".into(),
            epoch_seed: empty_hash(),
            genesis_time_unix_millis: 0,
            slot_duration_millis: 1000,
            slots_per_epoch: 10,
            max_txs_per_block: 16,
            witness_count: 2,
            validators: vec![ValidatorConfig {
                validator_id: 1,
                stake: 100,
                address: "127.0.0.1:3001".into(),
                dev_secret: "secret-1".into(),
                public_identity: PublicIdentity {
                    scheme: PublicKeyScheme::MlDsa,
                    bytes: verifying_key.encode().as_slice().to_vec(),
                },
            }],
            initial_balances: Default::default(),
        };
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
            signing_key_path: Some(key_path.display().to_string()),
        };
        let backend = build_crypto_backend(&genesis, &config).unwrap();
        let signature = backend.sign(1, b"ml-dsa-backend").unwrap();
        assert_eq!(signature.scheme, SignatureScheme::MlDsa);
        assert!(backend.verify(1, b"ml-dsa-backend", &signature).unwrap());
    }

    #[cfg(feature = "pq-ml-dsa")]
    #[test]
    fn measurement_report_includes_ml_dsa_scheme_and_sizes() {
        use ml_dsa::{KeyGen, MlDsa65};
        use rand_core::OsRng;

        let mut rng = OsRng;
        let keypair = MlDsa65::key_gen(&mut rng);
        let signing_key = keypair.signing_key().clone();
        let verifying_key = keypair.verifying_key().clone();

        let key_path = std::env::temp_dir().join(format!(
            "entangrid-ml-dsa-measurement-{}.key",
            std::process::id()
        ));
        let key_file = MlDsa65KeyFile {
            signing_key: signing_key.encode().as_slice().to_vec(),
            verifying_key: verifying_key.encode().as_slice().to_vec(),
        };
        std::fs::write(&key_path, serde_json::to_vec(&key_file).unwrap()).unwrap();

        let genesis = GenesisConfig {
            chain_id: "entangrid-test".into(),
            epoch_seed: empty_hash(),
            genesis_time_unix_millis: 0,
            slot_duration_millis: 1000,
            slots_per_epoch: 10,
            max_txs_per_block: 16,
            witness_count: 2,
            validators: vec![ValidatorConfig {
                validator_id: 1,
                stake: 100,
                address: "127.0.0.1:3001".into(),
                dev_secret: "secret-1".into(),
                public_identity: PublicIdentity {
                    scheme: PublicKeyScheme::MlDsa,
                    bytes: verifying_key.encode().as_slice().to_vec(),
                },
            }],
            initial_balances: Default::default(),
        };
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
            signing_key_path: Some(key_path.display().to_string()),
        };
        let report =
            measure_signing_backend("ml-dsa", &genesis, &config, 1, b"stage1c-measurement", 4)
                .unwrap();
        let markdown = render_signing_measurement_report(&[report.clone()]);
        assert_eq!(report.signature_scheme, SignatureScheme::MlDsa);
        assert!(report.public_identity_size_bytes > 0);
        assert!(report.signature_size_bytes > 0);
        assert!(markdown.contains("ML-DSA"));
        assert!(markdown.contains("Signature size"));
    }
}
