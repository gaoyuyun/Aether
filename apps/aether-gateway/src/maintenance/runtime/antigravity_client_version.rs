//! Antigravity 客户端版本刷新。
//!
//! Cloud Code 会淘汰过旧的客户端版本，以前版本写死在代码里，上游一淘汰就要改代码
//! 发版。这个 worker 每 6 小时从 Antigravity hub 的自动更新清单取最新版本，写进进程
//! 内的动态版本（见 `aether_provider_transport::antigravity::version`）并持久化到系统
//! 配置 `antigravity.client_version`，让重启后与其他实例都能立即用上上次拿到的值。
//!
//! 版本存在每个进程的内存里，所以每个实例各自刷新，不走单例租约；拉取失败时保留
//! 旧值，请求侧完全不受影响。

use std::time::Duration;

use aether_provider_transport::antigravity::{
    antigravity_client_version_state, parse_antigravity_hub_manifest_version,
    AntigravityClientVersionState, ANTIGRAVITY_CLIENT_VERSION_SYSTEM_CONFIG_KEY,
    ANTIGRAVITY_HUB_LATEST_MANIFEST_URL,
};
use serde_json::Value;
use tracing::{debug, info, warn};

use crate::AppState;

const ANTIGRAVITY_VERSION_REFRESH_INTERVAL: Duration = Duration::from_secs(6 * 60 * 60);
const ANTIGRAVITY_VERSION_STARTUP_DELAY: Duration = Duration::from_secs(5);
const ANTIGRAVITY_VERSION_FETCH_TIMEOUT: Duration = Duration::from_secs(10);
const ANTIGRAVITY_VERSION_MANIFEST_MAX_BYTES: usize = 4096;

pub(crate) fn antigravity_client_version_refresh_enabled() -> bool {
    std::env::var("ANTIGRAVITY_CLIENT_VERSION_REFRESH_ENABLED")
        .ok()
        .map(|value| !value.trim().eq_ignore_ascii_case("false"))
        .unwrap_or(true)
}

pub(crate) fn antigravity_hub_manifest_url() -> String {
    std::env::var("ANTIGRAVITY_HUB_MANIFEST_URL")
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| ANTIGRAVITY_HUB_LATEST_MANIFEST_URL.to_string())
}

pub(crate) fn spawn_antigravity_client_version_refresh_worker(
    state: AppState,
) -> Option<tokio::task::JoinHandle<()>> {
    if !antigravity_client_version_refresh_enabled() {
        info!("antigravity client version refresh disabled; using embedded version");
        return None;
    }

    Some(aether_runtime::task::spawn_named(
        "antigravity-client-version-refresh",
        async move {
            tokio::time::sleep(ANTIGRAVITY_VERSION_STARTUP_DELAY).await;
            perform_antigravity_client_version_refresh_once(&state, "startup").await;

            let mut interval = tokio::time::interval(ANTIGRAVITY_VERSION_REFRESH_INTERVAL);
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            interval.tick().await;
            loop {
                interval.tick().await;
                perform_antigravity_client_version_refresh_once(&state, "tick").await;
            }
        },
    ))
}

