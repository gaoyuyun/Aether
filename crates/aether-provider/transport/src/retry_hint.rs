//! 统一的上游重试提示提取。
//!
//! 每个上游用不同的方式告诉网关「多久之后再来」：HTTP `Retry-After`、Anthropic 的
//! `anthropic-ratelimit-unified-*` 头、Codex 错误体里的 `usage_limit_reached.resets_at`、
//! Google 的 `RetryInfo.retryDelay` / `ErrorInfo.metadata.quotaResetDelay`、Grok 的中英文
//! 等待文案。这里把它们收敛成一个 [`UpstreamRetryHint`]，冷却决策只看这个结构。
//!
//! 规则：
//! - 只在上游明确给出未来时刻或时长时返回提示；解析不到就返回 [`RetryHintSource::None`]，
//!   由调用方走指数退避。
//! - Anthropic 的 overage-only / 单模型拒绝不描述整把凭据，标成
//!   [`RetryHintScope::KeyModel`]，调用方据此只冷却 Key+模型。
//! - 提示带来源，落库后前端能解释「为什么冷却这么久」。

use std::collections::BTreeMap;
use std::sync::OnceLock;
use std::time::Duration;

use regex::Regex;
use serde_json::Value;

/// Anthropic 窗口重置时刻之后再加的随机抖动上限（秒），避免多把 Key 在同一秒
/// 一起撞回上游。
pub const ANTHROPIC_RESET_FUZZ_MAX_SECS: u64 = 30;
/// 上游给出的重试时长超过这个值就按「配额耗尽」处理，写入池成员的 quota 元数据。
pub const RETRY_HINT_QUOTA_EXHAUSTED_THRESHOLD: Duration = Duration::from_secs(5 * 60);
/// 上游给出的重试时长低于这个值时不冷却 Key，同 Key 立即重试一次。
pub const RETRY_HINT_IMMEDIATE_RETRY_THRESHOLD: Duration = Duration::from_secs(3);
/// 单次提示允许的最长时长；再长就是上游给错了，按上限截断。
const RETRY_HINT_MAX_DURATION: Duration = Duration::from_secs(14 * 24 * 60 * 60);

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RetryHintSource {
    /// HTTP `Retry-After` 头（秒或 HTTP-date）。
    RetryAfterHeader,
    /// Anthropic `anthropic-ratelimit-unified-5h-*` 窗口。
    AnthropicRateLimitWindow5h,
    /// Anthropic `anthropic-ratelimit-unified-7d-*` 窗口。
    AnthropicRateLimitWindow7d,
    /// Anthropic 只有 `anthropic-ratelimit-unified-reset`，没有具体窗口。
    AnthropicRateLimitWindow,
    /// Google `google.rpc.RetryInfo.retryDelay` 或 `ErrorInfo.metadata.quotaResetDelay`。
    GoogleRetryInfo,
    /// Codex `usage_limit_reached.resets_at` / `resets_in_seconds`。
    CodexResetsAt,
    /// Grok 错误文案里的等待时长。
    GrokText,
    /// 没有任何可用提示。
    None,
}

