//! Grok Build（xAI Grok CLI）OAuth 适配器。
//!
//! 走 RFC 8628 设备码流程：向 auth.x.ai 申请 device_code，用户在浏览器输入
//! user_code 授权，网关轮询 token 端点换取 access_token / refresh_token。
//! 刷新与 Bearer 鉴权直接复用通用适配器（表单 POST、public client、无 secret），
//! 本模块只补设备码两步和 xAI 特有的身份字段。

use crate::core::{redacted_oauth_error_body_excerpt, OAuthDeviceAuthorization, OAuthError};
use crate::network::{OAuthHttpExecutor, OAuthHttpRequest};
use crate::provider::{
    ProviderOAuthAccount, ProviderOAuthAdapter, ProviderOAuthCapabilities,
    ProviderOAuthImportInput, ProviderOAuthProbeResult, ProviderOAuthRequestAuth,
    ProviderOAuthTokenSet, ProviderOAuthTransportContext,
};
use async_trait::async_trait;
use serde_json::Value;
use std::collections::BTreeMap;
use url::form_urlencoded;

use super::generic::{
    provider_account_state_from_metadata, template_for_provider_type, GenericProviderOAuthAdapter,
};

pub const GROK_BUILD_PROVIDER_TYPE: &str = "grok_build";
pub const GROK_BUILD_ISSUER: &str = "https://auth.x.ai";
pub const GROK_BUILD_DEVICE_AUTHORIZATION_URL: &str = "https://auth.x.ai/oauth2/device/code";
pub const GROK_BUILD_TOKEN_URL: &str = "https://auth.x.ai/oauth2/token";
/// xAI 公开的 Grok CLI OAuth client_id（public client，无 secret）。
pub const GROK_BUILD_CLIENT_ID: &str = "b1a00492-073a-47ea-816f-4c329264a828";
pub const GROK_BUILD_OAUTH_SCOPES: &[&str] = &[
    "openid",
    "profile",
    "email",
    "offline_access",
    "grok-cli:access",
    "api:access",
];
pub const GROK_BUILD_DEVICE_CODE_GRANT_TYPE: &str = "urn:ietf:params:oauth:grant-type:device_code";
const DEFAULT_DEVICE_EXPIRES_IN_SECS: u64 = 600;
const DEFAULT_DEVICE_POLL_INTERVAL_SECS: u64 = 5;

/// 设备码轮询的一次非成功结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GrokBuildDevicePollOutcome {
    Pending,
    SlowDown,
    Expired,
    AccessDenied,
    Failed(String),
}

#[derive(Debug, Clone)]
pub struct GrokBuildProviderOAuthAdapter {
    inner: GenericProviderOAuthAdapter,
    device_authorization_url_override: Option<String>,
}

impl Default for GrokBuildProviderOAuthAdapter {
    fn default() -> Self {
        Self {
            inner: GenericProviderOAuthAdapter::new(
                template_for_provider_type(GROK_BUILD_PROVIDER_TYPE)
                    .expect("grok_build template should exist"),
            ),
            device_authorization_url_override: None,
        }
    }
}

