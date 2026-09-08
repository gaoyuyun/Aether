/// Proxy-side WebSocket connection handler
///
/// Handles the lifecycle of a single aether-tunnel connection:
/// accept -> authenticate (headers) -> read loop -> cleanup
use std::sync::Arc;
use std::time::Duration;

use aether_runtime::bounded_queue;
use axum::extract::ws::{Message, WebSocket};
use futures_util::{SinkExt, StreamExt};
use tokio::sync::watch;
use tracing::{debug, info, warn};

use super::hub::{ConnConfig, HubRouter, ProxyConn, ProxyManagementTokenCredential, SendStatus};
use super::protocol;
use aether_contracts::tunnel::{Frame, HelloPayload, MsgType};
use aether_contracts::tunnel_security::{SecureFrameCodec, TunnelSecurityRole};

/// Maximum single frame size: 64 MB
const MAX_FRAME_SIZE: usize = 64 * 1024 * 1024;
/// A connection that has passed the HTTP proof must still complete the
/// encrypted protocol handshake promptly.  Keeping this deadline independent
/// from the normal idle timeout prevents half-open authenticated sockets from
/// holding an admission permit indefinitely.
const PROXY_HELLO_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_PREAUTH_PINGS: usize = 8;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProxyHelloValidationError {
    MalformedFrame,
    DecryptionFailed,
    UnexpectedFrame,
    InvalidPayload,
    ProtocolVersionMismatch,
    SecuritySessionMismatch,
}

