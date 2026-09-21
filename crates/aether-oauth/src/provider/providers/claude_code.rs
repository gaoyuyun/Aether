use super::generic::{template_for_provider_type, GenericProviderOAuthAdapter};
use crate::core::{
    generate_oauth_nonce, generate_pkce_verifier, pkce_s256, OAuthAuthorizeResponse,
};
use crate::network::{OAuthHttpExecutor, OAuthHttpRequest};
use crate::provider::{
    ProviderOAuthAccount, ProviderOAuthAdapter, ProviderOAuthCapabilities,
    ProviderOAuthCookieAuthorizationInput, ProviderOAuthImportInput, ProviderOAuthProbeResult,
    ProviderOAuthRequestAuth, ProviderOAuthTokenSet, ProviderOAuthTransportContext,
};
use crate::OAuthError;
use aether_contracts::{
    ResolvedTransportProfile, EXECUTION_REQUEST_FOLLOW_REDIRECTS_HEADER,
    TRANSPORT_BACKEND_BROWSER_WREQ, TRANSPORT_HTTP_MODE_AUTO, TRANSPORT_POOL_SCOPE_KEY,
};
use async_trait::async_trait;
use serde_json::{json, Value};
use std::collections::BTreeMap;
use url::Url;

pub const CLAUDE_CODE_PROVIDER_TYPE: &str = "claude_code";
pub const CLAUDE_CODE_CLIENT_ID: &str = "9d1c250a-e61b-44d9-88ed-5944d1962f5e";
pub const CLAUDE_CODE_WEB_BASE_URL: &str = "https://claude.ai";
pub const CLAUDE_CODE_AUTHORIZE_URL: &str = "https://claude.ai/oauth/authorize";
pub const CLAUDE_CODE_TOKEN_URL: &str = "https://platform.claude.com/v1/oauth/token";
pub const CLAUDE_CODE_REDIRECT_URI: &str = "https://platform.claude.com/oauth/code/callback";
pub const CLAUDE_CODE_OAUTH_SCOPES: &[&str] = &[
    "org:create_api_key",
    "user:profile",
    "user:inference",
    "user:sessions:claude_code",
    "user:mcp_servers",
    "user:file_upload",
];
pub const CLAUDE_CODE_COOKIE_SCOPE: &str =
    "user:profile user:inference user:sessions:claude_code user:mcp_servers user:file_upload";

const CLAUDE_CODE_BROWSER_USER_AGENT: &str = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/136.0.0.0 Safari/537.36";
/// 原生客户端在 token 交换成功后约 500ms 内发起的两个控制面请求。
pub const CLAUDE_CODE_PROFILE_URL: &str = "https://api.anthropic.com/api/oauth/profile";
pub const CLAUDE_CODE_ROLES_URL: &str = "https://api.anthropic.com/api/oauth/claude_cli/roles";
/// 刷新遇到 5xx 时的最多重试次数（不含首次）。
pub const CLAUDE_CODE_REFRESH_MAX_RETRIES: usize = 3;
const CLAUDE_CODE_OAUTH_CONTROL_PLANE_USER_AGENT: &str = "axios/1.13.6";

#[derive(Debug, Clone)]
pub struct ClaudeCodeProviderOAuthAdapter {
    inner: GenericProviderOAuthAdapter,
    web_base_url: String,
    profile_url: String,
    roles_url: String,
}

fn claude_code_token_request_transport_profile(
    ctx: &ProviderOAuthTransportContext,
) -> Option<ResolvedTransportProfile> {
    claude_code_control_plane_transport_profile_for_context(
        ctx,
        CLAUDE_CODE_CONTROL_PLANE_REQUEST_KIND_REFRESH,
    )
}

impl Default for ClaudeCodeProviderOAuthAdapter {
    fn default() -> Self {
        Self {
            inner: GenericProviderOAuthAdapter::new(
                template_for_provider_type(CLAUDE_CODE_PROVIDER_TYPE)
                    .expect("claude code oauth template should exist"),
            )
            .with_token_transport_profile_resolver(claude_code_token_request_transport_profile),
            web_base_url: CLAUDE_CODE_WEB_BASE_URL.to_string(),
            profile_url: CLAUDE_CODE_PROFILE_URL.to_string(),
            roles_url: CLAUDE_CODE_ROLES_URL.to_string(),
        }
    }
}

impl ClaudeCodeProviderOAuthAdapter {
    pub fn with_endpoint_overrides(
        mut self,
        web_base_url: impl Into<String>,
        token_url: impl Into<String>,
    ) -> Self {
        self.web_base_url = web_base_url.into();
        self.inner = self.inner.with_token_url_override(token_url);
        self
    }

    /// 覆盖 profile / roles 控制面地址（测试与私有部署用）。
    pub fn with_control_plane_overrides(
        mut self,
        profile_url: impl Into<String>,
        roles_url: impl Into<String>,
    ) -> Self {
        self.profile_url = profile_url.into();
        self.roles_url = roles_url.into();
        self
    }

    /// 交换/刷新成功后补齐账号信息：`/api/oauth/profile` 给 org/account，
    /// `claude_cli/roles` 只记录原始 JSON。两者都是尽力而为，失败不影响令牌。
    async fn enrich_token_set_with_account_profile(
        &self,
        executor: &dyn OAuthHttpExecutor,
        ctx: &ProviderOAuthTransportContext,
        token_set: &mut ProviderOAuthTokenSet,
    ) {
        let access_token = token_set.token_set.access_token.trim().to_string();
        if access_token.is_empty() {
            return;
        }
        let Some(auth_config) = token_set.auth_config.as_object_mut() else {
            return;
        };
        if let Some(profile) = self
            .fetch_control_plane_json(executor, ctx, &self.profile_url, &access_token, "profile")
            .await
        {
            apply_claude_code_oauth_profile(auth_config, &profile);
        }
        if let Some(roles) = self
            .fetch_control_plane_json(
                executor,
                ctx,
                &self.roles_url,
                &access_token,
                "claude_cli roles",
            )
            .await
        {
            auth_config.insert("claude_cli_roles".to_string(), roles);
        }
    }

