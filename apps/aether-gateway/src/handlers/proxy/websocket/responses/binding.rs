//! Identity of the physical upstream connection backing a Responses session.
//!
//! A Responses continuation carries state that lives on one provider socket.
//! Comparing only the selected key is therefore not sufficient: transport
//! settings, stable account headers, credentials, and the protocol adapter can
//! all change the connection that would receive the next event. Ordinary Codex
//! OAuth access-token refreshes retain the credential generation and therefore
//! do not unnecessarily replace an already-upgraded socket.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use aether_contracts::{ProxySnapshot, ResolvedTransportProfile};
use sha2::{Digest, Sha256};

use super::adapter::ResponsesWebSocketProtocolAdapter;
use crate::ai_serving::AiExecutionDecision;
use crate::handlers::proxy::websocket::transport::{
    websocket_handshake_headers, websocket_upstream_url,
};
use crate::orchestration::ResponsesWebSocketAdapter;

/// Stable, comparable identity for the actual WebSocket connection target.
///
/// The identity deliberately owns the normalized handshake values rather than
/// retaining a reference to the planner decision.  A later re-plan can then
/// be compared without accidentally ignoring a field that changes the
/// physical connection.
#[derive(Clone, PartialEq)]
pub(super) struct UpstreamBindingIdentity {
    adapter_kind: ResponsesWebSocketAdapter,
    provider_id: Option<String>,
    endpoint_id: Option<String>,
    key_id: Option<String>,
    upstream_url: String,
    handshake_headers: BTreeMap<String, String>,
    /// One-way identity for the credential generation used by this socket.
    ///
    /// A provider key id identifies a catalog row, not the secret currently
    /// stored in that row. Codex decisions carry a server-owned credential
    /// generation which is stable across access-token refreshes but rotates
    /// when the account/static/refresh credential is replaced. Other
    /// decisions conservatively fingerprint the effective authentication
    /// handshake values.
    credential_fingerprint: [u8; 32],
    proxy: Option<ProxySnapshot>,
    transport_profile: Option<ResolvedTransportProfile>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum UpstreamBindingIdentityError {
    MissingUpstreamUrl,
    InvalidUpstreamUrl,
    InvalidHandshakeHeaders,
}

impl UpstreamBindingIdentity {
    /// Builds an identity from the same normalized URL and headers used by
    /// the WebSocket transport client.
    pub(super) fn from_decision(
        adapter: &'static dyn ResponsesWebSocketProtocolAdapter,
        decision: &AiExecutionDecision,
    ) -> Result<Self, UpstreamBindingIdentityError> {
        let raw_url = decision
            .upstream_url
            .as_deref()
            .filter(|value| !value.trim().is_empty())
            .ok_or(UpstreamBindingIdentityError::MissingUpstreamUrl)?;
        let upstream_url = websocket_upstream_url(raw_url, "invalid")
            .map_err(|_| UpstreamBindingIdentityError::InvalidUpstreamUrl)?
            .to_string();

        let adapter_kind = adapter.kind();
        let headers = websocket_handshake_headers(&decision.provider_request_headers, "invalid")
            .map_err(|_| UpstreamBindingIdentityError::InvalidHandshakeHeaders)?;
        let authentication_header_names = authentication_header_names(decision);
        let mut handshake_headers = BTreeMap::new();
        let mut authentication_headers = BTreeMap::new();
        for (name, value) in &headers {
            let name = name.as_str().to_ascii_lowercase();
            let value = value
                .to_str()
                .map_err(|_| UpstreamBindingIdentityError::InvalidHandshakeHeaders)?;
            if authentication_header_names.contains(name.as_str()) {
                authentication_headers.insert(name, value.to_string());
            } else if !is_turn_scoped_handshake_header(adapter_kind, name.as_str()) {
                handshake_headers.insert(name, value.to_string());
            }
        }
        let credential_fingerprint =
            credential_binding_fingerprint(decision, &authentication_headers);

        Ok(Self {
            adapter_kind,
            provider_id: decision.provider_id.clone(),
            endpoint_id: decision.endpoint_id.clone(),
            key_id: decision.key_id.clone(),
            upstream_url,
            handshake_headers,
            credential_fingerprint,
            proxy: effective_proxy_snapshot(decision.proxy.as_ref()),
            transport_profile: decision.transport_profile.clone(),
        })
    }

