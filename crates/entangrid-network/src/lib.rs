use std::{
    collections::BTreeMap,
    io::ErrorKind,
    sync::{Arc, Mutex},
    time::Duration,
};

use anyhow::{Context, Result, anyhow};
use entangrid_crypto::{
    CryptoBackend, FrameDirection, SessionMaterial, decrypt_frame_payload, encrypt_frame_payload,
};
use entangrid_types::{
    FaultProfile, HashBytes, NodeMetrics, PeerConfig, ProtocolMessage, SessionClientHello,
    SessionServerHello, SignedEnvelope, ValidatorId, canonical_hash, now_unix_millis,
};
use serde::{Serialize, de::DeserializeOwned};
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

struct EstablishedOutboundStream {
    stream: TcpStream,
    session: SessionMaterial,
    send_counter: u64,
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

fn session_nonce(local_validator_id: ValidatorId, peer_validator_id: ValidatorId) -> HashBytes {
    canonical_hash(&(
        "entangrid-network-session",
        local_validator_id,
        peer_validator_id,
        now_unix_millis(),
    ))
}

async fn read_frame_bytes(stream: &mut TcpStream) -> Result<Option<Vec<u8>>> {
    let frame_len = match stream.read_u32().await {
        Ok(length) => length as usize,
        Err(error) if error.kind() == ErrorKind::UnexpectedEof => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    if frame_len > MAX_FRAME_SIZE_BYTES {
        return Err(anyhow!(
            "frame length {frame_len} exceeds max {}",
            MAX_FRAME_SIZE_BYTES
        ));
    }
    let mut frame = vec![0u8; frame_len];
    stream.read_exact(&mut frame).await?;
    Ok(Some(frame))
}

fn decode_frame<T: DeserializeOwned>(bytes: &[u8], frame_kind: &str) -> Result<T> {
    bincode::serde::decode_from_slice(bytes, bincode::config::standard())
        .map(|(decoded, _)| decoded)
        .map_err(|error| anyhow!("failed to decode {frame_kind}: {error}"))
}

async fn write_bincode_frame<T: Serialize>(stream: &mut TcpStream, value: &T) -> Result<()> {
    let bytes = bincode::serde::encode_to_vec(value, bincode::config::standard())?;
    write_frame(stream, &bytes).await?;
    Ok(())
}

async fn write_application_frame(
    stream: &mut TcpStream,
    session: &SessionMaterial,
    send_counter: &mut u64,
    plaintext: &[u8],
) -> Result<usize> {
    let frame = if session.encrypt_frames {
        let counter = *send_counter;
        let ciphertext =
            encrypt_frame_payload(session, FrameDirection::Outbound, counter, plaintext)?;
        *send_counter = (*send_counter).saturating_add(1);
        ciphertext
    } else {
        plaintext.to_vec()
    };
    let frame_len = frame.len();
    write_frame(stream, &frame).await?;
    Ok(frame_len)
}

async fn read_application_frame(
    stream: &mut TcpStream,
    session: &SessionMaterial,
    receive_counter: &mut u64,
) -> Result<Option<(Vec<u8>, usize)>> {
    let frame = match read_frame_bytes(stream).await? {
        Some(frame) => frame,
        None => return Ok(None),
    };
    let wire_len = frame.len();
    if session.encrypt_frames {
        let counter = *receive_counter;
        let plaintext =
            decrypt_frame_payload(session, FrameDirection::Outbound, counter, &frame)
                .map_err(|error| anyhow!("encrypted frame authentication failed: {error}"))?;
        *receive_counter = (*receive_counter).saturating_add(1);
        Ok(Some((plaintext, wire_len)))
    } else {
        Ok(Some((frame, wire_len)))
    }
}

async fn run_outbound_handshake(
    stream: &mut TcpStream,
    local_validator_id: ValidatorId,
    peer_validator_id: ValidatorId,
    crypto: Arc<dyn CryptoBackend>,
) -> Result<SessionMaterial> {
    let client_hello = crypto.build_client_hello(
        local_validator_id,
        peer_validator_id,
        session_nonce(local_validator_id, peer_validator_id),
    )?;
    write_bincode_frame(stream, &client_hello).await?;
    let server_hello_bytes = read_frame_bytes(stream)
        .await?
        .ok_or_else(|| anyhow!("expected server hello before stream closed"))?;
    let server_hello: SessionServerHello = decode_frame(&server_hello_bytes, "server hello")?;
    crypto.finalize_client_session(local_validator_id, &client_hello, &server_hello)
}

async fn accept_inbound_handshake(
    stream: &mut TcpStream,
    local_validator_id: ValidatorId,
    crypto: Arc<dyn CryptoBackend>,
) -> Result<(ValidatorId, SessionMaterial)> {
    let client_hello_bytes = read_frame_bytes(stream)
        .await?
        .ok_or_else(|| anyhow!("expected client hello before stream closed"))?;
    let client_hello: SessionClientHello = decode_frame(&client_hello_bytes, "client hello")?;
    let peer_validator_id = client_hello.initiator_validator_id;
    let (server_hello, session) = crypto.accept_client_hello(local_validator_id, &client_hello)?;
    write_bincode_frame(stream, &server_hello).await?;
    Ok((peer_validator_id, session))
}

async fn send_message(
    local_validator_id: ValidatorId,
    request: OutboundRequest,
    fault_profile: FaultProfile,
    crypto: Arc<dyn CryptoBackend>,
    metrics: Arc<Mutex<NodeMetrics>>,
    event_tx: mpsc::UnboundedSender<NetworkEvent>,
    stream: &mut Option<EstablishedOutboundStream>,
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

    if fault_profile.artificial_delay_ms > 0 {
        tokio::time::sleep(Duration::from_millis(fault_profile.artificial_delay_ms)).await;
    }
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
            let mut connected = connect_with_retries(&request.peer.address).await?;
            connected.set_nodelay(true)?;
            update_metrics(&metrics, |metrics| {
                metrics.handshake_attempts += 1;
            });
            let session = match run_outbound_handshake(
                &mut connected,
                local_validator_id,
                request.peer.validator_id,
                Arc::clone(&crypto),
            )
            .await
            {
                Ok(session) => session,
                Err(error) => {
                    update_metrics(&metrics, |metrics| {
                        metrics.handshake_failures += 1;
                        metrics.active_sessions = 0;
                    });
                    return Err(error);
                }
            };
            update_metrics(&metrics, |metrics| {
                metrics.active_sessions = 1;
            });
            *stream = Some(EstablishedOutboundStream {
                stream: connected,
                session,
                send_counter: 0,
            });
        }

        let established = stream.as_mut().expect("stream populated");
        match write_application_frame(
            &mut established.stream,
            &established.session,
            &mut established.send_counter,
            &bytes,
        )
        .await
        {
            Ok(wire_len) => {
                update_metrics(&metrics, |metrics| {
                    metrics.bytes_sent += wire_len as u64;
                });
                break;
            }
            Err(error) if !retried && is_retryable_transport_error(&error) => {
                update_metrics(&metrics, |metrics| {
                    metrics.active_sessions = 0;
                });
                *stream = None;
                retried = true;
            }
            Err(error) => {
                update_metrics(&metrics, |metrics| {
                    metrics.active_sessions = 0;
                });
                *stream = None;
                return Err(error.into());
            }
        }
    }

