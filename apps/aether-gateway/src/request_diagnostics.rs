use std::collections::BTreeMap;
use std::future::Future;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use aether_data::DatabasePoolSummary;
use aether_data_contracts::repository::usage::REQUEST_ACCEPTED_AT_UNIX_MS_METADATA_KEY;
use serde_json::{Map, Value};

tokio::task_local! {
    static REQUEST_DIAGNOSTICS: Arc<RequestDiagnostics>;
}

#[derive(Debug, Default)]
pub(crate) struct RequestDiagnostics {
    inner: Mutex<RequestDiagnosticsInner>,
}

#[derive(Debug, Default)]
struct RequestDiagnosticsInner {
    request_accepted_at: Option<Instant>,
    /// Wall-clock twin of `request_accepted_at`, so the same origin can be persisted on the usage
    /// row and compared against `Date.now()` by readers that never see the monotonic instant.
    request_accepted_at_unix_ms: Option<u64>,
    next_candidate_indices: BTreeMap<String, u32>,
    db_operations: BTreeMap<&'static str, DbOperationTiming>,
    db_pool: Option<DbPoolObservation>,
}

#[derive(Debug, Clone, Copy, Default)]
struct DbOperationTiming {
    count: u64,
    sum_ms: u64,
    max_ms: u64,
}

#[derive(Debug, Clone, Copy)]
struct DbPoolObservation {
    max_checked_out: u64,
    max_pool_size: u64,
    min_idle: u64,
    max_connections: u64,
    max_usage_rate_x100: u64,
}

impl RequestDiagnostics {
    fn record_request_accepted_at(&self, accepted_at: Instant) {
        let accepted_at_unix_ms = crate::clock::current_unix_ms()
            .saturating_sub(accepted_at.elapsed().as_millis().min(u128::from(u64::MAX)) as u64);
        let Ok(mut inner) = self.inner.lock() else {
            return;
        };
        inner.request_accepted_at = Some(accepted_at);
        inner.request_accepted_at_unix_ms = Some(accepted_at_unix_ms);
    }

    pub(crate) fn request_accepted_at_unix_ms(&self) -> Option<u64> {
        let Ok(inner) = self.inner.lock() else {
            return None;
        };
        inner.request_accepted_at_unix_ms
    }

    pub(crate) fn request_accepted_at(&self) -> Option<Instant> {
        let Ok(inner) = self.inner.lock() else {
            return None;
        };
        inner.request_accepted_at
    }

    pub(crate) fn request_accepted_elapsed_ms(&self) -> Option<u64> {
        self.request_accepted_at()
            .map(|accepted_at| accepted_at.elapsed().as_millis().min(u128::from(u64::MAX)) as u64)
    }

    fn request_elapsed_ms_at(&self, observed_at: Instant) -> Option<u64> {
        self.request_accepted_at().map(|accepted_at| {
            observed_at
                .saturating_duration_since(accepted_at)
                .as_millis()
                .min(u128::from(u64::MAX)) as u64
        })
    }

    fn record_db_timing_ms(&self, operation: &'static str, elapsed_ms: u64) {
        let Ok(mut inner) = self.inner.lock() else {
            return;
        };
        let timing = inner.db_operations.entry(operation).or_default();
        timing.count = timing.count.saturating_add(1);
        timing.sum_ms = timing.sum_ms.saturating_add(elapsed_ms);
        timing.max_ms = timing.max_ms.max(elapsed_ms);
    }

    fn record_db_pool_summary(&self, summary: DatabasePoolSummary) {
        let observation = DbPoolObservation {
            max_checked_out: summary.checked_out as u64,
            max_pool_size: summary.pool_size as u64,
            min_idle: summary.idle as u64,
            max_connections: u64::from(summary.max_connections),
            max_usage_rate_x100: (summary.usage_rate * 100.0).max(0.0).round() as u64,
        };
        let Ok(mut inner) = self.inner.lock() else {
            return;
        };
        inner.db_pool = Some(match inner.db_pool {
            Some(existing) => DbPoolObservation {
                max_checked_out: existing.max_checked_out.max(observation.max_checked_out),
                max_pool_size: existing.max_pool_size.max(observation.max_pool_size),
                min_idle: existing.min_idle.min(observation.min_idle),
                max_connections: existing.max_connections.max(observation.max_connections),
                max_usage_rate_x100: existing
                    .max_usage_rate_x100
                    .max(observation.max_usage_rate_x100),
            },
            None => observation,
        });
    }

