//! Claude 模型 thinking 能力表。
//!
//! `claude_model_uses_adaptive_effort` 以前靠模型名前缀硬编码；新模型一出就误判，
//! 要么误发 `budget_tokens`，要么把 `adaptive` 发给不支持的模型触发 400。这里改成
//! 按模型家族/代际的能力表查询，未知模型走保守默认（手动 budget 模式）。

/// 模型的 thinking 控制方式。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClaudeThinkingMode {
    /// 不支持 thinking（例如 Haiku 3.x / Claude 3.x）。
    Unsupported,
    /// `thinking.type = "enabled"` + `budget_tokens`。
    ManualBudget,
    /// `thinking.type = "adaptive"` + `output_config.effort`。
    Adaptive,
}

/// 单个模型的 thinking 能力。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClaudeThinkingCapability {
    pub mode: ClaudeThinkingMode,
    /// `budget_tokens` 的最小值（Anthropic 要求 ≥ 1024）。
    pub min_budget_tokens: u64,
    /// 模型默认的 `max_tokens`（请求未提供时用于约束 budget）。
    pub default_max_tokens: u64,
    /// `output_config.effort` 是否接受 `max`。
    pub supports_max_effort: bool,
}

const MANUAL: ClaudeThinkingCapability = ClaudeThinkingCapability {
    mode: ClaudeThinkingMode::ManualBudget,
    min_budget_tokens: 1024,
    default_max_tokens: 32_000,
    supports_max_effort: false,
};

const ADAPTIVE: ClaudeThinkingCapability = ClaudeThinkingCapability {
    mode: ClaudeThinkingMode::Adaptive,
    min_budget_tokens: 1024,
    default_max_tokens: 64_000,
    supports_max_effort: false,
};

const ADAPTIVE_MAX_EFFORT: ClaudeThinkingCapability = ClaudeThinkingCapability {
    mode: ClaudeThinkingMode::Adaptive,
    min_budget_tokens: 1024,
    default_max_tokens: 128_000,
    supports_max_effort: true,
};

const UNSUPPORTED: ClaudeThinkingCapability = ClaudeThinkingCapability {
    mode: ClaudeThinkingMode::Unsupported,
    min_budget_tokens: 0,
    default_max_tokens: 8_192,
    supports_max_effort: false,
};

/// 按模型名查能力表。名字里的 `.`/`_` 归一为 `-`，忽略大小写与日期后缀。
pub fn claude_thinking_capability(model: &str) -> ClaudeThinkingCapability {
    let model = model.trim().to_ascii_lowercase().replace(['.', '_'], "-");
    let (family, generation) = parse_claude_family_and_generation(&model);
    match family {
        Some("mythos") | Some("fable") => ADAPTIVE_MAX_EFFORT,
        Some("opus") => match generation {
            Some((5, _)) | Some((4, 7)) | Some((4, 8)) => ADAPTIVE_MAX_EFFORT,
            Some((4, 6)) => ADAPTIVE,
            Some((4, _)) => MANUAL,
            Some((3, _)) | Some((2, _)) | Some((1, _)) => UNSUPPORTED,
            _ => MANUAL,
        },
        Some("sonnet") => match generation {
            Some((5, _)) | Some((4, 6)) | Some((4, 7)) | Some((4, 8)) => ADAPTIVE,
            Some((4, _)) | Some((3, 7)) => MANUAL,
            Some((3, _)) | Some((2, _)) | Some((1, _)) => UNSUPPORTED,
            _ => MANUAL,
        },
        Some("haiku") => match generation {
            Some((4, 5)) | Some((4, _)) | Some((5, _)) => MANUAL,
            Some((3, _)) => UNSUPPORTED,
            _ => MANUAL,
        },
        _ => MANUAL,
    }
}

/// 兼容旧调用：模型是否使用 adaptive effort。
pub fn claude_model_uses_adaptive_effort(model: &str) -> bool {
    claude_thinking_capability(model).mode == ClaudeThinkingMode::Adaptive
}

