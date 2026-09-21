use aether_contracts::{
    ExecutionTimeouts, ProxySnapshot, ResolvedTransportProfile, TRANSPORT_BACKEND_BROWSER_WREQ,
    TRANSPORT_BACKEND_REQWEST_RUSTLS, TRANSPORT_HTTP_MODE_AUTO, TRANSPORT_POOL_SCOPE_KEY,
};
use async_trait::async_trait;
use serde_json::{json, Map, Value};
use tracing::warn;

use crate::claude_code::{
    current_claude_code_transport_identity_profile,
    oauth_control_plane_tls_profile_for_provider_type, resolve_claude_code_tls_emulation_spec,
    selectable_tls_emulation_profiles_for_provider_type, CHATGPT_COM_CHROME_BROWSER_PROFILE,
    CHATGPT_COM_CHROME_TLS_PROFILE,
};
use crate::grok::grok_browser_resolved_transport_profile_from_auth_config;

use super::snapshot::GatewayProviderTransportSnapshot;

const TUNNEL_BASE_URL_EXTRA_KEY: &str = "tunnel_base_url";
/// `ResolvedTransportProfile.extra` 里内置 TLS 仿真 profile 的 id（P5）。
pub const TRANSPORT_EMULATION_PROFILE_EXTRA_KEY: &str = "emulation_profile";
/// `ResolvedTransportProfile.extra` 里附带的探针摘要（来自 Key `upstream_metadata.tls_probe`，
/// 只在探针记录的 profile 与当前 profile 一致时附带）。
pub const TRANSPORT_TLS_PROBE_EXTRA_KEY: &str = "tls_probe";
/// Key `upstream_metadata` 里探针结果的命名空间。
pub const TLS_PROBE_UPSTREAM_METADATA_NAMESPACE: &str = "tls_probe";
const TUNNEL_OWNER_INSTANCE_ID_EXTRA_KEY: &str = "tunnel_owner_instance_id";
const TUNNEL_OWNER_OBSERVED_AT_EXTRA_KEY: &str = "tunnel_owner_observed_at_unix_secs";
const DEFAULT_PROVIDER_STREAM_FIRST_BYTE_TIMEOUT_SECS: f64 = 30.0;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransportTunnelAttachmentOwner {
    pub gateway_instance_id: String,
    pub relay_base_url: String,
    pub observed_at_unix_secs: u64,
}

#[async_trait]
pub trait TransportTunnelAffinityLookup: Send + Sync {
    async fn lookup_tunnel_attachment_owner(
        &self,
        node_id: &str,
    ) -> Result<Option<TransportTunnelAttachmentOwner>, String>;
}

pub fn resolve_transport_execution_timeouts(
    transport: &GatewayProviderTransportSnapshot,
) -> Option<ExecutionTimeouts> {
    Some(ExecutionTimeouts {
        total_ms: transport
            .provider
            .request_timeout_secs
            .filter(|value| value.is_finite() && *value > 0.0)
            .map(timeout_secs_to_ms),
        first_byte_ms: Some(timeout_secs_to_ms(
            transport
                .provider
                .stream_first_byte_timeout_secs
                .filter(|value| value.is_finite() && *value > 0.0)
                .unwrap_or(DEFAULT_PROVIDER_STREAM_FIRST_BYTE_TIMEOUT_SECS),
        )),
        ..ExecutionTimeouts::default()
    })
}

fn timeout_secs_to_ms(secs: f64) -> u64 {
    ((secs * 1000.0).round() as u64).max(1)
}

pub fn resolve_transport_proxy_snapshot(
    transport: &GatewayProviderTransportSnapshot,
) -> Option<ProxySnapshot> {
    let raw = effective_proxy_config(transport)?;
    proxy_snapshot_from_value(raw)
}

pub async fn resolve_transport_proxy_snapshot_with_tunnel_affinity(
    lookup: &dyn TransportTunnelAffinityLookup,
    transport: &GatewayProviderTransportSnapshot,
) -> Option<ProxySnapshot> {
    let mut snapshot = resolve_transport_proxy_snapshot(transport)?;
    let Some(node_id) = snapshot
        .node_id
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    else {
        return Some(snapshot);
    };

    let owner = match lookup.lookup_tunnel_attachment_owner(node_id).await {
        Ok(owner) => owner,
        Err(error) => {
            warn!(error = %error, node_id = node_id, "failed to load tunnel attachment owner");
            None
        }
    };
    let Some(owner) = owner else {
        return Some(snapshot);
    };

    let mut extra = snapshot
        .extra
        .take()
        .and_then(|value| value.as_object().cloned())
        .unwrap_or_default();
    let configured_tunnel_base_url = extra
        .get(TUNNEL_BASE_URL_EXTRA_KEY)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned);
    if configured_tunnel_base_url.is_none() {
        extra.insert(
            TUNNEL_BASE_URL_EXTRA_KEY.to_string(),
            Value::String(owner.relay_base_url.clone()),
        );
    }
    extra.insert(
        TUNNEL_OWNER_INSTANCE_ID_EXTRA_KEY.to_string(),
        Value::String(owner.gateway_instance_id),
    );
    extra.insert(
        TUNNEL_OWNER_OBSERVED_AT_EXTRA_KEY.to_string(),
        json!(owner.observed_at_unix_secs),
    );
    snapshot.extra = Some(Value::Object(extra));
    Some(snapshot)
}

pub fn transport_proxy_is_locally_supported(transport: &GatewayProviderTransportSnapshot) -> bool {
    let has_configured_proxy = transport.provider.proxy.is_some()
        || transport.endpoint.proxy.is_some()
        || transport.key.proxy.is_some();
    if !has_configured_proxy {
        return true;
    }

    let Some(snapshot) = resolve_transport_proxy_snapshot(transport) else {
        return false;
    };

    if snapshot.enabled == Some(false) {
        return true;
    }

    let has_proxy_url = snapshot
        .url
        .as_deref()
        .map(str::trim)
        .is_some_and(|value| !value.is_empty());
    let has_node_id = snapshot
        .node_id
        .as_deref()
        .map(str::trim)
        .is_some_and(|value| !value.is_empty());
    // Nodes validate their own backend support when receiving the profile.
    // Keep the configured route available so an unsupported backend is reported
    // by that node without dropping the profile or bypassing the tunnel.
    has_proxy_url || has_node_id
}

pub fn resolve_transport_profile_id(
    transport: &GatewayProviderTransportSnapshot,
) -> Option<String> {
    resolve_transport_profile(transport).map(|profile| profile.profile_id)
}

pub fn resolve_transport_profile(
    transport: &GatewayProviderTransportSnapshot,
) -> Option<ResolvedTransportProfile> {
    let configured = resolve_configured_transport_profile(
        &transport.provider.provider_type,
        transport.provider.config.as_ref(),
        transport.key.fingerprint.as_ref(),
    );
    if configured.is_some() || transport_profile_is_configured(transport) {
        return configured.map(|profile| {
            attach_tls_probe_summary(profile, transport.key.upstream_metadata.as_ref())
        });
    }

    resolve_claude_code_transport_profile(transport)
        .or_else(|| resolve_grok_browser_transport_profile(transport))
}

