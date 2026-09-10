use axum::body::Body;
use axum::extract::Request;
use axum::http::{HeaderMap, HeaderName, HeaderValue, Response};
use axum::middleware::Next;

fn insert_if_missing(headers: &mut HeaderMap, name: &'static str, value: &'static str) {
    if !headers.contains_key(name) {
        headers.insert(
            HeaderName::from_static(name),
            HeaderValue::from_static(value),
        );
    }
}

fn is_sensitive_api_path(path: &str) -> bool {
    path.starts_with("/api/auth/")
        || path.starts_with("/api/admin/")
        || path.starts_with("/api/users/")
}

pub(crate) async fn security_headers_middleware(request: Request, next: Next) -> Response<Body> {
    let sensitive_api = is_sensitive_api_path(request.uri().path());
    let mut response = next.run(request).await;
    let headers = response.headers_mut();

    insert_if_missing(headers, "x-content-type-options", "nosniff");
    insert_if_missing(headers, "x-frame-options", "DENY");
    insert_if_missing(headers, "referrer-policy", "no-referrer");
    insert_if_missing(
        headers,
        "permissions-policy",
        "camera=(), geolocation=(), microphone=()",
    );
    insert_if_missing(
        headers,
        "content-security-policy",
        "base-uri 'self'; frame-ancestors 'none'; object-src 'none'",
    );
    if sensitive_api {
        let no_transform = headers
            .get_all(http::header::CACHE_CONTROL)
            .iter()
            .filter_map(|value| value.to_str().ok())
            .flat_map(|value| value.split(','))
            .any(|directive| directive.trim().eq_ignore_ascii_case("no-transform"));
        headers.insert(
            http::header::CACHE_CONTROL,
            HeaderValue::from_static(if no_transform {
                "no-store, no-transform"
            } else {
                "no-store"
            }),
        );
        headers.insert(http::header::PRAGMA, HeaderValue::from_static("no-cache"));
    }

    response
}

#[cfg(test)]
mod tests {
    use super::security_headers_middleware;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use axum::middleware::from_fn;
    use axum::routing::get;
    use axum::Router;
    use tower::ServiceExt;

    #[tokio::test]
    async fn adds_browser_headers_and_disables_sensitive_api_caching() {
        let app = Router::new()
            .route("/api/admin/keys", get(|| async { StatusCode::OK }))
            .layer(from_fn(security_headers_middleware));
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/api/admin/keys")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.headers()["x-content-type-options"], "nosniff");
        assert_eq!(response.headers()["x-frame-options"], "DENY");
        assert_eq!(response.headers()["referrer-policy"], "no-referrer");
        assert_eq!(response.headers()["cache-control"], "no-store");
    }
}
