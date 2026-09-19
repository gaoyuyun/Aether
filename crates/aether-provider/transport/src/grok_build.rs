//! Grok Build（xAI Grok CLI）传输层策略。
//!
//! 上游 `cli-chat-proxy.grok.com` 只接受带 Grok CLI 身份头的请求，这些头在
//! 出站策略阶段固定注入；Bearer 由通用 OAuth 路径解析，本模块不碰凭据。

use std::collections::BTreeMap;

use crate::outbound_request_policy::{
    ProviderOutboundRequestIdentityScope, ProviderOutboundRequestMutationScope,
    ProviderOutboundRequestPolicy, ProviderOutboundRequestPolicyReason,
    ProviderOutboundRequestPolicyResult,
};
use crate::snapshot::GatewayProviderTransportSnapshot;

pub use crate::provider_types::{GROK_BUILD_DEFAULT_BASE_URL, GROK_BUILD_PROVIDER_TYPE};

/// 与 CLIProxyAPI 对齐的 Grok CLI 客户端身份；chat-proxy 校验这些头。
pub const GROK_BUILD_CLIENT_VERSION: &str = "0.2.120";
pub const GROK_BUILD_CLIENT_IDENTIFIER: &str = "grok-shell";
pub const GROK_BUILD_TOKEN_AUTH_HEADER: &str = "x-xai-token-auth";
pub const GROK_BUILD_TOKEN_AUTH_VALUE: &str = "xai-grok-cli";
pub const GROK_BUILD_CLIENT_VERSION_HEADER: &str = "x-grok-client-version";
pub const GROK_BUILD_CLIENT_IDENTIFIER_HEADER: &str = "x-grok-client-identifier";
pub const GROK_BUILD_AUTHENTICATE_RESPONSE_HEADER: &str = "x-authenticateresponse";
pub const GROK_BUILD_AUTHENTICATE_RESPONSE_VALUE: &str = "authenticate-response";

pub fn is_grok_build_provider_transport(transport: &GatewayProviderTransportSnapshot) -> bool {
    transport
        .provider
        .provider_type
        .trim()
        .eq_ignore_ascii_case(GROK_BUILD_PROVIDER_TYPE)
}

fn is_cli_chat_proxy_base_url(base_url: &str) -> bool {
    url::Url::parse(base_url.trim())
        .ok()
        .and_then(|url| url.host_str().map(|host| host.to_ascii_lowercase()))
        .is_some_and(|host| host == "cli-chat-proxy.grok.com")
}

pub fn grok_build_client_headers() -> BTreeMap<String, String> {
    BTreeMap::from([
        (
            GROK_BUILD_TOKEN_AUTH_HEADER.to_string(),
            GROK_BUILD_TOKEN_AUTH_VALUE.to_string(),
        ),
        (
            GROK_BUILD_CLIENT_VERSION_HEADER.to_string(),
            GROK_BUILD_CLIENT_VERSION.to_string(),
        ),
        (
            "user-agent".to_string(),
            format!("xai-grok-workspace/{GROK_BUILD_CLIENT_VERSION}"),
        ),
        (
            GROK_BUILD_CLIENT_IDENTIFIER_HEADER.to_string(),
            GROK_BUILD_CLIENT_IDENTIFIER.to_string(),
        ),
        (
            GROK_BUILD_AUTHENTICATE_RESPONSE_HEADER.to_string(),
            GROK_BUILD_AUTHENTICATE_RESPONSE_VALUE.to_string(),
        ),
    ])
}

fn remove_header_case_insensitive(headers: &mut BTreeMap<String, String>, name: &str) {
    headers.retain(|key, _| !key.eq_ignore_ascii_case(name));
}

/// 固定注入 Grok CLI 身份头。只对 cli-chat-proxy 生效；用户把 base_url 改成
/// api.x.ai 之类的官方 API 时，官方 API 不认这些头，保持不注入。
pub(crate) fn apply_grok_build_client_identity_policy(
    transport: &GatewayProviderTransportSnapshot,
    provider_request_headers: &mut BTreeMap<String, String>,
) -> ProviderOutboundRequestPolicyResult {
    let policy = ProviderOutboundRequestPolicy::GrokBuildClientIdentity;
    if !is_grok_build_provider_transport(transport) {
        return ProviderOutboundRequestPolicyResult::skipped(
            policy,
            ProviderOutboundRequestPolicyReason::ProviderTypeMismatch,
        );
    }
    if !is_cli_chat_proxy_base_url(&transport.endpoint.base_url) {
        return ProviderOutboundRequestPolicyResult::skipped(
            policy,
            ProviderOutboundRequestPolicyReason::Disabled,
        );
    }
    for (name, value) in grok_build_client_headers() {
        remove_header_case_insensitive(provider_request_headers, &name);
        provider_request_headers.insert(name, value);
    }
    ProviderOutboundRequestPolicyResult::applied(
        policy,
        ProviderOutboundRequestMutationScope::Headers,
        ProviderOutboundRequestIdentityScope::Key,
    )
}

