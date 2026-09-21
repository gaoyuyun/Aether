//! Claude Code 客户端策略在网关侧的接线（计划 P2）。
//!
//! 输入：网关收到的原始客户端头 + 最终 provider 请求体 + 传输快照；
//! 输出：改写后的 body（第三方）或原样（原生）、线上头策略、写入 report_context 的
//! `claude_code_cloak` 字段，以及需要持久化的设备 profile。
//!
//! 顺序由 transport 层的 `claude_code::cloak` 流水线固定：敏感词混淆（P6）→ thinking →
//! 身份/计费头 → cache_control → CCH 签名。签名之后 body 不再改动，`family/request.rs`
//! 在 body 全部就绪（模型映射、body_rules、操作不变量）后才调用这里。

use std::time::Duration;

use aether_cache::ExpiringMap;
use aether_data_contracts::repository::provider_catalog::ProviderCatalogKeyRuntimeMetadataUpdate;
use serde_json::{json, Value};
use tracing::warn;

use crate::ai_serving::transport::claude_code::{
    apply_claude_code_cloak_pipeline, assemble_claude_code_beta_header, claude_code_cloak_applies,
    claude_code_request_is_subagent, current_claude_code_transport_identity_profile,
    derive_claude_code_account_uuid, detect_claude_code_client, resolve_claude_code_cloak_mode,
    resolve_claude_code_device_profile, ClaudeCodeBetaContext, ClaudeCodeClientDetection,
    ClaudeCodeCloakPipeline, ClaudeCodeDeviceBaseline, ClaudeCodeDeviceCandidate,
    ClaudeCodeDeviceProfile, ClaudeCodeIdentityInput, CLAUDE_CODE_DEVICE_PROFILE_KEY,
};
use crate::ai_serving::transport::{
    apply_sensitive_word_obfuscation, resolve_sensitive_word_list, ClaudeCodeWirePolicy,
    GatewayProviderTransportSnapshot,
};
use crate::clock::current_unix_secs;
use crate::AppState;

/// report_context 里的字段名。
pub(crate) const CLAUDE_CODE_CLOAK_REPORT_FIELD: &str = "claude_code_cloak";
/// `upstream_metadata` 里设备 profile 的命名空间（与 `auth_config.device_profile` 镜像，
/// 运行时走 CAS 元数据通道，不碰加密的 auth_config）。
pub(crate) const CLAUDE_CODE_DEVICE_METADATA_NAMESPACE: &str = "claude_code_device";
const DEVICE_PROFILE_PERSIST_GATE_TTL: Duration = Duration::from_secs(60);
const DEVICE_PROFILE_PERSIST_GATE_MAX_ENTRIES: usize = 20_000;
const RUNTIME_METADATA_CAS_MAX_ATTEMPTS: usize = 4;

static DEVICE_PROFILE_PERSIST_GATE: std::sync::LazyLock<ExpiringMap<String, ()>> =
    std::sync::LazyLock::new(ExpiringMap::new);

/// 一次请求的伪装决策。
#[derive(Debug, Clone)]
pub(crate) struct ClaudeCodeCloakOutcome {
    /// 传给头构建器的策略。
    pub(crate) wire_beta_header: Option<String>,
    pub(crate) native_passthrough: bool,
    /// 写入 report_context 的值。
    pub(crate) report: Value,
    /// 需要写回的设备 profile（与库里不同才有）。
    pub(crate) device_profile_update: Option<ClaudeCodeDeviceProfile>,
}

impl ClaudeCodeCloakOutcome {
    pub(crate) fn wire_policy(&self) -> Option<ClaudeCodeWirePolicy<'_>> {
        if self.native_passthrough {
            return Some(ClaudeCodeWirePolicy::NativePassthrough);
        }
        self.wire_beta_header
            .as_deref()
            .map(|beta_header| ClaudeCodeWirePolicy::Cloaked { beta_header })
    }
}

/// 早期识别结果：在构建 provider 请求体之前就要知道是否原生，因为原生请求连
/// 历史的 body 清洗（thinking 块过滤、计费头版本同步）都不能做。
#[derive(Debug, Clone)]
pub(crate) struct ClaudeCodeClientPolicy {
    pub(crate) detection: ClaudeCodeClientDetection,
    pub(crate) mode: crate::ai_serving::transport::claude_code::ClaudeCodeCloakMode,
    pub(crate) applies: bool,
}

impl ClaudeCodeClientPolicy {
    pub(crate) fn native_passthrough(&self) -> bool {
        self.detection.kind.is_native()
    }
}

