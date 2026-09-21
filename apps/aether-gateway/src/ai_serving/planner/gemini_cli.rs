use std::collections::BTreeMap;
use std::sync::Arc;

use serde_json::Value;

use crate::ai_serving::transport::{
    build_gemini_cli_v1internal_request, build_standard_provider_request_headers,
    GatewayProviderTransportSnapshot, GeminiCliRequestAuth, GeminiCliRequestAuthSupport,
    GeminiCliRequestEnvelopeSupport, StandardProviderRequestHeaders,
    StandardProviderRequestHeadersInput, GEMINI_CLI_USER_AGENT,
};
use crate::AppState;

pub(crate) enum GeminiCliV1InternalRequestError {
    ProjectUnavailable,
    EnvelopeUnsupported,
    UpstreamUrlUnavailable,
    HeaderRulesApplyFailed,
}

pub(crate) struct GeminiCliV1InternalRequestInput<'a> {
    pub(crate) state: &'a AppState,
    pub(crate) parts: &'a http::request::Parts,
    pub(crate) transport: &'a Arc<GatewayProviderTransportSnapshot>,
    pub(crate) trace_id: &'a str,
    pub(crate) mapped_model: &'a str,
    pub(crate) provider_api_format: &'a str,
    pub(crate) auth_header: &'a str,
    pub(crate) auth_value: &'a str,
    pub(crate) request_headers: &'a http::HeaderMap,
    pub(crate) original_request_body: &'a Value,
    pub(crate) gemini_request_body: &'a Value,
    pub(crate) upstream_is_stream: bool,
}

pub(crate) struct GeminiCliV1InternalRequest {
    pub(crate) transport: Arc<GatewayProviderTransportSnapshot>,
    pub(crate) body: Value,
    pub(crate) headers: StandardProviderRequestHeaders,
    pub(crate) upstream_url: String,
}

pub(crate) async fn build_gemini_cli_v1internal_provider_request(
    input: GeminiCliV1InternalRequestInput<'_>,
) -> Result<GeminiCliV1InternalRequest, GeminiCliV1InternalRequestError> {
    let payload = build_gemini_cli_v1internal_payload(
        input.state,
        input.transport,
        input.trace_id,
        input.mapped_model,
        input.gemini_request_body,
    )
    .await?;

    let upstream_url = crate::ai_serving::build_provider_transport_request_url_for_request_body(
        &payload.transport,
        input.provider_api_format,
        Some(input.mapped_model),
        input.upstream_is_stream,
        input.parts.uri.query(),
        None,
        None,
        Some(&payload.body),
    )
    .ok_or(GeminiCliV1InternalRequestError::UpstreamUrlUnavailable)?;

    let extra_headers =
        BTreeMap::from([("user-agent".to_string(), GEMINI_CLI_USER_AGENT.to_string())]);
    let headers = build_standard_provider_request_headers(StandardProviderRequestHeadersInput {
        transport: &payload.transport,
        provider_api_format: input.provider_api_format,
        same_format: false,
        headers: input.request_headers,
        auth_header: input.auth_header,
        auth_value: input.auth_value,
        extra_headers: &extra_headers,
        header_rules: payload.transport.endpoint.header_rules.as_ref(),
        provider_request_body: &payload.body,
        original_request_body: input.original_request_body,
        upstream_is_stream: input.upstream_is_stream,
    })
    .ok_or(GeminiCliV1InternalRequestError::HeaderRulesApplyFailed)?;

    Ok(GeminiCliV1InternalRequest {
        transport: payload.transport,
        body: payload.body,
        headers,
        upstream_url,
    })
}

struct GeminiCliV1InternalPayload {
    transport: Arc<GatewayProviderTransportSnapshot>,
    body: Value,
}

