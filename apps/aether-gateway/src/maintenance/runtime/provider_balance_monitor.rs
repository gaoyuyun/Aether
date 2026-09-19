//! Keeps balance snapshots fresh for every active provider with ops configured
//! so the admin page and the quota alert read recent values without querying
//! upstreams themselves.
use serde_json::Value;

use crate::admin_api::{
    admin_provider_ops_balance_needs_refresh, enqueue_admin_provider_ops_balance_refresh,
    read_admin_provider_ops_balance_snapshots, AdminAppState,
    AdminProviderOpsBalanceRefreshTrigger, ADMIN_PROVIDER_OPS_BALANCE_SCHEDULED_MAX_AGE_SECS,
};
use crate::{AppState, GatewayError};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct ProviderBalanceMonitorRunSummary {
    pub(crate) scanned: usize,
    pub(crate) enqueued: usize,
    pub(crate) fresh: usize,
}

pub(crate) async fn perform_provider_balance_monitor_once(
    state: &AppState,
) -> Result<ProviderBalanceMonitorRunSummary, GatewayError> {
    if !state.has_provider_catalog_data_reader() {
        return Ok(ProviderBalanceMonitorRunSummary::default());
    }
    let provider_ids = state
        .data
        .list_provider_catalog_providers(true)
        .await
        .map_err(|err| GatewayError::Internal(err.to_string()))?
        .into_iter()
        .filter(|provider| {
            provider
                .config
                .as_ref()
                .and_then(Value::as_object)
                .and_then(|config| config.get("provider_ops"))
                .is_some_and(Value::is_object)
        })
        .map(|provider| provider.id)
        .collect::<Vec<_>>();
    if provider_ids.is_empty() {
        return Ok(ProviderBalanceMonitorRunSummary::default());
    }

    let admin_state = AdminAppState::new(state);
    let snapshots = read_admin_provider_ops_balance_snapshots(&admin_state, &provider_ids).await;
    let now_unix_secs = chrono::Utc::now().timestamp().max(0) as u64;
    let mut summary = ProviderBalanceMonitorRunSummary {
        scanned: provider_ids.len(),
        ..ProviderBalanceMonitorRunSummary::default()
    };
    for provider_id in &provider_ids {
        if admin_provider_ops_balance_needs_refresh(
            snapshots.get(provider_id),
            now_unix_secs,
            ADMIN_PROVIDER_OPS_BALANCE_SCHEDULED_MAX_AGE_SECS,
        ) {
            if enqueue_admin_provider_ops_balance_refresh(
                &admin_state,
                provider_id,
                AdminProviderOpsBalanceRefreshTrigger::Scheduled,
            ) {
                summary.enqueued += 1;
            }
        } else {
            summary.fresh += 1;
        }
    }
    Ok(summary)
}
