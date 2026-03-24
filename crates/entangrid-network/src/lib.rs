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
    outbound_tx: mpsc::UnboundedSender<OutboundRequest>,
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
        self.outbound_tx
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
        self.outbound_tx
            .send(OutboundRequest {
                peer,
                payload,
                service_accountable: false,
            })
            .map_err(|_| anyhow!("network outbound worker is closed"))
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

    let (outbound_tx, mut outbound_rx) = mpsc::unbounded_channel::<OutboundRequest>();
    let listener_crypto = Arc::clone(&crypto);
    let listener_metrics = Arc::clone(&metrics);
    let listener_events = event_tx.clone();
    let inbound_limit = Arc::new(Semaphore::new(MAX_CONCURRENT_INBOUND_SESSIONS));

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

    tokio::spawn({
        let crypto = Arc::clone(&crypto);
        let metrics = Arc::clone(&metrics);
        let event_tx = event_tx.clone();
        let peers_by_id = Arc::clone(&peers_by_id);
        async move {
            while let Some(request) = outbound_rx.recv().await {
                let peer = request.peer.clone();
                let peer_id = peer.validator_id;
                let service_accountable = request.service_accountable;
                if !peers_by_id.contains_key(&peer_id) {
                    let _ = event_tx.send(NetworkEvent::SessionFailed {
                        peer_validator_id: Some(peer_id),
                        detail: "peer not configured".into(),
                        outbound: true,
                        service_accountable,
                        kind: NetworkFailureKind::PeerConfig,
                    });
                    continue;
                }
                if let Err(error) = send_message(
                    local_validator_id,
                    request,
                    fault_profile.clone(),
                    Arc::clone(&crypto),
                    Arc::clone(&metrics),
                    event_tx.clone(),
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
        }
    });

    Ok(NetworkHandle { outbound_tx })
}

async fn send_message(
    local_validator_id: ValidatorId,
    request: OutboundRequest,
    fault_profile: FaultProfile,
    crypto: Arc<dyn CryptoBackend>,
    metrics: Arc<Mutex<NodeMetrics>>,
    event_tx: mpsc::UnboundedSender<NetworkEvent>,
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
    let mut stream = connect_with_retries(&request.peer.address).await?;
    stream.write_u32(bytes.len() as u32).await?;
    stream.write_all(&bytes).await?;
    stream.flush().await?;

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
    let frame_len = stream.read_u32().await? as usize;
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
    Ok(())
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

fn update_metrics(metrics: &Arc<Mutex<NodeMetrics>>, update: impl FnOnce(&mut NodeMetrics)) {
    if let Ok(mut metrics) = metrics.lock() {
        update(&mut metrics);
        metrics.last_updated_unix_millis = entangrid_types::now_unix_millis();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Error;

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
}
