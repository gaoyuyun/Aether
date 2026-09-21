//! P5：管理端 TLS 指纹探针。
//!
//! 对固定允许清单里的回显服务（默认 `https://tls.peet.ws/api/all`）按该 Key 的传输 profile
//! 与代理发一次 GET，把回显的 JA3 / JA4 / peetprint / HTTP 版本写进 Key 的
//! `upstream_metadata.tls_probe`。之后 `network.rs::resolve_transport_profile` 在探针记录的
//! `emulation_profile` 与当前 profile 一致时把摘要附到 `extra.tls_probe`，请求详情的出站 TLS
//! 记录据此把 `observed` 置为 `true`。
//!
//! 探针 URL 不接受调用方传入：只能是清单内的 https 地址（系统配置 `transport.tls_probe_url`
//! 也只能从清单里选）。

use std::collections::BTreeMap;
use std::time::Duration;

use aether_contracts::{
    ExecutionPlan, ExecutionResult, ExecutionTimeouts, ProxySnapshot, RequestBody,
    ResolvedTransportProfile, TRANSPORT_BACKEND_BROWSER_WREQ, TRANSPORT_BACKEND_REQWEST_RUSTLS,
};
use aether_data_contracts::repository::provider_catalog::{
    ProviderCatalogKeyRuntimeMetadataUpdate, StoredProviderCatalogKey,
    StoredProviderCatalogProvider,
};
use serde_json::{json, Map, Value};
use tracing::warn;

use crate::clock::current_unix_secs;
use crate::provider_transport::{
    builtin_tls_emulation_transport_profile, claude_code,
    configured_transport_profile_id_from_fingerprint, resolve_transport_profile,
    transport_profile_emulation_id, GatewayProviderTransportSnapshot,
    TLS_PROBE_UPSTREAM_METADATA_NAMESPACE, TRANSPORT_TLS_PROBE_EXTRA_KEY,
};
use crate::AppState;

/// 默认探针地址。
pub(crate) const TLS_PROBE_DEFAULT_URL: &str = "https://tls.peet.ws/api/all";
/// 允许的探针地址清单（固定，不接受调用方传入）。
pub(crate) const TLS_PROBE_ALLOWED_URLS: &[&str] =
    &[TLS_PROBE_DEFAULT_URL, "https://tls.browserleaks.com/json"];
/// 系统配置键：可选地把探针地址换成清单里的另一项。
pub(crate) const TLS_PROBE_URL_SYSTEM_CONFIG_KEY: &str = "transport.tls_probe_url";
const TLS_PROBE_REQUEST_ID_PREFIX: &str = "admin-tls-probe";
const TLS_PROBE_CONNECT_TIMEOUT_MS: u64 = 10_000;
const TLS_PROBE_READ_TIMEOUT_MS: u64 = 15_000;
const TLS_PROBE_TOTAL_TIMEOUT_MS: u64 = 20_000;
const TLS_PROBE_RESPONSE_BODY_LIMIT_BYTES: usize = 256 * 1024;
const TLS_PROBE_MAX_HEADER_ORDER_ENTRIES: usize = 32;
const TLS_PROBE_MAX_HEADER_NAME_CHARS: usize = 64;
const TLS_PROBE_MAX_FINGERPRINT_CHARS: usize = 512;
const RUNTIME_METADATA_CAS_MAX_ATTEMPTS: usize = 4;

#[derive(Debug)]
pub(crate) struct TlsProbeError {
    pub(crate) message: String,
}

impl TlsProbeError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl From<crate::GatewayError> for TlsProbeError {
    fn from(error: crate::GatewayError) -> Self {
        Self::new(error.into_message())
    }
}

/// 探针使用的传输 profile：与真实请求一致（供应商 / Key 显式配置 → 内置仿真表；未配置 →
/// 类型默认，claude_code 是 `claude_code_nodejs`/reqwest）。
#[derive(Debug, Clone)]
pub(crate) struct TlsProbeTarget {
    pub(crate) probe_url: String,
    pub(crate) transport_profile: Option<ResolvedTransportProfile>,
    pub(crate) proxy: Option<ProxySnapshot>,
    pub(crate) user_agent: Option<String>,
}