    /// Returns only safe field/header names for operator diagnostics. Header
    /// values and credential fingerprints must never reach logs.
    pub(super) fn changed_field_names(&self, other: &Self) -> Vec<String> {
        let mut changed = Vec::new();
        if self.adapter_kind != other.adapter_kind {
            changed.push("adapter_kind".to_string());
        }
        if self.provider_id != other.provider_id {
            changed.push("provider_id".to_string());
        }
        if self.endpoint_id != other.endpoint_id {
            changed.push("endpoint_id".to_string());
        }
        if self.key_id != other.key_id {
            changed.push("key_id".to_string());
        }
        if self.upstream_url != other.upstream_url {
            changed.push("upstream_url".to_string());
        }
        for name in self
            .handshake_headers
            .keys()
            .chain(other.handshake_headers.keys())
            .collect::<BTreeSet<_>>()
        {
            if self.handshake_headers.get(name) != other.handshake_headers.get(name) {
                changed.push(format!("handshake_header:{name}"));
            }
        }
        if self.credential_fingerprint != other.credential_fingerprint {
            changed.push("credential_fingerprint".to_string());
        }
        if self.proxy != other.proxy {
            changed.push("proxy".to_string());
        }
        if self.transport_profile != other.transport_profile {
            changed.push("transport_profile".to_string());
        }
        changed
    }

    /// Returns a versioned, one-way identity suitable for a continuation
    /// registry. The digest covers every field used by `PartialEq`, including
    /// effective credential and transport state, without persisting header or
    /// credential values themselves.
    pub(super) fn continuation_fingerprint(&self) -> [u8; 32] {
        let mut digest = Sha256::new();
        digest.update(b"aether-responses-websocket-binding-v1");
        digest.update([match self.adapter_kind {
            ResponsesWebSocketAdapter::Standard => 0,
            ResponsesWebSocketAdapter::Codex => 1,
        }]);
        update_optional_string_digest(&mut digest, self.provider_id.as_deref());
        update_optional_string_digest(&mut digest, self.endpoint_id.as_deref());
        update_optional_string_digest(&mut digest, self.key_id.as_deref());
        update_string_digest(&mut digest, self.upstream_url.as_str());
        update_string_map_digest(&mut digest, &self.handshake_headers);
        digest.update(self.credential_fingerprint);
        update_proxy_digest(&mut digest, self.proxy.as_ref());
        update_transport_profile_digest(&mut digest, self.transport_profile.as_ref());
        digest.finalize().into()
    }

