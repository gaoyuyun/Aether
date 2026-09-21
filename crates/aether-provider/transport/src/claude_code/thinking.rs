//! thinking 归一化（计划 2.5）。
//!
//! - `tool_choice.type ∈ {any, tool}` 时 Anthropic 不允许 thinking：整块删除，
//!   连同 `output_config.effort`；
//! - `budget_tokens` 必须 `< max_tokens` 且 `≥ 模型最小值`：超过就压到 `max_tokens - 1`，
//!   压完低于最小值则保持原样交给上游报错（不能偷偷改语义）；请求没带 `max_tokens`
//!   时用能力表的默认值补上；
//! - 能力表说模型用 adaptive 的，`enabled + budget_tokens` 改成 `adaptive`（budget 转成
//!   effort 档位）；能力表说不支持 adaptive 的，`adaptive` 改成 `enabled + budget_tokens`
//!   （effort 转成 budget）；不支持 thinking 的模型整块删除。

use aether_ai_formats::{claude_thinking_capability, ClaudeThinkingMode};
use serde_json::{Map, Value};

#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize)]
pub struct ClaudeCodeThinkingSummary {
    pub disabled_for_forced_tool_choice: bool,
    pub removed_unsupported: bool,
    pub converted_to_adaptive: bool,
    pub converted_to_manual: bool,
    pub budget_clamped: bool,
    pub max_tokens_defaulted: bool,
}

impl ClaudeCodeThinkingSummary {
    pub fn changed(&self) -> bool {
        self.disabled_for_forced_tool_choice
            || self.removed_unsupported
            || self.converted_to_adaptive
            || self.converted_to_manual
            || self.budget_clamped
            || self.max_tokens_defaulted
    }
}

pub fn normalize_claude_code_thinking(body: &mut Value) -> ClaudeCodeThinkingSummary {
    let mut summary = ClaudeCodeThinkingSummary::default();
    let Some(object) = body.as_object_mut() else {
        return summary;
    };
    let tool_choice_forced = object
        .get("tool_choice")
        .and_then(|choice| choice.get("type"))
        .and_then(Value::as_str)
        .is_some_and(|value| matches!(value.trim(), "any" | "tool"));
    if tool_choice_forced && object.contains_key("thinking") {
        object.remove("thinking");
        remove_output_effort(object);
        summary.disabled_for_forced_tool_choice = true;
        return summary;
    }
    let Some(thinking_type) = object
        .get("thinking")
        .and_then(|thinking| thinking.get("type"))
        .and_then(Value::as_str)
        .map(|value| value.trim().to_ascii_lowercase())
    else {
        return summary;
    };
    if thinking_type == "disabled" {
        return summary;
    }
    let model = object
        .get("model")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let capability = claude_thinking_capability(&model);
    match capability.mode {
        ClaudeThinkingMode::Unsupported => {
            object.remove("thinking");
            remove_output_effort(object);
            summary.removed_unsupported = true;
        }
        ClaudeThinkingMode::Adaptive => {
            if thinking_type == "enabled" {
                let budget = object
                    .get("thinking")
                    .and_then(|thinking| thinking.get("budget_tokens"))
                    .and_then(Value::as_u64);
                let effort =
                    budget.map(|budget| effort_for_budget(budget, capability.supports_max_effort));
                if let Some(thinking) = object.get_mut("thinking").and_then(Value::as_object_mut) {
                    thinking.insert("type".to_string(), Value::String("adaptive".to_string()));
                    thinking.remove("budget_tokens");
                }
                if let Some(effort) = effort {
                    let has_effort = object
                        .get("output_config")
                        .and_then(|config| config.get("effort"))
                        .is_some();
                    if !has_effort {
                        let config = object
                            .entry("output_config".to_string())
                            .or_insert_with(|| Value::Object(Map::new()));
                        if let Some(config) = config.as_object_mut() {
                            config.insert("effort".to_string(), Value::String(effort.to_string()));
                        }
                    }
                }
                summary.converted_to_adaptive = true;
            } else if !capability.supports_max_effort {
                if let Some(config) = object
                    .get_mut("output_config")
                    .and_then(Value::as_object_mut)
                {
                    if config
                        .get("effort")
                        .and_then(Value::as_str)
                        .is_some_and(|effort| effort.eq_ignore_ascii_case("max"))
                    {
                        config.insert("effort".to_string(), Value::String("high".to_string()));
                        summary.budget_clamped = true;
                    }
                }
            }
        }
        ClaudeThinkingMode::ManualBudget => {
            if thinking_type == "adaptive" || thinking_type == "auto" {
                let effort = object
                    .get("output_config")
                    .and_then(|config| config.get("effort"))
                    .and_then(Value::as_str)
                    .map(|value| value.trim().to_ascii_lowercase());
                let budget = budget_for_effort(effort.as_deref());
                if let Some(thinking) = object.get_mut("thinking").and_then(Value::as_object_mut) {
                    thinking.insert("type".to_string(), Value::String("enabled".to_string()));
                    thinking.insert("budget_tokens".to_string(), Value::from(budget));
                }
                remove_output_effort(object);
                summary.converted_to_manual = true;
            }
            let budget = object
                .get("thinking")
                .and_then(|thinking| thinking.get("budget_tokens"))
                .and_then(Value::as_u64);
            if let Some(budget) = budget {
                let max_tokens = object.get("max_tokens").and_then(Value::as_u64);
                let effective_max = match max_tokens {
                    Some(value) if value > 0 => value,
                    _ => {
                        object.insert(
                            "max_tokens".to_string(),
                            Value::from(capability.default_max_tokens),
                        );
                        summary.max_tokens_defaulted = true;
                        capability.default_max_tokens
                    }
                };
                if budget >= effective_max {
                    let adjusted = effective_max.saturating_sub(1);
                    if adjusted >= capability.min_budget_tokens {
                        if let Some(thinking) =
                            object.get_mut("thinking").and_then(Value::as_object_mut)
                        {
                            thinking.insert("budget_tokens".to_string(), Value::from(adjusted));
                        }
                        summary.budget_clamped = true;
                    }
                }
            }
        }
    }
    summary
}