    pub(crate) fn db_timings_metadata(&self) -> Option<Value> {
        let Ok(inner) = self.inner.lock() else {
            return None;
        };
        if inner.db_operations.is_empty() && inner.db_pool.is_none() {
            return None;
        }

        let mut total_count = 0_u64;
        let mut query_total_ms = 0_u64;
        let mut query_max_ms = 0_u64;
        let mut operations = Map::new();
        for (operation, timing) in &inner.db_operations {
            total_count = total_count.saturating_add(timing.count);
            query_total_ms = query_total_ms.saturating_add(timing.sum_ms);
            query_max_ms = query_max_ms.max(timing.max_ms);
            operations.insert(
                (*operation).to_string(),
                Value::Object(Map::from_iter([
                    ("count".to_string(), Value::from(timing.count)),
                    ("sum".to_string(), Value::from(timing.sum_ms)),
                    ("max".to_string(), Value::from(timing.max_ms)),
                ])),
            );
        }

        let mut metadata = Map::new();
        if !operations.is_empty() {
            metadata.insert("query_count".to_string(), Value::from(total_count));
            metadata.insert("query_total".to_string(), Value::from(query_total_ms));
            metadata.insert("query_max".to_string(), Value::from(query_max_ms));
            metadata.insert("operations".to_string(), Value::Object(operations));
        }
        if let Some(pool) = inner.db_pool {
            metadata.insert(
                "pool".to_string(),
                Value::Object(Map::from_iter([
                    (
                        "max_checked_out".to_string(),
                        Value::from(pool.max_checked_out),
                    ),
                    ("max_pool_size".to_string(), Value::from(pool.max_pool_size)),
                    ("min_idle".to_string(), Value::from(pool.min_idle)),
                    (
                        "max_connections".to_string(),
                        Value::from(pool.max_connections),
                    ),
                    (
                        "max_usage_rate".to_string(),
                        Value::from(pool.max_usage_rate_x100 as f64 / 100.0),
                    ),
                ])),
            );
        }

        Some(Value::Object(metadata))
    }
}

pub(crate) async fn scope_request_diagnostics<F>(future: F) -> F::Output
where
    F: Future,
{
    scope_request_diagnostics_with(Some(Arc::new(RequestDiagnostics::default())), future).await
}

pub(crate) async fn scope_request_diagnostics_with<F>(
    diagnostics: Option<Arc<RequestDiagnostics>>,
    future: F,
) -> F::Output
where
    F: Future,
{
    match diagnostics {
        Some(diagnostics) => REQUEST_DIAGNOSTICS.scope(diagnostics, future).await,
        None => future.await,
    }
}

pub(crate) fn current_request_diagnostics() -> Option<Arc<RequestDiagnostics>> {
    REQUEST_DIAGNOSTICS.try_with(Arc::clone).ok()
}

/// Candidate indices are request-wide, while each execution path ranks its
/// own candidates from zero. Keep trace slots distinct across those paths.
pub(crate) fn allocate_request_candidate_index(request_id: &str, fallback: u32) -> u32 {
    let Some(diagnostics) = current_request_diagnostics() else {
        return fallback;
    };
    let mut inner = diagnostics
        .inner
        .lock()
        .expect("request candidate indices lock");
    let next = inner
        .next_candidate_indices
        .entry(request_id.to_string())
        .or_default();
    let index = (*next).max(fallback);
    *next = index.saturating_add(1);
    index
}

pub(crate) fn record_request_accepted_at(accepted_at: Instant) {
    if let Some(diagnostics) = current_request_diagnostics() {
        diagnostics.record_request_accepted_at(accepted_at);
    }
}

/// Stamps the request-level clock origin onto an attempt's report context.
///
/// Every lifecycle usage write (pending, first byte, terminal) derives its metadata from the
/// attempt's report context, so stamping it once per attempt is what lets the usage row carry
/// `request_accepted_at_unix_ms` from its very first write. The value is identical for every
/// attempt of the request, so a later attempt rewriting the row keeps the same origin.
pub(crate) fn attach_current_request_accepted_at_to_report_context(
    report_context: &mut Option<Value>,
) {
    let Some(accepted_at_unix_ms) = current_request_diagnostics()
        .and_then(|diagnostics| diagnostics.request_accepted_at_unix_ms())
    else {
        return;
    };
    let object = match report_context.take() {
        Some(Value::Object(object)) => object,
        Some(other) => Map::from_iter([("seed".to_string(), other)]),
        None => Map::new(),
    };
    let mut object = object;
    object.insert(
        REQUEST_ACCEPTED_AT_UNIX_MS_METADATA_KEY.to_string(),
        Value::from(accepted_at_unix_ms),
    );
    *report_context = Some(Value::Object(object));
}

