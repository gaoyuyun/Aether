use super::super::errors::{
    build_internal_control_error_response, normalize_provider_oauth_refresh_error_message,
};
use crate::handlers::admin::request::{AdminAppState, AdminProviderOAuthTemplate};
use aether_contracts::ProxySnapshot;
use aether_oauth::provider::providers::{
    AntigravityProviderOAuthAdapter, ClaudeCodeProviderOAuthAdapter, GenericProviderOAuthAdapter,
    ANTIGRAVITY_USER_INFO_URL, CLAUDE_CODE_PROVIDER_TYPE, CLAUDE_CODE_TOKEN_URL,
    CLAUDE_CODE_WEB_BASE_URL,
};
use aether_oauth::provider::{
    ProviderOAuthCookieAuthorizationInput, ProviderOAuthService, ProviderOAuthTransportContext,
};
use axum::{body::Body, http, response::Response};
use std::sync::Arc;

fn provider_oauth_transport_error_detail(prefix: &str, _error: &str) -> String {
    prefix.to_string()
}

fn provider_oauth_exchange_context(
    provider_type: &str,
    proxy: Option<ProxySnapshot>,
) -> ProviderOAuthTransportContext {
    provider_oauth_exchange_context_with_config(provider_type, proxy, None)
}

/// 带供应商 `config` 的交换上下文：交换阶段还没有 Key 快照，供应商级
/// `config.fingerprint.transport_profile` 是唯一能决定控制面 TLS profile 的来源。
fn provider_oauth_exchange_context_with_config(
    provider_type: &str,
    proxy: Option<ProxySnapshot>,
    provider_config: Option<serde_json::Value>,
) -> ProviderOAuthTransportContext {
    ProviderOAuthTransportContext {
        provider_id: String::new(),
        provider_type: provider_type.to_string(),
        endpoint_id: None,
        key_id: None,
        auth_type: Some("oauth".to_string()),
        decrypted_api_key: None,
        decrypted_auth_config: None,
        provider_config,
        endpoint_config: None,
        key_config: None,
        network: aether_oauth::network::OAuthNetworkContext::provider_operation(proxy),
    }
}

fn provider_oauth_service_for_template(
    template: AdminProviderOAuthTemplate,
    token_url: String,
    antigravity_user_info_url: String,
) -> Result<ProviderOAuthService, Response<Body>> {
    if template.provider_type.eq_ignore_ascii_case("antigravity") {
        let adapter = AntigravityProviderOAuthAdapter::default()
            .with_token_url_override(token_url)
            .with_user_info_url_override(antigravity_user_info_url);
        #[cfg(test)]
        let adapter = adapter.with_oauth_credentials_for_tests(
            "gateway-test-antigravity-client-id",
            "gateway-test-antigravity-client-secret",
        );
        return Ok(ProviderOAuthService::new().with_adapter(Arc::new(adapter)));
    }
    GenericProviderOAuthAdapter::for_provider_type(template.provider_type)
        .map(|adapter| adapter.with_token_url_override(token_url))
        .map(|adapter| ProviderOAuthService::new().with_adapter(Arc::new(adapter)))
        .ok_or_else(|| {
            build_internal_control_error_response(
                http::StatusCode::BAD_REQUEST,
                "该 Provider 不支持 OAuth 授权",
            )
        })
}

/// 适配器在交换/刷新后补齐的账号信息（P2.6：Claude 的 `/api/oauth/profile` 与
/// `claude_cli/roles`）不在 token 响应里；把这些键并入原始 payload，后续
/// `build_provider_oauth_auth_config_from_token_payload` 才能带走它们。
const ADAPTER_ENRICHED_AUTH_CONFIG_KEYS: &[&str] = &[
    "org_name",
    "account_name",
    "plan_type",
    "claude_cli_roles",
    "profile",
];

fn token_payload_from_provider_oauth_result(
    result: aether_oauth::provider::ProviderOAuthTokenSet,
) -> Result<serde_json::Value, Response<Body>> {
    let mut payload = result.token_set.raw_payload.ok_or_else(|| {
        build_internal_control_error_response(
            http::StatusCode::BAD_REQUEST,
            "token exchange 返回缺少 access_token",
        )
    })?;
    if let (Some(payload_object), Some(auth_config)) =
        (payload.as_object_mut(), result.auth_config.as_object())
    {
        for key in ADAPTER_ENRICHED_AUTH_CONFIG_KEYS {
            if let Some(value) = auth_config.get(*key) {
                payload_object
                    .entry((*key).to_string())
                    .or_insert_with(|| value.clone());
            }
        }
    }
    Ok(payload)
}

pub(crate) async fn exchange_admin_provider_oauth_code(
    state: &AdminAppState<'_>,
    template: AdminProviderOAuthTemplate,
    code: &str,
    state_nonce: &str,
    pkce_verifier: Option<&str>,
    proxy: Option<ProxySnapshot>,
    provider_config: Option<serde_json::Value>,
) -> Result<serde_json::Value, Response<Body>> {
    let token_url = state.provider_oauth_token_url(template.provider_type, template.token_url);
    let antigravity_user_info_url =
        state.provider_oauth_token_url("antigravity_user_info", ANTIGRAVITY_USER_INFO_URL);
    let service =
        provider_oauth_service_for_template(template, token_url, antigravity_user_info_url)?;
    let ctx =
        provider_oauth_exchange_context_with_config(template.provider_type, proxy, provider_config);
    let executor = crate::oauth::GatewayOAuthHttpExecutor::new(*state);
    let result = service
        .exchange_code(&executor, &ctx, code, state_nonce, pkce_verifier)
        .await
        .map_err(|error| match error {
            aether_oauth::core::OAuthError::HttpStatus { .. } => {
                build_internal_control_error_response(
                    http::StatusCode::BAD_REQUEST,
                    "token exchange 失败",
                )
            }
            error => build_internal_control_error_response(
                http::StatusCode::BAD_REQUEST,
                provider_oauth_transport_error_detail("token exchange 失败", &error.to_string()),
            ),
        })?;
    token_payload_from_provider_oauth_result(result)
}

