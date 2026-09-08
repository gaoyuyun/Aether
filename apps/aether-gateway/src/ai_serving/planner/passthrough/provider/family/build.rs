use crate::ai_serving::planner::common::extract_requested_model_from_request;
use crate::ai_serving::planner::runtime_miss::{
    apply_local_runtime_candidate_evaluation_progress_preserving_candidate_signal,
    apply_local_runtime_candidate_terminal_reason, set_local_runtime_miss_diagnostic_reason,
};
use crate::ai_serving::planner::spec_metadata::local_same_format_provider_spec_metadata;
use crate::ai_serving::GatewayControlDecision;
use crate::{AiExecutionDecision, AppState, GatewayError};

use super::super::plans::{resolve_stream_spec, resolve_sync_spec};
use super::candidates::{
    build_local_same_format_provider_candidate_attempt_source,
    resolve_local_same_format_provider_decision_input,
};
use super::payload::maybe_build_local_same_format_provider_decision_payload_for_candidate;

pub(crate) async fn maybe_build_sync_local_same_format_provider_decision_payload(
    state: &AppState,
    parts: &http::request::Parts,
    trace_id: &str,
    decision: &GatewayControlDecision,
    body_json: &serde_json::Value,
    plan_kind: &str,
) -> Result<Option<AiExecutionDecision>, GatewayError> {
    let Some(spec) = resolve_sync_spec(plan_kind) else {
        return Ok(None);
    };
    let spec_metadata = local_same_format_provider_spec_metadata(spec);
    let requested_model_family = spec_metadata
        .requested_model_family
        .expect("same-format provider spec metadata should include requested-model family");

    let Some(input) = resolve_local_same_format_provider_decision_input(
        state, parts, trace_id, decision, body_json, spec,
    )
    .await?
    else {
        set_local_runtime_miss_diagnostic_reason(
            state,
            trace_id,
            decision,
            spec_metadata.decision_kind,
            extract_requested_model_from_request(parts, body_json, requested_model_family)
                .as_deref(),
            "decision_input_unavailable",
        );
        return Ok(None);
    };

    set_local_runtime_miss_diagnostic_reason(
        state,
        trace_id,
        decision,
        spec_metadata.decision_kind,
        Some(input.requested_model.as_str()),
        "candidate_evaluation_incomplete",
    );
    let body_json = input.effective_body_json(body_json);
    let (mut source, candidate_count) = build_local_same_format_provider_candidate_attempt_source(
        state, trace_id, &input, body_json, spec,
    )
    .await?;
    apply_local_runtime_candidate_evaluation_progress_preserving_candidate_signal(
        state,
        trace_id,
        candidate_count,
    );

    while let Some(attempt) = source.next_attempt().await? {
        if let Some(payload) =
            maybe_build_local_same_format_provider_decision_payload_for_candidate(
                state, parts, trace_id, body_json, &input, attempt, spec,
            )
            .await?
        {
            return Ok(Some(payload));
        }
    }

    apply_local_runtime_candidate_terminal_reason(state, trace_id, "no_local_sync_plans");

    Ok(None)
}

pub(crate) async fn maybe_build_stream_local_same_format_provider_decision_payload(
    state: &AppState,
    parts: &http::request::Parts,
    trace_id: &str,
    decision: &GatewayControlDecision,
    body_json: &serde_json::Value,
    plan_kind: &str,
) -> Result<Option<AiExecutionDecision>, GatewayError> {
    maybe_build_pinned_stream_local_same_format_provider_decision_payload(
        state, parts, trace_id, decision, body_json, plan_kind, None,
    )
    .await
}

pub(crate) async fn maybe_build_pinned_stream_local_same_format_provider_decision_payload(
    state: &AppState,
    parts: &http::request::Parts,
    trace_id: &str,
    decision: &GatewayControlDecision,
    body_json: &serde_json::Value,
    plan_kind: &str,
    pinned_candidate: Option<(&str, &str, &str)>,
) -> Result<Option<AiExecutionDecision>, GatewayError> {
    let Some(spec) = resolve_stream_spec(plan_kind) else {
        return Ok(None);
    };
    let spec_metadata = local_same_format_provider_spec_metadata(spec);
    let requested_model_family = spec_metadata
        .requested_model_family
        .expect("same-format provider spec metadata should include requested-model family");

    let Some(input) = resolve_local_same_format_provider_decision_input(
        state, parts, trace_id, decision, body_json, spec,
    )
    .await?
    else {
        set_local_runtime_miss_diagnostic_reason(
            state,
            trace_id,
            decision,
            spec_metadata.decision_kind,
            extract_requested_model_from_request(parts, body_json, requested_model_family)
                .as_deref(),
            "decision_input_unavailable",
        );
        return Ok(None);
    };

    set_local_runtime_miss_diagnostic_reason(
        state,
        trace_id,
        decision,
        spec_metadata.decision_kind,
        Some(input.requested_model.as_str()),
        "candidate_evaluation_incomplete",
    );
    let body_json = input.effective_body_json(body_json);
    let (mut source, candidate_count) = build_local_same_format_provider_candidate_attempt_source(
        state, trace_id, &input, body_json, spec,
    )
    .await?;
    apply_local_runtime_candidate_evaluation_progress_preserving_candidate_signal(
        state,
        trace_id,
        candidate_count,
    );

    while let Some(attempt) = source.next_attempt().await? {
        if let Some(payload) =
            maybe_build_local_same_format_provider_decision_payload_for_candidate(
                state, parts, trace_id, body_json, &input, attempt, spec,
            )
            .await?
        {
            if pinned_candidate.is_some_and(|(provider_id, endpoint_id, key_id)| {
                payload.provider_id.as_deref() != Some(provider_id)
                    || payload.endpoint_id.as_deref() != Some(endpoint_id)
                    || payload.key_id.as_deref() != Some(key_id)
            }) {
                crate::orchestration::release_pool_key_lease_from_report_context(
                    state,
                    payload.report_context.as_ref(),
                )
                .await;
                continue;
            }
            return Ok(Some(payload));
        }
    }

    apply_local_runtime_candidate_terminal_reason(state, trace_id, "no_local_stream_plans");

    Ok(None)
}
