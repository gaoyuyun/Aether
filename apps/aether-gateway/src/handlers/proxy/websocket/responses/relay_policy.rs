//! Pure relay policy decisions for the Responses WebSocket session.
//!
//! The session owns sockets, provider planning, and usage persistence.  This
//! module deliberately owns none of those resources: it only turns observed
//! protocol facts into a bounded action.  Keeping this layer dependency-free
//! makes the failure paths executable with `rustc --test` without linking the
//! full gateway (which is useful on constrained CI/diagnostic hosts).

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FatalRelaySignal {
    ConnectionAdmissionLost,
    InvalidUpstreamText,
    UnexpectedUpstreamStreamId,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FatalRelayPolicy {
    pub status_code: u16,
    pub close_code: u16,
    pub error_code: &'static str,
    pub client_message: &'static str,
    pub close_reason: &'static str,
}

/// Map a local relay failure to the status/event/close tuple sent after the
/// HTTP upgrade.  In particular, capacity loss is retryable (1013), while a
/// malformed provider frame is an internal relay error (1011).
pub const fn fatal_relay_policy(signal: FatalRelaySignal) -> FatalRelayPolicy {
    match signal {
        FatalRelaySignal::ConnectionAdmissionLost => FatalRelayPolicy {
            status_code: 503,
            close_code: 1013,
            error_code: "gateway_connection_admission_lost",
            client_message: "Gateway capacity lease was lost; reconnect to continue",
            close_reason: "connection_admission_lost",
        },
        FatalRelaySignal::InvalidUpstreamText => FatalRelayPolicy {
            status_code: 502,
            close_code: 1011,
            error_code: "responses_websocket_invalid_upstream_event",
            client_message: "Provider returned an invalid WebSocket event",
            close_reason: "invalid_upstream_event",
        },
        FatalRelaySignal::UnexpectedUpstreamStreamId => FatalRelayPolicy {
            status_code: 502,
            close_code: 1011,
            error_code: "responses_websocket_unexpected_upstream_stream_id",
            client_message: "Provider returned a named-lane event on a default-lane connection",
            close_reason: "unexpected_upstream_stream_id",
        },
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UpstreamFrameKind {
    Other,
    Started,
    Terminal,
    Close,
    InvalidText,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UpstreamFrameAction {
    Continue,
    FinalizeTurn,
    FinalizeAndClose,
}

/// Classify the lifecycle effect of one upstream frame.  A malformed text
/// frame and a non-terminal close both finalize the active turn before the
/// client socket is closed; a valid terminal event finalizes the turn but is
/// still eligible for the normal downstream forwarding path.
pub const fn classify_upstream_frame(kind: UpstreamFrameKind) -> UpstreamFrameAction {
    match kind {
        UpstreamFrameKind::Other | UpstreamFrameKind::Started => UpstreamFrameAction::Continue,
        UpstreamFrameKind::Terminal => UpstreamFrameAction::FinalizeTurn,
        UpstreamFrameKind::Close | UpstreamFrameKind::InvalidText => {
            UpstreamFrameAction::FinalizeAndClose
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct QuotaRelayFacts {
    /// The adapter has observed a definitive quota signal and is ready to
    /// drain/rebind the current upstream.
    pub drain_ready: bool,
    /// The adapter allows a transparent replay of this turn.
    pub retry_current_turn: bool,
    /// The session already attempted the adapter-approved transparent replay
    /// and could not bind an alternate upstream. This prevents retry loops and
    /// causes the original upstream quota event to be relayed to the client.
    pub transparent_retry_failed: bool,
    /// The event contains the definitive `usage_limit_reached` error.  A
    /// merely exhausted-looking rate-limit snapshot must not trigger retry.
    pub usage_limit_error: bool,
    pub upstream_closed: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QuotaRelayAction {
    None,
    AttemptTransparentRetry,
    ForwardQuotaAndDetach,
}

/// Decide the quota branch before any response is forwarded to the client.
/// `retry_current_turn` intentionally wins over the continuation branch: the
/// session attempts the normal transparent retry first, then calls this again
/// with `transparent_retry_failed` after that attempt fails.  This preserves
/// the Codex recovery order while making each fallback explicit.
pub const fn classify_quota_relay(facts: QuotaRelayFacts) -> QuotaRelayAction {
    if !facts.drain_ready {
        return QuotaRelayAction::None;
    }
    if facts.usage_limit_error && facts.retry_current_turn && !facts.transparent_retry_failed {
        return QuotaRelayAction::AttemptTransparentRetry;
    }
    if facts.usage_limit_error || facts.upstream_closed {
        return QuotaRelayAction::ForwardQuotaAndDetach;
    }
    QuotaRelayAction::None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, Clone, Copy)]
    struct MockUpstream {
        frames: &'static [UpstreamFrameKind],
        cursor: usize,
    }

    impl MockUpstream {
        const fn new(frames: &'static [UpstreamFrameKind]) -> Self {
            Self { frames, cursor: 0 }
        }

        fn next(&mut self) -> Option<UpstreamFrameKind> {
            let frame = self.frames.get(self.cursor).copied()?;
            self.cursor += 1;
            Some(frame)
        }
    }

    #[test]
    fn mock_upstream_terminal_event_finalizes_without_waiting_for_an_extra_frame() {
        let mut upstream = MockUpstream::new(&[
            UpstreamFrameKind::Started,
            UpstreamFrameKind::Other,
            UpstreamFrameKind::Terminal,
        ]);

        assert_eq!(
            classify_upstream_frame(upstream.next().unwrap()),
            UpstreamFrameAction::Continue
        );
        assert_eq!(
            classify_upstream_frame(upstream.next().unwrap()),
            UpstreamFrameAction::Continue
        );
        assert_eq!(
            classify_upstream_frame(upstream.next().unwrap()),
            UpstreamFrameAction::FinalizeTurn
        );
        assert_eq!(upstream.next(), None);
    }

    #[test]
    fn mock_upstream_quota_429_attempts_one_transparent_retry_then_can_close() {
        let first = classify_quota_relay(QuotaRelayFacts {
            drain_ready: true,
            retry_current_turn: true,
            transparent_retry_failed: false,
            usage_limit_error: true,
            upstream_closed: false,
        });
        assert_eq!(first, QuotaRelayAction::AttemptTransparentRetry);

        // A failed transparent retry must not loop forever.  Once the adapter
        // no longer permits replay, the terminal quota event is forwarded and
        // the exhausted upstream is detached.
        let after_retry_failure = classify_quota_relay(QuotaRelayFacts {
            drain_ready: true,
            retry_current_turn: false,
            transparent_retry_failed: true,
            usage_limit_error: true,
            upstream_closed: true,
        });
        assert_eq!(after_retry_failure, QuotaRelayAction::ForwardQuotaAndDetach);
    }

    #[test]
    fn quota_error_is_forwarded_after_transparent_retry_is_unavailable() {
        assert_eq!(
            classify_quota_relay(QuotaRelayFacts {
                drain_ready: true,
                retry_current_turn: false,
                transparent_retry_failed: false,
                usage_limit_error: true,
                upstream_closed: false,
            }),
            QuotaRelayAction::ForwardQuotaAndDetach
        );
    }

    #[test]
    fn connection_admission_loss_is_retryable_and_invalid_json_is_terminal() {
        assert_eq!(
            fatal_relay_policy(FatalRelaySignal::ConnectionAdmissionLost),
            FatalRelayPolicy {
                status_code: 503,
                close_code: 1013,
                error_code: "gateway_connection_admission_lost",
                client_message: "Gateway capacity lease was lost; reconnect to continue",
                close_reason: "connection_admission_lost",
            }
        );
        assert_eq!(
            fatal_relay_policy(FatalRelaySignal::InvalidUpstreamText),
            FatalRelayPolicy {
                status_code: 502,
                close_code: 1011,
                error_code: "responses_websocket_invalid_upstream_event",
                client_message: "Provider returned an invalid WebSocket event",
                close_reason: "invalid_upstream_event",
            }
        );
    }

    #[test]
    fn invalid_json_never_maps_to_a_waiting_state() {
        let mut upstream = MockUpstream::new(&[UpstreamFrameKind::InvalidText]);
        let action = classify_upstream_frame(upstream.next().unwrap());
        assert_eq!(action, UpstreamFrameAction::FinalizeAndClose);
        assert_eq!(upstream.next(), None);
    }

    #[test]
    fn unexpected_named_lane_is_a_terminal_provider_protocol_error() {
        assert_eq!(
            fatal_relay_policy(FatalRelaySignal::UnexpectedUpstreamStreamId),
            FatalRelayPolicy {
                status_code: 502,
                close_code: 1011,
                error_code: "responses_websocket_unexpected_upstream_stream_id",
                client_message: "Provider returned a named-lane event on a default-lane connection",
                close_reason: "unexpected_upstream_stream_id",
            }
        );
    }

    #[test]
    fn quota_snapshot_without_definitive_error_does_not_trigger_retry() {
        assert_eq!(
            classify_quota_relay(QuotaRelayFacts {
                drain_ready: true,
                retry_current_turn: true,
                transparent_retry_failed: false,
                usage_limit_error: false,
                upstream_closed: false,
            }),
            QuotaRelayAction::None
        );

        assert_eq!(
            classify_quota_relay(QuotaRelayFacts {
                drain_ready: true,
                retry_current_turn: false,
                transparent_retry_failed: true,
                usage_limit_error: false,
                upstream_closed: true,
            }),
            QuotaRelayAction::ForwardQuotaAndDetach
        );
    }
}
