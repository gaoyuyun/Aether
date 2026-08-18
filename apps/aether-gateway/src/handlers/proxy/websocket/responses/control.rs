//! Per-turn control-plane refresh for long-lived Responses WebSockets.
//!
//! An Upgrade authenticates the connection, but it must not freeze API-key,
//! wallet, IP, model, or RPM policy for up to an hour. This module produces one
//! live decision and its exact strong API-key snapshot for every
//! `response.create`; the caller uses that pair consistently for rate limiting,
//! redaction, model authorization, planning, admission, balance, and retries.

use axum::http::StatusCode;
use serde_json::Value;

use crate::ai_serving::GatewayAuthApiKeySnapshot;
use crate::control::{
    refresh_execution_runtime_auth_context_with_snapshot, request_model_local_rejection,
    GatewayControlDecision, GatewayLocalAuthRejection,
};
use crate::handlers::proxy::websocket::ingress::WebSocketRequestContext;
use crate::handlers::proxy::websocket::session::WEBSOCKET_LOG_TRANSPORT;
use crate::handlers::shared::ip_rules_allow;
use crate::{AppState, GatewayError};

const LOG_TARGET: &str = "aether_gateway::handlers::proxy::responses_ws";

macro_rules! warn {
    ($($arg:tt)*) => {
        tracing::warn!(target: LOG_TARGET, $($arg)*)
    };
}

#[derive(Debug, Clone)]
pub(super) struct ResponsesWebSocketTurnControl {
    pub(super) decision: GatewayControlDecision,
    pub(super) auth_snapshot: Option<GatewayAuthApiKeySnapshot>,
    pub(super) rpm_bypassed: bool,
}

pub(super) async fn resolve_responses_websocket_turn_control(
    state: &AppState,
    context: &WebSocketRequestContext,
    parts: &http::request::Parts,
    client_event: &Value,
) -> Result<ResponsesWebSocketTurnControl, GatewayError> {
    if state
        .admin_security_ip_blacklisted(context.client_ip)
        .await?
    {
        return Err(GatewayError::Client {
            status: StatusCode::FORBIDDEN,
            message: "The current IP is blocked".to_string(),
        });
    }

    let mut decision = context.decision.clone();
    let auth_snapshot = if let Some(auth_context) = decision.auth_context.take() {
        let (refreshed, snapshot) = refresh_execution_runtime_auth_context_with_snapshot(
            state,
            auth_context,
            decision.auth_endpoint_signature.as_deref(),
        )
        .await?;
        decision.local_auth_rejection = refreshed.local_rejection.clone();
        decision.auth_context = Some(refreshed);
        snapshot
    } else {
        None
    };
    // Model-directive configuration is mutable policy too; do not retain the
    // Upgrade-time snapshot for the lifetime of the socket.
    decision.model_directive_policy =
        crate::system_features::ModelDirectivePolicySnapshot::load(state).await;

    if let Some(rejection) = decision.local_auth_rejection.clone() {
        return Err(websocket_auth_rejection_error(rejection));
    }
    let Some(auth_context) = decision.auth_context.as_ref() else {
        return Err(websocket_auth_rejection_error(
            GatewayLocalAuthRejection::InvalidApiKey,
        ));
    };
    if !auth_context.access_allowed
        || auth_context.user_id.trim().is_empty()
        || auth_context.api_key_id.trim().is_empty()
    {
        return Err(websocket_auth_rejection_error(
            GatewayLocalAuthRejection::InvalidApiKey,
        ));
    }
    if !ip_rules_allow(auth_context.ip_rules.as_deref(), context.client_ip) {
        return Err(websocket_auth_rejection_error(
            GatewayLocalAuthRejection::IpNotAllowed {
                remote_ip: context.client_ip.to_string(),
            },
        ));
    }

    let body = serde_json::to_vec(client_event)
        .map(axum::body::Bytes::from)
        .map_err(|error| GatewayError::Internal(error.to_string()))?;
    if let Some(rejection) =
        request_model_local_rejection(state, Some(&decision), &parts.uri, &parts.headers, &body)
            .await?
    {
        return Err(websocket_auth_rejection_error(rejection));
    }

    let rpm_bypassed = match state.admin_security_ip_whitelisted(context.client_ip).await {
        Ok(value) => value,
        Err(error) => {
            warn!(
                event_name = "responses_websocket_turn_ip_whitelist_check_failed",
                log_type = "ops",
                transport = WEBSOCKET_LOG_TRANSPORT,
                websocket = true,
                trace_id = %context.trace_id,
                client_ip = %context.client_ip,
                error = ?error,
                "gateway applied ordinary WebSocket RPM after the live IP whitelist check failed"
            );
            false
        }
    };

    Ok(ResponsesWebSocketTurnControl {
        decision,
        auth_snapshot,
        rpm_bypassed,
    })
}

fn websocket_auth_rejection_error(rejection: GatewayLocalAuthRejection) -> GatewayError {
    let (status, message) = match rejection {
        GatewayLocalAuthRejection::InvalidApiKey => {
            (StatusCode::UNAUTHORIZED, "The API key is invalid")
        }
        GatewayLocalAuthRejection::StandaloneKeysDisabled => {
            (StatusCode::FORBIDDEN, "Standalone API keys are disabled")
        }
        GatewayLocalAuthRejection::LockedApiKey => (
            StatusCode::FORBIDDEN,
            "The API key is locked and cannot be used",
        ),
        GatewayLocalAuthRejection::WalletUnavailable => {
            (StatusCode::FORBIDDEN, "The account wallet is unavailable")
        }
        GatewayLocalAuthRejection::BalanceDenied { remaining } => {
            let message = match remaining {
                Some(remaining) => format!("Insufficient balance (remaining: ${remaining:.2})"),
                None => "Insufficient balance".to_string(),
            };
            return GatewayError::Client {
                status: StatusCode::TOO_MANY_REQUESTS,
                message,
            };
        }
        GatewayLocalAuthRejection::ProviderNotAllowed { .. } => (
            StatusCode::FORBIDDEN,
            "The provider is not allowed for this API key",
        ),
        GatewayLocalAuthRejection::ApiFormatNotAllowed { .. } => (
            StatusCode::FORBIDDEN,
            "The API format is not allowed for this API key",
        ),
        GatewayLocalAuthRejection::ModelNotAllowed { .. } => (
            StatusCode::FORBIDDEN,
            "The requested model is not allowed for this API key",
        ),
        GatewayLocalAuthRejection::IpNotAllowed { .. } => (
            StatusCode::UNAUTHORIZED,
            "The current IP is not allowed for this API key",
        ),
    };
    GatewayError::Client {
        status,
        message: message.to_string(),
    }
}
