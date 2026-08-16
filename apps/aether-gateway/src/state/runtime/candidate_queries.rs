use crate::{AppState, GatewayError};
use aether_data_contracts::repository::{candidate_selection, candidates, quota, usage};
use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

const PROVIDER_QUOTA_RUNTIME_CACHE_TTL: Duration = Duration::from_secs(5);
const PROVIDER_QUOTA_WINDOW_USAGE_CACHE_TTL: Duration = Duration::from_secs(1);

impl AppState {
    pub(crate) async fn list_minimal_candidate_selection_rows_for_api_format(
        &self,
        api_format: &str,
    ) -> Result<Vec<candidate_selection::StoredMinimalCandidateSelectionRow>, GatewayError> {
        self.data
            .list_minimal_candidate_selection_rows_for_api_format(api_format)
            .await
            .map_err(|err| GatewayError::Internal(err.to_string()))
    }

    pub(crate) async fn list_minimal_candidate_selection_rows_for_api_format_and_global_model(
        &self,
        api_format: &str,
        global_model_name: &str,
    ) -> Result<Vec<candidate_selection::StoredMinimalCandidateSelectionRow>, GatewayError> {
        self.data
            .list_minimal_candidate_selection_rows(api_format, global_model_name)
            .await
            .map_err(|err| GatewayError::Internal(err.to_string()))
    }

    pub(crate) async fn list_minimal_candidate_selection_rows_for_api_format_and_requested_model(
        &self,
        api_format: &str,
        requested_model_name: &str,
    ) -> Result<Vec<candidate_selection::StoredMinimalCandidateSelectionRow>, GatewayError> {
        self.data
            .list_minimal_candidate_selection_rows_for_requested_model(
                api_format,
                requested_model_name,
            )
            .await
            .map_err(|err| GatewayError::Internal(err.to_string()))
    }

    pub(crate) async fn list_minimal_candidate_selection_rows_for_api_format_and_requested_model_page(
        &self,
        query: &candidate_selection::StoredRequestedModelCandidateRowsQuery,
    ) -> Result<Vec<candidate_selection::StoredMinimalCandidateSelectionRow>, GatewayError> {
        self.data
            .list_minimal_candidate_selection_rows_for_requested_model_page(query)
            .await
            .map_err(|err| GatewayError::Internal(err.to_string()))
    }

    pub(crate) async fn list_pool_key_candidate_rows_for_group(
        &self,
        query: &candidate_selection::StoredPoolKeyCandidateRowsQuery,
    ) -> Result<Vec<candidate_selection::StoredMinimalCandidateSelectionRow>, GatewayError> {
        self.data
            .list_pool_key_candidate_rows_for_group(query)
            .await
            .map_err(|err| GatewayError::Internal(err.to_string()))
    }

    pub(crate) async fn list_pool_key_candidate_rows_for_group_key_ids(
        &self,
        query: &candidate_selection::StoredPoolKeyCandidateRowsByKeyIdsQuery,
    ) -> Result<Vec<candidate_selection::StoredMinimalCandidateSelectionRow>, GatewayError> {
        self.data
            .list_pool_key_candidate_rows_for_group_key_ids(query)
            .await
            .map_err(|err| GatewayError::Internal(err.to_string()))
    }

    pub(crate) async fn read_provider_quota_snapshot(
        &self,
        provider_id: &str,
    ) -> Result<Option<quota::StoredProviderQuotaSnapshot>, GatewayError> {
        let provider_id = provider_id.trim();
        if provider_id.is_empty() {
            return Ok(None);
        }
        let cache_key = provider_id.to_string();
        self.provider_quota_snapshot_cache
            .get_or_load(cache_key, PROVIDER_QUOTA_RUNTIME_CACHE_TTL, || async move {
                self.data
                    .find_provider_quota_by_provider_id(provider_id)
                    .await
                    .map_err(|err| GatewayError::Internal(err.to_string()))
            })
            .await
    }

