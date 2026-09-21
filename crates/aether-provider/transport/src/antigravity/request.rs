use serde_json::{Map, Value};

use super::auth::AntigravityRequestAuth;
use super::version::{
    antigravity_request_user_agent_for_version, resolve_antigravity_client_version,
};

/// Antigravity 私有后端按模型卡限制 `maxOutputTokens`（对应 CLIProxyAPI 注册表里的
/// `max_completion_tokens`）。超过上限的请求会被上游拒绝，所以在信封层截断。
/// 前缀匹配放在精确匹配之后，用来兜住未列出的同系列型号。
const ANTIGRAVITY_MODEL_OUTPUT_LIMITS: &[(&str, u64)] = &[
    ("claude-opus-4-6-thinking", 64_000),
    ("claude-sonnet-4-6", 64_000),
    ("gemini-3-flash", 65_536),
    ("gemini-3.6-flash-high", 65_536),
    ("gemini-3.7-flash-high", 65_536),
    ("gemini-3.8-flash-high", 65_536),
    ("gemini-pro-agent", 65_535),
    ("gemini-3.1-pro-low", 65_535),
    ("gemini-3.1-flash-lite", 65_535),
    ("gemini-3.5-flash-lite", 65_535),
    ("gpt-oss-120b-medium", 32_768),
];
const ANTIGRAVITY_MODEL_OUTPUT_LIMIT_PREFIXES: &[(&str, u64)] = &[
    ("claude-", 64_000),
    ("gemini-", 65_535),
    ("gpt-oss-", 32_768),
];

/// 信封构造时的模型策略。`max_output_tokens_cap` 为 `None` 时按内置模型卡查表。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct AntigravityRequestPolicy {
    pub max_output_tokens_cap: Option<u64>,
}

/// Antigravity 上的 Claude 系列走 Anthropic Vertex 通道：工具调用只接受
/// `VALIDATED` 模式，`maxOutputTokens` 需要保留并按模型卡截断。
pub fn antigravity_model_is_claude(model: &str) -> bool {
    model
        .trim()
        .get(.."claude-".len())
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case("claude-"))
}

/// 内置模型卡里的输出上限；未知模型返回 `None`，此时不截断。
pub fn antigravity_model_max_output_tokens(model: &str) -> Option<u64> {
    let model = model.trim();
    if model.is_empty() {
        return None;
    }
    ANTIGRAVITY_MODEL_OUTPUT_LIMITS
        .iter()
        .find(|(known, _)| known.eq_ignore_ascii_case(model))
        .or_else(|| {
            ANTIGRAVITY_MODEL_OUTPUT_LIMIT_PREFIXES
                .iter()
                .find(|(prefix, _)| {
                    model
                        .get(..prefix.len())
                        .is_some_and(|head| head.eq_ignore_ascii_case(prefix))
                })
        })
        .map(|(_, limit)| *limit)
}