pub async fn handle_proxy_connection(
    ws: WebSocket,
    hub: Arc<HubRouter>,
    node_id: String,
    node_name: String,
    node_generation: String,
    max_streams: usize,
    protocol_version: u8,
    security_key: Option<String>,
    security_session: String,
    management_token_credential: Option<ProxyManagementTokenCredential>,
    cfg: ConnConfig,
) {
    let conn_id = hub.alloc_conn_id();
    let (mut ws_tx, mut ws_rx) = ws.split();

    let (security, initial_hello) = match security_key.as_deref() {
        Some(security_key) => {
            let security = match SecureFrameCodec::new(
                security_key,
                &security_session,
                TunnelSecurityRole::Server,
            ) {
                Ok(codec) => Arc::new(codec),
                Err(error) => {
                    warn!(conn_id, node_id = %node_id, error = %error, "secure tunnel codec initialization failed");
                    return;
                }
            };
            let Some(hello) = read_authenticated_proxy_hello(
                &mut ws_tx,
                &mut ws_rx,
                security.as_ref(),
                protocol_version,
                &security_session,
                conn_id,
                &node_id,
            )
            .await
            else {
                return;
            };
            (Some(security), Some(hello))
        }
        None => (None, None),
    };

    let (tx, mut rx) = bounded_queue::<Message>(cfg.outbound_queue_capacity);
    let (close_tx, mut close_rx) = watch::channel(false);
    let conn = ProxyConn::new(
        conn_id,
        node_id.clone(),
        node_name.clone(),
        tx,
        close_tx,
        max_streams,
        protocol_version,
    )
    .with_tunnel_generation(node_generation);
    let conn = match (security_key.clone(), management_token_credential) {
        (Some(key), None) => Arc::new(conn.with_authenticated_key(key)),
        (None, Some(credential)) => Arc::new(conn.with_management_token_credential(credential)),
        (Some(_), Some(_)) | (None, None) => {
            warn!(conn_id, node_id = %node_id, "proxy connection missing an unambiguous credential binding");
            return;
        }
    };

    hub.register_proxy(conn.clone());
    if let Some(mut hello) = initial_hello {
        hub.handle_proxy_frame(conn.id, &mut hello).await;
    }

    let writer_conn_id = conn_id;
    let writer_conn = conn.clone();
    let writer_security = security.clone();
    let writer = tokio::spawn(async move {
        let mut frames_sent: u64 = 0;
        loop {
            tokio::select! {
                msg = rx.recv() => match msg {
                    Some(msg) => {
                        let is_binary = matches!(&msg, Message::Binary(_));
                        let msg_len = match &msg {
                            Message::Binary(b) => b.len(),
                            _ => 0,
                        };
                        let send_started_at = std::time::Instant::now();
                            let msg = match encrypt_message(msg, writer_security.as_deref()) {
                            Ok(msg) => msg,
                            Err(error) => {
                                warn!(conn_id = writer_conn_id, error = %error, "failed to encrypt outbound proxy frame");
                                break;
                            }
                        };
                        let send_result = tokio::time::timeout(
                            Duration::from_secs(15),
                            ws_tx.send(msg),
                        ).await;
                        match send_result {
                            Ok(Ok(())) => {
                                writer_conn.record_write_latency(send_started_at.elapsed());
                            }
                            Ok(Err(e)) => {
                                let snapshot = writer_conn.outbound.snapshot();
                                warn!(
                                    conn_id = writer_conn_id,
                                    frames_sent = frames_sent,
                                    queue_depth = snapshot.depth,
                                    queue_capacity = snapshot.capacity,
                                    stream_count = writer_conn.stream_count.load(std::sync::atomic::Ordering::Relaxed),
                                    closing = writer_conn.outbound.is_closing(),
                                    draining = writer_conn.is_draining(),
                                    error = %e,
                                    "writer ws_tx.send failed"
                                );
                                break;
                            }
                            Err(_) => {
                                let snapshot = writer_conn.outbound.snapshot();
                                warn!(
                                    conn_id = writer_conn_id,
                                    frames_sent = frames_sent,
                                    queue_depth = snapshot.depth,
                                    queue_capacity = snapshot.capacity,
                                    stream_count = writer_conn.stream_count.load(std::sync::atomic::Ordering::Relaxed),
                                    closing = writer_conn.outbound.is_closing(),
                                    draining = writer_conn.is_draining(),
                                    "writer ws_tx.send timed out"
                                );
                                break;
                            }
                        }
                        frames_sent += 1;
                        if is_binary && msg_len > protocol::HEADER_SIZE {
                            debug!(
                                conn_id = writer_conn_id,
                                size = msg_len,
                                frames_sent = frames_sent,
                                "writer sent binary frame"
                            );
                        }
                    }
                    None => break,
                },
                changed = close_rx.changed() => {
                    if changed.is_err() || *close_rx.borrow() {
                        let snapshot = writer_conn.outbound.snapshot();
                        info!(
                            conn_id = writer_conn_id,
                            frames_sent = frames_sent,
                            queue_depth = snapshot.depth,
                            queue_capacity = snapshot.capacity,
                            stream_count = writer_conn.stream_count.load(std::sync::atomic::Ordering::Relaxed),
                            closing = writer_conn.outbound.is_closing(),
                            draining = writer_conn.is_draining(),
                            "writer close signal received"
                        );
                        break;
                    }
                }
            }
        }
        info!(
            conn_id = writer_conn_id,
            frames_sent = frames_sent,
            "writer task exiting"
        );
        writer_conn.request_close();
        match tokio::time::timeout(Duration::from_secs(5), ws_tx.close()).await {
            Ok(Ok(())) => debug!(
                conn_id = writer_conn_id,
                frames_sent = frames_sent,
                "writer WebSocket close completed"
            ),
            Ok(Err(error)) => warn!(
                conn_id = writer_conn_id,
                frames_sent = frames_sent,
                error = %error,
                "writer WebSocket close failed"
            ),
            Err(_) => warn!(
                conn_id = writer_conn_id,
                frames_sent = frames_sent,
                "writer WebSocket close timed out"
            ),
        }
    });

    let ping_conn = conn.clone();
    let ping_interval = cfg.ping_interval;
    let ping_task = tokio::spawn(async move {
        loop {
            tokio::time::sleep(ping_interval).await;
            let ping = protocol::encode_ping();
            let status = ping_conn
                .send_wait(Message::Binary(ping.into()), Duration::from_millis(250))
                .await;
            if !matches!(status, SendStatus::Queued) {
                let snapshot = ping_conn.outbound.snapshot();
                match status {
                    SendStatus::Closed => info!(
                        conn_id = ping_conn.id,
                        queue_depth = snapshot.depth,
                        queue_capacity = snapshot.capacity,
                        stream_count = ping_conn
                            .stream_count
                            .load(std::sync::atomic::Ordering::Relaxed),
                        closing = ping_conn.outbound.is_closing(),
                        draining = ping_conn.is_draining(),
                        "ping task stopped because connection is closing"
                    ),
                    SendStatus::Congested => warn!(
                        conn_id = ping_conn.id,
                        queue_depth = snapshot.depth,
                        queue_capacity = snapshot.capacity,
                        stream_count = ping_conn
                            .stream_count
                            .load(std::sync::atomic::Ordering::Relaxed),
                        closing = ping_conn.outbound.is_closing(),
                        draining = ping_conn.is_draining(),
                        "ping task stopped because outbound queue is congested"
                    ),
                    SendStatus::Queued => {}
                }
                break;
            }
        }
    });

    let reader_hub = hub.clone();
    let reader_conn = conn.clone();
    let reader = tokio::spawn(async move {
        run_proxy_reader(ws_rx, reader_hub, reader_conn, cfg.idle_timeout, security).await;
    });

    let _ = reader.await;
    ping_task.abort();
    let snapshot = conn.outbound.snapshot();
    info!(
        conn_id = conn.id,
        node_id = %conn.node_id,
        queue_depth = snapshot.depth,
        queue_capacity = snapshot.capacity,
        stream_count = conn.stream_count.load(std::sync::atomic::Ordering::Relaxed),
        closing = conn.outbound.is_closing(),
        draining = conn.is_draining(),
        "proxy connection cleanup starting"
    );
    conn.request_close();
    hub.unregister_proxy(conn_id, &node_id);
    drop(conn);
    tokio::time::sleep(Duration::from_millis(100)).await;
    writer.abort();
    let _ = writer.await;
}

