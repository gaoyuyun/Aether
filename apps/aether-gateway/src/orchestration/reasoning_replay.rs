//! 推理回放账本在网关侧的接线。
//!
//! 纯逻辑在 `aether_cache::reasoning_replay`；这里负责三件事：
//!
//! 1. 请求发往上游之前，按 `(provider_key_id, session_scope, model)` 查账本，把上一轮
//!    的推理签名插回请求体（[`apply_reasoning_replay_to_plan`]）。
//! 2. 上游成功返回后，从响应里捕获本轮签名写入账本（[`capture_reasoning_replay_from_*`]）。
//! 3. 上游 400 且错误文本涉及签名时，按代次 CAS 清掉账本条目
//!    （[`clear_reasoning_replay_on_invalid_signature`]），下一轮退回占位符行为。
//!
//! 账本存两层：进程内 [`ReasoningReplayLedger`] 做一级缓存；Redis 部署下再经运行时 KV
//! 共享，多实例之间同一会话不会因为落到不同网关实例而丢签名。KV 里的载荷用
//! 目录密钥密封，签名本身是上游加密材料，不以明文落在 Redis。
//!
//! 只在跨格式（`needs_conversion = true`）或客户端与上游格式不同族时启用：同族透传
//! 时客户端自己带着签名往返，网关不该插手。

use std::sync::LazyLock;
use std::time::Duration;

use aether_cache::{
    apply_reasoning_replay, capture_reasoning_replay_from_gemini_response,
    capture_reasoning_replay_from_openai_responses_output, capture_reasoning_replay_from_sse_text,
    error_text_indicates_invalid_reasoning_signature, reasoning_replay_report_value,
    ReasoningReplayEntry, ReasoningReplayItem, ReasoningReplayKey, ReasoningReplayLedger,
    ReasoningReplayProvider, REASONING_REPLAY_TTL,
};
use aether_contracts::ExecutionPlan;
use serde_json::Value;
use tracing::{debug, warn};

use crate::client_session_affinity::{
    client_session_affinity_from_report_context_value, CLIENT_SESSION_AFFINITY_REPORT_CONTEXT_FIELD,
};
use crate::clock::current_unix_secs;
use crate::AppState;

/// `report_context` 里记录回放结果的字段。
pub(crate) const REASONING_REPLAY_REPORT_FIELD: &str = "reasoning_replay";
const REASONING_REPLAY_SECRET_PURPOSE: &str = "reasoning-replay";
const REASONING_REPLAY_KV_TTL: Duration = REASONING_REPLAY_TTL;

static LEDGER: LazyLock<ReasoningReplayLedger> = LazyLock::new(ReasoningReplayLedger::new);

fn ledger() -> &'static ReasoningReplayLedger {
    &LEDGER
}

#[cfg(test)]
pub(crate) fn clear_reasoning_replay_ledger_for_tests() {
    LEDGER.clear_all();
}

fn report_context_str<'a>(report_context: Option<&'a Value>, field: &str) -> Option<&'a str> {
    report_context?
        .get(field)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
}

fn reasoning_replay_provider_for(
    provider_type: &str,
    provider_api_format: &str,
) -> Option<ReasoningReplayProvider> {
    let provider_type = provider_type.trim().to_ascii_lowercase();
    let api_format = crate::ai_serving::normalize_api_format_alias(provider_api_format);
    match provider_type.as_str() {
        "codex" if crate::ai_serving::is_openai_responses_family_format(api_format.as_str()) => {
            Some(ReasoningReplayProvider::Codex)
        }
        "gemini_cli" | "antigravity" | "vertex_ai" | "vertex"
            if api_format.starts_with("gemini:") =>
        {
            Some(ReasoningReplayProvider::Gemini)
        }
        _ => None,
    }
}

/// 只有跨格式时才需要回放：同族透传时客户端自己带着签名。
fn reasoning_replay_needed(report_context: Option<&Value>) -> bool {
    let Some(report_context) = report_context else {
        return false;
    };
    if report_context
        .get("needs_conversion")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        return true;
    }
    let provider = report_context_str(Some(report_context), "provider_api_format")
        .map(crate::ai_serving::normalize_api_format_alias)
        .unwrap_or_default();
    let client = report_context_str(Some(report_context), "client_api_format")
        .map(crate::ai_serving::normalize_api_format_alias)
        .unwrap_or_default();
    if provider.is_empty() || client.is_empty() {
        return false;
    }
    let same_family = provider == client
        || (crate::ai_serving::is_openai_responses_family_format(provider.as_str())
            && crate::ai_serving::is_openai_responses_family_format(client.as_str()));
    !same_family
}

