//! Connection-scoped limits and primitives shared by AI WebSocket sessions.

use std::time::{Duration, Instant};

/// The public Responses WebSocket contract is intentionally bounded so a
/// single active socket cannot retain gateway resources indefinitely.
#[derive(Debug, Clone, Copy)]
pub(crate) struct WebSocketSessionLimits {
    pub(crate) max_frame_size: usize,
    pub(crate) max_message_size: usize,
    pub(crate) initial_message_timeout: Duration,
    pub(crate) max_connection_duration: Duration,
}

pub(crate) const RESPONSES_WEBSOCKET_SESSION_LIMITS: WebSocketSessionLimits =
    WebSocketSessionLimits {
        max_frame_size: 16 << 20,
        max_message_size: 16 << 20,
        initial_message_timeout: Duration::from_secs(60),
        max_connection_duration: Duration::from_secs(60 * 60),
    };

pub(crate) const LIVE_WEBSOCKET_SESSION_LIMITS: WebSocketSessionLimits = WebSocketSessionLimits {
    max_frame_size: 16 << 20,
    max_message_size: 16 << 20,
    initial_message_timeout: Duration::from_secs(60),
    max_connection_duration: Duration::from_secs(60 * 60),
};

pub(crate) const REALTIME_WEBSOCKET_SESSION_LIMITS: WebSocketSessionLimits =
    WebSocketSessionLimits {
        max_frame_size: 16 << 20,
        max_message_size: 16 << 20,
        initial_message_timeout: Duration::from_secs(60),
        max_connection_duration: Duration::from_secs(60 * 60),
    };

/// A peer that stops draining its receive window must not be able to pin the
/// relay loop.  Session loops await socket writes inside a `tokio::select!`,
/// so an unbounded write also suspends the connection and per-turn deadlines
/// that would otherwise reclaim the upstream socket and the shared upstream
/// admission permits.
pub(crate) const RELAY_WRITE_TIMEOUT: Duration = Duration::from_secs(30);

/// Frames the gateway emits while tearing a session down are best-effort: the
/// session is ending either way, so an unresponsive peer must not delay
/// releasing the upstream.
pub(crate) const TEARDOWN_WRITE_TIMEOUT: Duration = Duration::from_secs(5);

pub(crate) const CLOSE_POLICY_VIOLATION: u16 = 1008;
pub(crate) const CLOSE_INTERNAL_ERROR: u16 = 1011;
pub(crate) const CLOSE_TRY_AGAIN: u16 = 1013;
pub(crate) const WEBSOCKET_LOG_TRANSPORT: &str = "websocket";

/// Waits for an optional per-turn deadline without allocating a timer when no
/// turn is active.  The protocol adapter retains ownership of the deadline's
/// meaning and terminal outcome.
pub(crate) async fn wait_for_optional_deadline(deadline: Option<Instant>) {
    match deadline {
        Some(deadline) => tokio::time::sleep_until(tokio::time::Instant::from_std(deadline)).await,
        None => std::future::pending::<()>().await,
    }
}
