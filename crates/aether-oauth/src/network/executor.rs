use crate::core::OAuthError;
use aether_contracts::{redact_url_for_debug, ResolvedTransportProfile};
use async_trait::async_trait;
use serde_json::Value;
use std::collections::BTreeMap;

use super::OAuthNetworkContext;

const OAUTH_HTTP_RESPONSE_BODY_LIMIT_BYTES: usize = 4 * 1024 * 1024;

#[derive(Clone, PartialEq)]
pub struct OAuthHttpRequest {
    pub request_id: String,
    pub method: reqwest::Method,
    pub url: String,
    pub headers: BTreeMap<String, String>,
    pub content_type: Option<String>,
    pub json_body: Option<Value>,
    pub body_bytes: Option<Vec<u8>>,
    pub network: OAuthNetworkContext,
    pub transport_profile: Option<ResolvedTransportProfile>,
}

impl std::fmt::Debug for OAuthHttpRequest {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("OAuthHttpRequest")
            .field("request_id", &self.request_id)
            .field("method", &self.method)
            .field("url", &redact_url_for_debug(&self.url))
            .field("header_names", &self.headers.keys().collect::<Vec<_>>())
            .field("content_type", &self.content_type)
            .field("has_json_body", &self.json_body.is_some())
            .field("body_bytes_len", &self.body_bytes.as_ref().map(Vec::len))
            .field("network_policy", &self.network.policy)
            .field("has_proxy", &self.network.proxy.is_some())
            .field(
                "transport_profile_id",
                &self
                    .transport_profile
                    .as_ref()
                    .map(|profile| profile.profile_id.as_str()),
            )
            .finish()
    }
}

#[derive(Clone, PartialEq)]
pub struct OAuthHttpResponse {
    pub status_code: u16,
    pub body_text: String,
    pub json_body: Option<Value>,
    /// 上游 `Retry-After`（秒）；刷新 429 时据此退避。
    pub retry_after_secs: Option<u64>,
}

impl OAuthHttpResponse {
    /// 解析 HTTP `Retry-After` 头（秒或 HTTP-date）。
    pub fn parse_retry_after_header(value: Option<&str>) -> Option<u64> {
        let value = value?.trim();
        if value.is_empty() {
            return None;
        }
        if let Ok(seconds) = value.parse::<u64>() {
            return Some(seconds);
        }
        if let Ok(seconds) = value.parse::<f64>() {
            return (seconds.is_finite() && seconds >= 0.0).then(|| seconds.ceil() as u64);
        }
        let deadline = httpdate::parse_http_date(value).ok()?;
        deadline
            .duration_since(std::time::SystemTime::now())
            .ok()
            .map(|duration| duration.as_secs())
            .or(Some(0))
    }
}

impl std::fmt::Debug for OAuthHttpResponse {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("OAuthHttpResponse")
            .field("status_code", &self.status_code)
            .field("body_bytes_len", &self.body_text.len())
            .field("has_json_body", &self.json_body.is_some())
            .finish()
    }
}

#[async_trait]
pub trait OAuthHttpExecutor: Send + Sync {
    async fn execute(&self, request: OAuthHttpRequest) -> Result<OAuthHttpResponse, OAuthError>;
}

#[derive(Debug, Clone)]
pub struct ReqwestOAuthHttpExecutor {
    client: reqwest::Client,
}

impl ReqwestOAuthHttpExecutor {
    pub fn new(client: reqwest::Client) -> Self {
        Self { client }
    }
}

#[async_trait]
impl OAuthHttpExecutor for ReqwestOAuthHttpExecutor {
    async fn execute(&self, request: OAuthHttpRequest) -> Result<OAuthHttpResponse, OAuthError> {
        let mut builder = self
            .client
            .request(request.method.clone(), request.url.as_str());
        for (name, value) in &request.headers {
            builder = builder.header(name, value);
        }
        if let Some(json_body) = request.json_body.as_ref() {
            builder = builder.json(json_body);
        } else if let Some(body_bytes) = request.body_bytes.as_ref() {
            builder = builder.body(body_bytes.clone());
        }

        let mut response = builder
            .send()
            .await
            .map_err(|err| OAuthError::transport(err.to_string()))?;
        let status_code = response.status().as_u16();
        let retry_after_secs = OAuthHttpResponse::parse_retry_after_header(
            response
                .headers()
                .get(reqwest::header::RETRY_AFTER)
                .and_then(|value| value.to_str().ok()),
        );
        if response
            .content_length()
            .is_some_and(|length| length > OAUTH_HTTP_RESPONSE_BODY_LIMIT_BYTES as u64)
        {
            return Err(oauth_http_response_too_large());
        }
        let mut body = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|err| OAuthError::transport(err.to_string()))?
        {
            if chunk.len() > OAUTH_HTTP_RESPONSE_BODY_LIMIT_BYTES.saturating_sub(body.len()) {
                return Err(oauth_http_response_too_large());
            }
            body.extend_from_slice(&chunk);
        }
        let body_text = String::from_utf8_lossy(&body).to_string();
        let json_body = serde_json::from_str::<Value>(&body_text).ok();
        Ok(OAuthHttpResponse {
            status_code,
            body_text,
            json_body,
            retry_after_secs,
        })
    }
}

fn oauth_http_response_too_large() -> OAuthError {
    OAuthError::transport(format!(
        "OAuth response body exceeds {OAUTH_HTTP_RESPONSE_BODY_LIMIT_BYTES} bytes"
    ))
}

#[cfg(test)]
mod tests {
    use super::{OAuthHttpRequest, OAuthHttpResponse};
    use crate::network::OAuthNetworkContext;
    use std::collections::BTreeMap;

    #[test]
    fn response_debug_output_does_not_expose_token_payloads() {
        let response = OAuthHttpResponse {
            status_code: 200,
            retry_after_secs: None,
            body_text: "{\"access_token\":\"response-body-canary\"}".to_string(),
            json_body: Some(serde_json::json!({"refresh_token": "response-json-canary"})),
        };

        let debug = format!("{response:?}");
        assert!(!debug.contains("response-body-canary"));
        assert!(!debug.contains("response-json-canary"));
        assert!(debug.contains("body_bytes_len"));
    }

    #[test]
    fn request_debug_redacts_url_credentials_and_query() {
        let request = OAuthHttpRequest {
            request_id: "request-1".into(),
            method: reqwest::Method::GET,
            url: "https://user:pass@example.test/oauth?client_secret=url-secret".into(),
            headers: BTreeMap::from([("authorization".into(), "Bearer header-secret".into())]),
            content_type: None,
            json_body: None,
            body_bytes: None,
            network: OAuthNetworkContext::direct_identity(),
            transport_profile: None,
        };
        let debug = format!("{request:?}");
        assert!(!debug.contains("user"));
        assert!(!debug.contains("pass"));
        assert!(!debug.contains("url-secret"));
        assert!(!debug.contains("header-secret"));
        assert!(debug.contains("https://example.test/oauth"));
    }
}
