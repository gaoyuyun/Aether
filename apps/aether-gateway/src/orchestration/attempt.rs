use super::policy::LOCAL_FAILOVER_POLICY_REPORT_FIELD;
use aether_ai_serving::{AiExecutionAttempt, SAME_KEY_RETRIES_REPORT_FIELD};
use aether_routing_core::DEFAULT_SAME_KEY_RETRIES;
use aether_runtime_state::RuntimeLockLease;
use aether_scheduler_core::parse_request_candidate_report_context;
use serde_json::Value;
use uuid::Uuid;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ExecutionAttemptIdentity {
    pub(crate) candidate_index: u32,
    pub(crate) scheduling_candidate_index: u32,
    pub(crate) retry_index: u32,
    pub(crate) pool_key_index: Option<u32>,
}

impl ExecutionAttemptIdentity {
    pub(crate) const fn new(candidate_index: u32, retry_index: u32) -> Self {
        Self {
            candidate_index,
            scheduling_candidate_index: candidate_index,
            retry_index,
            pool_key_index: None,
        }
    }

    pub(crate) const fn with_pool_key_index(mut self, pool_key_index: Option<u32>) -> Self {
        self.pool_key_index = pool_key_index;
        self
    }

    pub(crate) const fn with_scheduling_candidate_index(mut self, index: u32) -> Self {
        self.scheduling_candidate_index = index;
        self
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct LocalExecutionCandidateMetadata {
    pub(crate) candidate_group_id: Option<String>,
    pub(crate) pool_key_index: Option<u32>,
    pub(crate) pool_key_lease: Option<RuntimeLockLease>,
    pub(crate) scheduler_affinity_epoch: Option<u64>,
    /// Routing-policy default `same_key_retries` in effect for this request.
    /// `None` means the built-in default (no retry) applies.
    pub(crate) same_key_retries: Option<u32>,
}

pub(crate) const SCHEDULER_AFFINITY_EPOCH_REPORT_FIELD: &str = "scheduler_affinity_epoch";
pub(crate) const ROUTING_POOL_POLICY_OVERRIDE_REPORT_FIELD: &str = "routing_pool_policy_override";
pub(crate) const POOL_KEY_LEASE_KEY_REPORT_FIELD: &str = "pool_key_lease_key";
pub(crate) const POOL_KEY_LEASE_OWNER_REPORT_FIELD: &str = "pool_key_lease_owner";
pub(crate) const POOL_KEY_LEASE_TOKEN_REPORT_FIELD: &str = "pool_key_lease_token";
pub(crate) const POOL_KEY_LEASE_FENCING_REPORT_FIELD: &str = "pool_key_lease_fencing_token";
pub(crate) const POOL_KEY_LEASE_TTL_MS_REPORT_FIELD: &str = "pool_key_lease_ttl_ms";

/// Pool-expanded keys encode `pool_key_index * STRIDE + retry_index` into the
/// persisted `retry_index` so a pool group's keys stay ordered in one candidate
/// slot. Same-key retries on a pool key are therefore bounded by the stride.
pub(crate) const POOL_KEY_RETRY_INDEX_STRIDE: u32 = 100;

pub(crate) fn attempt_identity_from_report_context(
    report_context: Option<&Value>,
) -> Option<ExecutionAttemptIdentity> {
    let metadata = parse_request_candidate_report_context(report_context)?;
    let candidate_metadata = local_execution_candidate_metadata_from_report_context(report_context);

    let candidate_index = metadata.candidate_index?;
    Some(ExecutionAttemptIdentity {
        candidate_index,
        scheduling_candidate_index: report_context
            .and_then(|context| context.get("scheduling_candidate_index"))
            .and_then(Value::as_u64)
            .and_then(|value| u32::try_from(value).ok())
            .unwrap_or(candidate_index),
        retry_index: metadata.retry_index,
        pool_key_index: candidate_metadata.pool_key_index,
    })
}

pub(crate) fn local_execution_candidate_metadata_from_report_context(
    report_context: Option<&Value>,
) -> LocalExecutionCandidateMetadata {
    LocalExecutionCandidateMetadata {
        candidate_group_id: report_context
            .and_then(Value::as_object)
            .and_then(|value| value.get("candidate_group_id"))
            .and_then(Value::as_str)
            .map(ToOwned::to_owned),
        pool_key_index: report_context
            .and_then(|value| value.get("pool_key_index"))
            .and_then(Value::as_u64)
            .and_then(|value| u32::try_from(value).ok()),
        pool_key_lease: pool_key_lease_from_report_context(report_context),
        scheduler_affinity_epoch: report_context
            .and_then(|value| value.get(SCHEDULER_AFFINITY_EPOCH_REPORT_FIELD))
            .and_then(Value::as_u64),
        same_key_retries: report_context
            .and_then(|value| value.get(SAME_KEY_RETRIES_REPORT_FIELD))
            .and_then(Value::as_u64)
            .and_then(|value| u32::try_from(value).ok()),
    }
}

pub(crate) fn insert_pool_key_lease_report_context_fields(
    extra_fields: &mut serde_json::Map<String, Value>,
    lease: Option<&RuntimeLockLease>,
) {
    let Some(lease) = lease else {
        return;
    };
    extra_fields.insert(
        POOL_KEY_LEASE_KEY_REPORT_FIELD.to_string(),
        Value::String(lease.key.clone()),
    );
    extra_fields.insert(
        POOL_KEY_LEASE_OWNER_REPORT_FIELD.to_string(),
        Value::String(lease.owner.clone()),
    );
    extra_fields.insert(
        POOL_KEY_LEASE_TOKEN_REPORT_FIELD.to_string(),
        Value::String(lease.token.clone()),
    );
    extra_fields.insert(
        POOL_KEY_LEASE_FENCING_REPORT_FIELD.to_string(),
        Value::Number(lease.fencing_token.into()),
    );
    extra_fields.insert(
        POOL_KEY_LEASE_TTL_MS_REPORT_FIELD.to_string(),
        Value::Number(lease.ttl_ms.into()),
    );
}

fn pool_key_lease_from_report_context(report_context: Option<&Value>) -> Option<RuntimeLockLease> {
    let report_context = report_context?;
    let key = report_context
        .get(POOL_KEY_LEASE_KEY_REPORT_FIELD)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())?;
    let owner = report_context
        .get(POOL_KEY_LEASE_OWNER_REPORT_FIELD)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())?;
    let token = report_context
        .get(POOL_KEY_LEASE_TOKEN_REPORT_FIELD)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())?;
    let ttl_ms = report_context
        .get(POOL_KEY_LEASE_TTL_MS_REPORT_FIELD)
        .and_then(Value::as_u64)
        .filter(|value| *value > 0)?;
    let fencing_token = report_context
        .get(POOL_KEY_LEASE_FENCING_REPORT_FIELD)
        .and_then(Value::as_u64)
        .filter(|value| *value > 0)
        .unwrap_or(1);

    Some(RuntimeLockLease {
        key: key.to_string(),
        owner: owner.to_string(),
        token: token.to_string(),
        fencing_token,
        ttl_ms,
    })
}

