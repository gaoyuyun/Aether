use super::{
    json_object, ProviderOpsActionSpec, ProviderOpsArchitectureSpec, ProviderOpsAuthSpec,
    ProviderOpsBalanceMode, ProviderOpsCheckinMode, ProviderOpsVerifyMode,
};
use serde_json::{json, Map, Value};

pub(super) fn spec() -> ProviderOpsArchitectureSpec {
    let credentials_schema = json!({
        "type": "object",
        "properties": {
            "api_key": {
                "type": "string",
                "title": "API Key",
                "description": "提供商签发的 API Key",
                "x-sensitive": true,
                "x-input-type": "password"
            },
            "base_url": {
                "type": "string",
                "title": "站点地址",
                "description": "API 基础地址"
            }
        },
        "required": ["api_key"],
        "x-auth-method": "bearer",
        "x-auth-type": "api_key",
        "x-currency": "USD",
        "x-field-groups": [
            { "fields": ["base_url"] },
            { "fields": ["api_key"] }
        ],
        "x-validation": [
            {
                "type": "required",
                "fields": ["api_key"],
                "message": "请填写 API Key"
            }
        ]
    });

    ProviderOpsArchitectureSpec {
        architecture_id: "usage_api",
        display_name: "API Key 用量查询",
        description: "使用 Provider API Key 查询兼容 /v1/usage 的用量接口",
        hidden: false,
        credentials_schema: credentials_schema.clone(),
        verify_endpoint: "/v1/usage",
        verify_mode: ProviderOpsVerifyMode::DirectGet,
        balance_mode: ProviderOpsBalanceMode::SingleRequest,
        checkin_mode: ProviderOpsCheckinMode::None,
        query_balance_cookie_auth_errors: false,
        supported_auth_types: vec![ProviderOpsAuthSpec {
            auth_type: "api_key",
            display_name: "Provider API Key",
            credentials_schema,
        }],
        supported_actions: vec![ProviderOpsActionSpec {
            action_type: "query_balance",
            display_name: "查询余额",
            description: "查询 API Key 的剩余额度",
            config_schema: json!({
                "type": "object",
                "properties": {
                    "endpoint": {
                        "type": "string",
                        "title": "API 路径",
                        "description": "用量查询 API 路径",
                        "default": "/v1/usage"
                    },
                    "currency": {
                        "type": "string",
                        "title": "默认货币单位",
                        "default": "USD"
                    }
                },
                "required": []
            }),
        }],
        default_connector: Some("api_key"),
    }
}

pub(super) fn default_action_config(action_type: &str) -> Option<Map<String, Value>> {
    match action_type {
        "query_balance" => Some(json_object(json!({
            "endpoint": "/v1/usage",
            "method": "GET",
            "currency": "USD"
        }))),
        _ => None,
    }
}