    async fn fetch_control_plane_json(
        &self,
        executor: &dyn OAuthHttpExecutor,
        ctx: &ProviderOAuthTransportContext,
        url: &str,
        access_token: &str,
        label: &str,
    ) -> Option<Value> {
        let headers = BTreeMap::from([
            (
                "accept".to_string(),
                "application/json, text/plain, */*".to_string(),
            ),
            (
                "authorization".to_string(),
                format!("Bearer {access_token}"),
            ),
            ("cache-control".to_string(), "no-cache".to_string()),
            (
                "user-agent".to_string(),
                CLAUDE_CODE_OAUTH_CONTROL_PLANE_USER_AGENT.to_string(),
            ),
        ]);
        let response = executor
            .execute(OAuthHttpRequest {
                request_id: format!("provider-oauth:claude-{}", label.replace(' ', "-")),
                method: reqwest::Method::GET,
                url: url.to_string(),
                headers,
                content_type: None,
                json_body: None,
                body_bytes: None,
                network: ctx.network.clone(),
                transport_profile: claude_code_control_plane_transport_profile_for_context(
                    ctx,
                    CLAUDE_CODE_CONTROL_PLANE_REQUEST_KIND_INSPECT,
                ),
            })
            .await;
        match response {
            Ok(response) if (200..300).contains(&response.status_code) => response
                .json_body
                .or_else(|| serde_json::from_str::<Value>(&response.body_text).ok()),
            Ok(response) => {
                tracing::warn!(
                    status_code = response.status_code,
                    label,
                    "claude oauth control-plane lookup returned non-2xx; continuing without it"
                );
                None
            }
            Err(error) => {
                tracing::warn!(
                    error = %error,
                    label,
                    "claude oauth control-plane lookup failed; continuing without it"
                );
                None
            }
        }
    }

    fn web_url(&self, path_segments: &[&str]) -> Result<String, OAuthError> {
        let mut url = Url::parse(self.web_base_url.trim())
            .map_err(|_| OAuthError::invalid_request("claude web base url must be absolute"))?;
        url.set_query(None);
        url.set_fragment(None);
        {
            let mut segments = url
                .path_segments_mut()
                .map_err(|_| OAuthError::invalid_request("claude web base url is invalid"))?;
            segments.clear();
            segments.extend(path_segments.iter().copied());
        }
        Ok(url.to_string())
    }

    fn session_cookie(session_key: &str) -> Result<String, OAuthError> {
        let session_key = session_key.trim();
        if session_key.is_empty()
            || session_key.contains(['\r', '\n', ';'])
            || http::HeaderValue::from_str(session_key).is_err()
        {
            return Err(OAuthError::invalid_request("invalid Claude sessionKey"));
        }
        Ok(format!("sessionKey={session_key}"))
    }

    async fn organization_uuid(
        &self,
        executor: &dyn OAuthHttpExecutor,
        ctx: &ProviderOAuthTransportContext,
        cookie: &str,
    ) -> Result<String, OAuthError> {
        let response = executor
            .execute(OAuthHttpRequest {
                request_id: "provider-oauth:claude-cookie-organizations".to_string(),
                method: reqwest::Method::GET,
                url: self.web_url(&["api", "organizations"])?,
                headers: cookie_headers(cookie, false),
                content_type: None,
                json_body: None,
                body_bytes: None,
                network: ctx.network.clone(),
                transport_profile: claude_code_oauth_transport_profile_for_context(ctx),
            })
            .await?;
        ensure_success(&response)?;
        let organizations = response
            .json_body
            .or_else(|| serde_json::from_str::<Value>(&response.body_text).ok())
            .and_then(|value| value.as_array().cloned())
            .ok_or_else(|| {
                OAuthError::invalid_response("Claude organizations response is invalid")
            })?;

        let organization = if organizations.len() == 1 {
            organizations.first()
        } else {
            organizations
                .iter()
                .find(|organization| {
                    organization
                        .get("raven_type")
                        .and_then(Value::as_str)
                        .is_some_and(|value| value.eq_ignore_ascii_case("team"))
                })
                .or_else(|| organizations.first())
        }
        .ok_or_else(|| OAuthError::invalid_response("Claude account has no organizations"))?;

        organization
            .get("uuid")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(ToOwned::to_owned)
            .ok_or_else(|| OAuthError::invalid_response("Claude organization is missing uuid"))
    }

    async fn authorization_code(
        &self,
        executor: &dyn OAuthHttpExecutor,
        ctx: &ProviderOAuthTransportContext,
        cookie: &str,
        organization_uuid: &str,
        state: &str,
        code_challenge: &str,
    ) -> Result<String, OAuthError> {
        let response = executor
            .execute(OAuthHttpRequest {
                request_id: "provider-oauth:claude-cookie-authorize".to_string(),
                method: reqwest::Method::POST,
                url: self.web_url(&["v1", "oauth", organization_uuid, "authorize"])?,
                headers: cookie_headers(cookie, true),
                content_type: Some("application/json".to_string()),
                json_body: Some(json!({
                    "response_type": "code",
                    "client_id": CLAUDE_CODE_CLIENT_ID,
                    "organization_uuid": organization_uuid,
                    "redirect_uri": CLAUDE_CODE_REDIRECT_URI,
                    "scope": CLAUDE_CODE_COOKIE_SCOPE,
                    "state": state,
                    "code_challenge": code_challenge,
                    "code_challenge_method": "S256",
                })),
                body_bytes: None,
                network: ctx.network.clone(),
                transport_profile: claude_code_oauth_transport_profile_for_context(ctx),
            })
            .await?;
        ensure_success(&response)?;
        let payload = response
            .json_body
            .or_else(|| serde_json::from_str::<Value>(&response.body_text).ok())
            .ok_or_else(|| OAuthError::invalid_response("Claude authorize response is invalid"))?;
        let redirect_uri = payload
            .get("redirect_uri")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(ToOwned::to_owned)
            .ok_or_else(|| {
                OAuthError::invalid_response("Claude authorize response is missing redirect_uri")
            })?;

        validate_authorization_redirect(&redirect_uri, state)
    }
}

