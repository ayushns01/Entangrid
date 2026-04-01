use std::{
    collections::BTreeMap,
    io::ErrorKind,
    sync::{Arc, Mutex},
    time::Duration,
};

use anyhow::{Context, Result, anyhow};
use entangrid_crypto::CryptoBackend;
use entangrid_types::{
    FaultProfile, HashBytes, NodeMetrics, PeerConfig, ProtocolMessage, SignedEnvelope, ValidatorId,
    canonical_hash,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    sync::{Semaphore, mpsc},
};

const MAX_FRAME_SIZE_BYTES: usize = 8 * 1024 * 1024;
const MAX_CONCURRENT_INBOUND_SESSIONS: usize = 64;
const INBOUND_SESSION_ACQUIRE_TIMEOUT_MILLIS: u64 = 75;
const MAX_CONNECT_RETRIES: u32 = 4;
const CONNECT_RETRY_BACKOFF_MILLIS: u64 = 15;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NetworkFailureKind {
    FaultInjected,
    Transport,
    PeerConfig,
}

#[derive(Clone, Debug)]
pub enum NetworkEvent {
    Received {
        from_validator_id: ValidatorId,
        payload: ProtocolMessage,
        bytes: usize,
    },
    SessionObserved {
        peer_validator_id: ValidatorId,
        transcript_hash: HashBytes,
        outbound: bool,
        service_accountable: bool,
    },
    SessionFailed {
        peer_validator_id: Option<ValidatorId>,
        detail: String,
        outbound: bool,
        service_accountable: bool,
        kind: NetworkFailureKind,
    },
    InboundSessionDropped {
        detail: String,
    },
}

#[derive(Clone)]
pub struct NetworkHandle {
    outbound_by_peer: Arc<BTreeMap<ValidatorId, mpsc::UnboundedSender<OutboundRequest>>>,
}

#[derive(Clone, Debug)]
struct OutboundRequest {
    peer: PeerConfig,
    payload: ProtocolMessage,
    service_accountable: bool,
}

impl NetworkHandle {
    pub fn broadcast(&self, peers: &[PeerConfig], payload: ProtocolMessage) -> Result<()> {
        for peer in peers {
            self.send_to(peer.clone(), payload.clone())?;
        }
        Ok(())
    }

    pub fn send_to(&self, peer: PeerConfig, payload: ProtocolMessage) -> Result<()> {
        self.outbound_sender_for(peer.validator_id)?
            .send(OutboundRequest {
                peer,
                payload,
                service_accountable: true,
            })
            .map_err(|_| anyhow!("network outbound worker is closed"))
    }

    pub fn broadcast_control(&self, peers: &[PeerConfig], payload: ProtocolMessage) -> Result<()> {
        for peer in peers {
            self.send_control_to(peer.clone(), payload.clone())?;
        }
        Ok(())
    }

    pub fn send_control_to(&self, peer: PeerConfig, payload: ProtocolMessage) -> Result<()> {
        self.outbound_sender_for(peer.validator_id)?
            .send(OutboundRequest {
                peer,
                payload,
                service_accountable: false,
            })
            .map_err(|_| anyhow!("network outbound worker is closed"))
    }

    fn outbound_sender_for(
        &self,
        validator_id: ValidatorId,
    ) -> Result<&mpsc::UnboundedSender<OutboundRequest>> {
        self.outbound_by_peer
            .get(&validator_id)
            .ok_or_else(|| anyhow!("unknown peer {validator_id}"))
    }
}

