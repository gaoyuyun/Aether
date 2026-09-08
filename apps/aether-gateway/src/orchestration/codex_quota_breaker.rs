//! Short-lived runtime circuit breaker for exhausted Codex accounts.
//!
//! Persisted provider-key quota snapshots remain the durable source of truth.
//! This module closes the interval between receiving a definitive WebSocket
//! `usage_limit_reached` event and every scheduler/cache replica observing the
//! persisted snapshot.  The account-scoped entry also protects pools that
//! contain more than one catalog key for the same ChatGPT account.

use std::collections::BTreeMap;

use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};
use tracing::{info, warn};

use crate::clock::current_unix_secs;
use crate::{AppState, GatewayError};

const CODEX_QUOTA_BREAKER_KEY_PREFIX: &str = "aether:codex:quota-breaker:v1";
const CODEX_QUOTA_BREAKER_FALLBACK_TTL_SECONDS: u64 = 300;
const CODEX_QUOTA_BREAKER_MAX_TTL_SECONDS: u64 = 31 * 24 * 60 * 60;

/// Installs immediate runtime exclusions for a definitive Codex quota
/// exhaustion signal.  When RuntimeState is backed by Redis the exclusions are
/// shared by every gateway node; the in-memory backend still protects the
/// current node.
pub(crate) async fn install_codex_quota_exhaustion_breaker(
    state: &AppState,
    report_context: Option<&Value>,
    quota_metadata: &Value,
    source: &str,
) -> Result<bool, GatewayError> {
    if !aether_admin::provider::quota::codex_rate_limit_metadata_exhausted(quota_metadata) {
        return Ok(false);
    }

    let keys = codex_quota_breaker_keys_from_report_context(report_context);
    if keys.is_empty() {
        return Ok(false);
    }

    let now_unix_secs = current_unix_secs();
    let (ttl_seconds, reset_at_unix_secs) = codex_quota_breaker_ttl(quota_metadata, now_unix_secs);
    let value = json!({
        "version": 1,
        "observed_at": now_unix_secs,
        "reset_at": reset_at_unix_secs,
        "source": source,
    })
    .to_string();

    for key in &keys {
        state.runtime_kv_setex(key, &value, ttl_seconds).await?;
    }
    info!(
        event_name = "codex_account_quota_breaker_installed",
        log_type = "event",
        scope_count = keys.len(),
        ttl_seconds,
        reset_at_unix_secs = ?reset_at_unix_secs,
        source,
        "gateway installed immediate Codex quota exhaustion exclusions"
    );

    Ok(true)
}

/// Returns whether a planned Codex request is temporarily blocked by a
/// definitive account quota signal that has not yet been observed in the
/// durable provider catalog.
pub(crate) async fn codex_quota_breaker_blocks_candidate(
    state: &AppState,
    provider_type: Option<&str>,
    key_id: Option<&str>,
    provider_request_headers: &BTreeMap<String, String>,
) -> Result<bool, GatewayError> {
    if !provider_type.is_some_and(|value| value.trim().eq_ignore_ascii_case("codex")) {
        return Ok(false);
    }

    for key in codex_quota_breaker_keys(
        key_id,
        codex_account_id_from_headers(provider_request_headers),
    ) {
        if state.runtime_kv_exists(&key).await? {
            return Ok(true);
        }
    }
    Ok(false)
}

fn codex_quota_breaker_keys_from_report_context(report_context: Option<&Value>) -> Vec<String> {
    let key_id = report_context
        .and_then(|context| context.get("key_id"))
        .and_then(Value::as_str);
    let account_id = report_context
        .and_then(|context| context.get("provider_request_headers"))
        .and_then(Value::as_object)
        .and_then(account_id_from_header_object);
    codex_quota_breaker_keys(key_id, account_id)
}

fn codex_quota_breaker_keys(key_id: Option<&str>, account_id: Option<&str>) -> Vec<String> {
    let mut keys = Vec::with_capacity(2);
    if let Some(account_id) = normalize_identifier(account_id) {
        keys.push(codex_quota_breaker_runtime_key("account", account_id));
    }
    if let Some(key_id) = normalize_identifier(key_id) {
        keys.push(codex_quota_breaker_runtime_key("key", key_id));
    }
    keys
}

fn codex_quota_breaker_runtime_key(scope: &str, identifier: &str) -> String {
    format!(
        "{CODEX_QUOTA_BREAKER_KEY_PREFIX}:{scope}:{}",
        opaque_identifier(identifier)
    )
}

