use std::collections::BTreeMap;
use std::net::{IpAddr, SocketAddr};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use axum::{
    body::Body,
    http,
    response::{IntoResponse, Response},
    Json,
};
use serde_json::json;

use super::test_connection_shared::select_test_connection_provider;
use super::{
    provider_catalog_key_supports_format, query_param_value, AppState, GatewayPublicRequestContext,
};

const DEFAULT_NON_STREAM_TOTAL_TIMEOUT_MS: u64 = 300_000;
const MAX_TEST_CONNECTION_RESPONSE_BYTES: usize = 256 * 1024;

#[cfg(test)]
fn build_test_connection_client() -> Result<reqwest::Client, reqwest::Error> {
    reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(Duration::from_secs(10))
        .http2_adaptive_window(true)
        .build()
}

#[derive(Debug)]
struct ResolvedTestConnectionTarget {
    url: reqwest::Url,
    host: String,
    addresses: Vec<SocketAddr>,
}

/// Resolve the provider endpoint once and pin reqwest to that answer.  The
/// test-connection route is reachable through the public front door, so it
/// must not perform an unbounded DNS lookup on every connect (which would
/// permit DNS rebinding into private/reserved networks).
async fn resolve_test_connection_target(
    raw_url: &str,
    allow_private_targets: bool,
) -> Result<ResolvedTestConnectionTarget, &'static str> {
    let url = reqwest::Url::parse(raw_url).map_err(|_| "provider endpoint URL is invalid")?;
    let literal_loopback = aether_http::url_has_literal_loopback_host(&url);
    if !matches!(url.scheme(), "http" | "https")
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.fragment().is_some()
    {
        return Err("provider endpoint must be an HTTP(S) URL without credentials or fragment");
    }
    if url.scheme() == "http" && !(allow_private_targets && literal_loopback) {
        return Err("provider endpoint must use HTTPS");
    }
    let host = url
        .host_str()
        .ok_or("provider endpoint is missing a host")?
        .to_string();
    let literal_ip = host.parse::<IpAddr>().ok();
    let port = url
        .port_or_known_default()
        .ok_or("provider endpoint is missing a port")?;
    let addresses = if let Some(ip) = literal_ip {
        vec![SocketAddr::new(ip, port)]
    } else {
        aether_http::lookup_host_with_limits(
            host.as_str(),
            port,
            aether_http::DEFAULT_DNS_LOOKUP_TIMEOUT,
        )
        .await
        .map_err(|_| "provider endpoint DNS resolution failed")?
    };
    if addresses.is_empty() {
        return Err("provider endpoint DNS resolution returned no addresses");
    }
    let has_private_answer = addresses
        .iter()
        .any(|address| aether_http::is_private_or_reserved_ip(address.ip()));
    // `allow_private_targets` is only enabled for in-process test fixtures.
    // Keep that escape hatch narrowly scoped to literal loopback URLs whose
    // every DNS answer is loopback; otherwise a test-only build (or an
    // accidentally reused helper) could turn this public route into a
    // private-network HTTP client.
    let test_loopback_target = allow_private_targets
        && literal_loopback
        && addresses.iter().all(|address| address.ip().is_loopback());
    if has_private_answer && !test_loopback_target {
        return Err("provider endpoint resolves to a private or reserved address");
    }
    Ok(ResolvedTestConnectionTarget {
        url,
        host,
        addresses,
    })
}

fn build_pinned_test_connection_client(
    target: &ResolvedTestConnectionTarget,
) -> Result<reqwest::Client, reqwest::Error> {
    let mut builder = reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(Duration::from_secs(10))
        .http2_adaptive_window(true);
    if target.host.parse::<IpAddr>().is_err() {
        builder = builder.resolve_to_addrs(&target.host, &target.addresses);
    }
    builder.build()
}

