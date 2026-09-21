//! `metadata.user_id` 与设备 profile（计划 2.3）。
//!
//! 原生 Claude Code 的 `metadata.user_id` 是一个 JSON 字符串：
//! `{"device_id":"<64 位小写十六进制>","account_uuid":"<uuid>","session_id":"<uuid>"[,...extras]}`。
//! 第三方请求改写时：
//!
//! - `device_id` 由 Key 派生（`sha256("aether:claude_code:device:" + key_id + ":" + seed)`），
//!   一把 Key 对应一台"设备"，持久化后 7 天内只升不降：软件版本元组（CLI 版本、SDK 包版本、
//!   运行时版本）只接受更新的候选，不会被更旧的客户端头拉回去；
//! - `account_uuid` 取 OAuth 账号的 `account_uuid`，缺失时由 Key 派生一个稳定 UUID v5；
//! - `session_id` 取客户端会话 scope，缺失时由客户端原 `user_id` 派生，保证同一会话稳定；
//! - 原 `user_id` 里三个标准键之外的成员按原顺序保留在后面。

use std::cmp::Ordering;

use serde_json::{Map, Value};
use sha2::{Digest, Sha256};
use uuid::Uuid;

pub const CLAUDE_CODE_DEVICE_PROFILE_KEY: &str = "device_profile";
pub const CLAUDE_CODE_DEVICE_PROFILE_TTL_SECS: u64 = 7 * 24 * 60 * 60;

/// 持久化的设备 profile。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ClaudeCodeDeviceProfile {
    pub device_id: String,
    pub cli_version: String,
    pub package_version: String,
    pub runtime_version: String,
    pub os: String,
    pub arch: String,
    pub created_at_unix_secs: u64,
    pub updated_at_unix_secs: u64,
}

impl ClaudeCodeDeviceProfile {
    pub fn from_value(value: &Value) -> Option<Self> {
        let object = value.as_object()?;
        let device_id = object.get("device_id")?.as_str()?.trim().to_string();
        if !is_lower_hex_64(&device_id) {
            return None;
        }
        Some(Self {
            device_id,
            cli_version: string_field(object, "cli_version"),
            package_version: string_field(object, "package_version"),
            runtime_version: string_field(object, "runtime_version"),
            os: string_field(object, "os"),
            arch: string_field(object, "arch"),
            created_at_unix_secs: u64_field(object, "created_at_unix_secs"),
            updated_at_unix_secs: u64_field(object, "updated_at_unix_secs"),
        })
    }

    pub fn to_value(&self) -> Value {
        serde_json::to_value(self).unwrap_or(Value::Null)
    }

    /// 管理端展示用摘要（不泄露完整 device_id）。
    pub fn summary_value(&self) -> Value {
        let prefix = self.device_id.chars().take(12).collect::<String>();
        serde_json::json!({
            "device_id_prefix": prefix,
            "cli_version": self.cli_version,
            "package_version": self.package_version,
            "runtime_version": self.runtime_version,
            "os": self.os,
            "arch": self.arch,
            "created_at_unix_secs": self.created_at_unix_secs,
            "updated_at_unix_secs": self.updated_at_unix_secs,
        })
    }

    fn expired(&self, now_unix_secs: u64) -> bool {
        now_unix_secs.saturating_sub(self.updated_at_unix_secs)
            > CLAUDE_CODE_DEVICE_PROFILE_TTL_SECS
    }
}

/// 客户端请求头里能读到的软件元组候选。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ClaudeCodeDeviceCandidate {
    pub cli_version: Option<String>,
    pub package_version: Option<String>,
    pub runtime_version: Option<String>,
    pub os: Option<String>,
    pub arch: Option<String>,
}

impl ClaudeCodeDeviceCandidate {
    pub fn from_headers(headers: &http::HeaderMap) -> Self {
        let get = |name: &str| {
            headers
                .get(name)
                .and_then(|value| value.to_str().ok())
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(ToOwned::to_owned)
        };
        let cli_version = get("user-agent").and_then(|ua| parse_cli_version(&ua));
        Self {
            cli_version,
            package_version: get("x-stainless-package-version").filter(|v| is_semver(v)),
            runtime_version: get("x-stainless-runtime-version").filter(|v| is_node_version(v)),
            os: get("x-stainless-os"),
            arch: get("x-stainless-arch"),
        }
    }
}

/// 基线：网关自己的身份 profile。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClaudeCodeDeviceBaseline {
    pub cli_version: String,
    pub package_version: String,
    pub runtime_version: String,
    pub os: String,
    pub arch: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClaudeCodeDeviceResolution {
    pub profile: ClaudeCodeDeviceProfile,
    /// 与传入的持久化值不同，调用方应写回。
    pub changed: bool,
}

