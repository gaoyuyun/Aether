//! `cache_control` 治理（计划 2.5）。
//!
//! - 第三方请求没有任何断点时按原生策略补：tools 最后一个可缓存工具（仅当没有 system）、
//!   system 最后一块、最后一条 user/assistant 消息的最后一个内容块；
//! - 断点总数不超过 4（Anthropic 上限）：先删 system 里靠前的、再删 tools 里靠前的、最后删消息里的；
//! - OAuth 凭据把没写 ttl 的断点升到 `ttl: 1h`（与 `extended-cache-ttl` beta 配对），
//!   子代理与 API Key 凭据保持 5m；
//! - 在 tools → system → messages 的评估顺序上，1h 断点不能出现在 5m 断点之后
//!   （`prompt-caching-scope` 的排序约束），违反时把后面的 1h 降回默认。

use serde_json::{Map, Value};

pub const CLAUDE_CODE_MAX_CACHE_CONTROL_BREAKPOINTS: usize = 4;
pub const CLAUDE_CODE_CACHE_TTL_1H: &str = "1h";

/// 治理结果摘要，写入 report_context。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize)]
pub struct ClaudeCodeCacheControlSummary {
    pub injected: usize,
    pub removed_over_limit: usize,
    pub upgraded_to_1h: usize,
    pub stripped_ttl: usize,
    pub downgraded_out_of_order: usize,
    pub breakpoints: usize,
}

impl ClaudeCodeCacheControlSummary {
    pub fn changed(&self) -> bool {
        self.injected > 0
            || self.removed_over_limit > 0
            || self.upgraded_to_1h > 0
            || self.stripped_ttl > 0
            || self.downgraded_out_of_order > 0
    }
}

#[derive(Debug, Clone, Copy)]
pub struct ClaudeCodeCacheControlPolicy {
    /// 没有任何断点时是否按原生策略补断点。
    pub ensure_breakpoints: bool,
    /// 是否把无 ttl 的断点升到 1h（OAuth 凭据且非子代理）。
    pub upgrade_to_1h: bool,
    /// 是否剥掉所有 ttl（子代理、探针）。
    pub strip_ttl: bool,
}

pub fn apply_claude_code_cache_control_policy(
    body: &mut Value,
    policy: ClaudeCodeCacheControlPolicy,
) -> ClaudeCodeCacheControlSummary {
    let mut summary = ClaudeCodeCacheControlSummary::default();
    let Some(object) = body.as_object_mut() else {
        return summary;
    };
    if policy.ensure_breakpoints && count_cache_controls(object) == 0 {
        summary.injected = ensure_cache_control(object);
    }
    summary.removed_over_limit =
        enforce_cache_control_limit(object, CLAUDE_CODE_MAX_CACHE_CONTROL_BREAKPOINTS);
    if policy.strip_ttl {
        summary.stripped_ttl = strip_cache_control_ttl(object);
    } else if policy.upgrade_to_1h {
        summary.upgraded_to_1h = upgrade_cache_control_ttl(object, CLAUDE_CODE_CACHE_TTL_1H);
    }
    summary.downgraded_out_of_order = normalize_cache_control_ttl_order(object);
    summary.breakpoints = count_cache_controls(object);
    summary
}

/// 遍历所有能承载 `cache_control` 的块，按 tools → system → messages 的评估顺序。
pub(super) fn for_each_cache_control_block<F>(body: &Value, mut visit: F)
where
    F: FnMut(String, &Value),
{
    if let Some(tools) = body.get("tools").and_then(Value::as_array) {
        for (index, tool) in tools.iter().enumerate() {
            visit(format!("tools[{index}]"), tool);
        }
    }
    if let Some(system) = body.get("system").and_then(Value::as_array) {
        for (index, block) in system.iter().enumerate() {
            visit(format!("system[{index}]"), block);
        }
    }
    if let Some(messages) = body.get("messages").and_then(Value::as_array) {
        for (message_index, message) in messages.iter().enumerate() {
            let Some(content) = message.get("content").and_then(Value::as_array) else {
                continue;
            };
            for (block_index, block) in content.iter().enumerate() {
                visit(
                    format!("messages[{message_index}].content[{block_index}]"),
                    block,
                );
            }
        }
    }
}

