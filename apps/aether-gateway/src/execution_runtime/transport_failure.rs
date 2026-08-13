use std::collections::BTreeMap;
use std::future::Future;
use std::sync::{
    atomic::{AtomicBool, AtomicU64, Ordering},
    Arc,
};

use aether_data_contracts::repository::candidates::RequestCandidateStatus;
use aether_scheduler_core::SchedulerRequestCandidateStatusUpdate;
use aether_usage_runtime::{build_usage_event_data_seed, UsageEvent, UsageEventType};
use axum::body::Body;
use axum::http::Response;
use serde_json::{json, Value};
use tokio::sync::Notify;

use crate::ai_serving::{build_core_error_body_for_client_format, LocalCoreSyncErrorKind};
use crate::api::response::{attach_control_metadata_headers, build_client_response_from_parts};
use crate::control::GatewayControlDecision;
use crate::request_candidate_runtime::record_request_terminal_local_request_candidate_status;
use crate::request_diagnostics::attach_current_request_diagnostics_and_candidate_timing_to_report_context;
use crate::{AppState, GatewayError};

const TRANSPORT_ERROR_CLIENT_MESSAGE: &str =
    "Upstream transport failed before an HTTP response was received";

#[derive(Debug, Default)]
pub(crate) struct StreamCandidateWatchdogProgress {
    terminal_started: AtomicBool,
    timed_out: AtomicBool,
    precise_timeout_armed: AtomicBool,
    deadline_generation: AtomicU64,
    deadline_changed: Notify,
}

tokio::task_local! {
    static STREAM_CANDIDATE_WATCHDOG_PROGRESS: Arc<StreamCandidateWatchdogProgress>;
}

impl StreamCandidateWatchdogProgress {
    pub(crate) fn shared() -> Arc<Self> {
        Arc::new(Self::default())
    }

    pub(crate) fn terminal_started(&self) -> bool {
        self.terminal_started.load(Ordering::Acquire)
    }

    pub(crate) fn timed_out(&self) -> bool {
        self.timed_out.load(Ordering::Acquire)
    }

    pub(crate) fn mark_timed_out(&self) {
        self.timed_out.store(true, Ordering::Release);
    }

    pub(crate) fn precise_timeout_armed(&self) -> bool {
        self.precise_timeout_armed.load(Ordering::Acquire)
    }

    pub(crate) async fn deadline_changed_since(&self, observed_generation: u64) -> u64 {
        loop {
            let notified = self.deadline_changed.notified();
            let generation = self.deadline_generation.load(Ordering::Acquire);
            if generation != observed_generation {
                return generation;
            }
            notified.await;
        }
    }

    fn restart_deadline(&self, precise_timeout_armed: bool) {
        if precise_timeout_armed {
            self.precise_timeout_armed.store(true, Ordering::Release);
        }
        self.deadline_generation.fetch_add(1, Ordering::AcqRel);
        self.deadline_changed.notify_waiters();
    }

    pub(crate) async fn scope<F>(self: Arc<Self>, future: F) -> F::Output
    where
        F: Future,
    {
        STREAM_CANDIDATE_WATCHDOG_PROGRESS.scope(self, future).await
    }
}

pub(crate) fn current_stream_candidate_watchdog_progress(
) -> Option<Arc<StreamCandidateWatchdogProgress>> {
    STREAM_CANDIDATE_WATCHDOG_PROGRESS.try_with(Arc::clone).ok()
}

pub(crate) fn mark_stream_candidate_watchdog_upstream_started() {
    let _ = STREAM_CANDIDATE_WATCHDOG_PROGRESS.try_with(|progress| {
        progress.restart_deadline(false);
    });
}

pub(crate) fn mark_stream_candidate_watchdog_precise_timeout_armed() {
    let _ = STREAM_CANDIDATE_WATCHDOG_PROGRESS.try_with(|progress| {
        progress.restart_deadline(true);
    });
}

