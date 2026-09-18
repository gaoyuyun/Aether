use aether_data_contracts::repository::candidate_selection::StoredMinimalCandidateSelectionRow;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub enum SchedulerPriorityMode {
    #[default]
    Provider,
    GlobalKey,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct SchedulerMinimalCandidateSelectionCandidate {
    pub provider_id: String,
    pub provider_name: String,
    pub provider_type: String,
    pub provider_priority: i32,
    pub endpoint_id: String,
    pub endpoint_api_format: String,
    pub key_id: String,
    pub key_name: String,
    pub key_auth_type: String,
    pub key_internal_priority: i32,
    pub key_global_priority_for_format: Option<i32>,
    pub key_capabilities: Option<serde_json::Value>,
    pub model_id: String,
    pub global_model_id: String,
    pub global_model_name: String,
    pub selected_provider_model_name: String,
    pub supports_streaming: bool,
    pub mapping_matched_model: Option<String>,
}

/// 枚举阶段因 Key 模型白名单不含请求模型而被拒绝的候选行，对应
/// `request_candidates.skip_reason` 白名单里的 `key_model_not_allowed`。
pub const KEY_MODEL_NOT_ALLOWED_SKIP_REASON: &str = "key_model_not_allowed";

#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct RejectedMinimalCandidateSelectionRow {
    pub candidate: SchedulerMinimalCandidateSelectionCandidate,
    pub skip_reason: &'static str,
}

#[derive(Debug, Clone, Default, PartialEq, serde::Serialize)]
pub struct EnumeratedMinimalCandidateSelection {
    pub candidates: Vec<SchedulerMinimalCandidateSelectionCandidate>,
    pub rejected: Vec<RejectedMinimalCandidateSelectionRow>,
}

pub struct EnumerateMinimalCandidateSelectionInput<'a> {
    pub rows: Vec<StoredMinimalCandidateSelectionRow>,
    pub normalized_api_format: &'a str,
    pub request_operation: Option<&'a str>,
    pub requested_model_name: &'a str,
    pub resolved_global_model_name: &'a str,
    pub require_streaming: bool,
    pub required_capabilities: Option<&'a serde_json::Value>,
    pub auth_constraints: Option<&'a crate::SchedulerAuthConstraints>,
}