pub(crate) async fn exchange_admin_provider_oauth_refresh_token(
    state: &AdminAppState<'_>,
    template: AdminProviderOAuthTemplate,
    refresh_token: &str,
    proxy: Option<ProxySnapshot>,
    provider_config: Option<serde_json::Value>,
) -> Result<serde_json::Value, Response<Body>> {
    let token_url = state.provider_oauth_token_url(template.provider_type, template.token_url);
    let antigravity_user_info_url =
        state.provider_oauth_token_url("antigravity_user_info", ANTIGRAVITY_USER_INFO_URL);
    let service =
        provider_oauth_service_for_template(template, token_url, antigravity_user_info_url)?;
    let ctx =
        provider_oauth_exchange_context_with_config(template.provider_type, proxy, provider_config);
    let executor = crate::oauth::GatewayOAuthHttpExecutor::new(*state);
    let input = aether_oauth::provider::ProviderOAuthImportInput {
        provider_type: template.provider_type.to_string(),
        name: None,
        refresh_token: Some(refresh_token.to_string()),
        raw_credentials: None,
        network: ctx.network.clone(),
    };
    let result = service
        .import_credentials(&executor, &ctx, input)
        .await
        .map_err(|error| match error {
            aether_oauth::core::OAuthError::HttpStatus {
                status_code,
                body_excerpt,
            } => {
                let reason = normalize_provider_oauth_refresh_error_message(
                    Some(status_code),
                    Some(&body_excerpt),
                );
                build_internal_control_error_response(
                    http::StatusCode::BAD_REQUEST,
                    format!("Refresh Token 验证失败: {reason}"),
                )
            }
            error => build_internal_control_error_response(
                http::StatusCode::BAD_REQUEST,
                provider_oauth_transport_error_detail(
                    "Refresh Token 验证失败: token exchange 失败",
                    &error.to_string(),
                ),
            ),
        })?;
    token_payload_from_provider_oauth_result(result).map_err(|_| {
        build_internal_control_error_response(
            http::StatusCode::BAD_REQUEST,
            "token refresh 返回缺少 access_token",
        )
    })
}

pub(crate) async fn authorize_admin_provider_oauth_with_cookie(
    state: &AdminAppState<'_>,
    session_key: String,
    proxy: Option<ProxySnapshot>,
) -> Result<serde_json::Value, Response<Body>> {
    let web_base_url =
        state.provider_oauth_token_url("claude_code_cookie_base_url", CLAUDE_CODE_WEB_BASE_URL);
    let token_url =
        state.provider_oauth_token_url(CLAUDE_CODE_PROVIDER_TYPE, CLAUDE_CODE_TOKEN_URL);
    let service = ProviderOAuthService::new().with_adapter(Arc::new(
        ClaudeCodeProviderOAuthAdapter::default().with_endpoint_overrides(web_base_url, token_url),
    ));
    let ctx = provider_oauth_exchange_context(CLAUDE_CODE_PROVIDER_TYPE, proxy);
    let executor = crate::oauth::GatewayOAuthHttpExecutor::new(*state);
    let result = service
        .authorize_with_cookie(
            &executor,
            &ctx,
            ProviderOAuthCookieAuthorizationInput { session_key },
        )
        .await
        .map_err(|error| {
            let detail = if matches!(error, aether_oauth::core::OAuthError::InvalidRequest(_)) {
                "Claude Cookie 格式无效"
            } else {
                "Claude Cookie 授权失败"
            };
            build_internal_control_error_response(http::StatusCode::BAD_REQUEST, detail)
        })?;
    token_payload_from_provider_oauth_result(result).map_err(|_| {
        build_internal_control_error_response(
            http::StatusCode::BAD_REQUEST,
            "Claude Cookie 授权返回缺少 access_token",
        )
    })
}

#[cfg(test)]
mod tests {
    use super::{
        provider_oauth_exchange_context_with_config, provider_oauth_transport_error_detail,
    };
    use aether_oauth::provider::providers::claude_code_tls_emulation_configured;
    use serde_json::json;

    /// 交换阶段没有 Key 快照：供应商 `config.fingerprint.transport_profile` 必须进上下文，
    /// 否则首次 token 交换永远走不到控制面 TLS profile。
    #[test]
    fn exchange_context_carries_provider_transport_profile_config() {
        let bare = provider_oauth_exchange_context_with_config("claude_code", None, None);
        assert!(!claude_code_tls_emulation_configured(&bare));

        let configured = provider_oauth_exchange_context_with_config(
            "claude_code",
            None,
            Some(json!({
                "fingerprint": {"transport_profile": "claude_code_node_openssl"}
            })),
        );
        assert!(claude_code_tls_emulation_configured(&configured));
    }

    #[test]
    fn provider_oauth_transport_error_does_not_reflect_network_details() {
        let detail = provider_oauth_transport_error_detail(
            "token exchange 失败",
            "request failed for https://user:pass@example.test/token?secret=value authorization=Bearer upstream-secret",
        );

        assert_eq!(detail, "token exchange 失败");
        assert!(!detail.contains("upstream-secret"));
        assert!(!detail.contains("user:pass"));
    }
}
