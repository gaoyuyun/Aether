//! Responses WebSocket request normalization and model-selection helpers.
//!
//! These functions translate client protocol events into the HTTP-shaped
//! planning input and provider `response.create` events. They deliberately do
//! not depend on connection state or perform I/O.

use axum::http::header::{AUTHORIZATION, CONNECTION, CONTENT_TYPE, UPGRADE};
use axum::http::Method;
use serde_json::Value;
use sha2::Digest as _;

use crate::ai_serving::{AiExecutionDecision, ResponsesWebSocketBodyNormalization};
use crate::handlers::proxy::websocket::ingress::WebSocketRequestContext;
use crate::headers::request_origin_from_headers_and_remote_addr;
use crate::privacy::RedactionSessionSlot;

/// Model identifiers are copied into planner diagnostics. Bound them before
/// any planning/logging so a single 16 MiB WebSocket frame cannot amplify into
/// repeated multi-megabyte log records.
pub(super) const MAX_RESPONSES_WEBSOCKET_MODEL_BYTES: usize = 256;
/// Response IDs are opaque, but bounding their wire size prevents an
/// untrusted first frame from creating unbounded registry/hash work.
pub(super) const MAX_RESPONSES_WEBSOCKET_RESPONSE_ID_BYTES: usize = 256;

/// The static Responses Lite prefix already stored in a response chain.
///
/// Codex normally sends only the incremental input on a WebSocket
/// continuation. Some compatible clients repeat the current top-level tools
/// and instructions, while others repeat the Lite synthetic input prefix. We
/// retain the first turn's effective configuration so repeated copies can be
/// removed without silently discarding an actual configuration change.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(super) struct ResponsesLiteStaticConfig {
    tools_sha256: [u8; 32],
    instructions_sha256: [u8; 32],
}

impl Default for ResponsesLiteStaticConfig {
    fn default() -> Self {
        Self {
            tools_sha256: responses_lite_static_value_sha256(&Value::Array(Vec::new())),
            instructions_sha256: responses_lite_static_bytes_sha256(b""),
        }
    }
}

#[derive(Debug, Default)]
struct ResponsesLiteStaticConfigObservation {
    tools: Option<Value>,
    instructions: Option<String>,
    leading_input_items: usize,
}

impl ResponsesLiteStaticConfig {
    pub(super) fn from_response_create(event: &Value) -> Self {
        let observation = observe_responses_lite_static_config(event).unwrap_or_default();
        let tools = observation
            .tools
            .unwrap_or_else(|| Value::Array(Vec::new()));
        let instructions = observation.instructions.unwrap_or_default();
        Self {
            tools_sha256: responses_lite_static_value_sha256(&tools),
            instructions_sha256: responses_lite_static_bytes_sha256(instructions.as_bytes()),
        }
    }
}

fn responses_lite_static_bytes_sha256(value: &[u8]) -> [u8; 32] {
    sha2::Sha256::digest(value).into()
}

fn responses_lite_static_value_sha256(value: &Value) -> [u8; 32] {
    let mut digest = sha2::Sha256::new();
    update_responses_lite_static_digest(&mut digest, value);
    digest.finalize().into()
}

fn update_responses_lite_static_length(digest: &mut sha2::Sha256, length: usize) {
    let length = u64::try_from(length).expect("JSON values cannot exceed the u64 hash domain");
    digest.update(length.to_be_bytes());
}

fn update_responses_lite_static_digest(digest: &mut sha2::Sha256, value: &Value) {
    match value {
        Value::Null => digest.update(b"n"),
        Value::Bool(value) => digest.update(if *value { b"t" } else { b"f" }),
        Value::Number(value) => {
            digest.update(b"d");
            digest.update(value.to_string().as_bytes());
            digest.update(b";");
        }
        Value::String(value) => {
            digest.update(b"s");
            update_responses_lite_static_length(digest, value.len());
            digest.update(value.as_bytes());
        }
        Value::Array(values) => {
            digest.update(b"[");
            update_responses_lite_static_length(digest, values.len());
            for value in values {
                update_responses_lite_static_digest(digest, value);
            }
            digest.update(b"]");
        }
        Value::Object(values) => {
            digest.update(b"{");
            update_responses_lite_static_length(digest, values.len());
            let mut keys = values.keys().collect::<Vec<_>>();
            keys.sort_unstable();
            for key in keys {
                update_responses_lite_static_length(digest, key.len());
                digest.update(key.as_bytes());
                update_responses_lite_static_digest(digest, &values[key]);
            }
            digest.update(b"}");
        }
    }
}

fn is_responses_lite_additional_tools_item(item: &Value) -> bool {
    item.get("type").and_then(Value::as_str) == Some("additional_tools")
        && item.get("role").and_then(Value::as_str) == Some("developer")
        && item.get("tools").is_some_and(Value::is_array)
}

fn responses_lite_instruction_text(item: &Value) -> Option<&str> {
    (item.get("type").and_then(Value::as_str) == Some("message")
        && item.get("role").and_then(Value::as_str) == Some("developer"))
    .then(|| {
        item.get("content")
            .and_then(Value::as_array)
            .filter(|content| content.len() == 1)
            .and_then(|content| content[0].as_object())
            .filter(|content| content.get("type").and_then(Value::as_str) == Some("input_text"))
            .and_then(|content| content.get("text"))
            .and_then(Value::as_str)
    })
    .flatten()
}

fn normalize_responses_lite_tools(value: &Value) -> Result<Value, &'static str> {
    match value {
        Value::Null => Ok(Value::Array(Vec::new())),
        Value::Array(tools) => Ok(Value::Array(
            tools
                .iter()
                .filter(|tool| {
                    crate::ai_serving::codex_responses_lite_tool_is_client_executed(tool)
                })
                .cloned()
                .collect(),
        )),
        _ => Err("invalid_response_create_tools"),
    }
}

fn normalize_responses_lite_instructions(value: &Value) -> Result<String, &'static str> {
    match value {
        Value::Null => Ok(String::new()),
        Value::String(value) => Ok(value.clone()),
        _ => Err("invalid_response_create_instructions"),
    }
}

