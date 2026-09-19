//! Background refresher for provider ops balance snapshots.
//!
//! Every trigger (page load, scheduled monitor, quota alert, manual refresh)
//! goes through [`enqueue_admin_provider_ops_balance_refresh`]. The call
//! returns immediately; a fire-and-forget task waits for one of a small number
//! of refresh slots, runs the upstream query with short timeouts and folds the
//! result into the snapshot. One provider is refreshed by at most one task at a
//! time per gateway instance, and by at most one instance cluster-wide thanks
//! to a runtime lock.
use super::actions::admin_provider_ops_local_action_response;
use super::balance_cache::{
    admin_provider_ops_balance_now_unix_secs, admin_provider_ops_balance_status_is_success,
    apply_admin_provider_ops_balance_attempt, delete_admin_provider_ops_balance_snapshot,
    read_admin_provider_ops_balance_snapshot, write_admin_provider_ops_balance_snapshot,
};
use super::config::admin_provider_ops_config_object;
use super::verify::{
    with_admin_provider_ops_request_timeouts, ADMIN_PROVIDER_OPS_BALANCE_QUERY_TIMEOUTS,
};
use crate::handlers::admin::request::AdminAppState;
use crate::task_runtime::{spawn_fire_and_forget, TASK_KEY_PROVIDER_BALANCE_REFRESH};
use crate::AppState;
use aether_data_contracts::repository::provider_ops_balance::StoredProviderOpsBalanceSnapshot;
use serde_json::Value;
use std::collections::HashSet;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::{Notify, Semaphore};
use tracing::{debug, info, warn};

const ADMIN_PROVIDER_OPS_BALANCE_REFRESH_CONCURRENCY: usize = 4;
/// Longer than the worst-case sub2api refresh (token exchange plus two queries
/// at the balance timeout each) so a crashed instance releases the lock soon.
const ADMIN_PROVIDER_OPS_BALANCE_REFRESH_LOCK_TTL: Duration = Duration::from_secs(90);
const ADMIN_PROVIDER_OPS_BALANCE_REFRESH_LOCK_PREFIX: &str = "provider_ops:balance_refresh:";
/// Snapshots older than this are refreshed when the admin page asks for them.
pub(crate) const ADMIN_PROVIDER_OPS_BALANCE_PAGE_MAX_AGE_SECS: u64 = 5 * 60;
/// The scheduled monitor keeps every configured provider at least this fresh.
pub(crate) const ADMIN_PROVIDER_OPS_BALANCE_SCHEDULED_MAX_AGE_SECS: u64 = 10 * 60;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AdminProviderOpsBalanceRefreshTrigger {
    PageLoad,
    Scheduled,
    QuotaAlert,
    /// An operator asked for this provider explicitly; ignore any backoff.
    Manual,
}

impl AdminProviderOpsBalanceRefreshTrigger {
    fn as_str(self) -> &'static str {
        match self {
            Self::PageLoad => "page_load",
            Self::Scheduled => "scheduled",
            Self::QuotaAlert => "quota_alert",
            Self::Manual => "manual",
        }
    }

    fn forced(self) -> bool {
        matches!(self, Self::Manual)
    }
}

/// Per-gateway refresh bookkeeping shared by every clone of `AppState`.
#[derive(Debug)]
pub(crate) struct ProviderOpsBalanceRefreshState {
    in_flight: Mutex<HashSet<String>>,
    permits: Semaphore,
    idle: Notify,
}

impl Default for ProviderOpsBalanceRefreshState {
    fn default() -> Self {
        Self {
            in_flight: Mutex::new(HashSet::new()),
            permits: Semaphore::new(ADMIN_PROVIDER_OPS_BALANCE_REFRESH_CONCURRENCY),
            idle: Notify::new(),
        }
    }
}

impl ProviderOpsBalanceRefreshState {
    pub(crate) fn is_refreshing(&self, provider_id: &str) -> bool {
        self.in_flight
            .lock()
            .map(|in_flight| in_flight.contains(provider_id))
            .unwrap_or(false)
    }

    fn try_claim(&self, provider_id: &str) -> bool {
        self.in_flight
            .lock()
            .map(|mut in_flight| in_flight.insert(provider_id.to_string()))
            .unwrap_or(false)
    }

    fn release(&self, provider_id: &str) {
        if let Ok(mut in_flight) = self.in_flight.lock() {
            in_flight.remove(provider_id);
        }
        self.idle.notify_waiters();
    }

    /// Resolves once no refresh is running or queued. Intended for tests.
    #[cfg(test)]
    pub(crate) async fn wait_idle(&self) {
        loop {
            let notified = self.idle.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            let is_idle = self
                .in_flight
                .lock()
                .map(|in_flight| in_flight.is_empty())
                .unwrap_or(true);
            if is_idle {
                return;
            }
            notified.await;
        }
    }
}

