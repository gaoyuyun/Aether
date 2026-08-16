mod dispatch;
pub(crate) mod duplicates;
pub(crate) mod errors;
pub(crate) mod provisioning;
pub(crate) mod quota;
pub(crate) mod runtime;
pub(crate) mod state;

pub(crate) use self::dispatch::maybe_build_local_admin_provider_oauth_response;

pub(crate) fn format_provider_oauth_task_timestamps(payload: &mut serde_json::Value) {
    let Some(payload) = payload.as_object_mut() else {
        return;
    };
    for field in ["created_at", "started_at", "finished_at", "updated_at"] {
        let timestamp = payload.get(field).and_then(serde_json::Value::as_u64);
        payload.insert(
            field.to_string(),
            timestamp
                .and_then(crate::handlers::shared::unix_secs_to_rfc3339)
                .map(serde_json::Value::String)
                .unwrap_or(serde_json::Value::Null),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::format_provider_oauth_task_timestamps;
    use serde_json::json;

    #[test]
    fn formats_task_timestamps_without_changing_internal_state_shape() {
        let mut payload = json!({
            "created_at": 1_700_000_001u64,
            "started_at": 1_700_000_002u64,
            "finished_at": null,
            "updated_at": 1_700_000_004u64,
        });

        format_provider_oauth_task_timestamps(&mut payload);

        assert_eq!(payload["created_at"], "2023-11-14T22:13:21Z");
        assert_eq!(payload["started_at"], "2023-11-14T22:13:22Z");
        assert!(payload["finished_at"].is_null());
        assert_eq!(payload["updated_at"], "2023-11-14T22:13:24Z");
    }
}