/// 解析本次请求应使用的设备 profile。
///
/// `stored` 是已持久化的 profile；`candidate` 来自客户端头；`seed` 参与 device_id 派生
/// （通常是 Key ID），`key_id` 用于 device_id 命名空间。
pub fn resolve_claude_code_device_profile(
    stored: Option<&ClaudeCodeDeviceProfile>,
    candidate: &ClaudeCodeDeviceCandidate,
    baseline: &ClaudeCodeDeviceBaseline,
    key_id: &str,
    seed: &str,
    now_unix_secs: u64,
) -> ClaudeCodeDeviceResolution {
    // 平台钉在基线上：不同 OS/Arch 的第三方客户端不该让同一把 Key 的"设备"漂移。
    let tuple_from_candidate = |candidate: &ClaudeCodeDeviceCandidate| {
        (
            candidate
                .cli_version
                .clone()
                .unwrap_or_else(|| baseline.cli_version.clone()),
            candidate
                .package_version
                .clone()
                .unwrap_or_else(|| baseline.package_version.clone()),
            candidate
                .runtime_version
                .clone()
                .unwrap_or_else(|| baseline.runtime_version.clone()),
        )
    };
    let candidate_meets_baseline = candidate
        .cli_version
        .as_deref()
        .is_some_and(|version| compare_semver(version, &baseline.cli_version) != Ordering::Less);

    match stored {
        Some(stored) if !stored.expired(now_unix_secs) => {
            let mut next = stored.clone();
            if candidate_meets_baseline {
                let (cli, package, runtime) = tuple_from_candidate(candidate);
                if compare_semver(&cli, &stored.cli_version) == Ordering::Greater {
                    next.cli_version = cli;
                    next.package_version = package;
                    next.runtime_version = runtime;
                }
            }
            if compare_semver(&next.cli_version, &baseline.cli_version) == Ordering::Less {
                next.cli_version = baseline.cli_version.clone();
                next.package_version = baseline.package_version.clone();
                next.runtime_version = baseline.runtime_version.clone();
            }
            next.os = baseline.os.clone();
            next.arch = baseline.arch.clone();
            let changed = next != *stored;
            if changed {
                next.updated_at_unix_secs = now_unix_secs;
            }
            ClaudeCodeDeviceResolution {
                profile: next,
                changed,
            }
        }
        stored => {
            let device_id = stored
                .map(|profile| profile.device_id.clone())
                .unwrap_or_else(|| derive_claude_code_device_id(key_id, seed));
            let (cli, package, runtime) = if candidate_meets_baseline {
                tuple_from_candidate(candidate)
            } else {
                (
                    baseline.cli_version.clone(),
                    baseline.package_version.clone(),
                    baseline.runtime_version.clone(),
                )
            };
            ClaudeCodeDeviceResolution {
                profile: ClaudeCodeDeviceProfile {
                    device_id,
                    cli_version: cli,
                    package_version: package,
                    runtime_version: runtime,
                    os: baseline.os.clone(),
                    arch: baseline.arch.clone(),
                    created_at_unix_secs: stored
                        .map(|profile| profile.created_at_unix_secs)
                        .filter(|value| *value > 0)
                        .unwrap_or(now_unix_secs),
                    updated_at_unix_secs: now_unix_secs,
                },
                changed: true,
            }
        }
    }
}

pub fn derive_claude_code_device_id(key_id: &str, seed: &str) -> String {
    let digest = Sha256::digest(format!("aether:claude_code:device:{key_id}:{seed}").as_bytes());
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

pub fn derive_claude_code_account_uuid(key_id: &str) -> String {
    Uuid::new_v5(
        &Uuid::NAMESPACE_URL,
        format!("aether:claude_code:account:{key_id}").as_bytes(),
    )
    .to_string()
}

pub fn derive_claude_code_session_id(seed: &str) -> String {
    Uuid::new_v5(
        &Uuid::NAMESPACE_URL,
        format!("aether:claude_code:session:{seed}").as_bytes(),
    )
    .to_string()
}

/// 身份改写的输入。
#[derive(Debug, Clone, Copy)]
pub struct ClaudeCodeIdentityInput<'a> {
    pub device_id: &'a str,
    pub account_uuid: &'a str,
    /// 客户端会话；`None` 时由原 `user_id`（或请求体哈希）派生。
    pub session_id: Option<&'a str>,
}