pub(crate) async fn observe_db_operation<F>(
    operation: &'static str,
    pool_summary: Option<DatabasePoolSummary>,
    future: F,
) -> F::Output
where
    F: Future,
{
    if let Some(summary) = pool_summary {
        record_db_pool_summary(summary);
    }
    let started_at = Instant::now();
    let output = future.await;
    record_db_timing_ms(operation, started_at.elapsed().as_millis() as u64);
    output
}

pub(crate) fn record_db_timing_ms(operation: &'static str, elapsed_ms: u64) {
    if let Some(diagnostics) = current_request_diagnostics() {
        diagnostics.record_db_timing_ms(operation, elapsed_ms);
    }
}

pub(crate) fn record_db_pool_summary(summary: DatabasePoolSummary) {
    if let Some(diagnostics) = current_request_diagnostics() {
        diagnostics.record_db_pool_summary(summary);
    }
}

pub(crate) fn attach_request_diagnostics_to_report_context(
    report_context: Option<Value>,
    diagnostics: Option<&Arc<RequestDiagnostics>>,
) -> Option<Value> {
    let db_timings_ms = diagnostics.and_then(|diagnostics| diagnostics.db_timings_metadata());
    let end_to_end_time_ms =
        diagnostics.and_then(|diagnostics| diagnostics.request_accepted_elapsed_ms());
    let request_accepted_at_unix_ms =
        diagnostics.and_then(|diagnostics| diagnostics.request_accepted_at_unix_ms());
    if db_timings_ms.is_none() && end_to_end_time_ms.is_none() {
        return report_context;
    }

    let mut object = match report_context {
        Some(Value::Object(object)) => object,
        Some(other) => Map::from_iter([("seed".to_string(), other)]),
        None => Map::new(),
    };
    if let Some(db_timings_ms) = db_timings_ms {
        object.insert("db_timings_ms".to_string(), db_timings_ms);
    }
    if let Some(end_to_end_time_ms) = end_to_end_time_ms {
        object.insert(
            "end_to_end_time_ms".to_string(),
            Value::from(end_to_end_time_ms),
        );
    }
    if let Some(request_accepted_at_unix_ms) = request_accepted_at_unix_ms {
        object.insert(
            REQUEST_ACCEPTED_AT_UNIX_MS_METADATA_KEY.to_string(),
            Value::from(request_accepted_at_unix_ms),
        );
    }
    Some(Value::Object(object))
}

pub(crate) fn attach_request_diagnostics_and_candidate_timing_to_report_context(
    report_context: Option<Value>,
    diagnostics: Option<&Arc<RequestDiagnostics>>,
    candidate_elapsed_ms: Option<u64>,
    candidate_ttfb_ms: Option<u64>,
) -> Option<Value> {
    let end_to_end_time_ms =
        diagnostics.and_then(|diagnostics| diagnostics.request_accepted_elapsed_ms());
    let mut report_context =
        attach_request_diagnostics_to_report_context(report_context, diagnostics);
    let Some(end_to_end_first_byte_time_ms) =
        end_to_end_first_byte_time_ms(end_to_end_time_ms, candidate_elapsed_ms, candidate_ttfb_ms)
    else {
        return report_context;
    };

    let object = report_context
        .get_or_insert_with(|| Value::Object(Map::new()))
        .as_object_mut()?;
    object.insert(
        "end_to_end_first_byte_time_ms".to_string(),
        Value::from(end_to_end_first_byte_time_ms),
    );
    report_context
}