fn observe_responses_lite_static_config(
    event: &Value,
) -> Result<ResponsesLiteStaticConfigObservation, &'static str> {
    let object = event.as_object().ok_or("invalid_response_create")?;
    let mut observation = ResponsesLiteStaticConfigObservation::default();

    if let Some(tools) = object.get("tools") {
        observation.tools = Some(normalize_responses_lite_tools(tools)?);
    }
    if let Some(instructions) = object.get("instructions") {
        observation.instructions = Some(normalize_responses_lite_instructions(instructions)?);
    }

    let Some(input) = object.get("input").and_then(Value::as_array) else {
        return Ok(observation);
    };
    let mut consumed_leading_instruction = false;
    while input
        .get(observation.leading_input_items)
        .is_some_and(is_responses_lite_additional_tools_item)
    {
        let additional_tools = &input[observation.leading_input_items];
        observation.leading_input_items += 1;
        let tools = normalize_responses_lite_tools(&additional_tools["tools"])?;
        if observation.tools.as_ref().is_some_and(|existing| {
            responses_lite_static_value_sha256(existing)
                != responses_lite_static_value_sha256(&tools)
        }) {
            return Err("responses_lite_static_tools_conflict");
        }
        observation.tools = Some(tools);
        if let Some(instructions) = input
            .get(observation.leading_input_items)
            .and_then(responses_lite_instruction_text)
        {
            observation.leading_input_items += 1;
            consumed_leading_instruction = true;
            if observation
                .instructions
                .as_deref()
                .is_some_and(|existing| existing != instructions)
            {
                return Err("responses_lite_static_instructions_conflict");
            }
            observation.instructions = Some(instructions.to_string());
        }
    }
    // A Lite request may contain instructions without any client-executed
    // tools. Its normalized prefix then starts directly with the synthetic
    // developer message, which must be inherited just like the two-item
    // additional_tools + instructions prefix.
    if !consumed_leading_instruction {
        if let Some(instructions) = input
            .get(observation.leading_input_items)
            .and_then(responses_lite_instruction_text)
        {
            observation.leading_input_items += 1;
            if observation
                .instructions
                .as_deref()
                .is_some_and(|existing| existing != instructions)
            {
                return Err("responses_lite_static_instructions_conflict");
            }
            observation.instructions = Some(instructions.to_string());
        }
    }
    Ok(observation)
}

/// Removes a repeated Responses Lite static prefix from a continuation.
///
/// A changed prefix cannot safely be appended to the stored history: doing so
/// is the context-growth bug this path prevents, and the Lite backend has no
/// request primitive that erases the previous synthetic item. Match Codex's
/// own transport behavior by requiring a new response chain when request
/// properties change instead of silently retaining stale tools/instructions.
pub(super) fn prepare_responses_lite_continuation(
    event: &Value,
    stored: &ResponsesLiteStaticConfig,
) -> Result<Value, &'static str> {
    let observation = observe_responses_lite_static_config(event)?;
    if observation
        .tools
        .as_ref()
        .is_some_and(|tools| responses_lite_static_value_sha256(tools) != stored.tools_sha256)
        || observation
            .instructions
            .as_ref()
            .is_some_and(|instructions| {
                responses_lite_static_bytes_sha256(instructions.as_bytes())
                    != stored.instructions_sha256
            })
    {
        return Err("responses_lite_continuation_static_config_changed");
    }

    let mut prepared = event.clone();
    let object = prepared.as_object_mut().ok_or("invalid_response_create")?;
    object.remove("tools");
    object.remove("instructions");
    if observation.leading_input_items > 0 {
        let input = object
            .get_mut("input")
            .and_then(Value::as_array_mut)
            .ok_or("invalid_response_create_input")?;
        input.drain(..observation.leading_input_items);
    }
    Ok(prepared)
}

pub(super) fn validated_response_create_model(value: &Value) -> Result<&str, &'static str> {
    let Some(model) = value
        .as_str()
        .map(str::trim)
        .filter(|model| !model.is_empty())
    else {
        return Err("invalid_response_create_model");
    };
    if model.len() > MAX_RESPONSES_WEBSOCKET_MODEL_BYTES {
        return Err("invalid_response_create_model");
    }
    Ok(model)
}

/// 把一条 WebSocket turn 还原成 planner 需要的 HTTP 形状请求头部。
///
/// 这里必须和 HTTP 前门（`handlers/proxy/mod.rs`）保持同一份 extension 契约：
/// planner 只在 `parts.extensions` 里拿到 `RedactionSessionSlot` 时才做请求脱敏
/// （`ai_serving/planner/redaction.rs`），少插这一项等于整条 WS 链路静默绕过
/// 已启用的 PII 脱敏。
pub(super) fn build_planning_parts(context: &WebSocketRequestContext) -> http::request::Parts {
    let mut request = http::Request::builder()
        .method(Method::POST)
        .uri(context.uri.clone())
        .body(())
        .expect("a validated request URI should build planning request parts");
    let headers = request.headers_mut();
    *headers = context.headers.clone();
    headers.remove(AUTHORIZATION);
    headers.remove("x-api-key");
    headers.remove("api-key");
    headers.remove("x-goog-api-key");
    headers.remove(CONNECTION);
    headers.remove(UPGRADE);
    headers.remove("sec-websocket-key");
    headers.remove("sec-websocket-version");
    headers.remove("sec-websocket-protocol");
    headers.remove("sec-websocket-extensions");
    headers.insert(
        CONTENT_TYPE,
        http::HeaderValue::from_static("application/json"),
    );
    request
        .extensions_mut()
        .insert(request_origin_from_headers_and_remote_addr(
            &context.headers,
            &context.remote_addr,
        ));
    // slot 必须每个 turn 新建，不能按连接复用：planner 侧的请求脱敏缓存键是
    // `{format:?}:{body_json 指针地址}`（`ai_serving/planner/redaction.rs:169`），
    // 连接级复用同一个 slot 时，上一轮 client_event 释放后这一轮的 `Value` 很可能
    // 落在同一地址，会命中上一轮缓存，把上一轮的脱敏 body 当成这一轮的发出去。
    // 每个 `response.create` 本身就是独立计费/审计请求，per-turn 也正好对应
    // HTTP 前门「一个请求一个 slot」的语义。
    request
        .extensions_mut()
        .insert(RedactionSessionSlot::default());
    request.into_parts().0
}

pub(super) fn planned_response_create_event(
    decision: &AiExecutionDecision,
    normalization: &ResponsesWebSocketBodyNormalization,
    fallback: &Value,
) -> Result<String, &'static str> {
    let event = decision
        .provider_request_body
        .clone()
        .unwrap_or_else(|| fallback.clone());
    finish_response_create_event(event, fallback, normalization)
}

