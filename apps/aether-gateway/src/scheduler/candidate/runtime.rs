use std::collections::{BTreeMap, BTreeSet};

use aether_admin::provider::{
    pool as admin_provider_pool_pure, status as admin_provider_status_pure,
};
use aether_data_contracts::repository::candidates::StoredRequestCandidate;
use aether_data_contracts::repository::provider_catalog::{
    StoredProviderCatalogKey, StoredProviderCatalogProvider,
};
use aether_data_contracts::repository::usage::ProviderQuotaWindowUsageRequest;
use aether_scheduler_core::{
    auth_api_key_concurrency_limit_reached, candidate_is_selectable_with_runtime_state,
    candidate_runtime_skip_reason_with_state, effective_provider_key_rpm_limit,
    provider_quota_windows, should_skip_provider_quota_with_windows,
    CandidateRuntimeSelectabilityInput,
};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::data::auth::GatewayAuthApiKeySnapshot;
use crate::GatewayError;

use super::{SchedulerMinimalCandidateSelectionCandidate, SchedulerRuntimeState};

pub(super) use aether_scheduler_core::should_skip_provider_quota;

pub(super) struct CandidateRuntimeSelectionSnapshot {
    pub(super) recent_candidates: Vec<StoredRequestCandidate>,
    pub(super) provider_concurrent_limits: BTreeMap<String, usize>,
    pub(super) provider_key_rpm_states: BTreeMap<String, StoredProviderCatalogKey>,
    pub(super) pool_provider_ids: BTreeSet<String>,
    provider_quota_blocks_requests: BTreeMap<String, bool>,
    key_account_quota_exhausted: BTreeMap<String, bool>,
    key_oauth_invalid: BTreeMap<String, bool>,
    provider_key_rpm_reset_ats: BTreeMap<String, Option<u64>>,
}

pub(super) async fn read_candidate_runtime_selection_snapshot(
    state: &(impl SchedulerRuntimeState + ?Sized),
    candidates: &[SchedulerMinimalCandidateSelectionCandidate],
    auth_snapshot: Option<&GatewayAuthApiKeySnapshot>,
    now_unix_secs: u64,
) -> Result<CandidateRuntimeSelectionSnapshot, GatewayError> {
    let providers = read_provider_runtime_states(state, candidates).await?;
    let provider_concurrent_limits = providers
        .iter()
        .filter_map(|(provider_id, provider)| {
            provider
                .concurrent_limit
                .and_then(|limit| usize::try_from(limit).ok())
                .filter(|limit| *limit > 0)
                .map(|limit| (provider_id.clone(), limit))
        })
        .collect();
    let provider_pool_state = read_provider_pool_state_map(&providers);
    let provider_skip_exhausted_accounts = provider_pool_state
        .iter()
        .map(|(provider_id, state)| (provider_id.clone(), state.skip_exhausted_accounts))
        .collect::<BTreeMap<_, _>>();
    let pool_provider_ids = provider_pool_state
        .iter()
        .filter_map(|(provider_id, state)| state.pool_enabled.then_some(provider_id.clone()))
        .collect::<BTreeSet<_>>();
    let provider_key_rpm_states = read_provider_key_rpm_states(state, candidates).await?;
    let recent_candidates = if runtime_snapshot_requires_recent_candidates(
        auth_snapshot,
        &provider_concurrent_limits,
        &provider_key_rpm_states,
        now_unix_secs,
    ) {
        state.read_recent_request_candidates(128).await?
    } else {
        Vec::new()
    };
    let key_account_quota_exhausted = read_key_account_quota_exhaustion_map(
        candidates,
        &provider_key_rpm_states,
        &provider_skip_exhausted_accounts,
    );
    let key_oauth_invalid =
        read_key_oauth_invalid_map(candidates, &provider_key_rpm_states, now_unix_secs);
    let provider_quota_blocks_requests =
        read_provider_quota_block_map(state, &providers, now_unix_secs).await?;
    let provider_key_rpm_reset_ats =
        read_provider_key_rpm_reset_at_map(state, candidates, now_unix_secs);

    Ok(CandidateRuntimeSelectionSnapshot {
        recent_candidates,
        provider_concurrent_limits,
        provider_key_rpm_states,
        pool_provider_ids,
        provider_quota_blocks_requests,
        key_account_quota_exhausted,
        key_oauth_invalid,
        provider_key_rpm_reset_ats,
    })
}