fn remove_output_effort(object: &mut Map<String, Value>) {
    let empty = if let Some(config) = object
        .get_mut("output_config")
        .and_then(Value::as_object_mut)
    {
        config.remove("effort");
        config.is_empty()
    } else {
        false
    };
    if empty {
        object.remove("output_config");
    }
}

fn effort_for_budget(budget: u64, supports_max: bool) -> &'static str {
    match budget {
        0..=2_048 => "low",
        2_049..=8_192 => "medium",
        8_193..=32_000 => "high",
        _ if supports_max => "max",
        _ => "high",
    }
}

fn budget_for_effort(effort: Option<&str>) -> u64 {
    match effort {
        Some("low") | Some("minimal") | Some("none") => 1_280,
        Some("medium") => 4_096,
        Some("high") => 16_000,
        Some("max") | Some("xhigh") => 32_000,
        _ => 8_192,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn forced_tool_choice_strips_thinking_and_effort() {
        let mut body = json!({
            "model":"claude-opus-4-6",
            "tool_choice":{"type":"tool","name":"x"},
            "thinking":{"type":"adaptive"},
            "output_config":{"effort":"high"}
        });
        let summary = normalize_claude_code_thinking(&mut body);
        assert!(summary.disabled_for_forced_tool_choice);
        assert!(body.get("thinking").is_none());
        assert!(body.get("output_config").is_none());
        let mut auto = json!({"model":"claude-opus-4-6","tool_choice":{"type":"auto"},"thinking":{"type":"adaptive"}});
        assert!(!normalize_claude_code_thinking(&mut auto).changed());
    }

    #[test]
    fn budget_is_clamped_below_max_tokens_and_max_tokens_defaulted() {
        let mut body = json!({"model":"claude-sonnet-4-5-20250929","max_tokens":4000,"thinking":{"type":"enabled","budget_tokens":4000}});
        let summary = normalize_claude_code_thinking(&mut body);
        assert!(summary.budget_clamped);
        assert_eq!(body["thinking"]["budget_tokens"], 3999);

        let mut too_small = json!({"model":"claude-sonnet-4-5-20250929","max_tokens":1024,"thinking":{"type":"enabled","budget_tokens":2048}});
        let summary = normalize_claude_code_thinking(&mut too_small);
        assert!(
            !summary.budget_clamped,
            "leave the request alone when the clamp would violate the minimum"
        );
        assert_eq!(too_small["thinking"]["budget_tokens"], 2048);

        let mut no_max = json!({"model":"claude-haiku-4-5-20251001","thinking":{"type":"enabled","budget_tokens":2048}});
        let summary = normalize_claude_code_thinking(&mut no_max);
        assert!(summary.max_tokens_defaulted);
        assert_eq!(no_max["max_tokens"], 32_000);
    }

    #[test]
    fn adaptive_models_convert_manual_budget_to_effort_by_capability_not_name() {
        let mut body =
            json!({"model":"claude-opus-5","thinking":{"type":"enabled","budget_tokens":50000}});
        let summary = normalize_claude_code_thinking(&mut body);
        assert!(summary.converted_to_adaptive);
        assert_eq!(body["thinking"], json!({"type":"adaptive"}));
        assert_eq!(body["output_config"]["effort"], "max");

        let mut sonnet = json!({"model":"claude-sonnet-4-6","thinking":{"type":"enabled","budget_tokens":50000},"output_config":{"effort":"low"}});
        normalize_claude_code_thinking(&mut sonnet);
        assert_eq!(
            sonnet["output_config"]["effort"], "low",
            "explicit effort is kept"
        );

        let mut sonnet_max = json!({"model":"claude-sonnet-4-6","thinking":{"type":"adaptive"},"output_config":{"effort":"max"}});
        let summary = normalize_claude_code_thinking(&mut sonnet_max);
        assert!(summary.budget_clamped);
        assert_eq!(sonnet_max["output_config"]["effort"], "high");
    }

    #[test]
    fn manual_models_convert_adaptive_to_budget_and_unsupported_models_drop_thinking() {
        let mut body = json!({"model":"claude-sonnet-4-5-20250929","max_tokens":64000,"thinking":{"type":"adaptive"},"output_config":{"effort":"high","format":{"type":"json_schema"}}});
        let summary = normalize_claude_code_thinking(&mut body);
        assert!(summary.converted_to_manual);
        assert_eq!(
            body["thinking"],
            json!({"type":"enabled","budget_tokens":16000})
        );
        assert_eq!(
            body["output_config"],
            json!({"format":{"type":"json_schema"}})
        );

        let mut old = json!({"model":"claude-3-5-haiku-20241022","thinking":{"type":"enabled","budget_tokens":1024}});
        let summary = normalize_claude_code_thinking(&mut old);
        assert!(summary.removed_unsupported);
        assert!(old.get("thinking").is_none());
    }
}