    pub(super) fn adapter_kind(&self) -> ResponsesWebSocketAdapter {
        self.adapter_kind
    }
}

fn update_bytes_digest(digest: &mut Sha256, value: &[u8]) {
    digest.update((value.len() as u64).to_be_bytes());
    digest.update(value);
}

fn update_string_digest(digest: &mut Sha256, value: &str) {
    update_bytes_digest(digest, value.as_bytes());
}

fn update_optional_string_digest(digest: &mut Sha256, value: Option<&str>) {
    match value {
        Some(value) => {
            digest.update([1]);
            update_string_digest(digest, value);
        }
        None => digest.update([0]),
    }
}

fn update_optional_bool_digest(digest: &mut Sha256, value: Option<bool>) {
    digest.update([match value {
        None => 0,
        Some(false) => 1,
        Some(true) => 2,
    }]);
}

fn update_string_map_digest(digest: &mut Sha256, values: &BTreeMap<String, String>) {
    digest.update((values.len() as u64).to_be_bytes());
    for (name, value) in values {
        update_string_digest(digest, name);
        update_string_digest(digest, value);
    }
}

fn update_optional_json_digest(digest: &mut Sha256, value: Option<&serde_json::Value>) {
    match value {
        Some(value) => {
            digest.update([1]);
            update_json_digest(digest, value);
        }
        None => digest.update([0]),
    }
}

fn update_json_digest(digest: &mut Sha256, value: &serde_json::Value) {
    use serde_json::Value;

    match value {
        Value::Null => digest.update(b"n"),
        Value::Bool(value) => digest.update(if *value { b"t" } else { b"f" }),
        Value::Number(value) => {
            digest.update(b"d");
            update_string_digest(digest, value.to_string().as_str());
        }
        Value::String(value) => {
            digest.update(b"s");
            update_string_digest(digest, value);
        }
        Value::Array(values) => {
            digest.update(b"[");
            digest.update((values.len() as u64).to_be_bytes());
            for value in values {
                update_json_digest(digest, value);
            }
            digest.update(b"]");
        }
        Value::Object(values) => {
            digest.update(b"{");
            digest.update((values.len() as u64).to_be_bytes());
            let mut keys = values.keys().collect::<Vec<_>>();
            keys.sort_unstable();
            for key in keys {
                update_string_digest(digest, key);
                update_json_digest(digest, &values[key]);
            }
            digest.update(b"}");
        }
    }
}

fn update_proxy_digest(digest: &mut Sha256, proxy: Option<&ProxySnapshot>) {
    let Some(proxy) = proxy else {
        digest.update([0]);
        return;
    };
    digest.update([1]);
    update_optional_bool_digest(digest, proxy.enabled);
    update_optional_string_digest(digest, proxy.mode.as_deref());
    update_optional_string_digest(digest, proxy.node_id.as_deref());
    update_optional_string_digest(digest, proxy.label.as_deref());
    update_optional_string_digest(digest, proxy.url.as_deref());
    update_optional_json_digest(digest, proxy.extra.as_ref());
}

fn update_transport_profile_digest(
    digest: &mut Sha256,
    profile: Option<&ResolvedTransportProfile>,
) {
    let Some(profile) = profile else {
        digest.update([0]);
        return;
    };
    digest.update([1]);
    update_string_digest(digest, profile.profile_id.as_str());
    update_string_digest(digest, profile.backend.as_str());
    update_string_digest(digest, profile.http_mode.as_str());
    update_string_digest(digest, profile.pool_scope.as_str());
    update_optional_json_digest(digest, profile.header_fingerprint.as_ref());
    update_optional_json_digest(digest, profile.extra.as_ref());
}

/// Excludes headers whose values identify one downstream request/turn rather
/// than the provider connection contract.
///
/// `x-trace-id` is generated by Aether's access middleware for every
/// downstream request. A cross-socket continuation therefore receives a new
/// value even when it revalidates to the exact same provider binding.
///
/// `x-codex-turn-metadata` likewise describes one logical response turn.
/// Fingerprint convergence intentionally rewrites its `turn_id` and timestamp
/// for every `response.create`, including a continuation on an already-upgraded
/// socket. The per-turn copy in `client_metadata` still travels in the
/// response.create body.
fn is_turn_scoped_handshake_header(adapter_kind: ResponsesWebSocketAdapter, name: &str) -> bool {
    name.eq_ignore_ascii_case(crate::constants::TRACE_ID_HEADER)
        || (adapter_kind == ResponsesWebSocketAdapter::Codex
            && name.eq_ignore_ascii_case("x-codex-turn-metadata"))
}

/// Header names that carry credentials in the provider handshake.  The
/// planner's explicit `auth_header` extends this list for provider-specific
/// schemes; unknown headers remain part of the stable handshake identity.
fn authentication_header_names(decision: &AiExecutionDecision) -> BTreeSet<String> {
    let mut names = BTreeSet::from([
        "authorization".to_string(),
        "proxy-authorization".to_string(),
        "x-api-key".to_string(),
        "api-key".to_string(),
        "x-goog-api-key".to_string(),
        "x-azure-api-key".to_string(),
    ]);
    if let Some(name) = decision
        .auth_header
        .as_deref()
        .map(str::trim)
        .filter(|name| !name.is_empty())
    {
        names.insert(name.to_ascii_lowercase());
    }
    names
}

fn fingerprint_headers(headers: &BTreeMap<String, String>) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(b"aether-responses-websocket-auth-headers-v1");
    for (name, value) in headers {
        hasher.update((name.len() as u64).to_be_bytes());
        hasher.update(name.as_bytes());
        hasher.update((value.len() as u64).to_be_bytes());
        hasher.update(value.as_bytes());
    }
    hasher.finalize().into()
}