async fn read_authenticated_proxy_hello(
    ws_tx: &mut futures_util::stream::SplitSink<WebSocket, Message>,
    ws_rx: &mut futures_util::stream::SplitStream<WebSocket>,
    security: &SecureFrameCodec,
    protocol_version: u8,
    security_session: &str,
    conn_id: u64,
    node_id: &str,
) -> Option<Vec<u8>> {
    let result = tokio::time::timeout(PROXY_HELLO_TIMEOUT, async {
        let mut preauth_pings = 0usize;
        loop {
            match ws_rx.next().await {
                Some(Ok(Message::Binary(data))) => {
                    return match validate_authenticated_proxy_hello(
                        data,
                        security,
                        protocol_version,
                        security_session,
                    ) {
                        Ok(hello) => Some(hello),
                        Err(error) => {
                            warn!(
                                conn_id,
                                node_id = %node_id,
                                ?error,
                                "proxy connection rejected: invalid encrypted HELLO"
                            );
                            None
                        }
                    };
                }
                Some(Ok(Message::Ping(payload))) => {
                    preauth_pings = preauth_pings.saturating_add(1);
                    if preauth_pings > MAX_PREAUTH_PINGS {
                        warn!(
                            conn_id,
                            node_id = %node_id,
                            "proxy connection rejected: too many WebSocket pings before encrypted HELLO"
                        );
                        return None;
                    }
                    if let Err(error) = ws_tx.send(Message::Pong(payload)).await {
                        warn!(conn_id, node_id = %node_id, error = %error, "failed to answer WebSocket ping before proxy authentication");
                        return None;
                    }
                }
                Some(Ok(Message::Pong(_))) => {}
                Some(Ok(Message::Close(_))) | None => {
                    info!(conn_id, node_id = %node_id, "proxy disconnected before encrypted HELLO authentication");
                    return None;
                }
                Some(Ok(Message::Text(_))) => {
                    warn!(conn_id, node_id = %node_id, "proxy connection rejected: text message received before encrypted HELLO");
                    return None;
                }
                Some(Err(error)) => {
                    warn!(conn_id, node_id = %node_id, error = %error, "proxy WebSocket failed before encrypted HELLO authentication");
                    return None;
                }
            }
        }
    })
    .await;

    match result {
        Ok(hello) => hello,
        Err(_) => {
            warn!(
                conn_id,
                node_id = %node_id,
                timeout_ms = PROXY_HELLO_TIMEOUT.as_millis(),
                "proxy connection rejected: encrypted HELLO timed out"
            );
            None
        }
    }
}

