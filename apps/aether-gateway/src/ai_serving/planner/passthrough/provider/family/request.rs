use std::collections::BTreeMap;
use std::sync::Arc;

use aether_contracts::ResolvedTransportProfile;
use serde_json::Value;

use crate::ai_serving::planner::common::{
    enforce_provider_body_stream_policy, request_requires_body_stream_field,
};
use crate::ai_serving::planner::kiro_diagnostics::kiro_envelope_failure_diagnostic;
use crate::ai_serving::planner::redaction::{
    request_identity_response_encoding_when_redacted, resolve_provider_chat_pii_redaction,
};
use crate::ai_serving::transport::antigravity::{
    build_antigravity_safe_v1internal_request, build_antigravity_static_identity_headers,
    classify_local_antigravity_request_support, AntigravityEnvelopeRequestType,
    AntigravityRequestAuthUnsupportedReason, AntigravityRequestEnvelopeSupport,
    AntigravityRequestSideSupport, AntigravityRequestSideUnsupportedReason,
};
use crate::ai_serving::transport::{
    build_gemini_cli_v1internal_request, build_grok_browser_headers, build_grok_upstream_url,
    build_same_format_provider_headers, resolve_local_gemini_cli_request_auth,
    transport_api_operation_unsupported_reason, GeminiCliRequestAuth, GeminiCliRequestAuthSupport,
    GeminiCliRequestEnvelopeSupport, GrokHeaderInput, SameFormatProviderCompatibilityEdit,
    SameFormatProviderCompatibilityEditAction, SameFormatProviderHeadersInput,
    TransportOperationUnsupportedReason, GEMINI_CLI_USER_AGENT, GROK_CHAT_PATH,
};
use crate::ai_serving::{
    CandidateFailureDiagnostic, CandidateFailureDiagnosticKind, GatewayProviderTransportSnapshot,
};
use crate::{AppState, GatewayError};

mod policy;
mod prepare;

use self::prepare::prepare_local_same_format_provider_candidate;
use super::payload::{
    mark_skipped_local_same_format_provider_candidate,
    mark_skipped_local_same_format_provider_candidate_with_extra_data,
    mark_skipped_local_same_format_provider_candidate_with_failure_diagnostic,
};
use super::{
    LocalSameFormatProviderCandidateAttempt, LocalSameFormatProviderDecisionInput,
    LocalSameFormatProviderSpec,
};
use crate::ai_serving::planner::standard::{
    codex_model_capabilities_for_transport, openai_provider_request_contract_failure_extra_data,
    openai_responses_reasoning_replay_policy, same_format_provider_request_body_failure_extra_data,
};

pub(crate) fn resolve_same_format_provider_transport_unsupported_reason_for_trace(
    transport: &GatewayProviderTransportSnapshot,
    provider_api_format: &str,
) -> Option<&'static str> {
    let provider_api_format =
        match crate::ai_serving::normalize_api_format_alias(provider_api_format).as_str() {
            "openai:chat" => "openai:chat",
            "openai:responses" => "openai:responses",
            "openai:responses:compact" => "openai:responses:compact",
            "openai:search" => "openai:search",
            "openai:embedding" => "openai:embedding",
            "openai:rerank" => "openai:rerank",
            "claude:messages" => "claude:messages",
            "gemini:generate_content" => "gemini:generate_content",
            "gemini:embedding" => "gemini:embedding",
            "jina:embedding" => "jina:embedding",
            "jina:rerank" => "jina:rerank",
            "doubao:embedding" => "doubao:embedding",
            "aliyun:multimodal_embedding" => "aliyun:multimodal_embedding",
            _ => return Some("transport_api_format_unsupported"),
        };
    let behavior = policy::classify_same_format_provider_request_behavior(
        transport,
        provider_api_format,
        crate::ai_serving::planner::spec_metadata::LocalExecutionSurfaceSpecMetadata {
            api_format: provider_api_format,
            require_streaming: false,
            requested_model_family: None,
            decision_kind: "trace_candidate_metadata",
            report_kind: Some("trace_candidate_metadata"),
        },
        None,
    );
    if !behavior.is_antigravity
        && !behavior.is_claude_code_transport
        && !behavior.is_gemini_cli
        && !behavior.is_vertex
        && !behavior.is_kiro
    {
        return None;
    }

    let family = if provider_api_format.starts_with("gemini:") {
        crate::ai_serving::LocalSameFormatProviderFamily::Gemini
    } else {
        crate::ai_serving::LocalSameFormatProviderFamily::Standard
    };
    policy::same_format_provider_transport_unsupported_reason(
        &behavior,
        transport,
        family,
        provider_api_format,
    )
}