/// Returns the non-secret credential identity represented by a planner
/// decision. The generation is emitted by Aether's trusted Codex planner from
/// provider-key metadata; it is not sourced from the downstream request.
fn credential_binding_fingerprint(
    decision: &AiExecutionDecision,
    authentication_headers: &BTreeMap<String, String>,
) -> [u8; 32] {
    if decision
        .provider_type
        .as_deref()
        .is_some_and(|provider_type| provider_type.trim().eq_ignore_ascii_case("codex"))
    {
        if let Some(generation) = decision
            .report_context
            .as_ref()
            .and_then(|context| context.get("codex_credential_generation"))
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|generation| !generation.is_empty())
        {
            let mut hasher = Sha256::new();
            hasher.update(b"aether-responses-websocket-codex-credential-generation-v1");
            hasher.update((generation.len() as u64).to_be_bytes());
            hasher.update(generation.as_bytes());
            // Only a planner-owned Codex bearer access token is expected to
            // rotate without changing credential generation. Compare the
            // effective handshake value with the decision's original auth
            // value: auth-config/routing/header overrides change only the
            // former and therefore must force a rebind.
            let stable_authentication_headers = authentication_headers
                .iter()
                .filter(|(name, value)| {
                    !is_planner_owned_codex_bearer(decision, name.as_str(), value.as_str())
                })
                .map(|(name, value)| (name.clone(), value.clone()))
                .collect::<BTreeMap<_, _>>();
            hasher.update(fingerprint_headers(&stable_authentication_headers));
            return hasher.finalize().into();
        }
    }

    // Fail closed when no trusted generation is available. Rebinding after an
    // access-token change is preferable to sending a continuation over a
    // socket authenticated with a credential that may have been replaced.
    fingerprint_headers(authentication_headers)
}

fn is_planner_owned_codex_bearer(
    decision: &AiExecutionDecision,
    name: &str,
    effective_value: &str,
) -> bool {
    name.eq_ignore_ascii_case("authorization")
        && decision
            .auth_header
            .as_deref()
            .is_some_and(|header| header.eq_ignore_ascii_case(name))
        && decision.auth_value.as_deref() == Some(effective_value)
        && effective_value
            .get(.."bearer ".len())
            .is_some_and(|scheme| scheme.eq_ignore_ascii_case("bearer "))
}

/// Normalize only values that are provably direct transport.  Keep node/tunnel
/// fields even though the current WebSocket builder rejects those proxies: a
/// re-plan must not accidentally reuse an already-bound direct socket for a
/// decision that selected a different proxy topology.
fn effective_proxy_snapshot(proxy: Option<&ProxySnapshot>) -> Option<ProxySnapshot> {
    let proxy = proxy?;
    if proxy.enabled == Some(false) {
        return None;
    }
    let mut normalized = proxy.clone();
    normalized.url = normalized
        .url
        .take()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty());
    normalized.mode = normalized
        .mode
        .take()
        .map(|value| value.trim().to_ascii_lowercase())
        .filter(|value| !value.is_empty());
    normalized.node_id = normalized
        .node_id
        .take()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty());
    normalized.label = normalized
        .label
        .take()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty());

    let has_effective_proxy = normalized.url.is_some()
        || normalized.node_id.is_some()
        || normalized.mode.is_some()
        || normalized.extra.is_some();
    has_effective_proxy.then_some(normalized)
}

impl fmt::Debug for UpstreamBindingIdentity {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("UpstreamBindingIdentity")
            .field("adapter_kind", &self.adapter_kind)
            .field("provider_id", &self.provider_id)
            .field("endpoint_id", &self.endpoint_id)
            .field("key_id", &self.key_id)
            .field("upstream_url", &self.upstream_url)
            .field(
                "handshake_header_names",
                &self.handshake_headers.keys().collect::<Vec<_>>(),
            )
            .field("proxy_configured", &self.proxy.is_some())
            .field(
                "transport_profile_id",
                &self
                    .transport_profile
                    .as_ref()
                    .map(|profile| profile.profile_id.as_str()),
            )
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use serde_json::json;