    pub(crate) async fn read_provider_quota_snapshots(
        &self,
        provider_ids: &[String],
    ) -> Result<Vec<quota::StoredProviderQuotaSnapshot>, GatewayError> {
        let mut snapshots = Vec::with_capacity(provider_ids.len());
        let mut missing = BTreeSet::new();
        for provider_id in provider_ids {
            let provider_id = provider_id.trim();
            if provider_id.is_empty() {
                continue;
            }
            let cache_key = provider_id.to_string();
            match self
                .provider_quota_snapshot_cache
                .get(&cache_key, PROVIDER_QUOTA_RUNTIME_CACHE_TTL)
            {
                Some(Some(snapshot)) => snapshots.push(snapshot),
                Some(None) => {}
                None => {
                    missing.insert(cache_key);
                }
            }
        }
        if missing.is_empty() {
            return Ok(snapshots);
        }

        let _refresh_guard = self.provider_quota_cache_refresh_lock.lock().await;
        let mut still_missing = Vec::with_capacity(missing.len());
        for provider_id in missing {
            match self
                .provider_quota_snapshot_cache
                .get(&provider_id, PROVIDER_QUOTA_RUNTIME_CACHE_TTL)
            {
                Some(Some(snapshot)) => snapshots.push(snapshot),
                Some(None) => {}
                None => still_missing.push(provider_id),
            }
        }
        if still_missing.is_empty() {
            return Ok(snapshots);
        }

        let mut loaded = self
            .data
            .find_provider_quotas_by_provider_ids(&still_missing)
            .await
            .map_err(|err| GatewayError::Internal(err.to_string()))?
            .into_iter()
            .map(|snapshot| (snapshot.provider_id.clone(), snapshot))
            .collect::<BTreeMap<_, _>>();
        for provider_id in still_missing {
            let snapshot = loaded.remove(&provider_id);
            self.provider_quota_snapshot_cache.insert(
                provider_id,
                snapshot.clone(),
                PROVIDER_QUOTA_RUNTIME_CACHE_TTL,
            );
            if let Some(snapshot) = snapshot {
                snapshots.push(snapshot);
            }
        }
        Ok(snapshots)
    }

    pub(crate) async fn read_provider_quota_window_usage(
        &self,
        requests: &[usage::ProviderQuotaWindowUsageRequest],
    ) -> Result<Vec<usage::StoredProviderQuotaWindowUsage>, GatewayError> {
        let mut usage_rows = Vec::with_capacity(requests.len());
        let mut missing = BTreeMap::new();
        for request in requests {
            match self
                .provider_quota_window_usage_cache
                .get(request, PROVIDER_QUOTA_WINDOW_USAGE_CACHE_TTL)
            {
                Some(Some(usage)) => usage_rows.push(usage),
                Some(None) => {}
                None => {
                    missing.insert(request.clone(), ());
                }
            }
        }
        if missing.is_empty() {
            return Ok(usage_rows);
        }

        let _refresh_guard = self.provider_quota_cache_refresh_lock.lock().await;
        let mut still_missing = BTreeMap::new();
        for (request, _) in missing {
            match self
                .provider_quota_window_usage_cache
                .get(&request, PROVIDER_QUOTA_WINDOW_USAGE_CACHE_TTL)
            {
                Some(Some(usage)) => usage_rows.push(usage),
                Some(None) => {}
                None => {
                    still_missing.insert(request, ());
                }
            }
        }
        if still_missing.is_empty() {
            return Ok(usage_rows);
        }

        let missing_requests = still_missing.keys().cloned().collect::<Vec<_>>();
        let loaded = self
            .data
            .read_provider_quota_window_usage(&missing_requests)
            .await
            .map_err(|err| GatewayError::Internal(err.to_string()))?;
        let mut loaded = loaded
            .into_iter()
            .map(|usage| {
                (
                    usage::ProviderQuotaWindowUsageRequest {
                        provider_id: usage.provider_id.clone(),
                        duration_secs: usage.duration_secs,
                        window_start_unix_secs: usage.window_start_unix_secs,
                    },
                    usage,
                )
            })
            .collect::<BTreeMap<_, _>>();
        for (request, _) in still_missing {
            let usage = loaded.remove(&request);
            self.provider_quota_window_usage_cache.insert(
                request,
                usage.clone(),
                PROVIDER_QUOTA_WINDOW_USAGE_CACHE_TTL,
            );
            if let Some(usage) = usage {
                usage_rows.push(usage);
            }
        }
        Ok(usage_rows)
    }