impl RetryHintSource {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::RetryAfterHeader => "retry_after_header",
            Self::AnthropicRateLimitWindow5h => "ratelimit_window_5h",
            Self::AnthropicRateLimitWindow7d => "ratelimit_window_7d",
            Self::AnthropicRateLimitWindow => "ratelimit_window",
            Self::GoogleRetryInfo => "google_retry_info",
            Self::CodexResetsAt => "codex_resets_at",
            Self::GrokText => "grok_text",
            Self::None => "none",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RetryHintScope {
    /// 整把 Key 都不可用。
    Key,
    /// 只有这把 Key 上的这个模型不可用。
    KeyModel,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct UpstreamRetryHint {
    pub source: RetryHintSource,
    /// 距离现在还要等多久。`None` 表示没有提示。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retry_after: Option<Duration>,
    /// 上游给出的绝对重置时刻（Unix 秒）。有些来源只有时长，这里由 `now` 推算。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reset_at_unix_secs: Option<u64>,
    pub scope: RetryHintScope,
    /// 上游明确说这是配额耗尽（而不是短暂限流）。
    #[serde(default)]
    pub quota_exhausted: bool,
    /// 解析到但未采用的窗口名，便于诊断。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub rejected_windows: Vec<String>,
}

impl UpstreamRetryHint {
    pub const fn none() -> Self {
        Self {
            source: RetryHintSource::None,
            retry_after: None,
            reset_at_unix_secs: None,
            scope: RetryHintScope::Key,
            quota_exhausted: false,
            rejected_windows: Vec::new(),
        }
    }

    pub fn is_none(&self) -> bool {
        self.retry_after.is_none()
    }

    fn from_duration(source: RetryHintSource, retry_after: Duration, now_unix_secs: u64) -> Self {
        let retry_after = retry_after.min(RETRY_HINT_MAX_DURATION);
        Self {
            source,
            retry_after: Some(retry_after),
            reset_at_unix_secs: Some(now_unix_secs.saturating_add(retry_after.as_secs())),
            scope: RetryHintScope::Key,
            quota_exhausted: false,
            rejected_windows: Vec::new(),
        }
    }

    fn from_reset_at(source: RetryHintSource, reset_at_unix_secs: u64, now_unix_secs: u64) -> Self {
        let retry_after = Duration::from_secs(reset_at_unix_secs.saturating_sub(now_unix_secs))
            .min(RETRY_HINT_MAX_DURATION);
        Self {
            source,
            retry_after: Some(retry_after),
            reset_at_unix_secs: Some(reset_at_unix_secs),
            scope: RetryHintScope::Key,
            quota_exhausted: false,
            rejected_windows: Vec::new(),
        }
    }

    /// 提示的时长是否短到不值得冷却 Key。
    pub fn is_immediate_retry(&self) -> bool {
        self.retry_after
            .is_some_and(|value| value < RETRY_HINT_IMMEDIATE_RETRY_THRESHOLD)
    }

    /// 提示的时长是否长到应当按配额耗尽处理。
    pub fn indicates_quota_exhaustion(&self) -> bool {
        self.quota_exhausted
            || self
                .retry_after
                .is_some_and(|value| value >= RETRY_HINT_QUOTA_EXHAUSTED_THRESHOLD)
    }

    pub fn to_json(&self) -> Value {
        serde_json::json!({
            "source": self.source.as_str(),
            "retry_after_secs": self.retry_after.map(|value| value.as_secs()),
            "reset_at": self.reset_at_unix_secs,
            "scope": match self.scope {
                RetryHintScope::Key => "key",
                RetryHintScope::KeyModel => "key_model",
            },
            "quota_exhausted": self.quota_exhausted,
            "rejected_windows": self.rejected_windows,
        })
    }
}

/// 从一次失败响应里提取重试提示。
///
/// `headers` 的键不区分大小写；`body` 是上游错误体原文（可能不是 JSON）。
/// `now_unix_secs` 由调用方传入，便于测试与避免解析时刻漂移。
pub fn extract_upstream_retry_hint(
    provider_type: &str,
    status_code: u16,
    headers: Option<&BTreeMap<String, String>>,
    body: Option<&str>,
    now_unix_secs: u64,
) -> UpstreamRetryHint {
    let provider_type = provider_type.trim().to_ascii_lowercase();
    let body_json = body
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .and_then(|value| serde_json::from_str::<Value>(value).ok());

    let provider_hint = match provider_type.as_str() {
        "claude_code" => anthropic_rate_limit_hint(headers, now_unix_secs),
        "codex" | "chatgpt_web" => {
            codex_usage_limit_hint(status_code, body_json.as_ref(), now_unix_secs)
        }
        "antigravity" | "gemini_cli" | "vertex_ai" => {
            google_retry_info_hint(body_json.as_ref(), body, now_unix_secs).map(|mut hint| {
                if provider_type == "antigravity" {
                    // Antigravity 的配额按模型分组，调度层已有模型级隔离。
                    hint.scope = RetryHintScope::KeyModel;
                }
                hint
            })
        }
        "grok" => grok_text_hint(body_json.as_ref(), body, now_unix_secs),
        _ => None,
    };
    if let Some(hint) = provider_hint {
        return hint;
    }
    if let Some(hint) = retry_after_header_hint(headers, now_unix_secs) {
        return hint;
    }
    // 供应商特定解析失败后，再兜底看一遍通用的 Google 形状：很多自定义 OpenAI 兼容
    // 上游其实转发的是 Google 错误。
    if !matches!(
        provider_type.as_str(),
        "antigravity" | "gemini_cli" | "vertex_ai"
    ) {
        if let Some(hint) = google_retry_info_hint(body_json.as_ref(), body, now_unix_secs) {
            return hint;
        }
    }
    UpstreamRetryHint::none()
}

fn header_value<'a>(headers: Option<&'a BTreeMap<String, String>>, name: &str) -> Option<&'a str> {
    headers?
        .iter()
        .find(|(key, _)| key.eq_ignore_ascii_case(name))
        .map(|(_, value)| value.trim())
        .filter(|value| !value.is_empty())
}

/// `Retry-After`: 秒数或 HTTP-date（RFC 7231）。
pub fn parse_retry_after_header(raw: &str, now_unix_secs: u64) -> Option<Duration> {
    let raw = raw.trim();
    if raw.is_empty() {
        return None;
    }
    if let Ok(seconds) = raw.parse::<u64>() {
        return Some(Duration::from_secs(seconds));
    }
    if let Ok(seconds) = raw.parse::<f64>() {
        if seconds.is_finite() && seconds >= 0.0 {
            return Some(Duration::from_secs(seconds.ceil() as u64));
        }
    }
    let parsed = chrono::DateTime::parse_from_rfc2822(raw)
        .ok()
        .or_else(|| chrono::DateTime::parse_from_rfc3339(raw).ok())?;
    let deadline = parsed.timestamp().max(0) as u64;
    (deadline > now_unix_secs).then(|| Duration::from_secs(deadline - now_unix_secs))
}

fn retry_after_header_hint(
    headers: Option<&BTreeMap<String, String>>,
    now_unix_secs: u64,
) -> Option<UpstreamRetryHint> {
    let raw = header_value(headers, "retry-after")?;
    let retry_after = parse_retry_after_header(raw, now_unix_secs)?;
    Some(UpstreamRetryHint::from_duration(
        RetryHintSource::RetryAfterHeader,
        retry_after,
        now_unix_secs,
    ))
}

