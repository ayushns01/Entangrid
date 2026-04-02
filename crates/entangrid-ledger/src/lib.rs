use std::collections::BTreeMap;

use anyhow::{Result, anyhow, bail};
use entangrid_crypto::CryptoBackend;
use entangrid_types::{Block, GenesisConfig, SignedTransaction, StateSnapshot, validator_account};

#[derive(Clone, Debug)]
pub struct LedgerState {
    snapshot: StateSnapshot,
}

impl LedgerState {
    pub fn from_genesis(genesis: &GenesisConfig) -> Self {
        let snapshot = StateSnapshot {
            balances: genesis.initial_balances.clone(),
            nonces: BTreeMap::new(),
            tip_hash: entangrid_types::empty_hash(),
            height: 0,
            last_slot: 0,
        };
        Self { snapshot }
    }

    pub fn from_snapshot(snapshot: StateSnapshot) -> Self {
        Self { snapshot }
    }

    pub fn snapshot(&self) -> &StateSnapshot {
        &self.snapshot
    }

    pub fn validate_tx(
        &self,
        transaction: &SignedTransaction,
        crypto: &dyn CryptoBackend,
    ) -> Result<()> {
        let expected_hash = entangrid_types::canonical_hash(&transaction.transaction);
        if expected_hash != transaction.tx_hash {
            bail!("transaction hash mismatch");
        }

        let from_account = validator_account(transaction.signer_id);
        if transaction.transaction.from != from_account {
            bail!("transaction source account must match signer");
        }

        let verified = crypto.verify(
            transaction.signer_id,
            &transaction.tx_hash,
            &transaction.signature,
        )?;
        if !verified {
            bail!("transaction signature verification failed");
        }

        let balance = self
            .snapshot
            .balances
            .get(&transaction.transaction.from)
            .copied()
            .unwrap_or_default();
        if balance < transaction.transaction.amount {
            bail!("insufficient balance");
        }

        let expected_nonce = self
            .snapshot
            .nonces
            .get(&transaction.transaction.from)
            .copied()
            .unwrap_or_default();
        if expected_nonce != transaction.transaction.nonce {
            bail!("unexpected nonce");
        }

        Ok(())
    }

    pub fn apply_transaction(&mut self, transaction: &SignedTransaction) -> Result<()> {
        let from_balance = self
            .snapshot
            .balances
            .get(&transaction.transaction.from)
            .copied()
            .unwrap_or_default();
        if from_balance < transaction.transaction.amount {
            bail!("insufficient balance during apply");
        }

        let to_balance = self
            .snapshot
            .balances
            .get(&transaction.transaction.to)
            .copied()
            .unwrap_or_default();
        self.snapshot.balances.insert(
            transaction.transaction.from.clone(),
            from_balance - transaction.transaction.amount,
        );
        self.snapshot.balances.insert(
            transaction.transaction.to.clone(),
            to_balance + transaction.transaction.amount,
        );

        let nonce = self
            .snapshot
            .nonces
            .get(&transaction.transaction.from)
            .copied()
            .unwrap_or_default();
        self.snapshot
            .nonces
            .insert(transaction.transaction.from.clone(), nonce + 1);
        Ok(())
    }

    pub fn apply_block(&mut self, block: &Block, crypto: &dyn CryptoBackend) -> Result<()> {
        for transaction in &block.transactions {
            self.validate_tx(transaction, crypto)?;
            self.apply_transaction(transaction)?;
        }
        let expected_state_root = self.state_root();
        if expected_state_root != block.header.state_root {
            bail!("state root mismatch");
        }
        self.snapshot.tip_hash = block.block_hash;
        self.snapshot.height = block.header.block_number;
        self.snapshot.last_slot = block.header.slot;
        Ok(())
    }

    pub fn replay_blocks(
        genesis: &GenesisConfig,
        blocks: &[Block],
        crypto: &dyn CryptoBackend,
    ) -> Result<Self> {
        let mut state = Self::from_genesis(genesis);
        for block in blocks {
            state.apply_block(block, crypto)?;
        }
        Ok(state)
    }

    pub fn state_root(&self) -> [u8; 32] {
        entangrid_types::canonical_hash(&(&self.snapshot.balances, &self.snapshot.nonces))
    }

