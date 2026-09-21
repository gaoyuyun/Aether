//! `anthropic-beta` 按条件有序组装（计划 2.2）。
//!
//! 原生 Claude Code 的 beta 列表是**逐请求**生成的，而不是固定字符串：顺序固定、
//! 每一项都有出现条件。这里按凭据类型、请求类型（messages / count_tokens）、是否子代理、
//! thinking 是否启用、请求体形状生成，并剔除已知无效项（例如没有 `speed:fast` 却声明
//! `fast-mode`）。不认识的客户端 beta 原样追加在末尾，保证新版客户端的功能不被吃掉。

use serde_json::Value;

use super::profile::ClaudeCodeTransportIdentityProfile;
use aether_ai_formats::ApiOperation;

pub const BETA_CLAUDE_CODE: &str = "claude-code-20250219";
pub const BETA_OAUTH: &str = "oauth-2025-04-20";
pub const BETA_CONTEXT_1M: &str = "context-1m-2025-08-07";
pub const BETA_INTERLEAVED_THINKING: &str = "interleaved-thinking-2025-05-14";
pub const BETA_PROMPT_CACHING_SCOPE: &str = "prompt-caching-scope-2026-01-05";
pub const BETA_EFFORT: &str = "effort-2025-11-24";
pub const BETA_CONTEXT_MANAGEMENT: &str = "context-management-2025-06-27";
pub const BETA_EXTENDED_CACHE_TTL: &str = "extended-cache-ttl-2025-04-11";
pub const BETA_TOKEN_COUNTING: &str = "token-counting-2024-11-01";
pub const BETA_FAST_MODE: &str = "fast-mode-2026-02-01";
pub const BETA_STRUCTURED_OUTPUTS: &str = "structured-outputs-2025-12-15";
pub const BETA_ADVANCED_TOOL_USE: &str = "advanced-tool-use-2025-11-20";
pub const BETA_REDACT_THINKING: &str = "redact-thinking-2026-02-12";
pub const BETA_THINKING_DISPLAY_UPDATES: &str = "thinking-display-updates-2026-08-18";

/// 网关自己组装或门控的 beta；不在这个集合里的客户端 beta 原样透传。
const MANAGED_BETAS: &[&str] = &[
    BETA_CLAUDE_CODE,
    BETA_OAUTH,
    BETA_CONTEXT_1M,
    BETA_INTERLEAVED_THINKING,
    BETA_PROMPT_CACHING_SCOPE,
    BETA_EFFORT,
    BETA_CONTEXT_MANAGEMENT,
    BETA_EXTENDED_CACHE_TTL,
    BETA_TOKEN_COUNTING,
    BETA_FAST_MODE,
    BETA_STRUCTURED_OUTPUTS,
    BETA_ADVANCED_TOOL_USE,
    BETA_REDACT_THINKING,
    BETA_THINKING_DISPLAY_UPDATES,
];

/// beta 组装的输入。
#[derive(Debug, Clone, Copy)]
pub struct ClaudeCodeBetaContext<'a> {
    pub profile: ClaudeCodeTransportIdentityProfile,
    pub operation: Option<ApiOperation>,
    /// 上游凭据是否为 OAuth（Bearer）；决定 `oauth-2025-04-20` 与 `extended-cache-ttl`。
    pub oauth_credential: bool,
    pub is_subagent: bool,
    /// 客户端原始 `anthropic-beta`（逗号分隔，可多值）。
    pub requested: Option<&'a str>,
    /// 最终请求体（决定 thinking / speed / output_config / tools 相关 beta）。
    pub body: Option<&'a Value>,
}