struct InFlightGuard {
    refresher: Arc<ProviderOpsBalanceRefreshState>,
    provider_id: String,
}

impl Drop for InFlightGuard {
    fn drop(&mut self) {
        self.refresher.release(&self.provider_id);
    }
}

pub(crate) fn admin_provider_ops_balance_refresh_state_label(
    state: &AdminAppState<'_>,
    provider_id: &str,
) -> &'static str {
    if state
        .app()
        .provider_ops_balance_refresher
        .is_refreshing(provider_id)
    {
        "refreshing"
    } else {
        "idle"
    }
}

/// Whether a snapshot should be refreshed for a non-forced trigger: missing,
/// never successful, failed last time (once the backoff elapsed) or older than
/// `max_age_secs`.
pub(crate) fn admin_provider_ops_balance_needs_refresh(
    snapshot: Option<&StoredProviderOpsBalanceSnapshot>,
    now_unix_secs: u64,
    max_age_secs: u64,
) -> bool {
    let Some(snapshot) = snapshot else {
        return true;
    };
    if snapshot
        .next_refresh_at_unix_secs
        .is_some_and(|next| next > now_unix_secs)
    {
        return false;
    }
    if snapshot
        .last_status
        .as_deref()
        .is_some_and(|status| !admin_provider_ops_balance_status_is_success(status))
    {
        return true;
    }
    snapshot
        .last_success_at_unix_secs
        .is_none_or(|fetched_at| now_unix_secs.saturating_sub(fetched_at) >= max_age_secs)
}

/// Schedules a refresh and returns `true` when a new job was created. Returns
/// `false` when the provider is already being refreshed by this instance.
pub(crate) fn enqueue_admin_provider_ops_balance_refresh(
    state: &AdminAppState<'_>,
    provider_id: &str,
    trigger: AdminProviderOpsBalanceRefreshTrigger,
) -> bool {
    let app = state.cloned_app();
    let refresher = Arc::clone(&app.provider_ops_balance_refresher);
    if !refresher.try_claim(provider_id) {
        debug!(
            provider_id,
            trigger = trigger.as_str(),
            "provider ops balance refresh already in flight"
        );
        return false;
    }
    let guard = InFlightGuard {
        refresher: Arc::clone(&refresher),
        provider_id: provider_id.to_string(),
    };
    let provider_id = provider_id.to_string();
    spawn_fire_and_forget(TASK_KEY_PROVIDER_BALANCE_REFRESH, async move {
        let _guard = guard;
        let _permit = refresher.permits.acquire().await.ok();
        run_admin_provider_ops_balance_refresh(&app, &provider_id, trigger).await;
    });
    true
}

async fn run_admin_provider_ops_balance_refresh(
    app: &AppState,
    provider_id: &str,
    trigger: AdminProviderOpsBalanceRefreshTrigger,
) {
    let state = AdminAppState::new(app);
    let previous = read_admin_provider_ops_balance_snapshot(&state, provider_id).await;
    let now_unix_secs = admin_provider_ops_balance_now_unix_secs();
    if !trigger.forced() {
        if let Some(next_refresh_at) = previous
            .as_ref()
            .and_then(|snapshot| snapshot.next_refresh_at_unix_secs)
            .filter(|next_refresh_at| *next_refresh_at > now_unix_secs)
        {
            debug!(
                provider_id,
                trigger = trigger.as_str(),
                next_refresh_at,
                "provider ops balance refresh skipped by backoff"
            );
            return;
        }
    }

    let lock_key = format!("{ADMIN_PROVIDER_OPS_BALANCE_REFRESH_LOCK_PREFIX}{provider_id}");
    let lease = match app
        .runtime_state
        .lock_try_acquire(
            &lock_key,
            app.tunnel.local_instance_id(),
            ADMIN_PROVIDER_OPS_BALANCE_REFRESH_LOCK_TTL,
        )
        .await
    {
        Ok(Some(lease)) => Some(lease),
        Ok(None) => {
            debug!(
                provider_id,
                trigger = trigger.as_str(),
                "provider ops balance refresh owned by another instance"
            );
            return;
        }
        Err(err) => {
            warn!(
                error = %err,
                provider_id,
                "provider ops balance refresh lock unavailable; refreshing without it"
            );
            None
        }
    };

    refresh_admin_provider_ops_balance_snapshot(&state, provider_id, previous, trigger).await;

    if let Some(lease) = lease {
        if let Err(err) = app.runtime_state.lock_release(&lease).await {
            debug!(error = %err, provider_id, "provider ops balance refresh lock release failed");
        }
    }
}