impl GrokBuildProviderOAuthAdapter {
    pub fn with_endpoint_overrides(
        mut self,
        device_authorization_url: Option<String>,
        token_url: Option<String>,
    ) -> Self {
        self.device_authorization_url_override = device_authorization_url
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty());
        if let Some(token_url) = token_url
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
        {
            self.inner = self.inner.with_token_url_override(token_url);
        }
        self
    }

    fn device_authorization_url(&self) -> &str {
        self.device_authorization_url_override
            .as_deref()
            .unwrap_or(GROK_BUILD_DEVICE_AUTHORIZATION_URL)
    }

    /// 向 xAI 申请设备码。
    pub async fn start_device_authorization(
        &self,
        executor: &dyn OAuthHttpExecutor,
        ctx: &ProviderOAuthTransportContext,
    ) -> Result<OAuthDeviceAuthorization, OAuthError> {
        let form = form_urlencoded::Serializer::new(String::new())
            .append_pair("client_id", GROK_BUILD_CLIENT_ID)
            .append_pair("scope", &GROK_BUILD_OAUTH_SCOPES.join(" "))
            .finish();
        let response = executor
            .execute(OAuthHttpRequest {
                request_id: "provider-oauth:grok-build-device-authorize".to_string(),
                method: reqwest::Method::POST,
                url: self.device_authorization_url().to_string(),
                headers: form_headers(),
                content_type: Some("application/x-www-form-urlencoded".to_string()),
                json_body: None,
                body_bytes: Some(form.into_bytes()),
                network: ctx.network.clone(),
                transport_profile: None,
            })
            .await?;
        if !(200..300).contains(&response.status_code) {
            return Err(OAuthError::HttpStatus {
                status_code: response.status_code,
                body_excerpt: redacted_oauth_error_body_excerpt(&response.body_text),
            });
        }
        let payload = response
            .json_body
            .or_else(|| serde_json::from_str::<Value>(&response.body_text).ok())
            .ok_or_else(|| {
                OAuthError::invalid_response("grok build device authorization is not json")
            })?;
        let device_code = non_empty_string(payload.get("device_code")).ok_or_else(|| {
            OAuthError::invalid_response("grok build device authorization missing device_code")
        })?;
        let user_code = non_empty_string(payload.get("user_code")).ok_or_else(|| {
            OAuthError::invalid_response("grok build device authorization missing user_code")
        })?;
        let verification_uri =
            non_empty_string(payload.get("verification_uri")).ok_or_else(|| {
                OAuthError::invalid_response(
                    "grok build device authorization missing verification_uri",
                )
            })?;
        let verification_uri_complete = non_empty_string(payload.get("verification_uri_complete"))
            .unwrap_or_else(|| verification_uri.clone());
        Ok(OAuthDeviceAuthorization {
            device_code,
            user_code,
            verification_uri,
            verification_uri_complete,
            expires_in: payload
                .get("expires_in")
                .and_then(Value::as_u64)
                .filter(|value| *value > 0)
                .unwrap_or(DEFAULT_DEVICE_EXPIRES_IN_SECS),
            interval: payload
                .get("interval")
                .and_then(Value::as_u64)
                .filter(|value| *value > 0)
                .unwrap_or(DEFAULT_DEVICE_POLL_INTERVAL_SECS),
        })
    }

    /// 用 device_code 轮询 token 端点。`Ok(Err(outcome))` 表示尚未完成或终态失败，
    /// 只有 `Ok(Ok(token_set))` 才拿到了凭据。
    pub async fn poll_device_token(
        &self,
        executor: &dyn OAuthHttpExecutor,
        ctx: &ProviderOAuthTransportContext,
        device_code: &str,
    ) -> Result<Result<ProviderOAuthTokenSet, GrokBuildDevicePollOutcome>, OAuthError> {
        let form = form_urlencoded::Serializer::new(String::new())
            .append_pair("grant_type", GROK_BUILD_DEVICE_CODE_GRANT_TYPE)
            .append_pair("device_code", device_code.trim())
            .append_pair("client_id", GROK_BUILD_CLIENT_ID)
            .finish();
        let response = executor
            .execute(OAuthHttpRequest {
                request_id: "provider-oauth:grok-build-device-poll".to_string(),
                method: reqwest::Method::POST,
                url: self.inner.resolved_token_url(),
                headers: form_headers(),
                content_type: Some("application/x-www-form-urlencoded".to_string()),
                json_body: None,
                body_bytes: Some(form.into_bytes()),
                network: ctx.network.clone(),
                transport_profile: None,
            })
            .await?;
        let payload = response
            .json_body
            .clone()
            .or_else(|| serde_json::from_str::<Value>(&response.body_text).ok());
        if let Some(error) = payload
            .as_ref()
            .and_then(|value| value.get("error"))
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
        {
            return Ok(Err(match error {
                "authorization_pending" => GrokBuildDevicePollOutcome::Pending,
                "slow_down" => GrokBuildDevicePollOutcome::SlowDown,
                "expired_token" => GrokBuildDevicePollOutcome::Expired,
                "access_denied" => GrokBuildDevicePollOutcome::AccessDenied,
                other => GrokBuildDevicePollOutcome::Failed(sanitize_error_code(other)),
            }));
        }
        if !(200..300).contains(&response.status_code) {
            return Err(OAuthError::HttpStatus {
                status_code: response.status_code,
                body_excerpt: redacted_oauth_error_body_excerpt(&response.body_text),
            });
        }
        let payload = payload
            .ok_or_else(|| OAuthError::invalid_response("grok build token response is not json"))?;
        Ok(Ok(self.inner.provider_token_set_from_payload(payload)?))
    }
}