pub(super) async fn maybe_build_local_test_connection_route_response(
    state: &AppState,
    request_context: &GatewayPublicRequestContext,
) -> Option<Response<Body>> {
    if request_context.request_path != "/v1/test-connection" {
        return None;
    }
    if !state.has_provider_catalog_data_reader() {
        return None;
    }

    let provider_query =
        query_param_value(request_context.request_query_string.as_deref(), "provider");
    let model = query_param_value(request_context.request_query_string.as_deref(), "model")
        .unwrap_or_else(|| "claude-3-haiku-20240307".to_string());
    let requested_api_format = query_param_value(
        request_context.request_query_string.as_deref(),
        "api_format",
    );

    let providers = state
        .list_provider_catalog_providers(true)
        .await
        .ok()
        .unwrap_or_default();
    let Some(provider) = select_test_connection_provider(providers, provider_query.as_deref())
    else {
        return Some(
            (
                http::StatusCode::SERVICE_UNAVAILABLE,
                Json(json!({"detail": "No active provider available"})),
            )
                .into_response(),
        );
    };

    let provider_ids = vec![provider.id.clone()];
    let active_endpoints = state
        .list_provider_catalog_endpoints_by_provider_ids(&provider_ids)
        .await
        .ok()
        .unwrap_or_default()
        .into_iter()
        .filter(|endpoint| endpoint.is_active)
        .collect::<Vec<_>>();
    if active_endpoints.is_empty() {
        return Some(
            (
                http::StatusCode::SERVICE_UNAVAILABLE,
                Json(json!({"detail": "Provider has no active endpoints"})),
            )
                .into_response(),
        );
    }

    let (endpoint, format_value) = if let Some(api_format) = requested_api_format.as_deref() {
        let Some(endpoint) = active_endpoints
            .into_iter()
            .find(|endpoint| endpoint.api_format.eq_ignore_ascii_case(api_format))
        else {
            return Some(
                (
                    http::StatusCode::BAD_REQUEST,
                    Json(json!({
                        "detail": format!(
                            "Provider has no active endpoint for api_format={api_format}"
                        ),
                    })),
                )
                    .into_response(),
            );
        };
        (endpoint, api_format.to_string())
    } else {
        let endpoint = active_endpoints.into_iter().next()?;
        let format_value = if endpoint.api_format.trim().is_empty() {
            "claude:messages".to_string()
        } else {
            crate::ai_serving::normalize_api_format_alias(&endpoint.api_format)
        };
        (endpoint, format_value)
    };

    let format_value = crate::ai_serving::normalize_api_format_alias(&format_value);
    if !matches!(
        format_value.as_str(),
        "openai:chat" | "claude:messages" | "gemini:generate_content" | "gemini:interactions"
    ) {
        return None;
    }

    let active_keys = state
        .list_provider_catalog_keys_by_provider_ids(&provider_ids)
        .await
        .ok()
        .unwrap_or_default()
        .into_iter()
        .filter(|key| key.is_active)
        .collect::<Vec<_>>();
    if active_keys.is_empty() {
        return Some(
            (
                http::StatusCode::SERVICE_UNAVAILABLE,
                Json(json!({"detail": "Provider has no active api keys"})),
            )
                .into_response(),
        );
    }
    let Some(key) = active_keys
        .iter()
        .find(|key| {
            provider_catalog_key_supports_format(
                key,
                provider.provider_type.as_str(),
                &format_value,
            )
        })
        .cloned()
        .or_else(|| active_keys.into_iter().next())
    else {
        return Some(
            (
                http::StatusCode::SERVICE_UNAVAILABLE,
                Json(json!({"detail": "Provider has no active api keys"})),
            )
                .into_response(),
        );
    };

    let transport = match state
        .read_provider_transport_snapshot(&provider.id, &endpoint.id, &key.id)
        .await
    {
        Ok(Some(transport)) => transport,
        Ok(None) => return None,
        Err(_) => {
            return Some(
                (
                    http::StatusCode::SERVICE_UNAVAILABLE,
                    Json(json!({"detail": "Provider transport snapshot unavailable"})),
                )
                    .into_response(),
            )
        }
    };

    if transport.provider.proxy.is_some()
        || transport.endpoint.proxy.is_some()
        || transport.key.proxy.is_some()
        || crate::provider_transport::resolve_transport_profile(&transport).is_some()
    {
        return None;
    }

    let mut provider_request_body = match format_value.as_str() {
        "openai:chat" => json!({
            "model": model,
            "messages": [{"role": "user", "content": "Health check"}],
        }),
        "claude:messages" => json!({
            "model": model,
            "messages": [{"role": "user", "content": "Health check"}],
            "max_tokens": 5,
        }),
        "gemini:generate_content" => json!({
            "contents": [{
                "role": "user",
                "parts": [{"text": "Health check"}],
            }],
        }),
        "gemini:interactions" => json!({
            "model": model,
            "input": "Health check",
        }),
        _ => return None,
    };
    if !crate::provider_transport::apply_local_body_rules(
        &mut provider_request_body,
        transport.endpoint.body_rules.as_ref(),
        None,
    ) {
        return None;
    }

    let oauth_auth = match format_value.as_str() {
        "openai:chat" | "claude:messages" | "gemini:generate_content" | "gemini:interactions" => {
            match state.resolve_local_oauth_request_auth(&transport).await {
                Ok(Some(crate::provider_transport::LocalResolvedOAuthRequestAuth::Header {
                    name,
                    value,
                })) => Some((name, value)),
                _ => None,
            }
        }
        _ => None,
    };

    let auth = match format_value.as_str() {
        "openai:chat" => {
            crate::provider_transport::auth::resolve_local_openai_bearer_auth(&transport)
                .or(oauth_auth.clone())
        }
        "claude:messages" => {
            crate::provider_transport::auth::resolve_local_standard_auth(&transport)
                .or(oauth_auth.clone())
        }
        "gemini:generate_content" | "gemini:interactions" => {
            crate::provider_transport::auth::resolve_local_gemini_auth(&transport)
                .or(oauth_auth.clone())
        }
        _ => None,
    };
    let uses_vertex_query_auth = crate::provider_transport::uses_vertex_api_key_query_auth(
        &transport,
        format_value.as_str(),
    );
    let vertex_query_auth = if uses_vertex_query_auth {
        crate::provider_transport::vertex::resolve_local_vertex_api_key_query_auth(&transport)
    } else {
        None
    };
    let (auth_header, auth_value) = match auth {
        Some((auth_header, auth_value)) => (auth_header, auth_value),
        None if uses_vertex_query_auth && vertex_query_auth.is_some() => {
            (String::new(), String::new())
        }
        None => return None,
    };

    let upstream_url = crate::provider_transport::build_transport_request_url(
        &transport,
        crate::provider_transport::TransportRequestUrlParams {
            provider_api_format: format_value.as_str(),
            mapped_model: Some(model.as_str()),
            upstream_is_stream: false,
            request_query: None,
            kiro_api_region: None,
            api_operation: None,
        },
    );
    let Some(upstream_url) = upstream_url else {
        return None;
    };

    let mut provider_request_headers =
        BTreeMap::from([("content-type".to_string(), "application/json".to_string())]);
    if !auth_header.trim().is_empty() && !auth_value.trim().is_empty() {
        provider_request_headers.insert(auth_header.clone(), auth_value.clone());
    }
    if uses_vertex_query_auth {
        provider_request_headers.remove("x-goog-api-key");
    }
    let protected_headers = if uses_vertex_query_auth || auth_value.trim().is_empty() {
        vec!["content-type"]
    } else {
        vec![auth_header.as_str(), "content-type"]
    };
    if !crate::provider_transport::apply_local_header_rules(
        &mut provider_request_headers,
        transport.endpoint.header_rules.as_ref(),
        &protected_headers,
        &provider_request_body,
        None,
    ) {
        return None;
    }
    if !uses_vertex_query_auth {
        crate::provider_transport::ensure_upstream_auth_header(
            &mut provider_request_headers,
            &auth_header,
            &auth_value,
        );
    }

    // Resolve and pin the endpoint before constructing the request.  This
    // keeps the public health-check route subject to the same DNS/SSRF
    // boundary as the main execution transport.  Unit-test fixtures may use
    // loopback listeners; production requests never opt into private targets.
    let target = match resolve_test_connection_target(&upstream_url, cfg!(test)).await {
        Ok(target) => target,
        Err(reason) => {
            tracing::warn!(
                event_name = "provider_test_connection_target_rejected",
                provider_id = %provider.id,
                endpoint_id = %endpoint.id,
                reason,
                "provider connection test target was rejected"
            );
            return Some(
                (
                    http::StatusCode::SERVICE_UNAVAILABLE,
                    Json(json!({"detail": "Provider connection test unavailable"})),
                )
                    .into_response(),
            );
        }
    };
    let test_client = match build_pinned_test_connection_client(&target) {
        Ok(client) => client,
        Err(_) => {
            return Some(
                (
                    http::StatusCode::SERVICE_UNAVAILABLE,
                    Json(json!({"detail": "Provider connection test unavailable"})),
                )
                    .into_response(),
            );
        }
    };
    let mut upstream_request = test_client.post(target.url);
    for (name, value) in &provider_request_headers {
        upstream_request = upstream_request.header(name, value);
    }
    let total_ms = crate::provider_transport::resolve_transport_execution_timeouts(&transport)
        .and_then(|timeouts| timeouts.total_ms)
        .unwrap_or(DEFAULT_NON_STREAM_TOTAL_TIMEOUT_MS);
    upstream_request = upstream_request.timeout(Duration::from_millis(total_ms));

    let response = match upstream_request.json(&provider_request_body).send().await {
        Ok(response) => response,
        Err(_) => {
            tracing::warn!(
                event_name = "provider_test_connection_request_failed",
                provider_id = %provider.id,
                endpoint_id = %endpoint.id,
                "provider connection test request failed"
            );
            return Some(
                (
                    http::StatusCode::SERVICE_UNAVAILABLE,
                    Json(json!({"detail": "Provider connection test failed"})),
                )
                    .into_response(),
            );
        }
    };

    let status = response.status();
    let response_json =
        aether_http::read_response_bytes_with_limit(response, MAX_TEST_CONNECTION_RESPONSE_BYTES)
            .await
            .ok()
            .and_then(|body| serde_json::from_slice::<serde_json::Value>(&body).ok());
    if !status.is_success() {
        tracing::warn!(
            event_name = "provider_test_connection_upstream_rejected",
            provider_id = %provider.id,
            endpoint_id = %endpoint.id,
            upstream_status = %status,
            "provider connection test upstream returned an error"
        );
        return Some(
            (
                http::StatusCode::SERVICE_UNAVAILABLE,
                Json(json!({ "detail": "Provider connection test failed" })),
            )
                .into_response(),
        );
    }

    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs().to_string())
        .unwrap_or_else(|_| "0".to_string());
    let response_id = response_json
        .as_ref()
        .and_then(|value| value.get("id"))
        .and_then(serde_json::Value::as_str)
        .unwrap_or("unknown");

    Some(
        Json(json!({
            "status": "success",
            "provider_id": provider.id,
            "endpoint_id": endpoint.id,
            "api_format": format_value,
            "timestamp": timestamp,
            "response_id": response_id,
        }))
        .into_response(),
    )
}