/// 解析 Unix 秒、Unix 毫秒或 RFC 3339 时间戳。
pub fn parse_unix_or_timestamp(raw: &str) -> Option<u64> {
    let raw = raw.trim();
    if raw.is_empty() {
        return None;
    }
    if let Ok(value) = raw.parse::<u64>() {
        // 13 位以上按毫秒处理。
        return Some(if value > 100_000_000_000 {
            value / 1_000
        } else {
            value
        });
    }
    if let Ok(value) = raw.parse::<f64>() {
        if value.is_finite() && value > 0.0 {
            return Some(if value > 100_000_000_000.0 {
                (value / 1_000.0) as u64
            } else {
                value as u64
            });
        }
    }
    chrono::DateTime::parse_from_rfc3339(raw)
        .ok()
        .map(|value| value.timestamp().max(0) as u64)
}

fn anthropic_window_allowed(status: &str) -> bool {
    status == "allowed" || status == "allowed_warning"
}

fn anthropic_utilization_healthy(raw: Option<&str>) -> bool {
    raw.and_then(|value| value.trim().parse::<f64>().ok())
        .is_some_and(|value| value.is_finite() && (0.0..1.0).contains(&value))
}

/// Anthropic 的 overage-only / Fable-only 拒绝不描述整把凭据：共享的 5h / 7d 窗口
/// 仍然可用时，只该冷却这个模型。
fn anthropic_overage_only_rejection(
    headers: Option<&BTreeMap<String, String>>,
    status_5h: &str,
    status_7d: &str,
    status_7d_oi: &str,
) -> bool {
    if status_5h == "rejected" || status_7d == "rejected" {
        return false;
    }
    let overage_status = header_value(headers, "anthropic-ratelimit-unified-overage-status")
        .map(str::to_ascii_lowercase)
        .unwrap_or_default();
    let overage_disabled_reason = header_value(
        headers,
        "anthropic-ratelimit-unified-overage-disabled-reason",
    )
    .is_some();
    let representative_claim =
        header_value(headers, "anthropic-ratelimit-unified-representative-claim")
            .map(str::to_ascii_lowercase)
            .unwrap_or_default();
    let overage_rejected = status_7d_oi == "rejected"
        || overage_status == "rejected"
        || overage_disabled_reason
        || representative_claim.contains("overage");
    if !overage_rejected {
        return false;
    }
    let shared_5h_allowed = anthropic_window_allowed(status_5h);
    let shared_7d_allowed = anthropic_window_allowed(status_7d);
    if shared_5h_allowed && shared_7d_allowed {
        return true;
    }
    if shared_7d_allowed
        && status_5h.is_empty()
        && anthropic_utilization_healthy(header_value(
            headers,
            "anthropic-ratelimit-unified-5h-utilization",
        ))
    {
        return true;
    }
    if shared_5h_allowed
        && status_7d.is_empty()
        && anthropic_utilization_healthy(header_value(
            headers,
            "anthropic-ratelimit-unified-7d-utilization",
        ))
    {
        return true;
    }
    false
}

/// Anthropic 响应头是否明确宣告共享 5h / 7d 窗口被拒绝。
pub fn anthropic_headers_indicate_unified_rejection(
    headers: Option<&BTreeMap<String, String>>,
) -> bool {
    let unified_status = header_value(headers, "anthropic-ratelimit-unified-status")
        .map(str::to_ascii_lowercase)
        .unwrap_or_default();
    let status_5h = header_value(headers, "anthropic-ratelimit-unified-5h-status")
        .map(str::to_ascii_lowercase)
        .unwrap_or_default();
    let status_7d = header_value(headers, "anthropic-ratelimit-unified-7d-status")
        .map(str::to_ascii_lowercase)
        .unwrap_or_default();
    if status_5h == "rejected" || status_7d == "rejected" {
        return true;
    }
    if unified_status != "rejected" {
        return false;
    }
    let status_7d_oi = header_value(headers, "anthropic-ratelimit-unified-7d_oi-status")
        .map(str::to_ascii_lowercase)
        .unwrap_or_default();
    !anthropic_overage_only_rejection(headers, &status_5h, &status_7d, &status_7d_oi)
}