#[async_trait]
impl ProviderOAuthAdapter for GrokBuildProviderOAuthAdapter {
    fn provider_type(&self) -> &'static str {
        GROK_BUILD_PROVIDER_TYPE
    }

    fn capabilities(&self) -> ProviderOAuthCapabilities {
        ProviderOAuthCapabilities {
            supports_authorization_code: false,
            supports_cookie_authorization: false,
            supports_refresh_token_import: true,
            supports_batch_import: true,
            supports_device_flow: true,
            supports_account_probe: true,
            rotates_refresh_token: true,
        }
    }

    async fn import_credentials(
        &self,
        executor: &dyn OAuthHttpExecutor,
        ctx: &ProviderOAuthTransportContext,
        mut input: ProviderOAuthImportInput,
    ) -> Result<ProviderOAuthTokenSet, OAuthError> {
        if input
            .refresh_token
            .as_deref()
            .map(str::trim)
            .is_none_or(str::is_empty)
        {
            input.refresh_token = input
                .raw_credentials
                .as_ref()
                .and_then(|raw| raw.get("refresh_token").or_else(|| raw.get("refreshToken")))
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(ToOwned::to_owned);
        }
        self.inner.import_credentials(executor, ctx, input).await
    }

    async fn refresh(
        &self,
        executor: &dyn OAuthHttpExecutor,
        ctx: &ProviderOAuthTransportContext,
        account: &ProviderOAuthAccount,
    ) -> Result<ProviderOAuthTokenSet, OAuthError> {
        self.inner.refresh(executor, ctx, account).await
    }

    fn resolve_request_auth(
        &self,
        account: &ProviderOAuthAccount,
    ) -> Result<ProviderOAuthRequestAuth, OAuthError> {
        self.inner.resolve_request_auth(account)
    }

    fn account_fingerprint(&self, account: &ProviderOAuthAccount) -> Option<String> {
        if let Some(user_id) = non_empty_string(account.auth_config.get("user_id")) {
            return Some(format!("grok-build:sub:{user_id}"));
        }
        self.inner.account_fingerprint(account)
    }

    async fn probe_account_state(
        &self,
        _executor: &dyn OAuthHttpExecutor,
        _ctx: &ProviderOAuthTransportContext,
        account: &ProviderOAuthAccount,
    ) -> Result<Option<ProviderOAuthProbeResult>, OAuthError> {
        Ok(Some(provider_account_state_from_metadata(
            GROK_BUILD_PROVIDER_TYPE,
            account,
        )))
    }
}

fn form_headers() -> BTreeMap<String, String> {
    BTreeMap::from([
        (
            "content-type".to_string(),
            "application/x-www-form-urlencoded".to_string(),
        ),
        ("accept".to_string(), "application/json".to_string()),
    ])
}

fn non_empty_string(value: Option<&Value>) -> Option<String> {
    value
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
}

fn sanitize_error_code(value: &str) -> String {
    let sanitized = value
        .chars()
        .filter(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '_' | '-'))
        .take(64)
        .collect::<String>();
    if sanitized.is_empty() {
        "upstream_error".to_string()
    } else {
        sanitized
    }
}

#[cfg(test)]
mod tests {
    use super::{
        GrokBuildDevicePollOutcome, GrokBuildProviderOAuthAdapter, GROK_BUILD_CLIENT_ID,
        GROK_BUILD_DEVICE_CODE_GRANT_TYPE,
    };
    use crate::network::{OAuthHttpExecutor, OAuthHttpRequest, OAuthHttpResponse};
    use crate::provider::{
        ProviderOAuthAccount, ProviderOAuthAdapter, ProviderOAuthImportInput,
        ProviderOAuthTransportContext,
    };
    use async_trait::async_trait;
    use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
    use serde_json::{json, Value};
    use std::collections::BTreeMap;
    use std::sync::Mutex;

    struct RecordingExecutor {
        response: OAuthHttpResponse,
        requests: Mutex<Vec<OAuthHttpRequest>>,
    }

    impl RecordingExecutor {
        fn new(status_code: u16, body: Value) -> Self {
            Self {
                response: OAuthHttpResponse {
                    status_code,
                    body_text: body.to_string(),
                    json_body: Some(body),
                },
                requests: Mutex::new(Vec::new()),
            }
        }

