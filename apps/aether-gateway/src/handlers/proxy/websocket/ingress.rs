//! Authenticated public WebSocket upgrade admission shared by AI adapters.

use std::future::Future;
use std::net::{IpAddr, SocketAddr};

use axum::body::Body;
use axum::extract::ws::{WebSocket, WebSocketUpgrade};
use axum::http::header::{
    AUTHORIZATION, CONNECTION, COOKIE, HOST, PROXY_AUTHORIZATION, TE, TRAILER, TRANSFER_ENCODING,
    UPGRADE,
};
use axum::http::uri::PathAndQuery;
use axum::http::{HeaderMap, HeaderName, Method, Response, StatusCode, Uri};
use tracing::{info, warn};

use crate::api::response::{
    build_local_auth_rejection_response, build_local_http_error_response,
    build_local_overloaded_response,
};
use crate::control::{
    trusted_auth_local_rejection, GatewayControlDecision, GatewayCredentialCarrier,
    GatewayLocalAuthRejection,
};
use crate::handlers::proxy::websocket::session::{WebSocketSessionLimits, WEBSOCKET_LOG_TRANSPORT};
use crate::handlers::shared::ip_rules_allow;
use crate::headers::{effective_client_ip, extract_or_generate_trace_id};
use crate::router::RequestAdmissionError;
use crate::{AppState, GatewayError};

/// Request facts that survive the HTTP Upgrade and are needed by a protocol
/// adapter for planning, rate limiting, and connection-scoped audit logs.
pub(crate) struct WebSocketRequestContext {
    pub(crate) trace_id: String,
    pub(crate) headers: HeaderMap,
    pub(crate) uri: Uri,
    pub(crate) remote_addr: SocketAddr,
    /// Effective client IP resolved once from the authenticated Upgrade. Every
    /// turn re-checks live API-key/admin IP policy against this immutable fact.
    pub(crate) client_ip: IpAddr,
    pub(crate) decision: GatewayControlDecision,
    /// Held for the lifetime of the upgraded socket. The Responses session
    /// polls its health and closes the client when a distributed lease is
    /// revoked or expires.
    pub(crate) websocket_connection_permit: Option<aether_runtime::AdmissionPermit>,
}

/// Adapter-specific wording and event identifiers for generic upgrade checks.
#[derive(Clone, Copy)]
pub(crate) struct WebSocketIngressSpec {
    pub(crate) route_unavailable_message: &'static str,
}

pub(crate) enum AuthenticatedAiWebSocketUpgradePreparation {
    Ready(AuthenticatedAiWebSocketUpgrade),
    Rejected(Response<Body>),
}

/// Authenticated HTTP Upgrade state retained while an adapter performs any
/// protocol-specific preflight that must complete before status 101 is sent.
pub(crate) struct AuthenticatedAiWebSocketUpgrade {
    state: AppState,
    context: WebSocketRequestContext,
    request_permit: Option<aether_runtime::AdmissionPermit>,
}

impl AuthenticatedAiWebSocketUpgrade {
    pub(crate) fn state(&self) -> &AppState {
        &self.state
    }

    pub(crate) fn context(&self) -> &WebSocketRequestContext {
        &self.context
    }

    pub(crate) fn rejection_response(
        &self,
        status: StatusCode,
        message: &str,
    ) -> Result<Response<Body>, GatewayError> {
        build_local_http_error_response(
            self.context.trace_id.as_str(),
            Some(&self.context.decision),
            status,
            message,
        )
    }

    pub(crate) fn into_response<F, Fut>(
        self,
        ws: WebSocketUpgrade,
        limits: WebSocketSessionLimits,
        run_session: F,
    ) -> Response<Body>
    where
        F: FnOnce(WebSocket, AppState, WebSocketRequestContext) -> Fut + Send + 'static,
        Fut: Future<Output = ()> + Send + 'static,
    {
        self.into_response_with(ws, limits, (), move |socket, state, context, ()| {
            run_session(socket, state, context)
        })
    }

