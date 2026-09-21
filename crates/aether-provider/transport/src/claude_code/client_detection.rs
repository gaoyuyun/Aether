//! Claude Code 客户端识别与伪装策略。
//!
//! 综合四个强信号判断进入网关的 `claude:messages` 请求是不是**原生 Claude Code**：
//!
//! 1. `x-app: cli`；
//! 2. `user-agent` 匹配 `claude-cli/<semver> (external, <entrypoint>[, agent-sdk/<ver>])`，
//!    且 entrypoint 属于已核实透传形状的产品面（`cli`、`sdk-cli`、`claude-vscode`）；
//! 3. `anthropic-beta` 含 `claude-code-20250219`；
//! 4. `metadata.user_id` 是 `{"device_id","account_uuid","session_id"}` 形状的 JSON 字符串
//!    （count_tokens 不要求）。
//!
//! 四个信号齐全 → [`ClaudeCodeClientKind::NativeClaudeCode`]；一个都没有 → `Unknown`；
//! 其它 → `ThirdParty`。原生请求完全透传；第三方请求按供应商 `config.cloak.mode`
//! （`auto | always | off`，默认 `auto`）决定是否应用身份/beta/cache_control/CCH 改写。
//!
//! 与 `client_session_affinity.rs` 的家族探测用途不同：那里只为会话亲和，这里决定改写策略。

use std::sync::OnceLock;

use regex::Regex;
use serde_json::Value;

pub const CLAUDE_CODE_BETA: &str = "claude-code-20250219";
pub const CLAUDE_CODE_CLOAK_CONFIG_KEY: &str = "cloak";
pub const CLAUDE_CODE_CLOAK_MODE_CONFIG_KEY: &str = "mode";

/// 已核实 2.1.x 透传形状的原生入口点；其它第一方形状（sdk-ts、sdk-py 等）在抓包核实前
/// 一律按第三方处理。
const NATIVE_ENTRYPOINTS: &[&str] = &["cli", "sdk-cli", "claude-vscode"];

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClaudeCodeClientKind {
    NativeClaudeCode,
    ThirdParty,
    Unknown,
}

impl ClaudeCodeClientKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::NativeClaudeCode => "native_claude_code",
            Self::ThirdParty => "third_party",
            Self::Unknown => "unknown",
        }
    }

    pub const fn is_native(self) -> bool {
        matches!(self, Self::NativeClaudeCode)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClaudeCodeCloakMode {
    #[default]
    Auto,
    Always,
    Off,
}

impl ClaudeCodeCloakMode {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Always => "always",
            Self::Off => "off",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "auto" => Some(Self::Auto),
            "always" => Some(Self::Always),
            "off" | "never" | "disabled" => Some(Self::Off),
            _ => None,
        }
    }
}

/// 识别结果，写入 report_context 便于排障核对（计划 §8）。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct ClaudeCodeClientDetection {
    pub kind: ClaudeCodeClientKind,
    pub x_app_cli: bool,
    pub user_agent_native: bool,
    pub claude_code_beta: bool,
    pub metadata_user_id_native: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub entrypoint: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cli_version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent_sdk_version: Option<String>,
}

impl ClaudeCodeClientDetection {
    pub fn to_json(&self) -> Value {
        serde_json::to_value(self).unwrap_or(Value::Null)
    }

    fn signal_count(&self) -> usize {
        [
            self.x_app_cli,
            self.user_agent_native,
            self.claude_code_beta,
            self.metadata_user_id_native,
        ]
        .into_iter()
        .filter(|signal| *signal)
        .count()
    }
}

/// 识别输入：网关收到的**原始**客户端头与请求体。
pub fn detect_claude_code_client(
    headers: &http::HeaderMap,
    body: Option<&Value>,
    count_tokens: bool,
) -> ClaudeCodeClientDetection {
    let user_agent = header_value(headers, "user-agent");
    let parsed_user_agent = parse_claude_code_user_agent(user_agent.as_deref().unwrap_or(""));
    let x_app_cli = header_value(headers, "x-app").is_some_and(|value| value.trim() == "cli");
    let claude_code_beta = header_has_claude_code_beta(headers);
    let metadata_user_id_native = body
        .and_then(|body| body.get("metadata"))
        .and_then(|metadata| metadata.get("user_id"))
        .and_then(Value::as_str)
        .is_some_and(claude_code_metadata_user_id_is_native);
    let user_agent_native = parsed_user_agent
        .as_ref()
        .is_some_and(|parsed| parsed.native_entrypoint);

    let mut detection = ClaudeCodeClientDetection {
        kind: ClaudeCodeClientKind::Unknown,
        x_app_cli,
        user_agent_native,
        claude_code_beta,
        metadata_user_id_native,
        entrypoint: parsed_user_agent.as_ref().map(|p| p.entrypoint.clone()),
        cli_version: parsed_user_agent.as_ref().map(|p| p.version.clone()),
        agent_sdk_version: parsed_user_agent
            .as_ref()
            .and_then(|p| p.agent_sdk_version.clone()),
    };
    let strong = x_app_cli
        && user_agent_native
        && claude_code_beta
        && (count_tokens || metadata_user_id_native);
    detection.kind = if strong {
        ClaudeCodeClientKind::NativeClaudeCode
    } else if detection.signal_count() == 0 && user_agent.is_none_or(|ua| !looks_like_claude(&ua)) {
        ClaudeCodeClientKind::Unknown
    } else {
        ClaudeCodeClientKind::ThirdParty
    };
    detection
}