pub(crate) struct LocalSameFormatProviderCandidatePayloadParts {
    pub(super) transport: Arc<GatewayProviderTransportSnapshot>,
    pub(super) is_antigravity: bool,
    pub(super) is_gemini_cli: bool,
    pub(super) is_kiro: bool,
    pub(super) auth_header: Option<String>,
    pub(super) auth_value: Option<String>,
    pub(super) provider_api_format: String,
    pub(super) mapped_model: String,
    pub(super) report_kind: &'static str,
    pub(super) upstream_is_stream: bool,
    pub(super) upstream_url: String,
    pub(super) provider_request_headers: BTreeMap<String, String>,
    pub(super) provider_request_body: Value,
    pub(super) transport_profile: Option<ResolvedTransportProfile>,
    pub(super) compatibility_edits: Vec<SameFormatProviderCompatibilityEdit>,
    pub(super) request_redacted: bool,
    /// P2：Claude Code 客户端策略的结果，写入 `report_context.claude_code_cloak`。
    pub(super) claude_code_cloak_report: Option<Value>,
    /// P6：Antigravity 信封的敏感词混淆报告（Claude Code 的报告嵌在 `claude_code_cloak` 里）。
    pub(super) sensitive_words_obfuscation: Option<Value>,
}

pub(crate) async fn resolve_local_same_format_provider_candidate_payload_parts(
    state: &AppState,
    parts: &http::request::Parts,
    trace_id: &str,
    body_json: &serde_json::Value,
    input: &LocalSameFormatProviderDecisionInput,
    attempt: &LocalSameFormatProviderCandidateAttempt,
    spec: LocalSameFormatProviderSpec,
) -> Result<Option<LocalSameFormatProviderCandidatePayloadParts>, GatewayError> {
    let candidate = &attempt.eligible.candidate;
    if let Some((skip_reason, unsupported_reason)) = same_format_provider_operation_skip_reason(
        &attempt.eligible.transport,
        attempt.eligible.provider_api_format.as_str(),
        spec.operation,
    ) {
        mark_skipped_local_same_format_provider_candidate_with_failure_diagnostic(
            state,
            input,
            trace_id,
            candidate,
            attempt.candidate_index,
            &attempt.candidate_id,
            skip_reason,
            same_format_provider_operation_failure_diagnostic(
                &attempt.eligible.transport,
                attempt.eligible.provider_api_format.as_str(),
                spec.operation,
                unsupported_reason,
            ),
        )
        .await;
        return Ok(None);
    }
    let Some(prepared) = prepare_local_same_format_provider_candidate(
        state,
        trace_id,
        input,
        &attempt.eligible,
        attempt.candidate_index,
        &attempt.candidate_id,
        spec,
    )
    .await
    else {
        return Ok(None);
    };
    let model_directive_resolution = input
        .model_directive_policy
        .resolve_reasoning(spec.api_format, Some(&input.requested_model));
    let model_directive_mapping =
        match model_directive_resolution.mapping_patch_for_mapped_model(&prepared.mapped_model) {
            Ok(mapping) => mapping,
            Err(skip_reason) => {
                mark_skipped_local_same_format_provider_candidate(
                    state,
                    input,
                    trace_id,
                    candidate,
                    attempt.candidate_index,
                    &attempt.candidate_id,
                    skip_reason,
                )
                .await;
                return Ok(None);
            }
        };
    let effective_headers = input.effective_headers(&parts.headers);
    let reasoning_replay_policy = openai_responses_reasoning_replay_policy(
        prepared.transport.provider.provider_type.as_str(),
        prepared.transport.endpoint.base_url.as_str(),
        prepared.mapped_model.as_str(),
    );
    let redaction = resolve_provider_chat_pii_redaction(
        state,
        parts,
        body_json,
        &input.auth_context,
        spec.api_format,
        reasoning_replay_policy,
        &attempt.candidate_id,
    )
    .await?;
    let body_json = redaction.body_json.as_ref();
    let mut transport = Arc::clone(&prepared.transport);

    // P2：先识别客户端。原生 Claude Code 连历史的 body 清洗都要跳过，保证逐字节透传。
    let claude_code_policy =
        crate::ai_serving::planner::claude_code_cloak::resolve_claude_code_client_policy(
            &transport,
            effective_headers,
            Some(body_json),
            spec.operation,
        );
    let claude_code_native_passthrough = claude_code_policy
        .as_ref()
        .is_some_and(|policy| policy.native_passthrough());

    let Some(base_provider_request) =
        super::super::request::build_same_format_provider_request_body_with_compatibility_report(
            body_json,
            prepared.provider_api_format.as_str(),
            &prepared.mapped_model,
            spec,
            prepared.transport.endpoint.body_rules.as_ref(),
            Some(effective_headers),
            prepared.upstream_is_stream,
            prepared.force_body_stream_field,
            prepared.kiro_auth.as_ref(),
            prepared.is_claude_code && !claude_code_native_passthrough,
            false,
            reasoning_replay_policy,
        )
    else {
        let provider_api_format = attempt.eligible.provider_api_format.as_str();
        let body_rules = prepared.transport.endpoint.body_rules.as_ref();
        let extra_data = match prepared.kiro_auth.as_ref() {
            Some(kiro_auth) => Some(
                kiro_envelope_failure_diagnostic(
                    body_json,
                    prepared.mapped_model.as_str(),
                    &kiro_auth.auth_config,
                    body_rules,
                    Some(effective_headers),
                    provider_api_format,
                    provider_api_format,
                    "kiro_envelope",
                )
                .to_extra_data(),
            ),
            None => same_format_provider_request_body_failure_extra_data(
                body_json,
                provider_api_format,
                body_rules,
                "same_format",
            ),
        };
        mark_skipped_local_same_format_provider_candidate_with_extra_data(
            state,
            input,
            trace_id,
            candidate,
            attempt.candidate_index,
            &attempt.candidate_id,
            "provider_request_body_missing",
            extra_data,
        )
        .await;
        return Ok(None);
    };
    let mut base_provider_request_body = base_provider_request.body;
    let mut compatibility_edits = base_provider_request.compatibility_edits;
    if let Some(mapping) = model_directive_mapping.as_ref() {
        let before_mapping = base_provider_request_body.clone();
        crate::ai_serving::apply_model_directive_mapping_patch(
            &mut base_provider_request_body,
            mapping,
        );
        if before_mapping != base_provider_request_body {
            compatibility_edits.push(SameFormatProviderCompatibilityEdit {
                field: "model_directive_mapping".to_string(),
                action: SameFormatProviderCompatibilityEditAction::RuntimeRewrite,
                detail: "applied configured model directive mapping patch".to_string(),
            });
        }
        // Directive mapping is a deep-merge patch and may overwrite/add `stream`;
        // re-enforce stream-field policy afterward.
        // Kiro behavior classification already hard-requires upstream streaming,
        // and the Kiro envelope does not use a top-level body stream field.
        if prepared.kiro_auth.is_none() {
            enforce_provider_body_stream_policy(
                &mut base_provider_request_body,
                prepared.provider_api_format.as_str(),
                prepared.upstream_is_stream,
                request_requires_body_stream_field(body_json, prepared.force_body_stream_field),
            );
        }
    }

    let source_model = body_json
        .get("model")
        .and_then(Value::as_str)
        .unwrap_or(input.requested_model.as_str());
    let codex_model_capabilities = codex_model_capabilities_for_transport(
        &transport,
        prepared.provider_api_format.as_str(),
        prepared.mapped_model.as_str(),
        source_model,
    );
    if let Err(violation) =
        crate::ai_serving::finalize_openai_provider_request_with_codex_model_capabilities_and_reasoning_replay_policy(
            &mut base_provider_request_body,
            crate::ai_serving::OpenAiProviderRequestFinalization {
                source_api_format: spec.api_format,
                provider_api_format: prepared.provider_api_format.as_str(),
                provider_type: transport.provider.provider_type.as_str(),
                provider_model: prepared.mapped_model.as_str(),
                source_model,
                body_rules: transport.endpoint.body_rules.as_ref(),
                upstream_is_stream: prepared.upstream_is_stream,
                require_body_stream_field: request_requires_body_stream_field(
                    body_json,
                    prepared.force_body_stream_field,
                ),
            },
            codex_model_capabilities.as_ref(),
            reasoning_replay_policy,
        )
    {
        mark_skipped_local_same_format_provider_candidate_with_extra_data(
            state,
            input,
            trace_id,
            candidate,
            attempt.candidate_index,
            &attempt.candidate_id,
            "provider_request_body_build_failed",
            Some(openai_provider_request_contract_failure_extra_data(
                &violation,
                spec.api_format,
                prepared.provider_api_format.as_str(),
                "same_format_provider_request_finalization",
            )),
        )
        .await;
        return Ok(None);
    }

    let antigravity_auth = if prepared.is_antigravity {
        let mut antigravity_support = classify_local_antigravity_request_support(
            &transport,
            &base_provider_request_body,
            AntigravityEnvelopeRequestType::Agent,
        );
        if matches!(
            antigravity_support,
            AntigravityRequestSideSupport::Unsupported(
                AntigravityRequestSideUnsupportedReason::UnsupportedAuth(
                    AntigravityRequestAuthUnsupportedReason::MissingProjectId
                )
            )
        ) {
            if let Some(hydrated) = state
                .hydrate_antigravity_project_metadata_for_transport(&transport)
                .await
            {
                transport = Arc::new(hydrated);
                antigravity_support = classify_local_antigravity_request_support(
                    &transport,
                    &base_provider_request_body,
                    AntigravityEnvelopeRequestType::Agent,
                );
            }
        }
        match antigravity_support {
            AntigravityRequestSideSupport::Supported(spec) => Some(spec.auth),
            AntigravityRequestSideSupport::Unsupported(_) => {
                mark_skipped_local_same_format_provider_candidate(
                    state,
                    input,
                    trace_id,
                    candidate,
                    attempt.candidate_index,
                    &attempt.candidate_id,
                    "transport_unsupported",
                )
                .await;
                return Ok(None);
            }
        }
    } else {
        None
    };
    let gemini_cli_auth = if prepared.behavior.is_gemini_cli {
        let mut auth = match resolve_local_gemini_cli_request_auth(&transport) {
            GeminiCliRequestAuthSupport::Supported(auth) => auth,
            GeminiCliRequestAuthSupport::Unsupported(_) => {
                mark_skipped_local_same_format_provider_candidate(
                    state,
                    input,
                    trace_id,
                    candidate,
                    attempt.candidate_index,
                    &attempt.candidate_id,
                    "transport_auth_unavailable",
                )
                .await;
                return Ok(None);
            }
        };
        if auth.project_id.is_none() {
            auth = match state
                .hydrate_gemini_cli_project_metadata_for_transport(&transport)
                .await
            {
                Some(hydrated) => {
                    transport = Arc::new(hydrated);
                    match resolve_local_gemini_cli_request_auth(&transport) {
                        GeminiCliRequestAuthSupport::Supported(auth) => auth,
                        GeminiCliRequestAuthSupport::Unsupported(_) => {
                            GeminiCliRequestAuth::default()
                        }
                    }
                }
                None => GeminiCliRequestAuth::default(),
            };
        }
        Some(auth)
    } else {
        None
    };
    let mut provider_request_body = if let Some(antigravity_auth) = antigravity_auth.as_ref() {
        match build_antigravity_safe_v1internal_request(
            antigravity_auth,
            trace_id,
            &prepared.mapped_model,
            &base_provider_request_body,
            AntigravityEnvelopeRequestType::Agent,
        ) {
            AntigravityRequestEnvelopeSupport::Supported(envelope) => envelope,
            AntigravityRequestEnvelopeSupport::Unsupported(_) => {
                mark_skipped_local_same_format_provider_candidate_with_extra_data(
                    state,
                    input,
                    trace_id,
                    candidate,
                    attempt.candidate_index,
                    &attempt.candidate_id,
                    "provider_request_body_missing",
                    same_format_provider_request_body_failure_extra_data(
                        body_json,
                        attempt.eligible.provider_api_format.as_str(),
                        prepared.transport.endpoint.body_rules.as_ref(),
                        "antigravity_envelope",
                    ),
                )
                .await;
                return Ok(None);
            }
        }
    } else if let Some(gemini_cli_auth) = gemini_cli_auth.as_ref() {
        match build_gemini_cli_v1internal_request(
            gemini_cli_auth,
            trace_id,
            &prepared.mapped_model,
            &base_provider_request_body,
        ) {
            GeminiCliRequestEnvelopeSupport::Supported(envelope) => envelope,
            GeminiCliRequestEnvelopeSupport::Unsupported(_) => {
                mark_skipped_local_same_format_provider_candidate_with_extra_data(
                    state,
                    input,
                    trace_id,
                    candidate,
                    attempt.candidate_index,
                    &attempt.candidate_id,
                    "provider_request_body_missing",
                    same_format_provider_request_body_failure_extra_data(
                        body_json,
                        attempt.eligible.provider_api_format.as_str(),
                        prepared.transport.endpoint.body_rules.as_ref(),
                        "gemini_cli_v1internal_envelope",
                    ),
                )
                .await;
                return Ok(None);
            }
        }
    } else {
        base_provider_request_body
    };
    // P6：Antigravity 信封构建成功后做敏感词混淆（只碰 systemInstruction 文本）。
    let antigravity_sensitive_words = if antigravity_auth.is_some() {
        crate::ai_serving::planner::antigravity_sensitive_words::apply_antigravity_sensitive_words(
            &transport,
            &mut provider_request_body,
        )
    } else {
        None
    };
    if crate::ai_serving::transport::enforce_same_format_provider_api_operation_body_policy(
        &mut provider_request_body,
        spec.operation,
    ) {
        compatibility_edits.push(SameFormatProviderCompatibilityEdit {
            field: "stream".to_string(),
            action: SameFormatProviderCompatibilityEditAction::RuntimeRewrite,
            detail: "removed stream field for non-streaming API operation".to_string(),
        });
    }

    // P2：Claude Code 客户端策略。body 到这里已经是最终形状（模型映射、body_rules、操作
    // 不变量都已应用），流水线在此对第三方请求做混淆 → 身份 → cache_control → CCH 签名，
    // 之后 body 不再改动；原生 Claude Code 请求原样通过。
    let claude_code_cloak = claude_code_policy.as_ref().map(|policy| {
        crate::ai_serving::planner::claude_code_cloak::apply_claude_code_client_policy(
            policy,
            &transport,
            effective_headers,
            &mut provider_request_body,
            spec.operation,
            input
                .client_session_affinity
                .as_ref()
                .and_then(|affinity| affinity.session_key.as_deref()),
        )
    });
    if let Some(profile) = claude_code_cloak
        .as_ref()
        .and_then(|outcome| outcome.device_profile_update.clone())
    {
        crate::ai_serving::planner::claude_code_cloak::spawn_persist_claude_code_device_profile(
            state,
            &transport.key.id,
            profile,
        );
    }
    if claude_code_cloak
        .as_ref()
        .is_some_and(|outcome| !outcome.native_passthrough)
    {
        compatibility_edits.push(SameFormatProviderCompatibilityEdit {
            field: "claude_code_cloak".to_string(),
            action: SameFormatProviderCompatibilityEditAction::ProviderCompatibilityRewrite,
            detail: "applied Claude Code client cloak pipeline (identity, cache_control, cch)"
                .to_string(),
        });
    }
    let claude_code_wire = claude_code_cloak
        .as_ref()
        .and_then(|outcome| outcome.wire_policy());

    let is_grok = prepared
        .transport
        .provider
        .provider_type
        .trim()
        .eq_ignore_ascii_case("grok");
    let transport_profile = crate::ai_serving::transport::resolve_transport_profile(&transport);
    let upstream_url = if is_grok {
        Some(build_grok_upstream_url(&transport, GROK_CHAT_PATH))
    } else {
        super::super::request::build_same_format_upstream_url(
            parts,
            &transport,
            &prepared.mapped_model,
            prepared.provider_api_format.as_str(),
            spec,
            prepared.upstream_is_stream,
            prepared.kiro_auth.as_ref(),
            Some(&provider_request_body),
        )
    };
    let Some(upstream_url) = upstream_url else {
        mark_skipped_local_same_format_provider_candidate_with_failure_diagnostic(
            state,
            input,
            trace_id,
            candidate,
            attempt.candidate_index,
            &attempt.candidate_id,
            "upstream_url_missing",
            CandidateFailureDiagnostic::upstream_url_missing(
                attempt.eligible.provider_api_format.as_str(),
                attempt.eligible.provider_api_format.as_str(),
                "same_format_provider_url",
            ),
        )
        .await;
        return Ok(None);
    };

    let mut extra_headers = antigravity_auth
        .as_ref()
        .map(build_antigravity_static_identity_headers)
        .unwrap_or_default();
    if prepared.behavior.is_gemini_cli {
        extra_headers.insert("user-agent".to_string(), GEMINI_CLI_USER_AGENT.to_string());
    }
    let Some(mut provider_request_headers) = (if is_grok {
        build_grok_browser_headers(GrokHeaderInput {
            transport: &transport,
            transport_profile: transport_profile.as_ref(),
            request_headers: Some(effective_headers),
            content_type: "application/json",
            accept: "text/event-stream",
            header_rules: transport.endpoint.header_rules.as_ref(),
            provider_request_body: &provider_request_body,
            original_request_body: body_json,
        })
    } else {
        build_same_format_provider_headers(SameFormatProviderHeadersInput {
            headers: effective_headers,
            provider_request_body: &provider_request_body,
            original_request_body: body_json,
            header_rules: transport.endpoint.header_rules.as_ref(),
            behavior: prepared.behavior,
            api_operation: spec.operation,
            auth_header: prepared.auth_header.as_deref(),
            auth_value: prepared.auth_value.as_deref(),
            extra_headers: &extra_headers,
            kiro_auth_config: prepared.kiro_auth.as_ref().map(|auth| &auth.auth_config),
            kiro_machine_id: prepared
                .kiro_auth
                .as_ref()
                .map(|auth| auth.machine_id.as_str()),
            claude_code_wire,
        })
    }) else {
        mark_skipped_local_same_format_provider_candidate_with_failure_diagnostic(
            state,
            input,
            trace_id,
            candidate,
            attempt.candidate_index,
            &attempt.candidate_id,
            "transport_header_rules_apply_failed",
            CandidateFailureDiagnostic::header_rules_apply_failed(
                attempt.eligible.provider_api_format.as_str(),
                attempt.eligible.provider_api_format.as_str(),
                "same_format_provider_headers",
            ),
        )
        .await;
        return Ok(None);
    };
    crate::ai_serving::apply_codex_openai_special_headers(
        &mut provider_request_headers,
        &provider_request_body,
        effective_headers,
        transport.provider.provider_type.as_str(),
        prepared.provider_api_format.as_str(),
        Some(trace_id),
        transport.key.decrypted_auth_config.as_deref(),
    );
    let provider_model = provider_request_body
        .get("model")
        .and_then(Value::as_str)
        .unwrap_or(prepared.mapped_model.as_str());
    crate::ai_serving::apply_codex_openai_responses_lite_header_for_request_body_with_capabilities(
        &mut provider_request_headers,
        Some(&provider_request_body),
        transport.provider.provider_type.as_str(),
        prepared.provider_api_format.as_str(),
        provider_model,
        source_model,
        codex_model_capabilities.as_ref(),
    );
    request_identity_response_encoding_when_redacted(
        &mut provider_request_headers,
        redaction.redacted,
    );

    Ok(Some(LocalSameFormatProviderCandidatePayloadParts {
        transport,
        is_antigravity: prepared.is_antigravity,
        is_gemini_cli: prepared.behavior.is_gemini_cli,
        is_kiro: prepared.is_kiro,
        auth_header: prepared.auth_header,
        auth_value: prepared.auth_value,
        provider_api_format: prepared.provider_api_format,
        mapped_model: prepared.mapped_model,
        report_kind: prepared.report_kind,
        upstream_is_stream: prepared.upstream_is_stream,
        upstream_url,
        provider_request_headers,
        provider_request_body,
        transport_profile,
        compatibility_edits,
        request_redacted: redaction.redacted,
        claude_code_cloak_report: claude_code_cloak.map(|outcome| outcome.report),
        sensitive_words_obfuscation: antigravity_sensitive_words,
    }))
}

