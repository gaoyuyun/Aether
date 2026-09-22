use super::{
    DataLayerError, GatewayDataState, GetPoolMemberScoresByIdsQuery,
    ListPoolMemberProbeCandidatesQuery, ListPoolMemberScoresQuery, ListRankedPoolMembersQuery,
    PoolMemberHardState, PoolMemberIdentity, PoolMemberProbeAttempt, PoolMemberProbeResult,
    PoolMemberScheduleFeedback, PoolScoreScope, StoredPoolMemberScore, UpsertPoolMemberScore,
};
use aether_data_contracts::repository::pool_scores::{
    merge_score_reason_patch, PoolMemberScoreUpsertMode,
};

fn score_reason_patch_with_hard_state(
    patch: Option<serde_json::Value>,
    hard_state: Option<PoolMemberHardState>,
) -> Option<serde_json::Value> {
    let Some(hard_state) = hard_state else {
        return patch;
    };
    Some(merge_score_reason_patch(
        patch.unwrap_or_else(|| serde_json::json!({})),
        Some(serde_json::json!({"hard_state": hard_state.as_database()})),
    ))
}

impl GatewayDataState {
    pub(crate) async fn list_ranked_pool_members(
        &self,
        query: &ListRankedPoolMembersQuery,
    ) -> Result<Vec<StoredPoolMemberScore>, DataLayerError> {
        match &self.pool_score_reader {
            Some(repository) => repository.list_ranked_pool_members(query).await,
            None => Ok(Vec::new()),
        }
    }

    pub(crate) async fn list_pool_member_probe_candidates(
        &self,
        query: &ListPoolMemberProbeCandidatesQuery,
    ) -> Result<Vec<StoredPoolMemberScore>, DataLayerError> {
        match &self.pool_score_reader {
            Some(repository) => repository.list_pool_member_probe_candidates(query).await,
            None => Ok(Vec::new()),
        }
    }

    pub(crate) async fn list_pool_member_scores(
        &self,
        query: &ListPoolMemberScoresQuery,
    ) -> Result<Vec<StoredPoolMemberScore>, DataLayerError> {
        match &self.pool_score_reader {
            Some(repository) => repository.list_pool_member_scores(query).await,
            None => Ok(Vec::new()),
        }
    }

    pub(crate) async fn get_pool_member_scores_by_ids(
        &self,
        query: &GetPoolMemberScoresByIdsQuery,
    ) -> Result<Vec<StoredPoolMemberScore>, DataLayerError> {
        match &self.pool_score_reader {
            Some(repository) => repository.get_pool_member_scores_by_ids(query).await,
            None => Ok(Vec::new()),
        }
    }

    pub(crate) async fn upsert_pool_member_score(
        &self,
        score: UpsertPoolMemberScore,
    ) -> Result<Option<StoredPoolMemberScore>, DataLayerError> {
        match &self.pool_score_writer {
            Some(repository) => repository.upsert_pool_member_score(score).await.map(Some),
            None => Ok(None),
        }
    }

    pub(crate) async fn upsert_pool_member_score_with_mode(
        &self,
        score: UpsertPoolMemberScore,
        mode: PoolMemberScoreUpsertMode,
    ) -> Result<Option<StoredPoolMemberScore>, DataLayerError> {
        match &self.pool_score_writer {
            Some(repository) => repository
                .upsert_pool_member_score_with_mode(score, mode)
                .await
                .map(Some),
            None => Ok(None),
        }
    }

    pub(crate) async fn record_pool_member_probe_result(
        &self,
        mut result: PoolMemberProbeResult,
    ) -> Result<usize, DataLayerError> {
        // Feedback changes the persisted hard state between score rebuilds.
        // Keep the calculation details from displaying the previous state.
        result.score_reason_patch =
            score_reason_patch_with_hard_state(result.score_reason_patch, result.hard_state);
        match &self.pool_score_writer {
            Some(repository) => repository.record_pool_member_probe_result(result).await,
            None => Ok(0),
        }
    }

    pub(crate) async fn mark_pool_member_probe_in_progress(
        &self,
        attempt: PoolMemberProbeAttempt,
    ) -> Result<usize, DataLayerError> {
        match &self.pool_score_writer {
            Some(repository) => repository.mark_pool_member_probe_in_progress(attempt).await,
            None => Ok(0),
        }
    }