fn for_each_cache_control_block_mut<F>(object: &mut Map<String, Value>, mut visit: F)
where
    F: FnMut(&mut Map<String, Value>),
{
    if let Some(tools) = object.get_mut("tools").and_then(Value::as_array_mut) {
        for tool in tools.iter_mut().filter_map(Value::as_object_mut) {
            visit(tool);
        }
    }
    if let Some(system) = object.get_mut("system").and_then(Value::as_array_mut) {
        for block in system.iter_mut().filter_map(Value::as_object_mut) {
            visit(block);
        }
    }
    if let Some(messages) = object.get_mut("messages").and_then(Value::as_array_mut) {
        for message in messages.iter_mut() {
            let Some(content) = message.get_mut("content").and_then(Value::as_array_mut) else {
                continue;
            };
            for block in content.iter_mut().filter_map(Value::as_object_mut) {
                visit(block);
            }
        }
    }
}

pub fn count_cache_controls(object: &Map<String, Value>) -> usize {
    let mut count = 0;
    for_each_cache_control_block(&Value::Object(object.clone()), |_, block| {
        if block.get("cache_control").is_some() {
            count += 1;
        }
    });
    count
}

fn ephemeral() -> Value {
    serde_json::json!({"type": "ephemeral"})
}

fn payload_has_cacheable_system(object: &Map<String, Value>) -> bool {
    match object.get("system") {
        Some(Value::Array(blocks)) => !blocks.is_empty(),
        Some(Value::String(text)) => !text.trim().is_empty(),
        _ => false,
    }
}

fn ensure_cache_control(object: &mut Map<String, Value>) -> usize {
    let mut injected = 0;
    if !payload_has_cacheable_system(object) {
        injected += inject_tools_cache_control(object);
    }
    injected += inject_system_cache_control(object);
    injected += inject_messages_cache_control(object);
    injected
}

fn inject_tools_cache_control(object: &mut Map<String, Value>) -> usize {
    let Some(tools) = object.get_mut("tools").and_then(Value::as_array_mut) else {
        return 0;
    };
    let mut last_eligible = None;
    for (index, tool) in tools.iter().enumerate() {
        if tool.get("cache_control").is_some() {
            return 0;
        }
        if !tool
            .get("defer_loading")
            .and_then(Value::as_bool)
            .unwrap_or(false)
        {
            last_eligible = Some(index);
        }
    }
    let Some(index) = last_eligible else {
        return 0;
    };
    let Some(tool) = tools.get_mut(index).and_then(Value::as_object_mut) else {
        return 0;
    };
    tool.insert("cache_control".to_string(), ephemeral());
    1
}

fn inject_system_cache_control(object: &mut Map<String, Value>) -> usize {
    match object.get_mut("system") {
        Some(Value::Array(blocks)) => {
            if blocks.is_empty()
                || blocks
                    .iter()
                    .any(|block| block.get("cache_control").is_some())
            {
                return 0;
            }
            let Some(last) = blocks.last_mut().and_then(Value::as_object_mut) else {
                return 0;
            };
            last.insert("cache_control".to_string(), ephemeral());
            1
        }
        Some(Value::String(text)) => {
            if text.trim().is_empty() {
                return 0;
            }
            let text = text.clone();
            object.insert(
                "system".to_string(),
                Value::Array(vec![serde_json::json!({
                    "type": "text",
                    "text": text,
                    "cache_control": ephemeral()
                })]),
            );
            1
        }
        _ => 0,
    }
}