/// Same-key retry budget for one attempt, read from its report context.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct SameKeyRetryBudget {
    /// Routing-policy default `same_key_retries`: retries after the first
    /// attempt on every key without a provider override. `None` means the
    /// built-in default (no retry) applies.
    pub(crate) policy_same_key_retries: Option<u32>,
    /// Provider-level override carried as `local_failover_policy.max_retries`:
    /// retries after the first attempt on every key of that provider. `None`
    /// inherits the routing-policy default.
    pub(crate) provider_same_key_retries: Option<u32>,
}

impl SameKeyRetryBudget {
    /// Retries allowed after the first attempt on the key: the provider
    /// override wins, then the routing-policy default, then no retry.
    pub(crate) fn allowed_retries(self) -> u32 {
        self.provider_same_key_retries
            .or(self.policy_same_key_retries)
            .unwrap_or(DEFAULT_SAME_KEY_RETRIES)
    }
}

/// Provider-level same-key retry override carried in the report context as
/// `local_failover_policy.max_retries`. The provider's own `max_retries`
/// column, its endpoint column and `failover_rules.max_retries` are folded
/// into that field when the attempt is planned; `None` means the provider
/// inherits the routing policy.
pub(crate) fn provider_same_key_retries_from_report_context(
    report_context: Option<&Value>,
) -> Option<u32> {
    let value = report_context?
        .get(LOCAL_FAILOVER_POLICY_REPORT_FIELD)?
        .get("max_retries")?;
    let retries = value
        .as_u64()
        .or_else(|| value.as_i64().and_then(|value| u64::try_from(value).ok()))?;
    Some(u32::try_from(retries).unwrap_or(u32::MAX))
}