pub async fn spawn_network(
    local_validator_id: ValidatorId,
    listen_address: String,
    peers: Vec<PeerConfig>,
    fault_profile: FaultProfile,
    crypto: Arc<dyn CryptoBackend>,
    metrics: Arc<Mutex<NodeMetrics>>,
    event_tx: mpsc::UnboundedSender<NetworkEvent>,
) -> Result<NetworkHandle> {
    let listener = TcpListener::bind(&listen_address)
        .await
        .with_context(|| format!("failed to bind to {listen_address}"))?;
    let peers_by_id: Arc<BTreeMap<ValidatorId, PeerConfig>> = Arc::new(
        peers
            .into_iter()
            .map(|peer| (peer.validator_id, peer))
            .collect(),
    );

    let listener_crypto = Arc::clone(&crypto);
    let listener_metrics = Arc::clone(&metrics);
    let listener_events = event_tx.clone();
    let inbound_limit = Arc::new(Semaphore::new(MAX_CONCURRENT_INBOUND_SESSIONS));
    let mut outbound_by_peer = BTreeMap::new();

    tokio::spawn(async move {
        loop {
            match listener.accept().await {
                Ok((stream, _)) => {
                    let crypto = Arc::clone(&listener_crypto);
                    let metrics = Arc::clone(&listener_metrics);
                    let event_tx = listener_events.clone();
                    let inbound_limit = Arc::clone(&inbound_limit);
                    let permit = match tokio::time::timeout(
                        Duration::from_millis(INBOUND_SESSION_ACQUIRE_TIMEOUT_MILLIS),
                        inbound_limit.acquire_owned(),
                    )
                    .await
                    {
                        Ok(Ok(permit)) => permit,
                        Ok(Err(_)) | Err(_) => {
                            update_metrics(&metrics, |metrics| {
                                metrics.inbound_session_drops += 1;
                            });
                            let _ = event_tx.send(NetworkEvent::InboundSessionDropped {
                                detail: format!(
                                    "inbound session capacity {} unavailable for {}ms",
                                    MAX_CONCURRENT_INBOUND_SESSIONS,
                                    INBOUND_SESSION_ACQUIRE_TIMEOUT_MILLIS
                                ),
                            });
                            continue;
                        }
                    };
                    tokio::spawn(async move {
                        let _permit = permit;
                        if let Err(error) = handle_inbound(
                            stream,
                            local_validator_id,
                            crypto,
                            metrics,
                            event_tx.clone(),
                        )
                        .await
                        {
                            let _ = event_tx.send(NetworkEvent::SessionFailed {
                                peer_validator_id: None,
                                detail: error.to_string(),
                                outbound: false,
                                service_accountable: false,
                                kind: NetworkFailureKind::Transport,
                            });
                        }
                    });
                }
                Err(error) => {
                    let _ = listener_events.send(NetworkEvent::SessionFailed {
                        peer_validator_id: None,
                        detail: error.to_string(),
                        outbound: false,
                        service_accountable: false,
                        kind: NetworkFailureKind::Transport,
                    });
                }
            }
        }
    });

    for peer in peers_by_id.values() {
        let peer_id = peer.validator_id;
        let (peer_tx, mut peer_rx) = mpsc::unbounded_channel::<OutboundRequest>();
        outbound_by_peer.insert(peer_id, peer_tx);
        let crypto = Arc::clone(&crypto);
        let metrics = Arc::clone(&metrics);
        let event_tx = event_tx.clone();
        let fault_profile = fault_profile.clone();
        tokio::spawn(async move {
            let mut stream = None;
            while let Some(request) = peer_rx.recv().await {
                let service_accountable = request.service_accountable;
                if let Err(error) = send_message(
                    local_validator_id,
                    request,
                    fault_profile.clone(),
                    Arc::clone(&crypto),
                    Arc::clone(&metrics),
                    event_tx.clone(),
                    &mut stream,
                )
                .await
                {
                    let _ = event_tx.send(NetworkEvent::SessionFailed {
                        peer_validator_id: Some(peer_id),
                        detail: error.to_string(),
                        outbound: true,
                        service_accountable,
                        kind: NetworkFailureKind::Transport,
                    });
                }
            }
        });
    }

    Ok(NetworkHandle {
        outbound_by_peer: Arc::new(outbound_by_peer),
    })
}