fn inject_messages_cache_control(object: &mut Map<String, Value>) -> usize {
    let Some(messages) = object.get_mut("messages").and_then(Value::as_array_mut) else {
        return 0;
    };
    let last_eligible = messages
        .iter()
        .enumerate()
        .filter(|(_, message)| {
            matches!(
                message.get("role").and_then(Value::as_str),
                Some("user") | Some("assistant")
            )
        })
        .filter(|(_, message)| message_eligible_for_rolling_cache(message))
        .map(|(index, _)| index)
        .last();
    let Some(index) = last_eligible else {
        return 0;
    };
    let Some(message) = messages.get_mut(index).and_then(Value::as_object_mut) else {
        return 0;
    };
    match message.get_mut("content") {
        Some(Value::Array(content)) => {
            if content
                .iter()
                .any(|block| block.get("cache_control").is_some())
            {
                return 0;
            }
            let Some(last) = content.last_mut().and_then(Value::as_object_mut) else {
                return 0;
            };
            last.insert("cache_control".to_string(), ephemeral());
            1
        }
        Some(Value::String(text)) => {
            if text.trim().is_empty() {
                return 0;
            }
            let text = text.clone();
            message.insert(
                "content".to_string(),
                Value::Array(vec![serde_json::json!({
                    "type": "text",
                    "text": text,
                    "cache_control": ephemeral()
                })]),
            );
            1
        }
        _ => 0,
    }
}

fn message_eligible_for_rolling_cache(message: &Value) -> bool {
    match message.get("content") {
        Some(Value::String(text)) => !text.trim().is_empty(),
        Some(Value::Array(content)) => content.last().is_some_and(|block| {
            let block_type = block
                .get("type")
                .and_then(Value::as_str)
                .unwrap_or_default();
            !matches!(block_type, "thinking" | "redacted_thinking")
        }),
        _ => false,
    }
}

fn enforce_cache_control_limit(object: &mut Map<String, Value>, max_blocks: usize) -> usize {
    let total = count_cache_controls(object);
    if total <= max_blocks {
        return 0;
    }
    let mut excess = total - max_blocks;
    let mut removed = 0;

    // system：删靠前的，保留最后一个。
    if let Some(system) = object.get_mut("system").and_then(Value::as_array_mut) {
        let last_with = system
            .iter()
            .rposition(|block| block.get("cache_control").is_some());
        if let Some(last_index) = last_with {
            for (index, block) in system.iter_mut().enumerate() {
                if excess == 0 {
                    break;
                }
                if index == last_index {
                    continue;
                }
                if let Some(block) = block.as_object_mut() {
                    if block.remove("cache_control").is_some() {
                        excess -= 1;
                        removed += 1;
                    }
                }
            }
        }
    }
    if excess == 0 {
        return removed;
    }
    if let Some(tools) = object.get_mut("tools").and_then(Value::as_array_mut) {
        let last_with = tools
            .iter()
            .rposition(|tool| tool.get("cache_control").is_some());
        if let Some(last_index) = last_with {
            for (index, tool) in tools.iter_mut().enumerate() {
                if excess == 0 {
                    break;
                }
                if index == last_index {
                    continue;
                }
                if let Some(tool) = tool.as_object_mut() {
                    if tool.remove("cache_control").is_some() {
                        excess -= 1;
                        removed += 1;
                    }
                }
            }
        }
    }
    if excess == 0 {
        return removed;
    }
    if let Some(messages) = object.get_mut("messages").and_then(Value::as_array_mut) {
        for message in messages.iter_mut() {
            if excess == 0 {
                break;
            }
            let Some(content) = message.get_mut("content").and_then(Value::as_array_mut) else {
                continue;
            };
            for block in content.iter_mut().filter_map(Value::as_object_mut) {
                if excess == 0 {
                    break;
                }
                if block.remove("cache_control").is_some() {
                    excess -= 1;
                    removed += 1;
                }
            }
        }
    }
    removed
}