pub(crate) fn mark_stream_candidate_watchdog_terminal_started() {
    let _ = STREAM_CANDIDATE_WATCHDOG_PROGRESS.try_with(|progress| {
        progress.terminal_started.store(true, Ordering::Release);
    });
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn build_transport_error_stop_response(
    state: &AppState,
    plan: &aether_contracts::ExecutionPlan,
    report_context: Option<&Value>,
    trace_id: &str,
    decision: &GatewayControlDecision,
    client_status_code: u16,
    error_type: &str,
    error_message: &str,
    elapsed_ms: u64,
) -> Result<Response<Body>, GatewayError> {
    mark_stream_candidate_watchdog_terminal_started();
    let client_body = build_core_error_body_for_client_format(
        &plan.client_api_format,
        TRANSPORT_ERROR_CLIENT_MESSAGE,
        Some("upstream_transport_error"),
        LocalCoreSyncErrorKind::ServerError,
    )
    .unwrap_or_else(|| {
        json!({
            "error": {
                "type": "server_error",
                "message": TRANSPORT_ERROR_CLIENT_MESSAGE,
                "code": "upstream_transport_error",
            }
        })
    });
    let body_bytes =
        serde_json::to_vec(&client_body).map_err(|err| GatewayError::Internal(err.to_string()))?;
    let headers = BTreeMap::from([
        ("content-type".to_string(), "application/json".to_string()),
        ("content-length".to_string(), body_bytes.len().to_string()),
    ]);
    let terminal_unix_ms = crate::clock::current_unix_ms();
    record_request_terminal_local_request_candidate_status(
        state,
        plan,
        report_context,
        SchedulerRequestCandidateStatusUpdate {
            status: RequestCandidateStatus::Failed,
            status_code: Some(client_status_code),
            error_type: Some(error_type.to_string()),
            error_message: Some(error_message.to_string()),
            latency_ms: Some(elapsed_ms),
            // Preserve the exact start timestamp written when the candidate began. The elapsed
            // duration is sufficient when this terminal update has to create the row itself.
            started_at_unix_ms: None,
            finished_at_unix_ms: Some(terminal_unix_ms),
        },
    )
    .await;

    if state.usage_runtime.is_enabled() {
        let report_context_with_diagnostics =
            attach_current_request_diagnostics_and_candidate_timing_to_report_context(
                report_context,
                Some(elapsed_ms),
                None,
            );
        let mut usage_data = build_usage_event_data_seed(
            plan,
            report_context_with_diagnostics.as_ref().or(report_context),
        );
        usage_data.status_code = Some(client_status_code);
        usage_data.error_message = Some(error_message.to_string());
        usage_data.error_category = Some("server_error".to_string());
        usage_data.response_time_ms = Some(elapsed_ms);
        usage_data.response_headers = None;
        usage_data.response_body = None;
        usage_data.client_response_headers = Some(json!({"content-type": "application/json"}));
        usage_data.client_response_body = Some(client_body);
        let mut request_metadata = match usage_data.request_metadata.take() {
            Some(Value::Object(object)) => object,
            Some(other) => serde_json::Map::from_iter([("seed".to_string(), other)]),
            None => serde_json::Map::new(),
        };
        request_metadata.insert("transport_error".to_string(), Value::Bool(true));
        request_metadata.insert(
            "transport_error_type".to_string(),
            Value::String(error_type.to_string()),
        );
        usage_data.request_metadata = Some(Value::Object(request_metadata));
        state
            .usage_runtime
            .record_terminal_event_direct(
                state.usage_lifecycle_data_state().as_ref(),
                UsageEvent::new(UsageEventType::Failed, plan.request_id.clone(), usage_data),
            )
            .await;
    }

    attach_control_metadata_headers(
        build_client_response_from_parts(
            client_status_code,
            &headers,
            Body::from(body_bytes),
            trace_id,
            Some(decision),
        )?,
        Some(plan.request_id.as_str()),
        plan.candidate_id.as_deref(),
    )
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::sync::Arc;

    use aether_contracts::{ExecutionPlan, RequestBody};
    use aether_data::repository::candidates::InMemoryRequestCandidateRepository;
    use aether_data_contracts::repository::candidates::{
        RequestCandidateReadRepository, RequestCandidateStatus,
    };
    use serde_json::json;

    use super::build_transport_error_stop_response;
    use crate::control::GatewayControlDecision;
    use crate::data::GatewayDataState;
    use crate::request_candidate_runtime::request_candidate_marks_request_terminal;
    use crate::AppState;

    #[tokio::test]
    async fn transport_stop_response_marks_owning_candidate_request_terminal() {
        let repository = Arc::new(InMemoryRequestCandidateRepository::default());
        let state = AppState::new()
            .expect("state should build")
            .with_data_state_for_tests(
                GatewayDataState::with_request_candidate_repository_for_tests(Arc::clone(
                    &repository,
                )),
            );
        let plan = ExecutionPlan {
            request_id: "request-transport-stop".to_string(),
            candidate_id: Some("candidate-transport-stop".to_string()),
            provider_name: Some("custom".to_string()),
            provider_id: "provider-transport-stop".to_string(),
            endpoint_id: "endpoint-transport-stop".to_string(),
            key_id: "key-transport-stop".to_string(),
            method: "POST".to_string(),
            url: "https://provider.example/v1/chat/completions".to_string(),
            headers: BTreeMap::new(),
            content_type: Some("application/json".to_string()),
            content_encoding: None,
            body: RequestBody::from_json(json!({"model": "gpt-5"})),
            stream: false,
            client_api_format: "openai:chat".to_string(),
            provider_api_format: "openai:chat".to_string(),
            model_name: Some("gpt-5".to_string()),
            proxy: None,
            transport_profile: None,
            timeouts: None,
        };
        let report_context = json!({
            "candidate_index": 0,
            "retry_index": 0,
        });
        let response = build_transport_error_stop_response(
            &state,
            &plan,
            Some(&report_context),
            "trace-transport-stop",
            &GatewayControlDecision::synthetic(
                "/v1/chat/completions",
                Some("ai_public".to_string()),
                Some("openai".to_string()),
                Some("chat".to_string()),
                Some("openai:chat".to_string()),
            ),
            504,
            "local_stream_candidate_watchdog_timeout",
            "Stream first byte timeout",
            250,
        )
        .await
        .expect("transport stop response should build");

        assert_eq!(response.status().as_u16(), 504);
        let candidates = repository
            .list_by_request_id(plan.request_id.as_str())
            .await
            .expect("request candidates should read");
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].status, RequestCandidateStatus::Failed);
        assert_eq!(candidates[0].status_code, Some(504));
        assert!(request_candidate_marks_request_terminal(&candidates[0]));
    }
}