/// Restores the WebSocket protocol framing that provider-body normalization is
/// not aware of.
///
/// `previous_response_id` is on the Codex unsupported-field list, Codex HTTP
/// normalization may force `store`, and `generate` is not an HTTP body option
/// at all. Those fields are WebSocket protocol state. `store` and `generate`
/// may still be owned by an endpoint body rule, but lineage is security state:
/// the final `previous_response_id` must be exactly the client value whose
/// ownership the session validated. A body rule may neither replace nor inject
/// an opaque response ID after that check.
/// `stream`/`background` go the other way: the normalizer inserts `stream`, and
/// the WebSocket protocol has no use for it. Named `stream_id` lanes are not
/// yet exposed by Aether, so provider rules cannot inject one behind the
/// default-lane validator.
fn finish_response_create_event(
    mut event: Value,
    client_event: &Value,
    normalization: &ResponsesWebSocketBodyNormalization,
) -> Result<String, &'static str> {
    let object = event
        .as_object_mut()
        .ok_or("responses_websocket_request_invalid")?;
    object.insert(
        "type".to_string(),
        Value::String("response.create".to_string()),
    );
    for field in ["store", "generate"] {
        if !normalization.body_rules_handle_websocket_field(client_event, field) {
            if let Some(value) = client_event.get(field) {
                object.insert(field.to_string(), value.clone());
            }
        }
    }
    match client_event.get("previous_response_id") {
        Some(value) => {
            object.insert("previous_response_id".to_string(), value.clone());
        }
        None => {
            object.remove("previous_response_id");
        }
    }
    object.remove("stream");
    object.remove("background");
    object.remove("stream_id");
    serde_json::to_string(&event).map_err(|_| "responses_websocket_request_invalid")
}

pub(super) fn response_create_has_previous_response_id(event: &Value) -> bool {
    event
        .get("previous_response_id")
        .and_then(Value::as_str)
        .is_some_and(|value| !value.trim().is_empty())
}

pub(super) fn validate_response_create_previous_response_id(
    event: &Value,
) -> Result<(), &'static str> {
    match event.get("previous_response_id") {
        None | Some(Value::Null) => Ok(()),
        Some(Value::String(value))
            if !value.trim().is_empty()
                && value.len() <= MAX_RESPONSES_WEBSOCKET_RESPONSE_ID_BYTES =>
        {
            Ok(())
        }
        Some(_) => Err("invalid_response_create_previous_response_id"),
    }
}

/// Returns a named lane only after the complete public `stream_id` grammar has
/// been checked. This is the sole source for reflecting a request-scoped lane
/// into gateway-generated error events.
pub(super) fn validated_named_stream_id(event: &Value) -> Option<&str> {
    let stream_id = event.get("stream_id")?.as_str()?;
    (!stream_id.is_empty()
        && stream_id.len() <= 256
        && stream_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.')))
    .then_some(stream_id)
}

/// Aether currently owns one logical turn for each physical Responses socket.
///
/// OpenAI's named `stream_id` lanes require per-lane turn state, FIFO queues,
/// usage settlement, timeouts, and gateway-generated error routing. Silently
/// dropping the field would merge independent conversations into the default
/// lane, so fail closed until that complete multiplexing contract is present.
pub(super) fn validate_response_create_stream_id_support(
    event: &Value,
) -> Result<(), &'static str> {
    if event.get("stream_id").is_none() {
        return Ok(());
    }
    if validated_named_stream_id(event).is_none() {
        return Err("invalid_response_create_stream_id");
    }
    Err("responses_websocket_named_stream_unsupported")
}

pub(super) fn changed_followup_response_create_model(
    event: &Value,
    current_client_model: &str,
) -> Result<Option<String>, &'static str> {
    let Some(object) = event.as_object() else {
        return Err("invalid_response_create");
    };
    let Some(model) = object.get("model") else {
        return Ok(None);
    };
    let model = validated_response_create_model(model)?;
    if model.eq_ignore_ascii_case(current_client_model) {
        Ok(None)
    } else {
        Ok(Some(model.to_string()))
    }
}

pub(super) fn response_create_model_or_current(
    event: &mut Value,
    current_client_model: &str,
) -> Result<String, &'static str> {
    let Some(object) = event.as_object_mut() else {
        return Err("invalid_response_create");
    };
    let Some(model) = object.get("model") else {
        object.insert(
            "model".to_string(),
            Value::String(current_client_model.to_string()),
        );
        return Ok(current_client_model.to_string());
    };
    let model = validated_response_create_model(model)?;
    let model = model.to_string();
    object.insert("model".to_string(), Value::String(model.clone()));
    Ok(model)
}

pub(super) fn provider_model_from_decision(decision: &AiExecutionDecision) -> Option<String> {
    decision
        .provider_request_body
        .as_ref()
        .and_then(|body| body.get("model"))
        .and_then(Value::as_str)
        .or(decision.mapped_model.as_deref())
        .map(str::trim)
        .filter(|model| !model.is_empty())
        .map(str::to_string)
}

/// Returns the effective Codex contract after body rules, routing mutations,
/// model directives, and terminal header convergence have all run.
///
/// Model capability alone is insufficient: a non-null `context_management`
/// request deliberately disables Responses Lite for that turn. The protected
/// Lite header is emitted from the final provider body, so it is the stable
/// contract marker retained by a WebSocket binding.
pub(super) fn planned_request_uses_codex_responses_lite(
    decision: &AiExecutionDecision,
    normalization: &ResponsesWebSocketBodyNormalization,
) -> bool {
    let decision_is_codex_responses = decision
        .provider_type
        .as_deref()
        .is_some_and(|value| value.trim().eq_ignore_ascii_case("codex"))
        && decision
            .provider_api_format
            .as_deref()
            .is_some_and(crate::ai_serving::is_openai_responses_family_format);
    let final_body_supports_lite = decision
        .provider_request_body
        .as_ref()
        .is_none_or(|body| body.get("context_management").is_none_or(Value::is_null));
    decision_is_codex_responses
        && normalization.uses_codex_responses_lite()
        && final_body_supports_lite
        && decision
            .provider_request_headers
            .iter()
            .any(|(name, value)| {
                name.eq_ignore_ascii_case(crate::ai_serving::CODEX_RESPONSES_LITE_HEADER)
                    && value.trim().eq_ignore_ascii_case("true")
            })
}