    pub(crate) fn into_response_with<P, F, Fut>(
        self,
        ws: WebSocketUpgrade,
        limits: WebSocketSessionLimits,
        prepared: P,
        run_session: F,
    ) -> Response<Body>
    where
        P: Send + 'static,
        F: FnOnce(WebSocket, AppState, WebSocketRequestContext, P) -> Fut + Send + 'static,
        Fut: Future<Output = ()> + Send + 'static,
    {
        let Self {
            state,
            context,
            request_permit,
        } = self;
        ws.max_frame_size(limits.max_frame_size)
            .max_message_size(limits.max_message_size)
            .on_upgrade(move |socket| async move {
                drop(request_permit);
                run_session(socket, state, context, prepared).await;
            })
    }
}

/// Performs the HTTP-only part of an AI WebSocket request.
///
/// The ordinary request permit covers only the HTTP Upgrade window. A
/// dedicated WebSocket connection permit is held for the socket lifetime so
/// idle clients cannot consume capacity reserved for normal HTTP requests.
pub(crate) async fn upgrade_authenticated_ai_websocket<F, Fut>(
    state: AppState,
    remote_addr: SocketAddr,
    ws: WebSocketUpgrade,
    headers: HeaderMap,
    uri: Uri,
    limits: WebSocketSessionLimits,
    spec: WebSocketIngressSpec,
    run_session: F,
) -> Result<Response<Body>, GatewayError>
where
    F: FnOnce(WebSocket, AppState, WebSocketRequestContext) -> Fut + Send + 'static,
    Fut: Future<Output = ()> + Send + 'static,
{
    match prepare_authenticated_ai_websocket(state, remote_addr, headers, uri, spec).await? {
        AuthenticatedAiWebSocketUpgradePreparation::Ready(prepared) => {
            Ok(prepared.into_response(ws, limits, run_session))
        }
        AuthenticatedAiWebSocketUpgradePreparation::Rejected(response) => Ok(response),
    }
}