    pub fn block_height(&self) -> u64 {
        self.snapshot.height
    }

    pub fn balance_of(&self, account: &str) -> u64 {
        self.snapshot
            .balances
            .get(account)
            .copied()
            .unwrap_or_default()
    }
}

pub fn parse_snapshot(contents: &str) -> Result<StateSnapshot> {
    serde_json::from_str(contents).map_err(|error| anyhow!("invalid snapshot json: {error}"))
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use entangrid_crypto::{DeterministicCryptoBackend, Signer};
    use entangrid_types::{
        GenesisConfig, PublicIdentity, SignedTransaction, Transaction, ValidatorConfig,
        canonical_hash, empty_hash, now_unix_millis, validator_account,
    };

    use super::*;

    #[test]
    fn applies_signed_transfer() {
        let mut balances = BTreeMap::new();
        balances.insert(validator_account(1), 100);
        balances.insert(validator_account(2), 0);

        let genesis = GenesisConfig {
            chain_id: "test".into(),
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
                    public_identity: PublicIdentity::default(),
                },
                ValidatorConfig {
                    validator_id: 2,
                    stake: 100,
                    address: "127.0.0.1:3002".into(),
                    dev_secret: "secret-2".into(),
                    public_identity: PublicIdentity::default(),
                },
            ],
            initial_balances: balances,
        };
        let crypto = DeterministicCryptoBackend::from_genesis(&genesis);
        let transaction = Transaction {
            from: validator_account(1),
            to: validator_account(2),
            amount: 10,
            nonce: 0,
            memo: None,
        };
        let tx_hash = canonical_hash(&transaction);
        let signed = SignedTransaction {
            transaction,
            signer_id: 1,
            signature: crypto.sign(1, &tx_hash).unwrap(),
            tx_hash,
            submitted_at_unix_millis: now_unix_millis(),
        };

        let mut state = LedgerState::from_genesis(&genesis);
        state.validate_tx(&signed, &crypto).unwrap();
        state.apply_transaction(&signed).unwrap();
        assert_eq!(state.balance_of(&validator_account(1)), 90);
        assert_eq!(state.balance_of(&validator_account(2)), 10);
    }

    #[test]
    fn typed_signature_transaction_validates_with_deterministic_backend() {
        let mut balances = BTreeMap::new();
        balances.insert(validator_account(1), 100);
        balances.insert(validator_account(2), 0);

        let genesis = GenesisConfig {
            chain_id: "test".into(),
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
                    public_identity: PublicIdentity::default(),
                },
                ValidatorConfig {
                    validator_id: 2,
                    stake: 100,
                    address: "127.0.0.1:3002".into(),
                    dev_secret: "secret-2".into(),
                    public_identity: PublicIdentity::default(),
                },
            ],
            initial_balances: balances,
        };
        let crypto = DeterministicCryptoBackend::from_genesis(&genesis);
        let transaction = Transaction {
            from: validator_account(1),
            to: validator_account(2),
            amount: 10,
            nonce: 0,
            memo: None,
        };
        let tx_hash = canonical_hash(&transaction);
        let signed = SignedTransaction {
            transaction,
            signer_id: 1,
            signature: crypto.sign(1, &tx_hash).unwrap(),
            tx_hash,
            submitted_at_unix_millis: now_unix_millis(),
        };

        let state = LedgerState::from_genesis(&genesis);
        state.validate_tx(&signed, &crypto).unwrap();
    }

    #[cfg(feature = "pq-ml-dsa")]
    #[test]
    fn ml_dsa_transaction_validates_with_configured_backend() {
        use entangrid_crypto::build_crypto_backend;
        use entangrid_types::{
            FaultProfile, FeatureFlags, NodeConfig, PublicKeyScheme, SigningBackendKind,
        };
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
        let key_path = std::env::temp_dir().join(format!(
            "entangrid-ledger-ml-dsa-{}.json",
            std::process::id()
        ));
        let key_file = MlDsa65KeyFileFixture {
            signing_key: signing_key.encode().as_slice().to_vec(),
            verifying_key: verifying_key.encode().as_slice().to_vec(),
        };
        std::fs::write(&key_path, serde_json::to_vec(&key_file).unwrap()).unwrap();

        let mut balances = BTreeMap::new();
        balances.insert(validator_account(1), 100);
        balances.insert(validator_account(2), 0);

        let genesis = GenesisConfig {
            chain_id: "test".into(),
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
                    public_identity: PublicIdentity::single(
                        PublicKeyScheme::MlDsa,
                        verifying_key.encode().as_slice().to_vec(),
                    ),
                },
                ValidatorConfig {
                    validator_id: 2,
                    stake: 100,
                    address: "127.0.0.1:3002".into(),
                    dev_secret: "secret-2".into(),
                    public_identity: PublicIdentity::default(),
                },
            ],
            initial_balances: balances,
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
        let crypto = build_crypto_backend(&genesis, &config).unwrap();
        let transaction = Transaction {
            from: validator_account(1),
            to: validator_account(2),
            amount: 10,
            nonce: 0,
            memo: Some("ml-dsa".into()),
        };
        let tx_hash = canonical_hash(&transaction);
        let signed = SignedTransaction {
            transaction,
            signer_id: 1,
            signature: crypto.sign(1, &tx_hash).unwrap(),
            tx_hash,
            submitted_at_unix_millis: now_unix_millis(),
        };

        let state = LedgerState::from_genesis(&genesis);
        state.validate_tx(&signed, crypto.as_ref()).unwrap();
    }

    #[cfg(feature = "pq-ml-dsa")]
    #[test]
    fn hybrid_transaction_validates_with_configured_backend() {
        use entangrid_crypto::build_crypto_backend;
        use entangrid_types::{
            FaultProfile, FeatureFlags, NodeConfig, PublicIdentityComponent, PublicKeyScheme,
            SigningBackendKind,
        };
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
        let key_path = std::env::temp_dir().join(format!(
            "entangrid-ledger-hybrid-{}.json",
            std::process::id()
        ));
        let key_file = MlDsa65KeyFileFixture {
            signing_key: signing_key.encode().as_slice().to_vec(),
            verifying_key: verifying_key.encode().as_slice().to_vec(),
        };
        std::fs::write(&key_path, serde_json::to_vec(&key_file).unwrap()).unwrap();

        let mut balances = BTreeMap::new();
        balances.insert(validator_account(1), 100);
        balances.insert(validator_account(2), 0);

        let genesis = GenesisConfig {
            chain_id: "test".into(),
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
                    public_identity: PublicIdentity::try_hybrid(vec![
                        PublicIdentityComponent {
                            scheme: PublicKeyScheme::DevDeterministic,
                            bytes: format!("validator-{}", 1).into_bytes(),
                        },
                        PublicIdentityComponent {
                            scheme: PublicKeyScheme::MlDsa,
                            bytes: verifying_key.encode().as_slice().to_vec(),
                        },
                    ])
                    .unwrap(),
                },
                ValidatorConfig {
                    validator_id: 2,
                    stake: 100,
                    address: "127.0.0.1:3002".into(),
                    dev_secret: "secret-2".into(),
                    public_identity: PublicIdentity::default(),
                },
            ],
            initial_balances: balances,
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
            signing_key_path: Some(key_path.display().to_string()),
        };
        let crypto = build_crypto_backend(&genesis, &config).unwrap();
        let transaction = Transaction {
            from: validator_account(1),
            to: validator_account(2),
            amount: 10,
            nonce: 0,
            memo: Some("hybrid".into()),
        };
        let tx_hash = canonical_hash(&transaction);
        let signed = SignedTransaction {
            transaction,
            signer_id: 1,
            signature: crypto.sign(1, &tx_hash).unwrap(),
            tx_hash,
            submitted_at_unix_millis: now_unix_millis(),
        };

        assert_eq!(
            signed.signature.scheme(),
            entangrid_types::SignatureScheme::Hybrid
        );
        assert!(
            signed
                .signature
                .component_bytes(entangrid_types::SignatureScheme::DevDeterministic)
                .is_some()
        );
        assert!(
            signed
                .signature
                .component_bytes(entangrid_types::SignatureScheme::MlDsa)
                .is_some()
        );

        let state = LedgerState::from_genesis(&genesis);
        state.validate_tx(&signed, crypto.as_ref()).unwrap();
    }
}