/// Prepares a continuation `response.create` for the already-bound upstream.
///
/// The turn cannot be re-planned without risking a different provider key, so
/// the binding's retained normalizer is replayed instead. That keeps model
/// directives, endpoint body rules and the Codex body contract applied on every
/// turn rather than only on the one that bound the socket.
pub(super) fn normalize_followup_response_create(
    event: &Value,
    provider_model: &str,
    normalization: &ResponsesWebSocketBodyNormalization,
) -> Result<String, &'static str> {
    if event.as_object().is_none() {
        return Err("invalid_response_create");
    }
    if event.get("type").and_then(Value::as_str) != Some("response.create") {
        return Err("invalid_response_create");
    }
    validate_response_create_previous_response_id(event)?;
    // Never fall back to the raw event. That would bypass endpoint rules and
    // the Responses Lite de-duplication exactly when normalization rejected a
    // malformed request, potentially re-appending static configuration.
    let mut normalized = normalization
        .normalize_response_create(event)
        .ok_or("responses_websocket_request_normalization_failed")?;
    let Some(object) = normalized.as_object_mut() else {
        return Err("invalid_response_create");
    };
    // A continuation must never switch models mid-socket, and normalization is
    // allowed to rewrite `model` (the Codex image-tool path does).
    object.insert(
        "model".to_string(),
        Value::String(provider_model.to_string()),
    );
    finish_response_create_event(normalized, event, normalization)
        .map_err(|_| "response_create_serialization_failed")
}

#[cfg(test)]
mod tests {
    use std::net::SocketAddr;

    use axum::http::{HeaderMap, Uri};
    use serde_json::{json, Value};

    use super::{
        build_planning_parts, normalize_followup_response_create,
        planned_request_uses_codex_responses_lite, planned_response_create_event,
        prepare_responses_lite_continuation, response_create_has_previous_response_id,
        validate_response_create_previous_response_id, validate_response_create_stream_id_support,
        validated_named_stream_id, ResponsesLiteStaticConfig,
        MAX_RESPONSES_WEBSOCKET_RESPONSE_ID_BYTES,
    };
    use crate::ai_serving::ResponsesWebSocketBodyNormalization;
    use crate::control::GatewayControlDecision;
    use crate::handlers::proxy::websocket::ingress::WebSocketRequestContext;
    use crate::privacy::RedactionSessionSlot;

    fn websocket_context() -> WebSocketRequestContext {
        WebSocketRequestContext {
            trace_id: "trace-planning-parts".to_string(),
            headers: HeaderMap::new(),
            uri: Uri::from_static("/v1/responses"),
            remote_addr: "127.0.0.1:65001"
                .parse::<SocketAddr>()
                .expect("remote address should parse"),
            client_ip: "127.0.0.1".parse().expect("client IP should parse"),
            decision: GatewayControlDecision::synthetic(
                "/v1/responses".to_string(),
                Some("ai_public".to_string()),
                Some("openai".to_string()),
                Some("responses_websocket".to_string()),
                Some("openai:responses".to_string()),
            ),
            websocket_connection_permit: None,
        }
    }

    #[test]
    fn planning_parts_carry_a_fresh_redaction_session_slot_per_turn() {
        // 没有这个 extension，planner 会静默跳过已启用的 PII 脱敏
        // （ai_serving/planner/redaction.rs），整条 WS 链路都按原文发上游。
        let context = websocket_context();
        let first = build_planning_parts(&context);
        let second = build_planning_parts(&context);

        let first_slot = first
            .extensions
            .get::<RedactionSessionSlot>()
            .expect("planning parts must carry a redaction session slot");
        let second_slot = second
            .extensions
            .get::<RedactionSessionSlot>()
            .expect("planning parts must carry a redaction session slot");

        // 每轮必须是独立 slot：slot 内的请求缓存以 body 指针地址为键，跨轮共享会
        // 命中上一轮缓存。用缓存条目相互不可见来证明两者不是同一个 slot。
        first_slot.put_cached_request_redaction(
            "turn-1",
            crate::privacy::CachedRequestRedaction::unredacted(),
        );
        assert!(first_slot.cached_request_redaction("turn-1").is_some());
        assert!(second_slot.cached_request_redaction("turn-1").is_none());
    }

    fn normalized_continuation(
        event: &serde_json::Value,
        normalization: &ResponsesWebSocketBodyNormalization,
    ) -> serde_json::Value {
        let outbound = normalize_followup_response_create(event, "provider-model", normalization)
            .expect("continuation should normalize");
        serde_json::from_str(&outbound).expect("normalized event should be JSON")
    }

    #[test]
    fn continuation_keeps_protocol_state_that_provider_normalization_strips() {
        // `previous_response_id` is on the Codex unsupported-field list, so
        // normalization removes it — yet it is what continues the chain. If
        // this regresses, every continuation turn silently starts a new one.
        let event = json!({
            "type": "response.create",
            "model": "public-model",
            "previous_response_id": "resp_123",
            "input": [],
            "stream": true,
            "background": true,
        });

        let normalized = normalized_continuation(
            &event,
            &ResponsesWebSocketBodyNormalization::for_tests("provider-model")
                .with_provider_type_for_tests("codex"),
        );

        assert_eq!(normalized["type"], "response.create");
        assert_eq!(normalized["previous_response_id"], "resp_123");
        assert_eq!(normalized["model"], "provider-model");
        assert!(normalized.get("stream").is_none());
        assert!(normalized.get("background").is_none());
    }

    #[test]
    fn websocket_first_response_create_keeps_responses_lite_static_config() {
        // A socket's first event has no previous_response_id, so its tools and
        // instructions still need the normal Responses Lite projection. The
        // continuation-only pass must not accidentally suppress this prefix.
        let event = json!({
            "type": "response.create",
            "model": "gpt-5.6-sol",
            "instructions": "developer instructions",
            "tools": [{
                "type": "function",
                "name": "lookup",
                "parameters": {"type": "object"}
            }],
            "input": [{
                "role": "user",
                "content": [{"type": "input_text", "text": "hello"}]
            }]
        });

        let normalization = ResponsesWebSocketBodyNormalization::for_tests("gpt-5.6-sol")
            .with_provider_type_for_tests("codex");
        let normalized = normalized_continuation(&event, &normalization);
        let input = normalized["input"].as_array().expect("normalized input");
        assert_eq!(input[0]["type"], "additional_tools");
        assert_eq!(input[0]["tools"][0]["name"], "lookup");
        assert_eq!(input[1]["role"], "developer");
        assert_eq!(input[1]["content"][0]["text"], "developer instructions");
        assert!(normalized.get("tools").is_none());
        assert!(normalized.get("instructions").is_none());
        assert!(normalized.get("previous_response_id").is_none());
    }