fn same_format_provider_operation_skip_reason(
    transport: &GatewayProviderTransportSnapshot,
    provider_api_format: &str,
    operation: Option<crate::ai_serving::ApiOperation>,
) -> Option<(&'static str, TransportOperationUnsupportedReason)> {
    transport_api_operation_unsupported_reason(transport, provider_api_format, operation)
        .map(|reason| ("transport_operation_unsupported", reason))
}

/// Explains an operation-level skip in terms of the configuration an operator can
/// change, instead of the bare `transport_operation_unsupported` code.
fn same_format_provider_operation_failure_diagnostic(
    transport: &GatewayProviderTransportSnapshot,
    provider_api_format: &str,
    operation: Option<crate::ai_serving::ApiOperation>,
    reason: TransportOperationUnsupportedReason,
) -> CandidateFailureDiagnostic {
    let operation_label = operation.map_or("该操作", |operation| operation.as_str());
    let provider_type = transport.provider.provider_type.trim();
    let (path, message) = match reason {
        TransportOperationUnsupportedReason::ProviderAdapter => (
            "$.provider.provider_type",
            format!(
                "{provider_type} 类型的提供商没有 {operation_label} 操作的上游接口，无法由该渠道处理；请为此操作配置其他渠道"
            ),
        ),
        TransportOperationUnsupportedReason::ApiFormat => (
            "$.endpoint.api_format",
            format!("端点 API 格式 {provider_api_format} 不支持 {operation_label} 操作"),
        ),
        TransportOperationUnsupportedReason::EndpointConfig => (
            "$.endpoint.config.anthropic.supported_operations",
            format!(
                "端点配置的 supported_operations 未包含 {operation_label}；如该上游支持此操作，请在端点设置中启用"
            ),
        ),
    };
    CandidateFailureDiagnostic::new(
        CandidateFailureDiagnosticKind::TransportOperation,
        path,
        message,
    )
    .formats(provider_api_format, provider_api_format)
    .source("same_format_provider_operation")
}