/// Retry index of the next same-key attempt, or `None` when the same-key
/// budget for this key is used up.
///
/// Every candidate key is treated alike, whatever its rank: after a
/// candidate-scoped failure it is retried on the same key up to the budget's
/// retry count (`0` fails over immediately, `1` allows one retry, i.e. two
/// attempts). The provider's own setting overrides the routing policy default.
///
/// There is no upper bound: attempts are derived one at a time after each
/// failure, never materialized ahead of time. Inside a pool group the encoded
/// retry index is `pool_key_index * POOL_KEY_RETRY_INDEX_STRIDE + retry`, so
/// each pool key counts its own retries and stays below the stride to avoid
/// colliding with the next pool key.
pub(crate) fn next_same_key_retry_index(
    identity: ExecutionAttemptIdentity,
    budget: SameKeyRetryBudget,
) -> Option<u32> {
    let (retry_base, pool_limit) = match identity.pool_key_index {
        None => (0, u32::MAX),
        Some(pool_key_index) => (
            pool_key_index.checked_mul(POOL_KEY_RETRY_INDEX_STRIDE)?,
            POOL_KEY_RETRY_INDEX_STRIDE,
        ),
    };
    let retries_so_far = identity.retry_index.checked_sub(retry_base)?;
    let next_retry = retries_so_far.checked_add(1)?;
    if next_retry > budget.allowed_retries() || next_retry >= pool_limit {
        return None;
    }
    identity.retry_index.checked_add(1)
}

