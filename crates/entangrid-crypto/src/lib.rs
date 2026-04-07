use std::{collections::BTreeMap, sync::Arc, time::Instant};

use anyhow::{Result, anyhow};
use bincode::config::standard;
#[cfg(feature = "pq-ml-kem")]
use chacha20poly1305::{
    ChaCha20Poly1305,
    aead::{AeadInPlace, KeyInit},
};
#[cfg(feature = "pq-ml-dsa")]
use entangrid_types::SignatureComponent;
use entangrid_types::{
    GenesisConfig, HashBytes, NodeConfig, PublicIdentity, PublicKeyScheme, SessionBackendKind,
    SessionClientHello, SessionKeyScheme, SessionPublicIdentity, SessionServerHello,
    SignatureScheme, SigningBackendKind, TypedSignature, ValidatorConfig, ValidatorId, hash_many,
};
#[cfg(feature = "pq-ml-dsa")]
use ml_dsa::{
    EncodedSigningKey, EncodedVerifyingKey, MlDsa65, Signature as MlDsaSignature,
    SigningKey as MlDsaSigningKey, VerifyingKey as MlDsaVerifyingKey,
    signature::{Signer as _, Verifier as _},
};
#[cfg(feature = "pq-ml-kem")]
use ml_kem::{
    Encoded, EncodedSizeUser, KemCore, MlKem768,
    kem::{Decapsulate, Encapsulate},
};
#[cfg(feature = "pq-ml-kem")]
use rand_core::OsRng;
#[cfg(any(feature = "pq-ml-dsa", feature = "pq-ml-kem"))]
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionMaterial {
    pub session_key: HashBytes,
    pub transcript_hash: HashBytes,
    pub encrypt_frames: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FrameDirection {
    Outbound,
    Inbound,
}

#[cfg(feature = "pq-ml-kem")]
const FRAME_SESSION_ID_BYTES: usize = 16;
#[cfg(feature = "pq-ml-kem")]
const FRAME_TAG_SIZE_BYTES: usize = 16;

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
    fn build_client_hello(
        &self,
        local_validator_id: ValidatorId,
        peer_validator_id: ValidatorId,
        nonce: HashBytes,
    ) -> Result<SessionClientHello>;

    fn accept_client_hello(
        &self,
        local_validator_id: ValidatorId,
        client_hello: &SessionClientHello,
    ) -> Result<(SessionServerHello, SessionMaterial)>;

    fn finalize_client_session(
        &self,
        local_validator_id: ValidatorId,
        client_hello: &SessionClientHello,
        server_hello: &SessionServerHello,
    ) -> Result<SessionMaterial>;

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
        SignatureScheme::Hybrid => "Hybrid".into(),
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
        signature_scheme: signature.scheme(),
        public_identity_size_bytes: bincode::serde::encode_to_vec(
            &validator.public_identity,
            standard(),
        )
        .map_err(|error| anyhow!("failed to encode public identity for measurement: {error}"))?
        .len(),
        signature_size_bytes: bincode::serde::encode_to_vec(&signature, standard())
            .map_err(|error| anyhow!("failed to encode signature for measurement: {error}"))?
            .len(),
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

impl FrameDirection {
    #[cfg(feature = "pq-ml-kem")]
    fn as_byte(self) -> u8 {
        match self {
            Self::Outbound => 0,
            Self::Inbound => 1,
        }
    }
}

#[cfg(feature = "pq-ml-kem")]
fn frame_nonce(direction: FrameDirection, counter: u64) -> [u8; 12] {
    let mut nonce = [0u8; 12];
    nonce[0] = direction.as_byte();
    nonce[4..].copy_from_slice(&counter.to_be_bytes());
    nonce
}

#[cfg(feature = "pq-ml-kem")]
fn frame_session_id(session: &SessionMaterial) -> [u8; FRAME_SESSION_ID_BYTES] {
    let mut session_id = [0u8; FRAME_SESSION_ID_BYTES];
    session_id.copy_from_slice(&session.transcript_hash[..FRAME_SESSION_ID_BYTES]);
    session_id
}

#[cfg(feature = "pq-ml-kem")]
fn frame_associated_data(
    session: &SessionMaterial,
    direction: FrameDirection,
    counter: u64,
) -> [u8; 1 + 8 + FRAME_SESSION_ID_BYTES] {
    let mut associated_data = [0u8; 1 + 8 + FRAME_SESSION_ID_BYTES];
    associated_data[0] = direction.as_byte();
    associated_data[1..9].copy_from_slice(&counter.to_be_bytes());
    associated_data[9..].copy_from_slice(&frame_session_id(session));
    associated_data
}

pub fn encrypt_frame_payload(
    session: &SessionMaterial,
    direction: FrameDirection,
    counter: u64,
    plaintext: &[u8],
) -> Result<Vec<u8>> {
    #[cfg(not(feature = "pq-ml-kem"))]
    {
        let _ = (session, direction, counter, plaintext);
        Err(anyhow!("encrypted framing requires a build with pq-ml-kem"))
    }
    #[cfg(feature = "pq-ml-kem")]
    {
        let cipher = ChaCha20Poly1305::new_from_slice(&session.session_key)
            .map_err(|error| anyhow!("invalid frame key: {error}"))?;
        let nonce = frame_nonce(direction, counter);
        let associated_data = frame_associated_data(session, direction, counter);
        let mut ciphertext = plaintext.to_vec();
        let tag = cipher
            .encrypt_in_place_detached((&nonce).into(), &associated_data, &mut ciphertext)
            .map_err(|error| anyhow!("frame encryption failed: {error}"))?;
        ciphertext.extend_from_slice(tag.as_slice());
        Ok(ciphertext)
    }
}

pub fn decrypt_frame_payload(
    session: &SessionMaterial,
    direction: FrameDirection,
    counter: u64,
    ciphertext: &[u8],
) -> Result<Vec<u8>> {
    #[cfg(not(feature = "pq-ml-kem"))]
    {
        let _ = (session, direction, counter, ciphertext);
        Err(anyhow!("encrypted framing requires a build with pq-ml-kem"))
    }
    #[cfg(feature = "pq-ml-kem")]
    {
        if ciphertext.len() < FRAME_TAG_SIZE_BYTES {
            return Err(anyhow!("frame decryption failed: truncated ciphertext"));
        }
        let cipher = ChaCha20Poly1305::new_from_slice(&session.session_key)
            .map_err(|error| anyhow!("invalid frame key: {error}"))?;
        let nonce = frame_nonce(direction, counter);
        let associated_data = frame_associated_data(session, direction, counter);
        let split_at = ciphertext.len() - FRAME_TAG_SIZE_BYTES;
        let (encrypted_bytes, tag_bytes) = ciphertext.split_at(split_at);
        let mut plaintext = encrypted_bytes.to_vec();
        let tag = chacha20poly1305::Tag::from_slice(tag_bytes);
        cipher
            .decrypt_in_place_detached((&nonce).into(), &associated_data, &mut plaintext, tag)
            .map_err(|_| anyhow!("frame decryption failed"))?;
        Ok(plaintext)
    }
}

pub fn deterministic_public_identity(validator_id: ValidatorId) -> PublicIdentity {
    PublicIdentity::single(
        PublicKeyScheme::DevDeterministic,
        format!("validator-{validator_id}").into_bytes(),
    )
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
    let expected = deterministic_public_identity(validator.validator_id)
        .as_single_bytes()
        .ok_or_else(|| anyhow!("deterministic identity missing bytes"))?
        .to_vec();
    match validator.public_identity.scheme() {
        PublicKeyScheme::DevDeterministic => {
            if validator.public_identity.as_single_bytes() != Some(expected.as_slice()) {
                return Err(anyhow!(
                    "validator {} public identity does not match deterministic signer",
                    validator.validator_id
                ));
            }
        }
        PublicKeyScheme::Hybrid => {
            if validator
                .public_identity
                .component_bytes(PublicKeyScheme::DevDeterministic)
                != Some(expected.as_slice())
            {
                return Err(anyhow!(
                    "validator {} hybrid public identity does not include a matching deterministic component",
                    validator.validator_id
                ));
            }
        }
        _ => {
            return Err(anyhow!(
                "validator {} public identity does not match deterministic signer",
                validator.validator_id
            ));
        }
    }
    Ok(())
}

#[cfg(feature = "pq-ml-dsa")]
#[derive(Clone, Debug, Serialize, Deserialize)]
struct MlDsa65KeyFile {
    signing_key: Vec<u8>,
    verifying_key: Vec<u8>,
}

#[cfg(feature = "pq-ml-kem")]
#[derive(Clone, Debug, Serialize, Deserialize)]
struct MlKemSessionKeyFile {
    decapsulation_key: Vec<u8>,
    encapsulation_key: Vec<u8>,
}

#[cfg(feature = "pq-ml-kem")]
type MlKem768DecapsulationKey = <MlKem768 as KemCore>::DecapsulationKey;
#[cfg(feature = "pq-ml-kem")]
type MlKem768EncapsulationKey = <MlKem768 as KemCore>::EncapsulationKey;
#[cfg(feature = "pq-ml-kem")]
type MlKem768Ciphertext = ml_kem::Ciphertext<MlKem768>;

#[derive(Clone)]
enum LocalSigningBackend {
    Deterministic,
    #[cfg(feature = "pq-ml-dsa")]
    MlDsa65(Arc<MlDsaSigningKey<MlDsa65>>),
    #[cfg(feature = "pq-ml-dsa")]
    HybridDeterministicMlDsa65(Arc<MlDsaSigningKey<MlDsa65>>),
}

#[derive(Clone)]
enum LocalSessionBackend {
    Deterministic,
    #[cfg(feature = "pq-ml-kem")]
    HybridDeterministicMlKem768(LoadedMlKemSessionMaterial),
}

#[cfg(feature = "pq-ml-kem")]
#[derive(Clone)]
struct LoadedMlKemSessionMaterial {
    key_file: MlKemSessionKeyFile,
    session_public_identity: SessionPublicIdentity,
}

#[derive(Clone)]
struct ConfiguredCryptoBackend {
    #[cfg(feature = "pq-ml-dsa")]
    local_validator_id: ValidatorId,
    local_signing_backend: LocalSigningBackend,
    local_session_backend: LocalSessionBackend,
    deterministic: DeterministicCryptoBackend,
    identities: Arc<BTreeMap<ValidatorId, PublicIdentity>>,
    #[cfg(feature = "pq-ml-kem")]
    session_identities: Arc<BTreeMap<ValidatorId, Option<SessionPublicIdentity>>>,
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
    match validator.public_identity.scheme() {
        PublicKeyScheme::MlDsa => {
            if validator.public_identity.as_single_bytes() != Some(encoded_verifying_key.as_slice())
            {
                return Err(anyhow!(
                    "validator {} public identity does not match ML-DSA signing key",
                    validator.validator_id
                ));
            }
        }
        PublicKeyScheme::Hybrid => {
            if validator
                .public_identity
                .component_bytes(PublicKeyScheme::MlDsa)
                != Some(encoded_verifying_key.as_slice())
            {
                return Err(anyhow!(
                    "validator {} hybrid public identity does not include a matching ML-DSA component",
                    validator.validator_id
                ));
            }
        }
        _ => {
            return Err(anyhow!(
                "validator {} public identity does not match ML-DSA signing key",
                validator.validator_id
            ));
        }
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

#[cfg(feature = "pq-ml-kem")]
fn build_session_identity_map(
    genesis: &GenesisConfig,
) -> Arc<BTreeMap<ValidatorId, Option<SessionPublicIdentity>>> {
    Arc::new(
        genesis
            .validators
            .iter()
            .map(|validator| {
                (
                    validator.validator_id,
                    validator.session_public_identity.clone(),
                )
            })
            .collect(),
    )
}

#[cfg(feature = "pq-ml-dsa")]
fn build_ml_dsa_verifying_key_map(
    genesis: &GenesisConfig,
) -> Result<Arc<BTreeMap<ValidatorId, MlDsaVerifyingKey<MlDsa65>>>> {
    let mut verifying_keys = BTreeMap::new();
    for validator in &genesis.validators {
        if let Some(encoded_key) = match validator.public_identity.scheme() {
            PublicKeyScheme::MlDsa => validator.public_identity.as_single_bytes(),
            PublicKeyScheme::Hybrid => validator
                .public_identity
                .component_bytes(PublicKeyScheme::MlDsa),
            _ => None,
        } {
            verifying_keys.insert(
                validator.validator_id,
                decode_ml_dsa_verifying_key(encoded_key)?,
            );
        }
    }
    Ok(Arc::new(verifying_keys))
}

#[cfg(feature = "pq-ml-kem")]
fn load_ml_kem_session_material(
    validator: &ValidatorConfig,
    config: &NodeConfig,
) -> Result<LoadedMlKemSessionMaterial> {
    let key_path = config
        .session_key_path
        .as_ref()
        .ok_or_else(|| anyhow!("hybrid session backend requires session_key_path"))?;
    let session_identity = validator.session_public_identity.as_ref().ok_or_else(|| {
        anyhow!(
            "validator {} missing session public identity",
            validator.validator_id
        )
    })?;
    let key_bytes = std::fs::read(key_path).map_err(|error| {
        anyhow!("failed to read ML-KEM session key file at {key_path}: {error}")
    })?;
    let key_file: MlKemSessionKeyFile = serde_json::from_slice(&key_bytes).map_err(|error| {
        anyhow!("failed to parse ML-KEM session key file at {key_path}: {error}")
    })?;
    if key_file.decapsulation_key.is_empty() || key_file.encapsulation_key.is_empty() {
        return Err(anyhow!(
            "ML-KEM session key file at {key_path} is missing key material"
        ));
    }
    let expected_public_key = match session_identity.scheme() {
        SessionKeyScheme::MlKem => session_identity.as_single_bytes().ok_or_else(|| {
            anyhow!(
                "validator {} session public identity missing ML-KEM bytes",
                validator.validator_id
            )
        })?,
        SessionKeyScheme::Hybrid => session_identity
            .component_bytes(SessionKeyScheme::MlKem)
            .ok_or_else(|| {
                anyhow!(
                    "validator {} session public identity missing ML-KEM component",
                    validator.validator_id
                )
            })?,
        SessionKeyScheme::DevDeterministic => {
            return Err(anyhow!(
                "validator {} session public identity does not carry ML-KEM material",
                validator.validator_id
            ));
        }
    };
    if expected_public_key != key_file.encapsulation_key.as_slice() {
        return Err(anyhow!(
            "validator {} session public identity does not match configured ML-KEM session key",
            validator.validator_id
        ));
    }
    Ok(LoadedMlKemSessionMaterial {
        key_file,
        session_public_identity: session_identity.clone(),
    })
}

fn build_local_session_backend(
    validator: &ValidatorConfig,
    config: &NodeConfig,
) -> Result<LocalSessionBackend> {
    #[cfg(not(feature = "pq-ml-kem"))]
    let _ = validator;
    match config.session_backend {
        SessionBackendKind::DevDeterministic => Ok(LocalSessionBackend::Deterministic),
        SessionBackendKind::HybridDeterministicMlKemExperimental => {
            #[cfg(feature = "pq-ml-kem")]
            {
                load_ml_kem_session_material(validator, config)
                    .map(LocalSessionBackend::HybridDeterministicMlKem768)
            }
            #[cfg(not(feature = "pq-ml-kem"))]
            {
                Err(anyhow!(
                    "hybrid ML-KEM session backend requested but entangrid-crypto was built without pq-ml-kem"
                ))
            }
        }
    }
}

pub fn build_crypto_backend(
    genesis: &GenesisConfig,
    config: &NodeConfig,
) -> Result<Arc<dyn CryptoBackend>> {
    let validator = validator_config(genesis, config.validator_id)?;
    let deterministic = DeterministicCryptoBackend::from_genesis(genesis);
    let identities = build_identity_map(genesis);
    #[cfg(feature = "pq-ml-kem")]
    let session_identities = build_session_identity_map(genesis);
    let local_session_backend = build_local_session_backend(validator, config)?;
    match config.signing_backend {
        SigningBackendKind::DevDeterministic => {
            validate_deterministic_identity(validator)?;
            Ok(Arc::new(ConfiguredCryptoBackend {
                #[cfg(feature = "pq-ml-dsa")]
                local_validator_id: config.validator_id,
                local_signing_backend: LocalSigningBackend::Deterministic,
                local_session_backend,
                deterministic,
                identities,
                #[cfg(feature = "pq-ml-kem")]
                session_identities,
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
                    local_session_backend,
                    deterministic,
                    identities,
                    #[cfg(feature = "pq-ml-kem")]
                    session_identities,
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
        SigningBackendKind::HybridDeterministicMlDsaExperimental => {
            #[cfg(feature = "pq-ml-dsa")]
            {
                validate_deterministic_identity(validator)?;
                let signing_key = load_ml_dsa_signing_material(validator, config)?;
                return Ok(Arc::new(ConfiguredCryptoBackend {
                    local_validator_id: config.validator_id,
                    local_signing_backend: LocalSigningBackend::HybridDeterministicMlDsa65(
                        signing_key,
                    ),
                    local_session_backend,
                    deterministic,
                    identities,
                    #[cfg(feature = "pq-ml-kem")]
                    session_identities,
                    ml_dsa_verifying_keys: build_ml_dsa_verifying_key_map(genesis)?,
                }));
            }
            #[cfg(not(feature = "pq-ml-dsa"))]
            {
                Err(anyhow!(
                    "hybrid ML-DSA backend requested but entangrid-crypto was built without pq-ml-dsa"
                ))
            }
        }
    }
}

fn deterministic_session_public_identity(validator_id: ValidatorId) -> SessionPublicIdentity {
    SessionPublicIdentity::single(
        SessionKeyScheme::DevDeterministic,
        format!("session-validator-{validator_id}").into_bytes(),
    )
}

fn client_hello_signing_message(client_hello: &SessionClientHello) -> Result<Vec<u8>> {
    bincode::serde::encode_to_vec(
        &(
            "entangrid-session-client-hello",
            client_hello.initiator_validator_id,
            client_hello.responder_validator_id,
            client_hello.nonce,
            &client_hello.session_public_identity,
            &client_hello.kem_public_material,
        ),
        standard(),
    )
    .map_err(|error| anyhow!("failed to encode client hello signing message: {error}"))
}

fn server_hello_signing_message(
    client_hello: &SessionClientHello,
    server_hello: &SessionServerHello,
) -> Result<Vec<u8>> {
    bincode::serde::encode_to_vec(
        &(
            "entangrid-session-server-hello",
            &client_hello_signing_message(client_hello)?,
            server_hello.responder_validator_id,
            server_hello.initiator_validator_id,
            server_hello.nonce,
            &server_hello.session_public_identity,
            &server_hello.kem_ciphertext,
        ),
        standard(),
    )
    .map_err(|error| anyhow!("failed to encode server hello signing message: {error}"))
}

fn session_transcript_hash<H: TranscriptHasher>(
    hasher: &H,
    client_hello: &SessionClientHello,
    server_hello: &SessionServerHello,
) -> Result<HashBytes> {
    let client_bytes = bincode::serde::encode_to_vec(client_hello, standard())
        .map_err(|error| anyhow!("failed to encode client hello transcript: {error}"))?;
    let server_bytes = bincode::serde::encode_to_vec(server_hello, standard())
        .map_err(|error| anyhow!("failed to encode server hello transcript: {error}"))?;
    Ok(hasher.transcript_hash(&[
        b"entangrid-session-transcript",
        client_bytes.as_slice(),
        server_bytes.as_slice(),
    ]))
}

fn deterministic_session_component(
    deterministic: &DeterministicCryptoBackend,
    local_validator_id: ValidatorId,
    peer_validator_id: ValidatorId,
    nonce: &HashBytes,
) -> Result<HashBytes> {
    Ok(deterministic
        .open_session(local_validator_id, peer_validator_id, nonce)?
        .session_key)
}

fn derive_session_material<H: TranscriptHasher>(
    hasher: &H,
    deterministic_component: &HashBytes,
    kem_component: &[u8],
    client_hello: &SessionClientHello,
    server_hello: &SessionServerHello,
    encrypt_frames: bool,
) -> Result<SessionMaterial> {
    let transcript_hash = session_transcript_hash(hasher, client_hello, server_hello)?;
    let session_key = hash_many(&[
        b"entangrid-session-key-v2",
        deterministic_component,
        kem_component,
        &transcript_hash,
    ]);
    Ok(SessionMaterial {
        session_key,
        transcript_hash,
        encrypt_frames,
    })
}

#[cfg(feature = "pq-ml-kem")]
fn decode_ml_kem_decapsulation_key(bytes: &[u8]) -> Result<MlKem768DecapsulationKey> {
    let encoded = Encoded::<MlKem768DecapsulationKey>::try_from(bytes)
        .map_err(|_| anyhow!("invalid ML-KEM decapsulation key encoding"))?;
    Ok(MlKem768DecapsulationKey::from_bytes(&encoded))
}

#[cfg(feature = "pq-ml-kem")]
fn decode_ml_kem_encapsulation_key(bytes: &[u8]) -> Result<MlKem768EncapsulationKey> {
    let encoded = Encoded::<MlKem768EncapsulationKey>::try_from(bytes)
        .map_err(|_| anyhow!("invalid ML-KEM encapsulation key encoding"))?;
    Ok(MlKem768EncapsulationKey::from_bytes(&encoded))
}

#[cfg(feature = "pq-ml-kem")]
fn decode_ml_kem_ciphertext(bytes: &[u8]) -> Result<MlKem768Ciphertext> {
    ml_kem::Ciphertext::<MlKem768>::try_from(bytes)
        .map_err(|_| anyhow!("invalid ML-KEM ciphertext encoding"))
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
            components: Vec::new(),
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
    fn build_client_hello(
        &self,
        local_validator_id: ValidatorId,
        peer_validator_id: ValidatorId,
        nonce: HashBytes,
    ) -> Result<SessionClientHello> {
        let mut client_hello = SessionClientHello {
            initiator_validator_id: local_validator_id,
            responder_validator_id: peer_validator_id,
            nonce,
            session_public_identity: deterministic_session_public_identity(local_validator_id),
            kem_public_material: Vec::new(),
            signature: TypedSignature::default(),
        };
        let signing_message = client_hello_signing_message(&client_hello)?;
        client_hello.signature = self.sign(local_validator_id, &signing_message)?;
        Ok(client_hello)
    }

    fn accept_client_hello(
        &self,
        local_validator_id: ValidatorId,
        client_hello: &SessionClientHello,
    ) -> Result<(SessionServerHello, SessionMaterial)> {
        if client_hello.responder_validator_id != local_validator_id {
            return Err(anyhow!(
                "client hello responder {} does not match local validator {}",
                client_hello.responder_validator_id,
                local_validator_id
            ));
        }
        if client_hello.session_public_identity
            != deterministic_session_public_identity(client_hello.initiator_validator_id)
        {
            return Err(anyhow!(
                "validator {} advertised an unexpected deterministic session public identity",
                client_hello.initiator_validator_id
            ));
        }
        let signing_message = client_hello_signing_message(client_hello)?;
        if !self.verify(
            client_hello.initiator_validator_id,
            &signing_message,
            &client_hello.signature,
        )? {
            return Err(anyhow!("client hello signature verification failed"));
        }
        let mut server_hello = SessionServerHello {
            responder_validator_id: local_validator_id,
            initiator_validator_id: client_hello.initiator_validator_id,
            nonce: client_hello.nonce,
            session_public_identity: deterministic_session_public_identity(local_validator_id),
            kem_ciphertext: Vec::new(),
            signature: TypedSignature::default(),
        };
        let signing_message = server_hello_signing_message(client_hello, &server_hello)?;
        server_hello.signature = self.sign(local_validator_id, &signing_message)?;
        let deterministic_component = deterministic_session_component(
            self,
            local_validator_id,
            client_hello.initiator_validator_id,
            &client_hello.nonce,
        )?;
        let session = derive_session_material(
            self,
            &deterministic_component,
            &[],
            client_hello,
            &server_hello,
            false,
        )?;
        Ok((server_hello, session))
    }

    fn finalize_client_session(
        &self,
        local_validator_id: ValidatorId,
        client_hello: &SessionClientHello,
        server_hello: &SessionServerHello,
    ) -> Result<SessionMaterial> {
        if client_hello.initiator_validator_id != local_validator_id {
            return Err(anyhow!(
                "client hello initiator {} does not match local validator {}",
                client_hello.initiator_validator_id,
                local_validator_id
            ));
        }
        if server_hello.initiator_validator_id != local_validator_id
            || server_hello.responder_validator_id != client_hello.responder_validator_id
            || server_hello.nonce != client_hello.nonce
        {
            return Err(anyhow!(
                "server hello does not match the client hello transcript"
            ));
        }
        if server_hello.session_public_identity
            != deterministic_session_public_identity(server_hello.responder_validator_id)
        {
            return Err(anyhow!(
                "validator {} advertised an unexpected deterministic session public identity",
                server_hello.responder_validator_id
            ));
        }
        let signing_message = server_hello_signing_message(client_hello, server_hello)?;
        if !self.verify(
            server_hello.responder_validator_id,
            &signing_message,
            &server_hello.signature,
        )? {
            return Err(anyhow!("server hello signature verification failed"));
        }
        let deterministic_component = deterministic_session_component(
            self,
            local_validator_id,
            server_hello.responder_validator_id,
            &client_hello.nonce,
        )?;
        derive_session_material(
            self,
            &deterministic_component,
            &[],
            client_hello,
            server_hello,
            false,
        )
    }

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
            encrypt_frames: false,
        })
    }
}

impl TranscriptHasher for DeterministicCryptoBackend {
    fn transcript_hash(&self, parts: &[&[u8]]) -> HashBytes {
        hash_many(parts)
    }
}

impl ConfiguredCryptoBackend {
    #[cfg(feature = "pq-ml-kem")]
    fn expected_session_identity(
        &self,
        validator_id: ValidatorId,
    ) -> Result<Option<&SessionPublicIdentity>> {
        self.session_identities
            .get(&validator_id)
            .map(|identity| identity.as_ref())
            .ok_or_else(|| anyhow!("unknown validator id {validator_id}"))
    }

    fn local_session_identity(&self, validator_id: ValidatorId) -> Result<SessionPublicIdentity> {
        match &self.local_session_backend {
            LocalSessionBackend::Deterministic => {
                Ok(deterministic_session_public_identity(validator_id))
            }
            #[cfg(feature = "pq-ml-kem")]
            LocalSessionBackend::HybridDeterministicMlKem768(material) => {
                Ok(material.session_public_identity.clone())
            }
        }
    }

    #[cfg(feature = "pq-ml-kem")]
    fn local_ml_kem_session_material(&self) -> Result<&LoadedMlKemSessionMaterial> {
        match &self.local_session_backend {
            LocalSessionBackend::HybridDeterministicMlKem768(material) => Ok(material),
            LocalSessionBackend::Deterministic => Err(anyhow!(
                "ML-KEM session material is unavailable for deterministic session backend"
            )),
        }
    }

    #[cfg(feature = "pq-ml-kem")]
    fn verify_expected_session_identity(
        &self,
        validator_id: ValidatorId,
        advertised: &SessionPublicIdentity,
    ) -> Result<()> {
        if let Some(expected) = self.expected_session_identity(validator_id)? {
            if expected != advertised {
                return Err(anyhow!(
                    "validator {} advertised a session public identity that does not match genesis",
                    validator_id
                ));
            }
        }
        Ok(())
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
                Ok(TypedSignature::single(
                    SignatureScheme::MlDsa,
                    signature.encode().as_slice().to_vec(),
                ))
            }
            #[cfg(feature = "pq-ml-dsa")]
            LocalSigningBackend::HybridDeterministicMlDsa65(signing_key) => {
                if validator_id != self.local_validator_id {
                    return Err(anyhow!(
                        "hybrid signing material is only available for validator {}",
                        self.local_validator_id
                    ));
                }
                let deterministic = self.deterministic.sign(validator_id, message)?;
                let pq_signature: MlDsaSignature<MlDsa65> = signing_key.sign(message);
                TypedSignature::try_hybrid(vec![
                    SignatureComponent {
                        scheme: SignatureScheme::DevDeterministic,
                        bytes: deterministic
                            .as_single_bytes()
                            .ok_or_else(|| {
                                anyhow!("deterministic signature unexpectedly missing bytes")
                            })?
                            .to_vec(),
                    },
                    SignatureComponent {
                        scheme: SignatureScheme::MlDsa,
                        bytes: pq_signature.encode().as_slice().to_vec(),
                    },
                ])
                .map_err(|error| anyhow!(error))
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
        verify_signature_against_identity(self, validator_id, identity, message, signature)
    }
}

fn verify_single_signature_component(
    backend: &ConfiguredCryptoBackend,
    validator_id: ValidatorId,
    message: &[u8],
    scheme: SignatureScheme,
    bytes: &[u8],
) -> Result<bool> {
    match scheme {
        SignatureScheme::DevDeterministic => backend.deterministic.verify(
            validator_id,
            message,
            &TypedSignature::single(SignatureScheme::DevDeterministic, bytes.to_vec()),
        ),
        SignatureScheme::MlDsa => {
            #[cfg(feature = "pq-ml-dsa")]
            {
                let verifying_key = backend
                    .ml_dsa_verifying_keys
                    .get(&validator_id)
                    .ok_or_else(|| {
                        anyhow!("missing ML-DSA verifying key for validator {validator_id}")
                    })?;
                let signature = match MlDsaSignature::<MlDsa65>::try_from(bytes) {
                    Ok(signature) => signature,
                    Err(_) => return Ok(false),
                };
                Ok(verifying_key.verify(message, &signature).is_ok())
            }
            #[cfg(not(feature = "pq-ml-dsa"))]
            {
                let _ = (backend, validator_id, message, bytes);
                Ok(false)
            }
        }
        _ => Ok(false),
    }
}

fn verify_signature_against_identity(
    backend: &ConfiguredCryptoBackend,
    validator_id: ValidatorId,
    identity: &PublicIdentity,
    message: &[u8],
    signature: &TypedSignature,
) -> Result<bool> {
    match identity.scheme() {
        PublicKeyScheme::DevDeterministic => {
            if signature.scheme() != SignatureScheme::DevDeterministic {
                return Ok(false);
            }
            let bytes = match signature.as_single_bytes() {
                Some(bytes) => bytes,
                None => return Ok(false),
            };
            verify_single_signature_component(
                backend,
                validator_id,
                message,
                SignatureScheme::DevDeterministic,
                bytes,
            )
        }
        PublicKeyScheme::MlDsa => {
            if signature.scheme() != SignatureScheme::MlDsa {
                return Ok(false);
            }
            let bytes = match signature.as_single_bytes() {
                Some(bytes) => bytes,
                None => return Ok(false),
            };
            verify_single_signature_component(
                backend,
                validator_id,
                message,
                SignatureScheme::MlDsa,
                bytes,
            )
        }
        PublicKeyScheme::Hybrid => {
            if signature.scheme() == SignatureScheme::Hybrid {
                if signature.components().len() != identity.components().len() {
                    return Ok(false);
                }
                for component in identity.components() {
                    let signature_scheme = match component.scheme {
                        PublicKeyScheme::DevDeterministic => SignatureScheme::DevDeterministic,
                        PublicKeyScheme::MlDsa => SignatureScheme::MlDsa,
                        _ => return Ok(false),
                    };
                    let signature_bytes = match signature.component_bytes(signature_scheme.clone())
                    {
                        Some(bytes) => bytes,
                        None => return Ok(false),
                    };
                    if !verify_single_signature_component(
                        backend,
                        validator_id,
                        message,
                        signature_scheme,
                        signature_bytes,
                    )? {
                        return Ok(false);
                    }
                }
                Ok(true)
            } else {
                let public_key_scheme = match signature.scheme() {
                    SignatureScheme::DevDeterministic => PublicKeyScheme::DevDeterministic,
                    SignatureScheme::MlDsa => PublicKeyScheme::MlDsa,
                    _ => return Ok(false),
                };
                if identity.component_bytes(public_key_scheme).is_none() {
                    return Ok(false);
                }
                let bytes = match signature.as_single_bytes() {
                    Some(bytes) => bytes,
                    None => return Ok(false),
                };
                verify_single_signature_component(
                    backend,
                    validator_id,
                    message,
                    signature.scheme(),
                    bytes,
                )
            }
        }
        _ => Ok(false),
    }
}

impl HandshakeProvider for ConfiguredCryptoBackend {
    fn build_client_hello(
        &self,
        local_validator_id: ValidatorId,
        peer_validator_id: ValidatorId,
        nonce: HashBytes,
    ) -> Result<SessionClientHello> {
        let session_public_identity = self.local_session_identity(local_validator_id)?;
        let kem_public_material = match &self.local_session_backend {
            LocalSessionBackend::Deterministic => Vec::new(),
            #[cfg(feature = "pq-ml-kem")]
            LocalSessionBackend::HybridDeterministicMlKem768(material) => {
                material.key_file.encapsulation_key.clone()
            }
        };
        let mut client_hello = SessionClientHello {
            initiator_validator_id: local_validator_id,
            responder_validator_id: peer_validator_id,
            nonce,
            session_public_identity,
            kem_public_material,
            signature: TypedSignature::default(),
        };
        let signing_message = client_hello_signing_message(&client_hello)?;
        client_hello.signature = self.sign(local_validator_id, &signing_message)?;
        Ok(client_hello)
    }

    fn accept_client_hello(
        &self,
        local_validator_id: ValidatorId,
        client_hello: &SessionClientHello,
    ) -> Result<(SessionServerHello, SessionMaterial)> {
        if client_hello.responder_validator_id != local_validator_id {
            return Err(anyhow!(
                "client hello responder {} does not match local validator {}",
                client_hello.responder_validator_id,
                local_validator_id
            ));
        }
        let signing_message = client_hello_signing_message(client_hello)?;
        if !self.verify(
            client_hello.initiator_validator_id,
            &signing_message,
            &client_hello.signature,
        )? {
            return Err(anyhow!("client hello signature verification failed"));
        }

        let (kem_component, kem_ciphertext) = match &self.local_session_backend {
            LocalSessionBackend::Deterministic => {
                if !client_hello.kem_public_material.is_empty() {
                    return Err(anyhow!(
                        "deterministic session backend does not accept ML-KEM client material"
                    ));
                }
                if client_hello.session_public_identity
                    != deterministic_session_public_identity(client_hello.initiator_validator_id)
                {
                    return Err(anyhow!(
                        "validator {} advertised an unexpected deterministic session public identity",
                        client_hello.initiator_validator_id
                    ));
                }
                (Vec::new(), Vec::new())
            }
            #[cfg(feature = "pq-ml-kem")]
            LocalSessionBackend::HybridDeterministicMlKem768(_) => {
                self.verify_expected_session_identity(
                    client_hello.initiator_validator_id,
                    &client_hello.session_public_identity,
                )?;
                let expected_ml_kem = match client_hello.session_public_identity.scheme() {
                    SessionKeyScheme::MlKem => client_hello
                        .session_public_identity
                        .as_single_bytes()
                        .ok_or_else(|| {
                            anyhow!(
                                "validator {} session public identity missing ML-KEM bytes",
                                client_hello.initiator_validator_id
                            )
                        })?,
                    SessionKeyScheme::Hybrid => client_hello
                        .session_public_identity
                        .component_bytes(SessionKeyScheme::MlKem)
                        .ok_or_else(|| {
                            anyhow!(
                                "validator {} session public identity missing ML-KEM component",
                                client_hello.initiator_validator_id
                            )
                        })?,
                    SessionKeyScheme::DevDeterministic => {
                        return Err(anyhow!(
                            "validator {} session public identity does not carry ML-KEM material",
                            client_hello.initiator_validator_id
                        ));
                    }
                };
                if client_hello.kem_public_material.as_slice() != expected_ml_kem {
                    return Err(anyhow!(
                        "validator {} session public identity does not match advertised ML-KEM material",
                        client_hello.initiator_validator_id
                    ));
                }
                let initiator_encapsulation_key =
                    decode_ml_kem_encapsulation_key(&client_hello.kem_public_material)?;
                let mut rng = OsRng;
                let (ciphertext, shared_key) = initiator_encapsulation_key
                    .encapsulate(&mut rng)
                    .map_err(|_| anyhow!("failed to encapsulate ML-KEM shared secret"))?;
                (
                    shared_key.as_slice().to_vec(),
                    ciphertext.as_slice().to_vec(),
                )
            }
        };

        let mut server_hello = SessionServerHello {
            responder_validator_id: local_validator_id,
            initiator_validator_id: client_hello.initiator_validator_id,
            nonce: client_hello.nonce,
            session_public_identity: self.local_session_identity(local_validator_id)?,
            kem_ciphertext,
            signature: TypedSignature::default(),
        };
        let signing_message = server_hello_signing_message(client_hello, &server_hello)?;
        server_hello.signature = self.sign(local_validator_id, &signing_message)?;
        let deterministic_component = deterministic_session_component(
            &self.deterministic,
            local_validator_id,
            client_hello.initiator_validator_id,
            &client_hello.nonce,
        )?;
        let session = derive_session_material(
            self,
            &deterministic_component,
            kem_component.as_slice(),
            client_hello,
            &server_hello,
            true,
        )?;
        Ok((server_hello, session))
    }

    fn finalize_client_session(
        &self,
        local_validator_id: ValidatorId,
        client_hello: &SessionClientHello,
        server_hello: &SessionServerHello,
    ) -> Result<SessionMaterial> {
        if client_hello.initiator_validator_id != local_validator_id {
            return Err(anyhow!(
                "client hello initiator {} does not match local validator {}",
                client_hello.initiator_validator_id,
                local_validator_id
            ));
        }
        if server_hello.initiator_validator_id != local_validator_id
            || server_hello.responder_validator_id != client_hello.responder_validator_id
            || server_hello.nonce != client_hello.nonce
        {
            return Err(anyhow!(
                "server hello does not match the client hello transcript"
            ));
        }
        let signing_message = server_hello_signing_message(client_hello, server_hello)?;
        if !self.verify(
            server_hello.responder_validator_id,
            &signing_message,
            &server_hello.signature,
        )? {
            return Err(anyhow!("server hello signature verification failed"));
        }

        let kem_component = match &self.local_session_backend {
            LocalSessionBackend::Deterministic => {
                if !server_hello.kem_ciphertext.is_empty() {
                    return Err(anyhow!(
                        "deterministic session backend does not accept ML-KEM server material"
                    ));
                }
                if server_hello.session_public_identity
                    != deterministic_session_public_identity(server_hello.responder_validator_id)
                {
                    return Err(anyhow!(
                        "validator {} advertised an unexpected deterministic session public identity",
                        server_hello.responder_validator_id
                    ));
                }
                Vec::new()
            }
            #[cfg(feature = "pq-ml-kem")]
            LocalSessionBackend::HybridDeterministicMlKem768(_) => {
                self.verify_expected_session_identity(
                    server_hello.responder_validator_id,
                    &server_hello.session_public_identity,
                )?;
                let local_material = self.local_ml_kem_session_material()?;
                let decapsulation_key =
                    decode_ml_kem_decapsulation_key(&local_material.key_file.decapsulation_key)?;
                let ciphertext = decode_ml_kem_ciphertext(&server_hello.kem_ciphertext)?;
                decapsulation_key
                    .decapsulate(&ciphertext)
                    .map_err(|_| anyhow!("failed to decapsulate ML-KEM shared secret"))?
                    .as_slice()
                    .to_vec()
            }
        };
        let deterministic_component = deterministic_session_component(
            &self.deterministic,
            local_validator_id,
            server_hello.responder_validator_id,
            &client_hello.nonce,
        )?;
        derive_session_material(
            self,
            &deterministic_component,
            kem_component.as_slice(),
            client_hello,
            server_hello,
            true,
        )
    }

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
    #[cfg(feature = "pq-ml-dsa")]
    use entangrid_types::SignatureComponent;
    use entangrid_types::{
        FaultProfile, FeatureFlags, GenesisConfig, NodeConfig, PublicIdentity,
        PublicIdentityComponent, PublicKeyScheme, SessionKeyScheme, SessionPublicIdentity,
        SessionPublicIdentityComponent, SignatureScheme, SigningBackendKind, TypedSignature,
        ValidatorConfig, ValidatorId, empty_hash,
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
                session_public_identity: None,
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
                session_public_identity: None,
            }],
            initial_balances: Default::default(),
        };
        let backend = DeterministicCryptoBackend::from_genesis(&genesis);
        let message = b"typed-signature";
        let signature: TypedSignature = backend.sign(1, message).unwrap();
        assert_eq!(signature.scheme(), SignatureScheme::DevDeterministic);
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
                public_identity: PublicIdentity::single(
                    PublicKeyScheme::DevDeterministic,
                    b"validator-1".to_vec(),
                ),
                session_public_identity: None,
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
            session_backend: SessionBackendKind::DevDeterministic,
            session_key_path: None,
            session_ttl_millis: None,
        };
        let backend = build_crypto_backend(&genesis, &config).unwrap();
        let signature = backend.sign(1, b"factory").unwrap();
        assert_eq!(signature.scheme(), SignatureScheme::DevDeterministic);
    }

    #[test]
    fn deterministic_session_backend_accepts_default_config() {
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
                public_identity: PublicIdentity::single(
                    PublicKeyScheme::DevDeterministic,
                    b"validator-1".to_vec(),
                ),
                session_public_identity: None,
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
            session_backend: SessionBackendKind::DevDeterministic,
            session_key_path: None,
            session_ttl_millis: None,
        };

        build_crypto_backend(&genesis, &config).unwrap();
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
                public_identity: PublicIdentity::single(
                    PublicKeyScheme::DevDeterministic,
                    b"wrong-validator".to_vec(),
                ),
                session_public_identity: None,
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
            session_backend: SessionBackendKind::DevDeterministic,
            session_key_path: None,
            session_ttl_millis: None,
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
                session_public_identity: None,
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
            session_backend: SessionBackendKind::DevDeterministic,
            session_key_path: None,
            session_ttl_millis: None,
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

    fn hybrid_session_public_identity(encapsulation_key: Vec<u8>) -> SessionPublicIdentity {
        SessionPublicIdentity::try_hybrid(vec![
            SessionPublicIdentityComponent {
                scheme: SessionKeyScheme::DevDeterministic,
                bytes: b"session-validator-1".to_vec(),
            },
            SessionPublicIdentityComponent {
                scheme: SessionKeyScheme::MlKem,
                bytes: encapsulation_key,
            },
        ])
        .unwrap()
    }

    fn two_validator_genesis() -> GenesisConfig {
        GenesisConfig {
            chain_id: "entangrid-test".into(),
            epoch_seed: empty_hash(),
            genesis_time_unix_millis: 0,
            slot_duration_millis: 1000,
            slots_per_epoch: 10,
            max_txs_per_block: 16,
            witness_count: 2,
            validators: vec![
                ValidatorConfig {
                    validator_id: 1,
                    stake: 100,
                    address: "127.0.0.1:3001".into(),
                    dev_secret: "secret-1".into(),
                    public_identity: deterministic_public_identity(1),
                    session_public_identity: None,
                },
                ValidatorConfig {
                    validator_id: 2,
                    stake: 100,
                    address: "127.0.0.1:3002".into(),
                    dev_secret: "secret-2".into(),
                    public_identity: deterministic_public_identity(2),
                    session_public_identity: None,
                },
            ],
            initial_balances: Default::default(),
        }
    }

    fn deterministic_node_config(validator_id: ValidatorId) -> NodeConfig {
        NodeConfig {
            validator_id,
            data_dir: format!("/tmp/node-{validator_id}"),
            genesis_path: "/tmp/genesis.toml".into(),
            listen_address: format!("127.0.0.1:{}", 3000 + validator_id),
            peers: Vec::new(),
            log_path: "/tmp/events.log".into(),
            metrics_path: "/tmp/metrics.json".into(),
            feature_flags: FeatureFlags::default(),
            fault_profile: FaultProfile::default(),
            sync_on_startup: true,
            signing_backend: SigningBackendKind::DevDeterministic,
            signing_key_path: None,
            session_backend: SessionBackendKind::DevDeterministic,
            session_key_path: None,
            session_ttl_millis: None,
        }
    }

    #[test]
    fn deterministic_session_handshake_round_trips_between_validators() {
        let genesis = two_validator_genesis();
        let client = build_crypto_backend(&genesis, &deterministic_node_config(1)).unwrap();
        let server = build_crypto_backend(&genesis, &deterministic_node_config(2)).unwrap();
        let nonce = [7; 32];

        let client_hello = client.build_client_hello(1, 2, nonce).unwrap();
        let (server_hello, server_session) = server.accept_client_hello(2, &client_hello).unwrap();
        let client_session = client
            .finalize_client_session(1, &client_hello, &server_hello)
            .unwrap();

        assert_eq!(client_session, server_session);
    }

    #[test]
    fn handshake_rejects_invalid_client_transcript_signature() {
        let genesis = two_validator_genesis();
        let client = build_crypto_backend(&genesis, &deterministic_node_config(1)).unwrap();
        let server = build_crypto_backend(&genesis, &deterministic_node_config(2)).unwrap();
        let nonce = [9; 32];

        let mut client_hello = client.build_client_hello(1, 2, nonce).unwrap();
        client_hello.signature =
            TypedSignature::single(SignatureScheme::DevDeterministic, vec![0; 32]);

        let error = match server.accept_client_hello(2, &client_hello) {
            Ok(_) => panic!("expected invalid client signature rejection"),
            Err(error) => error,
        };
        assert!(
            error.to_string().contains("client hello signature"),
            "unexpected error: {error:?}"
        );
    }

    #[test]
    fn handshake_rejects_invalid_server_transcript_signature() {
        let genesis = two_validator_genesis();
        let client = build_crypto_backend(&genesis, &deterministic_node_config(1)).unwrap();
        let server = build_crypto_backend(&genesis, &deterministic_node_config(2)).unwrap();
        let nonce = [11; 32];

        let client_hello = client.build_client_hello(1, 2, nonce).unwrap();
        let (mut server_hello, _) = server.accept_client_hello(2, &client_hello).unwrap();
        server_hello.signature =
            TypedSignature::single(SignatureScheme::DevDeterministic, vec![1; 32]);

        let error = match client.finalize_client_session(1, &client_hello, &server_hello) {
            Ok(_) => panic!("expected invalid server signature rejection"),
            Err(error) => error,
        };
        assert!(
            error.to_string().contains("server hello signature"),
            "unexpected error: {error:?}"
        );
    }

    #[cfg(not(feature = "pq-ml-kem"))]
    #[test]
    fn hybrid_session_backend_requires_pq_kem_feature() {
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
                session_public_identity: Some(hybrid_session_public_identity(vec![7, 7, 7])),
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
            session_backend: SessionBackendKind::HybridDeterministicMlKemExperimental,
            session_key_path: Some("/tmp/ml-kem.session".into()),
            session_ttl_millis: None,
        };
        let error = match build_crypto_backend(&genesis, &config) {
            Ok(_) => panic!("expected ML-KEM feature gate failure"),
            Err(error) => error,
        };
        assert!(
            error.to_string().contains("built without pq-ml-kem"),
            "unexpected error: {error:?}"
        );
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
                public_identity: PublicIdentity::single(PublicKeyScheme::MlDsa, vec![7, 7, 7]),
                session_public_identity: None,
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
            session_backend: SessionBackendKind::DevDeterministic,
            session_key_path: None,
            session_ttl_millis: None,
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
                public_identity: PublicIdentity::single(
                    PublicKeyScheme::MlDsa,
                    verifying_key.encode().as_slice().to_vec(),
                ),
                session_public_identity: None,
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
            session_backend: SessionBackendKind::DevDeterministic,
            session_key_path: None,
            session_ttl_millis: None,
        };
        let backend = build_crypto_backend(&genesis, &config).unwrap();
        let signature = backend.sign(1, b"ml-dsa-backend").unwrap();
        assert_eq!(signature.scheme(), SignatureScheme::MlDsa);
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
                public_identity: PublicIdentity::single(
                    PublicKeyScheme::MlDsa,
                    verifying_key.encode().as_slice().to_vec(),
                ),
                session_public_identity: None,
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
            session_backend: SessionBackendKind::DevDeterministic,
            session_key_path: None,
            session_ttl_millis: None,
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

    #[cfg(not(feature = "pq-ml-dsa"))]
    #[test]
    fn backend_factory_rejects_hybrid_selection_without_feature() {
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
                public_identity: PublicIdentity::try_hybrid(vec![
                    PublicIdentityComponent {
                        scheme: PublicKeyScheme::DevDeterministic,
                        bytes: b"validator-1".to_vec(),
                    },
                    PublicIdentityComponent {
                        scheme: PublicKeyScheme::MlDsa,
                        bytes: vec![7, 7, 7],
                    },
                ])
                .unwrap(),
                session_public_identity: None,
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
            signing_backend: SigningBackendKind::HybridDeterministicMlDsaExperimental,
            signing_key_path: Some("/tmp/ml-dsa.sk".into()),
            session_backend: SessionBackendKind::DevDeterministic,
            session_key_path: None,
            session_ttl_millis: None,
        };
        let error = match build_crypto_backend(&genesis, &config) {
            Ok(_) => panic!("expected hybrid feature gate failure"),
            Err(error) => error,
        };
        assert!(
            error.to_string().contains("built without pq-ml-dsa"),
            "unexpected error: {error:?}"
        );
    }

    #[cfg(feature = "pq-ml-dsa")]
    fn write_ml_dsa_key_file(label: &str) -> (std::path::PathBuf, MlDsaVerifyingKey<MlDsa65>) {
        use ml_dsa::{KeyGen, MlDsa65};
        use rand_core::OsRng;
        use std::time::{SystemTime, UNIX_EPOCH};

        let mut rng = OsRng;
        let keypair = MlDsa65::key_gen(&mut rng);
        let signing_key = keypair.signing_key().clone();
        let verifying_key = keypair.verifying_key().clone();
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let key_path = std::env::temp_dir().join(format!(
            "entangrid-{label}-{}-{}.key",
            std::process::id(),
            nonce
        ));
        let key_file = MlDsa65KeyFile {
            signing_key: signing_key.encode().as_slice().to_vec(),
            verifying_key: verifying_key.encode().as_slice().to_vec(),
        };
        std::fs::write(&key_path, serde_json::to_vec(&key_file).unwrap()).unwrap();
        (key_path, verifying_key)
    }

    #[cfg(feature = "pq-ml-kem")]
    fn write_ml_kem_session_key_file(label: &str) -> (std::path::PathBuf, Vec<u8>) {
        use ml_kem::{EncodedSizeUser, KemCore, MlKem768};
        use rand_core::OsRng;
        use std::time::{SystemTime, UNIX_EPOCH};

        let mut rng = OsRng;
        let (decapsulation_key, encapsulation_key) = MlKem768::generate(&mut rng);
        let decapsulation_key_bytes = decapsulation_key.as_bytes().as_slice().to_vec();
        let encapsulation_key_bytes = encapsulation_key.as_bytes().as_slice().to_vec();
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let key_path = std::env::temp_dir().join(format!(
            "entangrid-{label}-session-{}-{}.key",
            std::process::id(),
            nonce
        ));
        let key_file = MlKemSessionKeyFile {
            decapsulation_key: decapsulation_key_bytes,
            encapsulation_key: encapsulation_key_bytes.clone(),
        };
        std::fs::write(&key_path, serde_json::to_vec(&key_file).unwrap()).unwrap();
        (key_path, encapsulation_key_bytes)
    }

    #[cfg(feature = "pq-ml-kem")]
    fn session_backend_test_genesis_and_config(
        session_public_identity: Option<SessionPublicIdentity>,
        session_backend: SessionBackendKind,
        session_key_path: Option<String>,
    ) -> (GenesisConfig, NodeConfig) {
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
                session_public_identity,
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
            session_backend,
            session_key_path,
            session_ttl_millis: None,
        };
        (genesis, config)
    }

    #[cfg(feature = "pq-ml-kem")]
    fn hybrid_session_genesis_and_configs() -> (GenesisConfig, NodeConfig, NodeConfig) {
        let (key_path_one, key_one) = write_ml_kem_session_key_file("handshake-one");
        let (key_path_two, key_two) = write_ml_kem_session_key_file("handshake-two");
        let genesis = GenesisConfig {
            chain_id: "entangrid-test".into(),
            epoch_seed: empty_hash(),
            genesis_time_unix_millis: 0,
            slot_duration_millis: 1000,
            slots_per_epoch: 10,
            max_txs_per_block: 16,
            witness_count: 2,
            validators: vec![
                ValidatorConfig {
                    validator_id: 1,
                    stake: 100,
                    address: "127.0.0.1:3001".into(),
                    dev_secret: "secret-1".into(),
                    public_identity: deterministic_public_identity(1),
                    session_public_identity: Some(hybrid_session_public_identity(key_one)),
                },
                ValidatorConfig {
                    validator_id: 2,
                    stake: 100,
                    address: "127.0.0.1:3002".into(),
                    dev_secret: "secret-2".into(),
                    public_identity: deterministic_public_identity(2),
                    session_public_identity: Some(hybrid_session_public_identity(key_two)),
                },
            ],
            initial_balances: Default::default(),
        };
        let config_one = NodeConfig {
            validator_id: 1,
            session_backend: SessionBackendKind::HybridDeterministicMlKemExperimental,
            session_key_path: Some(key_path_one.display().to_string()),
            ..deterministic_node_config(1)
        };
        let config_two = NodeConfig {
            validator_id: 2,
            session_backend: SessionBackendKind::HybridDeterministicMlKemExperimental,
            session_key_path: Some(key_path_two.display().to_string()),
            ..deterministic_node_config(2)
        };
        (genesis, config_one, config_two)
    }

    #[cfg(feature = "pq-ml-kem")]
    #[test]
    fn hybrid_session_backend_requires_session_key_path() {
        let (genesis, config) = session_backend_test_genesis_and_config(
            Some(hybrid_session_public_identity(vec![9, 9, 9])),
            SessionBackendKind::HybridDeterministicMlKemExperimental,
            None,
        );
        let error = match build_crypto_backend(&genesis, &config) {
            Ok(_) => panic!("expected missing session key path failure"),
            Err(error) => error,
        };
        assert!(
            error.to_string().contains("session_key_path"),
            "unexpected error: {error:?}"
        );
    }

    #[cfg(feature = "pq-ml-kem")]
    #[test]
    fn session_backend_rejects_mismatched_session_public_identity() {
        let (key_path, matching_encapsulation_key) = write_ml_kem_session_key_file("session-match");
        let mismatched_identity =
            hybrid_session_public_identity([matching_encapsulation_key, vec![42]].concat());
        let (genesis, config) = session_backend_test_genesis_and_config(
            Some(mismatched_identity),
            SessionBackendKind::HybridDeterministicMlKemExperimental,
            Some(key_path.display().to_string()),
        );
        let error = match build_crypto_backend(&genesis, &config) {
            Ok(_) => panic!("expected mismatched session identity failure"),
            Err(error) => error,
        };
        assert!(
            error.to_string().contains("session public identity"),
            "unexpected error: {error:?}"
        );
    }

    #[cfg(feature = "pq-ml-kem")]
    #[test]
    fn hybrid_session_backend_accepts_matching_session_public_identity() {
        let (key_path, encapsulation_key) = write_ml_kem_session_key_file("session-good");
        let (genesis, config) = session_backend_test_genesis_and_config(
            Some(hybrid_session_public_identity(encapsulation_key)),
            SessionBackendKind::HybridDeterministicMlKemExperimental,
            Some(key_path.display().to_string()),
        );
        build_crypto_backend(&genesis, &config).unwrap();
    }

    #[cfg(feature = "pq-ml-kem")]
    #[test]
    fn hybrid_session_handshake_round_trips_between_validators() {
        let (genesis, client_config, server_config) = hybrid_session_genesis_and_configs();
        let client = build_crypto_backend(&genesis, &client_config).unwrap();
        let server = build_crypto_backend(&genesis, &server_config).unwrap();
        let nonce = [13; 32];

        let client_hello = client.build_client_hello(1, 2, nonce).unwrap();
        let (server_hello, server_session) = server.accept_client_hello(2, &client_hello).unwrap();
        let client_session = client
            .finalize_client_session(1, &client_hello, &server_hello)
            .unwrap();

        assert_eq!(client_session, server_session);
    }

    #[cfg(feature = "pq-ml-kem")]
    #[test]
    fn hybrid_handshake_rejects_mismatched_peer_session_identity() {
        let (genesis, client_config, server_config) = hybrid_session_genesis_and_configs();
        let client = build_crypto_backend(&genesis, &client_config).unwrap();
        let server = build_crypto_backend(&genesis, &server_config).unwrap();
        let nonce = [17; 32];

        let mut client_hello = client.build_client_hello(1, 2, nonce).unwrap();
        client_hello.session_public_identity = hybrid_session_public_identity(vec![5; 1184]);
        client_hello.kem_public_material = vec![5; 1184];
        let signing_message = client_hello_signing_message(&client_hello).unwrap();
        client_hello.signature = client.sign(1, &signing_message).unwrap();

        let error = match server.accept_client_hello(2, &client_hello) {
            Ok(_) => panic!("expected peer session identity mismatch"),
            Err(error) => error,
        };
        assert!(
            error.to_string().contains("does not match genesis"),
            "unexpected error: {error:?}"
        );
    }

    #[cfg(feature = "pq-ml-kem")]
    #[test]
    fn encrypted_frame_round_trips_with_matching_counter() {
        let session = SessionMaterial {
            session_key: [1; 32],
            transcript_hash: [2; 32],
            encrypt_frames: true,
        };
        let plaintext = b"encrypted framing";

        let ciphertext =
            encrypt_frame_payload(&session, FrameDirection::Outbound, 0, plaintext).unwrap();
        let decrypted =
            decrypt_frame_payload(&session, FrameDirection::Outbound, 0, &ciphertext).unwrap();

        assert_eq!(decrypted, plaintext);
    }

    #[cfg(feature = "pq-ml-kem")]
    #[test]
    fn encrypted_frame_rejects_tampered_ciphertext() {
        let session = SessionMaterial {
            session_key: [1; 32],
            transcript_hash: [2; 32],
            encrypt_frames: true,
        };
        let plaintext = b"tamper-check";
        let mut ciphertext =
            encrypt_frame_payload(&session, FrameDirection::Outbound, 0, plaintext).unwrap();
        let last = ciphertext.last_mut().expect("ciphertext tag byte");
        *last ^= 0x01;

        let error = decrypt_frame_payload(&session, FrameDirection::Outbound, 0, &ciphertext)
            .expect_err("tampered ciphertext should fail");
        assert!(
            error.to_string().contains("frame decryption failed"),
            "unexpected error: {error:?}"
        );
    }

    #[cfg(feature = "pq-ml-kem")]
    #[test]
    fn encrypted_frame_nonce_depends_on_direction() {
        let session = SessionMaterial {
            session_key: [1; 32],
            transcript_hash: [2; 32],
            encrypt_frames: true,
        };
        let plaintext = b"direction matters";

        let outbound =
            encrypt_frame_payload(&session, FrameDirection::Outbound, 7, plaintext).unwrap();
        let inbound =
            encrypt_frame_payload(&session, FrameDirection::Inbound, 7, plaintext).unwrap();

        assert_ne!(outbound, inbound);
    }

    #[cfg(feature = "pq-ml-kem")]
    #[test]
    fn encrypted_frame_rejects_wrong_counter() {
        let session = SessionMaterial {
            session_key: [1; 32],
            transcript_hash: [2; 32],
            encrypt_frames: true,
        };
        let ciphertext =
            encrypt_frame_payload(&session, FrameDirection::Outbound, 3, b"counter-check").unwrap();

        let error = decrypt_frame_payload(&session, FrameDirection::Outbound, 4, &ciphertext)
            .expect_err("wrong counter should fail");
        assert!(
            error.to_string().contains("frame decryption failed"),
            "unexpected error: {error:?}"
        );
    }

    #[cfg(feature = "pq-ml-dsa")]
    fn hybrid_test_genesis_config(
        signing_backend: SigningBackendKind,
        public_identity: PublicIdentity,
        key_path: Option<String>,
    ) -> (GenesisConfig, NodeConfig) {
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
                public_identity,
                session_public_identity: None,
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
            signing_backend,
            signing_key_path: key_path,
            session_backend: SessionBackendKind::DevDeterministic,
            session_key_path: None,
            session_ttl_millis: None,
        };
        (genesis, config)
    }

    #[cfg(feature = "pq-ml-dsa")]
    fn hybrid_identity_for_validator(
        validator_id: ValidatorId,
        verifying_key: &MlDsaVerifyingKey<MlDsa65>,
    ) -> PublicIdentity {
        PublicIdentity::try_hybrid(vec![
            PublicIdentityComponent {
                scheme: PublicKeyScheme::DevDeterministic,
                bytes: deterministic_public_identity(validator_id)
                    .as_single_bytes()
                    .unwrap()
                    .to_vec(),
            },
            PublicIdentityComponent {
                scheme: PublicKeyScheme::MlDsa,
                bytes: verifying_key.encode().as_slice().to_vec(),
            },
        ])
        .unwrap()
    }

    #[cfg(feature = "pq-ml-dsa")]
    #[test]
    fn hybrid_backend_emits_signature_that_verifies_against_matching_hybrid_identity() {
        let (key_path, verifying_key) = write_ml_dsa_key_file("hybrid-sign");
        let public_identity = hybrid_identity_for_validator(1, &verifying_key);
        let (genesis, config) = hybrid_test_genesis_config(
            SigningBackendKind::HybridDeterministicMlDsaExperimental,
            public_identity,
            Some(key_path.display().to_string()),
        );
        let backend = build_crypto_backend(&genesis, &config).unwrap();
        let signature = backend.sign(1, b"hybrid-backend").unwrap();
        assert_eq!(signature.scheme(), SignatureScheme::Hybrid);
        assert!(
            signature
                .component_bytes(SignatureScheme::DevDeterministic)
                .is_some()
        );
        assert!(signature.component_bytes(SignatureScheme::MlDsa).is_some());
        assert!(backend.verify(1, b"hybrid-backend", &signature).unwrap());
    }

    #[cfg(feature = "pq-ml-dsa")]
    #[test]
    fn deterministic_signature_verifies_against_matching_hybrid_identity() {
        let (_key_path, verifying_key) = write_ml_dsa_key_file("hybrid-det");
        let public_identity = hybrid_identity_for_validator(1, &verifying_key);
        let (genesis, config) =
            hybrid_test_genesis_config(SigningBackendKind::DevDeterministic, public_identity, None);
        let backend = build_crypto_backend(&genesis, &config).unwrap();
        let signature = backend.sign(1, b"hybrid-det").unwrap();
        assert_eq!(signature.scheme(), SignatureScheme::DevDeterministic);
        assert!(backend.verify(1, b"hybrid-det", &signature).unwrap());
    }

    #[cfg(feature = "pq-ml-dsa")]
    #[test]
    fn ml_dsa_signature_verifies_against_matching_hybrid_identity() {
        let (key_path, verifying_key) = write_ml_dsa_key_file("hybrid-ml-dsa");
        let public_identity = hybrid_identity_for_validator(1, &verifying_key);
        let (genesis, config) = hybrid_test_genesis_config(
            SigningBackendKind::MlDsa65Experimental,
            public_identity,
            Some(key_path.display().to_string()),
        );
        let backend = build_crypto_backend(&genesis, &config).unwrap();
        let signature = backend.sign(1, b"hybrid-ml-dsa").unwrap();
        assert_eq!(signature.scheme(), SignatureScheme::MlDsa);
        assert!(backend.verify(1, b"hybrid-ml-dsa", &signature).unwrap());
    }

    #[cfg(feature = "pq-ml-dsa")]
    #[test]
    fn hybrid_verification_rejects_missing_or_mismatched_components() {
        let (key_path, verifying_key) = write_ml_dsa_key_file("hybrid-invalid");
        let public_identity = hybrid_identity_for_validator(1, &verifying_key);
        let (genesis, config) = hybrid_test_genesis_config(
            SigningBackendKind::HybridDeterministicMlDsaExperimental,
            public_identity,
            Some(key_path.display().to_string()),
        );
        let backend = build_crypto_backend(&genesis, &config).unwrap();
        let good_signature = backend.sign(1, b"hybrid-invalid").unwrap();

        let missing_component = TypedSignature::try_hybrid(vec![SignatureComponent {
            scheme: SignatureScheme::DevDeterministic,
            bytes: good_signature
                .component_bytes(SignatureScheme::DevDeterministic)
                .unwrap()
                .to_vec(),
        }])
        .unwrap();
        assert!(
            !backend
                .verify(1, b"hybrid-invalid", &missing_component)
                .unwrap()
        );

        let mismatched_component = TypedSignature::try_hybrid(vec![
            SignatureComponent {
                scheme: SignatureScheme::DevDeterministic,
                bytes: good_signature
                    .component_bytes(SignatureScheme::DevDeterministic)
                    .unwrap()
                    .to_vec(),
            },
            SignatureComponent {
                scheme: SignatureScheme::MlDsa,
                bytes: vec![1, 2, 3, 4],
            },
        ])
        .unwrap();
        assert!(
            !backend
                .verify(1, b"hybrid-invalid", &mismatched_component)
                .unwrap()
        );
    }
}