pub(crate) async fn prepare_authenticated_ai_websocket(
    state: AppState,
    remote_addr: SocketAddr,
    headers: HeaderMap,
    uri: Uri,
    spec: WebSocketIngressSpec,
) -> Result<AuthenticatedAiWebSocketUpgradePreparation, GatewayError> {
    let trace_id = extract_or_generate_trace_id(&headers);
    let client_ip = effective_client_ip(&headers, &remote_addr);
    if state.admin_security_ip_blacklisted(client_ip).await? {
        return build_local_http_error_response(
            &trace_id,
            None,
            StatusCode::FORBIDDEN,
            "当前 IP 已被禁止访问",
        )
        .map(AuthenticatedAiWebSocketUpgradePreparation::Rejected);
    }

    let request_context = crate::control::resolve_public_request_context(
        &state,
        &Method::GET,
        &uri,
        &headers,
        &trace_id,
    )
    .await?;
    let Some(mut decision) = request_context.control_decision else {
        return build_local_http_error_response(
            &trace_id,
            None,
            StatusCode::NOT_FOUND,
            spec.route_unavailable_message,
        )
        .map(AuthenticatedAiWebSocketUpgradePreparation::Rejected);
    };
    if let Some(rejection) = trusted_auth_local_rejection(Some(&decision), &headers) {
        return build_local_auth_rejection_response(&trace_id, Some(&decision), &rejection)
            .map(AuthenticatedAiWebSocketUpgradePreparation::Rejected);
    }
    // Browsers attach cookies to WebSocket handshakes automatically and the
    // WebSocket API does not let callers add an Authorization header.  A
    // cookie-only public upgrade would therefore be vulnerable to cross-site
    // WebSocket hijacking unless every deployment maintained an Origin
    // allowlist.  Explicit API-key/bearer credentials (or trusted internal
    // auth resolved by the control plane) remain supported.
    if !websocket_credential_carrier_is_allowed(decision.gateway_credential_carrier) {
        warn!(
            event_name = "ai_websocket_cookie_only_auth_rejected",
            log_type = "security",
            transport = WEBSOCKET_LOG_TRANSPORT,
            websocket = true,
            trace_id = %trace_id,
            client_ip = %client_ip,
            "gateway rejected cookie-only public WebSocket authentication"
        );
        return build_local_auth_rejection_response(
            &trace_id,
            Some(&decision),
            &GatewayLocalAuthRejection::InvalidApiKey,
        )
        .map(AuthenticatedAiWebSocketUpgradePreparation::Rejected);
    }
    let Some(auth_context) = decision.auth_context.as_ref() else {
        return build_local_auth_rejection_response(
            &trace_id,
            Some(&decision),
            &GatewayLocalAuthRejection::InvalidApiKey,
        )
        .map(AuthenticatedAiWebSocketUpgradePreparation::Rejected);
    };
    if !auth_context.access_allowed
        || auth_context.user_id.trim().is_empty()
        || auth_context.api_key_id.trim().is_empty()
    {
        return build_local_auth_rejection_response(
            &trace_id,
            Some(&decision),
            &GatewayLocalAuthRejection::InvalidApiKey,
        )
        .map(AuthenticatedAiWebSocketUpgradePreparation::Rejected);
    }
    if !ip_rules_allow(auth_context.ip_rules.as_deref(), client_ip) {
        return build_local_auth_rejection_response(
            &trace_id,
            Some(&decision),
            &GatewayLocalAuthRejection::IpNotAllowed {
                remote_ip: client_ip.to_string(),
            },
        )
        .map(AuthenticatedAiWebSocketUpgradePreparation::Rejected);
    }

    let request_permit = match state.try_acquire_request_permit().await {
        Ok(permit) => permit,
        Err(error) => {
            return websocket_admission_error_response(
                &trace_id,
                &decision,
                Some(uri.path()),
                error,
            )
            .map(AuthenticatedAiWebSocketUpgradePreparation::Rejected)
        }
    };
    let websocket_connection_permit = match state.try_acquire_websocket_connection_permit().await {
        Ok(permit) => permit,
        Err(error) => {
            return websocket_admission_error_response(
                &trace_id,
                &decision,
                Some(uri.path()),
                error,
            )
            .map(AuthenticatedAiWebSocketUpgradePreparation::Rejected)
        }
    };

    // Authentication has consumed the downstream credentials.  From this
    // point on the URI and headers become planner input, so retain neither an
    // API key from the query string nor client authentication/handshake
    // headers.  Provider authentication is added independently by the
    // planner and is therefore unaffected by this boundary.
    let uri = websocket_planning_uri(&uri);
    decision.public_query_string = uri.query().map(ToOwned::to_owned);
    let headers = websocket_planning_headers(headers);
    let context = WebSocketRequestContext {
        trace_id,
        headers,
        uri,
        remote_addr,
        client_ip,
        decision,
        websocket_connection_permit,
    };
    Ok(AuthenticatedAiWebSocketUpgradePreparation::Ready(
        AuthenticatedAiWebSocketUpgrade {
            state,
            context,
            request_permit,
        },
    ))
}

fn websocket_credential_carrier_is_allowed(carrier: Option<GatewayCredentialCarrier>) -> bool {
    carrier != Some(GatewayCredentialCarrier::CookieHeader)
}