/// 确定性抖动：同一把 Key 的同一个重置时刻得到同一个抖动值，避免单元测试不稳定，
/// 也避免同一批 Key 在同一秒一起撞回上游。
fn anthropic_reset_fuzz_secs(deadline_unix_secs: u64) -> u64 {
    let mut hash = 0xcbf29ce484222325u64;
    for byte in deadline_unix_secs.to_le_bytes() {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    1 + hash % ANTHROPIC_RESET_FUZZ_MAX_SECS
}

/// 对齐 CLIProxyAPI `ParseClaudeRateLimitReset`：只在窗口状态是 `rejected` 时采用
/// 对应的 `*-reset`，取最晚的截止时刻，再加 1–30 秒抖动。overage-only 拒绝不描述
/// 凭据本身，返回模型级作用域且不给冷却时长。
fn anthropic_rate_limit_hint(
    headers: Option<&BTreeMap<String, String>>,
    now_unix_secs: u64,
) -> Option<UpstreamRetryHint> {
    let lower = |name: &str| {
        header_value(headers, name)
            .map(str::to_ascii_lowercase)
            .unwrap_or_default()
    };
    let unified_status = lower("anthropic-ratelimit-unified-status");
    let status_5h = lower("anthropic-ratelimit-unified-5h-status");
    let status_7d = lower("anthropic-ratelimit-unified-7d-status");
    let status_7d_oi = lower("anthropic-ratelimit-unified-7d_oi-status");
    let overage_only =
        anthropic_overage_only_rejection(headers, &status_5h, &status_7d, &status_7d_oi);
    let any_anthropic_signal = unified_status == "rejected"
        || status_5h == "rejected"
        || status_7d == "rejected"
        || status_7d_oi == "rejected"
        || header_value(headers, "anthropic-ratelimit-unified-reset").is_some();
    if !any_anthropic_signal && !overage_only {
        // 没有任何 Anthropic 限流头：交给通用的 Retry-After 路径，不加窗口抖动。
        return None;
    }

    let mut rejected_windows = Vec::new();
    if unified_status == "rejected" {
        rejected_windows.push("unified".to_string());
    }
    if status_5h == "rejected" {
        rejected_windows.push("5h".to_string());
    }
    if status_7d == "rejected" {
        rejected_windows.push("7d".to_string());
    }
    if status_7d_oi == "rejected" {
        rejected_windows.push("7d_oi".to_string());
    }

    let mut candidates: Vec<(RetryHintSource, u64)> = Vec::new();
    if !overage_only {
        if let Some(raw) = header_value(headers, "retry-after") {
            rejected_windows.push("retry-after".to_string());
            if let Some(retry_after) = parse_retry_after_header(raw, now_unix_secs) {
                candidates.push((
                    RetryHintSource::RetryAfterHeader,
                    now_unix_secs.saturating_add(retry_after.as_secs()),
                ));
            }
        }
    }
    if status_5h == "rejected" {
        if let Some(deadline) = header_value(headers, "anthropic-ratelimit-unified-5h-reset")
            .and_then(parse_unix_or_timestamp)
            .filter(|deadline| *deadline > now_unix_secs)
        {
            candidates.push((RetryHintSource::AnthropicRateLimitWindow5h, deadline));
        }
    }
    if status_7d == "rejected" {
        if let Some(deadline) = header_value(headers, "anthropic-ratelimit-unified-7d-reset")
            .and_then(parse_unix_or_timestamp)
            .filter(|deadline| *deadline > now_unix_secs)
        {
            candidates.push((RetryHintSource::AnthropicRateLimitWindow7d, deadline));
        }
    }
    if status_7d_oi == "rejected" && !overage_only {
        if let Some(deadline) = header_value(headers, "anthropic-ratelimit-unified-7d_oi-reset")
            .and_then(parse_unix_or_timestamp)
            .filter(|deadline| *deadline > now_unix_secs)
        {
            candidates.push((RetryHintSource::AnthropicRateLimitWindow7d, deadline));
        }
    }
    let unified_rejected = !overage_only
        && (unified_status == "rejected"
            || status_5h == "rejected"
            || status_7d == "rejected"
            || status_7d_oi == "rejected"
            || (unified_status.is_empty()
                && !anthropic_window_allowed(&status_5h)
                && !anthropic_window_allowed(&status_7d)));
    if unified_rejected {
        if let Some(raw) = header_value(headers, "anthropic-ratelimit-unified-reset") {
            if !rejected_windows.iter().any(|window| window == "unified") {
                rejected_windows.push("unified".to_string());
            }
            if let Some(deadline) =
                parse_unix_or_timestamp(raw).filter(|deadline| *deadline > now_unix_secs)
            {
                candidates.push((RetryHintSource::AnthropicRateLimitWindow, deadline));
            }
        }
    }

    if overage_only {
        return Some(UpstreamRetryHint {
            source: RetryHintSource::None,
            retry_after: None,
            reset_at_unix_secs: None,
            scope: RetryHintScope::KeyModel,
            quota_exhausted: false,
            rejected_windows,
        });
    }
    let (source, deadline) = candidates
        .into_iter()
        .max_by_key(|(_, deadline)| *deadline)?;
    let deadline = deadline.saturating_add(anthropic_reset_fuzz_secs(deadline));
    let mut hint = UpstreamRetryHint::from_reset_at(source, deadline, now_unix_secs);
    hint.quota_exhausted = matches!(
        source,
        RetryHintSource::AnthropicRateLimitWindow5h
            | RetryHintSource::AnthropicRateLimitWindow7d
            | RetryHintSource::AnthropicRateLimitWindow
    );
    hint.rejected_windows = rejected_windows;
    Some(hint)
}

fn json_string<'a>(value: Option<&'a Value>) -> Option<&'a str> {
    value
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
}

fn json_u64(value: Option<&Value>) -> Option<u64> {
    let value = value?;
    value
        .as_u64()
        .or_else(|| value.as_i64().and_then(|value| u64::try_from(value).ok()))
        .or_else(|| {
            value
                .as_f64()
                .filter(|value| value.is_finite() && *value > 0.0)
                .map(|value| value as u64)
        })
        .or_else(|| {
            value
                .as_str()
                .and_then(|value| value.trim().parse::<u64>().ok())
        })
}