/// 识别客户端并读取供应商 `config.cloak.mode`。返回 `None` 表示不是 claude_code 传输。
pub(crate) fn resolve_claude_code_client_policy(
    transport: &GatewayProviderTransportSnapshot,
    client_headers: &http::HeaderMap,
    client_body: Option<&Value>,
    api_operation: Option<crate::ai_serving::ApiOperation>,
) -> Option<ClaudeCodeClientPolicy> {
    if !transport
        .provider
        .provider_type
        .trim()
        .eq_ignore_ascii_case("claude_code")
    {
        return None;
    }
    let count_tokens = api_operation == Some(crate::ai_serving::ApiOperation::ClaudeCountTokens);
    let detection = detect_claude_code_client(client_headers, client_body, count_tokens);
    let mode = resolve_claude_code_cloak_mode(transport.provider.config.as_ref());
    let applies = claude_code_cloak_applies(detection.kind, mode);
    Some(ClaudeCodeClientPolicy {
        detection,
        mode,
        applies,
    })
}

/// 对最终的 provider 请求体应用客户端策略（第三方改写 / 原生透传）。
pub(crate) fn apply_claude_code_client_policy(
    policy: &ClaudeCodeClientPolicy,
    transport: &GatewayProviderTransportSnapshot,
    client_headers: &http::HeaderMap,
    provider_request_body: &mut Value,
    api_operation: Option<crate::ai_serving::ApiOperation>,
    session_id: Option<&str>,
) -> ClaudeCodeCloakOutcome {
    let count_tokens = api_operation == Some(crate::ai_serving::ApiOperation::ClaudeCountTokens);
    let detection = &policy.detection;
    let mode = policy.mode;

    if !policy.applies {
        return ClaudeCodeCloakOutcome {
            wire_beta_header: None,
            native_passthrough: detection.kind.is_native(),
            report: json!({
                "applied": false,
                "mode": mode.as_str(),
                "client": detection.to_json(),
                "passthrough": detection.kind.is_native(),
            }),
            device_profile_update: None,
        };
    }

    let profile = *current_claude_code_transport_identity_profile();
    let now_unix_secs = current_unix_secs();
    let auth_config = parse_auth_config(transport.key.decrypted_auth_config.as_deref());
    let oauth_credential = transport.key.auth_type.trim().eq_ignore_ascii_case("oauth")
        || transport
            .key
            .decrypted_api_key
            .trim_start()
            .starts_with("sk-ant-oat");
    let is_subagent =
        claude_code_request_is_subagent(Some(client_headers), Some(provider_request_body));

    // 设备 profile：优先运行时元数据（CAS 通道），其次 auth_config 里的持久化值。
    let stored_profile = transport
        .key
        .upstream_metadata
        .as_ref()
        .and_then(|metadata| metadata.get(CLAUDE_CODE_DEVICE_METADATA_NAMESPACE))
        .and_then(ClaudeCodeDeviceProfile::from_value)
        .or_else(|| {
            auth_config
                .as_ref()
                .and_then(|config| config.get(CLAUDE_CODE_DEVICE_PROFILE_KEY))
                .and_then(ClaudeCodeDeviceProfile::from_value)
        });
    let baseline = ClaudeCodeDeviceBaseline {
        cli_version: profile.cli_version().to_string(),
        package_version: profile.stainless_package_version().to_string(),
        runtime_version: profile.stainless_runtime_version().to_string(),
        os: profile.stainless_os().to_string(),
        arch: profile.stainless_arch().to_string(),
    };
    let candidate = ClaudeCodeDeviceCandidate::from_headers(client_headers);
    let device_seed = device_seed(transport);
    let resolution = resolve_claude_code_device_profile(
        stored_profile.as_ref(),
        &candidate,
        &baseline,
        &transport.key.id,
        &device_seed,
        now_unix_secs,
    );
    let account_uuid = auth_config
        .as_ref()
        .and_then(|config| config.get("account_uuid"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
        .unwrap_or_else(|| derive_claude_code_account_uuid(&transport.key.id));

    // P6: sensitive word obfuscation hook —— 词表为空时 no-op；始终在身份改写与签名之前。
    let provider_type = transport.provider.provider_type.clone();
    let word_list = resolve_sensitive_word_list(
        transport.provider.config.as_ref(),
        transport.key.decrypted_auth_config.as_deref(),
    );
    let mut sensitive_words_hook = move |body: &mut Value| -> Option<Value> {
        if word_list.is_empty() {
            return None;
        }
        Some(apply_sensitive_word_obfuscation(body, &provider_type, &word_list).to_json())
    };

    let entrypoint = detection.entrypoint.clone();
    let pipeline_result = apply_claude_code_cloak_pipeline(
        provider_request_body,
        ClaudeCodeCloakPipeline {
            profile,
            detection,
            oauth_credential,
            is_subagent,
            identity: ClaudeCodeIdentityInput {
                device_id: &resolution.profile.device_id,
                account_uuid: &account_uuid,
                session_id,
            },
            entrypoint: entrypoint.as_deref(),
            sign: oauth_credential && !count_tokens,
            sensitive_words_hook: Some(&mut sensitive_words_hook),
        },
    );
    let mut report = match pipeline_result {
        Ok(report) => report.to_json(),
        Err(error) => {
            warn!(
                event_name = "claude_code_cloak_pipeline_failed",
                log_type = "ops",
                provider_id = %transport.provider.id,
                key_id = %transport.key.id,
                error = %error,
                "claude code cloak pipeline failed; sending body without signature"
            );
            json!({"applied": true, "client": detection.to_json(), "error": error.message()})
        }
    };
    if let Some(object) = report.as_object_mut() {
        object.insert("mode".to_string(), json!(mode.as_str()));
        object.insert("passthrough".to_string(), json!(false));
        object.insert(
            "device_profile".to_string(),
            resolution.profile.summary_value(),
        );
    }

    let requested_beta = client_headers
        .get_all("anthropic-beta")
        .iter()
        .filter_map(|value| value.to_str().ok())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .collect::<Vec<_>>()
        .join(",");
    let beta_header = assemble_claude_code_beta_header(ClaudeCodeBetaContext {
        profile,
        operation: api_operation,
        oauth_credential,
        is_subagent,
        requested: (!requested_beta.is_empty()).then_some(requested_beta.as_str()),
        body: Some(provider_request_body),
    });

    ClaudeCodeCloakOutcome {
        wire_beta_header: Some(beta_header),
        native_passthrough: false,
        report,
        device_profile_update: resolution.changed.then_some(resolution.profile),
    }
}

fn parse_auth_config(raw: Option<&str>) -> Option<serde_json::Map<String, Value>> {
    let raw = raw?.trim();
    if raw.is_empty() {
        return None;
    }
    serde_json::from_str::<Value>(raw)
        .ok()?
        .as_object()
        .cloned()
}

/// device_id 派生种子：优先 OAuth 账号 uuid（同一账号换 Key 也是同一"设备"），
/// 否则用 Key ID 本身；管理端「重置设备身份」后带上重置代数，派生出新的 device_id。
fn device_seed(transport: &GatewayProviderTransportSnapshot) -> String {
    let base = parse_auth_config(transport.key.decrypted_auth_config.as_deref())
        .and_then(|config| {
            config
                .get("account_uuid")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(ToOwned::to_owned)
        })
        .unwrap_or_else(|| transport.key.id.clone());
    let reset_generation = transport
        .key
        .upstream_metadata
        .as_ref()
        .and_then(|metadata| metadata.get(CLAUDE_CODE_DEVICE_METADATA_NAMESPACE))
        .and_then(|value| value.get("reset_generation"))
        .and_then(Value::as_u64)
        .unwrap_or(0);
    if reset_generation == 0 {
        base
    } else {
        format!("{base}#reset{reset_generation}")
    }
}

/// 把变化后的设备 profile 写回 `upstream_metadata.claude_code_device`（CAS，后台执行）。
/// 一分钟内同一把 Key 只写一次，避免热路径上反复 CAS。
pub(crate) fn spawn_persist_claude_code_device_profile(
    state: &AppState,
    key_id: &str,
    profile: ClaudeCodeDeviceProfile,
) {
    let key_id = key_id.trim().to_string();
    if key_id.is_empty() {
        return;
    }
    if DEVICE_PROFILE_PERSIST_GATE.contains_fresh(&key_id, DEVICE_PROFILE_PERSIST_GATE_TTL) {
        return;
    }
    DEVICE_PROFILE_PERSIST_GATE.insert(
        key_id.clone(),
        (),
        DEVICE_PROFILE_PERSIST_GATE_TTL,
        DEVICE_PROFILE_PERSIST_GATE_MAX_ENTRIES,
    );
    let state = state.clone();
    tokio::spawn(async move {
        if let Err(error) = persist_claude_code_device_profile(&state, &key_id, &profile).await {
            warn!(
                event_name = "claude_code_device_profile_persist_failed",
                log_type = "ops",
                key_id = %key_id,
                error = ?error,
                "gateway failed to persist claude code device profile"
            );
        }
    });
}

pub(crate) async fn persist_claude_code_device_profile(
    state: &AppState,
    key_id: &str,
    profile: &ClaudeCodeDeviceProfile,
) -> Result<bool, crate::GatewayError> {
    let now_unix_secs = current_unix_secs();
    for attempt in 0..RUNTIME_METADATA_CAS_MAX_ATTEMPTS {
        let Some(key) = state
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
            .and_then(|metadata| metadata.get(CLAUDE_CODE_DEVICE_METADATA_NAMESPACE))
            .cloned();
        // 库里已经是同一个（或更新的）profile：不写。
        if let Some(stored) = expected
            .as_ref()
            .and_then(ClaudeCodeDeviceProfile::from_value)
        {
            if stored == *profile {
                return Ok(false);
            }
            if stored.updated_at_unix_secs > profile.updated_at_unix_secs {
                return Ok(false);
            }
        }
        let mut next_value = profile.to_value();
        if let Some(generation) = expected
            .as_ref()
            .and_then(|value| value.get("reset_generation"))
            .cloned()
        {
            if let Some(object) = next_value.as_object_mut() {
                object.insert("reset_generation".to_string(), generation);
            }
        }
        let persisted = state
            .update_provider_catalog_key_runtime_metadata(
                &ProviderCatalogKeyRuntimeMetadataUpdate {
                    key_id: key_id.to_string(),
                    namespace: CLAUDE_CODE_DEVICE_METADATA_NAMESPACE.to_string(),
                    expected_upstream_metadata_value: expected,
                    upstream_metadata_value: next_value,
                    status_snapshot_patch: Value::Object(serde_json::Map::new()),
                    updated_at_unix_secs: Some(now_unix_secs),
                },
            )
            .await?;
        if persisted {
            return Ok(true);
        }
        if attempt + 1 < RUNTIME_METADATA_CAS_MAX_ATTEMPTS {
            tokio::time::sleep(Duration::from_micros(50 * (attempt as u64 + 1))).await;
        }
    }
    Ok(false)
}

/// 管理端「重置设备身份」：删除运行时元数据里的 profile，下一次请求重新派生。
pub(crate) async fn reset_claude_code_device_profile(
    state: &AppState,
    key_id: &str,
) -> Result<bool, crate::GatewayError> {
    let now_unix_secs = current_unix_secs();
    for attempt in 0..RUNTIME_METADATA_CAS_MAX_ATTEMPTS {
        let Some(key) = state
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
            .and_then(|metadata| metadata.get(CLAUDE_CODE_DEVICE_METADATA_NAMESPACE))
            .cloned();
        if expected.is_none() {
            return Ok(false);
        }
        // 写一个带 reset 标记的空 profile：`from_value` 解析失败即视为"无 profile"，
        // 下次请求会重新派生一个新的 device_id（seed 加上 reset 代数）。
        let generation = expected
            .as_ref()
            .and_then(|value| value.get("reset_generation"))
            .and_then(Value::as_u64)
            .unwrap_or(0)
            .saturating_add(1);
        let persisted = state
            .update_provider_catalog_key_runtime_metadata(
                &ProviderCatalogKeyRuntimeMetadataUpdate {
                    key_id: key_id.to_string(),
                    namespace: CLAUDE_CODE_DEVICE_METADATA_NAMESPACE.to_string(),
                    expected_upstream_metadata_value: expected,
                    upstream_metadata_value: json!({
                        "reset_generation": generation,
                        "reset_at_unix_secs": now_unix_secs,
                    }),
                    status_snapshot_patch: Value::Object(serde_json::Map::new()),
                    updated_at_unix_secs: Some(now_unix_secs),
                },
            )
            .await?;
        if persisted {
            return Ok(true);
        }
        if attempt + 1 < RUNTIME_METADATA_CAS_MAX_ATTEMPTS {
            tokio::time::sleep(Duration::from_micros(50 * (attempt as u64 + 1))).await;
        }
    }
    Ok(false)
}

/// 管理端展示用的设备 profile 摘要。
pub(crate) fn claude_code_device_profile_summary(
    upstream_metadata: Option<&Value>,
    auth_config: Option<&serde_json::Map<String, Value>>,
) -> Option<Value> {
    upstream_metadata
        .and_then(|metadata| metadata.get(CLAUDE_CODE_DEVICE_METADATA_NAMESPACE))
        .and_then(ClaudeCodeDeviceProfile::from_value)
        .or_else(|| {
            auth_config
                .and_then(|config| config.get(CLAUDE_CODE_DEVICE_PROFILE_KEY))
                .and_then(ClaudeCodeDeviceProfile::from_value)
        })
        .map(|profile| profile.summary_value())
}

#[cfg(test)]
pub(crate) fn clear_claude_code_device_profile_persist_gate_for_tests() {
    DEVICE_PROFILE_PERSIST_GATE.clear();
}