pub(crate) fn attach_request_diagnostics_and_candidate_start_timing_to_report_context(
    report_context: Option<Value>,
    diagnostics: Option<&Arc<RequestDiagnostics>>,
    candidate_started_at: Option<Instant>,
    candidate_ttfb_ms: Option<u64>,
) -> Option<Value> {
    let end_to_end_time_ms =
        diagnostics.and_then(|diagnostics| diagnostics.request_accepted_elapsed_ms());
    let candidate_started_elapsed_ms = diagnostics
        .zip(candidate_started_at)
        .and_then(|(diagnostics, started_at)| diagnostics.request_elapsed_ms_at(started_at));
    let mut report_context =
        attach_request_diagnostics_to_report_context(report_context, diagnostics);
    let Some(end_to_end_first_byte_time_ms) = end_to_end_first_byte_time_ms_from_candidate_start(
        end_to_end_time_ms,
        candidate_started_elapsed_ms,
        candidate_ttfb_ms,
    ) else {
        return report_context;
    };

    let object = report_context
        .get_or_insert_with(|| Value::Object(Map::new()))
        .as_object_mut()?;
    object.insert(
        "end_to_end_first_byte_time_ms".to_string(),
        Value::from(end_to_end_first_byte_time_ms),
    );
    report_context
}

pub(crate) fn attach_current_request_diagnostics_and_candidate_timing_to_report_context(
    report_context: Option<&Value>,
    candidate_elapsed_ms: Option<u64>,
    candidate_ttfb_ms: Option<u64>,
) -> Option<Value> {
    let diagnostics = current_request_diagnostics();
    attach_request_diagnostics_and_candidate_timing_to_report_context(
        report_context.cloned(),
        diagnostics.as_ref(),
        candidate_elapsed_ms,
        candidate_ttfb_ms,
    )
}

pub(crate) fn attach_current_request_diagnostics_and_candidate_start_timing_to_report_context(
    report_context: Option<&Value>,
    candidate_started_at: Instant,
    candidate_ttfb_ms: Option<u64>,
) -> Option<Value> {
    let diagnostics = current_request_diagnostics();
    attach_request_diagnostics_and_candidate_start_timing_to_report_context(
        report_context.cloned(),
        diagnostics.as_ref(),
        Some(candidate_started_at),
        candidate_ttfb_ms,
    )
}

fn end_to_end_first_byte_time_ms(
    end_to_end_time_ms: Option<u64>,
    candidate_elapsed_ms: Option<u64>,
    candidate_ttfb_ms: Option<u64>,
) -> Option<u64> {
    let end_to_end_time_ms = end_to_end_time_ms?;
    let candidate_elapsed_ms = candidate_elapsed_ms?;
    let candidate_ttfb_ms = candidate_ttfb_ms?;
    Some(
        end_to_end_time_ms
            .saturating_sub(candidate_elapsed_ms)
            .saturating_add(candidate_ttfb_ms)
            .min(end_to_end_time_ms),
    )
}

fn end_to_end_first_byte_time_ms_from_candidate_start(
    end_to_end_time_ms: Option<u64>,
    candidate_started_elapsed_ms: Option<u64>,
    candidate_ttfb_ms: Option<u64>,
) -> Option<u64> {
    let end_to_end_time_ms = end_to_end_time_ms?;
    let candidate_started_elapsed_ms = candidate_started_elapsed_ms?;
    let candidate_ttfb_ms = candidate_ttfb_ms?;
    Some(
        candidate_started_elapsed_ms
            .saturating_add(candidate_ttfb_ms)
            .min(end_to_end_time_ms),
    )
}

pub(crate) fn calibrate_candidate_first_byte_elapsed_ms(
    candidate_elapsed_at_result_ms: u64,
    execution_elapsed_ms: Option<u64>,
    execution_ttfb_ms: Option<u64>,
) -> Option<u64> {
    let execution_elapsed_ms = execution_elapsed_ms?;
    let execution_ttfb_ms = execution_ttfb_ms?;
    Some(
        candidate_elapsed_at_result_ms
            .saturating_sub(execution_elapsed_ms)
            .saturating_add(execution_ttfb_ms)
            .min(candidate_elapsed_at_result_ms),
    )
}