    use super::{UpstreamBindingIdentity, UpstreamBindingIdentityError};
    use crate::ai_serving::AiExecutionDecision;
    use crate::handlers::proxy::websocket::responses::adapter::resolve_responses_websocket_adapter;
    use crate::orchestration::ResponsesWebSocketAdapter;

    fn decision() -> AiExecutionDecision {
        AiExecutionDecision {
            action: "execute".to_string(),
            decision_kind: None,
            execution_strategy: None,
            conversion_mode: None,
            request_id: Some("request-1".to_string()),
            candidate_id: Some("candidate-1".to_string()),
            provider_name: Some("provider".to_string()),
            provider_type: Some("openai".to_string()),
            provider_id: Some("provider-1".to_string()),
            endpoint_id: Some("endpoint-1".to_string()),
            key_id: Some("key-1".to_string()),
            upstream_base_url: Some("https://api.example.test".to_string()),
            upstream_url: Some("https://api.example.test/v1/responses".to_string()),
            provider_request_method: Some("POST".to_string()),
            auth_header: Some("authorization".to_string()),
            auth_value: Some("Bearer secret".to_string()),
            provider_api_format: Some("openai:responses".to_string()),
            client_api_format: Some("openai:responses".to_string()),
            provider_contract: None,
            client_contract: None,
            model_name: Some("gpt-5.6-sol".to_string()),
            mapped_model: None,
            prompt_cache_key: None,
            extra_headers: BTreeMap::new(),
            provider_request_headers: BTreeMap::from([
                ("Authorization".to_string(), "Bearer secret".to_string()),
                ("X-Client".to_string(), "aether".to_string()),
                ("Connection".to_string(), "keep-alive".to_string()),
            ]),
            provider_request_body: Some(json!({"model": "gpt-5.6-sol"})),
            provider_request_body_base64: None,
            content_type: Some("application/json".to_string()),
            content_encoding: None,
            request_gzip: None,
            proxy: None,
            transport_profile: None,
            timeouts: None,
            upstream_is_stream: true,
            report_kind: None,
            report_context: None,
            auth_context: None,
        }
    }

    #[test]
    fn identity_normalizes_url_and_hop_by_hop_headers() {
        let adapter = resolve_responses_websocket_adapter(ResponsesWebSocketAdapter::Standard);
        let identity = UpstreamBindingIdentity::from_decision(adapter, &decision()).unwrap();

        assert_eq!(identity.upstream_url, "wss://api.example.test/v1/responses");
        assert_eq!(
            identity.handshake_headers,
            BTreeMap::from([("x-client".to_string(), "aether".to_string())])
        );
    }

    #[test]
    fn request_trace_id_does_not_change_standard_or_codex_binding_identity() {
        for adapter_kind in [
            ResponsesWebSocketAdapter::Standard,
            ResponsesWebSocketAdapter::Codex,
        ] {
            let adapter = resolve_responses_websocket_adapter(adapter_kind);
            let mut first = decision();
            if adapter_kind == ResponsesWebSocketAdapter::Codex {
                first.provider_type = Some("codex".to_string());
                first.report_context = Some(json!({
                    "codex_credential_generation": "credential-generation-1"
                }));
            }
            first
                .provider_request_headers
                .insert("x-trace-id".to_string(), "request-1".to_string());
            let first_identity = UpstreamBindingIdentity::from_decision(adapter, &first).unwrap();
            assert!(!first_identity.handshake_headers.contains_key("x-trace-id"));

            let mut continuation = first;
            continuation
                .provider_request_headers
                .insert("x-trace-id".to_string(), "request-2".to_string());
            let continuation_identity =
                UpstreamBindingIdentity::from_decision(adapter, &continuation).unwrap();
            assert_eq!(first_identity, continuation_identity);
            assert_eq!(
                first_identity.continuation_fingerprint(),
                continuation_identity.continuation_fingerprint()
            );

            continuation
                .provider_request_headers
                .insert("x-correlation-id".to_string(), "changed".to_string());
            let changed_identity =
                UpstreamBindingIdentity::from_decision(adapter, &continuation).unwrap();
            assert_ne!(first_identity, changed_identity);
            assert_eq!(
                first_identity.changed_field_names(&changed_identity),
                vec!["handshake_header:x-correlation-id".to_string()]
            );
        }
    }