fn upgrade_cache_control_ttl(object: &mut Map<String, Value>, ttl: &str) -> usize {
    let mut upgraded = 0;
    for_each_cache_control_block_mut(object, |block| {
        let Some(cache_control) = block
            .get_mut("cache_control")
            .and_then(Value::as_object_mut)
        else {
            return;
        };
        if cache_control.contains_key("ttl") {
            return;
        }
        let Some(block_type) = cache_control.get("type").and_then(Value::as_str) else {
            return;
        };
        // 重建对象保持原生 {type, ttl, scope} 的键顺序。
        let mut rebuilt = Map::new();
        rebuilt.insert("type".to_string(), Value::String(block_type.to_string()));
        rebuilt.insert("ttl".to_string(), Value::String(ttl.to_string()));
        if let Some(scope) = cache_control.get("scope").cloned() {
            rebuilt.insert("scope".to_string(), scope);
        }
        *cache_control = rebuilt;
        upgraded += 1;
    });
    upgraded
}

fn strip_cache_control_ttl(object: &mut Map<String, Value>) -> usize {
    let mut stripped = 0;
    for_each_cache_control_block_mut(object, |block| {
        if let Some(cache_control) = block
            .get_mut("cache_control")
            .and_then(Value::as_object_mut)
        {
            if cache_control.remove("ttl").is_some() {
                stripped += 1;
            }
        }
    });
    stripped
}

