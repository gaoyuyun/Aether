use crate::data::GatewayDataState;
use crate::{AppState, GatewayError};
use aether_data::DataLayerError;

pub(crate) const WALLET_MODULE_CONFIG_KEY: &str = "module.wallet.enabled";
pub(crate) const BILLING_PLANS_MODULE_CONFIG_KEY: &str = "module.billing_plans.enabled";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct CommerceBillingPolicy {
    pub(crate) wallet_enabled: bool,
    pub(crate) billing_plans_enabled: bool,
}

fn env_available(key: &str) -> bool {
    match std::env::var(key) {
        Ok(value) => matches!(
            value.trim().to_ascii_lowercase().as_str(),
            "true" | "1" | "yes"
        ),
        Err(_) => true,
    }
}

fn config_enabled(value: Option<&serde_json::Value>) -> bool {
    match value {
        Some(serde_json::Value::Bool(value)) => *value,
        Some(serde_json::Value::Number(value)) => value.as_i64().is_some_and(|value| value != 0),
        Some(serde_json::Value::String(value)) => matches!(
            value.trim().to_ascii_lowercase().as_str(),
            "true" | "1" | "yes" | "on"
        ),
        _ => false,
    }
}

// Commerce flags intentionally use the shared 30-second config cache. Module changes follow the
// maintenance-window contract in docs/operations/commerce-modules-runbook.md.
pub(crate) async fn wallet_module_enabled(state: &AppState) -> Result<bool, GatewayError> {
    if !env_available("WALLET_AVAILABLE") {
        return Ok(false);
    }
    let value = state
        .read_system_config_json_value(WALLET_MODULE_CONFIG_KEY)
        .await?;
    Ok(config_enabled(value.as_ref()))
}

pub(crate) async fn commerce_billing_policy(
    state: &AppState,
) -> Result<CommerceBillingPolicy, GatewayError> {
    let wallet_enabled = wallet_module_enabled(state).await?;
    let billing_plans_enabled = if wallet_enabled && env_available("BILLING_PLANS_AVAILABLE") {
        let value = state
            .read_system_config_json_value(BILLING_PLANS_MODULE_CONFIG_KEY)
            .await?;
        config_enabled(value.as_ref())
    } else {
        false
    };
    Ok(CommerceBillingPolicy {
        wallet_enabled,
        billing_plans_enabled,
    })
}

pub(crate) async fn billing_plans_module_enabled(state: &AppState) -> Result<bool, GatewayError> {
    Ok(commerce_billing_policy(state).await?.billing_plans_enabled)
}

pub(crate) async fn wallet_module_enabled_for_data(
    state: &GatewayDataState,
) -> Result<bool, DataLayerError> {
    if !env_available("WALLET_AVAILABLE") {
        return Ok(false);
    }
    let value = state
        .find_system_config_value(WALLET_MODULE_CONFIG_KEY)
        .await?;
    Ok(config_enabled(value.as_ref()))
}

pub(crate) async fn commerce_billing_policy_for_data(
    state: &GatewayDataState,
) -> Result<CommerceBillingPolicy, DataLayerError> {
    let wallet_enabled = wallet_module_enabled_for_data(state).await?;
    let billing_plans_enabled = if wallet_enabled && env_available("BILLING_PLANS_AVAILABLE") {
        let value = state
            .find_system_config_value(BILLING_PLANS_MODULE_CONFIG_KEY)
            .await?;
        config_enabled(value.as_ref())
    } else {
        false
    };
    Ok(CommerceBillingPolicy {
        wallet_enabled,
        billing_plans_enabled,
    })
}

pub(crate) fn mark_wallet_summary_unlimited(payload: &mut serde_json::Value) {
    payload["limit_mode"] = serde_json::Value::String("unlimited".to_string());
    payload["unlimited"] = serde_json::Value::Bool(true);
}

#[cfg(test)]
mod tests {
    use super::{config_enabled, mark_wallet_summary_unlimited};
    use serde_json::json;

    #[test]
    fn missing_wallet_module_config_is_disabled_by_default() {
        assert!(!config_enabled(None));
        assert!(!config_enabled(Some(&json!(false))));
        assert!(config_enabled(Some(&json!(true))));
    }

    #[test]
    fn disabled_wallet_module_is_reported_as_unlimited() {
        let mut payload = json!({ "limit_mode": "finite", "unlimited": false });

        mark_wallet_summary_unlimited(&mut payload);

        assert_eq!(payload["limit_mode"], json!("unlimited"));
        assert_eq!(payload["unlimited"], json!(true));
    }
}