/// 按条件有序组装，返回逗号连接的 header 值。
pub fn assemble_claude_code_beta_header(ctx: ClaudeCodeBetaContext<'_>) -> String {
    let requested = ctx
        .requested
        .unwrap_or_default()
        .split(',')
        .map(str::trim)
        .filter(|token| !token.is_empty())
        .map(ToOwned::to_owned)
        .collect::<Vec<_>>();
    let wants = |beta: &str| {
        requested
            .iter()
            .any(|token| token.eq_ignore_ascii_case(beta))
    };
    let dropped = ctx.profile.dropped_beta_tokens();
    let is_dropped = |beta: &str| dropped.iter().any(|d| beta.eq_ignore_ascii_case(d));

    let body = ctx.body;
    let count_tokens = ctx.operation == Some(ApiOperation::ClaudeCountTokens);
    let thinking_type = body
        .and_then(|b| b.get("thinking"))
        .and_then(|t| t.get("type"))
        .and_then(Value::as_str)
        .map(|value| value.trim().to_ascii_lowercase())
        .unwrap_or_default();
    let thinking_active = matches!(thinking_type.as_str(), "enabled" | "adaptive" | "auto");
    let model = body
        .and_then(|b| b.get("model"))
        .and_then(Value::as_str)
        .map(|value| value.trim().to_ascii_lowercase())
        .unwrap_or_default();
    let is_haiku = model.contains("haiku");
    let speed_fast = body
        .and_then(|b| b.get("speed"))
        .and_then(Value::as_str)
        .is_some_and(|value| value.trim().eq_ignore_ascii_case("fast"));
    let structured_output = body
        .and_then(|b| b.get("output_config"))
        .and_then(|config| config.get("format"))
        .is_some();
    let advanced_tool_use = body.is_some_and(body_uses_advanced_tool_use);
    let has_1h_ttl = body.is_some_and(body_has_1h_cache_ttl);
    let thinking_display_set = body
        .and_then(|b| b.get("thinking"))
        .and_then(|t| t.get("display"))
        .and_then(Value::as_str)
        .is_some_and(|value| !value.trim().is_empty());

    let mut out: Vec<String> = Vec::with_capacity(12);
    let push = |beta: &str, out: &mut Vec<String>| {
        if is_dropped(beta)
            || out
                .iter()
                .any(|existing| existing.eq_ignore_ascii_case(beta))
        {
            return;
        }
        out.push(beta.to_string());
    };

    push(BETA_CLAUDE_CODE, &mut out);
    if ctx.oauth_credential {
        push(BETA_OAUTH, &mut out);
    }
    if wants(BETA_CONTEXT_1M) {
        push(BETA_CONTEXT_1M, &mut out);
    }
    push(BETA_INTERLEAVED_THINKING, &mut out);
    if wants(BETA_REDACT_THINKING) && !thinking_display_set {
        push(BETA_REDACT_THINKING, &mut out);
    }
    if !count_tokens {
        push(BETA_PROMPT_CACHING_SCOPE, &mut out);
        if thinking_active && !is_haiku {
            push(BETA_EFFORT, &mut out);
        }
    }
    push(BETA_CONTEXT_MANAGEMENT, &mut out);
    if !count_tokens {
        if wants(BETA_ADVANCED_TOOL_USE) || advanced_tool_use {
            push(BETA_ADVANCED_TOOL_USE, &mut out);
        }
        if wants(BETA_STRUCTURED_OUTPUTS) && structured_output {
            push(BETA_STRUCTURED_OUTPUTS, &mut out);
        }
        if thinking_active && thinking_display_set && wants(BETA_THINKING_DISPLAY_UPDATES) {
            push(BETA_THINKING_DISPLAY_UPDATES, &mut out);
        }
        if wants(BETA_FAST_MODE) && speed_fast {
            push(BETA_FAST_MODE, &mut out);
        }
        let include_extended = (ctx.oauth_credential && !ctx.is_subagent)
            || wants(BETA_EXTENDED_CACHE_TTL)
            || has_1h_ttl;
        if include_extended {
            push(BETA_EXTENDED_CACHE_TTL, &mut out);
        }
    }
    if count_tokens {
        push(BETA_TOKEN_COUNTING, &mut out);
    }
    // 不认识的客户端 beta 原样追加，避免吃掉新版客户端的功能。
    for token in &requested {
        if MANAGED_BETAS
            .iter()
            .any(|managed| token.eq_ignore_ascii_case(managed))
        {
            continue;
        }
        push(token, &mut out);
    }
    out.join(",")
}