/// 校验探针地址只能来自清单。
pub(crate) fn resolve_tls_probe_url(configured: Option<&str>) -> String {
    configured
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .and_then(|value| {
            TLS_PROBE_ALLOWED_URLS
                .iter()
                .find(|allowed| allowed.eq_ignore_ascii_case(value))
        })
        .map(|value| value.to_string())
        .unwrap_or_else(|| TLS_PROBE_DEFAULT_URL.to_string())
}

/// 构造探针执行计划：GET 探针地址，头只带 `accept` 与（claude_code）原生 CLI 的 UA，
/// 让回显里的 `user_agent` / HTTP/1 头顺序与真实请求一致。
pub(crate) fn build_tls_probe_plan(
    provider: &StoredProviderCatalogProvider,
    key: &StoredProviderCatalogKey,
    target: &TlsProbeTarget,
) -> ExecutionPlan {
    let mut headers = BTreeMap::new();
    headers.insert("accept".to_string(), "application/json".to_string());
    if let Some(user_agent) = target.user_agent.as_deref() {
        headers.insert("user-agent".to_string(), user_agent.to_string());
    }
    headers.insert(
        aether_contracts::EXECUTION_REQUEST_FOLLOW_REDIRECTS_HEADER.to_string(),
        "false".to_string(),
    );
    let plan = ExecutionPlan {
        request_id: format!("{TLS_PROBE_REQUEST_ID_PREFIX}:{}", key.id),
        candidate_id: None,
        provider_name: Some(provider.name.clone()),
        provider_id: provider.id.clone(),
        endpoint_id: String::new(),
        key_id: key.id.clone(),
        method: "GET".to_string(),
        url: target.probe_url.clone(),
        headers,
        content_type: None,
        content_encoding: None,
        body: RequestBody {
            json_body: None,
            body_bytes_b64: None,
            body_ref: None,
        },
        stream: false,
        client_api_format: "admin:tls_probe".to_string(),
        provider_api_format: "admin:tls_probe".to_string(),
        model_name: None,
        proxy: target.proxy.clone(),
        transport_profile: target.transport_profile.clone(),
        timeouts: Some(ExecutionTimeouts {
            connect_ms: Some(TLS_PROBE_CONNECT_TIMEOUT_MS),
            read_ms: Some(TLS_PROBE_READ_TIMEOUT_MS),
            write_ms: Some(TLS_PROBE_READ_TIMEOUT_MS),
            pool_ms: Some(TLS_PROBE_CONNECT_TIMEOUT_MS),
            total_ms: Some(TLS_PROBE_TOTAL_TIMEOUT_MS),
            first_byte_ms: None,
        }),
    };
    crate::execution_runtime::transport::with_upstream_response_body_limit(
        &plan,
        TLS_PROBE_RESPONSE_BODY_LIMIT_BYTES,
    )
}

fn bounded_fingerprint_string(value: Option<&Value>, max_chars: usize) -> Option<String> {
    let value = value?.as_str()?.trim();
    (!value.is_empty()
        && value.chars().count() <= max_chars
        && value.chars().all(|ch| ch.is_ascii_graphic()))
    .then(|| value.to_string())
}

