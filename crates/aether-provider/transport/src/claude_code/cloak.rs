//! 第三方 Claude Code 请求的改写流水线（计划 2.1–2.5 的单入口）。
//!
//! 顺序固定，任何新增步骤都必须落在这条序列的正确位置：
//!
//! 1. **敏感词混淆（P6 挂钩）** — [`ClaudeCodeCloakPipeline::sensitive_words_hook`]，
//!    必须先于一切身份改写，否则签名会把混淆前的字节算进去；
//! 2. thinking 归一化与 `tool_choice` 剥 thinking（[`super::thinking`]）；
//! 3. 身份：`metadata.user_id` 重建、计费头兜底（[`super::identity`]、[`super::signing`]）；
//! 4. `cache_control` 治理（[`super::cache_control`]）；
//! 5. CCH 签名（[`super::signing`]）— **之后 body 不得再动**。
//!
//! beta 头由 [`super::beta`] 按最终 body 组装，调用方在签名之后拿 body 去算 header 即可
//! （header 不参与签名）。
//!
//! 原生 Claude Code 请求不进这条流水线（[`super::client_detection`] 决定）。

use serde_json::Value;

use super::cache_control::{
    apply_claude_code_cache_control_policy, ClaudeCodeCacheControlPolicy,
    ClaudeCodeCacheControlSummary,
};
use super::client_detection::ClaudeCodeClientDetection;
use super::identity::{apply_claude_code_metadata_user_id, ClaudeCodeIdentityInput};
use super::profile::ClaudeCodeTransportIdentityProfile;
use super::signing::{
    build_claude_code_fallback_billing_header, ensure_claude_code_billing_cch_placeholder,
    sign_claude_code_request_body, ClaudeCodeSigningError,
};
use super::thinking::{normalize_claude_code_thinking, ClaudeCodeThinkingSummary};

/// P6 敏感词混淆挂钩：在所有身份改写之前对 body 就地处理，返回写入 report_context 的摘要。
pub type SensitiveWordsHook<'a> = &'a mut dyn FnMut(&mut Value) -> Option<Value>;

/// 流水线输入。
pub struct ClaudeCodeCloakPipeline<'a> {
    pub profile: ClaudeCodeTransportIdentityProfile,
    pub detection: &'a ClaudeCodeClientDetection,
    /// 上游凭据是否为 OAuth。
    pub oauth_credential: bool,
    pub is_subagent: bool,
    pub identity: ClaudeCodeIdentityInput<'a>,
    /// 计费头兜底使用的入口点（默认 `cli`）。
    pub entrypoint: Option<&'a str>,
    /// 是否做 CCH 签名（OAuth 凭据或显式开启）。
    pub sign: bool,
    /// P6: sensitive word obfuscation hook。`None` 表示未配置词表。
    pub sensitive_words_hook: Option<SensitiveWordsHook<'a>>,
}

/// 流水线产出，整体写入 `report_context.claude_code_cloak`。
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize)]
pub struct ClaudeCodeCloakReport {
    pub applied: bool,
    pub client: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sensitive_words_obfuscation: Option<Value>,
    pub thinking: ClaudeCodeThinkingSummary,
    pub identity_rewritten: bool,
    pub billing_header_injected: bool,
    pub cache_control: ClaudeCodeCacheControlSummary,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cch: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
}

impl ClaudeCodeCloakReport {
    pub fn to_json(&self) -> Value {
        serde_json::to_value(self).unwrap_or(Value::Null)
    }
}

/// 执行流水线。`body` 必须是最终要发给上游的 JSON（模型映射、body_rules 等都已应用）。
pub fn apply_claude_code_cloak_pipeline(
    body: &mut Value,
    pipeline: ClaudeCodeCloakPipeline<'_>,
) -> Result<ClaudeCodeCloakReport, ClaudeCodeSigningError> {
    let mut report = ClaudeCodeCloakReport {
        applied: true,
        client: pipeline.detection.to_json(),
        ..ClaudeCodeCloakReport::default()
    };

    // 1. P6: sensitive word obfuscation hook — 必须在身份改写与签名之前。
    if let Some(hook) = pipeline.sensitive_words_hook {
        report.sensitive_words_obfuscation = hook(body);
    }

    // 2. thinking 归一化。
    report.thinking = normalize_claude_code_thinking(body);

    // 3. 身份与计费头。
    let session_id = apply_claude_code_metadata_user_id(body, pipeline.identity);
    report.identity_rewritten = session_id.is_some();
    report.session_id = session_id;
    let fallback_billing = build_claude_code_fallback_billing_header(
        pipeline.profile.billing_cli_version(),
        &first_user_message_text(body),
        pipeline.entrypoint.unwrap_or("cli"),
        pipeline.is_subagent,
    );
    if pipeline.sign {
        report.billing_header_injected =
            ensure_claude_code_billing_cch_placeholder(body, Some(&fallback_billing));
    }

    // 4. cache_control 治理。
    report.cache_control = apply_claude_code_cache_control_policy(
        body,
        ClaudeCodeCacheControlPolicy {
            ensure_breakpoints: true,
            upgrade_to_1h: pipeline.oauth_credential && !pipeline.is_subagent,
            strip_ttl: pipeline.is_subagent && !super::beta::body_has_1h_cache_ttl(body),
        },
    );

    // 5. CCH 签名；之后 body 不可再变。
    if pipeline.sign {
        report.cch = sign_claude_code_request_body(body)?;
    }
    Ok(report)
}