fn looks_like_claude(user_agent: &str) -> bool {
    let lower = user_agent.to_ascii_lowercase();
    lower.contains("claude")
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ParsedClaudeCodeUserAgent {
    version: String,
    entrypoint: String,
    agent_sdk_version: Option<String>,
    native_entrypoint: bool,
}

fn claude_code_user_agent_regex() -> &'static Regex {
    static REGEX: OnceLock<Regex> = OnceLock::new();
    REGEX.get_or_init(|| {
        Regex::new(
            r"(?i)^claude-cli/([0-9]+\.[0-9]+\.[0-9]+)\s+\(external,\s*([^,)]+)(?:,\s*agent-sdk/([0-9]+\.[0-9]+\.[0-9]+))?\)$",
        )
        .expect("claude code user agent regex must compile")
    })
}

fn parse_claude_code_user_agent(user_agent: &str) -> Option<ParsedClaudeCodeUserAgent> {
    let captures = claude_code_user_agent_regex().captures(user_agent.trim())?;
    let version = captures.get(1)?.as_str().to_string();
    let entrypoint = captures.get(2)?.as_str().trim().to_ascii_lowercase();
    let agent_sdk_version = captures.get(3).map(|m| m.as_str().to_string());
    let native_entrypoint = NATIVE_ENTRYPOINTS.contains(&entrypoint.as_str());
    Some(ParsedClaudeCodeUserAgent {
        version,
        entrypoint,
        agent_sdk_version,
        native_entrypoint,
    })
}

fn header_value(headers: &http::HeaderMap, name: &str) -> Option<String> {
    headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
}

fn header_has_claude_code_beta(headers: &http::HeaderMap) -> bool {
    headers
        .get_all("anthropic-beta")
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(','))
        .any(|token| token.trim() == CLAUDE_CODE_BETA)
}

/// 原生 `metadata.user_id`：JSON 对象字符串，`device_id` 为 64 位小写十六进制，
/// `session_id` 是 UUID，`account_uuid` 为空或 UUID。
pub fn claude_code_metadata_user_id_is_native(user_id: &str) -> bool {
    let Ok(value) = serde_json::from_str::<Value>(user_id.trim()) else {
        return false;
    };
    let Some(object) = value.as_object() else {
        return false;
    };
    let device_id = object
        .get("device_id")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if !is_lower_hex_64(device_id) {
        return false;
    }
    let session_id = object
        .get("session_id")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if uuid::Uuid::parse_str(session_id).is_err() {
        return false;
    }
    let account_uuid = object
        .get("account_uuid")
        .and_then(Value::as_str)
        .unwrap_or_default();
    account_uuid.is_empty() || uuid::Uuid::parse_str(account_uuid).is_ok()
}