fn opaque_identifier(identifier: &str) -> String {
    let digest = Sha256::digest(identifier.as_bytes());
    let mut encoded = String::with_capacity(digest.len().saturating_mul(2));
    for byte in digest {
        use std::fmt::Write as _;
        let _ = write!(encoded, "{byte:02x}");
    }
    encoded
}

fn normalize_identifier(value: Option<&str>) -> Option<&str> {
    value.map(str::trim).filter(|value| !value.is_empty())
}

pub(crate) fn codex_account_id_from_headers(headers: &BTreeMap<String, String>) -> Option<&str> {
    headers.iter().find_map(|(name, value)| {
        name.trim()
            .eq_ignore_ascii_case("chatgpt-account-id")
            .then_some(value.as_str())
            .and_then(|value| normalize_identifier(Some(value)))
    })
}

fn account_id_from_header_object(headers: &Map<String, Value>) -> Option<&str> {
    headers.iter().find_map(|(name, value)| {
        name.trim()
            .eq_ignore_ascii_case("chatgpt-account-id")
            .then(|| value.as_str())
            .flatten()
            .and_then(|value| normalize_identifier(Some(value)))
    })
}

fn codex_quota_breaker_ttl(quota_metadata: &Value, now_unix_secs: u64) -> (u64, Option<u64>) {
    let reset_at = codex_quota_exhaustion_reset_at(quota_metadata, now_unix_secs);
    let ttl_seconds = reset_at
        .and_then(|reset_at| reset_at.checked_sub(now_unix_secs))
        .filter(|ttl| *ttl > 0)
        .unwrap_or(CODEX_QUOTA_BREAKER_FALLBACK_TTL_SECONDS)
        .clamp(1, CODEX_QUOTA_BREAKER_MAX_TTL_SECONDS);

    (ttl_seconds, reset_at)
}

/// Returns the latest reset deadline required for a currently exhausted Codex
/// quota window.  It is shared by the distributed breaker and the per-socket
/// retry exclusion so both stop excluding the account at the same time.
pub(crate) fn codex_quota_exhaustion_reset_at(
    quota_metadata: &Value,
    now_unix_secs: u64,
) -> Option<u64> {
    codex_quota_exhaustion_reset_at_for_prefixes(
        quota_metadata,
        now_unix_secs,
        &["primary", "secondary"],
    )
}

/// Returns the reset deadline for an exhausted model-scoped Codex window without promoting that
/// window to account-wide exhaustion.
pub(crate) fn codex_model_quota_exhaustion_reset_at(
    quota_metadata: &Value,
    now_unix_secs: u64,
) -> Option<u64> {
    codex_quota_exhaustion_reset_at_for_prefixes(
        quota_metadata,
        now_unix_secs,
        &["spark_primary", "spark_secondary"],
    )
}

fn codex_quota_exhaustion_reset_at_for_prefixes(
    quota_metadata: &Value,
    now_unix_secs: u64,
    window_prefixes: &[&str],
) -> Option<u64> {
    let Some(metadata) = quota_metadata.as_object() else {
        return None;
    };

    let exhausted_windows = window_prefixes
        .iter()
        .copied()
        .filter(|prefix| codex_window_is_exhausted(metadata, prefix))
        .collect::<Vec<_>>();
    let prefixes = if exhausted_windows.is_empty() {
        window_prefixes.to_vec()
    } else {
        exhausted_windows
    };

    prefixes
        .iter()
        .filter_map(|prefix| codex_window_reset_at(metadata, prefix, now_unix_secs))
        .filter(|reset_at| *reset_at > now_unix_secs)
        .max()
}

fn codex_window_is_exhausted(metadata: &Map<String, Value>, prefix: &str) -> bool {
    let used_percent = metadata
        .get(&format!("{prefix}_used_percent"))
        .and_then(aether_admin::provider::quota::coerce_json_f64);
    used_percent.is_some_and(|used_percent| used_percent >= 100.0 - 1e-6)
}

fn codex_window_reset_at(
    metadata: &Map<String, Value>,
    prefix: &str,
    observed_at_unix_secs: u64,
) -> Option<u64> {
    metadata
        .get(&format!("{prefix}_reset_at"))
        .and_then(aether_admin::provider::quota::coerce_json_u64)
        .filter(|reset_at| *reset_at > observed_at_unix_secs)
        .or_else(|| {
            metadata
                .get(&format!("{prefix}_reset_after_seconds"))
                .and_then(aether_admin::provider::quota::coerce_json_u64)
                .and_then(|seconds| observed_at_unix_secs.checked_add(seconds))
        })
}

pub(crate) fn log_codex_quota_breaker_install_failure(error: &GatewayError) {
    warn!(
        event_name = "codex_account_quota_breaker_install_failed",
        log_type = "ops",
        error = ?error,
        "gateway could not install the immediate Codex quota exhaustion breaker"
    );
}

