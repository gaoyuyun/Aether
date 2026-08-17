use axum::http::StatusCode;
use axum::routing::{any, get, post};
use axum::Router;

use crate::async_task::{
    cancel_video_task, get_video_task_detail, get_video_task_stats, get_video_task_video,
    list_video_tasks,
};
use crate::audit::{get_auth_api_key_snapshot, get_decision_trace, get_request_candidate_trace};
use crate::hooks::{get_request_audit_bundle, get_request_usage_audit};
use crate::router::metrics;
use crate::state::AppState;

pub(crate) fn mount_operational_routes(router: Router<AppState>) -> Router<AppState> {
    router
        .route("/_gateway/metrics", get(metrics))
        .route("/_gateway/async-tasks/video-tasks", get(list_video_tasks))
        .route(
            "/_gateway/async-tasks/video-tasks/stats",
            get(get_video_task_stats),
        )
        .route(
            "/_gateway/async-tasks/video-tasks/{task_id}/video",
            get(get_video_task_video),
        )
        .route(
            "/_gateway/async-tasks/video-tasks/{task_id}/cancel",
            post(cancel_video_task),
        )
        .route(
            "/_gateway/async-tasks/video-tasks/{task_id}",
            get(get_video_task_detail),
        )
        .route(
            "/_gateway/audit/auth/users/{user_id}/api-keys/{api_key_id}",
            get(get_auth_api_key_snapshot),
        )
        .route(
            "/_gateway/audit/decision-trace/{request_id}",
            get(get_decision_trace),
        )
        .route(
            "/_gateway/audit/request-candidates/{request_id}",
            get(get_request_candidate_trace),
        )
        .route(
            "/_gateway/audit/request-audit/{request_id}",
            get(get_request_audit_bundle),
        )
        .route(
            "/_gateway/audit/request-usage/{request_id}",
            get(get_request_usage_audit),
        )
}

pub(crate) fn mount_disabled_operational_routes(router: Router<AppState>) -> Router<AppState> {
    router
        .route("/_gateway/metrics", any(disabled_operational_route))
        .route(
            "/_gateway/async-tasks/{*path}",
            any(disabled_operational_route),
        )
        .route("/_gateway/audit/{*path}", any(disabled_operational_route))
}

async fn disabled_operational_route() -> StatusCode {
    StatusCode::NOT_FOUND
}