async fn send_message(
    local_validator_id: ValidatorId,
    request: OutboundRequest,
    fault_profile: FaultProfile,
    crypto: Arc<dyn CryptoBackend>,
    metrics: Arc<Mutex<NodeMetrics>>,
    event_tx: mpsc::UnboundedSender<NetworkEvent>,
    stream: &mut Option<TcpStream>,
) -> Result<()> {
    if fault_profile.disable_outbound {
        let _ = event_tx.send(NetworkEvent::SessionFailed {
            peer_validator_id: Some(request.peer.validator_id),
            detail: "outbound disabled by fault profile".into(),
            outbound: true,
            service_accountable: request.service_accountable,
            kind: NetworkFailureKind::FaultInjected,
        });
        return Ok(());
    }

    let message_hash = canonical_hash(&request.payload);
    if should_drop(&fault_profile, request.peer.validator_id, &message_hash) {
        let _ = event_tx.send(NetworkEvent::SessionFailed {
            peer_validator_id: Some(request.peer.validator_id),
            detail: "outbound message dropped by fault profile".into(),
            outbound: true,
            service_accountable: request.service_accountable,
            kind: NetworkFailureKind::FaultInjected,
        });
        return Ok(());
    }

    update_metrics(&metrics, |metrics| {
        metrics.handshake_attempts += 1;
    });

    if fault_profile.artificial_delay_ms > 0 {
        tokio::time::sleep(Duration::from_millis(fault_profile.artificial_delay_ms)).await;
    }

    let session =
        crypto.open_session(local_validator_id, request.peer.validator_id, &message_hash)?;
    let signature = crypto.sign(local_validator_id, &message_hash)?;
    let envelope = SignedEnvelope {
        from_validator_id: local_validator_id,
        message_hash,
        signature,
        payload: request.payload,
    };

    let bytes = bincode::serde::encode_to_vec(&envelope, bincode::config::standard())?;
    let mut retried = false;
    loop {
        if stream.is_none() {
            let connected = connect_with_retries(&request.peer.address).await?;
            connected.set_nodelay(true)?;
            *stream = Some(connected);
        }

        match write_frame(stream.as_mut().expect("stream populated"), &bytes).await {
            Ok(()) => break,
            Err(error) if !retried && is_retryable_stream_error(&error) => {
                *stream = None;
                retried = true;
            }
            Err(error) => {
                *stream = None;
                return Err(error.into());
            }
        }
    }

    let _ = event_tx.send(NetworkEvent::SessionObserved {
        peer_validator_id: request.peer.validator_id,
        transcript_hash: session.transcript_hash,
        outbound: true,
        service_accountable: request.service_accountable,
    });

    update_metrics(&metrics, |metrics| {
        metrics.bytes_sent += bytes.len() as u64;
        metrics.active_sessions = 0;
    });
    Ok(())
}

async fn handle_inbound(
    mut stream: TcpStream,
    local_validator_id: ValidatorId,
    crypto: Arc<dyn CryptoBackend>,
    metrics: Arc<Mutex<NodeMetrics>>,
    event_tx: mpsc::UnboundedSender<NetworkEvent>,
) -> Result<()> {
    loop {
        let frame_len = match stream.read_u32().await {
            Ok(length) => length as usize,
            Err(error) if error.kind() == ErrorKind::UnexpectedEof => return Ok(()),
            Err(error) => return Err(error.into()),
        };
        if frame_len > MAX_FRAME_SIZE_BYTES {
            update_metrics(&metrics, |metrics| {
                metrics.handshake_failures += 1;
                metrics.active_sessions = 0;
            });
            return Err(anyhow!(
                "frame length {frame_len} exceeds max {}",
                MAX_FRAME_SIZE_BYTES
            ));
        }
        let mut frame = vec![0u8; frame_len];
        stream.read_exact(&mut frame).await?;

        let (envelope, _): (SignedEnvelope, usize) =
            bincode::serde::decode_from_slice(&frame, bincode::config::standard())?;
        update_metrics(&metrics, |metrics| {
            metrics.bytes_received += frame_len as u64;
            metrics.handshake_attempts += 1;
            metrics.active_sessions = 1;
        });

        let expected_hash = canonical_hash(&envelope.payload);
        if expected_hash != envelope.message_hash {
            update_metrics(&metrics, |metrics| {
                metrics.handshake_failures += 1;
                metrics.active_sessions = 0;
            });
            return Err(anyhow!("payload hash mismatch"));
        }

        let session = crypto.open_session(
            local_validator_id,
            envelope.from_validator_id,
            &envelope.message_hash,
        )?;
        let _ = event_tx.send(NetworkEvent::SessionObserved {
            peer_validator_id: envelope.from_validator_id,
            transcript_hash: session.transcript_hash,
            outbound: false,
            service_accountable: true,
        });

        let verified = crypto.verify(
            envelope.from_validator_id,
            &envelope.message_hash,
            &envelope.signature,
        )?;
        update_metrics(&metrics, |metrics| {
            if !verified {
                metrics.handshake_failures += 1;
            }
            metrics.active_sessions = 0;
        });
        if !verified {
            return Err(anyhow!("invalid envelope signature"));
        }

        let _ = event_tx.send(NetworkEvent::Received {
            from_validator_id: envelope.from_validator_id,
            payload: envelope.payload,
            bytes: frame_len,
        });
    }
}