fn parse_claude_family_and_generation(model: &str) -> (Option<&'static str>, Option<(u64, u64)>) {
    let family = ["mythos", "fable", "opus", "sonnet", "haiku"]
        .into_iter()
        .find(|family| model.contains(family));
    let Some(family) = family else {
        return (None, None);
    };
    let (before, after) = model.split_once(family).unwrap_or(("", ""));
    // 新命名：`claude-opus-4-6-...`，版本在家族名之后；旧命名：`claude-3-5-haiku-...`，
    // 版本在家族名之前。两边都试，日期后缀（例如 20251001）不是版本号。
    let generation =
        generation_from_parts(after.trim_start_matches('-').split('-')).or_else(|| {
            generation_from_parts(
                before
                    .trim_start_matches("claude")
                    .trim_matches('-')
                    .split('-'),
            )
        });
    (Some(family), generation)
}

fn generation_from_parts<'a>(parts: impl Iterator<Item = &'a str>) -> Option<(u64, u64)> {
    let mut numbers = parts
        .map(|part| {
            part.chars()
                .take_while(|c| c.is_ascii_digit())
                .collect::<String>()
        })
        .take_while(|part| !part.is_empty())
        .filter_map(|part| part.parse::<u64>().ok());
    let major = numbers.next()?;
    if major > 99 {
        return None;
    }
    let minor = numbers.next().filter(|minor| *minor <= 99).unwrap_or(0);
    Some((major, minor))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capability_table_covers_known_generations() {
        assert_eq!(
            claude_thinking_capability("claude-opus-4-6").mode,
            ClaudeThinkingMode::Adaptive
        );
        assert_eq!(
            claude_thinking_capability("claude-opus-4.7").mode,
            ClaudeThinkingMode::Adaptive
        );
        assert!(claude_thinking_capability("claude-opus-4-8").supports_max_effort);
        assert!(claude_thinking_capability("claude-opus-5").supports_max_effort);
        assert!(claude_thinking_capability("claude-fable-5-1").supports_max_effort);
        assert!(claude_thinking_capability("claude-mythos-5").supports_max_effort);
        assert_eq!(
            claude_thinking_capability("claude-sonnet-4-6").mode,
            ClaudeThinkingMode::Adaptive
        );
        assert_eq!(
            claude_thinking_capability("claude-sonnet-5").mode,
            ClaudeThinkingMode::Adaptive
        );
        assert_eq!(
            claude_thinking_capability("claude-sonnet-4-5-20250929").mode,
            ClaudeThinkingMode::ManualBudget
        );
        assert_eq!(
            claude_thinking_capability("claude-opus-4-5-20251101").mode,
            ClaudeThinkingMode::ManualBudget
        );
        assert_eq!(
            claude_thinking_capability("claude-haiku-4-5-20251001").mode,
            ClaudeThinkingMode::ManualBudget
        );
        assert_eq!(
            claude_thinking_capability("claude-3-5-haiku-20241022").mode,
            ClaudeThinkingMode::Unsupported
        );
        assert_eq!(
            claude_thinking_capability("claude-3-7-sonnet-20250219").mode,
            ClaudeThinkingMode::ManualBudget
        );
        assert_eq!(
            claude_thinking_capability("gpt-5").mode,
            ClaudeThinkingMode::ManualBudget
        );
    }

    #[test]
    fn legacy_helper_keeps_previous_answers() {
        assert!(claude_model_uses_adaptive_effort("claude-opus-4-6"));
        assert!(claude_model_uses_adaptive_effort("claude-sonnet-4-6"));
        assert!(claude_model_uses_adaptive_effort("claude-mythos-5"));
        assert!(!claude_model_uses_adaptive_effort(
            "claude-sonnet-4-5-20250929"
        ));
        assert!(!claude_model_uses_adaptive_effort(
            "claude-haiku-4-5-20251001"
        ));
    }
}
