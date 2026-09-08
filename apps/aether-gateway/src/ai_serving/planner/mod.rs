use crate::ai_serving::{AiExecutionDecision, AiExecutionPlanPayload, GatewayControlDecision};
use crate::{AppState, GatewayError};

mod antigravity;
mod candidate_affinity_cache;
mod candidate_materialization;
mod candidate_metadata;
mod candidate_preparation;
mod candidate_ranking;
mod candidate_resolution;
mod candidate_source;
mod candidate_transport_ranking_facts;
mod common;
mod decision;
mod decision_input;
mod gemini_cli;
mod materialization_policy;
mod passthrough;
mod plan_builders;
mod pool_scheduler;
pub(crate) mod pool_scores;
mod redaction;
mod report_context;
mod request_gzip;
mod route;
mod runtime_miss;
mod spec_metadata;
mod specialized;
mod standard;
mod state;

pub(crate) use self::candidate_materialization::LocalExecutionAttemptSource;
pub(crate) use self::candidate_resolution::{
    candidate_auth_channel_skip_reason, read_candidate_transport_snapshot,
    EligibleLocalExecutionCandidate, LocalExecutionCandidateKind, SkippedLocalExecutionCandidate,
};
pub(crate) use self::common::resolve_upstream_is_stream_for_provider;
pub(crate) use self::passthrough::{
    build_local_same_format_stream_attempt_source, build_local_same_format_stream_plan_and_reports,
    build_local_same_format_sync_attempt_source, build_local_same_format_sync_plan_and_reports,
    maybe_build_pinned_stream_local_same_format_provider_decision_payload,
};
pub(crate) use self::plan_builders::{
    build_gemini_stream_plan_from_decision, build_gemini_sync_plan_from_decision,
    build_openai_responses_stream_plan_from_decision,
    build_openai_responses_sync_plan_from_decision, build_passthrough_sync_plan_from_decision,
    build_standard_stream_plan_from_decision, build_standard_sync_plan_from_decision,
    AiStreamAttempt, AiSyncAttempt,
};
pub(crate) use self::pool_scores::{
    build_provider_key_pool_score_upsert, provider_key_pool_score_id, provider_key_pool_score_scope,
};
pub(crate) use self::redaction::resolve_provider_chat_pii_redaction;
pub(crate) use self::request_gzip::resolve_transport_request_encoding_policy;
pub(crate) use self::route::is_matching_stream_request as planner_is_matching_stream_request;
pub(crate) use self::runtime_miss::{
    apply_local_runtime_candidate_terminal_reason, record_local_runtime_candidate_skip_reason,
};
pub(crate) use self::specialized::{
    build_local_gemini_files_stream_attempt_source_for_kind,
    build_local_gemini_files_stream_plan_and_reports_for_kind,
    build_local_gemini_files_sync_attempt_source_for_kind,
    build_local_gemini_files_sync_plan_and_reports_for_kind,
    build_local_image_stream_attempt_source_for_kind,
    build_local_image_stream_plan_and_reports_for_kind,
    build_local_image_sync_attempt_source_for_kind,
    build_local_image_sync_plan_and_reports_for_kind,
    build_local_video_sync_attempt_source_for_kind,
    build_local_video_sync_plan_and_reports_for_kind,
    set_local_openai_image_execution_exhausted_diagnostic,
};
pub(crate) use self::standard::{
    build_local_openai_chat_stream_attempt_source_for_kind,
    build_local_openai_chat_stream_plan_and_reports_for_kind,
    build_local_openai_chat_sync_attempt_source_for_kind,
    build_local_openai_chat_sync_plan_and_reports_for_kind,
    build_local_openai_responses_stream_attempt_source_for_kind,
    build_local_openai_responses_stream_plan_and_reports_for_kind,
    build_local_openai_responses_sync_attempt_source_for_kind,
    build_local_openai_responses_sync_plan_and_reports_for_kind,
    build_local_stream_attempt_source as build_standard_family_stream_attempt_source,
    build_local_stream_plan_and_reports as build_standard_family_stream_plan_and_reports,
    build_local_sync_attempt_source as build_standard_family_sync_attempt_source,
    build_local_sync_plan_and_reports as build_standard_family_sync_plan_and_reports,
    codex_model_capabilities_for_transport, maybe_build_responses_websocket_decision,
    openai_responses_reasoning_replay_policy, set_local_openai_chat_execution_exhausted_diagnostic,
    validate_final_openai_provider_request, ResponsesWebSocketBodyNormalization,
    ResponsesWebSocketDecision, ResponsesWebSocketPinnedCandidate,
};
pub(crate) use self::state::{
    GatewayAuthApiKeySnapshot, GatewayProviderTransportSnapshot, LocalResolvedOAuthRequestAuth,
    PlannerAppState,
};
pub(crate) use aether_ai_serving::extract_ai_pool_sticky_session_token as extract_pool_sticky_session_token;
pub(crate) use aether_ai_serving::{
    build_ai_execution_decision_response, AiExecutionDecisionResponseParts,
    CandidateFailureDiagnostic, CandidateFailureDiagnosticKind,
};