#[async_trait]
impl ProviderOAuthAdapter for ClaudeCodeProviderOAuthAdapter {
    fn provider_type(&self) -> &'static str {
        CLAUDE_CODE_PROVIDER_TYPE
    }

    fn capabilities(&self) -> ProviderOAuthCapabilities {
        ProviderOAuthCapabilities {
            supports_cookie_authorization: true,
            ..self.inner.capabilities()
        }
    }

    fn build_authorize_url(
        &self,
        ctx: &ProviderOAuthTransportContext,
        state: &str,
        code_challenge: Option<&str>,
    ) -> Result<OAuthAuthorizeResponse, OAuthError> {
        let mut response = self.inner.build_authorize_url(ctx, state, code_challenge)?;
        let mut url = Url::parse(&response.authorize_url)
            .map_err(|_| OAuthError::invalid_response("invalid Claude authorize_url"))?;
        url.query_pairs_mut().append_pair("code", "true");
        response.authorize_url = url.to_string();
        Ok(response)
    }

    async fn exchange_code(
        &self,
        executor: &dyn OAuthHttpExecutor,
        ctx: &ProviderOAuthTransportContext,
        code: &str,
        state: &str,
        pkce_verifier: Option<&str>,
    ) -> Result<ProviderOAuthTokenSet, OAuthError> {
        let mut token_set = self
            .inner
            .exchange_code(executor, ctx, code, state, pkce_verifier)
            .await?;
        self.enrich_token_set_with_account_profile(executor, ctx, &mut token_set)
            .await;
        Ok(token_set)
    }

    async fn authorize_with_cookie(
        &self,
        executor: &dyn OAuthHttpExecutor,
        ctx: &ProviderOAuthTransportContext,
        input: ProviderOAuthCookieAuthorizationInput,
    ) -> Result<ProviderOAuthTokenSet, OAuthError> {
        let cookie = Self::session_cookie(&input.session_key)?;
        let organization_uuid = self.organization_uuid(executor, ctx, &cookie).await?;
        let state = generate_oauth_nonce();
        let verifier = generate_pkce_verifier();
        let challenge = pkce_s256(&verifier);
        let code = self
            .authorization_code(
                executor,
                ctx,
                &cookie,
                &organization_uuid,
                &state,
                &challenge,
            )
            .await?;
        let mut token_set = self
            .inner
            .exchange_code(executor, ctx, &code, &state, Some(&verifier))
            .await?;
        self.enrich_token_set_with_account_profile(executor, ctx, &mut token_set)
            .await;
        Ok(token_set)
    }

    async fn import_credentials(
        &self,
        executor: &dyn OAuthHttpExecutor,
        ctx: &ProviderOAuthTransportContext,
        input: ProviderOAuthImportInput,
    ) -> Result<ProviderOAuthTokenSet, OAuthError> {
        self.inner.import_credentials(executor, ctx, input).await
    }

    async fn refresh(
        &self,
        executor: &dyn OAuthHttpExecutor,
        ctx: &ProviderOAuthTransportContext,
        account: &ProviderOAuthAccount,
    ) -> Result<ProviderOAuthTokenSet, OAuthError> {
        let mut attempt = 0usize;
        let mut token_set = loop {
            match self.inner.refresh(executor, ctx, account).await {
                Ok(token_set) => break token_set,
                // 429：交给调用方按 Retry-After 退避（P1 的重试提示），不在这里空转。
                Err(error @ OAuthError::RateLimited { .. }) => return Err(error),
                Err(OAuthError::HttpStatus {
                    status_code,
                    body_excerpt,
                }) if status_code >= 500 && attempt < CLAUDE_CODE_REFRESH_MAX_RETRIES => {
                    attempt += 1;
                    tracing::warn!(
                        status_code,
                        attempt,
                        max_retries = CLAUDE_CODE_REFRESH_MAX_RETRIES,
                        "claude oauth refresh returned 5xx; retrying"
                    );
                    let _ = body_excerpt;
                    tokio::time::sleep(claude_code_refresh_retry_delay(attempt)).await;
                }
                Err(error) => return Err(error),
            }
        };
        self.enrich_token_set_with_account_profile(executor, ctx, &mut token_set)
            .await;
        Ok(token_set)
    }

    fn resolve_request_auth(
        &self,
        account: &ProviderOAuthAccount,
    ) -> Result<ProviderOAuthRequestAuth, OAuthError> {
        self.inner.resolve_request_auth(account)
    }

    fn account_fingerprint(&self, account: &ProviderOAuthAccount) -> Option<String> {
        self.inner.account_fingerprint(account)
    }

    async fn probe_account_state(
        &self,
        executor: &dyn OAuthHttpExecutor,
        ctx: &ProviderOAuthTransportContext,
        account: &ProviderOAuthAccount,
    ) -> Result<Option<ProviderOAuthProbeResult>, OAuthError> {
        self.inner.probe_account_state(executor, ctx, account).await
    }
}

fn claude_code_refresh_retry_delay(attempt: usize) -> std::time::Duration {
    // 500ms、1s、2s；测试环境可通过环境变量压到 0。
    if std::env::var("AETHER_OAUTH_RETRY_NO_DELAY").is_ok() {
        return std::time::Duration::ZERO;
    }
    std::time::Duration::from_millis(500u64 << attempt.saturating_sub(1).min(4))
}

/// 把 `/api/oauth/profile` 的响应写进 auth_config：`account.uuid` → `account_uuid`，
/// `account.email` / `email_address` → `email`，`organization.uuid` → `org_uuid`，
/// `organization.name` → `org_name`；已有值不覆盖，避免与 token 响应打架。
pub fn apply_claude_code_oauth_profile(
    auth_config: &mut serde_json::Map<String, Value>,
    profile: &Value,
) {
    if let Some(account) = profile.get("account").and_then(Value::as_object) {
        if let Some(uuid) = account
            .get("uuid")
            .and_then(Value::as_str)
            .filter(|v| !v.trim().is_empty())
        {
            auth_config
                .entry("account_uuid".to_string())
                .or_insert_with(|| Value::String(uuid.to_string()));
        }
        if let Some(email) = account
            .get("email_address")
            .or_else(|| account.get("email"))
            .and_then(Value::as_str)
            .filter(|v| !v.trim().is_empty())
        {
            auth_config
                .entry("email_address".to_string())
                .or_insert_with(|| Value::String(email.to_string()));
            auth_config
                .entry("email".to_string())
                .or_insert_with(|| Value::String(email.to_string()));
        }
        if let Some(name) = account
            .get("full_name")
            .or_else(|| account.get("display_name"))
            .and_then(Value::as_str)
        {
            auth_config
                .entry("account_name".to_string())
                .or_insert_with(|| Value::String(name.to_string()));
        }
    }
    if let Some(organization) = profile.get("organization").and_then(Value::as_object) {
        if let Some(uuid) = organization
            .get("uuid")
            .and_then(Value::as_str)
            .filter(|v| !v.trim().is_empty())
        {
            auth_config
                .entry("org_uuid".to_string())
                .or_insert_with(|| Value::String(uuid.to_string()));
        }
        if let Some(name) = organization.get("name").and_then(Value::as_str) {
            auth_config
                .entry("org_name".to_string())
                .or_insert_with(|| Value::String(name.to_string()));
        }
        if let Some(plan) = organization
            .get("organization_type")
            .or_else(|| organization.get("rate_limit_tier"))
            .and_then(Value::as_str)
        {
            auth_config
                .entry("plan_type".to_string())
                .or_insert_with(|| Value::String(plan.to_string()));
        }
    }
    auth_config.insert("profile".to_string(), profile.clone());
}