    pub(crate) async fn record_pool_member_schedule_feedback(
        &self,
        mut feedback: PoolMemberScheduleFeedback,
    ) -> Result<usize, DataLayerError> {
        feedback.score_reason_patch =
            score_reason_patch_with_hard_state(feedback.score_reason_patch, feedback.hard_state);
        match &self.pool_score_writer {
            Some(repository) => {
                repository
                    .record_pool_member_schedule_feedback(feedback)
                    .await
            }
            None => Ok(0),
        }
    }

    pub(crate) async fn mark_pool_member_hard_state(
        &self,
        identity: &PoolMemberIdentity,
        scope: Option<&PoolScoreScope>,
        hard_state: PoolMemberHardState,
        updated_at: u64,
    ) -> Result<usize, DataLayerError> {
        match &self.pool_score_writer {
            Some(repository) => {
                repository
                    .mark_pool_member_hard_state(identity, scope, hard_state, updated_at)
                    .await
            }
            None => Ok(0),
        }
    }

    pub(crate) async fn delete_pool_member_scores_for_member(
        &self,
        identity: &PoolMemberIdentity,
    ) -> Result<usize, DataLayerError> {
        match &self.pool_score_writer {
            Some(repository) => {
                repository
                    .delete_pool_member_scores_for_member(identity)
                    .await
            }
            None => Ok(0),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    use aether_data::repository::pool_scores::InMemoryPoolMemberScoreRepository;
    use aether_data_contracts::repository::pool_scores::PoolMemberProbeStatus;
    use aether_data_contracts::repository::provider_catalog::StoredProviderCatalogKey;
    use serde_json::json;

    #[tokio::test]
    async fn successful_feedback_clears_quota_exhaustion_in_score_details() {
        for is_probe in [false, true] {
            let key = StoredProviderCatalogKey::new(
                "key-1".to_string(),
                "provider-1".to_string(),
                "codex".to_string(),
                "oauth".to_string(),
                None,
                true,
            )
            .unwrap();
            let mut initial = crate::ai_serving::build_provider_key_pool_score_upsert(
                &key,
                "codex",
                None,
                100,
                Default::default(),
            )
            .into_stored();
            initial.hard_state = PoolMemberHardState::QuotaExhausted;
            initial.score_reason["hard_state"] = json!("quota_exhausted");
            let data = GatewayDataState::disabled().with_pool_score_repository_for_tests(Arc::new(
                InMemoryPoolMemberScoreRepository::seed(vec![initial.clone()]),
            ));
            let identity = PoolMemberIdentity::provider_api_key("provider-1", "key-1");
            let patch = json!({"last_result": {"status": "success"}});
            if is_probe {
                data.record_pool_member_probe_result(PoolMemberProbeResult {
                    identity,
                    scope: None,
                    attempted_at: 200,
                    succeeded: true,
                    hard_state: Some(PoolMemberHardState::Available),
                    probe_status: PoolMemberProbeStatus::Ok,
                    score_reason_patch: Some(patch),
                })
                .await
                .unwrap();
            } else {
                data.record_pool_member_schedule_feedback(PoolMemberScheduleFeedback {
                    identity,
                    scope: None,
                    scheduled_at: 200,
                    succeeded: Some(true),
                    hard_state: Some(PoolMemberHardState::Available),
                    score_delta: None,
                    score_reason_patch: Some(patch),
                })
                .await
                .unwrap();
            }
            let updated = data
                .get_pool_member_scores_by_ids(&GetPoolMemberScoresByIdsQuery {
                    ids: vec![initial.id.clone()],
                })
                .await
                .unwrap()
                .pop()
                .unwrap();
            assert_eq!(updated.hard_state, PoolMemberHardState::Available);
            assert_eq!(updated.score_reason["hard_state"], json!("available"));
            assert_eq!(
                updated.score_reason["last_result"]["status"],
                json!("success")
            );
            assert_eq!(
                updated.score_reason["factors"],
                initial.score_reason["factors"]
            );
        }
    }
}