fn should_drop(
    fault_profile: &FaultProfile,
    peer_validator_id: ValidatorId,
    message_hash: &HashBytes,
) -> bool {
    if fault_profile.outbound_drop_probability <= 0.0 {
        return false;
    }
    let mut sample = [0u8; 8];
    sample.copy_from_slice(&message_hash[..8]);
    let randomish = u64::from_le_bytes(sample) ^ peer_validator_id;
    let fraction = (randomish as f64 / u64::MAX as f64).clamp(0.0, 1.0);
    fraction < fault_profile.outbound_drop_probability
}

async fn connect_with_retries(address: &str) -> Result<TcpStream> {
    let mut last_error = None;
    for attempt in 0..=MAX_CONNECT_RETRIES {
        match TcpStream::connect(address).await {
            Ok(stream) => return Ok(stream),
            Err(error) => {
                let retryable = is_retryable_connect_error(&error);
                last_error = Some(error);
                if !retryable || attempt == MAX_CONNECT_RETRIES {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(
                    CONNECT_RETRY_BACKOFF_MILLIS * u64::from(attempt + 1),
                ))
                .await;
            }
        }
    }
    Err(last_error.expect("at least one connection attempt"))
        .with_context(|| format!("failed to connect to {address}"))
}

fn is_retryable_connect_error(error: &std::io::Error) -> bool {
    matches!(
        error.kind(),
        ErrorKind::ConnectionRefused
            | ErrorKind::ConnectionReset
            | ErrorKind::ConnectionAborted
            | ErrorKind::TimedOut
            | ErrorKind::AddrNotAvailable
            | ErrorKind::Interrupted
    )
}

fn is_retryable_stream_error(error: &std::io::Error) -> bool {
    matches!(
        error.kind(),
        ErrorKind::BrokenPipe
            | ErrorKind::ConnectionReset
            | ErrorKind::ConnectionAborted
            | ErrorKind::NotConnected
            | ErrorKind::UnexpectedEof
            | ErrorKind::TimedOut
            | ErrorKind::Interrupted
    )
}

async fn write_frame(stream: &mut TcpStream, bytes: &[u8]) -> Result<(), std::io::Error> {
    stream.write_u32(bytes.len() as u32).await?;
    stream.write_all(bytes).await?;
    stream.flush().await
}