    #[test]
    fn explicit_store_and_previous_response_id_are_forwarded() {
        let event = json!({
            "type": "response.create",
            "model": "public-model",
            "store": true,
            "previous_response_id": "resp_123",
            "input": [],
        });

        let normalized = normalized_continuation(
            &event,
            &ResponsesWebSocketBodyNormalization::for_tests("provider-model")
                .with_provider_type_for_tests("codex"),
        );

        // Codex HTTP normalization normally forces `store: false` and removes
        // `previous_response_id`. WebSocket framing restores exactly what the
        // client sent so the upstream owns validation and continuation lookup.
        assert_eq!(normalized["store"], true);
        assert_eq!(normalized["previous_response_id"], "resp_123");
    }

    #[test]
    fn endpoint_body_rules_cannot_replace_validated_websocket_lineage() {
        let event = json!({
            "type": "response.create",
            "model": "public-model",
            "store": true,
            "previous_response_id": "resp_client",
            "generate": false,
            "input": [],
        });
        let normalization = ResponsesWebSocketBodyNormalization::for_tests("provider-model")
            .with_provider_type_for_tests("codex")
            .with_body_rules_for_tests(json!([
                {"action": "set", "path": "store", "value": false},
                {
                    "action": "set",
                    "path": "previous_response_id",
                    "value": "resp_admin"
                },
                {"action": "set", "path": "generate", "value": true}
            ]));

        let normalized = normalized_continuation(&event, &normalization);

        // WebSocket framing runs after provider-body finalization. Store and
        // generate remain administrator-owned, but the opaque lineage ID must
        // be the exact value validated against the authenticated connection.
        assert_eq!(normalized["store"], false);
        assert_eq!(normalized["previous_response_id"], "resp_client");
        assert_eq!(normalized["generate"], true);
    }

    #[test]
    fn conditional_body_rules_own_store_and_generate_only_when_their_values_apply() {
        let normalization = ResponsesWebSocketBodyNormalization::for_tests("provider-model")
            .with_provider_type_for_tests("codex")
            .with_body_rules_for_tests(json!([
                {
                    "action": "set",
                    "path": "store",
                    "value": false,
                    "condition": {"path": "metadata.mode", "op": "eq", "value": "enforce"}
                },
                {
                    "action": "set",
                    "path": "generate",
                    "value": true,
                    "condition": {"path": "metadata.mode", "op": "eq", "value": "enforce"}
                }
            ]));

        let skipped = normalized_continuation(
            &json!({
                "type": "response.create",
                "model": "public-model",
                "store": true,
                "generate": false,
                "metadata": {"mode": "observe"},
                "input": []
            }),
            &normalization,
        );
        assert_eq!(skipped["store"], true);
        assert_eq!(skipped["generate"], false);

        let applied = normalized_continuation(
            &json!({
                "type": "response.create",
                "model": "public-model",
                "store": true,
                "generate": false,
                "metadata": {"mode": "enforce"},
                "input": []
            }),
            &normalization,
        );
        assert_eq!(applied["store"], false);
        assert_eq!(applied["generate"], true);
    }

    #[test]
    fn endpoint_body_rules_cannot_inject_websocket_lineage() {
        let event = json!({
            "type": "response.create",
            "model": "public-model",
            "input": [],
        });
        let normalization = ResponsesWebSocketBodyNormalization::for_tests("provider-model")
            .with_provider_type_for_tests("codex")
            .with_body_rules_for_tests(json!([{
                "action": "set",
                "path": "previous_response_id",
                "value": "resp_admin"
            }]));

        let normalized = normalized_continuation(&event, &normalization);

        assert!(normalized.get("previous_response_id").is_none());
    }

    #[test]
    fn endpoint_body_rules_cannot_inject_an_untracked_named_lane() {
        let event = json!({
            "type": "response.create",
            "model": "public-model",
            "input": [],
        });
        let normalization = ResponsesWebSocketBodyNormalization::for_tests("provider-model")
            .with_body_rules_for_tests(json!([{
                "action": "set",
                "path": "stream_id",
                "value": "admin-injected-lane"
            }]));

        let normalized = normalized_continuation(&event, &normalization);

        assert!(normalized.get("stream_id").is_none());
    }

    #[test]
    fn explicit_null_websocket_protocol_state_is_not_rewritten() {
        let event = json!({
            "type": "response.create",
            "model": "public-model",
            "store": null,
            "previous_response_id": null,
            "generate": null,
            "input": [],
        });

        let normalized = normalized_continuation(
            &event,
            &ResponsesWebSocketBodyNormalization::for_tests("provider-model")
                .with_provider_type_for_tests("codex"),
        );

        assert!(normalized.get("store").is_some_and(Value::is_null));
        assert!(normalized
            .get("previous_response_id")
            .is_some_and(Value::is_null));
        assert!(normalized.get("generate").is_some_and(Value::is_null));
    }

    #[test]
    fn continuation_strips_fields_the_codex_backend_rejects() {
        // The point of the fix: before it, turns 2..N reached Codex with the
        // client's raw body, so a `temperature` that turn 1 had stripped would
        // be rejected upstream. This also proves normalization really runs
        // rather than silently falling back to the unmodified event.
        let event = json!({
            "type": "response.create",
            "model": "public-model",
            "previous_response_id": "resp_123",
            "temperature": 0.7,
            "top_p": 0.9,
            "input": [],
        });

        let normalized = normalized_continuation(
            &event,
            &ResponsesWebSocketBodyNormalization::for_tests("provider-model")
                .with_provider_type_for_tests("codex"),
        );

        assert!(normalized.get("temperature").is_none());
        assert!(normalized.get("top_p").is_none());
        assert_eq!(normalized["store"], false);
        // ...and the protocol state survives the same pass.
        assert_eq!(normalized["previous_response_id"], "resp_123");
    }