#[cfg(test)]
mod tests {
    use super::{
        same_format_provider_operation_failure_diagnostic,
        same_format_provider_operation_skip_reason, TransportOperationUnsupportedReason,
    };
    use crate::ai_serving::transport::snapshot::{
        GatewayProviderTransportEndpoint, GatewayProviderTransportKey,
        GatewayProviderTransportProvider,
    };
    use crate::ai_serving::{ApiOperation, GatewayProviderTransportSnapshot};

    fn private_adapter_transport(provider_type: &str) -> GatewayProviderTransportSnapshot {
        GatewayProviderTransportSnapshot {
            provider: GatewayProviderTransportProvider {
                id: "provider-1".to_string(),
                name: provider_type.to_string(),
                provider_type: provider_type.to_string(),
                website: None,
                is_active: true,
                keep_priority_on_conversion: false,
                enable_format_conversion: true,
                concurrent_limit: None,
                max_retries: None,
                proxy: None,
                request_timeout_secs: None,
                stream_first_byte_timeout_secs: None,
                config: None,
            },
            endpoint: GatewayProviderTransportEndpoint {
                id: "endpoint-1".to_string(),
                provider_id: "provider-1".to_string(),
                api_format: "claude:messages".to_string(),
                api_family: Some("claude".to_string()),
                endpoint_kind: Some("chat".to_string()),
                is_active: true,
                base_url: "https://private.example".to_string(),
                header_rules: None,
                body_rules: None,
                max_retries: None,
                custom_path: None,
                config: None,
                format_acceptance_config: None,
                proxy: None,
            },
            key: GatewayProviderTransportKey {
                id: "key-1".to_string(),
                provider_id: "provider-1".to_string(),
                name: "key".to_string(),
                auth_type: "oauth".to_string(),
                is_active: true,
                api_formats: None,
                auth_type_by_format: None,
                allow_auth_channel_mismatch_formats: None,
                allowed_models: None,
                capabilities: None,
                rate_multipliers: None,
                global_priority_by_format: None,
                expires_at_unix_secs: None,
                proxy: None,
                fingerprint: None,
                upstream_metadata: None,
                decrypted_api_key: String::new(),
                decrypted_auth_config: None,
            },
        }
    }