async fn refresh_admin_provider_ops_balance_snapshot(
    state: &AdminAppState<'_>,
    provider_id: &str,
    previous: Option<StoredProviderOpsBalanceSnapshot>,
    trigger: AdminProviderOpsBalanceRefreshTrigger,
) {
    let provider_ids = [provider_id.to_string()];
    let provider = match state
        .read_provider_catalog_providers_by_ids(&provider_ids)
        .await
    {
        Ok(providers) => providers.into_iter().next(),
        Err(err) => {
            warn!(
                provider_id,
                error = ?err,
                "failed to load provider for balance refresh"
            );
            return;
        }
    };
    let Some(provider) =
        provider.filter(|provider| admin_provider_ops_config_object(provider).is_some())
    else {
        // The provider or its ops configuration is gone; drop the stale value.
        delete_admin_provider_ops_balance_snapshot(state, provider_id).await;
        return;
    };
    let architecture_id = admin_provider_ops_config_object(&provider)
        .and_then(|config| config.get("architecture_id"))
        .and_then(Value::as_str)
        .unwrap_or("generic_api")
        .to_string();

    let started = Instant::now();
    let payload = with_admin_provider_ops_request_timeouts(
        ADMIN_PROVIDER_OPS_BALANCE_QUERY_TIMEOUTS,
        admin_provider_ops_local_action_response(
            state,
            provider_id,
            Some(&provider),
            &[],
            "query_balance",
            None,
        ),
    )
    .await;
    let elapsed_ms = started.elapsed().as_millis() as u64;
    let now_unix_secs = admin_provider_ops_balance_now_unix_secs();
    let snapshot = apply_admin_provider_ops_balance_attempt(
        previous.as_ref(),
        provider_id,
        &payload,
        now_unix_secs,
    );
    write_admin_provider_ops_balance_snapshot(state, &snapshot).await;

    let status = snapshot.last_status.as_deref().unwrap_or("unknown_error");
    if admin_provider_ops_balance_status_is_success(status) {
        info!(
            event_name = "provider_ops_balance_refreshed",
            log_type = "ops",
            provider_id,
            provider_name = %provider.name,
            architecture_id = %architecture_id,
            trigger = trigger.as_str(),
            status,
            elapsed_ms,
            "provider ops balance refreshed"
        );
    } else {
        warn!(
            event_name = "provider_ops_balance_refresh_failed",
            log_type = "ops",
            provider_id,
            provider_name = %provider.name,
            architecture_id = %architecture_id,
            trigger = trigger.as_str(),
            status,
            error = snapshot.last_error.as_deref().unwrap_or_default(),
            elapsed_ms,
            consecutive_failures = snapshot.consecutive_failures,
            next_refresh_at = snapshot.next_refresh_at_unix_secs.unwrap_or_default(),
            "provider ops balance refresh failed"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::{
        admin_provider_ops_balance_needs_refresh, ADMIN_PROVIDER_OPS_BALANCE_PAGE_MAX_AGE_SECS,
    };
    use aether_data_contracts::repository::provider_ops_balance::StoredProviderOpsBalanceSnapshot;

    fn snapshot() -> StoredProviderOpsBalanceSnapshot {
        StoredProviderOpsBalanceSnapshot {
            provider_id: "provider-1".to_string(),
            payload_json: None,
            last_success_at_unix_secs: Some(1_000),
            last_attempt_at_unix_secs: Some(1_000),
            last_status: Some("success".to_string()),
            last_error: None,
            consecutive_failures: 0,
            next_refresh_at_unix_secs: None,
            updated_at_unix_secs: 1_000,
        }
    }

    #[test]
    fn refresh_is_needed_for_missing_stale_or_failed_snapshots() {
        let max_age = ADMIN_PROVIDER_OPS_BALANCE_PAGE_MAX_AGE_SECS;
        assert!(admin_provider_ops_balance_needs_refresh(
            None, 1_000, max_age
        ));
        assert!(!admin_provider_ops_balance_needs_refresh(
            Some(&snapshot()),
            1_000 + max_age - 1,
            max_age
        ));
        assert!(admin_provider_ops_balance_needs_refresh(
            Some(&snapshot()),
            1_000 + max_age,
            max_age
        ));

        let failed = StoredProviderOpsBalanceSnapshot {
            last_status: Some("network_error".to_string()),
            next_refresh_at_unix_secs: Some(1_120),
            ..snapshot()
        };
        assert!(!admin_provider_ops_balance_needs_refresh(
            Some(&failed),
            1_100,
            max_age
        ));
        assert!(admin_provider_ops_balance_needs_refresh(
            Some(&failed),
            1_120,
            max_age
        ));
    }
}
