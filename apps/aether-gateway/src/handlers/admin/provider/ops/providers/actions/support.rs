use serde_json::json;

pub(super) fn admin_provider_ops_checkin_data(
    reward: Option<f64>,
    streak_days: Option<i64>,
    next_reward: Option<f64>,
    message: Option<String>,
    extra: serde_json::Map<String, serde_json::Value>,
) -> serde_json::Value {
    json!({
        "reward": reward,
        "streak_days": streak_days,
        "next_reward": next_reward,
        "message": message,
        "extra": extra,
    })
}

pub(super) fn admin_provider_ops_json_object_map(
    value: serde_json::Value,
) -> serde_json::Map<String, serde_json::Value> {
    value.as_object().cloned().unwrap_or_default()
}

pub(super) fn admin_provider_ops_request_url(
    base_url: &str,
    action_config: &serde_json::Map<String, serde_json::Value>,
    default_endpoint: &str,
) -> String {
    let endpoint = action_config
        .get("endpoint")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or(default_endpoint);
    if endpoint.starts_with("http://") || endpoint.starts_with("https://") {
        endpoint.to_string()
    } else {
        format!("{}{}", base_url.trim_end_matches('/'), endpoint)
    }
}

pub(super) fn admin_provider_ops_request_method(
    action_config: &serde_json::Map<String, serde_json::Value>,
    default_method: &str,
) -> reqwest::Method {
    action_config
        .get("method")
        .and_then(serde_json::Value::as_str)
        .and_then(|value| reqwest::Method::from_bytes(value.trim().as_bytes()).ok())
        .unwrap_or_else(|| {
            reqwest::Method::from_bytes(default_method.as_bytes()).unwrap_or(reqwest::Method::GET)
        })
}