/// 把回显服务的 JSON 提炼成落库 / 返回给前端的探针记录。
///
/// 支持 tls.peet.ws 的嵌套字段与 tls.browserleaks.com 的根级字段。
/// 字段缺失时逐项跳过；
/// `ja3_hash` 与 `ja4` 都缺失视为探针失败。
pub(crate) fn parse_tls_probe_response(
    body: &Value,
    target: &TlsProbeTarget,
    probed_at_unix_secs: u64,
) -> Result<Value, TlsProbeError> {
    let tls = body
        .get("tls")
        .and_then(Value::as_object)
        .or_else(|| body.as_object());
    let ja3_hash = tls
        .and_then(|tls| bounded_fingerprint_string(tls.get("ja3_hash"), 64))
        .filter(|value| {
            value.len() == 32
                && value
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        });
    let ja4 = tls.and_then(|tls| bounded_fingerprint_string(tls.get("ja4"), 64));
    if ja3_hash.is_none() && ja4.is_none() {
        return Err(TlsProbeError::new(
            "探针回显缺少 ja3_hash / ja4，无法识别回显服务的响应形状",
        ));
    }
    let backend = target
        .transport_profile
        .as_ref()
        .map(|profile| profile.backend.trim().to_string())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| TRANSPORT_BACKEND_REQWEST_RUSTLS.to_string());
    let tls_stack = if backend.eq_ignore_ascii_case(TRANSPORT_BACKEND_BROWSER_WREQ) {
        "boringssl_wreq"
    } else {
        "rustls"
    };
    let mut record = Map::new();
    record.insert("observed".to_string(), Value::Bool(true));
    record.insert(
        "probe_url".to_string(),
        Value::String(target.probe_url.clone()),
    );
    record.insert(
        "probed_at_unix_secs".to_string(),
        json!(probed_at_unix_secs),
    );
    record.insert(
        "emulation_profile".to_string(),
        target
            .transport_profile
            .as_ref()
            .and_then(transport_profile_emulation_id)
            .map(Value::String)
            .unwrap_or(Value::Null),
    );
    record.insert(
        "profile_id".to_string(),
        target
            .transport_profile
            .as_ref()
            .map(|profile| Value::String(profile.profile_id.clone()))
            .unwrap_or(Value::Null),
    );
    record.insert("backend".to_string(), Value::String(backend));
    record.insert(
        "tls_stack".to_string(),
        Value::String(tls_stack.to_string()),
    );
    if let Some(value) = bounded_fingerprint_string(body.get("http_version"), 16) {
        record.insert("http_version".to_string(), Value::String(value));
    }
    if let Some(tls) = tls {
        if let Some(value) = bounded_fingerprint_string(
            tls.get("ja3").or_else(|| tls.get("ja3_text")),
            TLS_PROBE_MAX_FINGERPRINT_CHARS,
        ) {
            record.insert("ja3".to_string(), Value::String(value));
        }
        if let Some(value) = ja3_hash {
            record.insert("ja3_hash".to_string(), Value::String(value));
        }
        if let Some(value) = ja4 {
            record.insert("ja4".to_string(), Value::String(value));
        }
        for key in ["peetprint", "peetprint_hash", "tls_version_negotiated"] {
            if let Some(value) =
                bounded_fingerprint_string(tls.get(key), TLS_PROBE_MAX_FINGERPRINT_CHARS)
            {
                record.insert(key.to_string(), Value::String(value));
            }
        }
    }
    if let Some(http2) = body.get("http2").and_then(Value::as_object) {
        for key in ["akamai_fingerprint", "akamai_fingerprint_hash"] {
            if let Some(value) =
                bounded_fingerprint_string(http2.get(key), TLS_PROBE_MAX_FINGERPRINT_CHARS)
            {
                record.insert(key.to_string(), Value::String(value));
            }
        }
    } else {
        for (source, key) in [
            ("akamai_text", "akamai_fingerprint"),
            ("akamai_hash", "akamai_fingerprint_hash"),
        ] {
            if let Some(value) =
                bounded_fingerprint_string(body.get(source), TLS_PROBE_MAX_FINGERPRINT_CHARS)
            {
                record.insert(key.to_string(), Value::String(value));
            }
        }
    }
    if let Some(headers) = body
        .get("http1")
        .and_then(|http1| http1.get("headers"))
        .and_then(Value::as_array)
    {
        // 回显里的头是 `Name: value` 行；只落头名（顺序即指纹的一部分），不落值。
        let order = headers
            .iter()
            .filter_map(Value::as_str)
            .filter_map(|line| line.split_once(':').map(|(name, _)| name.trim()))
            .filter(|name| {
                !name.is_empty()
                    && name.chars().count() <= TLS_PROBE_MAX_HEADER_NAME_CHARS
                    && name
                        .chars()
                        .all(|ch| ch.is_ascii_alphanumeric() || ch == '-' || ch == '_')
            })
            .take(TLS_PROBE_MAX_HEADER_ORDER_ENTRIES)
            .map(|name| Value::String(name.to_string()))
            .collect::<Vec<_>>();
        if !order.is_empty() {
            record.insert("http1_header_order".to_string(), Value::Array(order));
        }
    }
    Ok(Value::Object(record))
}

