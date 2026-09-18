use aether_data_contracts::DataLayerError;

use super::state::GatewayDataState;

pub(crate) use aether_data_contracts::repository::candidates::{
    RequestCandidateFinalStatus, RequestCandidateTrace, RequestCandidateTraceScope,
};

pub(crate) async fn read_request_candidate_trace(
    state: &GatewayDataState,
    request_id: &str,
    attempted_only: bool,
) -> Result<Option<RequestCandidateTrace>, DataLayerError> {
    read_request_candidate_trace_in_scope(
        state,
        request_id,
        RequestCandidateTraceScope::from_attempted_only(attempted_only),
    )
    .await
}

pub(crate) async fn read_request_candidate_trace_in_scope(
    state: &GatewayDataState,
    request_id: &str,
    scope: RequestCandidateTraceScope,
) -> Result<Option<RequestCandidateTrace>, DataLayerError> {
    let all_candidates = match scope {
        RequestCandidateTraceScope::Attempted => {
            state
                .list_attempted_request_candidates_by_request_id(request_id)
                .await?
        }
        RequestCandidateTraceScope::All | RequestCandidateTraceScope::AttemptedOrSkipped => {
            state
                .list_request_candidates_by_request_id(request_id)
                .await?
        }
    };
    Ok(RequestCandidateTrace::from_candidates_in_scope(
        request_id,
        all_candidates,
        scope,
    ))
}

#[cfg(test)]
mod tests {
    use super::super::GatewayDataState;
    use super::{
        read_request_candidate_trace, read_request_candidate_trace_in_scope,
        RequestCandidateFinalStatus, RequestCandidateTraceScope,
    };
    use aether_data::repository::candidates::InMemoryRequestCandidateRepository;
    use aether_data_contracts::repository::candidates::{
        derive_request_candidate_final_status, RequestCandidateStatus, StoredRequestCandidate,
    };
    use std::sync::Arc;

    fn sample_candidate(
        id: &str,
        request_id: &str,
        candidate_index: i32,
        status: RequestCandidateStatus,
        started_at_unix_ms: Option<i64>,
        latency_ms: Option<i32>,
        status_code: Option<i32>,
    ) -> StoredRequestCandidate {
        StoredRequestCandidate::new(
            id.to_string(),
            request_id.to_string(),
            Some("user-1".to_string()),
            Some("api-key-1".to_string()),
            Some("alice".to_string()),
            Some("default".to_string()),
            candidate_index,
            0,
            Some("provider-1".to_string()),
            Some("endpoint-1".to_string()),
            Some("provider-key-1".to_string()),
            status,
            None,
            false,
            status_code,
            None,
            None,
            latency_ms,
            Some(1),
            None,
            None,
            (100 + i64::from(candidate_index)) * 1_000,
            started_at_unix_ms.map(|v| v * 1_000),
            started_at_unix_ms.map(|value| (value + 1) * 1_000),
        )
        .expect("candidate should build")
    }

    #[test]
    fn derive_final_status_prefers_success() {
        let candidates = vec![sample_candidate(
            "cand-1",
            "req-1",
            0,
            RequestCandidateStatus::Success,
            Some(100),
            Some(25),
            Some(200),
        )];

        assert_eq!(
            derive_request_candidate_final_status(&candidates),
            RequestCandidateFinalStatus::Success
        );
    }

    #[tokio::test]
    async fn read_request_candidate_trace_filters_attempted_rows() {
        let repository = Arc::new(InMemoryRequestCandidateRepository::seed(vec![
            sample_candidate(
                "cand-1",
                "req-1",
                0,
                RequestCandidateStatus::Pending,
                None,
                None,
                None,
            ),
            sample_candidate(
                "cand-2",
                "req-1",
                1,
                RequestCandidateStatus::Failed,
                Some(101),
                Some(33),
                Some(502),
            ),
        ]));
        let state = GatewayDataState::with_request_candidate_reader_for_tests(repository);

        let trace = read_request_candidate_trace(&state, "req-1", true)
            .await
            .expect("trace should succeed")
            .expect("trace should exist");

        assert_eq!(trace.total_candidates, 1);
        assert_eq!(trace.candidates[0].id, "cand-2");
        assert_eq!(trace.final_status, RequestCandidateFinalStatus::Failed);
        assert_eq!(trace.total_latency_ms, 33);
    }

    #[tokio::test]
    async fn attempted_or_skipped_scope_keeps_planner_skips_but_hides_unused_rows() {
        let mut quota_skipped = sample_candidate(
            "cand-quota",
            "req-scope",
            0,
            RequestCandidateStatus::Skipped,
            None,
            None,
            None,
        );
        quota_skipped.skip_reason = Some("provider_quota_blocked".to_string());
        let mut quota_skipped_again = sample_candidate(
            "cand-quota-again",
            "req-scope",
            1,
            RequestCandidateStatus::Skipped,
            None,
            None,
            None,
        );
        quota_skipped_again.skip_reason = Some("provider_quota_blocked".to_string());
        let repository = Arc::new(InMemoryRequestCandidateRepository::seed(vec![
            quota_skipped,
            quota_skipped_again,
            sample_candidate(
                "cand-unused",
                "req-scope",
                2,
                RequestCandidateStatus::Unused,
                None,
                None,
                None,
            ),
            sample_candidate(
                "cand-failed",
                "req-scope",
                3,
                RequestCandidateStatus::Failed,
                Some(101),
                Some(33),
                Some(502),
            ),
        ]));
        let state = GatewayDataState::with_request_candidate_reader_for_tests(repository);

        let trace = read_request_candidate_trace_in_scope(
            &state,
            "req-scope",
            RequestCandidateTraceScope::AttemptedOrSkipped,
        )
        .await
        .expect("trace should succeed")
        .expect("trace should exist");

        assert_eq!(
            trace
                .candidates
                .iter()
                .map(|candidate| candidate.id.as_str())
                .collect::<Vec<_>>(),
            vec!["cand-quota", "cand-failed"]
        );
        assert_eq!(trace.total_candidates, 2);
        assert_eq!(trace.final_status, RequestCandidateFinalStatus::Failed);
        assert_eq!(trace.total_latency_ms, 33);
    }

    #[tokio::test]
    async fn request_candidate_trace_recovers_missing_transport_latency_from_timestamps() {
        let repository = Arc::new(InMemoryRequestCandidateRepository::seed(vec![
            sample_candidate(
                "cand-timeout",
                "req-failover",
                0,
                RequestCandidateStatus::Failed,
                Some(100),
                None,
                None,
            ),
            sample_candidate(
                "cand-success",
                "req-failover",
                1,
                RequestCandidateStatus::Success,
                Some(101),
                Some(626),
                Some(200),
            ),
        ]));
        let state = GatewayDataState::with_request_candidate_reader_for_tests(repository);

        let trace = read_request_candidate_trace(&state, "req-failover", true)
            .await
            .expect("trace should succeed")
            .expect("trace should exist");

        assert_eq!(trace.total_candidates, 2);
        assert_eq!(trace.final_status, RequestCandidateFinalStatus::Success);
        assert_eq!(trace.total_latency_ms, 1_626);
    }
}