/// 供不经过出站策略分派的路径直接使用（如管理端模型测试）。
pub fn apply_grok_build_client_headers(
    transport: &GatewayProviderTransportSnapshot,
    provider_request_headers: &mut BTreeMap<String, String>,
) -> bool {
    apply_grok_build_client_identity_policy(transport, provider_request_headers).was_applied()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::snapshot::{
        GatewayProviderTransportEndpoint, GatewayProviderTransportKey,
        GatewayProviderTransportProvider,
    };

    fn sample_transport(provider_type: &str, base_url: &str) -> GatewayProviderTransportSnapshot {
        GatewayProviderTransportSnapshot {
            provider: GatewayProviderTransportProvider {
                id: "provider-1".to_string(),
                name: "Provider".to_string(),
                provider_type: provider_type.to_string(),
                website: None,
                is_active: true,
                keep_priority_on_conversion: false,
                enable_format_conversion: true,
                concurrent_limit: None,
                max_retries: None,
                proxy: None,
                request_timeout_secs: None,
                stream_first_byte_timeout_secs: None,
                config: None,
            },
            endpoint: GatewayProviderTransportEndpoint {
                id: "endpoint-1".to_string(),
                provider_id: "provider-1".to_string(),
                api_format: "openai:responses".to_string(),
                api_family: None,
                endpoint_kind: None,
                is_active: true,
                base_url: base_url.to_string(),
                header_rules: None,
                body_rules: None,
                max_retries: None,
                custom_path: None,
                config: None,
                format_acceptance_config: None,
                proxy: None,
            },
            key: GatewayProviderTransportKey {
                id: "key-1".to_string(),
                provider_id: "provider-1".to_string(),
                name: "Key".to_string(),
                auth_type: "oauth".to_string(),
                is_active: true,
                api_formats: None,
                auth_type_by_format: None,
                allow_auth_channel_mismatch_formats: None,
                allowed_models: None,
                capabilities: None,
                rate_multipliers: None,
                global_priority_by_format: None,
                expires_at_unix_secs: None,
                proxy: None,
                fingerprint: None,
                upstream_metadata: None,
                decrypted_api_key: "at".to_string(),
                decrypted_auth_config: Some(r#"{"provider_type":"grok_build"}"#.to_string()),
            },
        }
    }

    #[test]
    fn injects_cli_identity_headers_for_chat_proxy() {
        let transport = sample_transport("grok_build", GROK_BUILD_DEFAULT_BASE_URL);
        let mut headers = BTreeMap::from([
            ("User-Agent".to_string(), "client/1.0".to_string()),
            ("authorization".to_string(), "Bearer at".to_string()),
        ]);
        let result = apply_grok_build_client_identity_policy(&transport, &mut headers);
        assert!(result.was_applied());
        assert_eq!(
            headers.get("authorization").map(String::as_str),
            Some("Bearer at")
        );
        assert_eq!(
            headers.get("user-agent").map(String::as_str),
            Some("xai-grok-workspace/0.2.120")
        );
        assert!(!headers.contains_key("User-Agent"));
        assert_eq!(
            headers
                .get(GROK_BUILD_TOKEN_AUTH_HEADER)
                .map(String::as_str),
            Some(GROK_BUILD_TOKEN_AUTH_VALUE)
        );
        assert_eq!(
            headers
                .get(GROK_BUILD_CLIENT_IDENTIFIER_HEADER)
                .map(String::as_str),
            Some(GROK_BUILD_CLIENT_IDENTIFIER)
        );
    }

    #[test]
    fn skips_official_api_base_url_and_other_provider_types() {
        let mut headers = BTreeMap::new();
        let official = sample_transport("grok_build", "https://api.x.ai/v1");
        let result = apply_grok_build_client_identity_policy(&official, &mut headers);
        assert!(!result.was_applied());
        assert_eq!(result.reason, ProviderOutboundRequestPolicyReason::Disabled);
        assert!(headers.is_empty());

        let other = sample_transport("grok", GROK_BUILD_DEFAULT_BASE_URL);
        let result = apply_grok_build_client_identity_policy(&other, &mut headers);
        assert_eq!(
            result.reason,
            ProviderOutboundRequestPolicyReason::ProviderTypeMismatch
        );
        assert!(headers.is_empty());
    }
}