fn validate_authenticated_proxy_hello(
    data: bytes::Bytes,
    security: &SecureFrameCodec,
    protocol_version: u8,
    security_session: &str,
) -> Result<Vec<u8>, ProxyHelloValidationError> {
    let header =
        protocol::FrameHeader::parse(&data).ok_or(ProxyHelloValidationError::MalformedFrame)?;
    let expected_len = protocol::HEADER_SIZE
        .checked_add(header.payload_len as usize)
        .ok_or(ProxyHelloValidationError::MalformedFrame)?;
    if expected_len != data.len() {
        return Err(ProxyHelloValidationError::MalformedFrame);
    }

    let frame = Frame::decode(data).map_err(|_| ProxyHelloValidationError::MalformedFrame)?;
    let frame = security
        .decrypt_frame(frame)
        .map_err(|_| ProxyHelloValidationError::DecryptionFailed)?;
    if frame.stream_id != 0 || frame.msg_type != MsgType::Hello || frame.flags != 0 {
        return Err(ProxyHelloValidationError::UnexpectedFrame);
    }

    let hello = serde_json::from_slice::<HelloPayload>(&frame.payload)
        .map_err(|_| ProxyHelloValidationError::InvalidPayload)?;
    if hello.protocol_version != protocol_version {
        return Err(ProxyHelloValidationError::ProtocolVersionMismatch);
    }
    if hello.session_id.as_deref() != Some(security_session) {
        return Err(ProxyHelloValidationError::SecuritySessionMismatch);
    }

    Ok(frame.encode().to_vec())
}

async fn run_proxy_reader(
    mut ws_rx: futures_util::stream::SplitStream<WebSocket>,
    hub: Arc<HubRouter>,
    conn: Arc<ProxyConn>,
    idle_timeout: Duration,
    security: Option<Arc<SecureFrameCodec>>,
) {
    let idle_enabled = !idle_timeout.is_zero();
    let mut oversized_count = 0u32;
    let mut frames_received: u64 = 0;
    loop {
        let msg = if idle_enabled {
            tokio::select! {
                msg = ws_rx.next() => msg,
                _ = tokio::time::sleep(idle_timeout) => {
                    warn!(conn_id = conn.id, node_id = %conn.node_id, "proxy idle timeout");
                    let _ = conn.send(Message::Binary(protocol::encode_goaway().into()));
                    conn.request_close();
                    break;
                }
            }
        } else {
            ws_rx.next().await
        };

        match msg {
            Some(Ok(Message::Binary(data))) => {
                frames_received += 1;
                let mut data = match decrypt_message(data, security.as_deref()) {
                    Ok(data) => data,
                    Err(error) => {
                        warn!(conn_id = conn.id, error = %error, "failed to decrypt secure proxy frame");
                        conn.request_close();
                        break;
                    }
                };
                if data.len() > MAX_FRAME_SIZE {
                    oversized_count += 1;
                    warn!(
                        conn_id = conn.id,
                        size = data.len(),
                        "oversized frame from proxy"
                    );
                    if oversized_count >= 5 {
                        warn!(conn_id = conn.id, "too many oversized frames, closing");
                        conn.request_close();
                        break;
                    }
                    continue;
                }
                oversized_count = 0;

                if data.len() < protocol::HEADER_SIZE {
                    debug!(conn_id = conn.id, "frame too small, skipping");
                    continue;
                }

                hub.handle_proxy_frame(conn.id, &mut data).await;
            }
            Some(Ok(Message::Close(_))) | None => {
                info!(
                    conn_id = conn.id,
                    node_id = %conn.node_id,
                    frames_received = frames_received,
                    "proxy WebSocket closed"
                );
                break;
            }
            Some(Err(e)) => {
                warn!(
                    conn_id = conn.id,
                    frames_received = frames_received,
                    error = %e,
                    "proxy WebSocket error"
                );
                break;
            }
            Some(Ok(Message::Ping(payload))) => {
                conn.send(Message::Pong(payload));
            }
            _ => {}
        }
    }
}