fn body_uses_advanced_tool_use(body: &Value) -> bool {
    let Some(tools) = body.get("tools").and_then(Value::as_array) else {
        return false;
    };
    tools.iter().any(|tool| {
        let tool_type = tool
            .get("type")
            .and_then(Value::as_str)
            .map(|value| value.trim().to_ascii_lowercase())
            .unwrap_or_default();
        tool_type.starts_with("tool_search_tool_")
            || tool
                .get("defer_loading")
                .and_then(Value::as_bool)
                .unwrap_or(false)
            || tool.get("input_examples").is_some()
            || tool.get("allowed_callers").is_some()
    })
}

pub(super) fn body_has_1h_cache_ttl(body: &Value) -> bool {
    let mut found = false;
    super::cache_control::for_each_cache_control_block(body, |_, block| {
        if block
            .get("cache_control")
            .and_then(|cc| cc.get("ttl"))
            .and_then(Value::as_str)
            .is_some_and(|ttl| ttl == "1h")
        {
            found = true;
        }
    });
    found
}

/// 请求是否来自子代理：显式头、`metadata.user_id.parent_session_id`、或计费头里的
/// `cc_is_subagent=true`。永不检查用户消息正文。
pub fn claude_code_request_is_subagent(
    headers: Option<&http::HeaderMap>,
    body: Option<&Value>,
) -> bool {
    if let Some(headers) = headers {
        for name in ["x-claude-code-agent-id", "x-claude-code-parent-agent-id"] {
            if headers
                .get(name)
                .and_then(|value| value.to_str().ok())
                .is_some_and(|value| !value.trim().is_empty())
            {
                return true;
            }
        }
    }
    let Some(body) = body else {
        return false;
    };
    if body
        .get("metadata")
        .and_then(|metadata| metadata.get("user_id"))
        .and_then(Value::as_str)
        .is_some_and(|user_id| user_id.contains("\"parent_session_id\""))
    {
        return true;
    }
    match body.get("system") {
        Some(Value::Array(blocks)) => blocks
            .first()
            .and_then(|block| block.get("text"))
            .and_then(Value::as_str)
            .is_some_and(|text| text.contains("cc_is_subagent=true")),
        Some(Value::String(text)) => text.contains("cc_is_subagent=true"),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::claude_code::current_claude_code_transport_identity_profile;
    use serde_json::json;

    fn ctx<'a>(body: &'a Value, requested: Option<&'a str>) -> ClaudeCodeBetaContext<'a> {
        ClaudeCodeBetaContext {
            profile: *current_claude_code_transport_identity_profile(),
            operation: None,
            oauth_credential: true,
            is_subagent: false,
            requested,
            body: Some(body),
        }
    }

    #[test]
    fn messages_with_thinking_on_oauth_credential_use_full_ordered_list() {
        let body = json!({"model":"claude-opus-4-6","thinking":{"type":"adaptive"},"messages":[]});
        assert_eq!(
            assemble_claude_code_beta_header(ctx(&body, Some("context-1m-2025-08-07,custom-beta"))),
            "claude-code-20250219,oauth-2025-04-20,context-1m-2025-08-07,interleaved-thinking-2025-05-14,prompt-caching-scope-2026-01-05,effort-2025-11-24,context-management-2025-06-27,extended-cache-ttl-2025-04-11,custom-beta"
        );
    }

    #[test]
    fn api_key_credential_and_haiku_drop_oauth_effort_and_extended_ttl() {
        let body = json!({"model":"claude-haiku-4-5-20251001","thinking":{"type":"enabled","budget_tokens":1024},"messages":[]});
        let mut context = ctx(&body, None);
        context.oauth_credential = false;
        assert_eq!(
            assemble_claude_code_beta_header(context),
            "claude-code-20250219,interleaved-thinking-2025-05-14,prompt-caching-scope-2026-01-05,context-management-2025-06-27"
        );
    }

    #[test]
    fn subagent_without_1h_ttl_omits_extended_cache_ttl_but_explicit_request_keeps_it() {
        let body = json!({"model":"claude-opus-4-6","messages":[]});
        let mut context = ctx(&body, None);
        context.is_subagent = true;
        assert!(!assemble_claude_code_beta_header(context).contains(BETA_EXTENDED_CACHE_TTL));
        let with_ttl = json!({"model":"claude-opus-4-6","system":[{"type":"text","text":"s","cache_control":{"type":"ephemeral","ttl":"1h"}}],"messages":[]});
        let mut context = ctx(&with_ttl, None);
        context.is_subagent = true;
        assert!(assemble_claude_code_beta_header(context).contains(BETA_EXTENDED_CACHE_TTL));
    }

    #[test]
    fn count_tokens_uses_fixed_profile_with_token_counting() {
        let body = json!({"model":"claude-opus-4-6","thinking":{"type":"enabled","budget_tokens":2048},"messages":[]});
        let mut context = ctx(&body, Some("fast-mode-2026-02-01"));
        context.operation = Some(ApiOperation::ClaudeCountTokens);
        assert_eq!(
            assemble_claude_code_beta_header(context),
            "claude-code-20250219,oauth-2025-04-20,interleaved-thinking-2025-05-14,context-management-2025-06-27,token-counting-2024-11-01"
        );
    }

    #[test]
    fn invalid_requested_betas_are_dropped_unless_the_body_supports_them() {
        let body = json!({"model":"claude-opus-4-6","messages":[]});
        let header = assemble_claude_code_beta_header(ctx(
            &body,
            Some("fast-mode-2026-02-01,structured-outputs-2025-12-15,advanced-tool-use-2025-11-20"),
        ));
        assert!(!header.contains(BETA_FAST_MODE));
        assert!(!header.contains(BETA_STRUCTURED_OUTPUTS));
        assert!(
            header.contains(BETA_ADVANCED_TOOL_USE),
            "explicit advanced-tool-use is honoured"
        );

        let fast = json!({"model":"claude-opus-4-6","speed":"fast","output_config":{"format":{"type":"json_schema"}},"messages":[]});
        let header = assemble_claude_code_beta_header(ctx(
            &fast,
            Some("fast-mode-2026-02-01,structured-outputs-2025-12-15"),
        ));
        assert!(header.ends_with(
            "structured-outputs-2025-12-15,fast-mode-2026-02-01,extended-cache-ttl-2025-04-11"
        ));
    }

    #[test]
    fn subagent_detection_reads_headers_user_id_and_billing_only() {
        let mut headers = http::HeaderMap::new();
        headers.insert("x-claude-code-agent-id", "agent-1".parse().unwrap());
        assert!(claude_code_request_is_subagent(Some(&headers), None));
        let body =
            json!({"metadata":{"user_id":"{\"session_id\":\"s\",\"parent_session_id\":\"p\"}"}});
        assert!(claude_code_request_is_subagent(None, Some(&body)));
        let billing = json!({"system":[{"type":"text","text":"x-anthropic-billing-header: cc_version=2.1.161.abc; cc_entrypoint=cli; cc_is_subagent=true;"}]});
        assert!(claude_code_request_is_subagent(None, Some(&billing)));
        let user_text = json!({"messages":[{"role":"user","content":"cc_is_subagent=true"}]});
        assert!(!claude_code_request_is_subagent(None, Some(&user_text)));
    }
}
