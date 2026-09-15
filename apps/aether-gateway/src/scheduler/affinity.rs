use std::time::Duration;

use aether_routing_core::{ResolvedRoutingPolicy, RoutingSchedulingMode};
use aether_scheduler_core::{
    build_scheduler_affinity_cache_key_for_api_key_id_with_client_session,
    build_scheduler_affinity_cache_key_for_api_key_id_with_client_session_and_scope,
    ClientSessionAffinity, SchedulerAffinityScope, SchedulerAffinityTarget,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::state::SchedulerRuntimeState;

pub(crate) const SCHEDULER_AFFINITY_TTL: Duration = Duration::from_secs(300);
pub(crate) const SCHEDULER_AFFINITY_POLICY_REPORT_FIELD: &str = "scheduler_affinity_policy";

/// Claude `count_tokens` is a stateless helper call. It may follow the
/// session's remembered upstream, but it must never *become* the remembered
/// upstream: a token-count request that falls over to another provider would
/// otherwise drag the session's subsequent `/v1/messages` traffic with it.
pub(crate) fn request_operation_writes_session_affinity(request_operation: Option<&str>) -> bool {
    !request_operation.is_some_and(|operation| {
        operation
            .trim()
            .eq_ignore_ascii_case(crate::ai_serving::ApiOperation::ClaudeCountTokens.as_str())
    })
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct SchedulerAffinityPolicyContext {
    pub(crate) scheduling_mode: RoutingSchedulingMode,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) scope: Option<SchedulerAffinityScope>,
}

impl SchedulerAffinityPolicyContext {
    pub(crate) fn from_routing_policy(policy: &ResolvedRoutingPolicy) -> Self {
        let scope = policy
            .group_id
            .as_deref()
            .map(str::trim)
            .filter(|group_id| !group_id.is_empty())
            .map(|group_id| SchedulerAffinityScope::new(group_id, policy.group_version));
        Self {
            scheduling_mode: policy.scheduling_mode,
            scope,
        }
    }

    pub(crate) fn cache_affinity_enabled(&self) -> bool {
        self.scheduling_mode == RoutingSchedulingMode::CacheAffinity
    }
}

pub(crate) fn scheduler_affinity_policy_context_from_report_context(
    report_context: Option<&Value>,
) -> Option<SchedulerAffinityPolicyContext> {
    report_context
        .and_then(|context| context.get(SCHEDULER_AFFINITY_POLICY_REPORT_FIELD))
        .and_then(|value| serde_json::from_value(value.clone()).ok())
}

pub(crate) fn insert_scheduler_affinity_policy_report_context_field(
    extra_fields: &mut serde_json::Map<String, Value>,
    routing_policy: Option<&ResolvedRoutingPolicy>,
) {
    let Some(routing_policy) = routing_policy else {
        return;
    };
    let context = SchedulerAffinityPolicyContext::from_routing_policy(routing_policy);
    if let Ok(value) = serde_json::to_value(context) {
        extra_fields.insert(SCHEDULER_AFFINITY_POLICY_REPORT_FIELD.to_string(), value);
    }
}

pub(crate) fn read_cached_scheduler_affinity_target(
    state: &(impl SchedulerRuntimeState + ?Sized),
    api_key_id: &str,
    client_session_affinity: Option<&ClientSessionAffinity>,
    api_format: &str,
    global_model_name: &str,
) -> Option<SchedulerAffinityTarget> {
    let cache_key = build_scheduler_affinity_cache_key_for_api_key_id_with_client_session(
        api_key_id,
        api_format,
        global_model_name,
        client_session_affinity,
    )?;
    state.read_cached_scheduler_affinity_target(&cache_key, SCHEDULER_AFFINITY_TTL)
}

pub(crate) fn read_cached_scheduler_affinity_target_with_policy_context(
    state: &(impl SchedulerRuntimeState + ?Sized),
    api_key_id: &str,
    client_session_affinity: Option<&ClientSessionAffinity>,
    api_format: &str,
    global_model_name: &str,
    policy_context: &SchedulerAffinityPolicyContext,
) -> Option<SchedulerAffinityTarget> {
    if !policy_context.cache_affinity_enabled() {
        return None;
    }
    let cache_key =
        build_scheduler_affinity_cache_key_for_api_key_id_with_client_session_and_scope(
            api_key_id,
            api_format,
            global_model_name,
            client_session_affinity,
            policy_context.scope.as_ref(),
        )?;
    state.read_cached_scheduler_affinity_target(&cache_key, SCHEDULER_AFFINITY_TTL)
}

#[cfg(test)]
mod tests {
    use super::request_operation_writes_session_affinity;

    #[test]
    fn count_tokens_operation_never_writes_session_affinity() {
        assert!(request_operation_writes_session_affinity(None));
        assert!(request_operation_writes_session_affinity(Some("messages")));
        assert!(request_operation_writes_session_affinity(Some("compact")));
        assert!(!request_operation_writes_session_affinity(Some(
            "count_tokens"
        )));
        assert!(!request_operation_writes_session_affinity(Some(
            " COUNT_TOKENS "
        )));
    }
}