    #[test]
    fn lite_continuations_forward_only_each_turns_incremental_input() {
        let normalization = ResponsesWebSocketBodyNormalization::for_tests("gpt-5.6-sol")
            .with_provider_type_for_tests("codex");

        for turn in 1..=4 {
            let event = json!({
                "type": "response.create",
                "model": "gpt-5.6-sol",
                "previous_response_id": format!("resp_{turn}"),
                "instructions": "The same large developer instructions.",
                "tools": [{
                    "type": "function",
                    "name": "shell",
                    "parameters": {"type": "object"}
                }],
                "input": [
                    {
                        "type": "reasoning",
                        "id": format!("rs_{turn}"),
                        "content": [{
                            "type": "reasoning_text",
                            "text": format!("reasoning state {turn}")
                        }],
                        "encrypted_content": format!("opaque-{turn}")
                    },
                    {
                        "type": "function_call_output",
                        "call_id": format!("call_{turn}"),
                        "output": format!("result {turn}")
                    }
                ]
            });

            let normalized = normalized_continuation(&event, &normalization);
            let input = normalized["input"].as_array().expect("incremental input");
            assert_eq!(input.len(), 2);
            assert_eq!(input[0]["type"], "reasoning");
            assert_eq!(input[0]["content"][0]["type"], "reasoning_text");
            assert_eq!(input[1]["type"], "function_call_output");
            assert!(normalized.get("instructions").is_none());
            assert!(normalized.get("tools").is_none());
            assert!(!input.iter().any(|item| item["type"] == "additional_tools"));
            assert!(!input.iter().any(|item| item["role"] == "developer"));
            assert_eq!(normalized["previous_response_id"], format!("resp_{turn}"));
        }
    }

    #[test]
    fn deepseek_continuations_preserve_idless_opaque_reasoning_state() {
        let normalization = ResponsesWebSocketBodyNormalization::for_tests("deepseek-v4-flash")
            .with_provider_type_for_tests("custom")
            .with_reasoning_replay_policy_for_tests(
                crate::ai_serving::OpenAiResponsesReasoningReplayPolicy::DeepSeekOpaque,
            );
        let opaque_reasoning = json!({
            "type": "reasoning",
            "encrypted_content": "opaque-deepseek-state",
            "content": [{
                "type": "reasoning_text",
                "text": "provider-owned thinking state"
            }],
            "future_capability": {"preserve": true}
        });
        let event = json!({
            "type": "response.create",
            "model": "deepseek-v4-flash",
            "previous_response_id": "resp_deepseek_1",
            "input": [
                opaque_reasoning.clone(),
                {
                    "type": "function_call_output",
                    "call_id": "call_1",
                    "output": "result"
                }
            ]
        });

        let normalized = normalized_continuation(&event, &normalization);
        assert_eq!(normalized["input"][0], opaque_reasoning);
        assert_eq!(normalized["input"][1]["type"], "function_call_output");
        assert_eq!(normalized["previous_response_id"], "resp_deepseek_1");

        let strict = normalized_continuation(
            &event,
            &ResponsesWebSocketBodyNormalization::for_tests("deepseek-v4-flash")
                .with_provider_type_for_tests("custom"),
        );
        let strict_input = strict["input"].as_array().expect("strict provider input");
        assert_eq!(strict_input.len(), 1);
        assert_eq!(strict_input[0]["type"], "function_call_output");
    }

    #[test]
    fn continuation_keeps_a_warmup_generate_flag() {
        let event = json!({
            "type": "response.create",
            "model": "public-model",
            "previous_response_id": "resp_123",
            "generate": false,
            "input": [],
        });

        let normalized = normalized_continuation(
            &event,
            &ResponsesWebSocketBodyNormalization::for_tests("provider-model")
                .with_provider_type_for_tests("codex"),
        );

        assert_eq!(normalized["generate"], false);
    }

    #[test]
    fn continuation_applies_the_model_directive_patch_the_binding_turn_received() {
        let event = json!({
            "type": "response.create",
            "model": "public-model",
            "previous_response_id": "resp_123",
            "input": [],
        });

        let normalized = normalized_continuation(
            &event,
            &ResponsesWebSocketBodyNormalization::for_tests("provider-model")
                .with_model_directive_patch_for_tests(json!({"reasoning": {"effort": "high"}})),
        );

        assert_eq!(normalized["reasoning"]["effort"], "high");
    }

    #[test]
    fn continuation_still_forces_the_bound_provider_model() {
        let event = json!({
            "type": "response.create",
            "model": "some-other-model",
            "previous_response_id": "resp_123",
            "input": [],
        });

        let normalized = normalized_continuation(
            &event,
            &ResponsesWebSocketBodyNormalization::for_tests("provider-model"),
        );

        assert_eq!(normalized["model"], "provider-model");
    }

    #[test]
    fn a_continuation_that_is_not_a_response_create_is_rejected() {
        let normalization = ResponsesWebSocketBodyNormalization::for_tests("provider-model");

        assert!(normalize_followup_response_create(
            &json!({"type": "response.cancel"}),
            "provider-model",
            &normalization,
        )
        .is_err());
        assert!(normalize_followup_response_create(
            &json!("not an object"),
            "provider-model",
            &normalization,
        )
        .is_err());
    }

    #[test]
    fn previous_response_id_requires_a_non_empty_string() {
        assert!(response_create_has_previous_response_id(
            &json!({"previous_response_id": "resp_123"})
        ));
        assert!(!response_create_has_previous_response_id(
            &json!({"previous_response_id": "  "})
        ));
        assert!(!response_create_has_previous_response_id(
            &json!({"previous_response_id": 42})
        ));
        assert!(!response_create_has_previous_response_id(
            &json!({"previous_response_id": {"id": "resp_123"}})
        ));
        assert!(!response_create_has_previous_response_id(
            &json!({"previous_response_id": null})
        ));
        assert!(!response_create_has_previous_response_id(&json!({})));

        assert!(validate_response_create_previous_response_id(
            &json!({"previous_response_id": "resp_123"})
        )
        .is_ok());
        assert!(validate_response_create_previous_response_id(
            &json!({"previous_response_id": null})
        )
        .is_ok());
        for invalid in [json!("  "), json!(42), json!({"id": "resp_123"})] {
            assert!(validate_response_create_previous_response_id(
                &json!({"previous_response_id": invalid})
            )
            .is_err());
        }
        assert!(validate_response_create_previous_response_id(&json!({
            "previous_response_id": "x".repeat(MAX_RESPONSES_WEBSOCKET_RESPONSE_ID_BYTES + 1)
        }))
        .is_err());
    }

