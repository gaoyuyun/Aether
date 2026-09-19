//! 预设模型目录远程刷新。
//!
//! 目录保存在每个网关进程的内存里，所以这个 worker 在每个实例上各自运行，不走
//! 单例租约。目录发生变化时，对使用这些渠道类型的 Key 触发一次模型缓存重刷，
//! 让管理端和路由立刻看到新模型，而不用等下一次每日模型拉取。

use std::time::Duration;

use aether_model_fetch::{
    apply_remote_preset_model_catalog, preset_model_catalog_refresh_enabled,
    preset_model_catalog_refresh_minutes, preset_model_catalog_urls, PresetModelCatalogUpdate,
};
use tracing::{debug, info, warn};

use crate::AppState;

use super::runtime::perform_model_fetch_for_provider_types;

const PRESET_CATALOG_FETCH_TIMEOUT: Duration = Duration::from_secs(30);
const PRESET_CATALOG_STARTUP_DELAY: Duration = Duration::from_secs(5);
const PRESET_CATALOG_MAX_BODY_BYTES: usize = 4 * 1024 * 1024;

pub(crate) fn spawn_preset_catalog_refresh_worker(
    state: AppState,
) -> Option<tokio::task::JoinHandle<()>> {
    if !preset_model_catalog_refresh_enabled() {
        info!("preset model catalog remote refresh disabled; using embedded catalog");
        return None;
    }
    if preset_model_catalog_urls().is_empty() {
        return None;
    }

    Some(aether_runtime::task::spawn_named(
        "model-fetch-preset-catalog-refresh",
        async move {
            tokio::time::sleep(PRESET_CATALOG_STARTUP_DELAY).await;
            refresh_preset_catalog_once(&state, "startup").await;

            let mut interval = tokio::time::interval(Duration::from_secs(
                preset_model_catalog_refresh_minutes().saturating_mul(60),
            ));
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            interval.tick().await;
            loop {
                interval.tick().await;
                refresh_preset_catalog_once(&state, "tick").await;
            }
        },
    ))
}

async fn refresh_preset_catalog_once(state: &AppState, phase: &'static str) {
    let Some((url, update)) = fetch_and_apply_remote_catalog().await else {
        warn!(
            phase,
            "preset model catalog refresh failed from all urls; keeping current catalog"
        );
        return;
    };

    if !update.ignored_provider_types.is_empty() {
        warn!(
            phase,
            url,
            ignored = ?update.ignored_provider_types,
            "preset model catalog contains unsupported provider types"
        );
    }

    if update.changed_provider_types.is_empty() {
        debug!(phase, url, updated_at = ?update.updated_at, "preset model catalog unchanged");
        return;
    }

    info!(
        phase,
        url,
        updated_at = ?update.updated_at,
        changed = ?update.changed_provider_types,
        "preset model catalog updated"
    );

    match perform_model_fetch_for_provider_types(state, &update.changed_provider_types).await {
        Ok(summary) if summary.attempted > 0 => info!(
            attempted = summary.attempted,
            succeeded = summary.succeeded,
            failed = summary.failed,
            skipped = summary.skipped,
            "refreshed provider model caches after preset catalog update"
        ),
        Ok(_) => {}
        Err(error) => warn!(
            error = %super::safe_model_fetch_error(&error.into_message()),
            "model cache refresh after preset catalog update failed"
        ),
    }
}

async fn fetch_and_apply_remote_catalog() -> Option<(String, PresetModelCatalogUpdate)> {
    let client = match reqwest::Client::builder()
        .timeout(PRESET_CATALOG_FETCH_TIMEOUT)
        .user_agent(concat!("aether-gateway/", env!("CARGO_PKG_VERSION")))
        .build()
    {
        Ok(client) => client,
        Err(error) => {
            warn!(error = %error, "preset model catalog http client build failed");
            return None;
        }
    };

    for url in preset_model_catalog_urls() {
        let text = match fetch_catalog_text(&client, &url).await {
            Ok(text) => text,
            Err(error) => {
                debug!(url, error, "preset model catalog fetch failed");
                continue;
            }
        };
        match apply_remote_preset_model_catalog(&text) {
            Ok(update) => return Some((url, update)),
            Err(error) => warn!(url, error, "preset model catalog rejected"),
        }
    }
    None
}

async fn fetch_catalog_text(client: &reqwest::Client, url: &str) -> Result<String, String> {
    let response = client
        .get(url)
        .send()
        .await
        .map_err(|error| error.to_string())?;
    let status = response.status();
    if !status.is_success() {
        return Err(format!("http {}", status.as_u16()));
    }
    if response
        .content_length()
        .is_some_and(|length| length > PRESET_CATALOG_MAX_BODY_BYTES as u64)
    {
        return Err("response too large".to_string());
    }
    let body = response.bytes().await.map_err(|error| error.to_string())?;
    if body.len() > PRESET_CATALOG_MAX_BODY_BYTES {
        return Err("response too large".to_string());
    }
    String::from_utf8(body.to_vec()).map_err(|_| "response is not utf-8".to_string())
}