fn websocket_planning_uri(uri: &Uri) -> Uri {
    let Some(query) = uri.query() else {
        return uri.clone();
    };
    let mut retained = Vec::new();
    let mut removed_sensitive_value = false;
    for (name, value) in url::form_urlencoded::parse(query.as_bytes()) {
        if websocket_query_parameter_is_sensitive(name.as_ref()) {
            removed_sensitive_value = true;
        } else {
            retained.push((name.into_owned(), value.into_owned()));
        }
    }
    if !removed_sensitive_value {
        return uri.clone();
    }

    let mut serializer = url::form_urlencoded::Serializer::new(String::new());
    serializer.extend_pairs(retained.iter().map(|(name, value)| (name, value)));
    let retained_query = serializer.finish();
    let path_and_query = if retained_query.is_empty() {
        uri.path().to_string()
    } else {
        format!("{}?{retained_query}", uri.path())
    };
    let path_and_query = path_and_query
        .parse::<PathAndQuery>()
        .expect("a valid URI path plus form-encoded query must remain valid");
    let mut parts = uri.clone().into_parts();
    parts.path_and_query = Some(path_and_query);
    Uri::from_parts(parts).expect("replacing only path-and-query must preserve a valid URI")
}

fn websocket_query_parameter_is_sensitive(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        "key" | "api_key" | "api-key" | "access_token" | "authorization" | "token"
    )
}

fn websocket_planning_headers(mut headers: HeaderMap) -> HeaderMap {
    // RFC 9110 permits Connection to name additional hop-by-hop fields.  Read
    // those names before removing Connection itself.
    let connection_scoped_names = headers
        .get_all(CONNECTION)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(','))
        .filter_map(|name| HeaderName::from_bytes(name.trim().as_bytes()).ok())
        .collect::<Vec<_>>();
    for name in connection_scoped_names {
        headers.remove(name);
    }

    for name in [
        AUTHORIZATION,
        CONNECTION,
        COOKIE,
        HOST,
        PROXY_AUTHORIZATION,
        TE,
        TRAILER,
        TRANSFER_ENCODING,
        UPGRADE,
    ] {
        headers.remove(name);
    }
    for name in [
        "api-key",
        "keep-alive",
        "proxy-connection",
        "x-api-key",
        "x-goog-api-key",
        crate::constants::GATEWAY_HEADER,
        crate::constants::TRUSTED_AUTH_USER_ID_HEADER,
        crate::constants::TRUSTED_AUTH_API_KEY_ID_HEADER,
        crate::constants::TRUSTED_AUTH_BALANCE_HEADER,
        crate::constants::TRUSTED_AUTH_ACCESS_ALLOWED_HEADER,
        crate::constants::TRUSTED_ADMIN_USER_ID_HEADER,
        crate::constants::TRUSTED_ADMIN_USER_ROLE_HEADER,
        crate::constants::TRUSTED_ADMIN_SESSION_ID_HEADER,
        crate::constants::TRUSTED_ADMIN_MANAGEMENT_TOKEN_ID_HEADER,
    ] {
        headers.remove(name);
    }
    let websocket_managed_names = headers
        .keys()
        .filter(|name| name.as_str().starts_with("sec-websocket-"))
        .cloned()
        .collect::<Vec<_>>();
    for name in websocket_managed_names {
        headers.remove(name);
    }
    headers
}

fn websocket_admission_error_response(
    trace_id: &str,
    decision: &GatewayControlDecision,
    request_path: Option<&str>,
    error: RequestAdmissionError,
) -> Result<Response<Body>, GatewayError> {
    match error {
        RequestAdmissionError::Local(aether_runtime::ConcurrencyError::Saturated {
            gate,
            limit,
        })
        | RequestAdmissionError::Distributed(
            aether_runtime_state::RuntimeSemaphoreError::Saturated { gate, limit },
        )
        | RequestAdmissionError::Distributed(
            aether_runtime_state::RuntimeSemaphoreError::Unavailable { gate, limit, .. },
        ) => build_local_overloaded_response(trace_id, Some(decision), request_path, gate, limit),
        RequestAdmissionError::Local(aether_runtime::ConcurrencyError::Closed { gate }) => Err(
            GatewayError::Internal(format!("gateway concurrency gate {gate} is closed")),
        ),
        RequestAdmissionError::Distributed(
            aether_runtime_state::RuntimeSemaphoreError::InvalidConfiguration(message),
        ) => Err(GatewayError::Internal(message)),
    }
}

