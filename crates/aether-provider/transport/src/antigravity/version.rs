//! Antigravity 客户端版本的运行期来源。
//!
//! Cloud Code 会按客户端版本决定放行哪些模型：低于 2.9.0 的客户端拿不到新模型，
//! 太旧的版本还会被直接淘汰。以前版本写死在代码里，上游一淘汰就要改代码发版。
//! 现在版本有三层来源，优先级从高到低：
//!
//! 1. Key 级 `auth_config` / `upstream_metadata` 里的 `client_version`（必须不低于硬下限）；
//! 2. 进程内的动态版本：网关后台任务每 6 小时从 Antigravity hub manifest 拉取，
//!    并持久化到系统配置 `antigravity.client_version`，供重启与其他实例复用；
//! 3. 编译内嵌的默认值 [`ANTIGRAVITY_CLIENT_VERSION`]。
//!
//! 拉取失败时保留旧值；任何来源给出的版本只要低于硬下限或格式不对都不采用。

use std::sync::RwLock;

use super::auth::ANTIGRAVITY_CLIENT_VERSION;

/// 系统配置里持久化动态版本的键。
pub const ANTIGRAVITY_CLIENT_VERSION_SYSTEM_CONFIG_KEY: &str = "antigravity.client_version";
/// Cloud Code 对低于 2.9.0 的客户端拒绝新模型，这个下限必须保持在其之上。
pub const ANTIGRAVITY_MIN_CLIENT_VERSION: &str = "2.9.1";
/// Antigravity hub 自动更新器的最新版本清单（与 CLIProxyAPI 同源）。
pub const ANTIGRAVITY_HUB_LATEST_MANIFEST_URL: &str =
    "https://antigravity-hub-auto-updater-974169037036.us-central1.run.app/manifest/latest-arm64-mac.yml";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AntigravityClientVersionError {
    /// 不是 `major.minor.patch` 三段纯数字。
    Malformed(String),
    /// 低于 [`ANTIGRAVITY_MIN_CLIENT_VERSION`]。
    BelowFloor(String),
}

impl std::fmt::Display for AntigravityClientVersionError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Malformed(version) => {
                write!(formatter, "antigravity client version {version:?} is not major.minor.patch")
            }
            Self::BelowFloor(version) => write!(
                formatter,
                "antigravity client version {version:?} is below the floor {ANTIGRAVITY_MIN_CLIENT_VERSION}"
            ),
        }
    }
}

impl std::error::Error for AntigravityClientVersionError {}

/// 进程内动态版本的存放处。生产用全局单例 [`antigravity_client_version_state`]；
/// 测试可以各自新建实例，避免并行测试互相污染。
#[derive(Debug)]
pub struct AntigravityClientVersionState {
    version: RwLock<String>,
}

impl Default for AntigravityClientVersionState {
    fn default() -> Self {
        Self::new()
    }
}

impl AntigravityClientVersionState {
    pub fn new() -> Self {
        Self {
            version: RwLock::new(ANTIGRAVITY_CLIENT_VERSION.to_string()),
        }
    }

    /// 当前生效的版本。
    pub fn current(&self) -> String {
        self.version
            .read()
            .map(|guard| guard.clone())
            .unwrap_or_else(|poisoned| poisoned.into_inner().clone())
    }

    /// 采用一个新版本。返回值表示是否发生了变化；非法或低于下限的版本被拒绝，
    /// 当前值保持不变。
    pub fn apply(&self, candidate: &str) -> Result<bool, AntigravityClientVersionError> {
        let normalized = validate_antigravity_client_version(candidate)?;
        let mut guard = self
            .version
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if *guard == normalized {
            return Ok(false);
        }
        *guard = normalized;
        Ok(true)
    }
}

pub fn antigravity_client_version_state() -> &'static AntigravityClientVersionState {
    static STATE: std::sync::OnceLock<AntigravityClientVersionState> = std::sync::OnceLock::new();
    STATE.get_or_init(AntigravityClientVersionState::new)
}

/// 进程当前的动态版本。
pub fn antigravity_client_version() -> String {
    antigravity_client_version_state().current()
}

/// 解析并校验版本：三段纯数字且不低于硬下限，返回去掉首尾空白的规范形式。
pub fn validate_antigravity_client_version(
    candidate: &str,
) -> Result<String, AntigravityClientVersionError> {
    let trimmed = candidate.trim();
    let Some(parsed) = parse_antigravity_client_version(trimmed) else {
        return Err(AntigravityClientVersionError::Malformed(
            trimmed.to_string(),
        ));
    };
    let floor = parse_antigravity_client_version(ANTIGRAVITY_MIN_CLIENT_VERSION)
        .expect("ANTIGRAVITY_MIN_CLIENT_VERSION must be a valid version");
    if parsed < floor {
        return Err(AntigravityClientVersionError::BelowFloor(
            trimmed.to_string(),
        ));
    }
    Ok(trimmed.to_string())
}

/// `major.minor.patch` 三段纯数字；其他形状（预发布后缀、两段、空段）一律拒绝，
/// 与 CLIProxyAPI `isValidAntigravitySemVersion` 一致。
pub fn parse_antigravity_client_version(version: &str) -> Option<[u64; 3]> {
    let mut parts = version.trim().split('.');
    let mut parsed = [0u64; 3];
    for slot in &mut parsed {
        let part = parts.next()?;
        if part.is_empty() || !part.bytes().all(|byte| byte.is_ascii_digit()) {
            return None;
        }
        *slot = part.parse().ok()?;
    }
    parts.next().is_none().then_some(parsed)
}