pub(crate) struct ResolvedTunnelSchedulerAffinityContext {
    pub(crate) requested_model: String,
    pub(crate) client_session_affinity: Option<aether_scheduler_core::ClientSessionAffinity>,
    pub(crate) policy_context: Option<crate::scheduler::affinity::SchedulerAffinityPolicyContext>,
    pub(crate) routing_overlay: Option<aether_routing_core::RankingOverlay>,
}

pub(crate) async fn resolve_tunnel_scheduler_affinity_context(
    state: &AppState,
    parts: &http::request::Parts,
    decision: &GatewayControlDecision,
    requested_model: String,
    body_json: &serde_json::Value,
    client_api_format: &str,
) -> Result<Option<ResolvedTunnelSchedulerAffinityContext>, GatewayError> {
    let Some(auth_context) = decision.auth_context.as_ref() else {
        return Ok(None);
    };
    let execution_auth_context =
        crate::ai_serving::build_execution_runtime_auth_context(auth_context);
    let Some(auth_snapshot) = state
        .read_cached_auth_api_key_snapshot(
            &execution_auth_context.user_id,
            &execution_auth_context.api_key_id,
            crate::clock::current_unix_secs(),
        )
        .await?
    else {
        return Ok(None);
    };
    let resolved_auth_input = decision_input::ResolvedLocalDecisionAuthInput {
        auth_context: execution_auth_context,
        auth_snapshot,
        required_capabilities: None,
        model_directive_policy: decision.model_directive_policy.clone(),
    };
    let mut input = decision_input::build_local_requested_model_decision_input(
        resolved_auth_input,
        requested_model,
    );
    decision_input::attach_routing_policy_to_local_requested_model_input(
        state,
        parts,
        &mut input,
        body_json,
        client_api_format,
    )
    .await?;
    let policy_context = input
        .routing_policy
        .as_ref()
        .map(crate::scheduler::affinity::SchedulerAffinityPolicyContext::from_routing_policy);
    let routing_overlay = input
        .routing_policy
        .as_ref()
        .map(|policy| policy.ranking_overlay.clone());

    Ok(Some(ResolvedTunnelSchedulerAffinityContext {
        requested_model: input.requested_model,
        client_session_affinity: input.client_session_affinity,
        policy_context,
        routing_overlay,
    }))
}

pub(crate) async fn maybe_build_sync_decision_payload(
    state: &AppState,
    parts: &http::request::Parts,
    trace_id: &str,
    decision: &GatewayControlDecision,
    body_json: &serde_json::Value,
    body_base64: Option<&str>,
    body_is_empty: bool,
) -> Result<Option<AiExecutionDecision>, GatewayError> {
    decision::maybe_build_sync_decision_payload(
        state,
        parts,
        trace_id,
        decision,
        body_json,
        body_base64,
        body_is_empty,
    )
    .await
}

pub(crate) async fn maybe_build_stream_decision_payload(
    state: &AppState,
    parts: &http::request::Parts,
    trace_id: &str,
    decision: &GatewayControlDecision,
    body_json: &serde_json::Value,
    body_base64: Option<&str>,
) -> Result<Option<AiExecutionDecision>, GatewayError> {
    decision::maybe_build_stream_decision_payload(
        state,
        parts,
        trace_id,
        decision,
        body_json,
        body_base64,
    )
    .await
}

pub(crate) async fn maybe_build_sync_plan_payload(
    state: &AppState,
    parts: &http::request::Parts,
    trace_id: &str,
    decision: &GatewayControlDecision,
    body_json: &serde_json::Value,
    body_base64: Option<&str>,
    body_is_empty: bool,
) -> Result<Option<AiExecutionPlanPayload>, GatewayError> {
    decision::maybe_build_sync_plan_payload_impl(
        state,
        parts,
        trace_id,
        decision,
        body_json,
        body_base64,
        body_is_empty,
    )
    .await
}

pub(crate) async fn maybe_build_stream_plan_payload(
    state: &AppState,
    parts: &http::request::Parts,
    trace_id: &str,
    decision: &GatewayControlDecision,
    body_json: &serde_json::Value,
    body_base64: Option<&str>,
) -> Result<Option<AiExecutionPlanPayload>, GatewayError> {
    decision::maybe_build_stream_plan_payload_impl(
        state,
        parts,
        trace_id,
        decision,
        body_json,
        body_base64,
    )
    .await
}