/// 供应商 / Key 显式选择的 TLS 仿真 profile id（只认内置的三个；未配置或配置为其它
/// 字符串时返回 `None`）。
pub fn configured_tls_emulation_profile_id(
    transport: &GatewayProviderTransportSnapshot,
) -> Option<&'static str> {
    resolve_transport_profile(transport).and_then(|profile| {
        transport_profile_emulation_id(&profile)
            .and_then(|id| resolve_claude_code_tls_emulation_spec(&id))
            .map(|spec| spec.id)
    })
}

/// 从 `fingerprint` 对象（供应商 `config.fingerprint` 或 Key `fingerprint`）里取
/// `transport_profile` 的 id：字符串形态或 `{profile_id}` / `{id}` 对象形态。
pub fn configured_transport_profile_id_from_fingerprint(
    fingerprint: Option<&Value>,
) -> Option<String> {
    let value = fingerprint?.get("transport_profile")?;
    value
        .as_str()
        .or_else(|| {
            value
                .get("profile_id")
                .or_else(|| value.get("id"))
                .and_then(Value::as_str)
        })
        .map(str::trim)
        .filter(|id| !id.is_empty())
        .map(ToOwned::to_owned)
}

/// 管理端展示用的探针摘要（只读）：只从 `upstream_metadata.tls_probe` 取字符串 / 数值 /
/// 头名数组字段，`observed != true` 时返回 `None`。
pub fn tls_probe_summary_from_upstream_metadata(
    upstream_metadata: Option<&Value>,
) -> Option<Value> {
    let probe = upstream_metadata?
        .get(TLS_PROBE_UPSTREAM_METADATA_NAMESPACE)?
        .as_object()?;
    if probe.get("observed").and_then(Value::as_bool) != Some(true) {
        return None;
    }
    let mut summary = Map::new();
    for key in [
        "probe_url",
        "emulation_profile",
        "profile_id",
        "backend",
        "tls_stack",
        "http_version",
        "ja3",
        "ja3_hash",
        "ja4",
        "peetprint",
        "akamai_fingerprint",
        "tls_version_negotiated",
    ] {
        if let Some(value) = probe.get(key) {
            if value.is_string() || value.is_null() {
                summary.insert(key.to_string(), value.clone());
            }
        }
    }
    if let Some(value) = probe.get("probed_at_unix_secs").and_then(Value::as_u64) {
        summary.insert("probed_at_unix_secs".to_string(), json!(value));
    }
    if let Some(value) = probe.get("http1_header_order").and_then(Value::as_array) {
        summary.insert(
            "http1_header_order".to_string(),
            Value::Array(
                value
                    .iter()
                    .filter(|item| item.is_string())
                    .cloned()
                    .collect(),
            ),
        );
    }
    summary.insert("observed".to_string(), Value::Bool(true));
    Some(Value::Object(summary))
}

/// 从已解析的 profile 取内置仿真 id（`extra.emulation_profile`）。
pub fn transport_profile_emulation_id(profile: &ResolvedTransportProfile) -> Option<String> {
    profile
        .extra
        .as_ref()
        .and_then(|extra| extra.get(TRANSPORT_EMULATION_PROFILE_EXTRA_KEY))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
}

/// OAuth 控制面（token 交换 / 刷新 / profile / roles）使用的传输 profile：只有当该 Key /
/// 供应商显式选择了 TLS 仿真 profile 时，才按供应商类型换成对应的控制面 profile
/// （claude_code → `claude_code_oauth_control_plane`，codex → `chatgpt_com_chrome`）；
/// 否则返回 `None`，保持 reqwest 默认行为。
pub fn resolve_oauth_control_plane_transport_profile(
    transport: &GatewayProviderTransportSnapshot,
) -> Option<ResolvedTransportProfile> {
    configured_tls_emulation_profile_id(transport)?;
    resolve_oauth_control_plane_transport_profile_for_provider_type(
        &transport.provider.provider_type,
    )
}

/// 与 [`resolve_oauth_control_plane_transport_profile`] 相同，但输入是原始配置（供 OAuth
/// 交换阶段还没有 Key 快照时使用）：`provider_config.fingerprint.transport_profile` 或
/// `key_fingerprint.transport_profile` 选了仿真 profile 才启用。
pub fn resolve_oauth_control_plane_transport_profile_from_configs(
    provider_type: &str,
    provider_config: Option<&Value>,
    key_fingerprint: Option<&Value>,
) -> Option<ResolvedTransportProfile> {
    let configured =
        resolve_configured_transport_profile(provider_type, provider_config, key_fingerprint)?;
    transport_profile_emulation_id(&configured)
        .and_then(|id| resolve_claude_code_tls_emulation_spec(&id))?;
    resolve_oauth_control_plane_transport_profile_for_provider_type(provider_type)
}

fn resolve_oauth_control_plane_transport_profile_for_provider_type(
    provider_type: &str,
) -> Option<ResolvedTransportProfile> {
    let profile_id = oauth_control_plane_tls_profile_for_provider_type(provider_type)?;
    builtin_tls_emulation_transport_profile(profile_id)
}

fn resolve_configured_transport_profile(
    provider_type: &str,
    provider_config: Option<&Value>,
    key_fingerprint: Option<&Value>,
) -> Option<ResolvedTransportProfile> {
    let profile = resolve_transport_profile_from_fingerprint(key_fingerprint)
        .or_else(|| resolve_transport_profile_from_provider_config(provider_config))?;
    if let Some(spec) = transport_profile_emulation_id(&profile)
        .and_then(|id| resolve_claude_code_tls_emulation_spec(&id))
    {
        if !selectable_tls_emulation_profiles_for_provider_type(provider_type).contains(&spec.id) {
            return None;
        }
    }
    Some(profile)
}

/// 内置 TLS 仿真 profile → `ResolvedTransportProfile`（backend=browser_wreq）。
pub fn builtin_tls_emulation_transport_profile(
    profile_id: &str,
) -> Option<ResolvedTransportProfile> {
    let spec = resolve_claude_code_tls_emulation_spec(profile_id)?;
    let mut extra = Map::new();
    extra.insert(
        TRANSPORT_EMULATION_PROFILE_EXTRA_KEY.to_string(),
        Value::String(spec.id.to_string()),
    );
    if spec.id == CHATGPT_COM_CHROME_TLS_PROFILE {
        extra.insert(
            "browser_profile".to_string(),
            Value::String(CHATGPT_COM_CHROME_BROWSER_PROFILE.to_string()),
        );
    }
    Some(ResolvedTransportProfile {
        profile_id: spec.id.to_string(),
        backend: TRANSPORT_BACKEND_BROWSER_WREQ.to_string(),
        http_mode: spec.http_mode.to_string(),
        pool_scope: TRANSPORT_POOL_SCOPE_KEY.to_string(),
        header_fingerprint: None,
        extra: Some(Value::Object(extra)),
    })
}