    #[test]
    fn identity_changes_when_physical_binding_changes() {
        let adapter = resolve_responses_websocket_adapter(ResponsesWebSocketAdapter::Standard);
        let base = decision();
        let identity = UpstreamBindingIdentity::from_decision(adapter, &base).unwrap();

        let codex_adapter = resolve_responses_websocket_adapter(ResponsesWebSocketAdapter::Codex);
        assert_ne!(
            identity,
            UpstreamBindingIdentity::from_decision(codex_adapter, &base).unwrap()
        );

        for mutate in [
            |decision: &mut AiExecutionDecision| {
                decision.key_id = Some("key-2".to_string());
            },
            |decision: &mut AiExecutionDecision| {
                decision.upstream_url = Some("https://other.example.test/v1/responses".to_string());
            },
            |decision: &mut AiExecutionDecision| {
                decision
                    .provider_request_headers
                    .insert("X-Client".to_string(), "other".to_string());
            },
            |decision: &mut AiExecutionDecision| {
                decision.proxy = Some(aether_contracts::ProxySnapshot {
                    enabled: Some(true),
                    url: Some("http://proxy.example.test:8080".to_string()),
                    ..Default::default()
                });
            },
            |decision: &mut AiExecutionDecision| {
                decision.transport_profile = Some(aether_contracts::ResolvedTransportProfile {
                    profile_id: "chrome136".to_string(),
                    ..Default::default()
                });
            },
        ] {
            let mut changed = base.clone();
            mutate(&mut changed);
            let changed_identity =
                UpstreamBindingIdentity::from_decision(adapter, &changed).unwrap();
            assert_ne!(identity, changed_identity);
        }

        let mut static_secret_rotated = base.clone();
        static_secret_rotated
            .provider_request_headers
            .insert("Authorization".to_string(), "Bearer rotated".to_string());
        assert_ne!(
            identity,
            UpstreamBindingIdentity::from_decision(adapter, &static_secret_rotated).unwrap()
        );
    }

    #[test]
    fn continuation_fingerprint_changes_with_binding_and_is_not_secret_text() {
        let adapter = resolve_responses_websocket_adapter(ResponsesWebSocketAdapter::Standard);
        let base = decision();
        let identity = UpstreamBindingIdentity::from_decision(adapter, &base).unwrap();
        let mut changed = base;
        changed
            .provider_request_headers
            .insert("X-Client".to_string(), "different-client".to_string());
        let changed_identity = UpstreamBindingIdentity::from_decision(adapter, &changed).unwrap();
        assert_ne!(
            identity.continuation_fingerprint(),
            changed_identity.continuation_fingerprint()
        );
        let digest = format!("{:?}", identity.continuation_fingerprint());
        assert!(!digest.contains("secret"));
        assert!(!digest.contains("api.example"));
    }

    #[test]
    fn stable_key_identity_rejects_custom_static_auth_value_rotation() {
        let adapter = resolve_responses_websocket_adapter(ResponsesWebSocketAdapter::Standard);
        let mut base = decision();
        base.auth_header = Some("X-Provider-Token".to_string());
        base.provider_request_headers.remove("Authorization");
        base.provider_request_headers.insert(
            "X-Provider-Token".to_string(),
            "provider-token-1".to_string(),
        );
        let identity = UpstreamBindingIdentity::from_decision(adapter, &base).unwrap();
        assert!(!identity.handshake_headers.contains_key("x-provider-token"));

        let mut rotated = base;
        rotated.provider_request_headers.insert(
            "X-Provider-Token".to_string(),
            "provider-token-2".to_string(),
        );
        assert_ne!(
            identity,
            UpstreamBindingIdentity::from_decision(adapter, &rotated).unwrap()
        );
    }