/// Derive the next same-key attempt for `attempt` after a candidate-scoped
/// failure, reading the attempt identity and retry budget from its report
/// context. Returns `None` when no further same-key retry is allowed.
pub(crate) fn next_same_key_retry_attempt<A: AiExecutionAttempt>(attempt: &A) -> Option<A> {
    let owned_report_context = attempt
        .report_context_ref()
        .is_none()
        .then(|| attempt.report_context())
        .flatten();
    let report_context = attempt
        .report_context_ref()
        .or(owned_report_context.as_ref());
    let identity = attempt_identity_from_report_context(report_context)?;
    let metadata = local_execution_candidate_metadata_from_report_context(report_context);
    let budget = SameKeyRetryBudget {
        policy_same_key_retries: metadata.same_key_retries,
        provider_same_key_retries: provider_same_key_retries_from_report_context(report_context),
    };
    let retry_index = next_same_key_retry_index(identity, budget)?;
    attempt.with_same_key_retry(retry_index, Uuid::new_v4().to_string())
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{
        attempt_identity_from_report_context,
        local_execution_candidate_metadata_from_report_context, next_same_key_retry_attempt,
        next_same_key_retry_index, provider_same_key_retries_from_report_context,
        ExecutionAttemptIdentity, LocalExecutionCandidateMetadata, SameKeyRetryBudget,
        POOL_KEY_RETRY_INDEX_STRIDE,
    };
    use aether_ai_serving::{AiExecutionAttempt, AiSyncAttempt};
    use aether_runtime_state::RuntimeLockLease;

    fn policy_budget(same_key_retries: Option<u32>) -> SameKeyRetryBudget {
        SameKeyRetryBudget {
            policy_same_key_retries: same_key_retries,
            provider_same_key_retries: None,
        }
    }

    /// A provider override next to a generous policy default, so tests prove
    /// the override wins rather than the policy.
    fn provider_budget(retries: u32) -> SameKeyRetryBudget {
        SameKeyRetryBudget {
            policy_same_key_retries: Some(5),
            provider_same_key_retries: Some(retries),
        }
    }

    #[test]
    fn default_budget_never_retries_on_the_same_key() {
        for candidate_index in 0..3 {
            assert_eq!(
                next_same_key_retry_index(
                    ExecutionAttemptIdentity::new(candidate_index, 0),
                    policy_budget(None)
                ),
                None
            );
            assert_eq!(
                next_same_key_retry_index(
                    ExecutionAttemptIdentity::new(candidate_index, 0),
                    policy_budget(Some(0))
                ),
                None
            );
        }
    }

    #[test]
    fn policy_default_applies_to_every_candidate() {
        for candidate_index in 0..5 {
            assert_eq!(
                next_same_key_retry_index(
                    ExecutionAttemptIdentity::new(candidate_index, 0),
                    policy_budget(Some(1))
                ),
                Some(1)
            );
            assert_eq!(
                next_same_key_retry_index(
                    ExecutionAttemptIdentity::new(candidate_index, 1),
                    policy_budget(Some(1))
                ),
                None,
                "one retry exhausts a policy default of 1"
            );
        }
    }

    #[test]
    fn policy_default_has_no_upper_bound() {
        assert_eq!(
            next_same_key_retry_index(
                ExecutionAttemptIdentity::new(0, 4_999),
                policy_budget(Some(10_000))
            ),
            Some(5_000)
        );
        assert_eq!(
            next_same_key_retry_index(
                ExecutionAttemptIdentity::new(0, 10_000),
                policy_budget(Some(10_000))
            ),
            None
        );
    }

    #[test]
    fn provider_override_applies_to_every_candidate() {
        for candidate_index in 0..5 {
            assert_eq!(
                next_same_key_retry_index(
                    ExecutionAttemptIdentity::new(candidate_index, 0),
                    provider_budget(2)
                ),
                Some(1)
            );
            assert_eq!(
                next_same_key_retry_index(
                    ExecutionAttemptIdentity::new(candidate_index, 1),
                    provider_budget(2)
                ),
                Some(2)
            );
            assert_eq!(
                next_same_key_retry_index(
                    ExecutionAttemptIdentity::new(candidate_index, 2),
                    provider_budget(2)
                ),
                None,
                "two retries exhaust a provider budget of 2"
            );
        }
    }

    #[test]
    fn provider_override_replaces_the_policy_default_in_both_directions() {
        assert_eq!(
            next_same_key_retry_index(ExecutionAttemptIdentity::new(0, 0), provider_budget(0)),
            None,
            "a provider override of 0 disables the policy's five retries"
        );
        let generous_provider = SameKeyRetryBudget {
            policy_same_key_retries: Some(0),
            provider_same_key_retries: Some(3),
        };
        assert_eq!(
            next_same_key_retry_index(ExecutionAttemptIdentity::new(1, 2), generous_provider),
            Some(3),
            "a provider override of 3 retries even when the policy default is 0"
        );
        assert_eq!(
            next_same_key_retry_index(ExecutionAttemptIdentity::new(1, 3), generous_provider),
            None
        );
    }

    #[test]
    fn every_pool_key_retries_within_its_stride() {
        let first_pool_key = ExecutionAttemptIdentity::new(0, 0).with_pool_key_index(Some(0));
        assert_eq!(
            next_same_key_retry_index(first_pool_key, policy_budget(Some(3))),
            Some(1)
        );

        let second_pool_key = ExecutionAttemptIdentity::new(0, POOL_KEY_RETRY_INDEX_STRIDE)
            .with_pool_key_index(Some(1));
        assert_eq!(
            next_same_key_retry_index(second_pool_key, provider_budget(2)),
            Some(POOL_KEY_RETRY_INDEX_STRIDE + 1)
        );
        let after_two_retries = ExecutionAttemptIdentity::new(0, POOL_KEY_RETRY_INDEX_STRIDE + 2)
            .with_pool_key_index(Some(1));
        assert_eq!(
            next_same_key_retry_index(after_two_retries, provider_budget(2)),
            None,
            "the second pool key counts its own retries from its stride base"
        );

        let at_stride_limit = ExecutionAttemptIdentity::new(0, 2 * POOL_KEY_RETRY_INDEX_STRIDE - 1)
            .with_pool_key_index(Some(1));
        assert_eq!(
            next_same_key_retry_index(at_stride_limit, provider_budget(10_000)),
            None,
            "retries never spill into the next pool key's index range"
        );
        assert_eq!(
            next_same_key_retry_index(
                ExecutionAttemptIdentity::new(0, 0).with_pool_key_index(Some(0)),
                policy_budget(None)
            ),
            None,
            "pool keys do not retry without a configured budget"
        );
    }

    #[test]
    fn provider_same_key_retries_read_local_failover_policy_max_retries() {
        assert_eq!(provider_same_key_retries_from_report_context(None), None);
        assert_eq!(
            provider_same_key_retries_from_report_context(Some(&json!({}))),
            None
        );
        assert_eq!(
            provider_same_key_retries_from_report_context(Some(&json!({
                "local_failover_policy": {}
            }))),
            None
        );
        assert_eq!(
            provider_same_key_retries_from_report_context(Some(&json!({
                "local_failover_policy": {"max_retries": null}
            }))),
            None
        );
        assert_eq!(
            provider_same_key_retries_from_report_context(Some(&json!({
                "local_failover_policy": {"max_retries": 3}
            }))),
            Some(3)
        );
        assert_eq!(
            provider_same_key_retries_from_report_context(Some(&json!({
                "local_failover_policy": {"max_retries": -1}
            }))),
            None
        );
    }

    fn sample_attempt(report_context: serde_json::Value) -> AiSyncAttempt {
        AiSyncAttempt {
            plan: aether_contracts::ExecutionPlan {
                request_id: "trace-1".to_string(),
                candidate_id: report_context
                    .get("candidate_id")
                    .and_then(serde_json::Value::as_str)
                    .map(ToOwned::to_owned),
                provider_name: None,
                provider_id: "provider-1".to_string(),
                endpoint_id: "endpoint-1".to_string(),
                key_id: "key-1".to_string(),
                method: "POST".to_string(),
                url: "https://example.com".to_string(),
                headers: Default::default(),
                content_type: None,
                content_encoding: None,
                body: aether_contracts::RequestBody {
                    json_body: None,
                    body_bytes_b64: None,
                    body_ref: None,
                },
                stream: false,
                client_api_format: "openai:chat".to_string(),
                provider_api_format: "openai:chat".to_string(),
                model_name: None,
                proxy: None,
                transport_profile: None,
                timeouts: None,
            },
            report_kind: None,
            report_context: Some(report_context),
        }
    }

    #[test]
    fn next_same_key_retry_attempt_rewrites_candidate_id_and_retry_index() {
        let attempt = sample_attempt(json!({
            "candidate_id": "candidate-a",
            "candidate_index": 0,
            "retry_index": 0,
            "same_key_retries": 1,
        }));

        let retry = next_same_key_retry_attempt(&attempt).expect("one same-key retry remains");
        let retry_candidate_id = retry.plan.candidate_id.clone().expect("fresh candidate id");
        assert_ne!(retry_candidate_id, "candidate-a");
        assert_eq!(retry.plan.key_id, "key-1");
        let context = retry.report_context_ref().expect("context retained");
        assert_eq!(context["candidate_id"], json!(retry_candidate_id));
        assert_eq!(context["retry_index"], json!(1));
        assert_eq!(context["candidate_index"], json!(0));

        assert!(
            next_same_key_retry_attempt(&retry).is_none(),
            "a budget of one retry is exhausted after one retry"
        );
        assert!(
            next_same_key_retry_attempt(&sample_attempt(json!({
                "candidate_id": "candidate-a",
                "candidate_index": 0,
                "retry_index": 0,
            })))
            .is_none(),
            "without any configured budget a failure fails over immediately"
        );
    }

    #[test]
    fn next_same_key_retry_attempt_honours_the_provider_override() {
        let failover_attempt = sample_attempt(json!({
            "candidate_id": "candidate-b",
            "candidate_index": 3,
            "scheduling_candidate_index": 1,
            "retry_index": 0,
            "same_key_retries": 0,
            "local_failover_policy": {"max_retries": 1},
        }));
        let retry = next_same_key_retry_attempt(&failover_attempt)
            .expect("a provider override retries failover candidates too");
        let context = retry.report_context_ref().expect("context retained");
        assert_eq!(context["retry_index"], json!(1));
        assert_eq!(context["candidate_index"], json!(3));
        assert_eq!(context["scheduling_candidate_index"], json!(1));
        assert!(
            next_same_key_retry_attempt(&retry).is_none(),
            "one provider retry is exhausted after one retry"
        );

        let disabled_by_provider = sample_attempt(json!({
            "candidate_id": "candidate-a",
            "candidate_index": 0,
            "retry_index": 0,
            "same_key_retries": 2,
            "local_failover_policy": {"max_retries": 0},
        }));
        assert!(
            next_same_key_retry_attempt(&disabled_by_provider).is_none(),
            "a provider override of 0 disables the policy default"
        );

        let inheriting_attempt = sample_attempt(json!({
            "candidate_id": "candidate-a",
            "candidate_index": 0,
            "retry_index": 0,
            "same_key_retries": 1,
            "local_failover_policy": {"max_retries": null},
        }));
        assert!(
            next_same_key_retry_attempt(&inheriting_attempt).is_some(),
            "a null override inherits the routing policy default"
        );
    }

    #[test]
    fn request_wide_trace_indices_do_not_affect_same_key_retry_budgets() {
        let identity = attempt_identity_from_report_context(Some(&json!({
            "candidate_index": 7, "scheduling_candidate_index": 2,
            "retry_index": 0, "pool_key_index": 0,
        })))
        .expect("attempt identity should parse");
        assert_eq!(identity.candidate_index, 7);
        assert_eq!(identity.scheduling_candidate_index, 2);
        assert_eq!(
            next_same_key_retry_index(identity, policy_budget(Some(1))),
            Some(1)
        );
        assert_eq!(
            next_same_key_retry_index(
                ExecutionAttemptIdentity {
                    retry_index: 1,
                    ..identity
                },
                policy_budget(Some(1)),
            ),
            None
        );
    }

    #[test]
    fn parse_attempt_identity_from_report_context_reads_candidate_and_retry_indices() {
        let identity = attempt_identity_from_report_context(Some(&json!({
            "candidate_index": 4,
            "retry_index": 1,
            "pool_key_index": 7,
        })))
        .expect("attempt identity should parse");

        assert_eq!(
            identity,
            ExecutionAttemptIdentity {
                candidate_index: 4,
                scheduling_candidate_index: 4,
                retry_index: 1,
                pool_key_index: Some(7),
            }
        );
    }

    #[test]
    fn parse_candidate_metadata_from_report_context_reads_group_and_pool_metadata() {
        let metadata = local_execution_candidate_metadata_from_report_context(Some(&json!({
            "candidate_group_id": "group-1",
            "pool_key_index": 3,
            "pool_key_lease_key": "ap:provider-1:lease:key-1",
            "pool_key_lease_owner": "gateway-1",
            "pool_key_lease_token": "gateway-1:token-1",
            "pool_key_lease_fencing_token": 7,
            "pool_key_lease_ttl_ms": 900000,
            "same_key_retries": 3,
        })));

        assert_eq!(
            metadata,
            LocalExecutionCandidateMetadata {
                candidate_group_id: Some("group-1".to_string()),
                pool_key_index: Some(3),
                pool_key_lease: Some(RuntimeLockLease {
                    key: "ap:provider-1:lease:key-1".to_string(),
                    owner: "gateway-1".to_string(),
                    token: "gateway-1:token-1".to_string(),
                    fencing_token: 7,
                    ttl_ms: 900000,
                }),
                scheduler_affinity_epoch: None,
                same_key_retries: Some(3),
            }
        );
    }
}