pub(crate) fn attach_current_request_diagnostics_to_report_context(
    report_context: Option<&Value>,
) -> Option<Value> {
    let diagnostics = current_request_diagnostics()?;
    attach_request_diagnostics_to_report_context(report_context.cloned(), Some(&diagnostics))
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    use serde_json::json;

    use super::{
        attach_current_request_accepted_at_to_report_context,
        attach_request_diagnostics_and_candidate_start_timing_to_report_context,
        calibrate_candidate_first_byte_elapsed_ms,
        end_to_end_first_byte_time_ms_from_candidate_start, record_request_accepted_at,
        scope_request_diagnostics, RequestDiagnostics,
    };

    #[test]
    fn candidate_first_byte_calibration_includes_pre_transport_wait() {
        assert_eq!(
            calibrate_candidate_first_byte_elapsed_ms(826, Some(626), Some(120)),
            Some(320)
        );
    }

    #[test]
    fn end_to_end_first_byte_matches_candidate_ttfb_without_prior_retry() {
        assert_eq!(
            end_to_end_first_byte_time_ms_from_candidate_start(Some(626), Some(0), Some(120)),
            Some(120)
        );
    }

    #[test]
    fn end_to_end_first_byte_includes_time_spent_before_successful_retry() {
        assert_eq!(
            end_to_end_first_byte_time_ms_from_candidate_start(
                Some(10_626),
                Some(10_000),
                Some(120),
            ),
            Some(10_120)
        );
    }

    #[test]
    fn explicit_diagnostics_add_end_to_end_and_first_byte_timing() {
        let diagnostics = Arc::new(RequestDiagnostics::default());
        let accepted_at = Instant::now() - Duration::from_millis(1_000);
        let candidate_started_at = accepted_at + Duration::from_millis(800);
        diagnostics.record_request_accepted_at(accepted_at);

        let context = attach_request_diagnostics_and_candidate_start_timing_to_report_context(
            Some(json!({"candidate_index": 1})),
            Some(&diagnostics),
            Some(candidate_started_at),
            Some(50),
        )
        .expect("diagnostics context should build");

        let end_to_end = context["end_to_end_time_ms"]
            .as_u64()
            .expect("end-to-end timing should exist");
        let first_byte = context["end_to_end_first_byte_time_ms"]
            .as_u64()
            .expect("first-byte timing should exist");
        assert!(end_to_end >= 1_000);
        assert_eq!(first_byte, 850);

        // The wall-clock origin lands next to the elapsed values so the persisted row can anchor
        // a live clock on the same instant the terminal end-to-end timing starts from.
        let accepted_unix_ms = context["request_accepted_at_unix_ms"]
            .as_u64()
            .expect("request accepted wall clock should exist");
        let now_unix_ms = crate::clock::current_unix_ms();
        assert!(now_unix_ms.saturating_sub(accepted_unix_ms) >= 1_000);
        assert!(now_unix_ms.saturating_sub(accepted_unix_ms) < 60_000);
    }

    #[tokio::test]
    async fn scoped_request_accepted_clock_is_stamped_onto_attempt_report_context() {
        scope_request_diagnostics(async {
            let mut report_context = Some(json!({"candidate_index": 0}));
            attach_current_request_accepted_at_to_report_context(&mut report_context);
            assert_eq!(
                report_context,
                Some(json!({"candidate_index": 0})),
                "nothing to stamp before the request is accepted"
            );

            let accepted_at = Instant::now() - Duration::from_millis(250);
            record_request_accepted_at(accepted_at);

            attach_current_request_accepted_at_to_report_context(&mut report_context);
            let stamped = report_context
                .as_ref()
                .and_then(|context| context.get("request_accepted_at_unix_ms"))
                .and_then(serde_json::Value::as_u64)
                .expect("accepted clock should be stamped");
            let now_unix_ms = crate::clock::current_unix_ms();
            assert!(now_unix_ms.saturating_sub(stamped) >= 250);
            assert!(now_unix_ms.saturating_sub(stamped) < 60_000);
            assert_eq!(report_context.as_ref().unwrap()["candidate_index"], 0);

            let mut empty_context = None;
            attach_current_request_accepted_at_to_report_context(&mut empty_context);
            assert_eq!(
                empty_context
                    .as_ref()
                    .and_then(|context| context.get("request_accepted_at_unix_ms"))
                    .and_then(serde_json::Value::as_u64),
                Some(stamped)
            );
        })
        .await;
    }

    #[test]
    fn unscoped_attempt_report_context_is_left_untouched() {
        let mut report_context = Some(json!({"candidate_index": 1}));
        attach_current_request_accepted_at_to_report_context(&mut report_context);
        assert_eq!(report_context, Some(json!({"candidate_index": 1})));
    }
}
