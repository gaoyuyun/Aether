use super::verify::admin_provider_ops_value_as_f64;
use serde_json::{json, Map, Value};

#[derive(Debug, Clone)]
pub struct ProviderOpsCheckinOutcome {
    pub success: Option<bool>,
    pub message: String,
    pub cookie_expired: bool,
}

pub fn parse_query_balance_payload(
    architecture_id: &str,
    action_config: &Map<String, Value>,
    response_json: &Value,
) -> Result<Value, String> {
    match architecture_id {
        "generic_api" | "new_api" | "anyrouter" | "done_hub" => {
            parse_new_api_balance_payload(action_config, response_json)
        }
        _ => Err("Provider 操作仅支持 Rust execution runtime".to_string()),
    }
}

pub fn parse_sub2api_balance_payload(
    action_config: &Map<String, Value>,
    me_json: &Value,
    subscription_json: Option<&Value>,
) -> Result<Value, String> {
    let Some(me_payload) = me_json.as_object() else {
        return Err("响应格式无效".to_string());
    };
    if me_payload.get("code").and_then(Value::as_i64).unwrap_or(-1) != 0 {
        return Err(me_payload
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("查询用户信息失败")
            .to_string());
    }
    let Some(me_data) = me_payload.get("data").and_then(Value::as_object) else {
        return Err("响应格式无效".to_string());
    };

    let balance = value_as_f64(me_data.get("balance")).unwrap_or(0.0);
    let points = value_as_f64(me_data.get("points")).unwrap_or(0.0);
    let mut extra = Map::new();
    extra.insert("balance".to_string(), json!(balance));
    extra.insert("points".to_string(), json!(points));

    if let Some(subscription_json) = subscription_json {
        if let Some(subscription_payload) = subscription_json.as_object() {
            if subscription_payload
                .get("code")
                .and_then(Value::as_i64)
                .unwrap_or(-1)
                == 0
            {
                if let Some(summary) = subscription_payload.get("data").and_then(Value::as_object) {
                    if let Some(active_count) = summary.get("active_count") {
                        extra.insert("active_subscriptions".to_string(), active_count.clone());
                    }
                    if let Some(total_used_usd) = summary.get("total_used_usd") {
                        extra.insert("total_used_usd".to_string(), total_used_usd.clone());
                    }
                    if let Some(subscriptions) =
                        summary.get("subscriptions").and_then(Value::as_array)
                    {
                        extra.insert(
                            "subscriptions".to_string(),
                            Value::Array(
                                subscriptions
                                    .iter()
                                    .filter_map(parse_subscription)
                                    .collect(),
                            ),
                        );
                    }
                }
            }
        }
    }

    Ok(build_balance_data(
        None,
        None,
        Some(balance + points),
        action_config
            .get("currency")
            .and_then(Value::as_str)
            .unwrap_or("USD"),
        extra,
    ))
}

pub fn attach_balance_checkin_outcome(
    action_payload: &mut Value,
    outcome: &ProviderOpsCheckinOutcome,
) {
    if let Some(data) = action_payload
        .get_mut("data")
        .and_then(Value::as_object_mut)
    {
        let extra = data
            .entry("extra".to_string())
            .or_insert_with(|| Value::Object(Map::new()));
        if let Some(extra) = extra.as_object_mut() {
            if outcome.cookie_expired {
                extra.insert("cookie_expired".to_string(), Value::Bool(true));
                extra.insert(
                    "cookie_expired_message".to_string(),
                    Value::String(outcome.message.clone()),
                );
            } else {
                extra.insert(
                    "checkin_success".to_string(),
                    outcome.success.map(Value::Bool).unwrap_or(Value::Null),
                );
                extra.insert(
                    "checkin_message".to_string(),
                    Value::String(outcome.message.clone()),
                );
            }
        }
    }
    if outcome.cookie_expired {
        if let Some(object) = action_payload.as_object_mut() {
            object.insert("status".to_string(), json!("auth_expired"));
        }
    }
}

pub fn build_balance_data(
    total_granted: Option<f64>,
    total_used: Option<f64>,
    total_available: Option<f64>,
    currency: &str,
    extra: Map<String, Value>,
) -> Value {
    json!({
        "total_granted": total_granted,
        "total_used": total_used,
        "total_available": total_available,
        "expires_at": Value::Null,
        "currency": currency,
        "extra": extra,
    })
}