/// Codex：`error.type == usage_limit_reached` 才是配额耗尽，带 `resets_at`（Unix 秒）
/// 或 `resets_in_seconds`。`rate_limit_error` 是每分钟限流，应当重试而不是长冷却。
fn codex_usage_limit_hint(
    status_code: u16,
    body_json: Option<&Value>,
    now_unix_secs: u64,
) -> Option<UpstreamRetryHint> {
    if status_code != 429 {
        return None;
    }
    let body = body_json?;
    for quota in [body.get("error"), Some(body)].into_iter().flatten() {
        let is_usage_limit = json_string(quota.get("type"))
            .is_some_and(|value| value.eq_ignore_ascii_case("usage_limit_reached"));
        if !is_usage_limit {
            continue;
        }
        if let Some(resets_at) = json_u64(quota.get("resets_at")).filter(|value| *value > 0) {
            let resets_at = if resets_at > 100_000_000_000 {
                resets_at / 1_000
            } else {
                resets_at
            };
            if resets_at > now_unix_secs {
                let mut hint = UpstreamRetryHint::from_reset_at(
                    RetryHintSource::CodexResetsAt,
                    resets_at,
                    now_unix_secs,
                );
                hint.quota_exhausted = true;
                return Some(hint);
            }
        }
        if let Some(seconds) = json_u64(quota.get("resets_in_seconds")).filter(|value| *value > 0) {
            let mut hint = UpstreamRetryHint::from_duration(
                RetryHintSource::CodexResetsAt,
                Duration::from_secs(seconds),
                now_unix_secs,
            );
            hint.quota_exhausted = true;
            return Some(hint);
        }
    }
    None
}

/// Go `time.ParseDuration` 的子集：`1h2m3s`、`0.5s`、`45m`、`300ms`。
pub fn parse_go_duration(raw: &str) -> Option<Duration> {
    static GO_DURATION_RE: OnceLock<Regex> = OnceLock::new();
    let raw = raw.trim().to_ascii_lowercase();
    if raw.is_empty() {
        return None;
    }
    if let Ok(seconds) = raw.parse::<f64>() {
        return (seconds.is_finite() && seconds >= 0.0).then(|| Duration::from_secs_f64(seconds));
    }
    let regex = GO_DURATION_RE.get_or_init(|| {
        Regex::new(r"(\d+(?:\.\d+)?)(ns|us|µs|ms|s|m|h|d)").expect("go duration regex must compile")
    });
    let mut total = 0.0f64;
    let mut matched_len = 0usize;
    for capture in regex.captures_iter(&raw) {
        let whole = capture.get(0)?;
        matched_len += whole.as_str().len();
        let amount = capture.get(1)?.as_str().parse::<f64>().ok()?;
        let unit = capture.get(2)?.as_str();
        total += match unit {
            "ns" => amount / 1_000_000_000.0,
            "us" | "µs" => amount / 1_000_000.0,
            "ms" => amount / 1_000.0,
            "s" => amount,
            "m" => amount * 60.0,
            "h" => amount * 3_600.0,
            "d" => amount * 86_400.0,
            _ => 0.0,
        };
    }
    if matched_len == 0 || matched_len != raw.len() || !total.is_finite() {
        return None;
    }
    Some(Duration::from_secs_f64(total))
}

/// Google：`error.details[]` 里的 `RetryInfo.retryDelay`、`ErrorInfo.metadata.quotaResetDelay` /
/// `quotaResetTimeStamp`，以及 `error.message` 里的 "after 30s" / "reset after 45m"。
/// `ErrorInfo.reason ∈ {QUOTA_EXHAUSTED, RATE_LIMIT_EXCEEDED}` 决定是否算配额耗尽。
fn google_retry_info_hint(
    body_json: Option<&Value>,
    body: Option<&str>,
    now_unix_secs: u64,
) -> Option<UpstreamRetryHint> {
    let error = body_json?.get("error")?;
    let details = error.get("details").and_then(Value::as_array);
    let mut quota_exhausted = false;
    let mut retry_after: Option<Duration> = None;
    let mut reset_at: Option<u64> = None;
    if let Some(details) = details {
        for detail in details {
            let type_name = json_string(detail.get("@type")).unwrap_or_default();
            if type_name.ends_with("google.rpc.RetryInfo") {
                if let Some(delay) = json_string(detail.get("retryDelay"))
                    .or_else(|| json_string(detail.get("retry_delay")))
                    .and_then(parse_go_duration)
                {
                    retry_after = Some(retry_after.map_or(delay, |current| current.max(delay)));
                }
                if let Some(delay) = detail
                    .get("retryDelay")
                    .and_then(Value::as_object)
                    .and_then(|object| json_u64(object.get("seconds")))
                {
                    let delay = Duration::from_secs(delay);
                    retry_after = Some(retry_after.map_or(delay, |current| current.max(delay)));
                }
            }
            if type_name.ends_with("google.rpc.ErrorInfo") {
                let reason = json_string(detail.get("reason")).unwrap_or_default();
                if reason.eq_ignore_ascii_case("QUOTA_EXHAUSTED") {
                    quota_exhausted = true;
                }
                if let Some(metadata) = detail.get("metadata").and_then(Value::as_object) {
                    if let Some(delay) =
                        json_string(metadata.get("quotaResetDelay")).and_then(parse_go_duration)
                    {
                        retry_after = Some(retry_after.map_or(delay, |current| current.max(delay)));
                    }
                    if let Some(deadline) = json_string(metadata.get("quotaResetTimeStamp"))
                        .or_else(|| json_string(metadata.get("quotaResetTimestamp")))
                        .and_then(parse_unix_or_timestamp)
                        .filter(|deadline| *deadline > now_unix_secs)
                    {
                        reset_at = Some(reset_at.map_or(deadline, |current| current.max(deadline)));
                    }
                }
            }
            // 旧形状：details[].metadata.quotaResetDelay，没有 @type。
            if type_name.is_empty() {
                if let Some(metadata) = detail.get("metadata").and_then(Value::as_object) {
                    if let Some(delay) =
                        json_string(metadata.get("quotaResetDelay")).and_then(parse_go_duration)
                    {
                        retry_after = Some(retry_after.map_or(delay, |current| current.max(delay)));
                    }
                    if let Some(deadline) = json_string(metadata.get("quotaResetTimeStamp"))
                        .or_else(|| json_string(metadata.get("quotaResetTimestamp")))
                        .and_then(parse_unix_or_timestamp)
                        .filter(|deadline| *deadline > now_unix_secs)
                    {
                        reset_at = Some(reset_at.map_or(deadline, |current| current.max(deadline)));
                    }
                }
            }
        }
    }
    let status = json_string(error.get("status")).unwrap_or_default();
    if status.eq_ignore_ascii_case("RESOURCE_EXHAUSTED")
        && body
            .map(|body| body.to_ascii_lowercase())
            .is_some_and(|lower| {
                ["quota exhausted", "quota_exhausted", "quota exceeded"]
                    .iter()
                    .any(|keyword| lower.contains(keyword))
            })
    {
        quota_exhausted = true;
    }
    if retry_after.is_none() && reset_at.is_none() {
        static AFTER_SECONDS_RE: OnceLock<Regex> = OnceLock::new();
        let regex = AFTER_SECONDS_RE.get_or_init(|| {
            Regex::new(
                r"(?i)after\s+((?:\d+(?:\.\d+)?)(?:\s*(?:h|m|s))?(?:\d+(?:\.\d+)?(?:h|m|s))*)",
            )
            .expect("google after-duration regex must compile")
        });
        let message = json_string(error.get("message")).unwrap_or_default();
        if let Some(capture) = regex.captures(message) {
            let raw = capture.get(1)?.as_str().replace(' ', "");
            if let Some(delay) =
                parse_go_duration(&raw).or_else(|| raw.parse::<u64>().ok().map(Duration::from_secs))
            {
                retry_after = Some(delay);
            }
        }
    }
    let mut hint = match (reset_at, retry_after) {
        (Some(deadline), Some(delay)) => {
            let from_delay = now_unix_secs.saturating_add(delay.as_secs());
            UpstreamRetryHint::from_reset_at(
                RetryHintSource::GoogleRetryInfo,
                deadline.max(from_delay),
                now_unix_secs,
            )
        }
        (Some(deadline), None) => UpstreamRetryHint::from_reset_at(
            RetryHintSource::GoogleRetryInfo,
            deadline,
            now_unix_secs,
        ),
        (None, Some(delay)) => {
            UpstreamRetryHint::from_duration(RetryHintSource::GoogleRetryInfo, delay, now_unix_secs)
        }
        (None, None) => return None,
    };
    hint.quota_exhausted = quota_exhausted;
    Some(hint)
}