    #[test]
    fn private_adapter_count_tokens_is_rejected_by_pre_auth_operation_gate() {
        for provider_type in ["kiro", "grok"] {
            let transport = private_adapter_transport(provider_type);
            assert_eq!(
                same_format_provider_operation_skip_reason(
                    &transport,
                    "claude:messages",
                    Some(ApiOperation::ClaudeCountTokens),
                ),
                Some((
                    "transport_operation_unsupported",
                    TransportOperationUnsupportedReason::ProviderAdapter
                )),
                "provider_type={provider_type}"
            );
        }
    }

    #[test]
    fn operation_skip_diagnostic_names_the_endpoint_setting_that_disables_count_tokens() {
        let mut transport = private_adapter_transport("custom");
        transport.endpoint.config = Some(serde_json::json!({
            "anthropic": {"supported_operations": ["messages"]}
        }));

        let (skip_reason, reason) = same_format_provider_operation_skip_reason(
            &transport,
            "claude:messages",
            Some(ApiOperation::ClaudeCountTokens),
        )
        .expect("count_tokens should be skipped");
        assert_eq!(skip_reason, "transport_operation_unsupported");
        assert_eq!(reason, TransportOperationUnsupportedReason::EndpointConfig);

        let extra_data = same_format_provider_operation_failure_diagnostic(
            &transport,
            "claude:messages",
            Some(ApiOperation::ClaudeCountTokens),
            reason,
        )
        .to_extra_data();
        assert_eq!(
            extra_data["failure_diagnostic"]["kind"],
            "transport_operation"
        );
        assert_eq!(
            extra_data["failure_diagnostic"]["path"],
            "$.endpoint.config.anthropic.supported_operations"
        );
        assert!(extra_data["failure_diagnostic"]["message"]
            .as_str()
            .expect("message")
            .contains("count_tokens"));
        assert_eq!(extra_data["failure_diagnostic"]["safe_to_show"], true);

        let adapter_extra_data = same_format_provider_operation_failure_diagnostic(
            &private_adapter_transport("kiro"),
            "claude:messages",
            Some(ApiOperation::ClaudeCountTokens),
            TransportOperationUnsupportedReason::ProviderAdapter,
        )
        .to_extra_data();
        assert_eq!(
            adapter_extra_data["failure_diagnostic"]["path"],
            "$.provider.provider_type"
        );
        assert!(adapter_extra_data["failure_diagnostic"]["message"]
            .as_str()
            .expect("message")
            .contains("kiro"));
    }
}