fn is_lower_hex_64(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

/// 从供应商 `config` 读取 `cloak.mode`；缺省 `auto`。无法解析的值按 `auto` 并由调用方记警告。
pub fn resolve_claude_code_cloak_mode(provider_config: Option<&Value>) -> ClaudeCodeCloakMode {
    provider_config
        .and_then(Value::as_object)
        .and_then(|config| config.get(CLAUDE_CODE_CLOAK_CONFIG_KEY))
        .and_then(Value::as_object)
        .and_then(|cloak| cloak.get(CLAUDE_CODE_CLOAK_MODE_CONFIG_KEY))
        .and_then(Value::as_str)
        .and_then(ClaudeCodeCloakMode::parse)
        .unwrap_or_default()
}

/// 策略判定：是否对这条请求应用伪装改写。
///
/// - 原生 Claude Code：永不改写（`always` 也不覆盖强指纹）；
/// - `off`：不改写；
/// - `auto` / `always`：第三方与未知客户端都改写。
pub const fn claude_code_cloak_applies(
    kind: ClaudeCodeClientKind,
    mode: ClaudeCodeCloakMode,
) -> bool {
    if kind.is_native() {
        return false;
    }
    !matches!(mode, ClaudeCodeCloakMode::Off)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn native_headers() -> http::HeaderMap {
        let mut headers = http::HeaderMap::new();
        headers.insert("x-app", "cli".parse().unwrap());
        headers.insert(
            "user-agent",
            "claude-cli/2.1.161 (external, cli)".parse().unwrap(),
        );
        headers.insert(
            "anthropic-beta",
            "claude-code-20250219,oauth-2025-04-20".parse().unwrap(),
        );
        headers
    }

    fn native_body() -> Value {
        json!({
            "model": "claude-opus-4-6",
            "metadata": {
                "user_id": "{\"device_id\":\"0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef\",\"account_uuid\":\"6f1c7d8e-1b2c-4d3e-8f90-123456789abc\",\"session_id\":\"9a0b1c2d-3e4f-4a5b-8c6d-7e8f90a1b2c3\"}"
            },
            "messages": []
        })
    }

    #[test]
    fn all_four_signals_confirm_native_claude_code() {
        let detection = detect_claude_code_client(&native_headers(), Some(&native_body()), false);
        assert_eq!(detection.kind, ClaudeCodeClientKind::NativeClaudeCode);
        assert_eq!(detection.entrypoint.as_deref(), Some("cli"));
        assert_eq!(detection.cli_version.as_deref(), Some("2.1.161"));
        assert!(detection.metadata_user_id_native);
    }

    #[test]
    fn count_tokens_does_not_require_metadata_user_id() {
        let body = json!({"model": "claude-opus-4-6", "messages": []});
        let detection = detect_claude_code_client(&native_headers(), Some(&body), true);
        assert_eq!(detection.kind, ClaudeCodeClientKind::NativeClaudeCode);
        let messages = detect_claude_code_client(&native_headers(), Some(&body), false);
        assert_eq!(messages.kind, ClaudeCodeClientKind::ThirdParty);
    }

    #[test]
    fn copied_user_agent_without_other_signals_is_third_party() {
        let mut headers = http::HeaderMap::new();
        headers.insert(
            "user-agent",
            "claude-cli/2.1.161 (external, cli)".parse().unwrap(),
        );
        let detection = detect_claude_code_client(&headers, Some(&json!({})), false);
        assert_eq!(detection.kind, ClaudeCodeClientKind::ThirdParty);
        assert!(detection.user_agent_native);
        assert!(!detection.x_app_cli);
    }

    #[test]
    fn unverified_entrypoints_are_not_native() {
        let mut headers = native_headers();
        headers.insert(
            "user-agent",
            "claude-cli/2.1.161 (external, sdk-ts, agent-sdk/0.1.9)"
                .parse()
                .unwrap(),
        );
        let detection = detect_claude_code_client(&headers, Some(&native_body()), false);
        assert_eq!(detection.kind, ClaudeCodeClientKind::ThirdParty);
        assert_eq!(detection.entrypoint.as_deref(), Some("sdk-ts"));
        assert_eq!(detection.agent_sdk_version.as_deref(), Some("0.1.9"));
    }

    #[test]
    fn plain_sdk_request_is_unknown() {
        let mut headers = http::HeaderMap::new();
        headers.insert("user-agent", "anthropic-sdk-python/0.40".parse().unwrap());
        let detection = detect_claude_code_client(
            &headers,
            Some(&json!({"metadata": {"user_id": "user-1"}})),
            false,
        );
        assert_eq!(detection.kind, ClaudeCodeClientKind::Unknown);
        assert_eq!(detection.entrypoint, None);
    }

    #[test]
    fn metadata_user_id_shape_is_validated() {
        assert!(claude_code_metadata_user_id_is_native(
            "{\"device_id\":\"0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef\",\"account_uuid\":\"\",\"session_id\":\"9a0b1c2d-3e4f-4a5b-8c6d-7e8f90a1b2c3\"}"
        ));
        assert!(!claude_code_metadata_user_id_is_native(
            "user_abc_session_xyz"
        ));
        assert!(!claude_code_metadata_user_id_is_native(
            "{\"device_id\":\"ABCDEF\",\"session_id\":\"9a0b1c2d-3e4f-4a5b-8c6d-7e8f90a1b2c3\"}"
        ));
    }

    #[test]
    fn cloak_mode_parsing_and_policy() {
        assert_eq!(
            resolve_claude_code_cloak_mode(Some(&json!({"cloak": {"mode": "always"}}))),
            ClaudeCodeCloakMode::Always
        );
        assert_eq!(
            resolve_claude_code_cloak_mode(Some(&json!({"cloak": {"mode": "never"}}))),
            ClaudeCodeCloakMode::Off
        );
        assert_eq!(
            resolve_claude_code_cloak_mode(Some(&json!({"cloak": {"mode": "bogus"}}))),
            ClaudeCodeCloakMode::Auto
        );
        assert_eq!(
            resolve_claude_code_cloak_mode(None),
            ClaudeCodeCloakMode::Auto
        );

        assert!(!claude_code_cloak_applies(
            ClaudeCodeClientKind::NativeClaudeCode,
            ClaudeCodeCloakMode::Always
        ));
        assert!(claude_code_cloak_applies(
            ClaudeCodeClientKind::ThirdParty,
            ClaudeCodeCloakMode::Auto
        ));
        assert!(claude_code_cloak_applies(
            ClaudeCodeClientKind::Unknown,
            ClaudeCodeCloakMode::Always
        ));
        assert!(!claude_code_cloak_applies(
            ClaudeCodeClientKind::ThirdParty,
            ClaudeCodeCloakMode::Off
        ));
    }
}