/// 把 `metadata.user_id` 重建为原生 JSON 形状；返回最终 session_id。
pub fn apply_claude_code_metadata_user_id(
    body: &mut Value,
    identity: ClaudeCodeIdentityInput<'_>,
) -> Option<String> {
    let object = body.as_object_mut()?;
    let existing = object
        .get("metadata")
        .and_then(Value::as_object)
        .and_then(|metadata| metadata.get("user_id"))
        .and_then(Value::as_str)
        .map(ToOwned::to_owned);
    let session_id = identity
        .session_id
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(|value| {
            Uuid::parse_str(value)
                .map(|uuid| uuid.to_string())
                .unwrap_or_else(|_| derive_claude_code_session_id(value))
        })
        .unwrap_or_else(|| {
            derive_claude_code_session_id(existing.as_deref().unwrap_or("anonymous"))
        });
    let rebuilt = rebuild_claude_code_user_id(
        existing.as_deref(),
        identity.device_id,
        identity.account_uuid,
        &session_id,
    );
    let metadata = object
        .entry("metadata".to_string())
        .or_insert_with(|| Value::Object(Map::new()));
    if !metadata.is_object() {
        *metadata = Value::Object(Map::new());
    }
    metadata
        .as_object_mut()?
        .insert("user_id".to_string(), Value::String(rebuilt));
    Some(session_id)
}

fn rebuild_claude_code_user_id(
    existing: Option<&str>,
    device_id: &str,
    account_uuid: &str,
    session_id: &str,
) -> String {
    let mut output = Map::new();
    output.insert(
        "device_id".to_string(),
        Value::String(device_id.to_string()),
    );
    output.insert(
        "account_uuid".to_string(),
        Value::String(account_uuid.to_string()),
    );
    output.insert(
        "session_id".to_string(),
        Value::String(session_id.to_string()),
    );
    if let Some(Value::Object(extras)) =
        existing.and_then(|raw| serde_json::from_str(raw.trim()).ok())
    {
        for (key, value) in extras {
            if matches!(key.as_str(), "device_id" | "account_uuid" | "session_id") {
                continue;
            }
            output.insert(key, value);
        }
    }
    serde_json::to_string(&Value::Object(output)).unwrap_or_default()
}

fn string_field(object: &Map<String, Value>, key: &str) -> String {
    object
        .get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .unwrap_or_default()
        .to_string()
}

fn u64_field(object: &Map<String, Value>, key: &str) -> u64 {
    object.get(key).and_then(Value::as_u64).unwrap_or(0)
}