    #[test]
    fn named_streams_fail_closed_until_lane_multiplexing_is_implemented() {
        assert!(validate_response_create_stream_id_support(&json!({})).is_ok());
        for stream_id in [
            json!(null),
            json!(""),
            json!("invalid/lane"),
            json!("x".repeat(257)),
            json!(42),
        ] {
            assert_eq!(
                validate_response_create_stream_id_support(&json!({"stream_id": stream_id})),
                Err("invalid_response_create_stream_id")
            );
        }
        assert_eq!(
            validate_response_create_stream_id_support(&json!({"stream_id": "main-lane_1.test"})),
            Err("responses_websocket_named_stream_unsupported")
        );
        assert_eq!(
            validated_named_stream_id(&json!({"stream_id": "main-lane_1.test"})),
            Some("main-lane_1.test")
        );
        for invalid in [
            json!(null),
            json!(""),
            json!("invalid/lane"),
            json!("x".repeat(257)),
            json!(42),
        ] {
            assert_eq!(
                validated_named_stream_id(&json!({"stream_id": invalid})),
                None
            );
        }
    }

    #[test]
    fn effective_lite_contract_uses_the_converged_provider_header() {
        let mut decision: crate::ai_serving::AiExecutionDecision = serde_json::from_value(json!({
            "action": "local",
            "provider_type": "codex",
            "provider_api_format": "openai:responses",
            "provider_request_headers": {}
        }))
        .expect("minimal decision");
        let normalization = ResponsesWebSocketBodyNormalization::for_tests("gpt-5.6-sol")
            .with_provider_type_for_tests("codex");
        assert!(!planned_request_uses_codex_responses_lite(
            &decision,
            &normalization
        ));

        decision.provider_request_headers.insert(
            crate::ai_serving::CODEX_RESPONSES_LITE_HEADER.to_string(),
            "false".to_string(),
        );
        assert!(!planned_request_uses_codex_responses_lite(
            &decision,
            &normalization
        ));

        decision.provider_request_headers.clear();
        decision.provider_request_headers.insert(
            crate::ai_serving::CODEX_RESPONSES_LITE_HEADER.to_ascii_uppercase(),
            "TRUE".to_string(),
        );
        assert!(planned_request_uses_codex_responses_lite(
            &decision,
            &normalization
        ));

        decision.provider_request_body = Some(json!({
            "model": "gpt-5.6-sol",
            "context_management": {"compact_threshold": 1000}
        }));
        assert!(!planned_request_uses_codex_responses_lite(
            &decision,
            &normalization
        ));
        decision.provider_request_body = None;

        // Header rules on a custom provider must not be able to spoof the
        // internal Codex contract marker and enable Lite de-duplication.
        decision.provider_type = Some("custom".to_string());
        assert!(!planned_request_uses_codex_responses_lite(
            &decision,
            &normalization
        ));
        decision.provider_type = Some("codex".to_string());
        let custom_normalization = ResponsesWebSocketBodyNormalization::for_tests("gpt-5.6-sol");
        assert!(!planned_request_uses_codex_responses_lite(
            &decision,
            &custom_normalization
        ));
    }

    #[test]
    fn responses_lite_continuation_deduplicates_only_matching_static_config() {
        let first = json!({
            "type": "response.create",
            "model": "gpt-5.6-sol",
            "instructions": "developer instructions",
            "tools": [{"type": "function", "name": "lookup", "parameters": {}}],
            "input": [{"role": "user", "content": "hello"}]
        });
        let stored = ResponsesLiteStaticConfig::from_response_create(&first);
        let continuation = json!({
            "type": "response.create",
            "model": "gpt-5.6-sol",
            "previous_response_id": "resp_1",
            "instructions": "developer instructions",
            "tools": [{"type": "function", "name": "lookup", "parameters": {}}],
            "input": [{"type": "function_call_output", "call_id": "call_1", "output": "ok"}]
        });

        let prepared = prepare_responses_lite_continuation(&continuation, &stored)
            .expect("matching static config should be inherited");
        assert!(prepared.get("tools").is_none());
        assert!(prepared.get("instructions").is_none());
        assert_eq!(prepared["input"].as_array().map(Vec::len), Some(1));

        let changed_instructions = json!({
            "type": "response.create",
            "model": "gpt-5.6-sol",
            "previous_response_id": "resp_1",
            "instructions": "different instructions",
            "input": []
        });
        assert_eq!(
            prepare_responses_lite_continuation(&changed_instructions, &stored),
            Err("responses_lite_continuation_static_config_changed")
        );
        let changed_tools = json!({
            "type": "response.create",
            "model": "gpt-5.6-sol",
            "previous_response_id": "resp_1",
            "tools": [{"type": "function", "name": "other", "parameters": {}}],
            "input": []
        });
        assert_eq!(
            prepare_responses_lite_continuation(&changed_tools, &stored),
            Err("responses_lite_continuation_static_config_changed")
        );
    }

    #[test]
    fn responses_lite_static_tools_match_the_actual_client_executed_synthetic_subset() {
        let function = json!({"type": "function", "name": "lookup", "parameters": {}});
        let custom = json!({"type": "custom", "name": "shell", "format": {}});
        let namespace = json!({"type": "namespace", "name": "browser", "tools": []});
        let client_search = json!({"type": "tool_search", "execution": "client"});
        let first = json!({
            "type": "response.create",
            "model": "gpt-5.6-sol",
            "instructions": "developer instructions",
            "tools": [
                function.clone(),
                {"type": "web_search"},
                {"type": "image_generation"},
                custom.clone(),
                namespace.clone(),
                client_search.clone(),
                {"type": "tool_search", "execution": "server"},
                {"type": "tool_search"},
                {"type": "future_hosted_tool"}
            ],
            "input": [{"role": "user", "content": "hello"}]
        });
        let stored = ResponsesLiteStaticConfig::from_response_create(&first);
        let continuation = json!({
            "type": "response.create",
            "model": "gpt-5.6-sol",
            "previous_response_id": "resp_1",
            "input": [
                {
                    "type": "additional_tools",
                    "role": "developer",
                    "tools": [function, custom, namespace, client_search]
                },
                {
                    "type": "message",
                    "role": "developer",
                    "content": [{"type": "input_text", "text": "developer instructions"}]
                },
                {"type": "function_call_output", "call_id": "call_1", "output": "ok"}
            ]
        });

        let prepared = prepare_responses_lite_continuation(&continuation, &stored)
            .expect("hosted tools are not part of the stored Lite synthetic prefix");
        let input = prepared["input"].as_array().expect("prepared input");
        assert_eq!(input.len(), 1);
        assert_eq!(input[0]["type"], "function_call_output");
    }