/// P5：当供应商 / Key 的 `fingerprint.transport_profile` 显式选了 TLS 仿真 profile 时，
/// 原生 CLI 用 Axios 发出的控制面请求（token 交换 / 刷新、profile、claude_cli roles）
/// 改走 `claude_code_oauth_control_plane`（BoringSSL/wreq，无 ALPN，HTTP/1.1，Axios 头顺序）。
/// 未配置时返回 `None`，保持 reqwest 默认行为。隧道代理无法使用 browser_wreq，同样返回 `None`。
pub const CLAUDE_CODE_OAUTH_CONTROL_PLANE_TLS_PROFILE: &str = "claude_code_oauth_control_plane";
/// 与 transport crate `CLAUDE_CODE_TLS_EMULATION_PROFILE_IDS` 保持一致（aether-oauth 不依赖
/// transport crate，这里镜像一份只做"是否配置了仿真 profile"的判断）。
const CLAUDE_CODE_TLS_EMULATION_PROFILE_IDS: &[&str] = &[
    "claude_code_node_openssl",
    "claude_code_oauth_control_plane",
    "chatgpt_com_chrome",
];
const CLAUDE_CODE_CONTROL_PLANE_REQUEST_KIND_REFRESH: &str = "oauth_refresh";
const CLAUDE_CODE_CONTROL_PLANE_REQUEST_KIND_INSPECT: &str = "oauth_inspect";

fn configured_transport_profile_id(value: Option<&Value>) -> Option<String> {
    let transport_profile = value?
        .get("fingerprint")
        .or(Some(value?))?
        .get("transport_profile")?;
    let id = transport_profile
        .as_str()
        .or_else(|| {
            transport_profile
                .get("profile_id")
                .or_else(|| transport_profile.get("id"))
                .and_then(Value::as_str)
        })?
        .trim();
    (!id.is_empty()).then(|| id.to_ascii_lowercase().replace(['-', ' '], "_"))
}

/// 供应商 `config.fingerprint.transport_profile` 或 Key `key_config.fingerprint.transport_profile`
/// / `key_config.transport_profile` 选了内置 TLS 仿真 profile 时为 `true`。
pub fn claude_code_tls_emulation_configured(ctx: &ProviderOAuthTransportContext) -> bool {
    [ctx.key_config.as_ref(), ctx.provider_config.as_ref()]
        .into_iter()
        .flatten()
        .filter_map(|value| configured_transport_profile_id(Some(value)))
        .any(|id| CLAUDE_CODE_TLS_EMULATION_PROFILE_IDS.contains(&id.as_str()))
}

pub fn claude_code_control_plane_transport_profile_for_context(
    ctx: &ProviderOAuthTransportContext,
    request_kind: &str,
) -> Option<ResolvedTransportProfile> {
    if !claude_code_tls_emulation_configured(ctx) {
        return None;
    }
    if claude_code_context_uses_tunnel_only_proxy(ctx) {
        return None;
    }
    Some(ResolvedTransportProfile {
        profile_id: CLAUDE_CODE_OAUTH_CONTROL_PLANE_TLS_PROFILE.to_string(),
        backend: TRANSPORT_BACKEND_BROWSER_WREQ.to_string(),
        http_mode: TRANSPORT_HTTP_MODE_AUTO.to_string(),
        pool_scope: TRANSPORT_POOL_SCOPE_KEY.to_string(),
        header_fingerprint: None,
        extra: Some(json!({
            "emulation_profile": CLAUDE_CODE_OAUTH_CONTROL_PLANE_TLS_PROFILE,
            "request_kind": request_kind,
        })),
    })
}

fn claude_code_context_uses_tunnel_only_proxy(ctx: &ProviderOAuthTransportContext) -> bool {
    ctx.network.proxy.as_ref().is_some_and(|proxy| {
        if proxy.enabled == Some(false) {
            return false;
        }
        let has_proxy_url = proxy
            .url
            .as_deref()
            .map(str::trim)
            .is_some_and(|value| !value.is_empty());
        let has_node_id = proxy
            .node_id
            .as_deref()
            .map(str::trim)
            .is_some_and(|value| !value.is_empty());
        let tunnel_mode = proxy
            .mode
            .as_deref()
            .map(str::trim)
            .is_some_and(|value| value.eq_ignore_ascii_case("tunnel"));
        has_node_id && (tunnel_mode || !has_proxy_url)
    })
}

pub(super) fn claude_code_oauth_transport_profile() -> ResolvedTransportProfile {
    ResolvedTransportProfile {
        profile_id: "claude_oauth_chrome136".to_string(),
        backend: TRANSPORT_BACKEND_BROWSER_WREQ.to_string(),
        http_mode: TRANSPORT_HTTP_MODE_AUTO.to_string(),
        pool_scope: TRANSPORT_POOL_SCOPE_KEY.to_string(),
        header_fingerprint: None,
        extra: Some(json!({ "browser_profile": "chrome136" })),
    }
}

fn claude_code_oauth_transport_profile_for_context(
    ctx: &ProviderOAuthTransportContext,
) -> Option<ResolvedTransportProfile> {
    // Node-only proxies must execute through the tunnel runtime, which cannot use browser_wreq.
    // The explicit browser headers still keep that fallback compatible with Claude's web flow.
    (!claude_code_context_uses_tunnel_only_proxy(ctx)).then(claude_code_oauth_transport_profile)
}