async fn build_gemini_cli_v1internal_payload(
    state: &AppState,
    transport: &Arc<GatewayProviderTransportSnapshot>,
    trace_id: &str,
    mapped_model: &str,
    gemini_request_body: &Value,
) -> Result<GeminiCliV1InternalPayload, GeminiCliV1InternalRequestError> {
    let mut resolved_transport = Arc::clone(transport);
    let mut auth = match crate::ai_serving::transport::resolve_local_gemini_cli_request_auth(
        &resolved_transport,
    ) {
        GeminiCliRequestAuthSupport::Supported(auth) => auth,
        GeminiCliRequestAuthSupport::Unsupported(_) => {
            return Err(GeminiCliV1InternalRequestError::ProjectUnavailable);
        }
    };
    if auth.project_id.is_none() {
        auth = match state
            .hydrate_gemini_cli_project_metadata_for_transport(&resolved_transport)
            .await
        {
            Some(hydrated) => {
                resolved_transport = Arc::new(hydrated);
                match crate::ai_serving::transport::resolve_local_gemini_cli_request_auth(
                    &resolved_transport,
                ) {
                    GeminiCliRequestAuthSupport::Supported(auth) => auth,
                    GeminiCliRequestAuthSupport::Unsupported(_) => GeminiCliRequestAuth::default(),
                }
            }
            None => GeminiCliRequestAuth::default(),
        };
    }
    let body = match build_gemini_cli_v1internal_request(
        &auth,
        trace_id,
        mapped_model,
        gemini_request_body,
    ) {
        GeminiCliRequestEnvelopeSupport::Supported(envelope) => envelope,
        GeminiCliRequestEnvelopeSupport::Unsupported(_) => {
            return Err(GeminiCliV1InternalRequestError::EnvelopeUnsupported);
        }
    };

    Ok(GeminiCliV1InternalPayload {
        transport: resolved_transport,
        body,
    })
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use serde_json::{json, Value};

    use super::{
        build_gemini_cli_v1internal_provider_request, GeminiCliV1InternalRequestError,
        GeminiCliV1InternalRequestInput,
    };
    use crate::ai_serving::transport::snapshot::{
        GatewayProviderTransportEndpoint, GatewayProviderTransportKey,
        GatewayProviderTransportProvider, GatewayProviderTransportSnapshot,
    };
    use crate::ai_serving::transport::GEMINI_CLI_USER_AGENT;
    use crate::AppState;

    fn sample_transport(auth_config: Option<&str>) -> GatewayProviderTransportSnapshot {
        GatewayProviderTransportSnapshot {
            provider: GatewayProviderTransportProvider {
                id: "provider-1".to_string(),
                name: "Gemini CLI".to_string(),
                provider_type: "gemini_cli".to_string(),
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
                api_format: "gemini:generate_content".to_string(),
                api_family: Some("gemini".to_string()),
                endpoint_kind: Some("generate_content".to_string()),
                is_active: true,
                base_url: "https://cloudcode-pa.googleapis.com".to_string(),
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
                name: "key".to_string(),
                auth_type: "oauth".to_string(),
                is_active: true,
                api_formats: Some(vec!["gemini:generate_content".to_string()]),
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
                decrypted_api_key: "gemini-cli-access-token".to_string(),
                decrypted_auth_config: auth_config.map(ToOwned::to_owned),
            },
        }
    }

    fn sample_parts(uri: &str) -> http::request::Parts {
        let (parts, _) = http::Request::builder()
            .method("POST")
            .uri(uri)
            .body(())
            .expect("request should build")
            .into_parts();
        parts
    }

    fn sample_headers() -> http::HeaderMap {
        let mut headers = http::HeaderMap::new();
        headers.insert("content-type", "application/json".parse().expect("header"));
        headers.insert("x-goog-api-key", "client-secret".parse().expect("header"));
        headers
    }

    async fn build(
        transport: GatewayProviderTransportSnapshot,
        gemini_request_body: &Value,
        upstream_is_stream: bool,
    ) -> Result<super::GeminiCliV1InternalRequest, GeminiCliV1InternalRequestError> {
        let state = AppState::new().expect("gateway state should build");
        let parts = if upstream_is_stream {
            sample_parts(
                "/v1beta/models/gemini-2.5-pro:streamGenerateContent?alt=sse&key=client-secret",
            )
        } else {
            sample_parts("/v1beta/models/gemini-2.5-pro:generateContent?key=client-secret")
        };
        let request_headers = sample_headers();
        let original_request_body = json!({ "contents": [], "stream": true });
        let transport = Arc::new(transport);
        build_gemini_cli_v1internal_provider_request(GeminiCliV1InternalRequestInput {
            state: &state,
            parts: &parts,
            transport: &transport,
            trace_id: "trace-gemini-cli-1",
            mapped_model: "gemini-2.5-pro",
            provider_api_format: "gemini:generate_content",
            auth_header: "authorization",
            auth_value: "Bearer gemini-cli-access-token",
            request_headers: &request_headers,
            original_request_body: &original_request_body,
            gemini_request_body,
            upstream_is_stream,
        })
        .await
    }

    #[tokio::test]
    async fn wraps_the_gemini_body_into_a_v1internal_envelope_with_the_key_project() {
        let request_body = json!({
            "contents": [{ "role": "user", "parts": [{ "text": "hello" }] }],
            "generationConfig": { "temperature": 0.2 }
        });

        let request = build(
            sample_transport(Some(
                r#"{"project_id":"project-from-auth","refresh_token":"rt"}"#,
            )),
            &request_body,
            true,
        )
        .await
        .unwrap_or_else(|error| panic!("request should build: {}", error_label(&error)));

        assert_eq!(request.body["project"], "project-from-auth");
        assert_eq!(request.body["model"], "gemini-2.5-pro");
        assert_eq!(request.body["user_prompt_id"], "trace-gemini-cli-1");
        assert_eq!(
            request.body["request"]["contents"][0]["parts"][0]["text"],
            "hello"
        );
        assert_eq!(
            request.body["request"]["generationConfig"]["temperature"],
            0.2
        );
        assert!(request.body.get("contents").is_none());

        assert_eq!(
            request.upstream_url,
            "https://cloudcode-pa.googleapis.com/v1internal:streamGenerateContent?alt=sse"
        );
        assert_eq!(
            request
                .headers
                .headers
                .get("user-agent")
                .map(String::as_str),
            Some(GEMINI_CLI_USER_AGENT)
        );
        assert_eq!(
            request
                .headers
                .headers
                .get("authorization")
                .map(String::as_str),
            Some("Bearer gemini-cli-access-token")
        );
        assert_eq!(
            request.headers.headers.get("accept").map(String::as_str),
            Some("text/event-stream")
        );
        // 客户端自带的 Google API key 不得带到 Cloud Code。
        assert!(!request.headers.headers.contains_key("x-goog-api-key"));
        assert_eq!(request.transport.key.id, "key-1");
    }

    #[tokio::test]
    async fn non_stream_requests_target_generate_content() {
        let request_body = json!({
            "contents": [{ "role": "user", "parts": [{ "text": "hello" }] }]
        });

        let request = build(
            sample_transport(Some(r#"{"project_id":"project-from-auth"}"#)),
            &request_body,
            false,
        )
        .await
        .unwrap_or_else(|error| panic!("request should build: {}", error_label(&error)));

        assert_eq!(
            request.upstream_url,
            "https://cloudcode-pa.googleapis.com/v1internal:generateContent"
        );
        assert!(request
            .headers
            .headers
            .get("accept")
            .is_none_or(|accept| accept != "text/event-stream"));
    }

    #[tokio::test]
    async fn bodies_without_contents_are_rejected_as_unsupported_envelopes() {
        let error = build(
            sample_transport(Some(r#"{"project_id":"project-from-auth"}"#)),
            &json!({ "prompt": "not a gemini body" }),
            true,
        )
        .await
        .err()
        .expect("body without contents must not be wrapped");

        assert!(matches!(
            error,
            GeminiCliV1InternalRequestError::EnvelopeUnsupported
        ));
    }

    #[tokio::test]
    async fn invalid_auth_config_json_makes_the_project_unavailable() {
        let error = build(
            sample_transport(Some("{not json")),
            &json!({ "contents": [] }),
            true,
        )
        .await
        .err()
        .expect("invalid auth config must not produce a request");

        assert!(matches!(
            error,
            GeminiCliV1InternalRequestError::ProjectUnavailable
        ));
    }

    #[tokio::test]
    async fn metadata_project_wins_over_auth_config_and_session_id_is_forwarded() {
        let mut transport = sample_transport(Some(
            r#"{"project_id":"project-from-auth","session_id":"session-from-auth"}"#,
        ));
        transport.key.upstream_metadata = Some(json!({
            "gemini_cli": { "project_id": "project-from-metadata" }
        }));
        let request = build(
            transport,
            &json!({ "contents": [{ "role": "user", "parts": [{ "text": "hi" }] }] }),
            true,
        )
        .await
        .unwrap_or_else(|error| panic!("request should build: {}", error_label(&error)));

        assert_eq!(request.body["project"], "project-from-metadata");
        assert_eq!(request.body["request"]["session_id"], "session-from-auth");
    }

    fn error_label(error: &GeminiCliV1InternalRequestError) -> &'static str {
        match error {
            GeminiCliV1InternalRequestError::ProjectUnavailable => "project unavailable",
            GeminiCliV1InternalRequestError::EnvelopeUnsupported => "envelope unsupported",
            GeminiCliV1InternalRequestError::UpstreamUrlUnavailable => "upstream url unavailable",
            GeminiCliV1InternalRequestError::HeaderRulesApplyFailed => "header rules apply failed",
        }
    }
}