    #[test]
    fn codex_access_token_refresh_reuses_the_same_credential_generation() {
        let adapter = resolve_responses_websocket_adapter(ResponsesWebSocketAdapter::Codex);
        let mut first = decision();
        first.provider_type = Some("codex".to_string());
        first.report_context = Some(json!({
            "codex_credential_generation": "credential-generation-1"
        }));
        let first_identity = UpstreamBindingIdentity::from_decision(adapter, &first).unwrap();

        let mut access_token_refreshed = first;
        access_token_refreshed.auth_value = Some("Bearer refreshed-access-token".to_string());
        access_token_refreshed.provider_request_headers.insert(
            "Authorization".to_string(),
            "Bearer refreshed-access-token".to_string(),
        );
        assert_eq!(
            first_identity,
            UpstreamBindingIdentity::from_decision(adapter, &access_token_refreshed).unwrap()
        );
    }

    #[test]
    fn codex_turn_metadata_changes_do_not_change_the_physical_binding() {
        let adapter = resolve_responses_websocket_adapter(ResponsesWebSocketAdapter::Codex);
        let mut first = decision();
        first.provider_type = Some("codex".to_string());
        first.report_context = Some(json!({
            "codex_credential_generation": "credential-generation-1"
        }));
        first.provider_request_headers.insert(
            "x-codex-turn-metadata".to_string(),
            json!({
                "session_id": "session-1",
                "thread_id": "thread-1",
                "turn_id": "turn-1",
                "turn_started_at_unix_ms": 1
            })
            .to_string(),
        );
        let first_identity = UpstreamBindingIdentity::from_decision(adapter, &first).unwrap();
        assert!(!first_identity
            .handshake_headers
            .contains_key("x-codex-turn-metadata"));

        let mut continuation = first;
        continuation.provider_request_headers.insert(
            "x-codex-turn-metadata".to_string(),
            json!({
                "session_id": "session-1",
                "thread_id": "thread-1",
                "turn_id": "turn-2",
                "turn_started_at_unix_ms": 2
            })
            .to_string(),
        );
        assert_eq!(
            first_identity,
            UpstreamBindingIdentity::from_decision(adapter, &continuation).unwrap()
        );
    }

    #[test]
    fn unknown_codex_headers_remain_part_of_the_binding_identity() {
        let codex_adapter = resolve_responses_websocket_adapter(ResponsesWebSocketAdapter::Codex);
        let mut first = decision();
        first.provider_type = Some("codex".to_string());
        first.report_context = Some(json!({
            "codex_credential_generation": "credential-generation-1"
        }));
        first.provider_request_headers.insert(
            "x-codex-turn-metadata".to_string(),
            r#"{"turn_id":"turn-1"}"#.to_string(),
        );
        let first_identity = UpstreamBindingIdentity::from_decision(codex_adapter, &first).unwrap();

        let mut changed = first;
        changed
            .provider_request_headers
            .insert("x-codex-window-id".to_string(), "window-2".to_string());
        let changed_identity =
            UpstreamBindingIdentity::from_decision(codex_adapter, &changed).unwrap();
        assert_ne!(first_identity, changed_identity);
        assert_eq!(
            first_identity.changed_field_names(&changed_identity),
            vec!["handshake_header:x-codex-window-id".to_string()]
        );

        let standard_adapter =
            resolve_responses_websocket_adapter(ResponsesWebSocketAdapter::Standard);
        let standard_identity =
            UpstreamBindingIdentity::from_decision(standard_adapter, &changed).unwrap();
        assert!(standard_identity
            .handshake_headers
            .contains_key("x-codex-turn-metadata"));
    }

    #[test]
    fn codex_authorization_override_changes_binding_with_the_same_generation() {
        let adapter = resolve_responses_websocket_adapter(ResponsesWebSocketAdapter::Codex);
        let mut first = decision();
        first.provider_type = Some("codex".to_string());
        first.report_context = Some(json!({
            "codex_credential_generation": "credential-generation-1"
        }));
        let first_identity = UpstreamBindingIdentity::from_decision(adapter, &first).unwrap();

        // The planner-owned auth value remains unchanged while an effective
        // auth-config/header override replaces the actual handshake value.
        first.provider_request_headers.insert(
            "Authorization".to_string(),
            "Bearer endpoint-override".to_string(),
        );
        assert_ne!(
            first_identity,
            UpstreamBindingIdentity::from_decision(adapter, &first).unwrap()
        );
    }