fn cookie_headers(cookie: &str, json_request: bool) -> BTreeMap<String, String> {
    let mut headers = BTreeMap::from([
        ("accept".to_string(), "application/json".to_string()),
        ("accept-language".to_string(), "en-US,en;q=0.9".to_string()),
        ("cache-control".to_string(), "no-cache".to_string()),
        ("cookie".to_string(), cookie.to_string()),
        (
            "user-agent".to_string(),
            CLAUDE_CODE_BROWSER_USER_AGENT.to_string(),
        ),
        (
            EXECUTION_REQUEST_FOLLOW_REDIRECTS_HEADER.to_string(),
            "false".to_string(),
        ),
    ]);
    if json_request {
        headers.insert("content-type".to_string(), "application/json".to_string());
        headers.insert("origin".to_string(), CLAUDE_CODE_WEB_BASE_URL.to_string());
        headers.insert(
            "referer".to_string(),
            format!("{CLAUDE_CODE_WEB_BASE_URL}/new"),
        );
    }
    headers
}

fn ensure_success(response: &crate::network::OAuthHttpResponse) -> Result<(), OAuthError> {
    if (200..300).contains(&response.status_code) {
        return Ok(());
    }
    Err(OAuthError::HttpStatus {
        status_code: response.status_code,
        body_excerpt: "Claude Cookie authorization request failed".to_string(),
    })
}