fn first_user_message_text(body: &Value) -> String {
    let Some(messages) = body.get("messages").and_then(Value::as_array) else {
        return String::new();
    };
    let Some(message) = messages
        .iter()
        .find(|message| message.get("role").and_then(Value::as_str) == Some("user"))
    else {
        return String::new();
    };
    match message.get("content") {
        Some(Value::String(text)) => text.clone(),
        Some(Value::Array(parts)) => parts
            .iter()
            .filter(|part| part.get("type").and_then(Value::as_str) == Some("text"))
            .filter_map(|part| part.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join(""),
        _ => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::claude_code::client_detection::detect_claude_code_client;
    use crate::claude_code::current_claude_code_transport_identity_profile;
    use crate::claude_code::signing::{claude_code_cch_from_body, sign_claude_code_request_bytes};
    use serde_json::json;

    fn third_party_detection() -> ClaudeCodeClientDetection {
        let mut headers = http::HeaderMap::new();
        headers.insert("user-agent", "anthropic-sdk-python/0.40".parse().unwrap());
        detect_claude_code_client(&headers, None, false)
    }

    #[test]
    fn pipeline_runs_in_fixed_order_and_signature_covers_final_bytes() {
        let detection = third_party_detection();
        let mut body = json!({
            "model":"claude-opus-4-6",
            "max_tokens":1024,
            "system":"You are a proxy assistant.",
            "thinking":{"type":"enabled","budget_tokens":4096},
            "metadata":{"user_id":"user_abc_session_xyz"},
            "messages":[{"role":"user","content":"hello proxy"}]
        });
        let mut hook_calls = 0usize;
        let mut hook = |body: &mut Value| {
            hook_calls += 1;
            // 混淆挂钩必须先于身份改写：此时 user_id 仍是客户端原值。
            assert_eq!(body["metadata"]["user_id"], "user_abc_session_xyz");
            Some(json!({"applied": true, "replaced": 0}))
        };
        let report = apply_claude_code_cloak_pipeline(
            &mut body,
            ClaudeCodeCloakPipeline {
                profile: *current_claude_code_transport_identity_profile(),
                detection: &detection,
                oauth_credential: true,
                is_subagent: false,
                identity: ClaudeCodeIdentityInput {
                    device_id: "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
                    account_uuid: "6f1c7d8e-1b2c-4d3e-8f90-123456789abc",
                    session_id: Some("9a0b1c2d-3e4f-4a5b-8c6d-7e8f90a1b2c3"),
                },
                entrypoint: None,
                sign: true,
                sensitive_words_hook: Some(&mut hook),
            },
        )
        .expect("pipeline");
        assert_eq!(hook_calls, 1);
        assert!(report.applied);
        assert_eq!(
            report.sensitive_words_obfuscation,
            Some(json!({"applied": true, "replaced": 0}))
        );
        assert!(report.thinking.converted_to_adaptive);
        assert!(report.identity_rewritten);
        assert!(report.billing_header_injected);
        assert!(report.cache_control.injected >= 1);
        let cch = report.cch.clone().expect("signed");
        assert_eq!(
            claude_code_cch_from_body(&body).as_deref(),
            Some(cch.as_str())
        );

        // 身份：JSON user_id
        let user_id: Value =
            serde_json::from_str(body["metadata"]["user_id"].as_str().unwrap()).unwrap();
        assert_eq!(
            user_id["session_id"],
            "9a0b1c2d-3e4f-4a5b-8c6d-7e8f90a1b2c3"
        );
        // 计费头在 system[0]，且原 system 文本保留。
        assert!(body["system"][0]["text"]
            .as_str()
            .unwrap()
            .starts_with("x-anthropic-billing-header: cc_version=2.1.161."));
        assert_eq!(body["system"][1]["text"], "You are a proxy assistant.");
        // 1h ttl
        assert_eq!(body["system"][1]["cache_control"]["ttl"], "1h");

        // 签名覆盖最终字节：重新序列化再签得到同一个值。
        let bytes = serde_json::to_vec(&body).unwrap();
        let resigned = sign_claude_code_request_bytes(&bytes).unwrap();
        assert_eq!(resigned.cch.as_deref(), Some(cch.as_str()));
        assert_eq!(resigned.bytes, bytes);
        assert_eq!(report.to_json()["client"]["kind"], "unknown");
    }

    #[test]
    fn api_key_credential_without_signing_keeps_5m_ttl_and_no_billing_block() {
        let detection = third_party_detection();
        let mut body = json!({"model":"claude-sonnet-4-5-20250929","max_tokens":100,"messages":[{"role":"user","content":"hi"}]});
        let report = apply_claude_code_cloak_pipeline(
            &mut body,
            ClaudeCodeCloakPipeline {
                profile: *current_claude_code_transport_identity_profile(),
                detection: &detection,
                oauth_credential: false,
                is_subagent: false,
                identity: ClaudeCodeIdentityInput {
                    device_id: "d",
                    account_uuid: "a",
                    session_id: None,
                },
                entrypoint: None,
                sign: false,
                sensitive_words_hook: None,
            },
        )
        .unwrap();
        assert_eq!(report.cch, None);
        assert!(!report.billing_header_injected);
        assert!(body.get("system").is_none());
        assert_eq!(
            body["messages"][0]["content"][0]["cache_control"],
            json!({"type":"ephemeral"})
        );
    }
}