fn parse_new_api_balance_payload(
    action_config: &Map<String, Value>,
    response_json: &Value,
) -> Result<Value, String> {
    let user_data = if response_json.get("success").and_then(Value::as_bool) == Some(true)
        && response_json.get("data").is_some_and(Value::is_object)
    {
        response_json.get("data")
    } else if response_json.get("success").and_then(Value::as_bool) == Some(false) {
        return Err(response_json
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("业务状态码表示失败")
            .to_string());
    } else {
        Some(response_json)
    };
    let Some(user_data) = user_data.and_then(Value::as_object) else {
        return Err("响应格式无效".to_string());
    };
    let quota_divisor = quota_divisor(action_config);
    let total_available =
        admin_provider_ops_value_as_f64(user_data.get("quota")).map(|value| value / quota_divisor);
    let total_used = admin_provider_ops_value_as_f64(user_data.get("used_quota"))
        .map(|value| value / quota_divisor);
    Ok(build_balance_data(
        None,
        total_used,
        total_available,
        action_config
            .get("currency")
            .and_then(Value::as_str)
            .unwrap_or("USD"),
        Map::new(),
    ))
}

fn quota_divisor(action_config: &Map<String, Value>) -> f64 {
    admin_provider_ops_value_as_f64(action_config.get("quota_divisor"))
        .filter(|value| *value > 0.0)
        .unwrap_or(500000.0)
}

fn value_as_f64(value: Option<&Value>) -> Option<f64> {
    match value {
        Some(Value::Number(number)) => number.as_f64(),
        Some(Value::String(raw)) => raw.trim().parse::<f64>().ok(),
        _ => None,
    }
}

fn parse_subscription(value: &Value) -> Option<Value> {
    let item = value.as_object()?;
    let mut subscription = Map::new();
    subscription.insert(
        "group_name".to_string(),
        item.get("group_name")
            .cloned()
            .unwrap_or_else(|| Value::String(String::new())),
    );
    subscription.insert(
        "status".to_string(),
        item.get("status")
            .cloned()
            .unwrap_or_else(|| Value::String(String::new())),
    );
    for field in [
        "daily_used_usd",
        "daily_limit_usd",
        "weekly_used_usd",
        "weekly_limit_usd",
        "monthly_used_usd",
        "monthly_limit_usd",
        "expires_at",
    ] {
        if let Some(value) = item.get(field).filter(|value| !value.is_null()) {
            subscription.insert(field.to_string(), value.clone());
        }
    }
    Some(Value::Object(subscription))
}

#[cfg(test)]
mod tests {
    use super::{
        attach_balance_checkin_outcome, parse_query_balance_payload, parse_sub2api_balance_payload,
        ProviderOpsCheckinOutcome,
    };
    use serde_json::json;

    #[test]
    fn anyrouter_single_request_parser_uses_usage_fields() {
        let payload = parse_query_balance_payload(
            "anyrouter",
            &json!({ "quota_divisor": 500000 })
                .as_object()
                .cloned()
                .expect("config"),
            &json!({
                "quota": 2500000,
                "used_quota": 500000
            }),
        )
        .expect("payload should parse");

        assert_eq!(payload["total_available"], json!(5.0));
        assert_eq!(payload["total_used"], json!(1.0));
    }

    #[test]
    fn done_hub_single_request_parser_reads_wrapped_quota() {
        let payload = parse_query_balance_payload(
            "done_hub",
            &json!({ "quota_divisor": 500000 })
                .as_object()
                .cloned()
                .expect("config"),
            &json!({
                "success": true,
                "data": {
                    "quota": 2_276_139_911_u64,
                    "used_quota": 13860089
                }
            }),
        )
        .expect("payload should parse");

        assert_eq!(payload["total_available"], json!(4552.279822));
        assert_eq!(payload["total_used"], json!(27.720178));
    }

    #[test]
    fn sub2api_parser_sums_balance_and_points() {
        let payload = parse_sub2api_balance_payload(
            &json!({ "currency": "USD" })
                .as_object()
                .cloned()
                .expect("config"),
            &json!({
                "code": 0,
                "data": {
                    "balance": 8.5,
                    "points": 1.5
                }
            }),
            Some(&json!({
                "code": 0,
                "data": {
                    "active_count": 2,
                    "subscriptions": []
                }
            })),
        )
        .expect("payload should parse");

        assert_eq!(payload["total_available"], json!(10.0));
        assert_eq!(payload["extra"]["active_subscriptions"], json!(2));
    }

    #[test]
    fn attach_balance_checkin_outcome_marks_auth_expired() {
        let mut payload = json!({
            "status": "success",
            "data": { "extra": {} }
        });
        attach_balance_checkin_outcome(
            &mut payload,
            &ProviderOpsCheckinOutcome {
                success: None,
                message: "Cookie 已失效".to_string(),
                cookie_expired: true,
            },
        );

        assert_eq!(payload["status"], json!("auth_expired"));
        assert_eq!(payload["data"]["extra"]["cookie_expired"], json!(true));
    }
}