fn validate_authorization_redirect(
    redirect_uri: &str,
    expected_state: &str,
) -> Result<String, OAuthError> {
    let redirect = Url::parse(redirect_uri)
        .map_err(|_| OAuthError::invalid_response("Claude authorize redirect_uri is invalid"))?;
    let expected = Url::parse(CLAUDE_CODE_REDIRECT_URI).map_err(|_| {
        OAuthError::invalid_response("Claude redirect URI configuration is invalid")
    })?;
    if redirect.scheme() != expected.scheme()
        || redirect.host_str() != expected.host_str()
        || redirect.port_or_known_default() != expected.port_or_known_default()
        || redirect.path() != expected.path()
        || !redirect.username().is_empty()
        || redirect.password().is_some()
        || redirect.fragment().is_some()
    {
        return Err(OAuthError::invalid_response(
            "Claude authorize redirect_uri target is invalid",
        ));
    }

    let mut code = None;
    let mut state = None;
    for (key, value) in redirect.query_pairs() {
        match key.as_ref() {
            "code" if code.is_none() => code = Some(value.into_owned()),
            "state" if state.is_none() => state = Some(value.into_owned()),
            "code" | "state" => {
                return Err(OAuthError::invalid_response(
                    "Claude authorize redirect_uri has duplicate parameters",
                ));
            }
            _ => {}
        }
    }
    let code = code
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .ok_or_else(|| {
            OAuthError::invalid_response("Claude authorize redirect_uri is missing code")
        })?;
    let state = state
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .ok_or_else(|| {
            OAuthError::invalid_response("Claude authorize redirect_uri is missing state")
        })?;
    if state != expected_state {
        return Err(OAuthError::InvalidState);
    }
    Ok(code)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::network::{OAuthHttpResponse, OAuthNetworkContext};
    use aether_contracts::ProxySnapshot;
    use async_trait::async_trait;
    use std::sync::{Arc, Mutex};

    #[derive(Debug, Clone, Copy, Default)]
    enum RedirectMode {
        #[default]
        Matching,
        WrongState,
        HostileHost,
    }

    #[derive(Clone)]
    struct RecordingExecutor {
        requests: Arc<Mutex<Vec<OAuthHttpRequest>>>,
        organizations: Value,
        token_payload: Value,
        redirect_mode: RedirectMode,
        /// token 端点前 N 次返回的 (status, retry_after) 序列，耗尽后返回 token_payload。
        token_failures: Arc<Mutex<Vec<(u16, Option<u64>)>>>,
    }

    impl Default for RecordingExecutor {
        fn default() -> Self {
            Self {
                requests: Arc::new(Mutex::new(Vec::new())),
                organizations: json!([
                    {"uuid": "org-personal", "raven_type": "personal"},
                    {"uuid": "org-team", "raven_type": "team"}
                ]),
                token_payload: json!({
                    "access_token": "sk-ant-oat01-new",
                    "refresh_token": "sk-ant-ort01-new",
                    "expires_in": 3600,
                    "organization": {"uuid": "org-team"},
                    "account": {
                        "uuid": "account-123",
                        "email_address": "alice@example.com"
                    }
                }),
                redirect_mode: RedirectMode::Matching,
                token_failures: Arc::new(Mutex::new(Vec::new())),
            }
        }
    }

    #[async_trait]
    impl OAuthHttpExecutor for RecordingExecutor {
        async fn execute(
            &self,
            request: OAuthHttpRequest,
        ) -> Result<OAuthHttpResponse, OAuthError> {
            self.requests
                .lock()
                .expect("requests lock")
                .push(request.clone());

            if request.url.ends_with("/api/oauth/profile") {
                let payload = json!({
                    "account": {"uuid": "account-123", "email_address": "alice@example.com", "full_name": "Alice"},
                    "organization": {"uuid": "org-team", "name": "Team Org", "organization_type": "claude_max"}
                });
                return Ok(OAuthHttpResponse {
                    status_code: 200,
                    retry_after_secs: None,
                    body_text: payload.to_string(),
                    json_body: Some(payload),
                });
            }
            if request.url.ends_with("/api/oauth/claude_cli/roles") {
                let payload = json!({"roles": ["claude_cli"]});
                return Ok(OAuthHttpResponse {
                    status_code: 200,
                    retry_after_secs: None,
                    body_text: payload.to_string(),
                    json_body: Some(payload),
                });
            }
            if request.url.ends_with("/v1/oauth/token") {
                let next_failure = self
                    .token_failures
                    .lock()
                    .expect("token failures lock")
                    .pop();
                if let Some((status_code, retry_after_secs)) = next_failure {
                    return Ok(OAuthHttpResponse {
                        status_code,
                        retry_after_secs,
                        body_text: "{\"error\":\"upstream\"}".to_string(),
                        json_body: None,
                    });
                }
            }
            let payload = if request.url.ends_with("/api/organizations") {
                self.organizations.clone()
            } else if request.url.contains("/authorize") {
                let requested_state = request
                    .json_body
                    .as_ref()
                    .and_then(|body| body.get("state"))
                    .and_then(Value::as_str)
                    .expect("authorize request should contain state");
                let state = match self.redirect_mode {
                    RedirectMode::Matching | RedirectMode::HostileHost => requested_state,
                    RedirectMode::WrongState => "wrong-state",
                };
                let redirect_base = match self.redirect_mode {
                    RedirectMode::HostileHost => {
                        "https://platform.claude.com.evil/oauth/code/callback"
                    }
                    _ => CLAUDE_CODE_REDIRECT_URI,
                };
                let mut redirect = Url::parse(redirect_base).expect("redirect URL should parse");
                redirect
                    .query_pairs_mut()
                    .append_pair("code", "authorization-code")
                    .append_pair("state", state);
                json!({"redirect_uri": redirect.to_string()})
            } else {
                self.token_payload.clone()
            };

            Ok(OAuthHttpResponse {
                status_code: 200,
                retry_after_secs: None,
                body_text: payload.to_string(),
                json_body: Some(payload),
            })
        }
    }

    fn context(proxy: Option<ProxySnapshot>) -> ProviderOAuthTransportContext {
        ProviderOAuthTransportContext {
            provider_id: "provider-claude".to_string(),
            provider_type: CLAUDE_CODE_PROVIDER_TYPE.to_string(),
            endpoint_id: None,
            key_id: None,
            auth_type: Some("oauth".to_string()),
            decrypted_api_key: None,
            decrypted_auth_config: None,
            provider_config: None,
            endpoint_config: None,
            key_config: None,
            network: OAuthNetworkContext::provider_operation(proxy),
        }
    }

    #[test]
    fn builds_current_manual_authorize_url() {
        let adapter = ClaudeCodeProviderOAuthAdapter::default();
        let response = adapter
            .build_authorize_url(&context(None), "state-123", Some("challenge-123"))
            .expect("authorize URL should build");
        let url = Url::parse(&response.authorize_url).expect("authorize URL should parse");
        let query = url
            .query_pairs()
            .map(|(key, value)| (key.into_owned(), value.into_owned()))
            .collect::<BTreeMap<_, _>>();

        assert_eq!(
            format!(
                "{}://{}{}",
                url.scheme(),
                url.host_str().unwrap_or_default(),
                url.path()
            ),
            CLAUDE_CODE_AUTHORIZE_URL
        );
        assert_eq!(
            query.get("client_id").map(String::as_str),
            Some(CLAUDE_CODE_CLIENT_ID)
        );
        assert_eq!(
            query.get("redirect_uri").map(String::as_str),
            Some(CLAUDE_CODE_REDIRECT_URI)
        );
        assert_eq!(
            query.get("scope").map(String::as_str),
            Some(CLAUDE_CODE_OAUTH_SCOPES.join(" ").as_str())
        );
        assert_eq!(query.get("code").map(String::as_str), Some("true"));
        assert_eq!(
            query.get("code_challenge").map(String::as_str),
            Some("challenge-123")
        );
        assert!(adapter.capabilities().supports_cookie_authorization);
    }

    #[tokio::test]
    async fn cookie_authorization_uses_team_org_safe_headers_and_current_token_contract() {
        let executor = RecordingExecutor::default();
        let adapter = ClaudeCodeProviderOAuthAdapter::default().with_endpoint_overrides(
            "https://claude.test",
            "https://platform.test/v1/oauth/token",
        );
        let session_key = "sk-ant-sid01-secret";

        let result = adapter
            .authorize_with_cookie(
                &executor,
                &context(None),
                ProviderOAuthCookieAuthorizationInput {
                    session_key: session_key.to_string(),
                },
            )
            .await
            .expect("cookie authorization should succeed");

        assert_eq!(result.token_set.access_token, "sk-ant-oat01-new");
        assert_eq!(
            result.token_set.refresh_token.as_deref(),
            Some("sk-ant-ort01-new")
        );
        assert_eq!(result.auth_config["org_uuid"], "org-team");
        assert_eq!(result.auth_config["account_uuid"], "account-123");
        assert_eq!(result.auth_config["email"], "alice@example.com");
        assert_eq!(result.auth_config["org_name"], "Team Org");
        assert_eq!(result.auth_config["account_name"], "Alice");
        assert_eq!(
            result.auth_config["claude_cli_roles"],
            json!({"roles": ["claude_cli"]})
        );

        let requests = executor.requests.lock().expect("requests lock").clone();
        assert_eq!(requests.len(), 5, "orgs, authorize, token, profile, roles");
        assert!(requests[3].url.ends_with("/api/oauth/profile"));
        assert!(requests[4].url.ends_with("/api/oauth/claude_cli/roles"));
        for request in &requests[3..] {
            assert_eq!(
                request.headers.get("authorization").map(String::as_str),
                Some("Bearer sk-ant-oat01-new")
            );
            assert_eq!(
                request.headers.get("user-agent").map(String::as_str),
                Some("axios/1.13.6")
            );
        }
        assert_eq!(requests[0].method, reqwest::Method::GET);
        assert!(requests[0].url.ends_with("/api/organizations"));
        assert!(requests[1].url.ends_with("/v1/oauth/org-team/authorize"));
        for request in &requests[..2] {
            assert_eq!(
                request.headers.get("cookie").map(String::as_str),
                Some("sessionKey=sk-ant-sid01-secret")
            );
            assert_eq!(
                request
                    .headers
                    .get(EXECUTION_REQUEST_FOLLOW_REDIRECTS_HEADER)
                    .map(String::as_str),
                Some("false")
            );
            assert_eq!(
                request.headers.get("user-agent").map(String::as_str),
                Some(CLAUDE_CODE_BROWSER_USER_AGENT)
            );
            assert_eq!(
                request
                    .transport_profile
                    .as_ref()
                    .map(|profile| profile.backend.as_str()),
                Some(TRANSPORT_BACKEND_BROWSER_WREQ)
            );
            assert!(!format!("{request:?}").contains(session_key));
        }
        assert_eq!(
            requests[1]
                .json_body
                .as_ref()
                .and_then(|body| body.get("scope"))
                .and_then(Value::as_str),
            Some(CLAUDE_CODE_COOKIE_SCOPE)
        );

        let token_request = &requests[2];
        assert_eq!(token_request.url, "https://platform.test/v1/oauth/token");
        assert!(!token_request.headers.contains_key("cookie"));
        assert!(token_request.transport_profile.is_none());
        assert_eq!(
            token_request.headers.get("user-agent").map(String::as_str),
            Some("axios/1.13.6")
        );
        let token_body = token_request
            .json_body
            .as_ref()
            .expect("token request should be JSON");
        assert!(token_body.get("scope").is_none());
        assert_eq!(token_body["redirect_uri"], CLAUDE_CODE_REDIRECT_URI);
        assert_eq!(token_body["code"], "authorization-code");
        assert!(token_body.get("code_verifier").is_some());
    }

    #[test]
    fn session_cookie_accepts_values_above_previous_length_cap() {
        let session_key = "x".repeat(20 * 1024);
        let cookie = ClaudeCodeProviderOAuthAdapter::session_cookie(&session_key)
            .expect("long sessionKey should remain valid");
        assert_eq!(cookie.len(), "sessionKey=".len() + session_key.len());
    }

    #[tokio::test]
    async fn rejects_wrong_state_and_hostile_authorize_redirects() {
        for redirect_mode in [RedirectMode::WrongState, RedirectMode::HostileHost] {
            let executor = RecordingExecutor {
                redirect_mode,
                ..RecordingExecutor::default()
            };
            let adapter = ClaudeCodeProviderOAuthAdapter::default().with_endpoint_overrides(
                "https://claude.test",
                "https://platform.test/v1/oauth/token",
            );
            let error = adapter
                .authorize_with_cookie(
                    &executor,
                    &context(None),
                    ProviderOAuthCookieAuthorizationInput {
                        session_key: "sk-ant-sid01-secret".to_string(),
                    },
                )
                .await
                .expect_err("unsafe redirect should be rejected");
            match redirect_mode {
                RedirectMode::WrongState => assert!(matches!(error, OAuthError::InvalidState)),
                RedirectMode::HostileHost => {
                    assert!(matches!(error, OAuthError::InvalidResponse(_)))
                }
                RedirectMode::Matching => unreachable!(),
            }
        }
    }

    #[test]
    fn tunnel_proxy_falls_back_from_browser_transport_even_when_url_is_present() {
        for proxy in [
            ProxySnapshot {
                node_id: Some("node-only".to_string()),
                ..ProxySnapshot::default()
            },
            ProxySnapshot {
                mode: Some("tunnel".to_string()),
                node_id: Some("node-with-url".to_string()),
                url: Some("http://127.0.0.1:9999".to_string()),
                ..ProxySnapshot::default()
            },
        ] {
            assert!(
                claude_code_oauth_transport_profile_for_context(&context(Some(proxy))).is_none()
            );
        }

        let url_proxy = ProxySnapshot {
            mode: Some("url".to_string()),
            node_id: Some("metadata-node".to_string()),
            url: Some("http://127.0.0.1:9999".to_string()),
            ..ProxySnapshot::default()
        };
        assert!(
            claude_code_oauth_transport_profile_for_context(&context(Some(url_proxy))).is_some()
        );
        assert_eq!(
            cookie_headers("sessionKey=test", false)
                .get("user-agent")
                .map(String::as_str),
            Some(CLAUDE_CODE_BROWSER_USER_AGENT)
        );
    }

    #[tokio::test]
    async fn refresh_rotates_claude_refresh_token_without_scope() {
        let executor = RecordingExecutor::default();
        let adapter = ClaudeCodeProviderOAuthAdapter::default().with_endpoint_overrides(
            "https://claude.test",
            "https://platform.test/v1/oauth/token",
        );
        let account = ProviderOAuthAccount {
            provider_type: CLAUDE_CODE_PROVIDER_TYPE.to_string(),
            access_token: "sk-ant-oat01-old".to_string(),
            auth_config: json!({
                "provider_type": CLAUDE_CODE_PROVIDER_TYPE,
                "refresh_token": "sk-ant-ort01-old",
                "email": "old@example.com"
            }),
            expires_at_unix_secs: Some(1),
            identity: BTreeMap::new(),
        };

        let refreshed = adapter
            .refresh(&executor, &context(None), &account)
            .await
            .expect("refresh should succeed");

        assert_eq!(refreshed.token_set.access_token, "sk-ant-oat01-new");
        assert_eq!(
            refreshed.token_set.refresh_token.as_deref(),
            Some("sk-ant-ort01-new")
        );
        assert_eq!(refreshed.auth_config["refresh_token"], "sk-ant-ort01-new");
        assert_eq!(refreshed.auth_config["org_name"], "Team Org");
        let requests = executor.requests.lock().expect("requests lock");
        assert_eq!(requests.len(), 3, "token, profile, roles");
        let body = requests[0]
            .json_body
            .as_ref()
            .expect("refresh request should be JSON");
        assert_eq!(body["grant_type"], "refresh_token");
        assert_eq!(body["refresh_token"], "sk-ant-ort01-old");
        assert!(body.get("scope").is_none());
    }
    #[tokio::test]
    async fn refresh_retries_5xx_three_times_then_succeeds() {
        std::env::set_var("AETHER_OAUTH_RETRY_NO_DELAY", "1");
        let executor = RecordingExecutor::default();
        *executor.token_failures.lock().expect("lock") =
            vec![(503, None), (502, None), (500, None)];
        let adapter = ClaudeCodeProviderOAuthAdapter::default();
        let account = ProviderOAuthAccount {
            provider_type: CLAUDE_CODE_PROVIDER_TYPE.to_string(),
            access_token: "sk-ant-oat01-old".to_string(),
            auth_config: json!({"provider_type": CLAUDE_CODE_PROVIDER_TYPE, "refresh_token": "sk-ant-ort01-old"}),
            expires_at_unix_secs: Some(1),
            identity: BTreeMap::new(),
        };
        let refreshed = adapter
            .refresh(&executor, &context(None), &account)
            .await
            .expect("refresh should succeed after retries");
        assert_eq!(refreshed.token_set.access_token, "sk-ant-oat01-new");
        let token_calls = executor
            .requests
            .lock()
            .expect("lock")
            .iter()
            .filter(|request| request.url.ends_with("/v1/oauth/token"))
            .count();
        assert_eq!(token_calls, 4, "initial + 3 retries");

        *executor.token_failures.lock().expect("lock") =
            vec![(503, None), (503, None), (503, None), (503, None)];
        executor.requests.lock().expect("lock").clear();
        let error = adapter
            .refresh(&executor, &context(None), &account)
            .await
            .expect_err("fourth 5xx must surface");
        assert!(matches!(
            error,
            OAuthError::HttpStatus {
                status_code: 503,
                ..
            }
        ));
    }

    #[tokio::test]
    async fn refresh_429_surfaces_retry_after_without_retrying() {
        let executor = RecordingExecutor::default();
        *executor.token_failures.lock().expect("lock") = vec![(429, Some(17))];
        let adapter = ClaudeCodeProviderOAuthAdapter::default();
        let account = ProviderOAuthAccount {
            provider_type: CLAUDE_CODE_PROVIDER_TYPE.to_string(),
            access_token: "sk-ant-oat01-old".to_string(),
            auth_config: json!({"provider_type": CLAUDE_CODE_PROVIDER_TYPE, "refresh_token": "sk-ant-ort01-old"}),
            expires_at_unix_secs: Some(1),
            identity: BTreeMap::new(),
        };
        let error = adapter
            .refresh(&executor, &context(None), &account)
            .await
            .expect_err("429 must surface");
        assert!(matches!(
            error,
            OAuthError::RateLimited {
                retry_after_secs: Some(17),
                ..
            }
        ));
        assert_eq!(executor.requests.lock().expect("lock").len(), 1);
    }

    #[test]
    fn control_plane_profile_is_only_used_when_tls_emulation_is_configured() {
        let ctx = context(None);
        assert!(!claude_code_tls_emulation_configured(&ctx));
        assert!(
            claude_code_control_plane_transport_profile_for_context(&ctx, "oauth_refresh")
                .is_none()
        );
        assert!(claude_code_token_request_transport_profile(&ctx).is_none());

        let mut configured = context(None);
        configured.provider_config = Some(json!({
            "fingerprint": {"transport_profile": "claude_code_node_openssl"}
        }));
        assert!(claude_code_tls_emulation_configured(&configured));
        let profile =
            claude_code_control_plane_transport_profile_for_context(&configured, "oauth_inspect")
                .expect("control plane profile");
        assert_eq!(
            profile.profile_id,
            CLAUDE_CODE_OAUTH_CONTROL_PLANE_TLS_PROFILE
        );
        assert_eq!(profile.backend, TRANSPORT_BACKEND_BROWSER_WREQ);
        assert_eq!(
            profile
                .extra
                .as_ref()
                .and_then(|extra| extra.get("request_kind")),
            Some(&json!("oauth_inspect"))
        );
        assert_eq!(
            claude_code_token_request_transport_profile(&configured)
                .and_then(|profile| profile.extra)
                .and_then(|extra| extra.get("request_kind").cloned()),
            Some(json!("oauth_refresh"))
        );

        // Key 级配置（对象形态、连字符大小写）同样命中；非仿真 profile 不命中。
        let mut key_level = context(None);
        key_level.key_config = Some(json!({
            "fingerprint": {"transport_profile": {"profile_id": "Claude-Code-OAuth-Control-Plane"}}
        }));
        assert!(claude_code_tls_emulation_configured(&key_level));
        let mut plain = context(None);
        plain.provider_config = Some(json!({"fingerprint": {"transport_profile": "chrome_136"}}));
        assert!(!claude_code_tls_emulation_configured(&plain));

        // 隧道代理无法使用 browser_wreq。
        let tunnel = ProxySnapshot {
            enabled: Some(true),
            mode: Some("tunnel".to_string()),
            node_id: Some("node-1".to_string()),
            ..ProxySnapshot::default()
        };
        let mut tunnel_ctx = context(Some(tunnel));
        tunnel_ctx.provider_config = Some(json!({
            "fingerprint": {"transport_profile": "claude_code_node_openssl"}
        }));
        assert!(claude_code_control_plane_transport_profile_for_context(
            &tunnel_ctx,
            "oauth_refresh"
        )
        .is_none());
    }

    #[tokio::test]
    async fn refresh_and_profile_lookups_use_control_plane_profile_when_configured() {
        let executor = RecordingExecutor::default();
        let adapter = ClaudeCodeProviderOAuthAdapter::default();
        let mut ctx = context(None);
        ctx.provider_config = Some(json!({
            "fingerprint": {"transport_profile": "claude_code_node_openssl"}
        }));
        let account = ProviderOAuthAccount {
            provider_type: CLAUDE_CODE_PROVIDER_TYPE.to_string(),
            access_token: "sk-ant-oat01-old".to_string(),
            auth_config: json!({
                "provider_type": CLAUDE_CODE_PROVIDER_TYPE,
                "refresh_token": "sk-ant-ort01-old"
            }),
            expires_at_unix_secs: Some(1),
            identity: BTreeMap::new(),
        };
        adapter
            .refresh(&executor, &ctx, &account)
            .await
            .expect("refresh should succeed");
        let requests = executor.requests.lock().expect("mutex should lock");
        let refresh = requests
            .iter()
            .find(|request| request.request_id == "provider-oauth:refresh-token")
            .expect("refresh request");
        let profile = refresh
            .transport_profile
            .as_ref()
            .expect("token refresh should use the control plane profile");
        assert_eq!(
            profile.profile_id,
            CLAUDE_CODE_OAUTH_CONTROL_PLANE_TLS_PROFILE
        );
        assert_eq!(
            profile
                .extra
                .as_ref()
                .and_then(|extra| extra.get("request_kind")),
            Some(&json!("oauth_refresh"))
        );
        let inspect = requests
            .iter()
            .find(|request| request.request_id == "provider-oauth:claude-profile")
            .expect("profile request");
        assert_eq!(
            inspect
                .transport_profile
                .as_ref()
                .and_then(|profile| profile.extra.as_ref())
                .and_then(|extra| extra.get("request_kind")),
            Some(&json!("oauth_inspect"))
        );
        drop(requests);

        // 未配置时保持 reqwest 默认（transport_profile = None）。
        let executor = RecordingExecutor::default();
        adapter
            .refresh(&executor, &context(None), &account)
            .await
            .expect("refresh should succeed");
        let requests = executor.requests.lock().expect("mutex should lock");
        assert!(requests
            .iter()
            .all(|request| request.transport_profile.is_none()));
    }
}