    #[test]
    fn codex_credential_replacement_changes_binding_for_the_same_key_id() {
        let adapter = resolve_responses_websocket_adapter(ResponsesWebSocketAdapter::Codex);
        let mut first = decision();
        first.provider_type = Some("codex".to_string());
        first.report_context = Some(json!({
            "codex_credential_generation": "credential-generation-1"
        }));
        let first_identity = UpstreamBindingIdentity::from_decision(adapter, &first).unwrap();

        let mut replaced = first;
        replaced.provider_request_headers.insert(
            "Authorization".to_string(),
            "Bearer replacement-access-token".to_string(),
        );
        replaced.report_context = Some(json!({
            "codex_credential_generation": "credential-generation-2"
        }));
        assert_ne!(
            first_identity,
            UpstreamBindingIdentity::from_decision(adapter, &replaced).unwrap()
        );
    }

    #[test]
    fn codex_custom_auth_rotation_changes_binding_with_the_same_generation() {
        let adapter = resolve_responses_websocket_adapter(ResponsesWebSocketAdapter::Codex);
        let mut first = decision();
        first.provider_type = Some("codex".to_string());
        first.auth_header = Some("X-Provider-Token".to_string());
        first.provider_request_headers.insert(
            "X-Provider-Token".to_string(),
            "provider-token-1".to_string(),
        );
        first.report_context = Some(json!({
            "codex_credential_generation": "credential-generation-1"
        }));
        let first_identity = UpstreamBindingIdentity::from_decision(adapter, &first).unwrap();

        first.provider_request_headers.insert(
            "X-Provider-Token".to_string(),
            "provider-token-2".to_string(),
        );
        assert_ne!(
            first_identity,
            UpstreamBindingIdentity::from_decision(adapter, &first).unwrap()
        );
    }

    #[test]
    fn missing_codex_credential_generation_fails_closed_on_auth_rotation() {
        let adapter = resolve_responses_websocket_adapter(ResponsesWebSocketAdapter::Codex);
        let mut first = decision();
        first.provider_type = Some("codex".to_string());
        let first_identity = UpstreamBindingIdentity::from_decision(adapter, &first).unwrap();

        first.provider_request_headers.insert(
            "Authorization".to_string(),
            "Bearer possibly-replaced-credential".to_string(),
        );
        assert_ne!(
            first_identity,
            UpstreamBindingIdentity::from_decision(adapter, &first).unwrap()
        );
    }

    #[test]
    fn disabled_proxy_is_equivalent_to_direct_transport() {
        let adapter = resolve_responses_websocket_adapter(ResponsesWebSocketAdapter::Standard);
        let direct = decision();
        let direct_identity = UpstreamBindingIdentity::from_decision(adapter, &direct).unwrap();
        let mut explicitly_disabled = direct;
        explicitly_disabled.proxy = Some(aether_contracts::ProxySnapshot {
            enabled: Some(false),
            url: Some("http://ignored.example.test:8080".to_string()),
            ..Default::default()
        });

        assert_eq!(
            direct_identity,
            UpstreamBindingIdentity::from_decision(adapter, &explicitly_disabled).unwrap()
        );
    }

    #[test]
    fn identity_rejects_missing_or_invalid_connection_fields() {
        let adapter = resolve_responses_websocket_adapter(ResponsesWebSocketAdapter::Standard);
        let mut missing = decision();
        missing.upstream_url = None;
        assert_eq!(
            UpstreamBindingIdentity::from_decision(adapter, &missing),
            Err(UpstreamBindingIdentityError::MissingUpstreamUrl)
        );

        let mut invalid = decision();
        invalid.upstream_url = Some("file:///tmp/responses".to_string());
        assert_eq!(
            UpstreamBindingIdentity::from_decision(adapter, &invalid),
            Err(UpstreamBindingIdentityError::InvalidUpstreamUrl)
        );
    }
}