fn normalize_cache_control_ttl_order(object: &mut Map<String, Value>) -> usize {
    let mut seen_5m = false;
    let mut downgraded = 0;
    for_each_cache_control_block_mut(object, |block| {
        let Some(cache_control) = block.get_mut("cache_control") else {
            return;
        };
        let Some(cache_control) = cache_control.as_object_mut() else {
            seen_5m = true;
            return;
        };
        let is_1h = cache_control
            .get("ttl")
            .and_then(Value::as_str)
            .is_some_and(|ttl| ttl == CLAUDE_CODE_CACHE_TTL_1H);
        if !is_1h {
            seen_5m = true;
            return;
        }
        if seen_5m {
            cache_control.remove("ttl");
            downgraded += 1;
        }
    });
    downgraded
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn oauth_policy() -> ClaudeCodeCacheControlPolicy {
        ClaudeCodeCacheControlPolicy {
            ensure_breakpoints: true,
            upgrade_to_1h: true,
            strip_ttl: false,
        }
    }

    #[test]
    fn third_party_body_without_breakpoints_gets_native_placement_and_1h_ttl() {
        let mut body = json!({
            "model":"claude-opus-4-6",
            "system":"You are helpful.",
            "tools":[{"name":"a","input_schema":{"type":"object"}}],
            "messages":[
                {"role":"user","content":"hi"},
                {"role":"assistant","content":[{"type":"thinking","thinking":"t","signature":"s"}]},
                {"role":"user","content":[{"type":"text","text":"again"}]}
            ]
        });
        let summary = apply_claude_code_cache_control_policy(&mut body, oauth_policy());
        assert_eq!(summary.injected, 2, "system + last user message");
        assert_eq!(summary.upgraded_to_1h, 2);
        assert_eq!(summary.breakpoints, 2);
        assert_eq!(
            body["system"],
            json!([{"type":"text","text":"You are helpful.","cache_control":{"type":"ephemeral","ttl":"1h"}}])
        );
        assert!(
            body["tools"][0].get("cache_control").is_none(),
            "tools skipped when system exists"
        );
        assert_eq!(
            body["messages"][2]["content"][0]["cache_control"],
            json!({"type":"ephemeral","ttl":"1h"})
        );
        assert!(body["messages"][1]["content"][0]
            .get("cache_control")
            .is_none());
    }

    #[test]
    fn tools_receive_breakpoint_only_without_system() {
        let mut body = json!({
            "tools":[{"name":"a"},{"name":"b","defer_loading":true}],
            "messages":[{"role":"user","content":"hi"}]
        });
        let summary = apply_claude_code_cache_control_policy(
            &mut body,
            ClaudeCodeCacheControlPolicy {
                ensure_breakpoints: true,
                upgrade_to_1h: false,
                strip_ttl: false,
            },
        );
        assert_eq!(summary.injected, 2);
        assert_eq!(
            body["tools"][0]["cache_control"],
            json!({"type":"ephemeral"})
        );
        assert!(body["tools"][1].get("cache_control").is_none());
        assert_eq!(
            body["messages"][0]["content"][0]["cache_control"],
            json!({"type":"ephemeral"})
        );
    }

    #[test]
    fn breakpoints_are_capped_at_four_dropping_earlier_system_then_tools_then_messages() {
        let mut body = json!({
            "tools":[{"name":"a","cache_control":{"type":"ephemeral"}},{"name":"b","cache_control":{"type":"ephemeral"}}],
            "system":[{"type":"text","text":"1","cache_control":{"type":"ephemeral"}},{"type":"text","text":"2","cache_control":{"type":"ephemeral"}}],
            "messages":[{"role":"user","content":[{"type":"text","text":"a","cache_control":{"type":"ephemeral"}},{"type":"text","text":"b","cache_control":{"type":"ephemeral"}}]}]
        });
        let summary = apply_claude_code_cache_control_policy(
            &mut body,
            ClaudeCodeCacheControlPolicy {
                ensure_breakpoints: true,
                upgrade_to_1h: false,
                strip_ttl: false,
            },
        );
        assert_eq!(summary.removed_over_limit, 2);
        assert_eq!(summary.breakpoints, 4);
        assert!(body["system"][0].get("cache_control").is_none());
        assert!(body["system"][1].get("cache_control").is_some());
        assert!(body["tools"][0].get("cache_control").is_none());
        assert!(body["tools"][1].get("cache_control").is_some());
        assert!(body["messages"][0]["content"][0]
            .get("cache_control")
            .is_some());
    }

    #[test]
    fn explicit_ttl_survives_and_out_of_order_1h_is_downgraded() {
        let mut body = json!({
            "system":[{"type":"text","text":"s","cache_control":{"type":"ephemeral","ttl":"5m"}}],
            "messages":[{"role":"user","content":[{"type":"text","text":"u","cache_control":{"type":"ephemeral","ttl":"1h","scope":"global"}}]}]
        });
        let summary = apply_claude_code_cache_control_policy(&mut body, oauth_policy());
        assert_eq!(summary.upgraded_to_1h, 0);
        assert_eq!(summary.downgraded_out_of_order, 1);
        assert_eq!(body["system"][0]["cache_control"]["ttl"], "5m");
        assert_eq!(
            body["messages"][0]["content"][0]["cache_control"],
            json!({"type":"ephemeral","scope":"global"})
        );
    }

    #[test]
    fn subagent_policy_strips_ttl() {
        let mut body = json!({
            "system":[{"type":"text","text":"s","cache_control":{"type":"ephemeral","ttl":"1h"}}],
            "messages":[]
        });
        let summary = apply_claude_code_cache_control_policy(
            &mut body,
            ClaudeCodeCacheControlPolicy {
                ensure_breakpoints: true,
                upgrade_to_1h: false,
                strip_ttl: true,
            },
        );
        assert_eq!(summary.stripped_ttl, 1);
        assert_eq!(
            body["system"][0]["cache_control"],
            json!({"type":"ephemeral"})
        );
    }

    #[test]
    fn native_owned_placement_is_left_alone_when_ensure_is_off() {
        let original = json!({"system":"x","messages":[{"role":"user","content":"hi"}]});
        let mut body = original.clone();
        let summary = apply_claude_code_cache_control_policy(
            &mut body,
            ClaudeCodeCacheControlPolicy {
                ensure_breakpoints: false,
                upgrade_to_1h: true,
                strip_ttl: false,
            },
        );
        assert!(!summary.changed());
        assert_eq!(body, original);
    }
}