#[cfg(test)]
mod tests {
    use super::{build_test_connection_client, resolve_test_connection_target};
    use axum::{
        body::Body,
        http::{header, Request, StatusCode},
        response::IntoResponse,
        routing::post,
        Router,
    };
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    };

    #[tokio::test]
    async fn test_connection_client_never_forwards_credentials_across_redirects() {
        let redirected_hits = Arc::new(AtomicUsize::new(0));
        let redirected_hits_for_route = Arc::clone(&redirected_hits);
        let redirected_listener = crate::test_support::bind_loopback_listener()
            .await
            .expect("redirect target listener");
        let redirected_addr = redirected_listener
            .local_addr()
            .expect("redirect target addr");
        let redirected_app = Router::new().route(
            "/capture",
            post(move |request: Request<Body>| {
                let hits = Arc::clone(&redirected_hits_for_route);
                async move {
                    hits.fetch_add(1, Ordering::SeqCst);
                    assert!(request.headers().get(header::AUTHORIZATION).is_none());
                    StatusCode::OK
                }
            }),
        );
        let redirected_server = tokio::spawn(async move {
            axum::serve(redirected_listener, redirected_app)
                .await
                .expect("redirect target server");
        });

        let source_listener = crate::test_support::bind_loopback_listener()
            .await
            .expect("redirect source listener");
        let source_addr = source_listener.local_addr().expect("redirect source addr");
        let location = format!("http://{redirected_addr}/capture");
        let source_app = Router::new().route(
            "/redirect",
            post(move || {
                let location = location.clone();
                async move { (StatusCode::FOUND, [(header::LOCATION, location)]).into_response() }
            }),
        );
        let source_server = tokio::spawn(async move {
            axum::serve(source_listener, source_app)
                .await
                .expect("redirect source server");
        });

        let response = build_test_connection_client()
            .expect("client")
            .post(format!("http://{source_addr}/redirect"))
            .header(header::AUTHORIZATION, "Bearer stored-provider-secret")
            .send()
            .await
            .expect("redirect response");
        assert_eq!(response.status(), StatusCode::FOUND);
        assert_eq!(redirected_hits.load(Ordering::SeqCst), 0);

        source_server.abort();
        redirected_server.abort();
    }

    #[tokio::test]
    async fn test_connection_target_rejects_private_addresses_in_production_mode() {
        for raw_url in [
            "http://127.0.0.1:8080/v1/chat/completions",
            "https://10.0.0.1/v1/chat/completions",
            "https://[::1]/v1/chat/completions",
            "https://localhost/v1/chat/completions",
            "http://8.8.8.8/v1/chat/completions",
        ] {
            assert!(
                resolve_test_connection_target(raw_url, false)
                    .await
                    .is_err(),
                "private provider target should be rejected: {raw_url}"
            );
        }
    }

    #[tokio::test]
    async fn test_connection_target_allows_loopback_only_for_test_fixtures() {
        let target = resolve_test_connection_target("http://127.0.0.1:8080/v1/chat", true)
            .await
            .expect("test fixture target should resolve");
        assert_eq!(target.host, "127.0.0.1");
        assert_eq!(target.addresses.len(), 1);
        assert!(
            resolve_test_connection_target("http://8.8.8.8/v1/chat", true)
                .await
                .is_err(),
            "test mode must not make cleartext public endpoints acceptable"
        );
        assert!(
            resolve_test_connection_target("https://10.0.0.1/v1/chat", true)
                .await
                .is_err(),
            "test mode must not make private non-loopback endpoints acceptable"
        );
        assert!(
            resolve_test_connection_target("http://localhost:8080/v1/chat", true)
                .await
                .is_ok(),
            "literal localhost should remain available for local fixtures"
        );
    }

    #[tokio::test]
    async fn test_connection_target_rejects_url_credentials_and_fragments() {
        for raw_url in [
            "https://user:pass@example.com/v1/chat",
            "https://example.com/v1/chat#fragment",
        ] {
            assert!(
                resolve_test_connection_target(raw_url, false)
                    .await
                    .is_err(),
                "unsafe provider target should be rejected: {raw_url}"
            );
        }
    }
}
