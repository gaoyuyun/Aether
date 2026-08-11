use crate::handlers::shared::{module_available_from_env, system_config_bool};
use crate::{AppState, GatewayError};

pub(crate) const STANDALONE_KEYS_MODULE_CONFIG_KEY: &str = "module.standalone_keys.enabled";

pub(crate) async fn standalone_keys_module_enabled(state: &AppState) -> Result<bool, GatewayError> {
    if !module_available_from_env("STANDALONE_KEYS_AVAILABLE", true) {
        return Ok(false);
    }
    let value = state
        .read_system_config_json_value(STANDALONE_KEYS_MODULE_CONFIG_KEY)
        .await?;
    Ok(system_config_bool(value.as_ref(), false))
}