fn runtime_snapshot_requires_recent_candidates(
    auth_snapshot: Option<&GatewayAuthApiKeySnapshot>,
    provider_concurrent_limits: &BTreeMap<String, usize>,
    provider_key_rpm_states: &BTreeMap<String, StoredProviderCatalogKey>,
    now_unix_secs: u64,
) -> bool {
    if auth_snapshot
        .and_then(|snapshot| snapshot.api_key_concurrent_limit)
        .is_some_and(|limit| limit > 0)
    {
        return true;
    }

    if provider_concurrent_limits.values().any(|limit| *limit > 0) {
        return true;
    }

    provider_key_rpm_states.values().any(|key| {
        key.concurrent_limit.is_some_and(|limit| limit > 0)
            || effective_provider_key_rpm_limit(key, now_unix_secs).is_some()
    })
}

pub(super) fn auth_snapshot_concurrency_limit_reached(
    auth_snapshot: Option<&GatewayAuthApiKeySnapshot>,
    snapshot: &CandidateRuntimeSelectionSnapshot,
    now_unix_secs: u64,
) -> bool {
    auth_snapshot
        .and_then(|snapshot| {
            usize::try_from(snapshot.api_key_concurrent_limit?)
                .ok()
                .and_then(|limit| {
                    if limit == 0 {
                        return None;
                    }
                    Some((snapshot.api_key_id.as_str(), limit))
                })
        })
        .is_some_and(|(api_key_id, limit)| {
            auth_api_key_concurrency_limit_reached(
                &snapshot.recent_candidates,
                now_unix_secs,
                api_key_id,
                limit,
            )
        })
}

pub(super) fn is_candidate_selectable(
    candidate: &SchedulerMinimalCandidateSelectionCandidate,
    snapshot: &CandidateRuntimeSelectionSnapshot,
    now_unix_secs: u64,
) -> bool {
    let pool_group = snapshot
        .pool_provider_ids
        .contains(candidate.provider_id.as_str());
    candidate_is_selectable_with_runtime_state(CandidateRuntimeSelectabilityInput {
        candidate,
        recent_candidates: &snapshot.recent_candidates,
        provider_concurrent_limits: &snapshot.provider_concurrent_limits,
        provider_key_rpm_states: &snapshot.provider_key_rpm_states,
        now_unix_secs,
        provider_quota_blocks_requests: snapshot
            .provider_quota_blocks_requests
            .get(candidate.provider_id.as_str())
            .copied()
            .unwrap_or(false),
        account_quota_exhausted: !pool_group
            && snapshot
                .key_account_quota_exhausted
                .get(candidate.key_id.as_str())
                .copied()
                .unwrap_or(false),
        oauth_invalid: !pool_group
            && snapshot
                .key_oauth_invalid
                .get(candidate.key_id.as_str())
                .copied()
                .unwrap_or(false),
        enforce_key_circuit_breaker: !pool_group,
        rpm_reset_at: (!pool_group)
            .then(|| {
                snapshot
                    .provider_key_rpm_reset_ats
                    .get(candidate.key_id.as_str())
                    .copied()
                    .flatten()
            })
            .flatten(),
    })
}

pub(super) fn current_candidate_runtime_skip_reason(
    candidate: &SchedulerMinimalCandidateSelectionCandidate,
    snapshot: &CandidateRuntimeSelectionSnapshot,
    now_unix_secs: u64,
) -> Option<&'static str> {
    let pool_group = snapshot
        .pool_provider_ids
        .contains(candidate.provider_id.as_str());
    let provider_quota_blocks_requests = snapshot
        .provider_quota_blocks_requests
        .get(candidate.provider_id.as_str())
        .copied()
        .unwrap_or(false);
    let rpm_reset_at = (!pool_group)
        .then(|| {
            snapshot
                .provider_key_rpm_reset_ats
                .get(candidate.key_id.as_str())
                .copied()
                .flatten()
        })
        .flatten();

    candidate_runtime_skip_reason_with_state(CandidateRuntimeSelectabilityInput {
        candidate,
        recent_candidates: &snapshot.recent_candidates,
        provider_concurrent_limits: &snapshot.provider_concurrent_limits,
        provider_key_rpm_states: &snapshot.provider_key_rpm_states,
        now_unix_secs,
        provider_quota_blocks_requests,
        account_quota_exhausted: !pool_group
            && snapshot
                .key_account_quota_exhausted
                .get(candidate.key_id.as_str())
                .copied()
                .unwrap_or(false),
        oauth_invalid: !pool_group
            && snapshot
                .key_oauth_invalid
                .get(candidate.key_id.as_str())
                .copied()
                .unwrap_or(false),
        enforce_key_circuit_breaker: !pool_group,
        rpm_reset_at,
    })
}

async fn read_provider_runtime_states(
    state: &(impl SchedulerRuntimeState + ?Sized),
    candidates: &[SchedulerMinimalCandidateSelectionCandidate],
) -> Result<BTreeMap<String, StoredProviderCatalogProvider>, GatewayError> {
    let provider_ids = candidates
        .iter()
        .map(|candidate| candidate.provider_id.clone())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect::<Vec<_>>();
    if provider_ids.is_empty() {
        return Ok(BTreeMap::new());
    }

    Ok(state
        .read_provider_catalog_providers_by_ids(&provider_ids)
        .await?
        .into_iter()
        .map(|provider| (provider.id.clone(), provider))
        .collect())
}