/// 没有会话信号时 Gemini 系不回放的原因码（写入 `report_context.reasoning_replay.skipped_reason`）。
pub(crate) const REASONING_REPLAY_SKIP_NO_SESSION_SCOPE: &str = "no_session_scope";

fn session_scope_from_report_context(
    report_context: Option<&Value>,
    provider: ReasoningReplayProvider,
) -> Option<String> {
    let report_context = report_context?;
    if let Some(affinity) = client_session_affinity_from_report_context_value(
        report_context.get(CLIENT_SESSION_AFFINITY_REPORT_CONTEXT_FIELD),
    ) {
        if let Some(session_key) = affinity
            .session_key
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
        {
            return Some(format!("session:{session_key}"));
        }
    }
    match provider {
        // Codex 的锚点是 call_id（全局唯一的强锚点）：没有会话信号时退回 API Key 维度，
        // 同一把客户端 Key 的其它会话最多表现为「找不到锚点」，不会插错位置。
        ReasoningReplayProvider::Codex => report_context_str(Some(report_context), "api_key_id")
            .map(|api_key_id| format!("api_key:{api_key_id}")),
        // Gemini 响应里的 functionCall 通常没有 id，锚点是工具名 + 参数摘要；
        // 团队共用一把 Key 的多个会话同名同参调用会互相串扰，所以没有会话信号就不回放。
        ReasoningReplayProvider::Gemini => None,
    }
}

/// 账本键的推导结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ReasoningReplayKeyResolution {
    Key(ReasoningReplayKey),
    /// 不满足回放条件（同格式透传、非回放供应商等），请求详情里不记录。
    NotApplicable,
    /// 供应商需要回放，但缺少可靠的会话作用域；请求详情里记原因。
    Skipped {
        provider: ReasoningReplayProvider,
        reason: &'static str,
    },
}

impl ReasoningReplayKeyResolution {
    fn into_key(self) -> Option<ReasoningReplayKey> {
        match self {
            Self::Key(key) => Some(key),
            Self::NotApplicable | Self::Skipped { .. } => None,
        }
    }
}

pub(crate) fn resolve_reasoning_replay_key_for_plan(
    plan: &ExecutionPlan,
    provider_type: &str,
    report_context: Option<&Value>,
) -> ReasoningReplayKeyResolution {
    if !reasoning_replay_needed(report_context) {
        return ReasoningReplayKeyResolution::NotApplicable;
    }
    let Some(provider) = reasoning_replay_provider_for(provider_type, &plan.provider_api_format)
    else {
        return ReasoningReplayKeyResolution::NotApplicable;
    };
    let Some(session_scope) = session_scope_from_report_context(report_context, provider) else {
        return ReasoningReplayKeyResolution::Skipped {
            provider,
            reason: REASONING_REPLAY_SKIP_NO_SESSION_SCOPE,
        };
    };
    let Some(model) = plan_model(plan, report_context) else {
        return ReasoningReplayKeyResolution::NotApplicable;
    };
    match ReasoningReplayKey::new(provider, plan.key_id.as_str(), session_scope, model) {
        Some(key) => ReasoningReplayKeyResolution::Key(key),
        None => ReasoningReplayKeyResolution::NotApplicable,
    }
}