    #[test]
    fn responses_lite_static_identity_must_be_retained_before_redaction() {
        let raw_first = json!({
            "type": "response.create",
            "model": "gpt-5.6-sol",
            "instructions": "customer secret",
            "tools": [{
                "type": "function",
                "name": "lookup",
                "description": "customer secret"
            }],
            "input": []
        });
        let redacted_first = json!({
            "type": "response.create",
            "model": "gpt-5.6-sol",
            "instructions": "<AETHER_MASK:bucket-a:1>",
            "tools": [{
                "type": "function",
                "name": "lookup",
                "description": "<AETHER_MASK:bucket-a:2>"
            }],
            "input": []
        });
        let raw_continuation = json!({
            "type": "response.create",
            "model": "gpt-5.6-sol",
            "previous_response_id": "resp_1",
            "instructions": "customer secret",
            "tools": [{
                "type": "function",
                "name": "lookup",
                "description": "customer secret"
            }],
            "input": [{"type": "function_call_output", "call_id": "call_1", "output": "ok"}]
        });

        let raw_identity = ResponsesLiteStaticConfig::from_response_create(&raw_first);
        let prepared = prepare_responses_lite_continuation(&raw_continuation, &raw_identity)
            .expect("the same plaintext configuration should be inherited");
        assert!(prepared.get("tools").is_none());
        assert!(prepared.get("instructions").is_none());

        let redacted_identity = ResponsesLiteStaticConfig::from_response_create(&redacted_first);
        assert_eq!(
            prepare_responses_lite_continuation(&raw_continuation, &redacted_identity),
            Err("responses_lite_continuation_static_config_changed")
        );
    }

    #[test]
    fn responses_lite_static_hash_is_stable_across_object_key_order() {
        let first = json!({
            "type": "response.create",
            "tools": [{
                "type": "function",
                "name": "lookup",
                "parameters": {"type": "object", "properties": {"b": {}, "a": {}}}
            }],
            "instructions": "same"
        });
        let reordered = json!({
            "instructions": "same",
            "tools": [{
                "parameters": {"properties": {"a": {}, "b": {}}, "type": "object"},
                "name": "lookup",
                "type": "function"
            }],
            "type": "response.create"
        });

        assert_eq!(
            ResponsesLiteStaticConfig::from_response_create(&first),
            ResponsesLiteStaticConfig::from_response_create(&reordered)
        );
    }

    #[test]
    fn decision_without_a_provider_body_uses_the_prepared_continuation_fallback() {
        let first = json!({
            "type": "response.create",
            "model": "gpt-5.6-sol",
            "instructions": "developer instructions",
            "tools": [{"type": "function", "name": "lookup", "parameters": {}}],
            "input": [{"role": "user", "content": "hello"}]
        });
        let stored = ResponsesLiteStaticConfig::from_response_create(&first);
        let continuation = json!({
            "type": "response.create",
            "model": "gpt-5.6-sol",
            "previous_response_id": "resp_1",
            "instructions": "developer instructions",
            "tools": [{"type": "function", "name": "lookup", "parameters": {}}],
            "input": [{"type": "function_call_output", "call_id": "call_1", "output": "ok"}]
        });
        let prepared = prepare_responses_lite_continuation(&continuation, &stored)
            .expect("matching configuration");
        let decision: crate::ai_serving::AiExecutionDecision = serde_json::from_value(json!({
            "action": "local"
        }))
        .expect("minimal decision");

        let normalization = ResponsesWebSocketBodyNormalization::for_tests("gpt-5.6-sol")
            .with_provider_type_for_tests("codex");
        let outbound = planned_response_create_event(&decision, &normalization, &prepared)
            .expect("prepared fallback should serialize");
        let outbound: Value = serde_json::from_str(&outbound).expect("provider event");
        assert!(outbound.get("tools").is_none());
        assert!(outbound.get("instructions").is_none());
        assert_eq!(outbound["previous_response_id"], "resp_1");
        assert_eq!(outbound["input"].as_array().map(Vec::len), Some(1));
    }

    #[test]
    fn responses_lite_continuation_removes_a_repeated_synthetic_prefix() {
        let first = json!({
            "type": "response.create",
            "model": "gpt-5.6-sol",
            "input": [
                {"type": "additional_tools", "role": "developer", "tools": [
                    {"type": "function", "name": "lookup", "parameters": {}}
                ]},
                {"type": "message", "role": "developer", "content": [
                    {"type": "input_text", "text": "developer instructions"}
                ]},
                {"role": "user", "content": "hello"}
            ]
        });
        let stored = ResponsesLiteStaticConfig::from_response_create(&first);
        let continuation = json!({
            "type": "response.create",
            "model": "gpt-5.6-sol",
            "previous_response_id": "resp_1",
            "input": [
                {"type": "additional_tools", "role": "developer", "tools": [
                    {"type": "function", "name": "lookup", "parameters": {}}
                ]},
                {"type": "message", "role": "developer", "content": [
                    {"type": "input_text", "text": "developer instructions"}
                ]},
                {"type": "function_call_output", "call_id": "call_1", "output": "ok"}
            ]
        });

        let prepared = prepare_responses_lite_continuation(&continuation, &stored)
            .expect("matching synthetic prefix should be inherited");
        let input = prepared["input"].as_array().expect("prepared input");
        assert_eq!(input.len(), 1);
        assert_eq!(input[0]["type"], "function_call_output");
    }

    #[test]
    fn responses_lite_continuation_removes_an_instructions_only_synthetic_prefix() {
        let first = json!({
            "type": "response.create",
            "model": "gpt-5.6-sol",
            "instructions": "developer instructions",
            "input": [{"role": "user", "content": "hello"}]
        });
        let stored = ResponsesLiteStaticConfig::from_response_create(&first);
        let continuation = json!({
            "type": "response.create",
            "model": "gpt-5.6-sol",
            "previous_response_id": "resp_1",
            "input": [
                {"type": "message", "role": "developer", "content": [
                    {"type": "input_text", "text": "developer instructions"}
                ]},
                {"type": "function_call_output", "call_id": "call_1", "output": "ok"}
            ]
        });

        let prepared = prepare_responses_lite_continuation(&continuation, &stored)
            .expect("matching instructions-only prefix should be inherited");
        let input = prepared["input"].as_array().expect("prepared input");
        assert_eq!(input.len(), 1);
        assert_eq!(input[0]["type"], "function_call_output");
    }
}