pub(super) async fn read_provider_key_rpm_states(
    state: &(impl SchedulerRuntimeState + ?Sized),
    candidates: &[SchedulerMinimalCandidateSelectionCandidate],
) -> Result<BTreeMap<String, StoredProviderCatalogKey>, GatewayError> {
    let key_ids = candidates
        .iter()
        .map(|candidate| candidate.key_id.clone())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect::<Vec<_>>();
    if key_ids.is_empty() {
        return Ok(BTreeMap::new());
    }

    let keys = state.read_provider_catalog_keys_by_ids(&key_ids).await?;
    Ok(keys
        .into_iter()
        .map(|key| (key.id.clone(), key))
        .collect::<BTreeMap<_, _>>())
}

async fn read_provider_quota_block_map(
    state: &(impl SchedulerRuntimeState + ?Sized),
    providers: &BTreeMap<String, StoredProviderCatalogProvider>,
    now_unix_secs: u64,
) -> Result<BTreeMap<String, bool>, GatewayError> {
    let provider_ids = providers.keys().cloned().collect::<Vec<_>>();
    let quotas = state
        .read_provider_quota_snapshots(&provider_ids)
        .await?
        .into_iter()
        .map(|quota| (quota.provider_id.clone(), quota))
        .collect::<BTreeMap<_, _>>();
    let mut provider_windows = BTreeMap::new();
    let mut window_requests = Vec::new();
    for provider_id in &provider_ids {
        let Some(quota) = quotas.get(provider_id) else {
            continue;
        };
        if aether_scheduler_core::should_skip_provider_quota(quota, now_unix_secs) {
            continue;
        }
        let Some(provider) = providers.get(provider_id) else {
            continue;
        };
        let windows = provider_quota_windows(provider.config.as_ref());
        for window in &windows {
            window_requests.push(ProviderQuotaWindowUsageRequest {
                provider_id: provider_id.clone(),
                duration_secs: window.duration_secs,
                window_start_unix_secs: aether_wallet::quota_window_start_unix_secs(
                    now_unix_secs,
                    quota.quota_last_reset_at_unix_secs,
                    window.duration_secs,
                ),
            });
        }
        if !windows.is_empty() {
            provider_windows.insert(provider_id.clone(), windows);
        }
    }
    let mut window_usage = BTreeMap::<String, BTreeMap<(u64, u64), f64>>::new();
    for usage in state
        .read_provider_quota_window_usage(&window_requests)
        .await?
    {
        window_usage.entry(usage.provider_id).or_default().insert(
            (usage.duration_secs, usage.window_start_unix_secs),
            usage.used_usd,
        );
    }
    let mut quota_blocks = BTreeMap::new();

    for provider_id in provider_ids {
        let quota = quotas.get(&provider_id);
        let mut blocks_requests = quota.as_ref().is_some_and(|quota| {
            aether_scheduler_core::should_skip_provider_quota(quota, now_unix_secs)
        });

        if !blocks_requests {
            if let (Some(quota), Some(windows)) = (quota, provider_windows.get(&provider_id)) {
                let usage = windows
                    .iter()
                    .map(|window| {
                        let window_start = aether_wallet::quota_window_start_unix_secs(
                            now_unix_secs,
                            quota.quota_last_reset_at_unix_secs,
                            window.duration_secs,
                        );
                        window_usage
                            .get(&provider_id)
                            .and_then(|usage| {
                                usage.get(&(window.duration_secs, window_start)).copied()
                            })
                            .unwrap_or(0.0)
                    })
                    .collect::<Vec<_>>();
                blocks_requests =
                    should_skip_provider_quota_with_windows(quota, now_unix_secs, windows, &usage);
            }
        }
        quota_blocks.insert(provider_id, blocks_requests);
    }

    Ok(quota_blocks)
}

#[derive(Debug, Clone, Copy, Default)]
struct ProviderPoolState {
    pool_enabled: bool,
    skip_exhausted_accounts: bool,
}

fn read_provider_pool_state_map(
    providers: &BTreeMap<String, StoredProviderCatalogProvider>,
) -> BTreeMap<String, ProviderPoolState> {
    providers
        .values()
        .map(|provider| {
            let pool_advanced = provider
                .config
                .as_ref()
                .and_then(|value| value.get("pool_advanced"));
            let skip_exhausted_accounts = pool_advanced
                .and_then(serde_json::Value::as_object)
                .and_then(|value| value.get("skip_exhausted_accounts"))
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false);
            (
                provider.id.clone(),
                ProviderPoolState {
                    pool_enabled: pool_advanced.is_some(),
                    skip_exhausted_accounts,
                },
            )
        })
        .collect()
}

