use std::collections::BTreeSet;

use aether_data_contracts::repository::candidate_selection::StoredMinimalCandidateSelectionRow;
use aether_data_contracts::DataLayerError;

use super::types::{
    EnumerateMinimalCandidateSelectionInput, EnumeratedMinimalCandidateSelection,
    RejectedMinimalCandidateSelectionRow, SchedulerMinimalCandidateSelectionCandidate,
    KEY_MODEL_NOT_ALLOWED_SKIP_REASON,
};

pub fn enumerate_minimal_candidate_selection(
    input: EnumerateMinimalCandidateSelectionInput<'_>,
) -> Result<Vec<SchedulerMinimalCandidateSelectionCandidate>, DataLayerError> {
    enumerate_minimal_candidate_selection_inner(input, false).map(|outcome| outcome.candidates)
}

pub fn enumerate_minimal_candidate_selection_with_model_directives(
    input: EnumerateMinimalCandidateSelectionInput<'_>,
    enable_model_directives: bool,
) -> Result<Vec<SchedulerMinimalCandidateSelectionCandidate>, DataLayerError> {
    enumerate_minimal_candidate_selection_inner(input, enable_model_directives)
        .map(|outcome| outcome.candidates)
}

/// 与 [`enumerate_minimal_candidate_selection_with_model_directives`] 相同，但额外返回
/// 因 Key 模型白名单不含请求模型而被拒绝的行，供调用方记录为跳过候选。
pub fn enumerate_minimal_candidate_selection_with_model_directives_and_rejections(
    input: EnumerateMinimalCandidateSelectionInput<'_>,
    enable_model_directives: bool,
) -> Result<EnumeratedMinimalCandidateSelection, DataLayerError> {
    enumerate_minimal_candidate_selection_inner(input, enable_model_directives)
}

fn enumerate_minimal_candidate_selection_inner(
    input: EnumerateMinimalCandidateSelectionInput<'_>,
    enable_model_directives: bool,
) -> Result<EnumeratedMinimalCandidateSelection, DataLayerError> {
    let EnumerateMinimalCandidateSelectionInput {
        rows,
        normalized_api_format,
        request_operation,
        requested_model_name,
        resolved_global_model_name,
        require_streaming,
        auth_constraints,
        ..
    } = input;

    if normalized_api_format.is_empty() {
        return Ok(EnumeratedMinimalCandidateSelection::default());
    }
    if !crate::auth_constraints_allow_api_format(auth_constraints, normalized_api_format) {
        return Ok(EnumeratedMinimalCandidateSelection::default());
    }
    if !crate::auth_constraints_allow_model_with_model_directives(
        auth_constraints,
        requested_model_name,
        resolved_global_model_name,
        enable_model_directives,
    ) {
        return Ok(EnumeratedMinimalCandidateSelection::default());
    }

    let mut candidates = Vec::with_capacity(rows.len());
    let mut rejected = Vec::new();
    for row in rows {
        if !crate::auth_constraints_allow_provider(
            auth_constraints,
            &row.provider_id,
            &row.provider_name,
            &row.provider_type,
        ) {
            continue;
        }
        if require_streaming && !row.supports_streaming() {
            continue;
        }
        let Some((selected_provider_model_name, mapping_matched_model)) =
            crate::resolve_provider_model_name_with_model_directives_and_request_operation(
                &row,
                requested_model_name,
                normalized_api_format,
                enable_model_directives,
                request_operation,
            )
        else {
            // 供应商模型本身可用但 Key 白名单不含请求模型：记为被拒绝的行而不是静默丢弃，
            // 让上层能把它作为跳过候选落库并计入未命中诊断。
            let Some(selected_provider_model_name) =
                crate::model::resolve_selected_provider_model_name_for_row(
                    &row,
                    normalized_api_format,
                    request_operation,
                )
            else {
                continue;
            };
            let candidate = candidate_from_row(
                row,
                selected_provider_model_name,
                None,
                normalized_api_format,
            )?;
            rejected.push(RejectedMinimalCandidateSelectionRow {
                candidate,
                skip_reason: KEY_MODEL_NOT_ALLOWED_SKIP_REASON,
            });
            continue;
        };

        candidates.push(candidate_from_row(
            row,
            selected_provider_model_name,
            mapping_matched_model,
            normalized_api_format,
        )?);
    }

    Ok(EnumeratedMinimalCandidateSelection {
        candidates,
        rejected,
    })
}

fn candidate_from_row(
    row: StoredMinimalCandidateSelectionRow,
    selected_provider_model_name: String,
    mapping_matched_model: Option<String>,
    normalized_api_format: &str,
) -> Result<SchedulerMinimalCandidateSelectionCandidate, DataLayerError> {
    let supports_streaming = row.supports_streaming();
    Ok(SchedulerMinimalCandidateSelectionCandidate {
        provider_id: row.provider_id,
        provider_name: row.provider_name,
        provider_type: row.provider_type,
        provider_priority: row.provider_priority,
        endpoint_id: row.endpoint_id,
        endpoint_api_format: row.endpoint_api_format,
        key_id: row.key_id,
        key_name: row.key_name,
        key_auth_type: row.key_auth_type,
        key_internal_priority: row.key_internal_priority,
        key_global_priority_for_format: crate::extract_global_priority_for_format(
            row.key_global_priority_by_format.as_ref(),
            normalized_api_format,
        )?,
        key_capabilities: row.key_capabilities,
        model_id: row.model_id,
        global_model_id: row.global_model_id,
        global_model_name: row.global_model_name,
        selected_provider_model_name,
        supports_streaming,
        mapping_matched_model,
    })
}

pub fn collect_global_model_names_for_required_capability(
    rows: Vec<StoredMinimalCandidateSelectionRow>,
    normalized_api_format: &str,
    required_capability: &str,
    require_streaming: bool,
    auth_constraints: Option<&crate::SchedulerAuthConstraints>,
) -> Vec<String> {
    if normalized_api_format.is_empty() || required_capability.trim().is_empty() {
        return Vec::new();
    }
    if !crate::auth_constraints_allow_api_format(auth_constraints, normalized_api_format) {
        return Vec::new();
    }

    let mut model_names = BTreeSet::new();
    for row in rows {
        if !crate::auth_constraints_allow_provider(
            auth_constraints,
            &row.provider_id,
            &row.provider_name,
            &row.provider_type,
        ) {
            continue;
        }
        if !crate::row_supports_required_capability(&row, required_capability) {
            continue;
        }
        if require_streaming && !row.supports_streaming() {
            continue;
        }
        if !crate::auth_constraints_allow_model(
            auth_constraints,
            &row.global_model_name,
            &row.global_model_name,
        ) {
            continue;
        }
        model_names.insert(row.global_model_name);
    }

    model_names.into_iter().collect()
}