fn encrypt_message(
    msg: Message,
    security: Option<&SecureFrameCodec>,
) -> Result<Message, aether_contracts::tunnel_security::TunnelSecurityError> {
    let Some(codec) = security else {
        return Ok(msg);
    };
    match msg {
        Message::Binary(data) => {
            let frame = Frame::decode(bytes::Bytes::from(data.to_vec()))
                .map_err(|_| aether_contracts::tunnel_security::TunnelSecurityError::Encrypt)?;
            Ok(Message::Binary(codec.encrypt_frame(frame)?))
        }
        other => Ok(other),
    }
}

fn decrypt_message(
    data: bytes::Bytes,
    security: Option<&SecureFrameCodec>,
) -> Result<Vec<u8>, aether_contracts::tunnel_security::TunnelSecurityError> {
    let Some(codec) = security else {
        return Ok(data.to_vec());
    };
    let frame = Frame::decode(data)
        .map_err(|_| aether_contracts::tunnel_security::TunnelSecurityError::Decrypt)?;
    let frame = codec.decrypt_frame(frame)?;
    Ok(frame.encode().to_vec())
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEY: &str = "BwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwc=";
    const SESSION: &str = "0123456789abcdef0123456789abcdef";
    const PROTOCOL_VERSION: u8 = 3;

    fn codecs() -> (SecureFrameCodec, SecureFrameCodec) {
        (
            SecureFrameCodec::new(KEY, SESSION, TunnelSecurityRole::Client).expect("client codec"),
            SecureFrameCodec::new(KEY, SESSION, TunnelSecurityRole::Server).expect("server codec"),
        )
    }

    fn hello_frame(protocol_version: u8, session_id: &str) -> Frame {
        Frame::control(
            MsgType::Hello,
            serde_json::to_vec(&HelloPayload {
                protocol_version,
                capabilities: vec!["flow-control".to_string()],
                session_id: Some(session_id.to_string()),
                replica_id: None,
            })
            .expect("hello payload"),
        )
    }

    #[test]
    fn authenticated_proxy_hello_accepts_bound_encrypted_control_frame() {
        let (client, server) = codecs();
        let encrypted = client
            .encrypt_frame(hello_frame(PROTOCOL_VERSION, SESSION))
            .expect("encrypted HELLO");

        let clear =
            validate_authenticated_proxy_hello(encrypted, &server, PROTOCOL_VERSION, SESSION)
                .expect("authenticated HELLO");
        let frame = Frame::decode(bytes::Bytes::from(clear)).expect("clear HELLO");

        assert_eq!(frame.stream_id, 0);
        assert_eq!(frame.msg_type, MsgType::Hello);
    }

    #[test]
    fn authenticated_proxy_hello_advances_shared_receive_sequence() {
        let (client, server) = codecs();
        let encrypted_hello = client
            .encrypt_frame(hello_frame(PROTOCOL_VERSION, SESSION))
            .expect("encrypted HELLO");
        validate_authenticated_proxy_hello(encrypted_hello, &server, PROTOCOL_VERSION, SESSION)
            .expect("authenticated HELLO");

        let encrypted_settings = client
            .encrypt_frame(Frame::control(MsgType::Settings, bytes::Bytes::new()))
            .expect("encrypted SETTINGS");
        let settings = server
            .decrypt_frame(Frame::decode(encrypted_settings).expect("wire SETTINGS"))
            .expect("next sequence should decrypt");

        assert_eq!(settings.msg_type, MsgType::Settings);
    }

    #[test]
    fn authenticated_proxy_hello_rejects_non_encrypted_or_wrong_frame() {
        let (_, server) = codecs();
        let clear = hello_frame(PROTOCOL_VERSION, SESSION).encode();
        assert_eq!(
            validate_authenticated_proxy_hello(clear, &server, PROTOCOL_VERSION, SESSION),
            Err(ProxyHelloValidationError::DecryptionFailed)
        );

        let (client, server) = codecs();
        let encrypted = client
            .encrypt_frame(Frame::control(MsgType::Settings, bytes::Bytes::new()))
            .expect("encrypted SETTINGS");
        assert_eq!(
            validate_authenticated_proxy_hello(encrypted, &server, PROTOCOL_VERSION, SESSION),
            Err(ProxyHelloValidationError::UnexpectedFrame)
        );

        let (client, server) = codecs();
        let encrypted = client
            .encrypt_frame(Frame::new(
                1,
                MsgType::Hello,
                0,
                hello_frame(PROTOCOL_VERSION, SESSION).payload,
            ))
            .expect("encrypted stream HELLO");
        assert_eq!(
            validate_authenticated_proxy_hello(encrypted, &server, PROTOCOL_VERSION, SESSION),
            Err(ProxyHelloValidationError::UnexpectedFrame)
        );
    }

    #[test]
    fn authenticated_proxy_hello_rejects_protocol_or_session_mismatch() {
        let (client, server) = codecs();
        let encrypted = client
            .encrypt_frame(hello_frame(PROTOCOL_VERSION - 1, SESSION))
            .expect("encrypted HELLO");
        assert_eq!(
            validate_authenticated_proxy_hello(encrypted, &server, PROTOCOL_VERSION, SESSION),
            Err(ProxyHelloValidationError::ProtocolVersionMismatch)
        );

        let (client, server) = codecs();
        let encrypted = client
            .encrypt_frame(hello_frame(PROTOCOL_VERSION, "different-session"))
            .expect("encrypted HELLO");
        assert_eq!(
            validate_authenticated_proxy_hello(encrypted, &server, PROTOCOL_VERSION, SESSION),
            Err(ProxyHelloValidationError::SecuritySessionMismatch)
        );
    }

    #[test]
    fn authenticated_proxy_hello_rejects_malformed_or_ambiguous_frame() {
        let (client, server) = codecs();
        let mut encrypted = client
            .encrypt_frame(hello_frame(PROTOCOL_VERSION, SESSION))
            .expect("encrypted HELLO")
            .to_vec();
        encrypted.push(0);
        assert_eq!(
            validate_authenticated_proxy_hello(
                bytes::Bytes::from(encrypted),
                &server,
                PROTOCOL_VERSION,
                SESSION,
            ),
            Err(ProxyHelloValidationError::MalformedFrame)
        );

        let (client, server) = codecs();
        let encrypted = client
            .encrypt_frame(Frame::control(
                MsgType::Hello,
                bytes::Bytes::from_static(b"not-json"),
            ))
            .expect("encrypted malformed HELLO");
        assert_eq!(
            validate_authenticated_proxy_hello(encrypted, &server, PROTOCOL_VERSION, SESSION),
            Err(ProxyHelloValidationError::InvalidPayload)
        );
    }
}