fn execution_result_json(result: &ExecutionResult) -> Option<Value> {
    let body = result.body.as_ref()?;
    if let Some(json_body) = body.json_body.as_ref() {
        return Some(json_body.clone());
    }
    let bytes = crate::execution_runtime::transport::decode_base64_body_with_limit(
        body.body_bytes_b64.as_deref()?,
        TLS_PROBE_RESPONSE_BODY_LIMIT_BYTES,
    )
    .ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// 按 Key 解析探针目标：传输 profile 与代理来自第一个可用端点的传输快照；没有端点时退回
/// 只看供应商 / Key 的 `fingerprint.transport_profile`（代理为空）。
async fn resolve_tls_probe_target(
    app: &AppState,
    provider: &StoredProviderCatalogProvider,
    key: &StoredProviderCatalogKey,
    probe_url: String,
) -> Result<TlsProbeTarget, TlsProbeError> {
    let endpoints = app
        .list_provider_catalog_endpoints_by_provider_ids(std::slice::from_ref(&provider.id))
        .await?;
    let endpoint = endpoints
        .iter()
        .find(|endpoint| endpoint.is_active)
        .or_else(|| endpoints.first());
    let user_agent = provider
        .provider_type
        .trim()
        .eq_ignore_ascii_case("claude_code")
        .then(|| claude_code::current_claude_code_transport_identity_profile().user_agent());
    if let Some(endpoint) = endpoint {
        if let Some(transport) = app
            .read_provider_transport_snapshot_uncached(&provider.id, &endpoint.id, &key.id)
            .await?
        {
            return Ok(
                tls_probe_target_from_snapshot(app, &transport, probe_url, user_agent).await,
            );
        }
    }
    let configured = configured_transport_profile_id_from_fingerprint(key.fingerprint.as_ref())
        .or_else(|| {
            configured_transport_profile_id_from_fingerprint(
                provider
                    .config
                    .as_ref()
                    .and_then(|config| config.get("fingerprint")),
            )
        });
    Ok(TlsProbeTarget {
        probe_url,
        transport_profile: configured
            .as_deref()
            .and_then(builtin_tls_emulation_transport_profile)
            .filter(|profile| {
                claude_code::selectable_tls_emulation_profiles_for_provider_type(
                    &provider.provider_type,
                )
                .contains(&profile.profile_id.as_str())
            }),
        proxy: None,
        user_agent,
    })
}

async fn tls_probe_target_from_snapshot(
    app: &AppState,
    transport: &GatewayProviderTransportSnapshot,
    probe_url: String,
    user_agent: Option<String>,
) -> TlsProbeTarget {
    let mut transport_profile = resolve_transport_profile(transport);
    if let Some(profile) = transport_profile.as_mut() {
        // 探针是 GET，不带 body；头顺序按 messages 形状即可。旧的探针摘要不带进本次探针。
        if let Some(extra) = profile.extra.as_mut().and_then(Value::as_object_mut) {
            extra.remove(TRANSPORT_TLS_PROBE_EXTRA_KEY);
        }
    }
    let proxy = app
        .resolve_transport_proxy_snapshot_with_tunnel_affinity(transport)
        .await;
    TlsProbeTarget {
        probe_url,
        transport_profile,
        proxy,
        user_agent,
    }
}

/// 管理端入口：跑一次探针并把结果写进 `upstream_metadata.tls_probe`，返回同一 JSON。
pub(crate) async fn run_tls_probe_for_key(
    app: &AppState,
    provider: &StoredProviderCatalogProvider,
    key: &StoredProviderCatalogKey,
) -> Result<Value, TlsProbeError> {
    let configured_url = app
        .read_system_config_json_value(TLS_PROBE_URL_SYSTEM_CONFIG_KEY)
        .await
        .ok()
        .flatten()
        .and_then(|value| value.as_str().map(ToOwned::to_owned));
    let probe_url = resolve_tls_probe_url(configured_url.as_deref());
    let target = resolve_tls_probe_target(app, provider, key, probe_url).await?;
    let plan = build_tls_probe_plan(provider, key, &target);
    let result = crate::execution_runtime::execute_execution_runtime_sync_plan(app, None, &plan)
        .await
        .map_err(|error| TlsProbeError::new(format!("探针请求失败: {}", error.into_message())))?;
    if !(200..300).contains(&result.status_code) {
        return Err(TlsProbeError::new(format!(
            "探针服务返回 HTTP {}",
            result.status_code
        )));
    }
    let body = execution_result_json(&result)
        .ok_or_else(|| TlsProbeError::new("探针服务返回的不是 JSON"))?;
    let record = parse_tls_probe_response(&body, &target, current_unix_secs())?;
    persist_tls_probe_result(app, &key.id, &record).await?;
    Ok(record)
}

/// CAS 写入 `upstream_metadata.tls_probe`（整体覆盖），写完清传输快照缓存，让下一次请求
/// 能把探针摘要带进出站 TLS 记录。
pub(crate) async fn persist_tls_probe_result(
    app: &AppState,
    key_id: &str,
    record: &Value,
) -> Result<bool, TlsProbeError> {
    let now_unix_secs = current_unix_secs();
    for attempt in 0..RUNTIME_METADATA_CAS_MAX_ATTEMPTS {
        let Some(key) = app
            .read_provider_catalog_keys_by_ids(std::slice::from_ref(&key_id.to_string()))
            .await?
            .into_iter()
            .next()
        else {
            return Ok(false);
        };
        let expected = key
            .upstream_metadata
            .as_ref()
            .and_then(Value::as_object)
            .and_then(|metadata| metadata.get(TLS_PROBE_UPSTREAM_METADATA_NAMESPACE))
            .cloned();
        let persisted = app
            .update_provider_catalog_key_runtime_metadata(
                &ProviderCatalogKeyRuntimeMetadataUpdate {
                    key_id: key_id.to_string(),
                    namespace: TLS_PROBE_UPSTREAM_METADATA_NAMESPACE.to_string(),
                    expected_upstream_metadata_value: expected,
                    upstream_metadata_value: record.clone(),
                    status_snapshot_patch: Value::Object(Map::new()),
                    updated_at_unix_secs: Some(now_unix_secs),
                },
            )
            .await?;
        if persisted {
            app.clear_provider_transport_snapshot_cache();
            return Ok(true);
        }
        if attempt + 1 < RUNTIME_METADATA_CAS_MAX_ATTEMPTS {
            tokio::time::sleep(Duration::from_micros(50 * (attempt as u64 + 1))).await;
        }
    }
    warn!(
        event_name = "tls_probe_persist_cas_exhausted",
        log_type = "ops",
        key_id = %key_id,
        "gateway failed to persist tls probe result after CAS retries"
    );
    Err(TlsProbeError::new("探针结果写入 Key 元数据失败，请重试"))
}

/// 管理端 Key 载荷里的探针摘要（只读）。
pub(crate) fn tls_probe_summary(upstream_metadata: Option<&Value>) -> Option<Value> {
    crate::provider_transport::tls_probe_summary_from_upstream_metadata(upstream_metadata)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SYNTHETIC_PROBE_FIXTURE: &str =
        include_str!("../../../tests/fixtures/claude_code/tls_probe_synthetic.json");

    fn node_target() -> TlsProbeTarget {
        TlsProbeTarget {
            probe_url: TLS_PROBE_DEFAULT_URL.to_string(),
            transport_profile: builtin_tls_emulation_transport_profile("claude_code_node_openssl"),
            proxy: None,
            user_agent: Some("claude-cli/2.1.161 (external, cli)".to_string()),
        }
    }

    #[test]
    fn probe_url_is_restricted_to_the_allow_list() {
        assert_eq!(resolve_tls_probe_url(None), TLS_PROBE_DEFAULT_URL);
        assert_eq!(
            resolve_tls_probe_url(Some("https://tls.browserleaks.com/json")),
            "https://tls.browserleaks.com/json"
        );
        assert_eq!(
            resolve_tls_probe_url(Some("https://evil.example/api/all")),
            TLS_PROBE_DEFAULT_URL
        );
        assert_eq!(
            resolve_tls_probe_url(Some("http://tls.peet.ws/api/all")),
            TLS_PROBE_DEFAULT_URL
        );
    }

    #[test]
    fn synthetic_probe_fixture_parses_into_the_persisted_record_shape() {
        let body: Value = serde_json::from_str(SYNTHETIC_PROBE_FIXTURE).expect("fixture json");
        assert_eq!(body["_synthetic"], true, "fixture must be marked synthetic");
        let record =
            parse_tls_probe_response(&body, &node_target(), 1_760_000_000).expect("record");
        assert_eq!(record["observed"], true);
        assert_eq!(record["probe_url"], TLS_PROBE_DEFAULT_URL);
        assert_eq!(record["probed_at_unix_secs"], 1_760_000_000u64);
        assert_eq!(record["emulation_profile"], "claude_code_node_openssl");
        assert_eq!(record["profile_id"], "claude_code_node_openssl");
        assert_eq!(record["backend"], TRANSPORT_BACKEND_BROWSER_WREQ);
        assert_eq!(record["tls_stack"], "boringssl_wreq");
        assert_eq!(record["http_version"], "h1");
        assert_eq!(record["ja3_hash"], body["tls"]["ja3_hash"]);
        assert_eq!(record["ja4"], body["tls"]["ja4"]);
        assert_eq!(record["ja3"], body["tls"]["ja3"]);
        assert_eq!(record["peetprint"], body["tls"]["peetprint"]);
        assert_eq!(record["tls_version_negotiated"], "772");
        assert!(
            record.get("akamai_fingerprint").is_none(),
            "HTTP/1.1 probe has no h2 frames"
        );
        let order = record["http1_header_order"]
            .as_array()
            .expect("header order");
        assert_eq!(order[0], "Host");
        assert!(order.iter().any(|name| name == "User-Agent"));
        // 头值不落库。
        assert!(!record.to_string().contains("claude-cli/2.1.161"));

        let summary =
            tls_probe_summary(Some(&json!({"tls_probe": record.clone()}))).expect("summary");
        assert_eq!(summary["ja4"], record["ja4"]);
        assert_eq!(summary["observed"], true);
        assert!(tls_probe_summary(Some(&json!({"tls_probe": {"observed": false}}))).is_none());
        assert!(tls_probe_summary(None).is_none());
    }

    #[test]
    fn probe_parse_rejects_bodies_without_fingerprints_and_drops_malformed_fields() {
        let error = parse_tls_probe_response(&json!({"ip": "203.0.113.9"}), &node_target(), 1)
            .expect_err("missing fingerprints must fail");
        assert!(error.message.contains("ja3_hash"));

        let record = parse_tls_probe_response(
            &json!({
                "http_version": "h2",
                "tls": {
                    "ja3_hash": "NOT-A-HASH",
                    "ja4": "t13d1516h2_8daaf6152771_02713d6af862",
                    "ja3": "771,4865;DROP TABLE",
                    "peetprint": "x".repeat(600)
                },
                "http2": {"akamai_fingerprint": "1:65536;2:0;4:6291456|15663105|0|m,a,s,p"},
                "http1": {"headers": ["Host: example", "Bad Header Name: x", "no-colon"]}
            }),
            &TlsProbeTarget {
                probe_url: TLS_PROBE_DEFAULT_URL.to_string(),
                transport_profile: None,
                proxy: None,
                user_agent: None,
            },
            7,
        )
        .expect("ja4 alone is enough");
        assert!(record.get("ja3_hash").is_none());
        assert_eq!(record["ja4"], "t13d1516h2_8daaf6152771_02713d6af862");
        // 含空格的 ja3 不是合法指纹串。
        assert!(record.get("ja3").is_none());
        assert!(record.get("peetprint").is_none());
        assert_eq!(
            record["akamai_fingerprint"],
            "1:65536;2:0;4:6291456|15663105|0|m,a,s,p"
        );
        assert_eq!(record["http1_header_order"], json!(["Host"]));
        assert_eq!(record["emulation_profile"], Value::Null);
        assert_eq!(record["backend"], TRANSPORT_BACKEND_REQWEST_RUSTLS);
        assert_eq!(record["tls_stack"], "rustls");
    }

    #[test]
    fn browserleaks_root_fields_parse_into_the_same_probe_record() {
        // tls.browserleaks.com/json uses root-level *_text fields, unlike peet.ws.
        let body = json!({
            "user_agent": "must-not-be-persisted",
            "ja3_hash": "0123456789abcdef0123456789abcdef",
            "ja3_text": "771,4865-4866-4867,0-23-65281-10-11,29-23-24,0",
            "ja4": "t13d1712h2_5b57614c22b0_d5fe2c511efa",
            "akamai_hash": "abcdef0123456789abcdef0123456789",
            "akamai_text": "2:0;1:4096;8:0;3:1024;4:1048576|25100289|0|a,p,m,s"
        });
        let mut target = node_target();
        target.probe_url = "https://tls.browserleaks.com/json".into();
        let record = parse_tls_probe_response(&body, &target, 7).expect("browserleaks record");
        assert_eq!(record["ja3_hash"], body["ja3_hash"]);
        assert_eq!(record["ja3"], body["ja3_text"]);
        assert_eq!(record["ja4"], body["ja4"]);
        assert_eq!(record["akamai_fingerprint"], body["akamai_text"]);
        assert_eq!(record["akamai_fingerprint_hash"], body["akamai_hash"]);
        assert_eq!(record["observed"], true);
        assert!(!record.to_string().contains("must-not-be-persisted"));
    }

    #[test]
    fn probe_plan_uses_the_key_transport_profile_and_never_follows_redirects() {
        let provider = StoredProviderCatalogProvider::new(
            "provider-claude".to_string(),
            "claude".to_string(),
            None,
            "claude_code".to_string(),
        )
        .expect("provider");
        let key = StoredProviderCatalogKey::new(
            "key-1".to_string(),
            "provider-claude".to_string(),
            "default".to_string(),
            "oauth".to_string(),
            None,
            true,
        )
        .expect("key");
        let plan = build_tls_probe_plan(&provider, &key, &node_target());
        assert_eq!(plan.method, "GET");
        assert_eq!(plan.url, TLS_PROBE_DEFAULT_URL);
        assert_eq!(plan.request_id, "admin-tls-probe:key-1");
        assert_eq!(
            plan.transport_profile
                .as_ref()
                .map(|profile| profile.backend.as_str()),
            Some(TRANSPORT_BACKEND_BROWSER_WREQ)
        );
        assert_eq!(
            plan.headers.get("user-agent").map(String::as_str),
            Some("claude-cli/2.1.161 (external, cli)")
        );
        assert_eq!(
            plan.headers
                .get(aether_contracts::EXECUTION_REQUEST_FOLLOW_REDIRECTS_HEADER)
                .map(String::as_str),
            Some("false")
        );
        assert!(plan.body.json_body.is_none() && plan.body.body_bytes_b64.is_none());
        assert_eq!(
            crate::execution_runtime::transport::execution_plan_response_body_limit_bytes(&plan),
            TLS_PROBE_RESPONSE_BODY_LIMIT_BYTES
        );
    }
}