/// 与 CLIProxyAPI `antigravity_executor_request.go` 一致的模型分支：
/// 1. `maxOutputTokens` 超过模型卡上限时截断到上限；
/// 2. Claude 模型强制 `toolConfig.functionCallingConfig.mode = "VALIDATED"`；
/// 3. 非 Claude 模型删除 `maxOutputTokens`，交给上游按模型默认值处理。
fn apply_antigravity_model_request_policy(
    request: &mut Map<String, Value>,
    model: &str,
    policy: AntigravityRequestPolicy,
) {
    let cap = policy
        .max_output_tokens_cap
        .or_else(|| antigravity_model_max_output_tokens(model));
    if let Some(cap) = cap {
        for config_key in ["generationConfig", "generation_config"] {
            let Some(config) = request.get_mut(config_key).and_then(Value::as_object_mut) else {
                continue;
            };
            for tokens_key in ["maxOutputTokens", "max_output_tokens"] {
                let Some(current) = config.get(tokens_key).and_then(Value::as_u64) else {
                    continue;
                };
                if current > cap {
                    config.insert(tokens_key.to_string(), Value::from(cap));
                }
            }
        }
    }

    if antigravity_model_is_claude(model) {
        let tool_config = request
            .entry("toolConfig".to_string())
            .or_insert_with(|| Value::Object(Map::new()));
        if !tool_config.is_object() {
            *tool_config = Value::Object(Map::new());
        }
        let Some(tool_config) = tool_config.as_object_mut() else {
            return;
        };
        tool_config.remove("function_calling_config");
        let calling_config = tool_config
            .entry("functionCallingConfig".to_string())
            .or_insert_with(|| Value::Object(Map::new()));
        if !calling_config.is_object() {
            *calling_config = Value::Object(Map::new());
        }
        if let Some(calling_config) = calling_config.as_object_mut() {
            calling_config.insert("mode".to_string(), Value::String("VALIDATED".to_string()));
        }
        return;
    }

    for config_key in ["generationConfig", "generation_config"] {
        let Some(config) = request.get_mut(config_key).and_then(Value::as_object_mut) else {
            continue;
        };
        config.remove("maxOutputTokens");
        config.remove("max_output_tokens");
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AntigravityEnvelopeRequestType {
    Agent,
    Checkpoint,
    EndpointTest,
}

impl AntigravityEnvelopeRequestType {
    fn as_str(self) -> &'static str {
        match self {
            Self::Agent => "agent",
            Self::Checkpoint => "checkpoint",
            Self::EndpointTest => "endpoint_test",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum AntigravityRequestEnvelopeSupport {
    Supported(Value),
    Unsupported(AntigravityRequestEnvelopeUnsupportedReason),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AntigravityRequestEnvelopeUnsupportedReason {
    NonObjectBody,
    MissingContents,
    MissingRequestId,
    MissingModel,
}

pub fn classify_antigravity_safe_request_body(
    request_body: &Value,
) -> Result<(), AntigravityRequestEnvelopeUnsupportedReason> {
    let Value::Object(map) = request_body else {
        return Err(AntigravityRequestEnvelopeUnsupportedReason::NonObjectBody);
    };
    if !map.contains_key("contents") && existing_v1internal_request_object(map).is_none() {
        return Err(AntigravityRequestEnvelopeUnsupportedReason::MissingContents);
    }

    Ok(())
}

pub fn build_antigravity_safe_v1internal_request(
    auth: &AntigravityRequestAuth,
    request_id: &str,
    model: &str,
    request_body: &Value,
    request_type: AntigravityEnvelopeRequestType,
) -> AntigravityRequestEnvelopeSupport {
    build_antigravity_safe_v1internal_request_with_policy(
        auth,
        request_id,
        model,
        request_body,
        request_type,
        AntigravityRequestPolicy::default(),
    )
}

pub fn build_antigravity_safe_v1internal_request_with_policy(
    auth: &AntigravityRequestAuth,
    request_id: &str,
    model: &str,
    request_body: &Value,
    request_type: AntigravityEnvelopeRequestType,
    policy: AntigravityRequestPolicy,
) -> AntigravityRequestEnvelopeSupport {
    if request_id.trim().is_empty() {
        return AntigravityRequestEnvelopeSupport::Unsupported(
            AntigravityRequestEnvelopeUnsupportedReason::MissingRequestId,
        );
    }
    if model.trim().is_empty() {
        return AntigravityRequestEnvelopeSupport::Unsupported(
            AntigravityRequestEnvelopeUnsupportedReason::MissingModel,
        );
    }
    if let Err(reason) = classify_antigravity_safe_request_body(request_body) {
        return AntigravityRequestEnvelopeSupport::Unsupported(reason);
    }

    let Value::Object(source) = request_body else {
        return AntigravityRequestEnvelopeSupport::Unsupported(
            AntigravityRequestEnvelopeUnsupportedReason::NonObjectBody,
        );
    };

    if let Some(existing_request) = existing_v1internal_request_object(source) {
        let mut inner_request: Map<String, Value> = existing_request.clone();
        inner_request.remove("model");
        inner_request.remove("safetySettings");
        inner_request.remove("safety_settings");
        normalize_antigravity_builtin_tool_names(&mut inner_request);
        normalize_antigravity_function_declaration_parameters(&mut inner_request);
        apply_antigravity_model_request_policy(&mut inner_request, model, policy);
        let request_id = non_empty_string_field(source, "requestId").unwrap_or(request_id);
        let default_user_agent = antigravity_request_user_agent_for_version(
            &resolve_antigravity_client_version(auth.client_version.as_deref()),
        );
        let user_agent =
            non_empty_string_field(source, "userAgent").unwrap_or(default_user_agent.as_str());
        let request_type =
            existing_v1internal_request_type(source).unwrap_or_else(|| request_type.as_str());

        return AntigravityRequestEnvelopeSupport::Supported(serde_json::json!({
            "project": auth.project_id,
            "requestId": request_id,
            "request": Value::Object(inner_request),
            "model": model,
            "userAgent": user_agent,
            "requestType": request_type,
        }));
    }

    let mut inner_request: Map<String, Value> = source.clone();
    inner_request.remove("model");
    inner_request.remove("safetySettings");
    inner_request.remove("safety_settings");
    normalize_antigravity_builtin_tool_names(&mut inner_request);
    normalize_antigravity_function_declaration_parameters(&mut inner_request);
    apply_antigravity_model_request_policy(&mut inner_request, model, policy);

    AntigravityRequestEnvelopeSupport::Supported(serde_json::json!({
        "project": auth.project_id,
        "requestId": request_id,
        "request": Value::Object(inner_request),
        "model": model,
        "userAgent": antigravity_request_user_agent_for_version(
            &resolve_antigravity_client_version(auth.client_version.as_deref()),
        ),
        "requestType": request_type.as_str(),
    }))
}

/// Antigravity's private v1internal Gemini surface still uses the legacy
/// `googleSearchRetrieval` spelling. The public Gemini converter emits the
/// newer `googleSearch` spelling, which the private backend rejects when it is
/// combined with function declarations. Normalize only at this transport
/// boundary so public Gemini requests retain their native shape.
fn normalize_antigravity_builtin_tool_names(request: &mut Map<String, Value>) {
    let Some(tools) = request.get_mut("tools").and_then(Value::as_array_mut) else {
        return;
    };

    for tool in tools {
        let Some(tool_object) = tool.as_object_mut() else {
            continue;
        };

        if let Some(payload) = tool_object.remove("googleSearch") {
            tool_object
                .entry("googleSearchRetrieval".to_string())
                .or_insert(payload);
        }
        if let Some(payload) = tool_object.remove("google_search") {
            tool_object
                .entry("googleSearchRetrieval".to_string())
                .or_insert(payload);
        }
    }
}

fn normalize_antigravity_function_declaration_parameters(request: &mut Map<String, Value>) {
    let Some(tools) = request.get_mut("tools").and_then(Value::as_array_mut) else {
        return;
    };

    for tool in tools {
        let Some(tool_object) = tool.as_object_mut() else {
            continue;
        };
        for key in ["functionDeclarations", "function_declarations"] {
            let Some(declarations) = tool_object.get_mut(key).and_then(Value::as_array_mut) else {
                continue;
            };
            for declaration in declarations {
                let Some(declaration_object) = declaration.as_object_mut() else {
                    continue;
                };
                if let Some(parameters) = declaration_object.remove("parametersJsonSchema") {
                    declaration_object
                        .entry("parameters".to_string())
                        .or_insert(parameters);
                }
                if let Some(parameters) = declaration_object.remove("parameters_json_schema") {
                    declaration_object
                        .entry("parameters".to_string())
                        .or_insert(parameters);
                }
            }
        }
    }
}

fn existing_v1internal_request_object(source: &Map<String, Value>) -> Option<&Map<String, Value>> {
    source
        .get("request")
        .and_then(Value::as_object)
        .filter(|request| request.contains_key("contents"))
}

fn non_empty_string_field<'a>(source: &'a Map<String, Value>, key: &str) -> Option<&'a str> {
    source
        .get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
}

fn existing_v1internal_request_type(source: &Map<String, Value>) -> Option<&str> {
    match non_empty_string_field(source, "requestType")? {
        "agent" => Some("agent"),
        "checkpoint" => Some("checkpoint"),
        "endpoint_test" => Some("endpoint_test"),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{
        antigravity_model_is_claude, antigravity_model_max_output_tokens,
        build_antigravity_safe_v1internal_request,
        build_antigravity_safe_v1internal_request_with_policy,
        classify_antigravity_safe_request_body, AntigravityEnvelopeRequestType,
        AntigravityRequestAuth, AntigravityRequestEnvelopeSupport, AntigravityRequestPolicy,
    };
    use crate::antigravity::ANTIGRAVITY_REQUEST_USER_AGENT;

    fn sample_auth() -> AntigravityRequestAuth {
        AntigravityRequestAuth {
            project_id: "project-ant-123".to_string(),
            client_version: None,
            session_id: None,
        }
    }

    #[test]
    fn real_agent_request_preserves_antigravity_agent_fields() {
        let request_body = json!({
            "model": "client-side-model-should-not-be-nested",
            "contents": [
                {
                    "role": "user",
                    "parts": [
                        { "text": "Reply with OK only." }
                    ]
                }
            ],
            "systemInstruction": {
                "role": "user",
                "parts": [
                    { "text": "Antigravity agent system prompt" }
                ]
            },
            "generationConfig": {
                "maxOutputTokens": 8192,
                "thinkingConfig": {
                    "includeThoughts": true,
                    "thinkingBudget": 4000
                }
            },
            "toolConfig": {
                "includeServerSideToolInvocations": true,
                "functionCallingConfig": {
                    "mode": "VALIDATED"
                }
            },
            "tools": [
                {
                    "googleSearch": {}
                },
                {
                    "functionDeclarations": [
                        {
                            "name": "run_command",
                            "description": "Run a command",
                            "parameters": {
                                "type": "object",
                                "properties": {
                                    "cmd": { "type": "string" }
                                },
                                "required": ["cmd"]
                            }
                        }
                    ]
                }
            ],
            "labels": {
                "trajectory_id": "trajectory-123",
                "used_claude": "false"
            },
            "sessionId": "session-ant-123",
            "safetySettings": [
                { "category": "HARM_CATEGORY_UNSPECIFIED" }
            ]
        });

        assert_eq!(
            classify_antigravity_safe_request_body(&request_body),
            Ok(())
        );

        let envelope = match build_antigravity_safe_v1internal_request(
            &sample_auth(),
            "request-ant-agent-123",
            "gemini-3.5-flash-low",
            &request_body,
            AntigravityEnvelopeRequestType::Agent,
        ) {
            AntigravityRequestEnvelopeSupport::Supported(envelope) => envelope,
            AntigravityRequestEnvelopeSupport::Unsupported(reason) => {
                panic!("real agent envelope should be supported: {reason:?}")
            }
        };

        assert_eq!(envelope["project"], "project-ant-123");
        assert_eq!(envelope["requestId"], "request-ant-agent-123");
        assert_eq!(envelope["model"], "gemini-3.5-flash-low");
        assert_eq!(envelope["userAgent"], ANTIGRAVITY_REQUEST_USER_AGENT);
        assert_eq!(envelope["requestType"], "agent");
        assert!(envelope["request"].get("model").is_none());
        assert!(envelope["request"].get("safetySettings").is_none());
        assert_eq!(
            envelope["request"]["systemInstruction"]["parts"][0]["text"],
            "Antigravity agent system prompt"
        );
        assert_eq!(
            envelope["request"]["generationConfig"]["thinkingConfig"]["thinkingBudget"],
            4000
        );
        // 非 Claude 模型不带 maxOutputTokens，由上游按模型默认值处理。
        assert!(envelope["request"]["generationConfig"]
            .get("maxOutputTokens")
            .is_none());
        assert_eq!(
            envelope["request"]["toolConfig"]["functionCallingConfig"]["mode"],
            "VALIDATED"
        );
        assert_eq!(
            envelope["request"]["toolConfig"]["includeServerSideToolInvocations"],
            true
        );
        assert!(envelope["request"]["toolConfig"]
            .get("include_server_side_tool_invocations")
            .is_none());
        assert!(envelope["request"]["tools"][0]
            .get("googleSearch")
            .is_none());
        assert_eq!(
            envelope["request"]["tools"][0]["googleSearchRetrieval"],
            json!({})
        );
        assert_eq!(
            envelope["request"]["tools"][1]["functionDeclarations"][0]["name"],
            "run_command"
        );
        assert_eq!(
            envelope["request"]["tools"][1]["functionDeclarations"][0]["parameters"]["properties"]
                ["cmd"]["type"],
            "string"
        );
        assert!(envelope["request"]["tools"][1]["functionDeclarations"][0]
            .get("parametersJsonSchema")
            .is_none());
        assert_eq!(
            envelope["request"]["labels"]["trajectory_id"],
            "trajectory-123"
        );
        assert_eq!(envelope["request"]["sessionId"], "session-ant-123");
    }

    #[test]
    fn checkpoint_request_type_builds_checkpoint_envelope() {
        let request_body = json!({
            "contents": [
                {
                    "role": "user",
                    "parts": [
                        { "text": "checkpoint context" }
                    ]
                }
            ],
            "generationConfig": {
                "maxOutputTokens": 8192,
                "thinkingConfig": {
                    "includeThoughts": true,
                    "thinkingBudget": 4000
                }
            },
            "toolConfig": {
                "functionCallingConfig": {
                    "mode": "NONE"
                }
            }
        });

        let envelope = match build_antigravity_safe_v1internal_request(
            &sample_auth(),
            "request-ant-checkpoint-123",
            "gemini-3.5-flash-low",
            &request_body,
            AntigravityEnvelopeRequestType::Checkpoint,
        ) {
            AntigravityRequestEnvelopeSupport::Supported(envelope) => envelope,
            AntigravityRequestEnvelopeSupport::Unsupported(reason) => {
                panic!("checkpoint envelope should be supported: {reason:?}")
            }
        };

        assert_eq!(envelope["requestType"], "checkpoint");
        assert_eq!(
            envelope["request"]["toolConfig"]["functionCallingConfig"]["mode"],
            "NONE"
        );
    }

    #[test]
    fn existing_v1internal_envelope_is_not_double_wrapped() {
        let request_body = json!({
            "project": "client-side-project",
            "requestId": "client-request-id-123",
            "model": "gemini-3.5-flash-low",
            "userAgent": "antigravity",
            "requestType": "checkpoint",
            "request": {
                "contents": [
                    {
                        "role": "user",
                        "parts": [
                            { "text": "checkpoint context" }
                        ]
                    }
                ],
                "generationConfig": {
                    "thinkingConfig": {
                        "includeThoughts": true
                    }
                },
                "toolConfig": {
                    "functionCallingConfig": {
                        "mode": "NONE"
                    }
                },
                "tools": [{
                    "google_search": {
                        "dynamicRetrievalConfig": {
                            "mode": "MODE_UNSPECIFIED"
                        }
                    }
                }]
            }
        });

        assert_eq!(
            classify_antigravity_safe_request_body(&request_body),
            Ok(())
        );

        let envelope = match build_antigravity_safe_v1internal_request(
            &sample_auth(),
            "trace-request-id-should-not-overwrite-client-id",
            "mapped-antigravity-model",
            &request_body,
            AntigravityEnvelopeRequestType::Agent,
        ) {
            AntigravityRequestEnvelopeSupport::Supported(envelope) => envelope,
            AntigravityRequestEnvelopeSupport::Unsupported(reason) => {
                panic!("existing v1internal envelope should be supported: {reason:?}")
            }
        };

        assert_eq!(envelope["project"], "project-ant-123");
        assert_eq!(envelope["requestId"], "client-request-id-123");
        assert_eq!(envelope["model"], "mapped-antigravity-model");
        assert_eq!(envelope["userAgent"], "antigravity");
        assert_eq!(envelope["requestType"], "checkpoint");
        assert!(envelope["request"].get("request").is_none());
        assert_eq!(
            envelope["request"]["contents"][0]["parts"][0]["text"],
            "checkpoint context"
        );
        assert_eq!(
            envelope["request"]["toolConfig"]["functionCallingConfig"]["mode"],
            "NONE"
        );
        assert!(envelope["request"]["tools"][0]
            .get("google_search")
            .is_none());
        assert_eq!(
            envelope["request"]["tools"][0]["googleSearchRetrieval"],
            json!({
                "dynamicRetrievalConfig": {
                    "mode": "MODE_UNSPECIFIED"
                }
            })
        );
    }

    #[test]
    fn antigravity_envelope_normalizes_json_schema_parameter_spellings() {
        let request_body = json!({
            "contents": [{
                "role": "user",
                "parts": [{ "text": "hello" }]
            }],
            "tools": [{
                "function_declarations": [{
                    "name": "lookup",
                    "parametersJsonSchema": { "type": "object" }
                }, {
                    "name": "weather",
                    "parameters_json_schema": { "type": "object" }
                }]
            }]
        });

        let envelope = match build_antigravity_safe_v1internal_request(
            &sample_auth(),
            "request-ant-schema-123",
            "gemini-3.5-flash-low",
            &request_body,
            AntigravityEnvelopeRequestType::Agent,
        ) {
            AntigravityRequestEnvelopeSupport::Supported(envelope) => envelope,
            AntigravityRequestEnvelopeSupport::Unsupported(reason) => {
                panic!("schema envelope should be supported: {reason:?}")
            }
        };

        let declarations = &envelope["request"]["tools"][0]["function_declarations"];
        assert_eq!(declarations[0]["parameters"]["type"], "object");
        assert_eq!(declarations[1]["parameters"]["type"], "object");
        assert!(declarations[0].get("parametersJsonSchema").is_none());
        assert!(declarations[1].get("parameters_json_schema").is_none());
    }
    #[test]
    fn claude_models_force_validated_function_calling_and_keep_a_capped_output_limit() {
        let request_body = json!({
            "contents": [{ "role": "user", "parts": [{ "text": "hi" }] }],
            "generationConfig": { "maxOutputTokens": 128000, "temperature": 0.1 },
            "toolConfig": { "functionCallingConfig": { "mode": "AUTO" } },
            "tools": [{ "functionDeclarations": [{ "name": "lookup" }] }]
        });

        let envelope = match build_antigravity_safe_v1internal_request(
            &sample_auth(),
            "request-claude-1",
            "claude-sonnet-4-6",
            &request_body,
            AntigravityEnvelopeRequestType::Agent,
        ) {
            AntigravityRequestEnvelopeSupport::Supported(envelope) => envelope,
            AntigravityRequestEnvelopeSupport::Unsupported(reason) => {
                panic!("claude envelope should be supported: {reason:?}")
            }
        };

        assert_eq!(
            envelope["request"]["toolConfig"]["functionCallingConfig"]["mode"],
            "VALIDATED"
        );
        assert_eq!(
            envelope["request"]["generationConfig"]["maxOutputTokens"],
            64000
        );
        assert_eq!(envelope["request"]["generationConfig"]["temperature"], 0.1);
    }

    #[test]
    fn claude_models_get_a_tool_config_even_when_the_client_sent_none() {
        let request_body = json!({
            "contents": [{ "role": "user", "parts": [{ "text": "hi" }] }],
            "generationConfig": { "maxOutputTokens": 4096 }
        });

        let envelope = match build_antigravity_safe_v1internal_request(
            &sample_auth(),
            "request-claude-2",
            "claude-opus-4-6-thinking",
            &request_body,
            AntigravityEnvelopeRequestType::Agent,
        ) {
            AntigravityRequestEnvelopeSupport::Supported(envelope) => envelope,
            AntigravityRequestEnvelopeSupport::Unsupported(reason) => {
                panic!("claude envelope should be supported: {reason:?}")
            }
        };

        assert_eq!(
            envelope["request"]["toolConfig"]["functionCallingConfig"]["mode"],
            "VALIDATED"
        );
        // 未超过上限的值原样保留。
        assert_eq!(
            envelope["request"]["generationConfig"]["maxOutputTokens"],
            4096
        );
    }

    #[test]
    fn non_claude_models_drop_max_output_tokens_in_both_spellings() {
        let request_body = json!({
            "contents": [{ "role": "user", "parts": [{ "text": "hi" }] }],
            "generationConfig": { "maxOutputTokens": 8192, "max_output_tokens": 8192, "topP": 0.9 },
            "toolConfig": { "functionCallingConfig": { "mode": "NONE" } }
        });

        let envelope = match build_antigravity_safe_v1internal_request(
            &sample_auth(),
            "request-gemini-1",
            "gemini-3.5-flash-low",
            &request_body,
            AntigravityEnvelopeRequestType::Agent,
        ) {
            AntigravityRequestEnvelopeSupport::Supported(envelope) => envelope,
            AntigravityRequestEnvelopeSupport::Unsupported(reason) => {
                panic!("gemini envelope should be supported: {reason:?}")
            }
        };

        let generation_config = &envelope["request"]["generationConfig"];
        assert!(generation_config.get("maxOutputTokens").is_none());
        assert!(generation_config.get("max_output_tokens").is_none());
        assert_eq!(generation_config["topP"], 0.9);
        // 非 Claude 模型不动 toolConfig。
        assert_eq!(
            envelope["request"]["toolConfig"]["functionCallingConfig"]["mode"],
            "NONE"
        );
    }

    #[test]
    fn explicit_policy_cap_overrides_the_builtin_model_card() {
        let request_body = json!({
            "contents": [{ "role": "user", "parts": [{ "text": "hi" }] }],
            "generationConfig": { "maxOutputTokens": 50000 }
        });

        let envelope = match build_antigravity_safe_v1internal_request_with_policy(
            &sample_auth(),
            "request-claude-3",
            "claude-sonnet-4-6",
            &request_body,
            AntigravityEnvelopeRequestType::Agent,
            AntigravityRequestPolicy {
                max_output_tokens_cap: Some(32000),
            },
        ) {
            AntigravityRequestEnvelopeSupport::Supported(envelope) => envelope,
            AntigravityRequestEnvelopeSupport::Unsupported(reason) => {
                panic!("claude envelope should be supported: {reason:?}")
            }
        };

        assert_eq!(
            envelope["request"]["generationConfig"]["maxOutputTokens"],
            32000
        );
    }

    #[test]
    fn existing_envelope_also_gets_the_model_policy() {
        let request_body = json!({
            "project": "client-project",
            "requestId": "client-request",
            "model": "claude-sonnet-4-6",
            "userAgent": "antigravity",
            "requestType": "agent",
            "request": {
                "contents": [{ "role": "user", "parts": [{ "text": "hi" }] }],
                "generationConfig": { "maxOutputTokens": 999999 }
            }
        });

        let envelope = match build_antigravity_safe_v1internal_request(
            &sample_auth(),
            "trace-id",
            "claude-sonnet-4-6",
            &request_body,
            AntigravityEnvelopeRequestType::Agent,
        ) {
            AntigravityRequestEnvelopeSupport::Supported(envelope) => envelope,
            AntigravityRequestEnvelopeSupport::Unsupported(reason) => {
                panic!("existing envelope should be supported: {reason:?}")
            }
        };

        assert_eq!(
            envelope["request"]["generationConfig"]["maxOutputTokens"],
            64000
        );
        assert_eq!(
            envelope["request"]["toolConfig"]["functionCallingConfig"]["mode"],
            "VALIDATED"
        );
    }

    #[test]
    fn envelope_user_agent_follows_the_key_level_client_version() {
        let auth = AntigravityRequestAuth {
            project_id: "project-ant-123".to_string(),
            client_version: Some("3.2.1".to_string()),
            session_id: None,
        };
        let request_body = json!({
            "contents": [{ "role": "user", "parts": [{ "text": "hi" }] }]
        });

        let envelope = match build_antigravity_safe_v1internal_request(
            &auth,
            "request-ua",
            "gemini-3.5-flash-low",
            &request_body,
            AntigravityEnvelopeRequestType::Agent,
        ) {
            AntigravityRequestEnvelopeSupport::Supported(envelope) => envelope,
            AntigravityRequestEnvelopeSupport::Unsupported(reason) => {
                panic!("envelope should be supported: {reason:?}")
            }
        };

        assert_eq!(envelope["userAgent"], "vscode/1.X.X (Antigravity/3.2.1)");
    }

    #[test]
    fn model_card_lookup_matches_exact_ids_then_prefixes() {
        assert_eq!(
            antigravity_model_max_output_tokens("claude-sonnet-4-6"),
            Some(64000)
        );
        assert_eq!(
            antigravity_model_max_output_tokens("CLAUDE-OPUS-4-6-THINKING"),
            Some(64000)
        );
        assert_eq!(
            antigravity_model_max_output_tokens("claude-future-9"),
            Some(64000)
        );
        assert_eq!(
            antigravity_model_max_output_tokens("gemini-3-flash"),
            Some(65536)
        );
        assert_eq!(
            antigravity_model_max_output_tokens("gemini-3.5-flash-low"),
            Some(65535)
        );
        assert_eq!(antigravity_model_max_output_tokens("chat_23310"), None);
        assert_eq!(antigravity_model_max_output_tokens(""), None);

        assert!(antigravity_model_is_claude("claude-sonnet-4-6"));
        assert!(antigravity_model_is_claude(" Claude-Opus-4-6-thinking "));
        assert!(!antigravity_model_is_claude("gemini-3.5-flash-low"));
        assert!(!antigravity_model_is_claude("claude"));
    }
}