/// 在 Key 级覆盖与进程动态版本之间做选择：覆盖值合法且不低于下限才生效。
pub fn resolve_antigravity_client_version(client_version: Option<&str>) -> String {
    client_version
        .and_then(|candidate| validate_antigravity_client_version(candidate).ok())
        .unwrap_or_else(antigravity_client_version)
}

/// Antigravity 请求 UA 的固定形状，只有版本号随来源变化。
pub fn antigravity_request_user_agent_for_version(version: &str) -> String {
    format!("vscode/1.X.X (Antigravity/{})", version.trim())
}

/// 从 hub 自动更新器的 YAML 清单里取 `version:` 字段。清单只有一层键值，
/// 不引入 YAML 依赖，按行扫描即可。
pub fn parse_antigravity_hub_manifest_version(manifest: &str) -> Option<String> {
    manifest
        .lines()
        .map(|line| line.trim_end_matches('\r'))
        .find_map(|line| {
            let rest = line.strip_prefix("version:")?;
            let value = rest.trim().trim_matches(|character| {
                character == '"' || character == '\'' || character == ' '
            });
            (!value.is_empty()).then(|| value.to_string())
        })
}

#[cfg(test)]
mod tests {
    use super::{
        antigravity_request_user_agent_for_version, parse_antigravity_client_version,
        parse_antigravity_hub_manifest_version, resolve_antigravity_client_version,
        validate_antigravity_client_version, AntigravityClientVersionError,
        AntigravityClientVersionState, ANTIGRAVITY_MIN_CLIENT_VERSION,
    };
    use crate::antigravity::ANTIGRAVITY_CLIENT_VERSION;

    #[test]
    fn parses_only_three_numeric_segments() {
        assert_eq!(parse_antigravity_client_version("2.15.0"), Some([2, 15, 0]));
        assert_eq!(parse_antigravity_client_version(" 4.3.0 "), Some([4, 3, 0]));
        assert_eq!(parse_antigravity_client_version("2.15"), None);
        assert_eq!(parse_antigravity_client_version("2.15.0.1"), None);
        assert_eq!(parse_antigravity_client_version("2.15.0-beta"), None);
        assert_eq!(parse_antigravity_client_version("2..0"), None);
        assert_eq!(parse_antigravity_client_version(""), None);
    }

    #[test]
    fn validation_enforces_the_floor() {
        assert_eq!(
            validate_antigravity_client_version(ANTIGRAVITY_MIN_CLIENT_VERSION).as_deref(),
            Ok(ANTIGRAVITY_MIN_CLIENT_VERSION)
        );
        assert_eq!(
            validate_antigravity_client_version("2.9.0"),
            Err(AntigravityClientVersionError::BelowFloor(
                "2.9.0".to_string()
            ))
        );
        assert_eq!(
            validate_antigravity_client_version("latest"),
            Err(AntigravityClientVersionError::Malformed(
                "latest".to_string()
            ))
        );
        assert_eq!(
            validate_antigravity_client_version("10.0.0").as_deref(),
            Ok("10.0.0")
        );
    }

    #[test]
    fn state_keeps_the_previous_value_when_a_candidate_is_rejected() {
        let state = AntigravityClientVersionState::new();
        assert_eq!(state.current(), ANTIGRAVITY_CLIENT_VERSION);

        assert_eq!(state.apply("5.0.1"), Ok(true));
        assert_eq!(state.current(), "5.0.1");
        assert_eq!(state.apply("5.0.1"), Ok(false));

        assert!(state.apply("garbage").is_err());
        assert!(state.apply("1.0.0").is_err());
        assert_eq!(state.current(), "5.0.1");
    }

    #[test]
    fn key_level_override_only_wins_when_valid() {
        let default_version = resolve_antigravity_client_version(None);
        assert_eq!(resolve_antigravity_client_version(Some("3.0.0")), "3.0.0");
        assert_eq!(
            resolve_antigravity_client_version(Some("1.2.3")),
            default_version
        );
        assert_eq!(
            resolve_antigravity_client_version(Some("not-a-version")),
            default_version
        );
        assert_eq!(
            resolve_antigravity_client_version(Some("")),
            default_version
        );
    }

    #[test]
    fn user_agent_embeds_the_version() {
        assert_eq!(
            antigravity_request_user_agent_for_version("2.15.0"),
            "vscode/1.X.X (Antigravity/2.15.0)"
        );
    }

    #[test]
    fn hub_manifest_version_is_read_from_the_yaml_top_level() {
        let manifest = "version: 2.15.0\nfiles:\n  - url: https://example.invalid/Antigravity.zip\n    version: 9.9.9\npath: Antigravity.zip\n";
        assert_eq!(
            parse_antigravity_hub_manifest_version(manifest).as_deref(),
            Some("2.15.0")
        );
        assert_eq!(
            parse_antigravity_hub_manifest_version("version: \"2.16.1\"\r\n").as_deref(),
            Some("2.16.1")
        );
        assert_eq!(parse_antigravity_hub_manifest_version("files: []\n"), None);
        assert_eq!(parse_antigravity_hub_manifest_version("version:\n"), None);
    }
}