/// Connection-level access log fields which are independent of a protocol's
/// per-turn usage lifecycle.
#[derive(Clone, Copy)]
pub(crate) struct WebSocketConnectionLogSpec {
    pub(crate) opened_event_name: &'static str,
    pub(crate) closed_event_name: &'static str,
    pub(crate) opened_message: &'static str,
    pub(crate) closed_message: &'static str,
    pub(crate) execution_path: &'static str,
    pub(crate) provider_type: &'static str,
}

pub(crate) struct WebSocketConnectionLog {
    spec: WebSocketConnectionLogSpec,
    trace_id: String,
    remote_addr: SocketAddr,
    path: String,
    route_class: String,
    user_id: String,
    api_key_id: String,
    started_at: std::time::Instant,
}

impl WebSocketConnectionLog {
    pub(crate) fn new(context: &WebSocketRequestContext, spec: WebSocketConnectionLogSpec) -> Self {
        let auth_context = context.decision.auth_context.as_ref();
        Self {
            spec,
            trace_id: context.trace_id.clone(),
            remote_addr: context.remote_addr,
            path: websocket_log_path(context.uri.path()),
            route_class: context
                .decision
                .route_class
                .as_deref()
                .unwrap_or("ai_public")
                .to_string(),
            user_id: auth_context
                .map(|auth_context| auth_context.user_id.clone())
                .unwrap_or_else(|| "-".to_string()),
            api_key_id: auth_context
                .map(|auth_context| auth_context.api_key_id.clone())
                .unwrap_or_else(|| "-".to_string()),
            started_at: std::time::Instant::now(),
        }
    }

    pub(crate) fn log_opened(&self) {
        info!(
            event_name = self.spec.opened_event_name,
            log_type = "access",
            transport = WEBSOCKET_LOG_TRANSPORT,
            websocket = true,
            status = "upgraded",
            status_code = 101u16,
            trace_id = %self.trace_id,
            remote_addr = %self.remote_addr,
            method = "GET",
            path = %self.path,
            user_id = %self.user_id,
            api_key_id = %self.api_key_id,
            route_class = %self.route_class,
            execution_path = self.spec.execution_path,
            provider_type = self.spec.provider_type,
            message = self.spec.opened_message,
        );
    }
}

fn websocket_log_path(path: &str) -> String {
    if path
        .strip_prefix("/v1/live/")
        .is_some_and(|call_id| !call_id.is_empty())
    {
        "/v1/live/{call_id}".to_string()
    } else {
        path.to_string()
    }
}

impl Drop for WebSocketConnectionLog {
    fn drop(&mut self) {
        info!(
            event_name = self.spec.closed_event_name,
            log_type = "access",
            transport = WEBSOCKET_LOG_TRANSPORT,
            websocket = true,
            status = "closed",
            status_code = 101u16,
            trace_id = %self.trace_id,
            remote_addr = %self.remote_addr,
            method = "GET",
            path = %self.path,
            user_id = %self.user_id,
            api_key_id = %self.api_key_id,
            route_class = %self.route_class,
            execution_path = self.spec.execution_path,
            provider_type = self.spec.provider_type,
            elapsed_ms = self.started_at.elapsed().as_millis() as u64,
            message = self.spec.closed_message,
        );
    }
}

#[cfg(test)]
mod tests {
    use axum::http::header::{
        AUTHORIZATION, CONNECTION, COOKIE, HOST, ORIGIN, SEC_WEBSOCKET_KEY, UPGRADE, USER_AGENT,
    };
    use axum::http::{HeaderMap, HeaderValue, Uri};

    use super::{
        websocket_credential_carrier_is_allowed, websocket_log_path, websocket_planning_headers,
        websocket_planning_uri,
    };
    use crate::control::GatewayCredentialCarrier;

