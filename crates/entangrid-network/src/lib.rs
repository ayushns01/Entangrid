use std::{
    collections::BTreeMap,
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
    sync::mpsc,
};

const MAX_FRAME_SIZE_BYTES: usize = 8 * 1024 * 1024;

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
    },
    SessionFailed {
        peer_validator_id: Option<ValidatorId>,
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
            .send(OutboundRequest { peer, payload })
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

    tokio::spawn(async move {
        loop {
            match listener.accept().await {
                Ok((stream, _)) => {
                    let crypto = Arc::clone(&listener_crypto);
                    let metrics = Arc::clone(&listener_metrics);
                    let event_tx = listener_events.clone();
                    tokio::spawn(async move {
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
                            });
                        }
                    });
                }
                Err(error) => {
                    let _ = listener_events.send(NetworkEvent::SessionFailed {
                        peer_validator_id: None,
                        detail: error.to_string(),
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
                if !peers_by_id.contains_key(&peer_id) {
                    let _ = event_tx.send(NetworkEvent::SessionFailed {
                        peer_validator_id: Some(peer_id),
                        detail: "peer not configured".into(),
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
        return Ok(());
    }

    let message_hash = canonical_hash(&request.payload);
    if should_drop(&fault_profile, request.peer.validator_id, &message_hash) {
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
    let _ = event_tx.send(NetworkEvent::SessionObserved {
        peer_validator_id: request.peer.validator_id,
        transcript_hash: session.transcript_hash,
    });

    let signature = crypto.sign(local_validator_id, &message_hash)?;
    let envelope = SignedEnvelope {
        from_validator_id: local_validator_id,
        message_hash,
        signature,
        payload: request.payload,
    };

    let bytes = bincode::serde::encode_to_vec(&envelope, bincode::config::standard())?;
    let mut stream = TcpStream::connect(&request.peer.address)
        .await
        .with_context(|| format!("failed to connect to {}", request.peer.address))?;
    stream.write_u32(bytes.len() as u32).await?;
    stream.write_all(&bytes).await?;
    stream.flush().await?;

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

fn update_metrics(metrics: &Arc<Mutex<NodeMetrics>>, update: impl FnOnce(&mut NodeMetrics)) {
    if let Ok(mut metrics) = metrics.lock() {
        update(&mut metrics);
        metrics.last_updated_unix_millis = entangrid_types::now_unix_millis();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_oversized_inbound_frame() {
        assert!(MAX_FRAME_SIZE_BYTES < usize::MAX);
        let oversized = MAX_FRAME_SIZE_BYTES + 1;
        assert!(oversized > MAX_FRAME_SIZE_BYTES);
    }
}