/// 探针结果只在其记录的 `emulation_profile` 与当前 profile 一致时才随 profile 下发，
/// 避免换了 profile 之后旧的 JA3/JA4 被当成当前指纹。
fn attach_tls_probe_summary(
    mut profile: ResolvedTransportProfile,
    upstream_metadata: Option<&Value>,
) -> ResolvedTransportProfile {
    let Some(emulation_id) = transport_profile_emulation_id(&profile) else {
        return profile;
    };
    let Some(probe) = upstream_metadata
        .and_then(|metadata| metadata.get(TLS_PROBE_UPSTREAM_METADATA_NAMESPACE))
        .and_then(Value::as_object)
    else {
        return profile;
    };
    if probe.get("observed").and_then(Value::as_bool) != Some(true) {
        return profile;
    }
    let probe_profile = probe
        .get(TRANSPORT_EMULATION_PROFILE_EXTRA_KEY)
        .and_then(Value::as_str)
        .map(str::trim)
        .unwrap_or_default();
    if !probe_profile.eq_ignore_ascii_case(&emulation_id) {
        return profile;
    }
    let mut summary = Map::new();
    for key in [
        "ja3",
        "ja3_hash",
        "ja4",
        "peetprint",
        "akamai_fingerprint",
        "http_version",
        "probe_url",
    ] {
        if let Some(value) = probe.get(key).and_then(Value::as_str) {
            summary.insert(key.to_string(), Value::String(value.to_string()));
        }
    }
    if let Some(value) = probe.get("probed_at_unix_secs").and_then(Value::as_u64) {
        summary.insert("probed_at_unix_secs".to_string(), json!(value));
    }
    if summary.is_empty() {
        return profile;
    }
    let mut extra = profile
        .extra
        .take()
        .and_then(|value| value.as_object().cloned())
        .unwrap_or_default();
    extra.insert(
        TRANSPORT_TLS_PROBE_EXTRA_KEY.to_string(),
        Value::Object(summary),
    );
    profile.extra = Some(Value::Object(extra));
    profile
}

fn resolve_claude_code_transport_profile(
    transport: &GatewayProviderTransportSnapshot,
) -> Option<ResolvedTransportProfile> {
    if !transport
        .provider
        .provider_type
        .trim()
        .eq_ignore_ascii_case("claude_code")
    {
        return None;
    }

    let identity_profile = *current_claude_code_transport_identity_profile();
    Some(ResolvedTransportProfile {
        profile_id: identity_profile.transport_profile_id().to_string(),
        backend: TRANSPORT_BACKEND_REQWEST_RUSTLS.to_string(),
        http_mode: TRANSPORT_HTTP_MODE_AUTO.to_string(),
        pool_scope: TRANSPORT_POOL_SCOPE_KEY.to_string(),
        header_fingerprint: None,
        extra: None,
    })
}

fn resolve_grok_browser_transport_profile(
    transport: &GatewayProviderTransportSnapshot,
) -> Option<ResolvedTransportProfile> {
    if !transport
        .provider
        .provider_type
        .trim()
        .eq_ignore_ascii_case("grok")
    {
        return None;
    }
    let auth_config = transport
        .key
        .decrypted_auth_config
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .and_then(|value| serde_json::from_str::<Value>(value).ok())?;
    let object = auth_config.as_object()?;
    let has_session = json_string_field(object, "sso_token")
        .or_else(|| json_string_field(object, "access_token"))
        .or_else(|| json_string_field(object, "token"))
        .is_some();
    if !has_session {
        return None;
    }
    grok_browser_resolved_transport_profile_from_auth_config(object, "grok_auth_config")
}

fn resolve_transport_profile_from_provider_config(
    config: Option<&Value>,
) -> Option<ResolvedTransportProfile> {
    let fingerprint = config?.get("fingerprint");
    resolve_transport_profile_from_fingerprint(fingerprint)
}

fn resolve_transport_profile_from_fingerprint(
    fingerprint: Option<&Value>,
) -> Option<ResolvedTransportProfile> {
    let fingerprint = fingerprint?;
    fingerprint
        .get("transport_profile")
        .and_then(parse_transport_profile_value)
}

pub fn transport_profile_is_configured(transport: &GatewayProviderTransportSnapshot) -> bool {
    transport_profile_configured_in_fingerprint(transport.key.fingerprint.as_ref())
        || transport_profile_configured_in_provider_config(transport.provider.config.as_ref())
}

fn transport_profile_configured_in_provider_config(config: Option<&Value>) -> bool {
    let fingerprint = config.and_then(|value| value.get("fingerprint"));
    transport_profile_configured_in_fingerprint(fingerprint)
}

fn transport_profile_configured_in_fingerprint(fingerprint: Option<&Value>) -> bool {
    fingerprint
        .and_then(|value| value.get("transport_profile"))
        .is_some_and(|value| !value.is_null())
}

fn parse_transport_profile_value(value: &Value) -> Option<ResolvedTransportProfile> {
    if let Some(profile_id) = value
        .as_str()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        if let Some(builtin) = builtin_tls_emulation_transport_profile(profile_id) {
            return Some(builtin);
        }
        return Some(ResolvedTransportProfile {
            profile_id: profile_id.to_string(),
            backend: TRANSPORT_BACKEND_REQWEST_RUSTLS.to_string(),
            http_mode: TRANSPORT_HTTP_MODE_AUTO.to_string(),
            pool_scope: TRANSPORT_POOL_SCOPE_KEY.to_string(),
            header_fingerprint: None,
            extra: None,
        });
    }

    let object = value.as_object()?;
    let profile_id = json_string_field(object, "profile_id")
        .or_else(|| json_string_field(object, "id"))
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())?;
    if let Some(builtin) = builtin_tls_emulation_transport_profile(&profile_id) {
        return Some(builtin);
    }
    let backend = json_string_field(object, "backend")
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| TRANSPORT_BACKEND_REQWEST_RUSTLS.to_string());
    let http_mode = json_string_field(object, "http_mode")
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| TRANSPORT_HTTP_MODE_AUTO.to_string());
    let pool_scope = json_string_field(object, "pool_scope")
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| TRANSPORT_POOL_SCOPE_KEY.to_string());
    let header_fingerprint = object.get("header_fingerprint").cloned();
    let extra = object.get("extra").cloned();

    Some(ResolvedTransportProfile {
        profile_id,
        backend,
        http_mode,
        pool_scope,
        header_fingerprint,
        extra,
    })
}

fn effective_proxy_config(transport: &GatewayProviderTransportSnapshot) -> Option<&Value> {
    [
        transport.key.proxy.as_ref(),
        transport.endpoint.proxy.as_ref(),
        transport.provider.proxy.as_ref(),
    ]
    .into_iter()
    .flatten()
    .find(|candidate| proxy_enabled(candidate))
}

fn proxy_enabled(value: &Value) -> bool {
    value
        .as_object()
        .and_then(|object| object.get("enabled"))
        .and_then(Value::as_bool)
        .unwrap_or(true)
}