pub(crate) fn log_codex_quota_breaker_check_failure(error: &GatewayError) {
    warn!(
        event_name = "codex_account_quota_breaker_check_failed",
        log_type = "ops",
        transport = "websocket",
        websocket = true,
        error = ?error,
        "gateway could not check the immediate Codex quota exhaustion breaker; allowing candidate selection"
    );
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use serde_json::json;

    use super::{
        codex_quota_breaker_blocks_candidate, codex_quota_breaker_keys,
        codex_quota_breaker_keys_from_report_context, codex_quota_breaker_ttl,
        codex_quota_exhaustion_reset_at, install_codex_quota_exhaustion_breaker,
    };
    use crate::AppState;

    #[test]
    fn account_scope_is_shared_across_catalog_keys() {
        let first = codex_quota_breaker_keys(Some("key-first"), Some("account-123"));
        let second = codex_quota_breaker_keys(Some("key-second"), Some("account-123"));

        assert_eq!(first.first(), second.first());
        assert_ne!(first.last(), second.last());
        assert!(!first.first().is_some_and(|key| key.contains("account-123")));
    }

    #[test]
    fn report_context_and_planned_headers_use_the_same_account_scope() {
        let report_context = json!({
            "key_id": "key-first",
            "provider_request_headers": {
                "ChatGPT-Account-ID": "account-123"
            }
        });
        let context_keys = codex_quota_breaker_keys_from_report_context(Some(&report_context));
        let planned_keys = codex_quota_breaker_keys(Some("key-second"), Some("account-123"));

        assert_eq!(context_keys.first(), planned_keys.first());
    }

    #[test]
    fn ttl_uses_the_exhausted_window_reset_deadline() {
        let (ttl, reset_at) = codex_quota_breaker_ttl(
            &json!({
                "primary_used_percent": 100,
                "primary_reset_at": 1_000,
                "secondary_used_percent": 10,
                "secondary_reset_at": 2_000,
            }),
            500,
        );

        assert_eq!(ttl, 500);
        assert_eq!(reset_at, Some(1_000));
    }

    #[test]
    fn ttl_falls_back_when_an_error_has_no_reset_metadata() {
        let (ttl, reset_at) = codex_quota_breaker_ttl(&json!({"allowed": false}), 500);

        assert_eq!(ttl, 300);
        assert_eq!(reset_at, None);
    }

    #[test]
    fn reset_deadline_uses_relative_metadata_when_absolute_reset_is_stale() {
        assert_eq!(
            codex_quota_exhaustion_reset_at(
                &json!({
                    "primary_used_percent": 100,
                    "primary_reset_at": 999,
                    "primary_reset_after_seconds": 120,
                }),
                1_000,
            ),
            Some(1_120)
        );
    }

    #[test]
    fn account_header_matching_is_case_insensitive() {
        let headers =
            BTreeMap::from([("CHATGPT-ACCOUNT-ID".to_string(), "account-123".to_string())]);
        let keys = codex_quota_breaker_keys(
            Some("key-first"),
            headers.iter().find_map(|(name, value)| {
                name.eq_ignore_ascii_case("chatgpt-account-id")
                    .then_some(value.as_str())
            }),
        );

        assert_eq!(keys.len(), 2);
    }

    #[tokio::test]
    async fn exhausted_account_immediately_blocks_a_different_catalog_key() {
        let state = AppState::new().expect("gateway state should build");
        let report_context = json!({
            "key_id": "key-first",
            "provider_request_headers": {
                "ChatGPT-Account-ID": "account-123"
            }
        });
        let quota_metadata = json!({
            "allowed": false,
            "limit_reached": true,
            "primary_used_percent": 100,
            "primary_reset_after_seconds": 60,
        });

        assert!(install_codex_quota_exhaustion_breaker(
            &state,
            Some(&report_context),
            &quota_metadata,
            "test",
        )
        .await
        .expect("breaker installation should succeed"));

        let same_account_other_key =
            BTreeMap::from([("chatgpt-account-id".to_string(), "account-123".to_string())]);
        assert!(codex_quota_breaker_blocks_candidate(
            &state,
            Some("codex"),
            Some("key-second"),
            &same_account_other_key,
        )
        .await
        .expect("breaker lookup should succeed"));

        let other_account =
            BTreeMap::from([("chatgpt-account-id".to_string(), "account-456".to_string())]);
        assert!(!codex_quota_breaker_blocks_candidate(
            &state,
            Some("codex"),
            Some("key-second"),
            &other_account,
        )
        .await
        .expect("breaker lookup should succeed"));
    }
}