fn is_lower_hex_64(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn parse_cli_version(user_agent: &str) -> Option<String> {
    let rest = user_agent.trim().strip_prefix("claude-cli/")?;
    let version = rest
        .split(|c: char| c.is_whitespace() || c == '(')
        .next()?
        .trim();
    is_semver(version).then(|| version.to_string())
}

fn is_semver(value: &str) -> bool {
    let parts = value.split('.').collect::<Vec<_>>();
    parts.len() == 3
        && parts
            .iter()
            .all(|part| !part.is_empty() && part.bytes().all(|b| b.is_ascii_digit()))
}

fn is_node_version(value: &str) -> bool {
    value.strip_prefix('v').is_some_and(is_semver)
}

pub fn compare_semver(left: &str, right: &str) -> Ordering {
    let parse = |value: &str| -> [u64; 3] {
        let mut out = [0u64; 3];
        for (index, part) in value
            .trim()
            .trim_start_matches('v')
            .split('.')
            .take(3)
            .enumerate()
        {
            out[index] = part
                .chars()
                .take_while(|c| c.is_ascii_digit())
                .collect::<String>()
                .parse()
                .unwrap_or(0);
        }
        out
    };
    parse(left).cmp(&parse(right))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn baseline() -> ClaudeCodeDeviceBaseline {
        ClaudeCodeDeviceBaseline {
            cli_version: "2.1.161".into(),
            package_version: "0.94.0".into(),
            runtime_version: "v24.3.0".into(),
            os: "Linux".into(),
            arch: "arm64".into(),
        }
    }

    #[test]
    fn first_resolution_derives_a_stable_device_id_from_the_key() {
        let resolution = resolve_claude_code_device_profile(
            None,
            &ClaudeCodeDeviceCandidate::default(),
            &baseline(),
            "key-1",
            "seed",
            1_000,
        );
        assert!(resolution.changed);
        assert_eq!(
            resolution.profile.device_id,
            derive_claude_code_device_id("key-1", "seed")
        );
        assert_eq!(resolution.profile.device_id.len(), 64);
        assert_eq!(resolution.profile.cli_version, "2.1.161");
        assert_eq!(resolution.profile.created_at_unix_secs, 1_000);
    }

    #[test]
    fn profile_only_upgrades_within_seven_days() {
        let mut headers = http::HeaderMap::new();
        headers.insert(
            "user-agent",
            "claude-cli/2.1.200 (external, cli)".parse().unwrap(),
        );
        headers.insert("x-stainless-package-version", "0.99.0".parse().unwrap());
        headers.insert("x-stainless-runtime-version", "v24.9.0".parse().unwrap());
        headers.insert("x-stainless-os", "Windows".parse().unwrap());
        let newer = ClaudeCodeDeviceCandidate::from_headers(&headers);
        let first =
            resolve_claude_code_device_profile(None, &newer, &baseline(), "k", "s", 1_000).profile;
        assert_eq!(first.cli_version, "2.1.200");
        assert_eq!(first.package_version, "0.99.0");
        assert_eq!(first.os, "Linux", "platform pinned to baseline");

        let mut older_headers = http::HeaderMap::new();
        older_headers.insert(
            "user-agent",
            "claude-cli/2.1.170 (external, cli)".parse().unwrap(),
        );
        let older = ClaudeCodeDeviceCandidate::from_headers(&older_headers);
        let second =
            resolve_claude_code_device_profile(Some(&first), &older, &baseline(), "k", "s", 2_000);
        assert!(
            !second.changed,
            "older client must not downgrade the profile"
        );
        assert_eq!(second.profile.cli_version, "2.1.200");

        let expired_now = 1_000 + CLAUDE_CODE_DEVICE_PROFILE_TTL_SECS + 1;
        let third = resolve_claude_code_device_profile(
            Some(&first),
            &older,
            &baseline(),
            "k",
            "s",
            expired_now,
        );
        assert!(third.changed);
        assert_eq!(
            third.profile.cli_version, "2.1.170",
            "expired profile accepts the candidate"
        );
        assert_eq!(
            third.profile.device_id, first.device_id,
            "device id is never regenerated"
        );
        assert_eq!(third.profile.created_at_unix_secs, 1_000);
    }

    #[test]
    fn candidate_below_baseline_is_ignored() {
        let mut headers = http::HeaderMap::new();
        headers.insert(
            "user-agent",
            "claude-cli/1.0.0 (external, cli)".parse().unwrap(),
        );
        let candidate = ClaudeCodeDeviceCandidate::from_headers(&headers);
        let resolution =
            resolve_claude_code_device_profile(None, &candidate, &baseline(), "k", "s", 1);
        assert_eq!(resolution.profile.cli_version, "2.1.161");
    }

    #[test]
    fn user_id_is_rebuilt_in_native_shape_preserving_extras() {
        let mut body = json!({
            "metadata": {"user_id": "{\"session_id\":\"old\",\"parent_session_id\":\"p1\",\"device_id\":\"x\"}"}
        });
        let session = apply_claude_code_metadata_user_id(
            &mut body,
            ClaudeCodeIdentityInput {
                device_id: "d",
                account_uuid: "a",
                session_id: Some("9a0b1c2d-3e4f-4a5b-8c6d-7e8f90a1b2c3"),
            },
        )
        .expect("session");
        assert_eq!(session, "9a0b1c2d-3e4f-4a5b-8c6d-7e8f90a1b2c3");
        assert_eq!(
            body["metadata"]["user_id"],
            "{\"device_id\":\"d\",\"account_uuid\":\"a\",\"session_id\":\"9a0b1c2d-3e4f-4a5b-8c6d-7e8f90a1b2c3\",\"parent_session_id\":\"p1\"}"
        );
    }

    #[test]
    fn missing_session_is_derived_deterministically_from_existing_user_id() {
        let mut first = json!({"metadata": {"user_id": "user_abc_session_xyz"}});
        let mut second = json!({"metadata": {"user_id": "user_abc_session_xyz"}});
        let identity = ClaudeCodeIdentityInput {
            device_id: "d",
            account_uuid: "a",
            session_id: None,
        };
        let a = apply_claude_code_metadata_user_id(&mut first, identity).unwrap();
        let b = apply_claude_code_metadata_user_id(&mut second, identity).unwrap();
        assert_eq!(a, b);
        assert!(Uuid::parse_str(&a).is_ok());
        let mut none = json!({"messages": []});
        let c = apply_claude_code_metadata_user_id(&mut none, identity).unwrap();
        assert!(Uuid::parse_str(&c).is_ok());
        assert!(none["metadata"]["user_id"]
            .as_str()
            .unwrap()
            .contains("\"device_id\":\"d\""));
    }

    #[test]
    fn device_profile_round_trips_through_json() {
        let profile = resolve_claude_code_device_profile(
            None,
            &ClaudeCodeDeviceCandidate::default(),
            &baseline(),
            "key-1",
            "seed",
            42,
        )
        .profile;
        let parsed = ClaudeCodeDeviceProfile::from_value(&profile.to_value()).expect("parse");
        assert_eq!(parsed, profile);
        assert!(ClaudeCodeDeviceProfile::from_value(&json!({"device_id": "short"})).is_none());
        assert_eq!(
            profile.summary_value()["device_id_prefix"]
                .as_str()
                .unwrap()
                .len(),
            12
        );
    }
}