    #[test]
    fn planning_uri_removes_query_credentials_without_losing_safe_parameters() {
        let uri: Uri = "/v1/responses?key=downstream-secret&client_hint=a%20b&token=also-secret"
            .parse()
            .expect("request URI should parse");

        let sanitized = websocket_planning_uri(&uri);

        assert_eq!(sanitized.path(), "/v1/responses");
        assert_eq!(sanitized.query(), Some("client_hint=a+b"));
        assert!(!sanitized.to_string().contains("downstream-secret"));
        assert!(!sanitized.to_string().contains("also-secret"));
    }

    #[test]
    fn planning_uri_leaves_an_uncredentialed_query_byte_for_byte_unchanged() {
        let uri: Uri = "/v1/responses?client_hint=a%20b&empty="
            .parse()
            .expect("request URI should parse");

        assert_eq!(websocket_planning_uri(&uri), uri);
    }

    #[test]
    fn planning_headers_drop_client_auth_cookie_and_websocket_transport_state() {
        let mut headers = HeaderMap::new();
        headers.insert(
            AUTHORIZATION,
            HeaderValue::from_static("Bearer client-secret"),
        );
        headers.insert(COOKIE, HeaderValue::from_static("session=client-secret"));
        headers.insert("x-api-key", HeaderValue::from_static("client-secret"));
        headers.insert(HOST, HeaderValue::from_static("gateway.example"));
        headers.insert(
            CONNECTION,
            HeaderValue::from_static("keep-alive, Upgrade, x-connection-secret"),
        );
        headers.insert(UPGRADE, HeaderValue::from_static("websocket"));
        headers.insert(SEC_WEBSOCKET_KEY, HeaderValue::from_static("handshake-key"));
        headers.insert(
            "sec-websocket-future-field",
            HeaderValue::from_static("future-handshake-value"),
        );
        headers.insert(
            "x-connection-secret",
            HeaderValue::from_static("connection-secret"),
        );
        headers.insert(ORIGIN, HeaderValue::from_static("https://client.example"));
        headers.insert(USER_AGENT, HeaderValue::from_static("codex-cli/test"));
        headers.insert("x-client-hint", HeaderValue::from_static("safe"));

        let sanitized = websocket_planning_headers(headers);

        for name in [
            AUTHORIZATION.as_str(),
            COOKIE.as_str(),
            "x-api-key",
            HOST.as_str(),
            CONNECTION.as_str(),
            UPGRADE.as_str(),
            SEC_WEBSOCKET_KEY.as_str(),
            "sec-websocket-future-field",
            "x-connection-secret",
        ] {
            assert!(sanitized.get(name).is_none(), "{name} must not survive");
        }
        assert_eq!(
            sanitized.get(ORIGIN),
            Some(&HeaderValue::from_static("https://client.example"))
        );
        assert_eq!(
            sanitized.get(USER_AGENT),
            Some(&HeaderValue::from_static("codex-cli/test"))
        );
        assert_eq!(
            sanitized.get("x-client-hint"),
            Some(&HeaderValue::from_static("safe"))
        );
    }

    #[test]
    fn websocket_auth_requires_an_explicit_credential_instead_of_cookie_only() {
        assert!(!websocket_credential_carrier_is_allowed(Some(
            GatewayCredentialCarrier::CookieHeader
        )));
        for carrier in [
            None,
            Some(GatewayCredentialCarrier::AuthorizationBearer),
            Some(GatewayCredentialCarrier::XApiKey),
            Some(GatewayCredentialCarrier::ApiKey),
            Some(GatewayCredentialCarrier::XGoogApiKey),
            Some(GatewayCredentialCarrier::QueryKey),
        ] {
            assert!(websocket_credential_carrier_is_allowed(carrier));
        }
    }

    #[test]
    fn live_sideband_access_logs_do_not_retain_the_opaque_call_id() {
        assert_eq!(websocket_log_path("/v1/live"), "/v1/live");
        assert_eq!(
            websocket_log_path("/v1/live/rtc_secret_opaque"),
            "/v1/live/{call_id}"
        );
    }
}