    pub(crate) async fn read_recent_request_candidates(
        &self,
        limit: usize,
    ) -> Result<Vec<candidates::StoredRequestCandidate>, GatewayError> {
        self.data
            .list_recent_request_candidates(limit)
            .await
            .map_err(|err| GatewayError::Internal(err.to_string()))
    }

    pub(crate) async fn upsert_request_candidate(
        &self,
        candidate: candidates::UpsertRequestCandidateRecord,
    ) -> Result<Option<candidates::StoredRequestCandidate>, GatewayError> {
        if let Some(queue) = self.request_candidate_queue.as_ref() {
            let stored = stored_request_candidate_from_upsert(&candidate)?;
            queue
                .enqueue_or_fallback(candidate)
                .await
                .map_err(|err| GatewayError::Internal(err.to_string()))?;
            return Ok(Some(stored));
        }

        self.data
            .upsert_request_candidate(candidate)
            .await
            .map_err(|err| GatewayError::Internal(err.to_string()))
    }

    /// Persist a candidate status when the caller does not need the materialized row.
    ///
    /// Lifecycle updates are emitted on the hot path (in particular the first-byte
    /// `pending -> streaming` transition). Rebuilding `StoredRequestCandidate` here
    /// only to discard it adds validation and clones for every update, especially when
    /// the async queue is enabled.
    pub(crate) async fn enqueue_request_candidate_status(
        &self,
        candidate: candidates::UpsertRequestCandidateRecord,
    ) -> Result<Option<()>, GatewayError> {
        if let Some(queue) = self.request_candidate_queue.as_ref() {
            queue
                .enqueue_or_fallback(candidate)
                .await
                .map_err(|err| GatewayError::Internal(err.to_string()))?;
            return Ok(Some(()));
        }

        self.data
            .upsert_request_candidate(candidate)
            .await
            .map(|stored| stored.map(|_| ()))
            .map_err(|err| GatewayError::Internal(err.to_string()))
    }

    /// Try the in-memory lifecycle lane without awaiting or touching the repository.
    /// The returned record must be persisted through `enqueue_request_candidate_status`
    /// when the queue is disabled or closed.
    pub(crate) fn try_enqueue_request_candidate_status(
        &self,
        candidate: candidates::UpsertRequestCandidateRecord,
    ) -> Result<(), candidates::UpsertRequestCandidateRecord> {
        let Some(queue) = self.request_candidate_queue.as_ref() else {
            return Err(candidate);
        };
        queue.try_enqueue_priority_status(candidate)
    }
}

fn stored_request_candidate_from_upsert(
    candidate: &candidates::UpsertRequestCandidateRecord,
) -> Result<candidates::StoredRequestCandidate, GatewayError> {
    candidate
        .validate()
        .map_err(|err| GatewayError::Internal(err.to_string()))?;
    candidates::StoredRequestCandidate::new(
        candidate.id.clone(),
        candidate.request_id.clone(),
        candidate.user_id.clone(),
        candidate.api_key_id.clone(),
        candidate.username.clone(),
        candidate.api_key_name.clone(),
        candidate.candidate_index.try_into().unwrap_or(i32::MAX),
        candidate.retry_index.try_into().unwrap_or(i32::MAX),
        candidate.provider_id.clone(),
        candidate.endpoint_id.clone(),
        candidate.key_id.clone(),
        candidate.status,
        candidate.skip_reason.clone(),
        candidate.is_cached.unwrap_or(false),
        candidate.status_code.map(i32::from),
        candidate.error_type.clone(),
        candidate.error_message.clone(),
        candidate
            .latency_ms
            .map(|value| i32::try_from(value).unwrap_or(i32::MAX)),
        candidate
            .concurrent_requests
            .map(|value| i32::try_from(value).unwrap_or(i32::MAX)),
        candidate.extra_data.clone(),
        candidate.required_capabilities.clone(),
        candidate
            .created_at_unix_ms
            .or(candidate.started_at_unix_ms)
            .or(candidate.finished_at_unix_ms)
            .unwrap_or_else(crate::clock::current_unix_ms)
            .try_into()
            .unwrap_or(i64::MAX),
        candidate
            .started_at_unix_ms
            .map(|value| value.try_into().unwrap_or(i64::MAX)),
        candidate
            .finished_at_unix_ms
            .map(|value| value.try_into().unwrap_or(i64::MAX)),
    )
    .map_err(|err| GatewayError::Internal(err.to_string()))
}