fn proxy_snapshot_from_value(value: &Value) -> Option<ProxySnapshot> {
    let object = value.as_object()?;
    let enabled = object.get("enabled").and_then(Value::as_bool);
    let mode = json_string_field(object, "mode");
    let node_id = json_string_field(object, "node_id");
    let label = json_string_field(object, "label");
    let url = json_string_field(object, "url")
        .or_else(|| json_string_field(object, "proxy_url"))
        .and_then(|proxy_url| {
            proxy_url_with_auth(
                &proxy_url,
                json_proxy_credential_field(object, "username"),
                json_proxy_credential_field(object, "password"),
            )
        });

    let mut extra = Map::new();
    for (key, value) in object {
        if matches!(
            key.as_str(),
            "enabled"
                | "mode"
                | "node_id"
                | "label"
                | "url"
                | "proxy_url"
                | "username"
                | "password"
        ) {
            continue;
        }
        extra.insert(key.clone(), value.clone());
    }

    Some(ProxySnapshot {
        enabled,
        mode,
        node_id,
        label,
        url,
        extra: if extra.is_empty() {
            None
        } else {
            Some(Value::Object(extra))
        },
    })
}

fn json_string_field(object: &Map<String, Value>, key: &str) -> Option<String> {
    object
        .get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
}

fn json_proxy_credential_field<'a>(object: &'a Map<String, Value>, key: &str) -> Option<&'a str> {
    object
        .get(key)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
}