fn read_key_account_quota_exhaustion_map(
    candidates: &[SchedulerMinimalCandidateSelectionCandidate],
    provider_key_rpm_states: &BTreeMap<String, StoredProviderCatalogKey>,
    provider_skip_exhausted_accounts: &BTreeMap<String, bool>,
) -> BTreeMap<String, bool> {
    candidates
        .iter()
        .map(|candidate| {
            let exhausted = provider_skip_exhausted_accounts
                .get(candidate.provider_id.as_str())
                .copied()
                .unwrap_or(false)
                && provider_key_rpm_states
                    .get(candidate.key_id.as_str())
                    .is_some_and(|key| {
                        admin_provider_pool_pure::admin_pool_key_account_quota_exhausted(
                            key,
                            candidate.provider_type.as_str(),
                        )
                    });
            (candidate.key_id.clone(), exhausted)
        })
        .collect()
}

fn read_key_oauth_invalid_map(
    candidates: &[SchedulerMinimalCandidateSelectionCandidate],
    provider_key_rpm_states: &BTreeMap<String, StoredProviderCatalogKey>,
    now_unix_secs: u64,
) -> BTreeMap<String, bool> {
    candidates
        .iter()
        .map(|candidate| {
            let oauth_invalid = provider_key_rpm_states
                .get(candidate.key_id.as_str())
                .is_some_and(|key| {
                    key_requires_oauth_reauth(key, candidate.provider_type.as_str(), now_unix_secs)
                });
            (candidate.key_id.clone(), oauth_invalid)
        })
        .collect()
}

fn key_requires_oauth_reauth(
    key: &StoredProviderCatalogKey,
    provider_type: &str,
    now_unix_secs: u64,
) -> bool {
    if !key.auth_type.trim().eq_ignore_ascii_case("oauth") {
        return false;
    }

    let invalid_reason = key
        .oauth_invalid_reason
        .as_deref()
        .map(str::trim)
        .unwrap_or_default();
    if !invalid_reason.is_empty() {
        return oauth_invalid_reason_blocks_scheduling(
            key,
            provider_type,
            invalid_reason,
            now_unix_secs,
        );
    }

    false
}

fn oauth_invalid_reason_blocks_scheduling(
    key: &StoredProviderCatalogKey,
    provider_type: &str,
    invalid_reason: &str,
    now_unix_secs: u64,
) -> bool {
    let trimmed_reason = invalid_reason.trim();

    let account_state = admin_provider_status_pure::resolve_pool_account_state(
        Some(provider_type),
        key.upstream_metadata.as_ref(),
        Some(trimmed_reason),
    );
    if account_state.blocked
        && !account_state.recoverable
        && account_state
            .code
            .as_deref()
            .is_some_and(oauth_account_state_code_is_hard_block)
    {
        return true;
    }

    if oauth_invalid_reason_has_tag(trimmed_reason, "[REFRESH_FAILED]") {
        return oauth_access_token_expired(key, now_unix_secs);
    }

    false
}

fn oauth_invalid_reason_has_tag(reason: &str, tag: &str) -> bool {
    reason
        .lines()
        .map(str::trim)
        .any(|line| line.starts_with(tag))
}

fn oauth_access_token_expired(key: &StoredProviderCatalogKey, now_unix_secs: u64) -> bool {
    let now_unix_secs = if now_unix_secs == 0 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .ok()
            .map(|duration| duration.as_secs())
            .unwrap_or(0)
    } else {
        now_unix_secs
    };
    key.expires_at_unix_secs
        .is_none_or(|expires_at| expires_at == 0 || expires_at <= now_unix_secs)
}

fn oauth_account_state_code_is_hard_block(code: &str) -> bool {
    matches!(
        code.trim().to_ascii_lowercase().as_str(),
        "account_banned"
            | "account_suspended"
            | "account_disabled"
            | "workspace_deactivated"
            | "account_forbidden"
            | "account_blocked"
            | "account_verification"
            | "oauth_token_invalid"
    )
}

fn read_provider_key_rpm_reset_at_map(
    state: &(impl SchedulerRuntimeState + ?Sized),
    candidates: &[SchedulerMinimalCandidateSelectionCandidate],
    now_unix_secs: u64,
) -> BTreeMap<String, Option<u64>> {
    candidates
        .iter()
        .map(|candidate| {
            (
                candidate.key_id.clone(),
                state.provider_key_rpm_reset_at(candidate.key_id.as_str(), now_unix_secs),
            )
        })
        .collect::<BTreeMap<_, _>>()
}