fn update_metrics(metrics: &Arc<Mutex<NodeMetrics>>, update: impl FnOnce(&mut NodeMetrics)) {
    if let Ok(mut metrics) = metrics.lock() {
        update(&mut metrics);
        metrics.last_updated_unix_millis = entangrid_types::now_unix_millis();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use entangrid_crypto::{DeterministicCryptoBackend, Signer};
    use entangrid_types::HeartbeatPulse;
    use entangrid_types::{GenesisConfig, PublicIdentity, ValidatorConfig};
    use std::io::Error;
    use tokio::{
        net::TcpListener,
        sync::mpsc,
        time::{Duration, timeout},
    };

    fn sample_validators() -> Vec<ValidatorConfig> {
        vec![
            ValidatorConfig {
                validator_id: 1,
                stake: 100,
                address: "127.0.0.1:4100".into(),
                dev_secret: "secret-1".into(),
                public_identity: PublicIdentity::default(),
            },
            ValidatorConfig {
                validator_id: 2,
                stake: 100,
                address: "127.0.0.1:4101".into(),
                dev_secret: "secret-2".into(),
                public_identity: PublicIdentity::default(),
            },
        ]
    }

    fn sample_genesis() -> GenesisConfig {
        GenesisConfig {
            chain_id: "entangrid-network-test".into(),
            epoch_seed: [0u8; 32],
            genesis_time_unix_millis: 0,
            slot_duration_millis: 1_000,
            slots_per_epoch: 6,
            max_txs_per_block: 16,
            witness_count: 1,
            validators: sample_validators(),
            initial_balances: BTreeMap::new(),
        }
    }

    async fn reserve_local_address() -> String {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap().to_string();
        drop(listener);
        address
    }

    #[test]
    fn rejects_oversized_inbound_frame() {
        assert!(MAX_FRAME_SIZE_BYTES < usize::MAX);
        let oversized = MAX_FRAME_SIZE_BYTES + 1;
        assert!(oversized > MAX_FRAME_SIZE_BYTES);
    }

    #[test]
    fn classifies_retryable_connect_errors() {
        assert!(is_retryable_connect_error(&Error::from(
            ErrorKind::ConnectionRefused
        )));
        assert!(is_retryable_connect_error(&Error::from(
            ErrorKind::TimedOut
        )));
        assert!(!is_retryable_connect_error(&Error::from(
            ErrorKind::PermissionDenied
        )));
    }

    #[test]
    fn send_to_rejects_unknown_peer_without_configured_lane() {
        let handle = NetworkHandle {
            outbound_by_peer: Arc::new(BTreeMap::new()),
        };
        let error = handle
            .send_to(
                PeerConfig {
                    validator_id: 9,
                    address: "127.0.0.1:9999".into(),
                },
                ProtocolMessage::HeartbeatPulse(HeartbeatPulse {
                    epoch: 0,
                    slot: 0,
                    source_validator_id: 1,
                    sequence_number: 0,
                    emitted_at_unix_millis: 0,
                }),
            )
            .unwrap_err();
        assert!(error.to_string().contains("unknown peer"));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn send_to_routes_messages_to_configured_peer_lane() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let handle = NetworkHandle {
            outbound_by_peer: Arc::new(BTreeMap::from([(2, tx)])),
        };
        let payload = ProtocolMessage::HeartbeatPulse(HeartbeatPulse {
            epoch: 0,
            slot: 4,
            source_validator_id: 1,
            sequence_number: 4,
            emitted_at_unix_millis: 123,
        });

        handle
            .send_to(
                PeerConfig {
                    validator_id: 2,
                    address: "127.0.0.1:4002".into(),
                },
                payload.clone(),
            )
            .unwrap();

        let queued = rx.recv().await.expect("request queued");
        assert_eq!(queued.peer.validator_id, 2);
        assert_eq!(queued.payload, payload);
        assert!(queued.service_accountable);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn outbound_lane_reuses_same_connection_for_same_peer() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let peer_address = listener.local_addr().unwrap().to_string();
        let local_address = reserve_local_address().await;
        let validators = sample_validators();
        let crypto: Arc<dyn CryptoBackend> =
            Arc::new(DeterministicCryptoBackend::from_validators(&validators));
        let metrics = Arc::new(Mutex::new(NodeMetrics {
            validator_id: 1,
            ..NodeMetrics::default()
        }));
        let (event_tx, _event_rx) = mpsc::unbounded_channel();
        let network = spawn_network(
            1,
            local_address,
            vec![PeerConfig {
                validator_id: 2,
                address: peer_address.clone(),
            }],
            FaultProfile::default(),
            Arc::clone(&crypto),
            metrics,
            event_tx,
        )
        .await
        .unwrap();

        let payload_one = ProtocolMessage::HeartbeatPulse(HeartbeatPulse {
            epoch: 0,
            slot: 1,
            source_validator_id: 1,
            sequence_number: 1,
            emitted_at_unix_millis: 1,
        });
        let payload_two = ProtocolMessage::HeartbeatPulse(HeartbeatPulse {
            epoch: 0,
            slot: 2,
            source_validator_id: 1,
            sequence_number: 2,
            emitted_at_unix_millis: 2,
        });
        network
            .send_to(
                PeerConfig {
                    validator_id: 2,
                    address: peer_address.clone(),
                },
                payload_one.clone(),
            )
            .unwrap();
        network
            .send_to(
                PeerConfig {
                    validator_id: 2,
                    address: peer_address.clone(),
                },
                payload_two.clone(),
            )
            .unwrap();

        let (mut stream, _) = timeout(Duration::from_secs(2), listener.accept())
            .await
            .expect("accepted in time")
            .unwrap();
        let first_len = stream.read_u32().await.unwrap() as usize;
        let mut first_frame = vec![0u8; first_len];
        stream.read_exact(&mut first_frame).await.unwrap();
        let (first, _): (SignedEnvelope, usize) =
            bincode::serde::decode_from_slice(&first_frame, bincode::config::standard()).unwrap();
        assert_eq!(first.payload, payload_one);

        let second_len = timeout(Duration::from_secs(2), stream.read_u32())
            .await
            .expect("second frame on same stream")
            .unwrap() as usize;
        let mut second_frame = vec![0u8; second_len];
        stream.read_exact(&mut second_frame).await.unwrap();
        let (second, _): (SignedEnvelope, usize) =
            bincode::serde::decode_from_slice(&second_frame, bincode::config::standard()).unwrap();
        assert_eq!(second.payload, payload_two);
        assert!(
            timeout(Duration::from_millis(200), listener.accept())
                .await
                .is_err()
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn inbound_session_accepts_multiple_frames_from_same_stream() {
        let listen_address = reserve_local_address().await;
        let genesis = sample_genesis();
        let crypto: Arc<dyn CryptoBackend> =
            Arc::new(DeterministicCryptoBackend::from_genesis(&genesis));
        let metrics = Arc::new(Mutex::new(NodeMetrics {
            validator_id: 2,
            ..NodeMetrics::default()
        }));
        let (event_tx, mut event_rx) = mpsc::unbounded_channel();
        let _network = spawn_network(
            2,
            listen_address.clone(),
            vec![PeerConfig {
                validator_id: 1,
                address: reserve_local_address().await,
            }],
            FaultProfile::default(),
            Arc::clone(&crypto),
            metrics,
            event_tx,
        )
        .await
        .unwrap();

        let client_crypto = DeterministicCryptoBackend::from_genesis(&genesis);
        let mut stream = TcpStream::connect(&listen_address).await.unwrap();
        for slot in [1u64, 2u64] {
            let payload = ProtocolMessage::HeartbeatPulse(HeartbeatPulse {
                epoch: 0,
                slot,
                source_validator_id: 1,
                sequence_number: slot,
                emitted_at_unix_millis: slot,
            });
            let message_hash = canonical_hash(&payload);
            let envelope = SignedEnvelope {
                from_validator_id: 1,
                message_hash,
                signature: client_crypto.sign(1, &message_hash).unwrap(),
                payload,
            };
            let bytes =
                bincode::serde::encode_to_vec(&envelope, bincode::config::standard()).unwrap();
            stream.write_u32(bytes.len() as u32).await.unwrap();
            stream.write_all(&bytes).await.unwrap();
            stream.flush().await.unwrap();
        }

        let mut received = Vec::new();
        while received.len() < 2 {
            let event = timeout(Duration::from_secs(2), event_rx.recv())
                .await
                .expect("event in time")
                .expect("event received");
            if let NetworkEvent::Received { payload, .. } = event {
                received.push(payload);
            }
        }
        assert_eq!(
            received,
            vec![
                ProtocolMessage::HeartbeatPulse(HeartbeatPulse {
                    epoch: 0,
                    slot: 1,
                    source_validator_id: 1,
                    sequence_number: 1,
                    emitted_at_unix_millis: 1,
                }),
                ProtocolMessage::HeartbeatPulse(HeartbeatPulse {
                    epoch: 0,
                    slot: 2,
                    source_validator_id: 1,
                    sequence_number: 2,
                    emitted_at_unix_millis: 2,
                }),
            ]
        );
    }
}