fn grok_chinese_wait_duration_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"(?i)(?:(\d+)\s*天)?\s*(?:(\d+)\s*(?:小时|小時))?\s*(?:(\d+)\s*分钟)?\s*(?:(\d+)\s*秒)?")
            .expect("grok Chinese wait duration regex should compile")
    })
}

fn grok_english_wait_duration_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r"(?i)(?:(\d+)\s*(?:d|day|days))?\s*(?:(\d+)\s*(?:h|hour|hours))?\s*(?:(\d+)\s*(?:m|min|mins|minute|minutes))?\s*(?:(\d+)\s*(?:s|sec|secs|second|seconds))?",
        )
        .expect("grok English wait duration regex should compile")
    })
}

fn grok_duration_capture_seconds(captures: regex::Captures<'_>) -> Option<u64> {
    let values = [1usize, 2, 3, 4]
        .into_iter()
        .map(|index| {
            captures
                .get(index)
                .and_then(|item| item.as_str().parse::<u64>().ok())
                .unwrap_or(0)
        })
        .collect::<Vec<_>>();
    let seconds = values[0]
        .saturating_mul(86_400)
        .saturating_add(values[1].saturating_mul(3_600))
        .saturating_add(values[2].saturating_mul(60))
        .saturating_add(values[3]);
    (seconds > 0).then_some(seconds)
}

/// Grok 错误文案里的中英文等待时长（"等待 6小时 13分钟"、"wait 6h 13m"）。
pub fn grok_wait_duration_seconds_from_text(text: &str) -> Option<u64> {
    let text = text.trim();
    if text.is_empty() {
        return None;
    }
    for captures in grok_chinese_wait_duration_regex().captures_iter(text) {
        if let Some(seconds) = grok_duration_capture_seconds(captures) {
            return Some(seconds);
        }
    }
    for captures in grok_english_wait_duration_regex().captures_iter(text) {
        if let Some(seconds) = grok_duration_capture_seconds(captures) {
            return Some(seconds);
        }
    }
    None
}

/// Grok 响应体里的错误文案。
pub fn grok_response_error_text(value: &Value) -> Option<String> {
    match value {
        Value::String(text) => Some(text.trim().to_string()).filter(|text| !text.is_empty()),
        Value::Object(object) => {
            if let Some(error) = object.get("error") {
                if let Some(text) = grok_response_error_text(error) {
                    return Some(text);
                }
            }
            for key in ["message", "detail", "reason", "error"] {
                if let Some(text) = object
                    .get(key)
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|text| !text.is_empty())
                {
                    return Some(text.to_string());
                }
            }
            None
        }
        _ => None,
    }
}