/// 一次完整刷新：先用系统配置里持久化的版本对齐进程状态（其他实例可能已经刷新
/// 过），再拉 hub 清单；拿到新版本就写回系统配置。
pub(crate) async fn perform_antigravity_client_version_refresh_once(
    state: &AppState,
    phase: &'static str,
) {
    let version_state = antigravity_client_version_state();
    apply_persisted_antigravity_client_version(state, version_state).await;

    let manifest_url = antigravity_hub_manifest_url();
    let fetched = fetch_antigravity_hub_manifest_version(&manifest_url).await;
    match apply_fetched_antigravity_client_version(version_state, fetched) {
        AntigravityClientVersionRefresh::Updated(version) => {
            info!(
                phase,
                version, "antigravity client version updated from hub manifest"
            );
            persist_antigravity_client_version(state, &version).await;
        }
        AntigravityClientVersionRefresh::Unchanged(version) => {
            debug!(phase, version, "antigravity client version unchanged");
            // 首次启动时系统配置里可能还没有值，补一份，让其他实例与重启后可复用。
            persist_antigravity_client_version(state, &version).await;
        }
        AntigravityClientVersionRefresh::Kept { version, error } => {
            warn!(
                phase,
                version,
                error,
                "antigravity client version refresh failed; keeping current version"
            );
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum AntigravityClientVersionRefresh {
    /// 清单给出了一个不同且合法的版本，已切换。
    Updated(String),
    /// 清单版本与当前一致。
    Unchanged(String),
    /// 拉取或校验失败，沿用当前版本。
    Kept { version: String, error: String },
}

/// 把一次拉取结果套用到版本状态上。拉取失败、清单不合法或版本低于下限都不改变
/// 当前值，这是「失败保留旧值」的唯一实现点。
pub(crate) fn apply_fetched_antigravity_client_version(
    version_state: &AntigravityClientVersionState,
    fetched: Result<String, String>,
) -> AntigravityClientVersionRefresh {
    let candidate = match fetched {
        Ok(candidate) => candidate,
        Err(error) => {
            return AntigravityClientVersionRefresh::Kept {
                version: version_state.current(),
                error,
            }
        }
    };
    match version_state.apply(&candidate) {
        Ok(true) => AntigravityClientVersionRefresh::Updated(version_state.current()),
        Ok(false) => AntigravityClientVersionRefresh::Unchanged(version_state.current()),
        Err(error) => AntigravityClientVersionRefresh::Kept {
            version: version_state.current(),
            error: error.to_string(),
        },
    }
}

async fn apply_persisted_antigravity_client_version(
    state: &AppState,
    version_state: &AntigravityClientVersionState,
) {
    let persisted = match state
        .read_system_config_json_value(ANTIGRAVITY_CLIENT_VERSION_SYSTEM_CONFIG_KEY)
        .await
    {
        Ok(value) => value,
        Err(error) => {
            debug!(
                error = %error.into_message(),
                "persisted antigravity client version unavailable"
            );
            return;
        }
    };
    let Some(candidate) = persisted.as_ref().and_then(system_config_version_string) else {
        return;
    };
    match version_state.apply(&candidate) {
        Ok(true) => info!(
            version = %candidate,
            "antigravity client version restored from system config"
        ),
        Ok(false) => {}
        Err(error) => warn!(
            error = %error,
            "persisted antigravity client version rejected; keeping current version"
        ),
    }
}

fn system_config_version_string(value: &Value) -> Option<String> {
    value
        .as_str()
        .or_else(|| value.get("version").and_then(Value::as_str))
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .map(ToOwned::to_owned)
}

async fn persist_antigravity_client_version(state: &AppState, version: &str) {
    let current = state
        .read_system_config_json_value(ANTIGRAVITY_CLIENT_VERSION_SYSTEM_CONFIG_KEY)
        .await
        .ok()
        .flatten();
    if current
        .as_ref()
        .and_then(system_config_version_string)
        .is_some_and(|persisted| persisted == version)
    {
        return;
    }
    if let Err(error) = state
        .upsert_system_config_json_value(
            ANTIGRAVITY_CLIENT_VERSION_SYSTEM_CONFIG_KEY,
            &Value::String(version.to_string()),
            Some("Antigravity 客户端版本，由后台任务从 hub manifest 刷新"),
        )
        .await
    {
        debug!(
            error = %error.into_message(),
            "antigravity client version could not be persisted to system config"
        );
    }
}

/// 拉取 hub 清单并解析出版本号。只返回字符串，合法性校验交给版本状态。
pub(crate) async fn fetch_antigravity_hub_manifest_version(url: &str) -> Result<String, String> {
    let client = reqwest::Client::builder()
        .timeout(ANTIGRAVITY_VERSION_FETCH_TIMEOUT)
        .user_agent("electron-builder")
        .build()
        .map_err(|error| format!("http client build failed: {error}"))?;
    let response = client
        .get(url)
        .header("cache-control", "no-cache")
        .send()
        .await
        .map_err(|error| format!("manifest fetch failed: {error}"))?;
    let status = response.status();
    if !status.is_success() {
        return Err(format!("manifest returned http {}", status.as_u16()));
    }
    if response
        .content_length()
        .is_some_and(|length| length > ANTIGRAVITY_VERSION_MANIFEST_MAX_BYTES as u64)
    {
        return Err("manifest response too large".to_string());
    }
    let body = response
        .bytes()
        .await
        .map_err(|error| format!("manifest read failed: {error}"))?;
    if body.len() > ANTIGRAVITY_VERSION_MANIFEST_MAX_BYTES {
        return Err("manifest response too large".to_string());
    }
    let text = std::str::from_utf8(&body).map_err(|_| "manifest is not utf-8".to_string())?;
    parse_antigravity_hub_manifest_version(text)
        .ok_or_else(|| "manifest has no version field".to_string())
}

#[cfg(test)]
mod tests {
    use aether_provider_transport::antigravity::{
        AntigravityClientVersionState, ANTIGRAVITY_CLIENT_VERSION,
    };
    use axum::routing::get;
    use axum::Router;
    use http::StatusCode;

    use super::{
        apply_fetched_antigravity_client_version, fetch_antigravity_hub_manifest_version,
        AntigravityClientVersionRefresh,
    };

    #[test]
    fn a_new_manifest_version_replaces_the_current_one() {
        let state = AntigravityClientVersionState::new();
        assert_eq!(
            apply_fetched_antigravity_client_version(&state, Ok("2.15.0".to_string())),
            AntigravityClientVersionRefresh::Updated("2.15.0".to_string())
        );
        assert_eq!(state.current(), "2.15.0");
        assert_eq!(
            apply_fetched_antigravity_client_version(&state, Ok("2.15.0".to_string())),
            AntigravityClientVersionRefresh::Unchanged("2.15.0".to_string())
        );
    }

    #[test]
    fn a_failed_fetch_keeps_the_current_version() {
        let state = AntigravityClientVersionState::new();
        state.apply("3.4.5").expect("valid version");

        let outcome = apply_fetched_antigravity_client_version(
            &state,
            Err("manifest returned http 503".into()),
        );
        assert_eq!(
            outcome,
            AntigravityClientVersionRefresh::Kept {
                version: "3.4.5".to_string(),
                error: "manifest returned http 503".to_string(),
            }
        );
        assert_eq!(state.current(), "3.4.5");
    }

    #[test]
    fn a_manifest_below_the_floor_or_malformed_keeps_the_current_version() {
        let state = AntigravityClientVersionState::new();
        for candidate in ["1.9.9", "2.9.0", "latest", ""] {
            let outcome =
                apply_fetched_antigravity_client_version(&state, Ok(candidate.to_string()));
            assert!(
                matches!(outcome, AntigravityClientVersionRefresh::Kept { ref version, .. } if version == ANTIGRAVITY_CLIENT_VERSION),
                "{candidate:?} -> {outcome:?}"
            );
        }
        assert_eq!(state.current(), ANTIGRAVITY_CLIENT_VERSION);
    }

    #[tokio::test]
    async fn manifest_fetch_parses_the_yaml_version_and_reports_http_failures() {
        let listener = crate::test_support::bind_loopback_listener()
            .await
            .expect("listener should bind");
        let addr = listener.local_addr().expect("address should resolve");
        let server = tokio::spawn(async move {
            let app = Router::new()
                .route(
                    "/manifest/latest-arm64-mac.yml",
                    get(|| async {
                        "version: 2.16.3\nfiles:\n  - url: https://example.invalid/Antigravity.zip\npath: Antigravity.zip\n"
                    }),
                )
                .route(
                    "/manifest/broken.yml",
                    get(|| async { (StatusCode::SERVICE_UNAVAILABLE, "down") }),
                );
            axum::serve(listener, app)
                .await
                .expect("server should start");
        });

        let version = fetch_antigravity_hub_manifest_version(&format!(
            "http://{addr}/manifest/latest-arm64-mac.yml"
        ))
        .await
        .expect("manifest should parse");
        assert_eq!(version, "2.16.3");

        let error =
            fetch_antigravity_hub_manifest_version(&format!("http://{addr}/manifest/broken.yml"))
                .await
                .expect_err("503 should be reported");
        assert_eq!(error, "manifest returned http 503");

        // 失败的拉取套用到版本状态上不会改动当前值：请求侧继续用旧版本。
        let state = AntigravityClientVersionState::new();
        assert!(matches!(
            apply_fetched_antigravity_client_version(&state, Err(error)),
            AntigravityClientVersionRefresh::Kept { .. }
        ));
        assert_eq!(state.current(), ANTIGRAVITY_CLIENT_VERSION);
        server.abort();
    }
}
