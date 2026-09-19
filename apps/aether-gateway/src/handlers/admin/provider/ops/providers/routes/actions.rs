use super::super::actions::{
    admin_provider_ops_is_valid_action_type, admin_provider_ops_local_action_response,
};
use super::super::balance_cache::{
    admin_provider_ops_balance_now_unix_secs, apply_admin_provider_ops_balance_attempt,
    attach_admin_provider_ops_balance_meta, build_admin_provider_ops_balance_response,
    read_admin_provider_ops_balance_snapshot, write_admin_provider_ops_balance_snapshot,
};
use super::super::balance_refresh::{
    admin_provider_ops_balance_refresh_state_label, enqueue_admin_provider_ops_balance_refresh,
    AdminProviderOpsBalanceRefreshTrigger,
};
use super::super::config::admin_provider_ops_config_object;
use super::super::support::AdminProviderOpsExecuteActionRequest;
use super::super::verify::{
    with_admin_provider_ops_request_timeouts, ADMIN_PROVIDER_OPS_BALANCE_QUERY_TIMEOUTS,
};
use crate::handlers::admin::request::AdminAppState;
use crate::GatewayError;
use aether_data_contracts::repository::provider_catalog::{
    StoredProviderCatalogEndpoint, StoredProviderCatalogProvider,
};
use axum::{
    body::{Body, Bytes},
    http,
    response::{IntoResponse, Response},
    Json,
};
use serde_json::json;

pub(super) async fn handle_admin_provider_ops_action(
    state: &AdminAppState<'_>,
    provider_id: &str,
    route_kind: &str,
    action_route: Option<&(String, String)>,
    query_string: Option<&str>,
    request_body: Option<&Bytes>,
) -> Result<Option<Response<Body>>, GatewayError> {
    let action_type = if route_kind == "provider_checkin" {
        "checkin".to_string()
    } else if matches!(
        route_kind,
        "get_provider_balance" | "refresh_provider_balance"
    ) {
        "query_balance".to_string()
    } else {
        let Some((_, action_type)) = action_route else {
            return Ok(None);
        };
        if !admin_provider_ops_is_valid_action_type(action_type) {
            return Ok(Some(
                (
                    http::StatusCode::BAD_REQUEST,
                    Json(json!({ "detail": format!("无效的操作类型: {action_type}") })),
                )
                    .into_response(),
            ));
        }
        action_type.clone()
    };

    let request_config = if route_kind == "execute_provider_action" {
        match request_body {
            Some(body) if !body.is_empty() => {
                let raw_value = match serde_json::from_slice::<serde_json::Value>(body) {
                    Ok(raw_value) => raw_value,
                    Err(_) => {
                        return Ok(Some(bad_request_detail_response(
                            "请求体必须是合法的 JSON 对象",
                        )));
                    }
                };
                let payload =
                    match serde_json::from_value::<AdminProviderOpsExecuteActionRequest>(raw_value)
                    {
                        Ok(payload) => payload,
                        Err(_) => {
                            return Ok(Some(bad_request_detail_response(
                                "请求体必须是合法的 JSON 对象",
                            )));
                        }
                    };
                payload.config
            }
            _ => None,
        }
    } else {
        None
    };

    let provider_ids = [provider_id.to_string()];
    let providers = state
        .read_provider_catalog_providers_by_ids(&provider_ids)
        .await?;
    let provider = providers.first();
    let endpoints = if provider.is_some() {
        state
            .list_provider_catalog_endpoints_by_provider_ids(&provider_ids)
            .await?
    } else {
        Vec::new()
    };
    let ops_configured =
        provider.is_some_and(|provider| admin_provider_ops_config_object(provider).is_some());
    let payload =
        if action_type == "query_balance" && route_kind == "get_provider_balance" && ops_configured
        {
            let refresh_requested = query_param_bool(query_string, "refresh", true);
            let snapshot = read_admin_provider_ops_balance_snapshot(state, provider_id).await;
            match snapshot {
                Some(snapshot) => {
                    if refresh_requested {
                        enqueue_admin_provider_ops_balance_refresh(
                            state,
                            provider_id,
                            AdminProviderOpsBalanceRefreshTrigger::Manual,
                        );
                    }
                    build_admin_provider_ops_balance_response(
                        Some(&snapshot),
                        admin_provider_ops_balance_refresh_state_label(state, provider_id),
                        admin_provider_ops_balance_now_unix_secs(),
                    )
                }
                None if refresh_requested => {
                    enqueue_admin_provider_ops_balance_refresh(
                        state,
                        provider_id,
                        AdminProviderOpsBalanceRefreshTrigger::Manual,
                    );
                    build_admin_provider_ops_balance_response(
                        None,
                        admin_provider_ops_balance_refresh_state_label(state, provider_id),
                        admin_provider_ops_balance_now_unix_secs(),
                    )
                }
                // `refresh=false` without a snapshot: the caller wants a value now.
                None => {
                    query_admin_provider_ops_balance_now(
                        state,
                        provider_id,
                        provider,
                        &endpoints,
                        request_config.as_ref(),
                    )
                    .await
                }
            }
        } else if action_type == "query_balance"
            && route_kind == "refresh_provider_balance"
            && ops_configured
        {
            query_admin_provider_ops_balance_now(
                state,
                provider_id,
                provider,
                &endpoints,
                request_config.as_ref(),
            )
            .await
        } else {
            admin_provider_ops_local_action_response(
                state,
                provider_id,
                provider,
                &endpoints,
                &action_type,
                request_config.as_ref(),
            )
            .await
        };

    Ok(Some(Json(payload).into_response()))
}

/// Queries the upstream in the request and records the outcome in the snapshot
/// so the page and the background refresher see the same value afterwards.
async fn query_admin_provider_ops_balance_now(
    state: &AdminAppState<'_>,
    provider_id: &str,
    provider: Option<&StoredProviderCatalogProvider>,
    endpoints: &[StoredProviderCatalogEndpoint],
    request_config: Option<&serde_json::Map<String, serde_json::Value>>,
) -> serde_json::Value {
    let previous = read_admin_provider_ops_balance_snapshot(state, provider_id).await;
    let mut payload = with_admin_provider_ops_request_timeouts(
        ADMIN_PROVIDER_OPS_BALANCE_QUERY_TIMEOUTS,
        admin_provider_ops_local_action_response(
            state,
            provider_id,
            provider,
            endpoints,
            "query_balance",
            request_config,
        ),
    )
    .await;
    let now_unix_secs = admin_provider_ops_balance_now_unix_secs();
    let snapshot = apply_admin_provider_ops_balance_attempt(
        previous.as_ref(),
        provider_id,
        &payload,
        now_unix_secs,
    );
    write_admin_provider_ops_balance_snapshot(state, &snapshot).await;
    attach_admin_provider_ops_balance_meta(
        &mut payload,
        Some(&snapshot),
        admin_provider_ops_balance_refresh_state_label(state, provider_id),
        now_unix_secs,
    );
    payload
}

fn query_param_bool(query: Option<&str>, key: &str, default: bool) -> bool {
    let Some(query) = query else {
        return default;
    };
    for (entry_key, value) in url::form_urlencoded::parse(query.as_bytes()) {
        if entry_key == key {
            let normalized = value.trim().to_ascii_lowercase();
            return matches!(normalized.as_str(), "1" | "true" | "yes" | "on");
        }
    }
    default
}

fn bad_request_detail_response(detail: &str) -> Response<Body> {
    (
        http::StatusCode::BAD_REQUEST,
        Json(json!({ "detail": detail })),
    )
        .into_response()
}