fn grok_text_hint(
    body_json: Option<&Value>,
    body: Option<&str>,
    now_unix_secs: u64,
) -> Option<UpstreamRetryHint> {
    let text = body_json.and_then(grok_response_error_text).or_else(|| {
        body.map(str::trim)
            .filter(|b| !b.is_empty())
            .map(ToOwned::to_owned)
    })?;
    let seconds = grok_wait_duration_seconds_from_text(&text)?;
    let mut hint = UpstreamRetryHint::from_duration(
        RetryHintSource::GrokText,
        Duration::from_secs(seconds),
        now_unix_secs,
    );
    hint.scope = RetryHintScope::KeyModel;
    hint.quota_exhausted = true;
    Some(hint)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const NOW: u64 = 1_800_000_000;

    fn headers(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(key, value)| (key.to_string(), value.to_string()))
            .collect()
    }

    #[test]
    fn retry_after_header_accepts_seconds_and_http_date() {
        assert_eq!(
            parse_retry_after_header("120", NOW),
            Some(Duration::from_secs(120))
        );
        assert_eq!(
            parse_retry_after_header("1.5", NOW),
            Some(Duration::from_secs(2))
        );
        let future = chrono::DateTime::from_timestamp(NOW as i64 + 90, 0)
            .expect("timestamp")
            .to_rfc2822();
        assert_eq!(
            parse_retry_after_header(&future, NOW),
            Some(Duration::from_secs(90))
        );
        let past = chrono::DateTime::from_timestamp(NOW as i64 - 90, 0)
            .expect("timestamp")
            .to_rfc2822();
        assert_eq!(parse_retry_after_header(&past, NOW), None);
        assert_eq!(parse_retry_after_header("garbage", NOW), None);
    }

    #[test]
    fn generic_provider_uses_retry_after_header() {
        let hint = extract_upstream_retry_hint(
            "custom",
            429,
            Some(&headers(&[("Retry-After", "2")])),
            Some(r#"{"error":{"message":"slow down"}}"#),
            NOW,
        );
        assert_eq!(hint.source, RetryHintSource::RetryAfterHeader);
        assert_eq!(hint.retry_after, Some(Duration::from_secs(2)));
        assert!(hint.is_immediate_retry());
        assert!(!hint.indicates_quota_exhaustion());
        assert_eq!(hint.scope, RetryHintScope::Key);
    }

    #[test]
    fn anthropic_rejected_5h_window_takes_latest_deadline_with_fuzz() {
        let reset_5h = NOW + 3 * 3600;
        let hint = extract_upstream_retry_hint(
            "claude_code",
            429,
            Some(&headers(&[
                ("anthropic-ratelimit-unified-status", "rejected"),
                ("anthropic-ratelimit-unified-5h-status", "rejected"),
                (
                    "anthropic-ratelimit-unified-5h-reset",
                    &reset_5h.to_string(),
                ),
                ("anthropic-ratelimit-unified-7d-status", "allowed"),
                (
                    "anthropic-ratelimit-unified-7d-reset",
                    &(NOW + 6 * 86_400).to_string(),
                ),
                ("retry-after", "60"),
            ])),
            None,
            NOW,
        );
        assert_eq!(hint.source, RetryHintSource::AnthropicRateLimitWindow5h);
        let reset_at = hint.reset_at_unix_secs.expect("reset at");
        assert!(reset_at > reset_5h && reset_at <= reset_5h + ANTHROPIC_RESET_FUZZ_MAX_SECS);
        assert!(hint.indicates_quota_exhaustion());
        assert_eq!(hint.scope, RetryHintScope::Key);
        assert!(hint.rejected_windows.contains(&"5h".to_string()));
    }

    #[test]
    fn anthropic_rejected_7d_window_wins_over_5h() {
        let reset_7d = NOW + 2 * 86_400;
        let hint = extract_upstream_retry_hint(
            "claude_code",
            429,
            Some(&headers(&[
                ("anthropic-ratelimit-unified-5h-status", "rejected"),
                (
                    "anthropic-ratelimit-unified-5h-reset",
                    &(NOW + 600).to_string(),
                ),
                ("anthropic-ratelimit-unified-7d-status", "rejected"),
                (
                    "anthropic-ratelimit-unified-7d-reset",
                    &reset_7d.to_string(),
                ),
            ])),
            None,
            NOW,
        );
        assert_eq!(hint.source, RetryHintSource::AnthropicRateLimitWindow7d);
        assert!(hint.reset_at_unix_secs.expect("reset") > reset_7d);
    }

    #[test]
    fn anthropic_overage_only_rejection_is_model_scoped_without_cooldown() {
        let hint = extract_upstream_retry_hint(
            "claude_code",
            429,
            Some(&headers(&[
                ("anthropic-ratelimit-unified-status", "rejected"),
                ("anthropic-ratelimit-unified-5h-status", "allowed"),
                ("anthropic-ratelimit-unified-7d-status", "allowed"),
                ("anthropic-ratelimit-unified-overage-status", "rejected"),
                (
                    "anthropic-ratelimit-unified-reset",
                    &(NOW + 3600).to_string(),
                ),
                ("retry-after", "3600"),
            ])),
            None,
            NOW,
        );
        assert!(hint.is_none());
        assert_eq!(hint.scope, RetryHintScope::KeyModel);
        assert!(!anthropic_headers_indicate_unified_rejection(Some(
            &headers(&[
                ("anthropic-ratelimit-unified-status", "rejected"),
                ("anthropic-ratelimit-unified-5h-status", "allowed"),
                ("anthropic-ratelimit-unified-7d-status", "allowed"),
                ("anthropic-ratelimit-unified-overage-status", "rejected"),
            ])
        )));
    }

    #[test]
    fn anthropic_without_rate_limit_headers_falls_back_to_retry_after() {
        let hint = extract_upstream_retry_hint(
            "claude_code",
            529,
            Some(&headers(&[("retry-after", "5")])),
            None,
            NOW,
        );
        assert_eq!(hint.source, RetryHintSource::RetryAfterHeader);
        assert_eq!(hint.retry_after, Some(Duration::from_secs(5)));
    }

    #[test]
    fn codex_usage_limit_reached_uses_resets_at_and_ignores_rate_limit_error() {
        let hint = extract_upstream_retry_hint(
            "codex",
            429,
            None,
            Some(
                &json!({"error":{"type":"usage_limit_reached","resets_at": NOW + 7200}})
                    .to_string(),
            ),
            NOW,
        );
        assert_eq!(hint.source, RetryHintSource::CodexResetsAt);
        assert_eq!(hint.retry_after, Some(Duration::from_secs(7200)));
        assert!(hint.indicates_quota_exhaustion());

        let in_seconds = extract_upstream_retry_hint(
            "codex",
            429,
            None,
            Some(
                &json!({"error":{"type":"usage_limit_reached","resets_in_seconds": 90}})
                    .to_string(),
            ),
            NOW,
        );
        assert_eq!(in_seconds.retry_after, Some(Duration::from_secs(90)));

        let rate_limited = extract_upstream_retry_hint(
            "codex",
            429,
            None,
            Some(&json!({"error":{"type":"rate_limit_error","resets_at": NOW + 7200}}).to_string()),
            NOW,
        );
        assert!(rate_limited.is_none());
    }

    #[test]
    fn google_retry_info_and_error_info_are_parsed() {
        let body = json!({
            "error": {
                "code": 429,
                "status": "RESOURCE_EXHAUSTED",
                "message": "Quota exceeded for this model.",
                "details": [
                    {"@type": "type.googleapis.com/google.rpc.ErrorInfo", "reason": "QUOTA_EXHAUSTED", "metadata": {"quotaResetDelay": "1h30m"}},
                    {"@type": "type.googleapis.com/google.rpc.RetryInfo", "retryDelay": "0.5s"}
                ]
            }
        })
        .to_string();
        let antigravity = extract_upstream_retry_hint("antigravity", 429, None, Some(&body), NOW);
        assert_eq!(antigravity.source, RetryHintSource::GoogleRetryInfo);
        assert_eq!(antigravity.retry_after, Some(Duration::from_secs(5400)));
        assert!(antigravity.quota_exhausted);
        assert_eq!(antigravity.scope, RetryHintScope::KeyModel);

        let gemini = extract_upstream_retry_hint("gemini_cli", 429, None, Some(&body), NOW);
        assert_eq!(gemini.scope, RetryHintScope::Key);

        let short = extract_upstream_retry_hint(
            "gemini_cli",
            429,
            None,
            Some(&json!({"error":{"status":"RESOURCE_EXHAUSTED","message":"rate limited","details":[
                {"@type":"type.googleapis.com/google.rpc.ErrorInfo","reason":"RATE_LIMIT_EXCEEDED"},
                {"@type":"type.googleapis.com/google.rpc.RetryInfo","retryDelay":"2s"}
            ]}}).to_string()),
            NOW,
        );
        assert!(short.is_immediate_retry());
        assert!(!short.quota_exhausted);

        let message_only = extract_upstream_retry_hint(
            "gemini_cli",
            429,
            None,
            Some(r#"{"error":{"message":"Please retry after 45s."}}"#),
            NOW,
        );
        assert_eq!(message_only.retry_after, Some(Duration::from_secs(45)));
    }

    #[test]
    fn go_duration_parser_matches_time_parse_duration_subset() {
        assert_eq!(
            parse_go_duration("1h30m15s"),
            Some(Duration::from_secs(5415))
        );
        assert_eq!(parse_go_duration("0.5s"), Some(Duration::from_millis(500)));
        assert_eq!(parse_go_duration("300ms"), Some(Duration::from_millis(300)));
        assert_eq!(parse_go_duration("45m"), Some(Duration::from_secs(2700)));
        assert_eq!(parse_go_duration("2d"), Some(Duration::from_secs(172_800)));
        assert_eq!(parse_go_duration("1h junk"), None);
        assert_eq!(parse_go_duration(""), None);
    }

    #[test]
    fn grok_wait_text_is_parsed_in_both_languages() {
        assert_eq!(
            grok_wait_duration_seconds_from_text("wait 6h 13m"),
            Some(22_380)
        );
        assert_eq!(
            grok_wait_duration_seconds_from_text("等待 6小时13分钟"),
            Some(22_380)
        );
        assert_eq!(
            grok_wait_duration_seconds_from_text("no duration here"),
            None
        );
        let hint = extract_upstream_retry_hint(
            "grok",
            429,
            None,
            Some(
                r#"{"error":{"message":"升级到 SuperGrok 获得更高使用上限，或等待 6小时 13分钟。"}}"#,
            ),
            NOW,
        );
        assert_eq!(hint.source, RetryHintSource::GrokText);
        assert_eq!(hint.retry_after, Some(Duration::from_secs(22_380)));
        assert_eq!(hint.scope, RetryHintScope::KeyModel);
    }

    #[test]
    fn no_signal_yields_none() {
        let hint = extract_upstream_retry_hint("codex", 429, None, Some("rate limited"), NOW);
        assert!(hint.is_none());
        assert_eq!(hint.source, RetryHintSource::None);
    }
}
