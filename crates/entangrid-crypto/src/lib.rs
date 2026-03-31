use std::{collections::BTreeMap, sync::Arc};

use anyhow::{Result, anyhow};
use entangrid_types::{
    GenesisConfig, HashBytes, SignatureScheme, TypedSignature, ValidatorConfig, ValidatorId,
    hash_many,
};

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

#[cfg(test)]
mod tests {
    use entangrid_types::{
        GenesisConfig, PublicIdentity, SignatureScheme, TypedSignature, ValidatorConfig,
        empty_hash,
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
}