    let session = &stream.as_ref().expect("stream retained").session;

    let _ = event_tx.send(NetworkEvent::SessionObserved {
        peer_validator_id: request.peer.validator_id,
        transcript_hash: session.transcript_hash,
        outbound: true,
        service_accountable: request.service_accountable,
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
    update_metrics(&metrics, |metrics| {
        metrics.handshake_attempts += 1;
    });
    let (peer_validator_id, session) = match accept_inbound_handshake(
        &mut stream,
        local_validator_id,
        Arc::clone(&crypto),
    )
    .await
    {
        Ok(handshake) => {
            update_metrics(&metrics, |metrics| {
                metrics.active_sessions = 1;
            });
            handshake
        }
        Err(error) => {
            update_metrics(&metrics, |metrics| {
                metrics.handshake_failures += 1;
                metrics.active_sessions = 0;
            });
            return Err(error);
        }
    };

    let mut receive_counter = 0u64;
    loop {
        let (frame, wire_len) =
            match read_application_frame(&mut stream, &session, &mut receive_counter).await {
                Ok(Some(frame)) => frame,
                Ok(None) => {
                    update_metrics(&metrics, |metrics| {
                        metrics.active_sessions = 0;
                    });
                    return Ok(());
                }
                Err(error) => {
                    update_metrics(&metrics, |metrics| {
                        metrics.handshake_failures += 1;
                        metrics.active_sessions = 0;
                    });
                    return Err(error);
                }
            };
        let envelope: SignedEnvelope = decode_frame(&frame, "signed envelope")?;
        update_metrics(&metrics, |metrics| {
            metrics.bytes_received += wire_len as u64;
        });

        let expected_hash = canonical_hash(&envelope.payload);
        if expected_hash != envelope.message_hash {
            update_metrics(&metrics, |metrics| {
                metrics.handshake_failures += 1;
                metrics.active_sessions = 0;
            });
            return Err(anyhow!("payload hash mismatch"));
        }
        if envelope.from_validator_id != peer_validator_id {
            update_metrics(&metrics, |metrics| {
                metrics.handshake_failures += 1;
                metrics.active_sessions = 0;
            });
            return Err(anyhow!(
                "envelope validator {} does not match established peer {}",
                envelope.from_validator_id,
                peer_validator_id
            ));
        }

        let _ = event_tx.send(NetworkEvent::SessionObserved {
            peer_validator_id,
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
            bytes: frame.len(),
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

fn is_retryable_transport_error(error: &anyhow::Error) -> bool {
    error
        .downcast_ref::<std::io::Error>()
        .map(is_retryable_stream_error)
        .unwrap_or(false)
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
    use entangrid_crypto::{DeterministicCryptoBackend, HandshakeProvider, Signer};
    #[cfg(feature = "pq-ml-kem")]
    use entangrid_crypto::{FrameDirection, build_crypto_backend, encrypt_frame_payload};
    use entangrid_types::HeartbeatPulse;
    #[cfg(feature = "pq-ml-kem")]
    use entangrid_types::{
        FeatureFlags, NodeConfig, SessionBackendKind, SessionKeyScheme, SessionPublicIdentity,
        SessionPublicIdentityComponent, SigningBackendKind,
    };
    use entangrid_types::{
        GenesisConfig, PublicIdentity, SessionClientHello, SessionServerHello, ValidatorConfig,
    };
    #[cfg(feature = "pq-ml-kem")]
    use ml_kem::{EncodedSizeUser, KemCore, MlKem768};
    #[cfg(feature = "pq-ml-kem")]
    use rand_core::OsRng;
    #[cfg(feature = "pq-ml-kem")]
    use serde::Serialize;
    use std::io::Error;
    use std::os::fd::AsRawFd;
    #[cfg(feature = "pq-ml-kem")]
    use std::time::{SystemTime, UNIX_EPOCH};
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
                session_public_identity: None,
            },
            ValidatorConfig {
                validator_id: 2,
                stake: 100,
                address: "127.0.0.1:4101".into(),
                dev_secret: "secret-2".into(),
                public_identity: PublicIdentity::default(),
                session_public_identity: None,
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

    async fn read_bincode_frame<T: serde::de::DeserializeOwned>(stream: &mut TcpStream) -> T {
        let frame_len = stream.read_u32().await.unwrap() as usize;
        let mut frame = vec![0u8; frame_len];
        stream.read_exact(&mut frame).await.unwrap();
        let (decoded, _): (T, usize) =
            bincode::serde::decode_from_slice(&frame, bincode::config::standard()).unwrap();
        decoded
    }

    async fn read_raw_frame(stream: &mut TcpStream) -> Vec<u8> {
        let frame_len = stream.read_u32().await.unwrap() as usize;
        let mut frame = vec![0u8; frame_len];
        stream.read_exact(&mut frame).await.unwrap();
        frame
    }

    async fn write_bincode_frame<T: serde::Serialize>(stream: &mut TcpStream, value: &T) {
        let bytes = bincode::serde::encode_to_vec(value, bincode::config::standard()).unwrap();
        stream.write_u32(bytes.len() as u32).await.unwrap();
        stream.write_all(&bytes).await.unwrap();
        stream.flush().await.unwrap();
    }

    async fn read_application_bincode_frame<T: serde::de::DeserializeOwned>(
        stream: &mut TcpStream,
        session: &SessionMaterial,
        receive_counter: &mut u64,
    ) -> T {
        let (frame, _) = read_application_frame(stream, session, receive_counter)
            .await
            .unwrap()
            .expect("application frame present");
        let (decoded, _): (T, usize) =
            bincode::serde::decode_from_slice(&frame, bincode::config::standard()).unwrap();
        decoded
    }

    fn abort_tcp_stream(stream: std::net::TcpStream) {
        let linger = libc::linger {
            l_onoff: 1,
            l_linger: 0,
        };
        let rc = unsafe {
            libc::setsockopt(
                stream.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_LINGER,
                &linger as *const libc::linger as *const libc::c_void,
                std::mem::size_of::<libc::linger>() as libc::socklen_t,
            )
        };
        assert_eq!(rc, 0, "failed to set SO_LINGER=0 for reconnect test");
        drop(stream);
    }

    #[cfg(feature = "pq-ml-kem")]
    #[derive(Serialize)]
    struct TestMlKemSessionKeyFile {
        decapsulation_key: Vec<u8>,
        encapsulation_key: Vec<u8>,
    }

    #[cfg(feature = "pq-ml-kem")]
    fn hybrid_session_public_identity(encapsulation_key: Vec<u8>) -> SessionPublicIdentity {
        SessionPublicIdentity::try_hybrid(vec![
            SessionPublicIdentityComponent {
                scheme: SessionKeyScheme::DevDeterministic,
                bytes: b"session-validator".to_vec(),
            },
            SessionPublicIdentityComponent {
                scheme: SessionKeyScheme::MlKem,
                bytes: encapsulation_key,
            },
        ])
        .unwrap()
    }

    #[cfg(feature = "pq-ml-kem")]
    fn write_ml_kem_session_key_file(label: &str) -> (std::path::PathBuf, Vec<u8>) {
        let mut rng = OsRng;
        let (decapsulation_key, encapsulation_key) = MlKem768::generate(&mut rng);
        let decapsulation_key_bytes = decapsulation_key.as_bytes().as_slice().to_vec();
        let encapsulation_key_bytes = encapsulation_key.as_bytes().as_slice().to_vec();
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let key_path = std::env::temp_dir().join(format!(
            "entangrid-network-{label}-{}-{}.key",
            std::process::id(),
            nonce
        ));
        let key_file = TestMlKemSessionKeyFile {
            decapsulation_key: decapsulation_key_bytes,
            encapsulation_key: encapsulation_key_bytes.clone(),
        };
        std::fs::write(&key_path, serde_json::to_vec(&key_file).unwrap()).unwrap();
        (key_path, encapsulation_key_bytes)
    }

    #[cfg(feature = "pq-ml-kem")]
    fn hybrid_session_genesis_and_configs() -> (GenesisConfig, NodeConfig, NodeConfig) {
        let (key_path_one, key_one) = write_ml_kem_session_key_file("one");
        let (key_path_two, key_two) = write_ml_kem_session_key_file("two");
        let genesis = GenesisConfig {
            chain_id: "entangrid-network-test".into(),
            epoch_seed: [0u8; 32],
            genesis_time_unix_millis: 0,
            slot_duration_millis: 1_000,
            slots_per_epoch: 6,
            max_txs_per_block: 16,
            witness_count: 1,
            validators: vec![
                ValidatorConfig {
                    validator_id: 1,
                    stake: 100,
                    address: "127.0.0.1:4100".into(),
                    dev_secret: "secret-1".into(),
                    public_identity: PublicIdentity::default(),
                    session_public_identity: Some(hybrid_session_public_identity(key_one)),
                },
                ValidatorConfig {
                    validator_id: 2,
                    stake: 100,
                    address: "127.0.0.1:4101".into(),
                    dev_secret: "secret-2".into(),
                    public_identity: PublicIdentity::default(),
                    session_public_identity: Some(hybrid_session_public_identity(key_two)),
                },
            ],
            initial_balances: BTreeMap::new(),
        };
        let config_one = NodeConfig {
            validator_id: 1,
            data_dir: "/tmp/network-node-1".into(),
            genesis_path: "/tmp/network-genesis.toml".into(),
            listen_address: "127.0.0.1:4100".into(),
            peers: Vec::new(),
            log_path: "/tmp/network-events-1.log".into(),
            metrics_path: "/tmp/network-metrics-1.json".into(),
            feature_flags: FeatureFlags::default(),
            fault_profile: FaultProfile::default(),
            sync_on_startup: true,
            signing_backend: SigningBackendKind::DevDeterministic,
            signing_key_path: None,
            session_backend: SessionBackendKind::HybridDeterministicMlKemExperimental,
            session_key_path: Some(key_path_one.display().to_string()),
        };
        let config_two = NodeConfig {
            validator_id: 2,
            data_dir: "/tmp/network-node-2".into(),
            genesis_path: "/tmp/network-genesis.toml".into(),
            listen_address: "127.0.0.1:4101".into(),
            peers: Vec::new(),
            log_path: "/tmp/network-events-2.log".into(),
            metrics_path: "/tmp/network-metrics-2.json".into(),
            feature_flags: FeatureFlags::default(),
            fault_profile: FaultProfile::default(),
            sync_on_startup: true,
            signing_backend: SigningBackendKind::DevDeterministic,
            signing_key_path: None,
            session_backend: SessionBackendKind::HybridDeterministicMlKemExperimental,
            session_key_path: Some(key_path_two.display().to_string()),
        };
        (genesis, config_one, config_two)
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
        let server_crypto = DeterministicCryptoBackend::from_validators(&validators);
        let client_hello: SessionClientHello = read_bincode_frame(&mut stream).await;
        let (server_hello, server_session) =
            server_crypto.accept_client_hello(2, &client_hello).unwrap();
        write_bincode_frame(&mut stream, &server_hello).await;

        let mut receive_counter = 0;
        let first: SignedEnvelope =
            read_application_bincode_frame(&mut stream, &server_session, &mut receive_counter)
                .await;
        assert_eq!(first.payload, payload_one);

        let second: SignedEnvelope = timeout(
            Duration::from_secs(2),
            read_application_bincode_frame(&mut stream, &server_session, &mut receive_counter),
        )
        .await
        .expect("second frame on same stream");
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
        let client_hello = client_crypto.build_client_hello(1, 2, [7; 32]).unwrap();
        write_bincode_frame(&mut stream, &client_hello).await;
        let server_hello: SessionServerHello = read_bincode_frame(&mut stream).await;
        let client_session = client_crypto
            .finalize_client_session(1, &client_hello, &server_hello)
            .unwrap();
        let mut send_counter = 0;
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
            write_application_frame(&mut stream, &client_session, &mut send_counter, &bytes)
                .await
                .unwrap();
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

    #[tokio::test(flavor = "current_thread")]
    async fn inbound_rejects_frames_before_handshake_success() {
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
            Arc::clone(&metrics),
            event_tx,
        )
        .await
        .unwrap();

        let client_crypto = DeterministicCryptoBackend::from_genesis(&genesis);
        let mut stream = TcpStream::connect(&listen_address).await.unwrap();
        let payload = ProtocolMessage::HeartbeatPulse(HeartbeatPulse {
            epoch: 0,
            slot: 1,
            source_validator_id: 1,
            sequence_number: 1,
            emitted_at_unix_millis: 1,
        });
        let message_hash = canonical_hash(&payload);
        let envelope = SignedEnvelope {
            from_validator_id: 1,
            message_hash,
            signature: client_crypto.sign(1, &message_hash).unwrap(),
            payload,
        };
        write_bincode_frame(&mut stream, &envelope).await;

        let event = timeout(Duration::from_secs(2), event_rx.recv())
            .await
            .expect("event in time")
            .expect("event received");
        match event {
            NetworkEvent::SessionFailed { detail, .. } => {
                assert!(detail.contains("client hello") || detail.contains("handshake"));
            }
            other => panic!("expected SessionFailed, got {other:?}"),
        }
        assert_eq!(metrics.lock().unwrap().handshake_failures, 1);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn reconnect_replays_full_handshake_before_resuming_frames() {
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
        network
            .send_to(
                PeerConfig {
                    validator_id: 2,
                    address: peer_address.clone(),
                },
                payload_one.clone(),
            )
            .unwrap();

        let server_crypto = DeterministicCryptoBackend::from_validators(&validators);
        let (mut first_stream, _) = timeout(Duration::from_secs(2), listener.accept())
            .await
            .expect("accepted first connection")
            .unwrap();
        let first_client_hello: SessionClientHello = read_bincode_frame(&mut first_stream).await;
        let (first_server_hello, first_server_session) = server_crypto
            .accept_client_hello(2, &first_client_hello)
            .unwrap();
        write_bincode_frame(&mut first_stream, &first_server_hello).await;
        let mut first_receive_counter = 0;
        let first_envelope: SignedEnvelope = read_application_bincode_frame(
            &mut first_stream,
            &first_server_session,
            &mut first_receive_counter,
        )
        .await;
        assert_eq!(first_envelope.payload, payload_one);
        abort_tcp_stream(first_stream.into_std().unwrap());
        tokio::time::sleep(Duration::from_millis(100)).await;

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
                payload_two.clone(),
            )
            .unwrap();

        let (mut second_stream, _) = timeout(Duration::from_secs(2), listener.accept())
            .await
            .expect("accepted second connection")
            .unwrap();
        let second_client_hello: SessionClientHello = read_bincode_frame(&mut second_stream).await;
        assert_ne!(first_client_hello.nonce, second_client_hello.nonce);
        let (second_server_hello, second_server_session) = server_crypto
            .accept_client_hello(2, &second_client_hello)
            .unwrap();
        write_bincode_frame(&mut second_stream, &second_server_hello).await;
        let mut second_receive_counter = 0;
        let second_envelope: SignedEnvelope = read_application_bincode_frame(
            &mut second_stream,
            &second_server_session,
            &mut second_receive_counter,
        )
        .await;
        assert_eq!(second_envelope.payload, payload_two);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn deterministic_stream_keeps_plaintext_frames() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let peer_address = listener.local_addr().unwrap().to_string();
        let local_address = reserve_local_address().await;
        let validators = sample_validators();
        let crypto: Arc<dyn CryptoBackend> =
            Arc::new(DeterministicCryptoBackend::from_validators(&validators));
        let client_crypto = DeterministicCryptoBackend::from_validators(&validators);
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

        let payload = ProtocolMessage::HeartbeatPulse(HeartbeatPulse {
            epoch: 0,
            slot: 1,
            source_validator_id: 1,
            sequence_number: 1,
            emitted_at_unix_millis: 1,
        });
        let message_hash = canonical_hash(&payload);
        let expected_envelope = SignedEnvelope {
            from_validator_id: 1,
            message_hash,
            signature: client_crypto.sign(1, &message_hash).unwrap(),
            payload: payload.clone(),
        };
        let expected_plaintext =
            bincode::serde::encode_to_vec(&expected_envelope, bincode::config::standard()).unwrap();

        network
            .send_to(
                PeerConfig {
                    validator_id: 2,
                    address: peer_address.clone(),
                },
                payload,
            )
            .unwrap();

        let (mut stream, _) = timeout(Duration::from_secs(2), listener.accept())
            .await
            .expect("accepted deterministic connection")
            .unwrap();
        let client_hello: SessionClientHello = read_bincode_frame(&mut stream).await;
        let server_crypto = DeterministicCryptoBackend::from_validators(&validators);
        let (server_hello, server_session) =
            server_crypto.accept_client_hello(2, &client_hello).unwrap();
        assert!(!server_session.encrypt_frames);
        write_bincode_frame(&mut stream, &server_hello).await;
        let first_frame = read_raw_frame(&mut stream).await;

        assert_eq!(
            first_frame, expected_plaintext,
            "deterministic post-handshake frames should remain plaintext"
        );
    }

    #[cfg(feature = "pq-ml-kem")]
    #[tokio::test(flavor = "current_thread")]
    async fn hybrid_stream_encrypts_post_handshake_frames() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let peer_address = listener.local_addr().unwrap().to_string();
        let local_address = reserve_local_address().await;
        let (genesis, client_config, server_config) = hybrid_session_genesis_and_configs();
        let client_crypto_for_expectation = build_crypto_backend(&genesis, &client_config).unwrap();
        let client_crypto = build_crypto_backend(&genesis, &client_config).unwrap();
        let server_crypto = build_crypto_backend(&genesis, &server_config).unwrap();
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
            Arc::clone(&client_crypto),
            metrics,
            event_tx,
        )
        .await
        .unwrap();

        let payload = ProtocolMessage::HeartbeatPulse(HeartbeatPulse {
            epoch: 0,
            slot: 1,
            source_validator_id: 1,
            sequence_number: 1,
            emitted_at_unix_millis: 1,
        });
        let message_hash = canonical_hash(&payload);
        let expected_envelope = SignedEnvelope {
            from_validator_id: 1,
            message_hash,
            signature: client_crypto_for_expectation
                .sign(1, &message_hash)
                .unwrap(),
            payload: payload.clone(),
        };
        let expected_plaintext =
            bincode::serde::encode_to_vec(&expected_envelope, bincode::config::standard()).unwrap();

        network
            .send_to(
                PeerConfig {
                    validator_id: 2,
                    address: peer_address.clone(),
                },
                payload,
            )
            .unwrap();

        let (mut stream, _) = timeout(Duration::from_secs(2), listener.accept())
            .await
            .expect("accepted hybrid connection")
            .unwrap();
        let client_hello: SessionClientHello = read_bincode_frame(&mut stream).await;
        let (server_hello, _) = server_crypto.accept_client_hello(2, &client_hello).unwrap();
        write_bincode_frame(&mut stream, &server_hello).await;
        let first_frame = read_raw_frame(&mut stream).await;

        assert_ne!(
            first_frame, expected_plaintext,
            "expected encrypted post-handshake frame, but observed plaintext envelope bytes"
        );
    }

    #[cfg(feature = "pq-ml-kem")]
    #[tokio::test(flavor = "current_thread")]
    async fn tampered_encrypted_frame_drops_stream() {
        let listen_address = reserve_local_address().await;
        let (genesis, server_config, client_config) = hybrid_session_genesis_and_configs();
        let server_crypto = build_crypto_backend(&genesis, &server_config).unwrap();
        let client_crypto = build_crypto_backend(&genesis, &client_config).unwrap();
        let metrics = Arc::new(Mutex::new(NodeMetrics {
            validator_id: 1,
            ..NodeMetrics::default()
        }));
        let (event_tx, mut event_rx) = mpsc::unbounded_channel();
        let _network = spawn_network(
            1,
            listen_address.clone(),
            vec![PeerConfig {
                validator_id: 2,
                address: reserve_local_address().await,
            }],
            FaultProfile::default(),
            Arc::clone(&server_crypto),
            Arc::clone(&metrics),
            event_tx,
        )
        .await
        .unwrap();

        let mut stream = TcpStream::connect(&listen_address).await.unwrap();
        let nonce = session_nonce(2, 1);
        let client_hello = client_crypto.build_client_hello(2, 1, nonce).unwrap();
        write_bincode_frame(&mut stream, &client_hello).await;
        let server_hello: SessionServerHello = read_bincode_frame(&mut stream).await;
        let session = client_crypto
            .finalize_client_session(2, &client_hello, &server_hello)
            .unwrap();

        let payload = ProtocolMessage::HeartbeatPulse(HeartbeatPulse {
            epoch: 0,
            slot: 1,
            source_validator_id: 2,
            sequence_number: 1,
            emitted_at_unix_millis: 1,
        });
        let message_hash = canonical_hash(&payload);
        let envelope = SignedEnvelope {
            from_validator_id: 2,
            message_hash,
            signature: client_crypto.sign(2, &message_hash).unwrap(),
            payload,
        };
        let plaintext =
            bincode::serde::encode_to_vec(&envelope, bincode::config::standard()).unwrap();
        let mut ciphertext =
            encrypt_frame_payload(&session, FrameDirection::Outbound, 0, &plaintext).unwrap();
        let last = ciphertext.last_mut().expect("ciphertext byte");
        *last ^= 0x01;
        write_frame(&mut stream, &ciphertext).await.unwrap();

        let event = timeout(Duration::from_secs(2), event_rx.recv())
            .await
            .expect("event in time")
            .expect("event received");
        match event {
            NetworkEvent::SessionFailed { detail, .. } => {
                assert!(
                    detail.contains("encrypted frame authentication failed"),
                    "unexpected detail: {detail}"
                );
            }
            other => panic!("expected SessionFailed, got {other:?}"),
        }
        assert_eq!(metrics.lock().unwrap().handshake_failures, 1);
    }
}