fn proxy_url_with_auth(
    proxy_url: &str,
    username: Option<&str>,
    password: Option<&str>,
) -> Option<String> {
    let username = username.filter(|value| !value.is_empty());
    let password = password.filter(|value| !value.is_empty());
    let mut parsed = url::Url::parse(proxy_url).ok()?;
    if !matches!(parsed.scheme(), "http" | "https" | "socks5" | "socks5h")
        || parsed.host_str().is_none()
    {
        return None;
    }
    if username.is_none() && password.is_none() {
        return Some(parsed.to_string());
    }
    let username = username.unwrap_or("");
    parsed.set_username(username).ok()?;
    parsed.set_password(password).ok()?;
    Some(parsed.to_string())
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use async_trait::async_trait;
    use serde_json::{json, Value};

    use super::super::snapshot::{
        GatewayProviderTransportEndpoint, GatewayProviderTransportKey,
        GatewayProviderTransportProvider, GatewayProviderTransportSnapshot,
    };
    use super::{
        configured_tls_emulation_profile_id, resolve_oauth_control_plane_transport_profile,
        resolve_oauth_control_plane_transport_profile_from_configs,
        resolve_transport_execution_timeouts, resolve_transport_profile,
        resolve_transport_profile_id, resolve_transport_proxy_snapshot,
        resolve_transport_proxy_snapshot_with_tunnel_affinity, transport_profile_is_configured,
        transport_proxy_is_locally_supported, TransportTunnelAffinityLookup,
        TransportTunnelAttachmentOwner,
    };
    use aether_contracts::{
        TRANSPORT_BACKEND_BROWSER_WREQ, TRANSPORT_HTTP_MODE_H2C_PRIOR_KNOWLEDGE,
        TRANSPORT_HTTP_MODE_HTTP1_ONLY,
    };

    #[derive(Default)]
    struct TestTunnelAffinityLookup {
        owners: BTreeMap<String, TransportTunnelAttachmentOwner>,
    }

    #[async_trait]
    impl TransportTunnelAffinityLookup for TestTunnelAffinityLookup {
        async fn lookup_tunnel_attachment_owner(
            &self,
            node_id: &str,
        ) -> Result<Option<TransportTunnelAttachmentOwner>, String> {
            Ok(self.owners.get(node_id).cloned())
        }
    }

    fn sample_lookup() -> TestTunnelAffinityLookup {
        let mut owners = BTreeMap::new();
        owners.insert(
            "proxy-node-1".to_string(),
            TransportTunnelAttachmentOwner {
                gateway_instance_id: "gateway-b".to_string(),
                relay_base_url: "http://gateway-b.internal".to_string(),
                observed_at_unix_secs: 4_102_444_800u64,
            },
        );
        TestTunnelAffinityLookup { owners }
    }

    fn sample_transport() -> GatewayProviderTransportSnapshot {
        GatewayProviderTransportSnapshot {
            provider: GatewayProviderTransportProvider {
                id: "provider-1".to_string(),
                name: "provider".to_string(),
                provider_type: "custom".to_string(),
                website: None,
                is_active: true,
                keep_priority_on_conversion: false,
                enable_format_conversion: false,
                concurrent_limit: None,
                max_retries: None,
                proxy: Some(json!({"url":"http://provider-proxy:8080"})),
                request_timeout_secs: None,
                stream_first_byte_timeout_secs: None,
                config: None,
            },
            endpoint: GatewayProviderTransportEndpoint {
                id: "endpoint-1".to_string(),
                provider_id: "provider-1".to_string(),
                api_format: "openai:chat".to_string(),
                api_family: Some("openai".to_string()),
                endpoint_kind: Some("chat".to_string()),
                is_active: true,
                base_url: "https://api.openai.example".to_string(),
                header_rules: None,
                body_rules: None,
                max_retries: None,
                custom_path: None,
                config: None,
                format_acceptance_config: None,
                proxy: Some(json!({"enabled":false,"url":"http://endpoint-proxy:8080"})),
            },
            key: GatewayProviderTransportKey {
                id: "key-1".to_string(),
                provider_id: "provider-1".to_string(),
                name: "key".to_string(),
                auth_type: "api_key".to_string(),
                is_active: true,
                api_formats: None,
                auth_type_by_format: None,
                allow_auth_channel_mismatch_formats: None,

                allowed_models: None,
                capabilities: None,
                rate_multipliers: None,
                global_priority_by_format: None,
                expires_at_unix_secs: None,
                proxy: Some(json!({"node_id":"proxy-node-1","kind":"manual"})),
                fingerprint: Some(json!({"transport_profile":"chrome_136"})),
                upstream_metadata: None,
                decrypted_api_key: "sk-test".to_string(),
                decrypted_auth_config: None,
            },
        }
    }

    #[test]
    fn transport_execution_timeouts_use_provider_defaults_when_unset() {
        let transport = sample_transport();

        let timeouts = resolve_transport_execution_timeouts(&transport)
            .expect("default provider timeouts should resolve");

        assert_eq!(timeouts.total_ms, None);
        assert_eq!(timeouts.first_byte_ms, Some(30_000));
    }

    #[test]
    fn transport_execution_timeouts_preserve_configured_values_independently() {
        let mut transport = sample_transport();
        transport.provider.request_timeout_secs = Some(12.0);

        let timeouts = resolve_transport_execution_timeouts(&transport)
            .expect("provider timeouts should resolve");

        assert_eq!(timeouts.total_ms, Some(12_000));
        assert_eq!(timeouts.first_byte_ms, Some(30_000));
    }

    #[test]
    fn transport_execution_timeouts_preserve_the_configurable_maximum() {
        let mut transport = sample_transport();
        transport.provider.request_timeout_secs =
            Some(aether_contracts::MAX_EXECUTION_REQUEST_TIMEOUT_SECS as f64);

        let timeouts = resolve_transport_execution_timeouts(&transport)
            .expect("provider timeouts should resolve");

        assert_eq!(
            timeouts.total_ms,
            Some(aether_contracts::MAX_EXECUTION_REQUEST_TIMEOUT_MS)
        );
    }

    #[test]
    fn transport_execution_timeouts_preserve_configured_first_byte_value() {
        let mut transport = sample_transport();
        transport.provider.stream_first_byte_timeout_secs = Some(7.5);

        let timeouts = resolve_transport_execution_timeouts(&transport)
            .expect("provider timeouts should resolve");

        assert_eq!(timeouts.total_ms, None);
        assert_eq!(timeouts.first_byte_ms, Some(7_500));
    }

    #[test]
    fn resolves_transport_proxy_with_key_precedence() {
        let snapshot = resolve_transport_proxy_snapshot(&sample_transport())
            .expect("proxy snapshot should resolve");
        assert_eq!(snapshot.node_id.as_deref(), Some("proxy-node-1"));
        assert_eq!(snapshot.url, None);
        assert_eq!(snapshot.extra, Some(json!({"kind":"manual"})));
    }

    #[test]
    fn resolves_authenticated_inline_proxy_without_secret_extra_fields() {
        let mut transport = sample_transport();
        transport.key.proxy = Some(json!({
            "url": "socks5h://proxy.example:1080",
            "username": " alice ",
            "password": " p:ss ",
            "kind": "manual",
        }));

        let snapshot = resolve_transport_proxy_snapshot(&transport)
            .expect("authenticated proxy snapshot should resolve");

        assert_eq!(
            snapshot.url.as_deref(),
            Some("socks5h://%20alice%20:%20p%3Ass%20@proxy.example:1080")
        );
        assert_eq!(snapshot.extra, Some(json!({"kind":"manual"})));
    }

    #[test]
    fn resolves_legacy_password_only_inline_proxy() {
        let mut transport = sample_transport();
        transport.key.proxy = Some(json!({
            "url": "http://proxy.example:8080",
            "password": "legacy-password",
        }));

        let snapshot = resolve_transport_proxy_snapshot(&transport)
            .expect("password-only proxy snapshot should resolve");
        assert_eq!(
            snapshot.url.as_deref(),
            Some("http://:legacy-password@proxy.example:8080/")
        );
        assert!(snapshot.extra.is_none());
    }

    #[tokio::test]
    async fn enriches_transport_proxy_snapshot_with_tunnel_owner_hint() {
        let state = sample_lookup();

        let snapshot =
            resolve_transport_proxy_snapshot_with_tunnel_affinity(&state, &sample_transport())
                .await
                .expect("proxy snapshot should resolve");

        assert_eq!(snapshot.node_id.as_deref(), Some("proxy-node-1"));
        assert_eq!(
            snapshot
                .extra
                .as_ref()
                .and_then(|value| value.get("tunnel_base_url"))
                .and_then(Value::as_str),
            Some("http://gateway-b.internal")
        );
        assert_eq!(
            snapshot
                .extra
                .as_ref()
                .and_then(|value| value.get("tunnel_owner_instance_id"))
                .and_then(Value::as_str),
            Some("gateway-b")
        );
    }

    #[tokio::test]
    async fn preserves_explicit_tunnel_base_url_when_owner_hint_exists() {
        let mut transport = sample_transport();
        transport.key.proxy = Some(json!({
            "node_id": "proxy-node-1",
            "kind": "manual",
            "tunnel_base_url": "http://configured-gateway.internal",
        }));
        let state = sample_lookup();

        let snapshot = resolve_transport_proxy_snapshot_with_tunnel_affinity(&state, &transport)
            .await
            .expect("proxy snapshot should resolve");

        assert_eq!(
            snapshot
                .extra
                .as_ref()
                .and_then(|value| value.get("tunnel_base_url"))
                .and_then(Value::as_str),
            Some("http://configured-gateway.internal")
        );
        assert_eq!(
            snapshot
                .extra
                .as_ref()
                .and_then(|value| value.get("tunnel_owner_instance_id"))
                .and_then(Value::as_str),
            Some("gateway-b")
        );
    }

    #[test]
    fn resolves_transport_profile_id_from_key_fingerprint() {
        assert_eq!(
            resolve_transport_profile_id(&sample_transport()).as_deref(),
            Some("chrome_136")
        );
        assert!(transport_proxy_is_locally_supported(&sample_transport()));
    }

    #[test]
    fn resolves_transport_profile_from_key_fingerprint_before_provider_default() {
        let mut transport = sample_transport();
        transport.provider.config = Some(json!({
            "fingerprint": {"transport_profile": "provider_profile"}
        }));
        transport.key.fingerprint = Some(json!({
            "transport_profile": {
                "profile_id": "key_profile",
                "backend": "reqwest_rustls",
                "http_mode": "http1_only"
            }
        }));

        let profile = resolve_transport_profile(&transport).expect("profile");

        assert_eq!(profile.profile_id, "key_profile");
        assert_eq!(profile.backend, "reqwest_rustls");
        assert_eq!(profile.http_mode, "http1_only");
        assert_eq!(profile.pool_scope, "key");
    }

    #[test]
    fn resolves_transport_profile_from_provider_default() {
        let mut transport = sample_transport();
        transport.key.fingerprint = None;
        transport.provider.config = Some(json!({
            "fingerprint": {"transport_profile": "provider_profile"}
        }));

        let profile = resolve_transport_profile(&transport).expect("profile");

        assert_eq!(profile.profile_id, "provider_profile");
        assert_eq!(profile.backend, "reqwest_rustls");
    }

    #[test]
    fn resolves_typed_claude_code_transport_profile_when_unconfigured() {
        let mut transport = sample_transport();
        transport.provider.provider_type = "claude_code".to_string();
        transport.key.fingerprint = None;
        transport.provider.config = None;

        let profile = resolve_transport_profile(&transport).expect("typed Claude Code profile");

        assert_eq!(profile.profile_id, "claude_code_nodejs");
        assert_eq!(profile.backend, "reqwest_rustls");
        assert_eq!(profile.http_mode, "auto");
        assert_eq!(profile.pool_scope, "key");
        assert!(profile.header_fingerprint.is_none());
        assert!(profile.extra.is_none());
        assert!(!transport_profile_is_configured(&transport));
    }

    #[test]
    fn explicit_claude_code_transport_profiles_precede_typed_default() {
        let mut transport = sample_transport();
        transport.provider.provider_type = "claude_code".to_string();
        transport.provider.config = Some(json!({
            "fingerprint": {"transport_profile": "provider_claude_profile"}
        }));
        transport.key.fingerprint = Some(json!({
            "transport_profile": "key_claude_profile"
        }));

        assert_eq!(
            resolve_transport_profile(&transport)
                .expect("key profile")
                .profile_id,
            "key_claude_profile"
        );

        transport.key.fingerprint = None;
        assert_eq!(
            resolve_transport_profile(&transport)
                .expect("provider profile")
                .profile_id,
            "provider_claude_profile"
        );
    }

    #[test]
    fn invalid_explicit_claude_code_transport_profile_blocks_typed_default() {
        let mut transport = sample_transport();
        transport.provider.provider_type = "claude_code".to_string();
        transport.provider.config = None;
        transport.key.fingerprint = Some(json!({
            "transport_profile": {"backend": "reqwest_rustls"}
        }));

        assert!(transport_profile_is_configured(&transport));
        assert!(resolve_transport_profile(&transport).is_none());
    }

    #[test]
    fn maps_string_transport_profile_to_resolved_profile() {
        let profile = resolve_transport_profile(&sample_transport()).expect("profile");

        assert_eq!(profile.profile_id, "chrome_136");
        assert_eq!(profile.backend, "reqwest_rustls");
        assert_eq!(profile.http_mode, "auto");
        assert_eq!(profile.pool_scope, "key");
    }

    #[test]
    fn resolves_h2c_prior_knowledge_transport_profile() {
        let mut transport = sample_transport();
        transport.key.fingerprint = Some(json!({
            "transport_profile": {
                "profile_id": "mock-h2c",
                "backend": "reqwest_rustls",
                "http_mode": "h2c_prior_knowledge"
            }
        }));

        let profile = resolve_transport_profile(&transport).expect("profile");

        assert_eq!(profile.profile_id, "mock-h2c");
        assert_eq!(profile.http_mode, TRANSPORT_HTTP_MODE_H2C_PRIOR_KNOWLEDGE);
    }

    #[test]
    fn resolves_no_transport_profile_without_fingerprint_configuration() {
        let mut transport = sample_transport();
        transport.key.fingerprint = None;
        transport.provider.config = None;

        assert!(resolve_transport_profile(&transport).is_none());
        assert!(!transport_profile_is_configured(&transport));
    }

    #[test]
    fn resolves_grok_browser_transport_profile_from_session_auth_config() {
        let mut transport = sample_transport();
        transport.provider.provider_type = "grok".to_string();
        transport.key.fingerprint = None;
        transport.provider.config = None;
        transport.key.decrypted_auth_config = Some(
            json!({
                "sso_token": "sso-token",
                "browser_profile": "chrome136",
                "cf_clearance": "clearance"
            })
            .to_string(),
        );

        let profile = resolve_transport_profile(&transport).expect("profile");

        assert_eq!(profile.profile_id, "chrome136");
        assert_eq!(profile.backend, "browser_wreq");
        assert_eq!(profile.http_mode, "auto");
        assert_eq!(profile.pool_scope, "key");
        assert_eq!(
            profile
                .extra
                .as_ref()
                .and_then(|value| value.get("browser_profile"))
                .and_then(Value::as_str),
            Some("chrome136")
        );
    }

    #[test]
    fn resolves_grok_browser_transport_profile_default_from_session_auth_config() {
        let mut transport = sample_transport();
        transport.provider.provider_type = "grok".to_string();
        transport.key.fingerprint = None;
        transport.provider.config = None;
        transport.key.decrypted_auth_config = Some(
            json!({
                "sso_token": "sso-token"
            })
            .to_string(),
        );

        let profile = resolve_transport_profile(&transport).expect("profile");

        assert_eq!(profile.profile_id, "chrome136");
        assert_eq!(profile.backend, "browser_wreq");
        assert_eq!(
            profile
                .extra
                .as_ref()
                .and_then(|value| value.get("source"))
                .and_then(Value::as_str),
            Some("grok_auth_config")
        );
    }

    #[test]
    fn resolves_grok_browser_transport_profile_normalizes_auth_config_alias() {
        let mut transport = sample_transport();
        transport.provider.provider_type = "grok".to_string();
        transport.key.fingerprint = None;
        transport.provider.config = None;
        transport.key.decrypted_auth_config = Some(
            json!({
                "sso_token": "sso-token",
                "browser_profile": "Chrome-137"
            })
            .to_string(),
        );

        let profile = resolve_transport_profile(&transport).expect("profile");

        assert_eq!(profile.profile_id, "chrome137");
        assert_eq!(
            profile
                .extra
                .as_ref()
                .and_then(|value| value.get("browser_profile"))
                .and_then(Value::as_str),
            Some("chrome137")
        );
    }

    #[test]
    fn resolves_grok_browser_transport_profile_from_legacy_user_agent() {
        let mut transport = sample_transport();
        transport.provider.provider_type = "grok".to_string();
        transport.key.fingerprint = None;
        transport.provider.config = None;
        transport.key.decrypted_auth_config = Some(
            json!({
                "sso_token": "sso-token",
                "user_agent": "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/137.0.0.0 Safari/537.36"
            })
            .to_string(),
        );

        let profile = resolve_transport_profile(&transport).expect("profile");

        assert_eq!(profile.profile_id, "chrome137");
        assert_eq!(
            profile
                .extra
                .as_ref()
                .and_then(|value| value.get("browser_profile"))
                .and_then(Value::as_str),
            Some("chrome137")
        );
    }

    #[test]
    fn key_fingerprint_wins_over_grok_auth_config_fallback() {
        let mut transport = sample_transport();
        transport.provider.provider_type = "grok".to_string();
        transport.provider.config = None;
        transport.key.fingerprint = Some(json!({
            "transport_profile": {
                "profile_id": "chrome136",
                "backend": "browser_wreq",
                "extra": {"browser_profile": "chrome136", "source": "key"}
            }
        }));
        transport.key.decrypted_auth_config = Some(
            json!({
                "sso_token": "sso-token",
                "browser_profile": "chrome137"
            })
            .to_string(),
        );

        let profile = resolve_transport_profile(&transport).expect("profile");

        assert_eq!(profile.profile_id, "chrome136");
        assert_eq!(
            profile
                .extra
                .as_ref()
                .and_then(|value| value.get("source"))
                .and_then(Value::as_str),
            Some("key")
        );
    }

    #[test]
    fn provider_fingerprint_wins_over_grok_auth_config_fallback() {
        let mut transport = sample_transport();
        transport.provider.provider_type = "grok".to_string();
        transport.key.fingerprint = None;
        transport.provider.config = Some(json!({
            "fingerprint": {
                "transport_profile": {
                    "profile_id": "chrome136",
                    "backend": "browser_wreq",
                    "extra": {"browser_profile": "chrome136", "source": "provider"}
                }
            }
        }));
        transport.key.decrypted_auth_config = Some(
            json!({
                "sso_token": "sso-token",
                "browser_profile": "chrome137"
            })
            .to_string(),
        );

        let profile = resolve_transport_profile(&transport).expect("profile");

        assert_eq!(profile.profile_id, "chrome136");
        assert_eq!(
            profile
                .extra
                .as_ref()
                .and_then(|value| value.get("source"))
                .and_then(Value::as_str),
            Some("provider")
        );
    }

    #[test]
    fn rejects_unsupported_grok_auth_config_browser_profile() {
        let mut transport = sample_transport();
        transport.provider.provider_type = "grok".to_string();
        transport.key.fingerprint = None;
        transport.provider.config = None;
        transport.key.decrypted_auth_config = Some(
            json!({
                "sso_token": "sso-token",
                "browser_profile": "safari999"
            })
            .to_string(),
        );

        assert!(resolve_transport_profile(&transport).is_none());
    }

    #[test]
    fn rejects_unsupported_grok_auth_config_user_agent_profile() {
        let mut transport = sample_transport();
        transport.provider.provider_type = "grok".to_string();
        transport.key.fingerprint = None;
        transport.provider.config = None;
        transport.key.decrypted_auth_config = Some(
            json!({
                "sso_token": "sso-token",
                "user_agent": "Mozilla/5.0 Version/18.0 Safari/605.1.15"
            })
            .to_string(),
        );

        assert!(resolve_transport_profile(&transport).is_none());
    }

    #[test]
    fn unconfigured_claude_code_transport_profile_is_byte_identical_to_legacy_default() {
        let mut transport = sample_transport();
        transport.provider.provider_type = "claude_code".to_string();
        transport.key.fingerprint = None;
        transport.provider.config = Some(json!({"cloak": {"mode": "auto"}}));

        let profile = resolve_transport_profile(&transport).expect("typed Claude Code profile");
        let expected = aether_contracts::ResolvedTransportProfile {
            profile_id: "claude_code_nodejs".to_string(),
            backend: "reqwest_rustls".to_string(),
            http_mode: "auto".to_string(),
            pool_scope: "key".to_string(),
            header_fingerprint: None,
            extra: None,
        };
        assert_eq!(profile, expected);
        assert_eq!(
            serde_json::to_string(&profile).expect("profile should serialize"),
            serde_json::to_string(&expected).expect("expected should serialize")
        );
        assert_eq!(configured_tls_emulation_profile_id(&transport), None);
        assert!(resolve_oauth_control_plane_transport_profile(&transport).is_none());
    }

    #[test]
    fn builtin_tls_emulation_profile_switches_backend_to_browser_wreq() {
        let mut transport = sample_transport();
        transport.provider.provider_type = "claude_code".to_string();
        transport.provider.config = None;
        transport.key.fingerprint = Some(json!({"transport_profile": "claude_code_node_openssl"}));

        let profile = resolve_transport_profile(&transport).expect("emulation profile");
        assert_eq!(profile.profile_id, "claude_code_node_openssl");
        assert_eq!(profile.backend, TRANSPORT_BACKEND_BROWSER_WREQ);
        assert_eq!(profile.http_mode, TRANSPORT_HTTP_MODE_HTTP1_ONLY);
        assert_eq!(profile.pool_scope, "key");
        assert_eq!(
            profile
                .extra
                .as_ref()
                .and_then(|extra| extra.get("emulation_profile")),
            Some(&json!("claude_code_node_openssl"))
        );
        assert!(profile
            .extra
            .as_ref()
            .and_then(|extra| extra.get("tls_probe"))
            .is_none());
        assert_eq!(
            configured_tls_emulation_profile_id(&transport),
            Some("claude_code_node_openssl")
        );

        // 供应商级配置、对象形态、大小写与连字符都能命中内置表。
        transport.key.fingerprint = None;
        transport.provider.config = Some(json!({
            "fingerprint": {"transport_profile": {"profile_id": "Claude-Code-Node-OpenSSL"}}
        }));
        let profile = resolve_transport_profile(&transport).expect("provider emulation profile");
        assert_eq!(profile.profile_id, "claude_code_node_openssl");
        assert_eq!(profile.backend, TRANSPORT_BACKEND_BROWSER_WREQ);
        assert_eq!(profile.http_mode, TRANSPORT_HTTP_MODE_HTTP1_ONLY);

        transport.provider.config = Some(json!({
            "fingerprint": {"transport_profile": "chatgpt_com_chrome"}
        }));
        let profile = resolve_transport_profile(&transport).expect("chrome emulation profile");
        assert_eq!(profile.backend, TRANSPORT_BACKEND_BROWSER_WREQ);
        assert_eq!(
            profile
                .extra
                .as_ref()
                .and_then(|extra| extra.get("browser_profile")),
            Some(&json!("chrome136"))
        );
    }

    #[test]
    fn configured_tls_profiles_obey_provider_scope_for_both_config_sources() {
        for (provider_type, profile_id, supported) in [
            ("claude_code", "Claude-Code-Node-OpenSSL", true),
            ("claude_code", "chatgpt_com_chrome", true),
            ("claude_code", "claude_code_oauth_control_plane", false),
            ("codex", "chatgpt_com_chrome", true),
            ("codex", "claude_code_node_openssl", false),
            ("gemini_cli", "chatgpt_com_chrome", false),
            ("antigravity", "claude_code_node_openssl", false),
        ] {
            for value in [json!(profile_id), json!({"profile_id": profile_id})] {
                for on_key in [false, true] {
                    let mut transport = sample_transport();
                    transport.provider.provider_type = provider_type.into();
                    transport.provider.config = (!on_key).then(|| {
                        json!({
                            "fingerprint": {"transport_profile": value}
                        })
                    });
                    transport.key.fingerprint = on_key.then(|| json!({"transport_profile": value}));
                    assert_eq!(
                        resolve_transport_profile(&transport).is_some(),
                        supported,
                        "{provider_type}: {profile_id}, key={on_key}"
                    );
                    assert_eq!(
                        resolve_oauth_control_plane_transport_profile(&transport).is_some(),
                        supported
                    );
                    assert_eq!(
                        resolve_oauth_control_plane_transport_profile_from_configs(
                            provider_type,
                            transport.provider.config.as_ref(),
                            transport.key.fingerprint.as_ref(),
                        )
                        .is_some(),
                        supported
                    );
                }
            }
        }
    }

    #[test]
    fn browser_tls_profiles_preserve_node_and_url_proxy_routes() {
        let mut transport = sample_transport();
        transport.provider.provider_type = "claude_code".into();
        transport.provider.proxy = None;
        transport.provider.config = None;
        transport.endpoint.proxy = None;
        transport.key.fingerprint = Some(json!({"transport_profile": "claude_code_node_openssl"}));
        for proxy in [
            json!({"node_id": "node-1"}),
            json!({"mode": "tunnel", "node_id": "node-1", "url": "http://proxy.example:8080"}),
        ] {
            transport.key.proxy = Some(proxy);
            assert!(transport_proxy_is_locally_supported(&transport));
            let profile = resolve_transport_profile(&transport).expect("inference profile");
            assert_eq!(profile.backend, TRANSPORT_BACKEND_BROWSER_WREQ);
            assert_eq!(profile.profile_id, "claude_code_node_openssl");
            let profile = resolve_oauth_control_plane_transport_profile(&transport)
                .expect("OAuth profile must also survive a node route");
            assert_eq!(profile.backend, TRANSPORT_BACKEND_BROWSER_WREQ);
            assert_eq!(profile.profile_id, "claude_code_oauth_control_plane");
        }
        for proxy in [
            None,
            Some(json!({"url": "http://proxy.example:8080"})),
            Some(
                json!({"mode": "manual", "node_id": "node-1", "url": "socks5h://proxy.example:1080"}),
            ),
        ] {
            transport.key.proxy = proxy;
            assert!(transport_proxy_is_locally_supported(&transport));
        }
        transport.key.fingerprint = None;
        transport.key.proxy = Some(json!({"node_id": "node-1"}));
        assert!(
            transport_proxy_is_locally_supported(&transport),
            "default rustls still supports nodes"
        );
    }

    #[test]
    fn unknown_transport_profile_strings_keep_reqwest_rustls_semantics() {
        let mut transport = sample_transport();
        transport.provider.provider_type = "claude_code".to_string();
        transport.provider.config = None;
        transport.key.fingerprint = Some(json!({"transport_profile": "claude_code_nodejs"}));

        let profile = resolve_transport_profile(&transport).expect("explicit profile");
        assert_eq!(profile.profile_id, "claude_code_nodejs");
        assert_eq!(profile.backend, "reqwest_rustls");
        assert!(profile.extra.is_none());
        assert_eq!(configured_tls_emulation_profile_id(&transport), None);
        assert!(resolve_oauth_control_plane_transport_profile(&transport).is_none());
    }

    #[test]
    fn oauth_control_plane_profile_follows_provider_type_only_when_emulation_is_configured() {
        let mut transport = sample_transport();
        transport.provider.provider_type = "claude_code".to_string();
        transport.provider.config = None;
        transport.key.fingerprint = Some(json!({"transport_profile": "claude_code_node_openssl"}));
        let profile = resolve_oauth_control_plane_transport_profile(&transport)
            .expect("claude control plane profile");
        assert_eq!(profile.profile_id, "claude_code_oauth_control_plane");
        assert_eq!(profile.backend, TRANSPORT_BACKEND_BROWSER_WREQ);

        transport.provider.provider_type = "codex".to_string();
        transport.key.fingerprint = Some(json!({"transport_profile": "chatgpt_com_chrome"}));
        let profile = resolve_oauth_control_plane_transport_profile(&transport)
            .expect("codex control plane profile");
        assert_eq!(profile.profile_id, "chatgpt_com_chrome");

        transport.provider.provider_type = "gemini_cli".to_string();
        assert!(resolve_oauth_control_plane_transport_profile(&transport).is_none());

        assert!(resolve_oauth_control_plane_transport_profile_from_configs(
            "claude_code",
            None,
            None
        )
        .is_none());
        assert_eq!(
            resolve_oauth_control_plane_transport_profile_from_configs(
                "claude_code",
                Some(&json!({"fingerprint": {"transport_profile": "claude_code_node_openssl"}})),
                None,
            )
            .map(|profile| profile.profile_id),
            Some("claude_code_oauth_control_plane".to_string())
        );
        assert!(resolve_oauth_control_plane_transport_profile_from_configs(
            "claude_code",
            Some(&json!({"fingerprint": {"transport_profile": "chrome_136"}})),
            None,
        )
        .is_none());
    }

    #[test]
    fn tls_probe_summary_is_attached_only_for_matching_emulation_profile() {
        let mut transport = sample_transport();
        transport.provider.provider_type = "claude_code".to_string();
        transport.provider.config = None;
        transport.key.fingerprint = Some(json!({"transport_profile": "claude_code_node_openssl"}));
        transport.key.upstream_metadata = Some(json!({
            "tls_probe": {
                "observed": true,
                "emulation_profile": "claude_code_node_openssl",
                "ja3": "771,4865-4866,0-23,29-23-24,0",
                "ja3_hash": "0123456789abcdef0123456789abcdef",
                "ja4": "t13d1716h1_5b57614c22b0_3d5db4fb5c1e",
                "http_version": "h1",
                "probe_url": "https://tls.peet.ws/api/all",
                "probed_at_unix_secs": 1_760_000_000u64,
                "secret": "must-not-leak"
            }
        }));

        let profile = resolve_transport_profile(&transport).expect("emulation profile");
        let probe = profile
            .extra
            .as_ref()
            .and_then(|extra| extra.get("tls_probe"))
            .and_then(Value::as_object)
            .expect("probe summary should be attached");
        assert_eq!(
            probe.get("ja3_hash"),
            Some(&json!("0123456789abcdef0123456789abcdef"))
        );
        assert_eq!(
            probe.get("ja4"),
            Some(&json!("t13d1716h1_5b57614c22b0_3d5db4fb5c1e"))
        );
        assert_eq!(
            probe.get("probed_at_unix_secs"),
            Some(&json!(1_760_000_000u64))
        );
        assert!(probe.get("secret").is_none());

        for observed in [Value::Bool(false), Value::Null] {
            transport.key.upstream_metadata.as_mut().unwrap()["tls_probe"]["observed"] = observed;
            let profile = resolve_transport_profile(&transport).expect("emulation profile");
            assert!(profile
                .extra
                .as_ref()
                .and_then(|extra| extra.get("tls_probe"))
                .is_none());
        }
        transport.key.upstream_metadata.as_mut().unwrap()["tls_probe"]["observed"] =
            Value::Bool(true);

        // 探针记录的是另一个 profile：不附带。
        transport.key.fingerprint = Some(json!({"transport_profile": "chatgpt_com_chrome"}));
        let profile = resolve_transport_profile(&transport).expect("chrome profile");
        assert!(profile
            .extra
            .as_ref()
            .and_then(|extra| extra.get("tls_probe"))
            .is_none());

        // 非仿真 profile 永远不附带。
        transport.key.fingerprint = Some(json!({"transport_profile": "chrome_136"}));
        let profile = resolve_transport_profile(&transport).expect("plain profile");
        assert!(profile.extra.is_none());
    }

    #[test]
    fn fingerprint_transport_profile_id_and_probe_summary_helpers() {
        use super::{
            configured_transport_profile_id_from_fingerprint,
            tls_probe_summary_from_upstream_metadata,
        };
        assert_eq!(
            configured_transport_profile_id_from_fingerprint(Some(&json!({
                "transport_profile": " claude_code_node_openssl "
            }))),
            Some("claude_code_node_openssl".to_string())
        );
        assert_eq!(
            configured_transport_profile_id_from_fingerprint(Some(&json!({
                "transport_profile": {"profile_id": "chatgpt_com_chrome", "backend": "browser_wreq"}
            }))),
            Some("chatgpt_com_chrome".to_string())
        );
        assert_eq!(
            configured_transport_profile_id_from_fingerprint(Some(&json!({"device_id": "x"}))),
            None
        );
        assert_eq!(configured_transport_profile_id_from_fingerprint(None), None);

        let summary = tls_probe_summary_from_upstream_metadata(Some(&json!({
            "tls_probe": {
                "observed": true,
                "ja4": "t13d1716h1_5b57614c22b0_3d5db4fb5c1e",
                "ja3_hash": "0123456789abcdef0123456789abcdef",
                "probed_at_unix_secs": 1_760_000_000u64,
                "http1_header_order": ["Host", 7, "Accept"],
                "nested": {"secret": true}
            }
        })))
        .expect("summary");
        assert_eq!(summary["ja4"], "t13d1716h1_5b57614c22b0_3d5db4fb5c1e");
        assert_eq!(summary["probed_at_unix_secs"], 1_760_000_000u64);
        assert_eq!(summary["http1_header_order"], json!(["Host", "Accept"]));
        assert!(summary.get("nested").is_none());
        assert!(tls_probe_summary_from_upstream_metadata(Some(&json!({
            "tls_probe": {"observed": false, "ja4": "x"}
        })))
        .is_none());
        assert!(tls_probe_summary_from_upstream_metadata(None).is_none());
    }
}