fn plan_model(plan: &ExecutionPlan, report_context: Option<&Value>) -> Option<String> {
    plan.body
        .json_body
        .as_ref()
        .and_then(|body| body.get("model"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
        .or_else(|| plan.model_name.clone())
        .or_else(|| report_context_str(report_context, "mapped_model").map(ToOwned::to_owned))
        .or_else(|| report_context_str(report_context, "model").map(ToOwned::to_owned))
}

/// 从执行计划与 report_context 推导账本键；不满足回放条件或缺少会话作用域时返回 `None`。
pub(crate) fn reasoning_replay_key_for_plan(
    plan: &ExecutionPlan,
    provider_type: &str,
    report_context: Option<&Value>,
) -> Option<ReasoningReplayKey> {
    resolve_reasoning_replay_key_for_plan(plan, provider_type, report_context).into_key()
}

async fn provider_type_for_plan(state: &AppState, plan: &ExecutionPlan) -> Option<String> {
    match state
        .read_provider_transport_snapshot_arc(&plan.provider_id, &plan.endpoint_id, &plan.key_id)
        .await
    {
        Ok(Some(snapshot)) => Some(snapshot.provider.provider_type.clone()),
        Ok(None) => None,
        Err(error) => {
            debug!(
                event_name = "reasoning_replay_transport_snapshot_unavailable",
                log_type = "debug",
                provider_id = %plan.provider_id,
                key_id = %plan.key_id,
                error = ?error,
                "reasoning replay skipped because the transport snapshot could not be read"
            );
            None
        }
    }
}

async fn read_entry(state: &AppState, key: &ReasoningReplayKey) -> Option<ReasoningReplayEntry> {
    if let Some(entry) = ledger().get(key) {
        return Some(entry);
    }
    if !state.runtime_state.is_redis() {
        return None;
    }
    let storage_key = key.storage_key();
    let stored = match state.runtime_state.kv_get(&storage_key).await {
        Ok(Some(stored)) => stored,
        Ok(None) => return None,
        Err(error) => {
            warn!(
                event_name = "reasoning_replay_read_failed",
                log_type = "ops",
                backend = state.runtime_state.backend_kind().as_str(),
                error = ?error,
                "gateway failed to read shared reasoning replay entry"
            );
            return None;
        }
    };
    let Some(plaintext) = crate::handlers::shared::open_runtime_secret_payload(
        state,
        REASONING_REPLAY_SECRET_PURPOSE,
        &stored,
    ) else {
        let _ = state.runtime_state.kv_delete(&storage_key).await;
        return None;
    };
    let entry = match serde_json::from_str::<ReasoningReplayEntry>(&plaintext) {
        Ok(entry) => entry,
        Err(_) => {
            let _ = state.runtime_state.kv_delete(&storage_key).await;
            return None;
        }
    };
    ledger().put(key, entry.clone());
    Some(entry)
}

async fn write_entry(state: &AppState, key: &ReasoningReplayKey, entry: ReasoningReplayEntry) {
    ledger().put(key, entry.clone());
    if !state.runtime_state.is_redis() {
        return;
    }
    let Ok(plaintext) = serde_json::to_string(&entry) else {
        return;
    };
    let Some(sealed) = crate::handlers::shared::seal_runtime_secret_payload(
        state,
        REASONING_REPLAY_SECRET_PURPOSE,
        &plaintext,
    ) else {
        warn!(
            event_name = "reasoning_replay_encryption_unavailable",
            log_type = "ops",
            "gateway refused to persist unencrypted reasoning replay entry"
        );
        return;
    };
    if let Err(error) = state
        .runtime_state
        .kv_set(&key.storage_key(), sealed, Some(REASONING_REPLAY_KV_TTL))
        .await
    {
        warn!(
            event_name = "reasoning_replay_write_failed",
            log_type = "ops",
            backend = state.runtime_state.backend_kind().as_str(),
            error = ?error,
            "gateway failed to persist shared reasoning replay entry"
        );
    }
}

/// 请求发往上游前：查账本并把签名插回 `plan.body.json_body`。改动写进
/// `report_context.reasoning_replay`，请求详情里能看到插了什么。
pub(crate) async fn apply_reasoning_replay_to_plan(
    state: &AppState,
    plan: &mut ExecutionPlan,
    report_context: &mut Option<Value>,
) {
    if plan.body.json_body.is_none() || !reasoning_replay_needed(report_context.as_ref()) {
        return;
    }
    let Some(provider_type) = provider_type_for_plan(state, plan).await else {
        return;
    };
    let key = match resolve_reasoning_replay_key_for_plan(
        plan,
        &provider_type,
        report_context.as_ref(),
    ) {
        ReasoningReplayKeyResolution::Key(key) => key,
        ReasoningReplayKeyResolution::NotApplicable => return,
        ReasoningReplayKeyResolution::Skipped { provider, reason } => {
            if let Some(object) = report_context.as_mut().and_then(Value::as_object_mut) {
                object.insert(
                    REASONING_REPLAY_REPORT_FIELD.to_string(),
                    serde_json::json!({
                        "provider": provider.as_str(),
                        "applied": false,
                        "skipped_reason": reason,
                    }),
                );
            }
            debug!(
                event_name = "reasoning_replay_skipped",
                log_type = "debug",
                provider = provider.as_str(),
                key_id = %plan.key_id,
                reason,
                "reasoning replay skipped because the request has no reliable session scope"
            );
            return;
        }
    };
    let Some(entry) = read_entry(state, &key).await else {
        return;
    };
    let Some(body) = plan.body.json_body.as_mut() else {
        return;
    };
    let applied = apply_reasoning_replay(body, &entry);
    let report_value = reasoning_replay_report_value(&key, entry.generation, &applied);
    if let Some(object) = report_context.as_mut().and_then(Value::as_object_mut) {
        object.insert(REASONING_REPLAY_REPORT_FIELD.to_string(), report_value);
    }
    if applied.changed() {
        debug!(
            event_name = "reasoning_replay_applied",
            log_type = "debug",
            provider = key.provider.as_str(),
            key_id = %plan.key_id,
            inserted = applied.inserted,
            restored_ids = applied.restored_ids,
            "reasoning replay inserted cached signatures into the provider request"
        );
    }
}

async fn store_captured_items(
    state: &AppState,
    plan: &ExecutionPlan,
    report_context: Option<&Value>,
    provider_type: &str,
    items: Vec<ReasoningReplayItem>,
) {
    let Some(key) = reasoning_replay_key_for_plan(plan, provider_type, report_context) else {
        return;
    };
    if items.is_empty() {
        // 本轮没有工具调用：保留旧轮次，客户端下一轮仍可能回传更早的工具历史。
        return;
    }
    let previous = read_entry(state, &key).await;
    let entry = ReasoningReplayEntry::merged_with(
        previous.as_ref(),
        ReasoningReplayEntry::new(key.provider, items, current_unix_secs()),
    );
    debug!(
        event_name = "reasoning_replay_captured",
        log_type = "debug",
        provider = key.provider.as_str(),
        key_id = %plan.key_id,
        items = entry.items.len(),
        "reasoning replay captured signatures from the provider response"
    );
    write_entry(state, &key, entry).await;
}

/// 同步响应成功后捕获签名。`body_json` 是上游原始响应体。
pub(crate) async fn capture_reasoning_replay_from_sync_response(
    state: &AppState,
    plan: &ExecutionPlan,
    report_context: Option<&Value>,
    body_json: Option<&Value>,
    body_base64: Option<&str>,
) {
    if !reasoning_replay_needed(report_context) {
        return;
    }
    let Some(provider_type) = provider_type_for_plan(state, plan).await else {
        return;
    };
    let Some(provider) = reasoning_replay_provider_for(&provider_type, &plan.provider_api_format)
    else {
        return;
    };
    let items = match body_json {
        Some(body) => match provider {
            ReasoningReplayProvider::Codex => body
                .get("output")
                .and_then(Value::as_array)
                .map(|output| capture_reasoning_replay_from_openai_responses_output(output))
                .unwrap_or_default(),
            ReasoningReplayProvider::Gemini => capture_reasoning_replay_from_gemini_response(body),
        },
        None => body_base64
            .and_then(|encoded| {
                aether_usage_runtime::decode_internal_report_body_base64(encoded).ok()
            })
            .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
            .map(|text| capture_reasoning_replay_from_sse_text(provider, &text))
            .unwrap_or_default(),
    };
    store_captured_items(state, plan, report_context, &provider_type, items).await;
}

/// 流式响应成功后捕获签名。`provider_body_base64` 是捕获的上游 SSE 原文。
pub(crate) async fn capture_reasoning_replay_from_stream_capture(
    state: &AppState,
    plan: &ExecutionPlan,
    report_context: Option<&Value>,
    provider_body_base64: Option<&str>,
) {
    if !reasoning_replay_needed(report_context) {
        return;
    }
    let Some(encoded) = provider_body_base64 else {
        return;
    };
    let Ok(bytes) = aether_usage_runtime::decode_internal_report_body_base64(encoded) else {
        return;
    };
    let Some(provider_type) = provider_type_for_plan(state, plan).await else {
        return;
    };
    let Some(provider) = reasoning_replay_provider_for(&provider_type, &plan.provider_api_format)
    else {
        return;
    };
    let text = String::from_utf8_lossy(&bytes);
    let items = capture_reasoning_replay_from_sse_text(provider, &text);
    store_captured_items(state, plan, report_context, &provider_type, items).await;
}

/// 上游 400 且错误文本涉及签名：按本次回放时记录的代次 CAS 清理。
pub(crate) async fn clear_reasoning_replay_on_invalid_signature(
    state: &AppState,
    plan: &ExecutionPlan,
    report_context: Option<&Value>,
    status_code: u16,
    error_text: Option<&str>,
) {
    if status_code != 400 {
        return;
    }
    let Some(error_text) = error_text else {
        return;
    };
    if !error_text_indicates_invalid_reasoning_signature(error_text) {
        return;
    }
    let Some(report) =
        report_context.and_then(|context| context.get(REASONING_REPLAY_REPORT_FIELD))
    else {
        return;
    };
    let Some(generation) = report.get("generation").and_then(Value::as_u64) else {
        return;
    };
    let Some(provider_type) = provider_type_for_plan(state, plan).await else {
        return;
    };
    let Some(key) = reasoning_replay_key_for_plan(plan, &provider_type, report_context) else {
        return;
    };
    let cleared_local = ledger().clear_if_generation(&key, generation);
    if state.runtime_state.is_redis() {
        // 共享账本按代次比较后删除：读回不是同一代就说明别的实例已经写了新一轮。
        let storage_key = key.storage_key();
        let same_generation = match state.runtime_state.kv_get(&storage_key).await {
            Ok(Some(stored)) => crate::handlers::shared::open_runtime_secret_payload(
                state,
                REASONING_REPLAY_SECRET_PURPOSE,
                &stored,
            )
            .and_then(|plaintext| serde_json::from_str::<ReasoningReplayEntry>(&plaintext).ok())
            .is_some_and(|entry| entry.generation == generation),
            _ => false,
        };
        if same_generation {
            let _ = state.runtime_state.kv_delete(&storage_key).await;
        }
    }
    warn!(
        event_name = "reasoning_replay_cleared_after_invalid_signature",
        log_type = "ops",
        provider = key.provider.as_str(),
        key_id = %plan.key_id,
        cleared = cleared_local,
        "upstream rejected the replayed reasoning signature; the cached entry was dropped"
    );
}

/// 单次清理最多处理的共享账本键数：一把 Key 的条目数受账本容量约束，超过这个数
/// 说明键空间异常，剩余的交给 TTL 自然过期，不让一次管理端操作扫爆 Redis。
const REASONING_REPLAY_CLEAR_MAX_KEYS: usize = 100_000;
/// `SCAN` 的 COUNT 提示与每批 `DEL` 的键数。
const REASONING_REPLAY_CLEAR_BATCH: usize = 512;

/// 管理端：清掉某把上游 Key 的全部回放条目。返回本地删除数。
pub(crate) async fn clear_reasoning_replay_for_provider_key(
    state: &AppState,
    provider_key_id: &str,
) -> usize {
    let cleared = ledger().clear_provider_key(provider_key_id);
    if state.runtime_state.is_redis() {
        clear_shared_reasoning_replay_entries(state, provider_key_id).await;
    }
    cleared
}

/// 清掉运行时 KV 里某把上游 Key 的全部回放条目，返回删除数。`scan_keys` 在 Redis 后端
/// 内部已经循环到游标结束（`count` 只是 COUNT 提示），这里负责总量上限与分批删除。
pub(crate) async fn clear_shared_reasoning_replay_entries(
    state: &AppState,
    provider_key_id: &str,
) -> usize {
    let mut deleted = 0usize;
    for prefix in ReasoningReplayKey::storage_key_prefix_for_provider_key(provider_key_id) {
        let pattern = format!("{prefix}*");
        let mut keys = match state
            .runtime_state
            .scan_keys(&pattern, REASONING_REPLAY_CLEAR_BATCH)
            .await
        {
            Ok(keys) => keys,
            Err(error) => {
                warn!(
                    event_name = "reasoning_replay_clear_scan_failed",
                    log_type = "ops",
                    backend = state.runtime_state.backend_kind().as_str(),
                    error = ?error,
                    "gateway failed to scan shared reasoning replay entries"
                );
                continue;
            }
        };
        if keys.len() > REASONING_REPLAY_CLEAR_MAX_KEYS.saturating_sub(deleted) {
            warn!(
                event_name = "reasoning_replay_clear_truncated",
                log_type = "ops",
                scanned = keys.len(),
                limit = REASONING_REPLAY_CLEAR_MAX_KEYS,
                "reasoning replay clear hit the key limit; remaining entries expire by TTL"
            );
            keys.truncate(REASONING_REPLAY_CLEAR_MAX_KEYS.saturating_sub(deleted));
        }
        for batch in keys.chunks(REASONING_REPLAY_CLEAR_BATCH) {
            match state.runtime_state.kv_delete_many(batch).await {
                Ok(count) => deleted += count,
                Err(error) => {
                    warn!(
                        event_name = "reasoning_replay_clear_delete_failed",
                        log_type = "ops",
                        backend = state.runtime_state.backend_kind().as_str(),
                        error = ?error,
                        "gateway failed to delete shared reasoning replay entries"
                    );
                }
            }
        }
    }
    deleted
}

#[cfg(test)]
mod tests {
    use super::*;
    use aether_contracts::RequestBody;
    use serde_json::json;

    fn codex_plan(key_id: &str, body: Value) -> ExecutionPlan {
        ExecutionPlan {
            request_id: "req-1".to_string(),
            candidate_id: None,
            provider_name: Some("codex".to_string()),
            provider_id: "provider-1".to_string(),
            endpoint_id: "endpoint-1".to_string(),
            key_id: key_id.to_string(),
            method: "POST".to_string(),
            url: "https://chatgpt.com/backend-api/codex/responses".to_string(),
            headers: Default::default(),
            content_type: None,
            content_encoding: None,
            body: RequestBody::from_json(body),
            stream: true,
            client_api_format: "openai:chat".to_string(),
            provider_api_format: "openai:responses".to_string(),
            model_name: Some("gpt-5.6-sol".to_string()),
            proxy: None,
            transport_profile: None,
            timeouts: None,
        }
    }

    #[test]
    fn replay_is_only_enabled_for_cross_format_requests() {
        let plan = codex_plan("key-1", json!({"model": "gpt-5.6-sol", "input": []}));
        let cross = json!({
            "needs_conversion": true,
            "client_api_format": "openai:chat",
            "provider_api_format": "openai:responses",
            "api_key_id": "api-1",
        });
        let key = reasoning_replay_key_for_plan(&plan, "codex", Some(&cross)).expect("key");
        assert_eq!(key.provider, ReasoningReplayProvider::Codex);
        assert_eq!(key.session_scope, "api_key:api-1");
        assert_eq!(key.model, "gpt-5.6-sol");

        let with_session = json!({
            "needs_conversion": true,
            "client_api_format": "claude:messages",
            "provider_api_format": "openai:responses",
            "api_key_id": "api-1",
            "client_session_affinity": {"client_family": "claude_code", "session_key": "sess-9"},
        });
        let key = reasoning_replay_key_for_plan(&plan, "codex", Some(&with_session)).expect("key");
        assert_eq!(key.session_scope, "session:sess-9");

        let same_family = json!({
            "needs_conversion": false,
            "client_api_format": "openai:responses",
            "provider_api_format": "openai:responses",
            "api_key_id": "api-1",
        });
        assert!(reasoning_replay_key_for_plan(&plan, "codex", Some(&same_family)).is_none());
        assert!(reasoning_replay_key_for_plan(&plan, "openai", Some(&cross)).is_none());

        // Gemini 系没有会话信号时不回放：functionCall 没有 id，按名 + 参数摘要锚定
        // 在共用 Key 的多个会话之间会串扰。
        let mut gemini_plan = plan.clone();
        gemini_plan.provider_api_format = "gemini:generate_content".to_string();
        assert_eq!(
            resolve_reasoning_replay_key_for_plan(&gemini_plan, "antigravity", Some(&cross)),
            ReasoningReplayKeyResolution::Skipped {
                provider: ReasoningReplayProvider::Gemini,
                reason: REASONING_REPLAY_SKIP_NO_SESSION_SCOPE,
            }
        );
        assert!(reasoning_replay_key_for_plan(&gemini_plan, "antigravity", Some(&cross)).is_none());
        let key = reasoning_replay_key_for_plan(&gemini_plan, "antigravity", Some(&with_session))
            .expect("gemini key with session");
        assert_eq!(key.provider, ReasoningReplayProvider::Gemini);
        assert_eq!(key.session_scope, "session:sess-9");
        assert!(reasoning_replay_key_for_plan(&gemini_plan, "codex", Some(&cross)).is_none());
        assert_eq!(
            resolve_reasoning_replay_key_for_plan(&gemini_plan, "openai", Some(&cross)),
            ReasoningReplayKeyResolution::NotApplicable
        );
    }

    #[tokio::test]
    async fn gemini_replay_without_session_scope_records_skip_reason_and_leaves_body_alone() {
        clear_reasoning_replay_ledger_for_tests();
        let state = AppState::new().expect("gateway state should build");
        let mut plan = codex_plan(
            "key-gemini-skip",
            json!({"model": "gemini-3-pro", "contents": [{"role": "model", "parts": [{"functionCall": {"name": "search", "args": {}}}]}]}),
        );
        plan.provider_api_format = "gemini:generate_content".to_string();
        let mut report_context = Some(json!({
            "needs_conversion": true,
            "client_api_format": "openai:chat",
            "provider_api_format": "gemini:generate_content",
            "api_key_id": "api-shared",
        }));
        // 没有传输快照时 provider_type 无法解析，走不到作用域判定；直接验证纯逻辑。
        let resolution =
            resolve_reasoning_replay_key_for_plan(&plan, "gemini_cli", report_context.as_ref());
        assert!(matches!(
            resolution,
            ReasoningReplayKeyResolution::Skipped { reason, .. } if reason == REASONING_REPLAY_SKIP_NO_SESSION_SCOPE
        ));
        apply_reasoning_replay_to_plan(&state, &mut plan, &mut report_context).await;
        assert!(
            plan.body.json_body.as_ref().expect("body")["contents"][0]["parts"][0]
                .get("thoughtSignature")
                .is_none()
        );
    }

    #[tokio::test]
    async fn clearing_shared_entries_walks_past_a_single_scan_page() {
        use aether_runtime_state::{MemoryRuntimeStateConfig, RuntimeState};

        let runtime =
            std::sync::Arc::new(RuntimeState::memory(MemoryRuntimeStateConfig::default()));
        let state = AppState::new()
            .expect("gateway state should build")
            .with_runtime_state(runtime.clone());
        let prefixes = ReasoningReplayKey::storage_key_prefix_for_provider_key("key-many");
        for index in 0..300usize {
            let prefix = &prefixes[index % prefixes.len()];
            runtime
                .kv_set(
                    &format!("{prefix}{index:04x}"),
                    "sealed",
                    Some(REASONING_REPLAY_KV_TTL),
                )
                .await
                .expect("kv set");
        }
        let other_prefix = &ReasoningReplayKey::storage_key_prefix_for_provider_key("key-other")[0];
        runtime
            .kv_set(
                &format!("{other_prefix}keep"),
                "sealed",
                Some(REASONING_REPLAY_KV_TTL),
            )
            .await
            .expect("kv set");

        let deleted = clear_shared_reasoning_replay_entries(&state, "key-many").await;
        assert_eq!(deleted, 300);
        for prefix in &prefixes {
            let remaining = runtime
                .scan_keys(&format!("{prefix}*"), REASONING_REPLAY_CLEAR_BATCH)
                .await
                .expect("scan");
            assert!(
                remaining.is_empty(),
                "prefix {prefix} still has {}",
                remaining.len()
            );
        }
        assert_eq!(
            runtime
                .kv_get(&format!("{other_prefix}keep"))
                .await
                .expect("kv get")
                .as_deref(),
            Some("sealed")
        );
    }
}