        fn last_request(&self) -> (String, BTreeMap<String, String>) {
            let requests = self.requests.lock().expect("lock");
            let request = requests.last().expect("request recorded");
            let body = request.body_bytes.clone().expect("form body");
            let form = url::form_urlencoded::parse(&body)
                .map(|(key, value)| (key.to_string(), value.to_string()))
                .collect();
            (request.url.clone(), form)
        }
    }

    #[async_trait]
    impl OAuthHttpExecutor for RecordingExecutor {
        async fn execute(
            &self,
            request: OAuthHttpRequest,
        ) -> Result<OAuthHttpResponse, crate::core::OAuthError> {
            self.requests.lock().expect("lock").push(request);
            Ok(OAuthHttpResponse {
                status_code: self.response.status_code,
                body_text: self.response.body_text.clone(),
                json_body: self.response.json_body.clone(),
            })
        }
    }

    fn ctx() -> ProviderOAuthTransportContext {
        ProviderOAuthTransportContext {
            provider_id: "provider-1".to_string(),
            provider_type: "grok_build".to_string(),
            endpoint_id: None,
            key_id: None,
            auth_type: Some("oauth".to_string()),
            decrypted_api_key: None,
            decrypted_auth_config: None,
            provider_config: None,
            endpoint_config: None,
            key_config: None,
            network: crate::network::OAuthNetworkContext::provider_operation(None),
        }
    }

