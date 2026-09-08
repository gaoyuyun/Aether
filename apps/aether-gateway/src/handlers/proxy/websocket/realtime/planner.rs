//! Candidate planning for the public OpenAI Realtime WebSocket transport.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use axum::http::{HeaderValue, Method};
use serde_json::json;

use crate::ai_serving::{
    build_standard_stream_plan_from_decision, maybe_build_stream_decision_payload,
    AiExecutionDecision,
};
use crate::control::GatewayControlDecision;
use crate::headers::request_origin_from_headers_and_remote_addr;
use crate::privacy::RedactionSessionSlot;
use crate::{AppState, GatewayError};

pub(super) struct PlannedRealtimeCandidate {
    pub(super) execution: AiExecutionDecision,
    pub(super) admission_plan: aether_contracts::ExecutionPlan,
    pub(super) provider_id: String,
    pub(super) endpoint_id: String,
    pub(super) key_id: String,
    pub(super) provider_model: String,
    pub(super) pool_lease: RealtimePoolLeaseGuard,
}

pub(super) struct RealtimePoolLeaseGuard {
    state: AppState,
    report_context: Option<serde_json::Value>,
    renewal_task: Option<tokio::task::JoinHandle<()>>,
    healthy: Arc<AtomicBool>,
    armed: bool,
}

impl RealtimePoolLeaseGuard {
    fn new(state: &AppState, decision: &AiExecutionDecision) -> Self {
        let report_context = decision.report_context.clone();
        let lease = crate::orchestration::local_execution_candidate_metadata_from_report_context(
            report_context.as_ref(),
        )
        .pool_key_lease;
        let healthy = Arc::new(AtomicBool::new(true));
        let renewal_task = lease.map(|lease| {
            let runtime_state = Arc::clone(&state.runtime_state);
            let healthy = Arc::clone(&healthy);
            tokio::spawn(async move {
                let ttl = Duration::from_millis(lease.ttl_ms);
                let interval = Duration::from_millis((lease.ttl_ms / 3).max(1));
                loop {
                    tokio::time::sleep(interval).await;
                    match runtime_state.lock_renew(&lease, ttl).await {
                        Ok(true) => {}
                        Ok(false) | Err(_) => {
                            healthy.store(false, Ordering::Release);
                            return;
                        }
                    }
                }
            })
        });
        Self {
            state: state.clone(),
            report_context,
            renewal_task,
            healthy,
            armed: true,
        }
    }

    pub(super) fn is_healthy(&self) -> bool {
        self.healthy.load(Ordering::Acquire)
    }

    pub(super) async fn release(mut self) {
        if let Some(task) = self.renewal_task.take() {
            task.abort();
        }
        crate::orchestration::release_pool_key_lease_from_report_context(
            &self.state,
            self.report_context.as_ref(),
        )
        .await;
        self.armed = false;
    }
}

impl Drop for RealtimePoolLeaseGuard {
    fn drop(&mut self) {
        if let Some(task) = self.renewal_task.take() {
            task.abort();
        }
        if !self.armed {
            return;
        }
        let state = self.state.clone();
        let report_context = self.report_context.take();
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            runtime.spawn(async move {
                crate::orchestration::release_pool_key_lease_from_report_context(
                    &state,
                    report_context.as_ref(),
                )
                .await;
            });
        }
    }
}

pub(super) async fn plan_realtime_candidate(
    state: &AppState,
    context: &crate::handlers::proxy::websocket::ingress::WebSocketRequestContext,
    client_model: &str,
) -> Result<Option<PlannedRealtimeCandidate>, GatewayError> {
    let parts = realtime_planning_parts(context);
    let body = json!({"model": client_model});
    let Some(execution) = maybe_build_stream_decision_payload(
        state,
        &parts,
        context.trace_id.as_str(),
        &context.decision,
        &body,
        None,
    )
    .await?
    else {
        return Ok(None);
    };
    if execution
        .provider_api_format
        .as_deref()
        .map(crate::ai_serving::normalize_api_format_alias)
        .as_deref()
        != Some("openai:realtime")
    {
        crate::orchestration::release_pool_key_lease_from_report_context(
            state,
            execution.report_context.as_ref(),
        )
        .await;
        return Ok(None);
    }

    let pool_lease = RealtimePoolLeaseGuard::new(state, &execution);
    let Some(attempt) =
        build_standard_stream_plan_from_decision(&parts, &body, execution.clone(), false)?
    else {
        pool_lease.release().await;
        return Ok(None);
    };
    let provider_id = execution.provider_id.clone().unwrap_or_default();
    let endpoint_id = execution.endpoint_id.clone().unwrap_or_default();
    let key_id = execution.key_id.clone().unwrap_or_default();
    let provider_model = execution
        .mapped_model
        .clone()
        .or_else(|| execution.model_name.clone())
        .unwrap_or_default();
    if provider_id.is_empty()
        || endpoint_id.is_empty()
        || key_id.is_empty()
        || provider_model.trim().is_empty()
        || execution.upstream_url.as_deref().is_none_or(str::is_empty)
    {
        pool_lease.release().await;
        return Ok(None);
    }

    Ok(Some(PlannedRealtimeCandidate {
        execution,
        admission_plan: attempt.plan,
        provider_id,
        endpoint_id,
        key_id,
        provider_model,
        pool_lease,
    }))
}

fn realtime_planning_parts(
    context: &crate::handlers::proxy::websocket::ingress::WebSocketRequestContext,
) -> http::request::Parts {
    let mut request = http::Request::builder()
        .method(Method::GET)
        .uri(context.uri.clone())
        .body(())
        .expect("the authenticated Realtime URI must remain valid");
    *request.headers_mut() = context.headers.clone();
    request.headers_mut().insert(
        http::header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    request
        .extensions_mut()
        .insert(request_origin_from_headers_and_remote_addr(
            &context.headers,
            &context.remote_addr,
        ));
    request
        .extensions_mut()
        .insert(RedactionSessionSlot::default());
    request.into_parts().0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn realtime_planning_requires_the_explicit_realtime_route() {
        let request = http::Request::builder()
            .method(Method::GET)
            .uri("/v1/realtime?model=gpt-realtime")
            .body(())
            .unwrap();
        let (parts, _) = request.into_parts();
        let decision = GatewayControlDecision {
            public_path: "/v1/realtime".to_string(),
            public_query_string: Some("model=gpt-realtime".to_string()),
            route_class: Some("ai_public".to_string()),
            route_family: Some("openai".to_string()),
            route_kind: Some("realtime".to_string()),
            client_surface: None,
            api_operation: None,
            gateway_credential_carrier: None,
            request_auth_channel: None,
            auth_context: None,
            admin_principal: None,
            auth_endpoint_signature: None,
            execution_runtime_candidate: true,
            local_auth_rejection: None,
            model_directive_policy: Default::default(),
        };

        assert_eq!(
            crate::ai_serving::resolve_execution_runtime_stream_plan_kind_with_client_surface(
                decision.route_class.as_deref(),
                decision.route_family.as_deref(),
                decision.route_kind.as_deref(),
                decision.client_surface,
                decision.request_auth_channel.as_deref(),
                &parts.method,
                parts.uri.path(),
            ),
            Some(crate::ai_serving::OPENAI_REALTIME_STREAM_PLAN_KIND)
        );
    }
}
