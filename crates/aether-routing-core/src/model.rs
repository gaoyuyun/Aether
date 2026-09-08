use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::actions::{
    RoutingAction, RoutingRulePhase, RoutingSchedulingMode, RoutingSetPriorityMode,
};
use crate::conditions::RoutingCondition;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RoutingSchedulingPreset {
    pub preset: String,
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct RoutingPoolPolicyOverride {
    #[serde(default)]
    pub scheduling_presets: Vec<RoutingSchedulingPreset>,
}

/// Default number of attempts on the first-ranked (sticky) candidate before
/// failing over: one retry on the same key.
pub const DEFAULT_STICKY_KEY_ATTEMPTS: u32 = 2;

/// Request-independent execution behaviours selected by a routing strategy.
///
/// These flags deliberately live beside scheduling rather than in provider
/// transport configuration. A resolved policy is snapshotted for the request
/// and can therefore be consumed by execution without rereading mutable
/// system settings.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Default)]
pub struct RoutingExecutionPolicy {
    #[serde(default, skip_serializing_if = "is_false")]
    pub enable_cf_heartbeat: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    pub cyber_continue_failover: bool,
}

impl<'de> Deserialize<'de> for RoutingExecutionPolicy {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize, Default)]
        struct LegacyCompatibleExecutionPolicy {
            #[serde(default)]
            enable_cf_heartbeat: bool,
            #[serde(default)]
            enable_openai_image_sync_heartbeat: bool,
            #[serde(default)]
            enable_standard_text_sync_heartbeat: bool,
            #[serde(default)]
            cyber_continue_failover: bool,
        }

        let value = LegacyCompatibleExecutionPolicy::deserialize(deserializer)?;
        Ok(Self {
            enable_cf_heartbeat: value.enable_cf_heartbeat
                || value.enable_openai_image_sync_heartbeat
                || value.enable_standard_text_sync_heartbeat,
            cyber_continue_failover: value.cyber_continue_failover,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RoutingDefaultPolicy {
    #[serde(default)]
    pub priority_mode: RoutingSetPriorityMode,
    #[serde(default)]
    pub scheduling_mode: RoutingSchedulingMode,
    #[serde(default)]
    pub keep_priority_on_conversion: bool,
    /// Total attempts on the first-ranked candidate before moving on. Later
    /// candidates always get a single attempt so failover keeps advancing.
    /// `0` and `1` both mean no same-key retry.
    #[serde(default = "default_sticky_key_attempts")]
    pub sticky_key_attempts: u32,
    /// Strategy-scoped execution behaviour. Flattened for a stable JSON
    /// shape and backwards-compatible migration from system settings.
    #[serde(flatten)]
    pub execution_policy: RoutingExecutionPolicy,
}

impl Default for RoutingDefaultPolicy {
    fn default() -> Self {
        Self {
            priority_mode: RoutingSetPriorityMode::default(),
            scheduling_mode: RoutingSchedulingMode::default(),
            keep_priority_on_conversion: false,
            sticky_key_attempts: DEFAULT_STICKY_KEY_ATTEMPTS,
            execution_policy: RoutingExecutionPolicy::default(),
        }
    }
}

fn default_sticky_key_attempts() -> u32 {
    DEFAULT_STICKY_KEY_ATTEMPTS
}

fn is_false(value: &bool) -> bool {
    !*value
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct RoutingModelPolicy {
    pub model: String,
    #[serde(default)]
    pub allowed_providers: Vec<String>,
    #[serde(default)]
    pub allowed_keys: Vec<String>,
    #[serde(default)]
    pub provider_priority_overrides: BTreeMap<String, i32>,
    #[serde(default)]
    pub key_priority_overrides: BTreeMap<String, i32>,
    /// Key priority overrides scoped to one API format: `api_format -> key_id -> priority`.
    ///
    /// A key can serve several API formats and legacy `global_priority_by_format`
    /// ranks it independently per format. Entries here take precedence over
    /// `key_priority_overrides` when the candidate format matches.
    #[serde(default)]
    pub key_priority_overrides_by_format: BTreeMap<String, BTreeMap<String, i32>>,
    #[serde(default)]
    pub pool_priority_overrides: BTreeMap<String, i32>,
    #[serde(default)]
    pub pool_policy_overrides: BTreeMap<String, RoutingPoolPolicyOverride>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RoutingRule {
    pub id: String,
    #[serde(default)]
    pub priority: i32,
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default)]
    pub phase: RoutingRulePhase,
    #[serde(default)]
    pub conditions: RoutingCondition,
    #[serde(default)]
    pub actions: Vec<RoutingAction>,
    #[serde(default)]
    pub stop_processing: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct RoutingGroupConfig {
    /// The default policy is global for the selected strategy group. Model
    /// differences are expressed through `model_policies` and `rules`.
    #[serde(default)]
    pub default_policy: RoutingDefaultPolicy,
    #[serde(default)]
    pub model_policies: Vec<RoutingModelPolicy>,
    #[serde(default)]
    pub rules: Vec<RoutingRule>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RoutingGroupRecord {
    pub id: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub enabled: bool,
    pub is_system_default: bool,
    pub config_json: Value,
    pub version: i64,
    pub created_at: i64,
    pub updated_at: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub published_at: Option<i64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RoutingGroupBindingSubject {
    User,
    ApiKey,
    UserGroup,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RoutingGroupBinding {
    pub id: String,
    pub group_id: String,
    pub subject_type: RoutingGroupBindingSubject,
    pub subject_id: String,
    pub is_default: bool,
    pub allow_explicit_select: bool,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RoutingGroupVersionRecord {
    pub id: String,
    pub group_id: String,
    pub version: i64,
    pub config_json: Value,
    pub created_at: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_by: Option<String>,
}

fn default_true() -> bool {
    true
}