    fn jwt_with_claims(claims: Value) -> String {
        let header = URL_SAFE_NO_PAD.encode(br#"{"alg":"none"}"#);
        let payload = URL_SAFE_NO_PAD.encode(claims.to_string().as_bytes());
        format!("{header}.{payload}.sig")
    }

    #[tokio::test]
    async fn device_authorization_sends_client_id_and_scope() {
        let executor = RecordingExecutor::new(
            200,
            json!({
                "device_code": "dc-1",
                "user_code": "ABCD-EFGH",
                "verification_uri": "https://auth.x.ai/device",
                "verification_uri_complete": "https://auth.x.ai/device?user_code=ABCD-EFGH",
                "expires_in": 900,
                "interval": 7
            }),
        );
        let adapter = GrokBuildProviderOAuthAdapter::default()
            .with_endpoint_overrides(Some("https://idp.test/device".to_string()), None);
        let authorization = adapter
            .start_device_authorization(&executor, &ctx())
            .await
            .expect("device authorization");

        assert_eq!(authorization.device_code, "dc-1");
        assert_eq!(authorization.user_code, "ABCD-EFGH");
        assert_eq!(authorization.expires_in, 900);
        assert_eq!(authorization.interval, 7);
        let (url, form) = executor.last_request();
        assert_eq!(url, "https://idp.test/device");
        assert_eq!(
            form.get("client_id").map(String::as_str),
            Some(GROK_BUILD_CLIENT_ID)
        );
        assert!(
            form.get("scope").is_some_and(
                |scope| scope.contains("grok-cli:access") && scope.contains("api:access")
            )
        );
    }

    #[tokio::test]
    async fn device_poll_maps_pending_and_terminal_errors() {
        let adapter = GrokBuildProviderOAuthAdapter::default()
            .with_endpoint_overrides(None, Some("https://idp.test/token".to_string()));
        for (error, expected) in [
            ("authorization_pending", GrokBuildDevicePollOutcome::Pending),
            ("slow_down", GrokBuildDevicePollOutcome::SlowDown),
            ("expired_token", GrokBuildDevicePollOutcome::Expired),
            ("access_denied", GrokBuildDevicePollOutcome::AccessDenied),
            (
                "weird <script>",
                GrokBuildDevicePollOutcome::Failed("weirdscript".to_string()),
            ),
        ] {
            let executor = RecordingExecutor::new(400, json!({"error": error}));
            let outcome = adapter
                .poll_device_token(&executor, &ctx(), "dc-1")
                .await
                .expect("poll should not error");
            assert_eq!(outcome, Err(expected), "error={error}");
            let (url, form) = executor.last_request();
            assert_eq!(url, "https://idp.test/token");
            assert_eq!(
                form.get("grant_type").map(String::as_str),
                Some(GROK_BUILD_DEVICE_CODE_GRANT_TYPE)
            );
            assert_eq!(form.get("device_code").map(String::as_str), Some("dc-1"));
        }
    }

    #[tokio::test]
    async fn device_poll_success_extracts_identity_from_id_token() {
        let id_token = jwt_with_claims(json!({"email": "grok@example.com", "sub": "user-42"}));
        let executor = RecordingExecutor::new(
            200,
            json!({
                "access_token": "at-1",
                "refresh_token": "rt-1",
                "id_token": id_token,
                "token_type": "Bearer",
                "expires_in": 1800
            }),
        );
        let adapter = GrokBuildProviderOAuthAdapter::default();
        let result = adapter
            .poll_device_token(&executor, &ctx(), "dc-1")
            .await
            .expect("poll")
            .expect("authorized");

        assert_eq!(result.token_set.access_token, "at-1");
        assert_eq!(result.token_set.refresh_token.as_deref(), Some("rt-1"));
        assert!(result.token_set.expires_at_unix_secs.is_some());
        assert_eq!(result.auth_config["provider_type"], "grok_build");
        assert_eq!(result.auth_config["auth_method"], "device_code");
        assert_eq!(result.auth_config["email"], "grok@example.com");
        assert_eq!(result.auth_config["user_id"], "user-42");
        assert_eq!(result.auth_config["refresh_token"], "rt-1");
        assert!(result.auth_config.get("id_token").is_none());
    }

    #[tokio::test]
    async fn refresh_preserves_refresh_token_and_identity_when_not_rotated() {
        let executor = RecordingExecutor::new(
            200,
            json!({"access_token": "at-2", "token_type": "Bearer", "expires_in": 1800}),
        );
        let adapter = GrokBuildProviderOAuthAdapter::default();
        let account = ProviderOAuthAccount {
            provider_type: "grok_build".to_string(),
            access_token: "at-1".to_string(),
            auth_config: json!({
                "provider_type": "grok_build",
                "refresh_token": "rt-1",
                "email": "grok@example.com",
                "user_id": "user-42"
            }),
            expires_at_unix_secs: Some(1),
            identity: BTreeMap::new(),
        };
        let result = adapter
            .refresh(&executor, &ctx(), &account)
            .await
            .expect("refresh");

        assert_eq!(result.token_set.access_token, "at-2");
        assert_eq!(result.token_set.refresh_token.as_deref(), Some("rt-1"));
        assert_eq!(result.auth_config["refresh_token"], "rt-1");
        assert_eq!(result.auth_config["email"], "grok@example.com");
        assert_eq!(result.auth_config["user_id"], "user-42");
        let (_, form) = executor.last_request();
        assert_eq!(
            form.get("grant_type").map(String::as_str),
            Some("refresh_token")
        );
        assert_eq!(form.get("refresh_token").map(String::as_str), Some("rt-1"));
        assert_eq!(
            form.get("client_id").map(String::as_str),
            Some(GROK_BUILD_CLIENT_ID)
        );
        assert!(!form.contains_key("client_secret"));
        assert!(!form.contains_key("scope"));
    }

    #[tokio::test]
    async fn import_reads_refresh_token_from_raw_credentials() {
        let executor = RecordingExecutor::new(
            200,
            json!({"access_token": "at", "refresh_token": "rt-raw", "expires_in": 60}),
        );
        let adapter = GrokBuildProviderOAuthAdapter::default();
        let result = adapter
            .import_credentials(
                &executor,
                &ctx(),
                ProviderOAuthImportInput {
                    provider_type: "grok_build".to_string(),
                    name: None,
                    refresh_token: None,
                    raw_credentials: Some(json!({"refreshToken": "rt-raw"})),
                    network: crate::network::OAuthNetworkContext::provider_operation(None),
                },
            )
            .await
            .expect("import");
        assert_eq!(result.token_set.access_token, "at");
        let (_, form) = executor.last_request();
        assert_eq!(
            form.get("refresh_token").map(String::as_str),
            Some("rt-raw")
        );
    }

    #[test]
    fn fingerprint_prefers_subject_over_secret() {
        let adapter = GrokBuildProviderOAuthAdapter::default();
        let account = ProviderOAuthAccount {
            provider_type: "grok_build".to_string(),
            access_token: "at".to_string(),
            auth_config: json!({"user_id": "user-42", "refresh_token": "rt"}),
            expires_at_unix_secs: None,
            identity: BTreeMap::new(),
        };
        assert_eq!(
            adapter.account_fingerprint(&account).as_deref(),
            Some("grok-build:sub:user-42")
        );
    }
}
