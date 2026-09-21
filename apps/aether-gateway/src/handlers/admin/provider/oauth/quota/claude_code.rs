use crate::handlers::admin::request::AdminAppState;
use crate::handlers::shared::sync_provider_key_quota_status_snapshot;
use crate::GatewayError;
use aether_contracts::ProxySnapshot;
use aether_data_contracts::repository::provider_catalog::{
    StoredProviderCatalogEndpoint, StoredProviderCatalogKey, StoredProviderCatalogProvider,
};
use serde_json::json;

/// Claude Code 的额度刷新是**被动**的：Anthropic 没有稳定可用的账号额度查询接口，
/// 5h / 7d 窗口来自每次请求响应头 `anthropic-ratelimit-unified-*`（P1 采集，写入
/// `upstream_metadata.claude_code`）。「刷新」只把已采集的元数据重新物化成状态快照，
/// 不向上游发任何请求。
pub(crate) async fn refresh_claude_code_provider_quota_locally(
    state: &AdminAppState<'_>,
    provider: &StoredProviderCatalogProvider,
    _endpoint: &StoredProviderCatalogEndpoint,
    keys: Vec<StoredProviderCatalogKey>,
    _proxy_override: Option<ProxySnapshot>,
) -> Result<Option<serde_json::Value>, GatewayError> {
    let mut results = Vec::with_capacity(keys.len());
    let mut success_count = 0usize;
    let mut no_metadata_count = 0usize;
    for key in keys {
        let has_metadata = key
            .upstream_metadata
            .as_ref()
            .and_then(serde_json::Value::as_object)
            .and_then(|metadata| metadata.get("claude_code"))
            .and_then(serde_json::Value::as_object)
            .is_some_and(|bucket| !bucket.is_empty());
        if !has_metadata {
            no_metadata_count += 1;
            results.push(json!({
                "key_id": key.id,
                "key_name": key.name,
                "status": "no_metadata",
                "message": "Claude Code 额度为被动采集：该 Key 尚未观察到带 anthropic-ratelimit-unified-* 头的响应",
            }));
            continue;
        }
        let snapshot = sync_provider_key_quota_status_snapshot(
            key.status_snapshot.as_ref(),
            provider.provider_type.as_str(),
            key.upstream_metadata.as_ref(),
            "passive_refresh",
        );
        let quota_snapshot = snapshot
            .as_ref()
            .and_then(|snapshot| snapshot.get("quota"))
            .cloned();
        if let Some(snapshot) = snapshot.as_ref() {
            if let Some(quota_patch) = snapshot.get("quota").cloned() {
                state
                    .app()
                    .update_provider_catalog_key_status_snapshot(
                        &aether_data_contracts::repository::provider_catalog::ProviderCatalogKeyStatusSnapshotUpdate {
                            key_id: key.id.clone(),
                            status_snapshot_patch: json!({"quota": quota_patch}),
                            updated_at_unix_secs: Some(crate::clock::current_unix_secs()),
                        },
                    )
                    .await?;
            }
        }
        success_count += 1;
        let mut item = json!({
            "key_id": key.id,
            "key_name": key.name,
            "status": "success",
            "message": "已根据最近一次响应头刷新 5h / 7d 窗口（被动采集，无上游请求）",
        });
        if let Some(metadata) = key
            .upstream_metadata
            .as_ref()
            .and_then(|metadata| metadata.get("claude_code"))
        {
            item["metadata"] =
                aether_admin::provider::redaction::admin_provider_metadata_bucket_safe_json(
                    "claude_code",
                    Some(metadata),
                );
        }
        if let Some(quota_snapshot) = quota_snapshot {
            item["quota_snapshot"] = quota_snapshot;
        }
        results.push(item);
    }
    let total = results.len();
    Ok(Some(json!({
        "success": success_count,
        "failed": 0,
        "no_metadata": no_metadata_count,
        "total": total,
        "passive": true,
        "results": results,
    })))
}
