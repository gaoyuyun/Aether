use aether_data_contracts::repository::usage::{
    UsageCleanupExecutionMode, UsageCleanupPreviewCounts, UsageCleanupSummary, UsageCleanupTargets,
    UsageCleanupWindow,
};
use chrono::{DateTime, Utc};
use futures_util::TryStreamExt;
use serde_json::Value;
use sqlx::Row;
use tracing::warn;

use super::SqlxUsageReadRepository;
use crate::{error::postgres_error, DataLayerError, PostgresPool};

const DELETE_OLD_USAGE_RECORDS_SQL: &str = r#"
WITH doomed AS (
    SELECT id
    FROM usage
    WHERE created_at < $1
    ORDER BY created_at ASC, id ASC
    LIMIT $2
)
DELETE FROM usage AS usage_rows
USING doomed
WHERE usage_rows.id = doomed.id
"#;
const SELECT_USAGE_LEGACY_BODY_REF_METADATA_BATCH_SQL: &str = r#"
SELECT id, request_id, request_metadata
FROM usage
WHERE created_at < $1
  AND ($2::timestamptz IS NULL OR created_at >= $2)
  AND request_metadata IS NOT NULL
  AND (
    request_metadata::jsonb ? 'request_body_ref'
    OR request_metadata::jsonb ? 'provider_request_body_ref'
    OR request_metadata::jsonb ? 'response_body_ref'
    OR request_metadata::jsonb ? 'client_response_body_ref'
  )
ORDER BY created_at ASC, id ASC
LIMIT $3
"#;
const SELECT_USAGE_HEADER_BATCH_SQL: &str = r#"
SELECT id, request_id
FROM usage
WHERE created_at < $1
  AND ($2::timestamptz IS NULL OR created_at >= $2)
  AND (
    request_headers IS NOT NULL
    OR response_headers IS NOT NULL
    OR provider_request_headers IS NOT NULL
    OR client_response_headers IS NOT NULL
    OR EXISTS (
      SELECT 1
      FROM usage_http_audits
      WHERE usage_http_audits.request_id = usage.request_id
        AND (
          usage_http_audits.request_headers IS NOT NULL
          OR usage_http_audits.response_headers IS NOT NULL
          OR usage_http_audits.provider_request_headers IS NOT NULL
          OR usage_http_audits.client_response_headers IS NOT NULL
        )
    )
  )
ORDER BY created_at ASC, id ASC
LIMIT $3
"#;
const CLEAR_USAGE_HEADER_FIELDS_SQL: &str = r#"
UPDATE usage
SET request_headers = NULL,
    response_headers = NULL,
    provider_request_headers = NULL,
    client_response_headers = NULL
WHERE id = ANY($1)
"#;
const CLEAR_USAGE_HTTP_AUDIT_HEADERS_SQL: &str = r#"
UPDATE usage_http_audits
SET request_headers = NULL,
    response_headers = NULL,
    provider_request_headers = NULL,
    client_response_headers = NULL,
    updated_at = NOW()
WHERE request_id = ANY($1)
"#;
const DELETE_EMPTY_USAGE_HTTP_AUDITS_SQL: &str = r#"
DELETE FROM usage_http_audits
WHERE request_id = ANY($1)
  AND request_headers IS NULL
  AND response_headers IS NULL
  AND provider_request_headers IS NULL
  AND client_response_headers IS NULL
  AND request_body_ref IS NULL
  AND provider_request_body_ref IS NULL
  AND response_body_ref IS NULL
  AND client_response_body_ref IS NULL
"#;
const SELECT_USAGE_STALE_BODY_BATCH_SQL: &str = r#"
SELECT id, request_id
FROM usage
WHERE created_at < $1
  AND ($2::timestamptz IS NULL OR created_at >= $2)
  AND (
    request_body IS NOT NULL
    OR response_body IS NOT NULL
    OR provider_request_body IS NOT NULL
    OR client_response_body IS NOT NULL
    OR request_body_compressed IS NOT NULL
    OR response_body_compressed IS NOT NULL
    OR provider_request_body_compressed IS NOT NULL
    OR client_response_body_compressed IS NOT NULL
    OR EXISTS (
      SELECT 1
      FROM usage_body_blobs
      WHERE usage_body_blobs.request_id = usage.request_id
    )
    OR EXISTS (
      SELECT 1
      FROM usage_http_audits
      WHERE usage_http_audits.request_id = usage.request_id
        AND (
          usage_http_audits.request_body_ref IS NOT NULL
          OR usage_http_audits.provider_request_body_ref IS NOT NULL
          OR usage_http_audits.response_body_ref IS NOT NULL
          OR usage_http_audits.client_response_body_ref IS NOT NULL
        )
    )
  )
ORDER BY created_at ASC, id ASC
LIMIT $3
"#;
const SELECT_USAGE_RAW_BODY_BATCH_SQL: &str = r#"
SELECT id, request_id
FROM usage
WHERE created_at < $1
  AND (
    request_body IS NOT NULL
    OR response_body IS NOT NULL
    OR provider_request_body IS NOT NULL
    OR client_response_body IS NOT NULL
  )
ORDER BY created_at ASC, id ASC
LIMIT $2
"#;
const CLEAR_USAGE_RAW_BODY_FIELDS_SQL: &str = r#"
UPDATE usage
SET request_body = NULL,
    response_body = NULL,
    provider_request_body = NULL,
    client_response_body = NULL
WHERE id = ANY($1)
"#;
const SELECT_USAGE_COMPRESSED_BODY_BATCH_SQL: &str = r#"
SELECT id, request_id
FROM usage
WHERE created_at < $1
  AND (
    request_body_compressed IS NOT NULL
    OR response_body_compressed IS NOT NULL
    OR provider_request_body_compressed IS NOT NULL
    OR client_response_body_compressed IS NOT NULL
    OR EXISTS (
      SELECT 1
      FROM usage_body_blobs
      WHERE usage_body_blobs.request_id = usage.request_id
    )
    OR EXISTS (
      SELECT 1
      FROM usage_http_audits
      WHERE usage_http_audits.request_id = usage.request_id
        AND (
          usage_http_audits.request_body_ref IS NOT NULL
          OR usage_http_audits.provider_request_body_ref IS NOT NULL
          OR usage_http_audits.response_body_ref IS NOT NULL
          OR usage_http_audits.client_response_body_ref IS NOT NULL
        )
    )
  )
ORDER BY created_at ASC, id ASC
LIMIT $2
"#;
const CLEAR_USAGE_COMPRESSED_BODY_FIELDS_SQL: &str = r#"
UPDATE usage
SET request_body_compressed = NULL,
    response_body_compressed = NULL,
    provider_request_body_compressed = NULL,
    client_response_body_compressed = NULL
WHERE id = ANY($1)
"#;
const CLEAR_USAGE_BODY_FIELDS_SQL: &str = r#"
UPDATE usage
SET request_body = NULL,
    response_body = NULL,
    provider_request_body = NULL,
    client_response_body = NULL,
    request_body_compressed = NULL,
    response_body_compressed = NULL,
    provider_request_body_compressed = NULL,
    client_response_body_compressed = NULL
WHERE id = ANY($1)
"#;
const DELETE_USAGE_BODY_BLOBS_SQL: &str = r#"
DELETE FROM usage_body_blobs
WHERE request_id = ANY($1)
"#;
const CLEAR_USAGE_HTTP_AUDIT_BODY_REFS_SQL: &str = r#"
UPDATE usage_http_audits
SET request_body_ref = NULL,
    provider_request_body_ref = NULL,
    response_body_ref = NULL,
    client_response_body_ref = NULL,
    body_capture_mode = 'none',
    updated_at = NOW()
WHERE request_id = ANY($1)
"#;
const SELECT_USAGE_BODY_COMPRESSION_BATCH_SQL: &str = r#"
SELECT id, request_id
FROM usage
WHERE created_at < $1
  AND ($2::timestamptz IS NULL OR created_at >= $2)
  AND (
    request_body IS NOT NULL
    OR request_body_compressed IS NOT NULL
    OR response_body IS NOT NULL
    OR response_body_compressed IS NOT NULL
    OR provider_request_body IS NOT NULL
    OR provider_request_body_compressed IS NOT NULL
    OR client_response_body IS NOT NULL
    OR client_response_body_compressed IS NOT NULL
    OR EXISTS (
      SELECT 1
      FROM usage_body_blobs
      WHERE usage_body_blobs.request_id = usage.request_id
    )
    OR EXISTS (
      SELECT 1
      FROM usage_http_audits
      WHERE usage_http_audits.request_id = usage.request_id
        AND (
          usage_http_audits.request_body_ref IS NOT NULL
          OR usage_http_audits.provider_request_body_ref IS NOT NULL
          OR usage_http_audits.response_body_ref IS NOT NULL
          OR usage_http_audits.client_response_body_ref IS NOT NULL
        )
    )
  )
ORDER BY created_at ASC, id ASC
LIMIT $3
"#;
const SELECT_EXPIRED_ACTIVE_API_KEYS_SQL: &str = r#"
SELECT id, auto_delete_on_expiry
FROM api_keys
WHERE expires_at <= NOW()
  AND is_active IS TRUE
ORDER BY expires_at ASC NULLS FIRST, id ASC
"#;
const DISABLE_EXPIRED_API_KEY_WALLET_SQL: &str = r#"
UPDATE wallets
SET status = 'disabled',
    updated_at = NOW()
WHERE api_key_id = $1
  AND status <> 'disabled'
"#;
const DELETE_EXPIRED_API_KEY_SQL: &str = r#"
DELETE FROM api_keys
WHERE id = $1
"#;
const DISABLE_EXPIRED_API_KEY_SQL: &str = r#"
UPDATE api_keys
SET is_active = FALSE,
    updated_at = $2
WHERE id = $1
  AND is_active IS TRUE
"#;

#[derive(Debug, Clone, PartialEq)]
pub struct UsageLegacyBodyRefMetadataRow {
    pub id: String,
    pub request_id: String,
    pub request_metadata: Option<Value>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct UsageLegacyBodyRefPurgePlan {
    pub request_metadata: Option<Value>,
}

#[derive(Debug, Clone, PartialEq)]
struct UsageBodyCleanupRow {
    id: String,
    request_id: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ExpiredApiKeyRow<'a> {
    id: &'a str,
    auto_delete_on_expiry: Option<bool>,
}

pub fn purge_legacy_body_ref_metadata_plan(
    request_metadata: Option<Value>,
) -> Option<UsageLegacyBodyRefPurgePlan> {
    let mut metadata = match request_metadata {
        Some(Value::Object(object)) => object,
        _ => return None,
    };

    let mut removed_any = false;
    for key in [
        "request_body_ref",
        "provider_request_body_ref",
        "response_body_ref",
        "client_response_body_ref",
    ] {
        if metadata.remove(key).is_some() {
            removed_any = true;
        }
    }

    if !removed_any {
        return None;
    }

    Some(UsageLegacyBodyRefPurgePlan {
        request_metadata: (!metadata.is_empty()).then_some(Value::Object(metadata)),
    })
}

impl SqlxUsageReadRepository {
    pub async fn cleanup_usage(
        &self,
        window: &UsageCleanupWindow,
        batch_size: usize,
        auto_delete_expired_keys: bool,
        targets: UsageCleanupTargets,
        mode: UsageCleanupExecutionMode,
    ) -> Result<UsageCleanupSummary, DataLayerError> {
        if batch_size == 0 || !targets.any_selected() {
            return Ok(UsageCleanupSummary::default());
        }
        if mode == UsageCleanupExecutionMode::BeforeNowBodyFields {
            let body_externalized = if targets.detail_body {
                cleanup_usage_raw_body_fields(&self.pool, window.detail_cutoff, batch_size).await?
            } else {
                0
            };
            let body_cleaned = if targets.compressed_body {
                cleanup_usage_compressed_body_fields(
                    &self.pool,
                    window.compressed_cutoff,
                    batch_size,
                )
                .await?
            } else {
                0
            };
            return Ok(UsageCleanupSummary {
                body_externalized,
                legacy_body_refs_migrated: 0,
                body_cleaned,
                header_cleaned: 0,
                keys_cleaned: 0,
                records_deleted: 0,
                cost_reservations_deleted: 0,
                request_admissions_deleted: 0,
            });
        }

        let records_deleted = if targets.records {
            delete_old_usage_records(&self.pool, window.log_cutoff, batch_size).await?
        } else {
            0
        };
        let header_cleaned = if targets.headers {
            cleanup_usage_header_fields(
                &self.pool,
                window.header_cutoff,
                batch_size,
                targets.records.then_some(window.log_cutoff),
            )
            .await?
        } else {
            0
        };
        let body_cleaned = if targets.compressed_body {
            cleanup_usage_stale_body_fields(
                &self.pool,
                window.compressed_cutoff,
                batch_size,
                targets.records.then_some(window.log_cutoff),
            )
            .await?
        } else {
            0
        };
        let detail_body_newer_than = detail_body_newer_than(window, targets);
        let legacy_body_refs_migrated = if targets.detail_body {
            purge_legacy_usage_body_ref_metadata(
                &self.pool,
                window.detail_cutoff,
                batch_size,
                detail_body_newer_than,
            )
            .await?
        } else {
            0
        };
        let body_externalized = if targets.detail_body {
            purge_usage_detail_body_fields(
                &self.pool,
                window.detail_cutoff,
                batch_size,
                detail_body_newer_than,
            )
            .await?
        } else {
            0
        };
        let keys_cleaned = if targets.expired_keys {
            match cleanup_expired_api_keys(&self.pool, auto_delete_expired_keys).await {
                Ok(count) => count,
                Err(err) => {
                    warn!(error = %err, "usage cleanup expired api key sweep failed");
                    0
                }
            }
        } else {
            0
        };

        Ok(UsageCleanupSummary {
            body_externalized,
            legacy_body_refs_migrated,
            body_cleaned,
            header_cleaned,
            keys_cleaned,
            records_deleted,
            cost_reservations_deleted: 0,
            request_admissions_deleted: 0,
        })
    }
}

pub async fn preview_usage_cleanup_impl(
    pool: &PostgresPool,
    window: &UsageCleanupWindow,
    targets: UsageCleanupTargets,
    mode: UsageCleanupExecutionMode,
) -> Result<UsageCleanupPreviewCounts, DataLayerError> {
    if mode == UsageCleanupExecutionMode::BeforeNowBodyFields {
        let detail = if targets.detail_body {
            count_usage_raw_body_candidates(pool, window.detail_cutoff).await?
        } else {
            0
        };
        let compressed = if targets.compressed_body {
            count_usage_compressed_body_candidates(pool, window.compressed_cutoff).await?
        } else {
            0
        };
        return Ok(UsageCleanupPreviewCounts {
            detail,
            compressed,
            header: 0,
            log: 0,
        });
    }

    let detail = if targets.detail_body {
        count_usage_detail_body_candidates(
            pool,
            window.detail_cutoff,
            detail_body_newer_than(window, targets),
        )
        .await?
    } else {
        0
    };
    let compressed = if targets.compressed_body {
        count_usage_stale_body_candidates(
            pool,
            window.compressed_cutoff,
            targets.records.then_some(window.log_cutoff),
        )
        .await?
    } else {
        0
    };
    let header = if targets.headers {
        count_usage_header_candidates(
            pool,
            window.header_cutoff,
            targets.records.then_some(window.log_cutoff),
        )
        .await?
    } else {
        0
    };
    let log = if targets.records {
        let log: i64 =
            sqlx::query_scalar("SELECT COUNT(*)::bigint FROM usage WHERE created_at < $1")
                .bind(window.log_cutoff)
                .fetch_one(pool)
                .await
                .map_err(postgres_error)?;
        u64::try_from(log).unwrap_or(0)
    } else {
        0
    };

    Ok(UsageCleanupPreviewCounts {
        detail,
        compressed,
        header,
        log,
    })
}

async fn cleanup_usage_raw_body_fields(
    pool: &PostgresPool,
    cutoff_time: DateTime<Utc>,
    batch_size: usize,
) -> Result<usize, DataLayerError> {
    let mut total_cleaned = 0usize;
    loop {
        let rows = fetch_usage_body_cleanup_rows(
            pool,
            SELECT_USAGE_RAW_BODY_BATCH_SQL,
            cutoff_time,
            batch_size,
        )
        .await?;
        if rows.is_empty() {
            break;
        }
        let ids = rows.iter().map(|row| row.id.clone()).collect::<Vec<_>>();
        let request_ids = rows
            .iter()
            .map(|row| row.request_id.clone())
            .collect::<Vec<_>>();
        let mut tx = pool.begin().await.map_err(postgres_error)?;
        let cleaned = sqlx::query(CLEAR_USAGE_RAW_BODY_FIELDS_SQL)
            .bind(ids)
            .execute(&mut *tx)
            .await
            .map_err(postgres_error)?
            .rows_affected();
        sqlx::query(DELETE_USAGE_BODY_BLOBS_SQL)
            .bind(&request_ids)
            .execute(&mut *tx)
            .await
            .map_err(postgres_error)?;
        sqlx::query(CLEAR_USAGE_HTTP_AUDIT_BODY_REFS_SQL)
            .bind(&request_ids)
            .execute(&mut *tx)
            .await
            .map_err(postgres_error)?;
        sqlx::query(DELETE_EMPTY_USAGE_HTTP_AUDITS_SQL)
            .bind(request_ids)
            .execute(&mut *tx)
            .await
            .map_err(postgres_error)?;
        tx.commit().await.map_err(postgres_error)?;
        let cleaned = usize::try_from(cleaned).unwrap_or(usize::MAX);
        total_cleaned += cleaned;
        if rows.len() < batch_size {
            break;
        }
    }
    Ok(total_cleaned)
}

async fn cleanup_usage_compressed_body_fields(
    pool: &PostgresPool,
    cutoff_time: DateTime<Utc>,
    batch_size: usize,
) -> Result<usize, DataLayerError> {
    let mut total_cleaned = 0usize;
    loop {
        let rows = fetch_usage_body_cleanup_rows(
            pool,
            SELECT_USAGE_COMPRESSED_BODY_BATCH_SQL,
            cutoff_time,
            batch_size,
        )
        .await?;
        if rows.is_empty() {
            break;
        }
        let ids = rows.iter().map(|row| row.id.clone()).collect::<Vec<_>>();
        let request_ids = rows
            .iter()
            .map(|row| row.request_id.clone())
            .collect::<Vec<_>>();

        let cleaned = sqlx::query(CLEAR_USAGE_COMPRESSED_BODY_FIELDS_SQL)
            .bind(ids)
            .execute(pool)
            .await
            .map_err(postgres_error)?
            .rows_affected();
        sqlx::query(DELETE_USAGE_BODY_BLOBS_SQL)
            .bind(&request_ids)
            .execute(pool)
            .await
            .map_err(postgres_error)?;
        sqlx::query(CLEAR_USAGE_HTTP_AUDIT_BODY_REFS_SQL)
            .bind(&request_ids)
            .execute(pool)
            .await
            .map_err(postgres_error)?;
        sqlx::query(DELETE_EMPTY_USAGE_HTTP_AUDITS_SQL)
            .bind(request_ids)
            .execute(pool)
            .await
            .map_err(postgres_error)?;
        let cleaned = usize::try_from(cleaned).unwrap_or(usize::MAX);
        total_cleaned += cleaned;
        if rows.len() < batch_size {
            break;
        }
    }
    Ok(total_cleaned)
}

async fn fetch_usage_body_cleanup_rows(
    pool: &PostgresPool,
    sql: &str,
    cutoff_time: DateTime<Utc>,
    batch_size: usize,
) -> Result<Vec<UsageBodyCleanupRow>, DataLayerError> {
    let rows = sqlx::query(sql)
        .bind(cutoff_time)
        .bind(i64::try_from(batch_size).unwrap_or(i64::MAX))
        .fetch_all(pool)
        .await
        .map_err(postgres_error)?
        .into_iter()
        .map(|row| {
            Ok(UsageBodyCleanupRow {
                id: row.try_get::<String, _>("id").map_err(postgres_error)?,
                request_id: row
                    .try_get::<String, _>("request_id")
                    .map_err(postgres_error)?,
            })
        })
        .collect::<Result<Vec<_>, DataLayerError>>()?;
    Ok(rows)
}

fn detail_body_newer_than(
    window: &UsageCleanupWindow,
    targets: UsageCleanupTargets,
) -> Option<DateTime<Utc>> {
    [
        targets.compressed_body.then_some(window.compressed_cutoff),
        targets.records.then_some(window.log_cutoff),
    ]
    .into_iter()
    .flatten()
    .max()
}

async fn count_usage_raw_body_candidates(
    pool: &PostgresPool,
    cutoff_time: DateTime<Utc>,
) -> Result<u64, DataLayerError> {
    let count: i64 = sqlx::query_scalar(
        r#"
SELECT COUNT(*)::bigint
FROM usage
WHERE created_at < $1
  AND (
    request_body IS NOT NULL
    OR response_body IS NOT NULL
    OR provider_request_body IS NOT NULL
    OR client_response_body IS NOT NULL
  )
"#,
    )
    .bind(cutoff_time)
    .fetch_one(pool)
    .await
    .map_err(postgres_error)?;
    Ok(u64::try_from(count).unwrap_or(0))
}

async fn count_usage_compressed_body_candidates(
    pool: &PostgresPool,
    cutoff_time: DateTime<Utc>,
) -> Result<u64, DataLayerError> {
    let count: i64 = sqlx::query_scalar(
        r#"
SELECT COUNT(*)::bigint
FROM usage
WHERE created_at < $1
  AND (
    request_body_compressed IS NOT NULL
    OR response_body_compressed IS NOT NULL
    OR provider_request_body_compressed IS NOT NULL
    OR client_response_body_compressed IS NOT NULL
    OR EXISTS (
      SELECT 1
      FROM usage_body_blobs
      WHERE usage_body_blobs.request_id = usage.request_id
    )
    OR EXISTS (
      SELECT 1
      FROM usage_http_audits
      WHERE usage_http_audits.request_id = usage.request_id
        AND (
          usage_http_audits.request_body_ref IS NOT NULL
          OR usage_http_audits.provider_request_body_ref IS NOT NULL
          OR usage_http_audits.response_body_ref IS NOT NULL
          OR usage_http_audits.client_response_body_ref IS NOT NULL
        )
    )
  )
"#,
    )
    .bind(cutoff_time)
    .fetch_one(pool)
    .await
    .map_err(postgres_error)?;
    Ok(u64::try_from(count).unwrap_or(0))
}

async fn count_usage_detail_body_candidates(
    pool: &PostgresPool,
    cutoff_time: DateTime<Utc>,
    newer_than: Option<DateTime<Utc>>,
) -> Result<u64, DataLayerError> {
    if matches!(newer_than, Some(value) if value >= cutoff_time) {
        return Ok(0);
    }
    let count: i64 = sqlx::query_scalar(
        r#"
SELECT COUNT(*)::bigint
FROM usage
WHERE created_at < $1
  AND ($2::timestamptz IS NULL OR created_at >= $2)
  AND (
    request_body IS NOT NULL
    OR request_body_compressed IS NOT NULL
    OR response_body IS NOT NULL
    OR response_body_compressed IS NOT NULL
    OR provider_request_body IS NOT NULL
    OR provider_request_body_compressed IS NOT NULL
    OR client_response_body IS NOT NULL
    OR client_response_body_compressed IS NOT NULL
    OR (
      request_metadata IS NOT NULL
      AND (
        request_metadata::jsonb ? 'request_body_ref'
        OR request_metadata::jsonb ? 'provider_request_body_ref'
        OR request_metadata::jsonb ? 'response_body_ref'
        OR request_metadata::jsonb ? 'client_response_body_ref'
      )
    )
  )
"#,
    )
    .bind(cutoff_time)
    .bind(newer_than)
    .fetch_one(pool)
    .await
    .map_err(postgres_error)?;
    Ok(u64::try_from(count).unwrap_or(0))
}

async fn count_usage_stale_body_candidates(
    pool: &PostgresPool,
    cutoff_time: DateTime<Utc>,
    newer_than: Option<DateTime<Utc>>,
) -> Result<u64, DataLayerError> {
    if matches!(newer_than, Some(value) if value >= cutoff_time) {
        return Ok(0);
    }
    let count: i64 = sqlx::query_scalar(
        r#"
SELECT COUNT(*)::bigint
FROM usage
WHERE created_at < $1
  AND ($2::timestamptz IS NULL OR created_at >= $2)
  AND (
    request_body IS NOT NULL
    OR response_body IS NOT NULL
    OR provider_request_body IS NOT NULL
    OR client_response_body IS NOT NULL
    OR request_body_compressed IS NOT NULL
    OR response_body_compressed IS NOT NULL
    OR provider_request_body_compressed IS NOT NULL
    OR client_response_body_compressed IS NOT NULL
    OR EXISTS (
      SELECT 1
      FROM usage_body_blobs
      WHERE usage_body_blobs.request_id = usage.request_id
    )
    OR EXISTS (
      SELECT 1
      FROM usage_http_audits
      WHERE usage_http_audits.request_id = usage.request_id
        AND (
          usage_http_audits.request_body_ref IS NOT NULL
          OR usage_http_audits.provider_request_body_ref IS NOT NULL
          OR usage_http_audits.response_body_ref IS NOT NULL
          OR usage_http_audits.client_response_body_ref IS NOT NULL
        )
    )
  )
"#,
    )
    .bind(cutoff_time)
    .bind(newer_than)
    .fetch_one(pool)
    .await
    .map_err(postgres_error)?;
    Ok(u64::try_from(count).unwrap_or(0))
}

async fn count_usage_header_candidates(
    pool: &PostgresPool,
    cutoff_time: DateTime<Utc>,
    newer_than: Option<DateTime<Utc>>,
) -> Result<u64, DataLayerError> {
    if matches!(newer_than, Some(value) if value >= cutoff_time) {
        return Ok(0);
    }
    let count: i64 = sqlx::query_scalar(
        r#"
SELECT COUNT(*)::bigint
FROM usage
WHERE created_at < $1
  AND ($2::timestamptz IS NULL OR created_at >= $2)
  AND (
    request_headers IS NOT NULL
    OR response_headers IS NOT NULL
    OR provider_request_headers IS NOT NULL
    OR client_response_headers IS NOT NULL
    OR EXISTS (
      SELECT 1
      FROM usage_http_audits
      WHERE usage_http_audits.request_id = usage.request_id
        AND (
          usage_http_audits.request_headers IS NOT NULL
          OR usage_http_audits.response_headers IS NOT NULL
          OR usage_http_audits.provider_request_headers IS NOT NULL
          OR usage_http_audits.client_response_headers IS NOT NULL
        )
    )
  )
"#,
    )
    .bind(cutoff_time)
    .bind(newer_than)
    .fetch_one(pool)
    .await
    .map_err(postgres_error)?;
    Ok(u64::try_from(count).unwrap_or(0))
}

async fn delete_old_usage_records(
    pool: &PostgresPool,
    cutoff_time: DateTime<Utc>,
    batch_size: usize,
) -> Result<usize, DataLayerError> {
    let mut total_deleted = 0usize;
    loop {
        let deleted = sqlx::query(DELETE_OLD_USAGE_RECORDS_SQL)
            .bind(cutoff_time)
            .bind(i64::try_from(batch_size).unwrap_or(i64::MAX))
            .execute(pool)
            .await
            .map_err(postgres_error)?
            .rows_affected();
        let deleted = usize::try_from(deleted).unwrap_or(usize::MAX);
        total_deleted += deleted;
        if deleted < batch_size {
            break;
        }
    }
    Ok(total_deleted)
}

async fn purge_legacy_usage_body_ref_metadata(
    pool: &PostgresPool,
    cutoff_time: DateTime<Utc>,
    batch_size: usize,
    newer_than: Option<DateTime<Utc>>,
) -> Result<usize, DataLayerError> {
    if matches!(newer_than, Some(value) if value >= cutoff_time) {
        warn!(
            cutoff_time = %cutoff_time,
            newer_than = ?newer_than,
            "usage cleanup legacy body-ref purge skipped due to invalid window"
        );
        return Ok(0);
    }

    let mut total_migrated = 0usize;
    loop {
        let rows = sqlx::query(SELECT_USAGE_LEGACY_BODY_REF_METADATA_BATCH_SQL)
            .bind(cutoff_time)
            .bind(newer_than)
            .bind(i64::try_from(batch_size).unwrap_or(i64::MAX))
            .fetch_all(pool)
            .await
            .map_err(postgres_error)?
            .into_iter()
            .map(|row| {
                Ok(UsageLegacyBodyRefMetadataRow {
                    id: row.try_get::<String, _>("id").map_err(postgres_error)?,
                    request_id: row
                        .try_get::<String, _>("request_id")
                        .map_err(postgres_error)?,
                    request_metadata: row
                        .try_get::<Option<Value>, _>("request_metadata")
                        .map_err(postgres_error)?,
                })
            })
            .collect::<Result<Vec<_>, DataLayerError>>()?;
        if rows.is_empty() {
            break;
        }

        let mut batch_purged = 0usize;
        for row in rows {
            let Some(plan) = purge_legacy_body_ref_metadata_plan(row.request_metadata) else {
                continue;
            };
            let mut tx = pool.begin().await.map_err(postgres_error)?;
            let updated = sqlx::query(UPDATE_USAGE_REQUEST_METADATA_SQL)
                .bind(&row.id)
                .bind(plan.request_metadata)
                .execute(&mut *tx)
                .await
                .map_err(postgres_error)?
                .rows_affected();
            let request_ids = vec![row.request_id];
            sqlx::query(DELETE_USAGE_BODY_BLOBS_SQL)
                .bind(&request_ids)
                .execute(&mut *tx)
                .await
                .map_err(postgres_error)?;
            sqlx::query(CLEAR_USAGE_HTTP_AUDIT_BODY_REFS_SQL)
                .bind(&request_ids)
                .execute(&mut *tx)
                .await
                .map_err(postgres_error)?;
            sqlx::query(DELETE_EMPTY_USAGE_HTTP_AUDITS_SQL)
                .bind(request_ids)
                .execute(&mut *tx)
                .await
                .map_err(postgres_error)?;
            tx.commit().await.map_err(postgres_error)?;
            if updated > 0 {
                batch_purged += 1;
            }
        }

        total_migrated += batch_purged;
        if batch_purged == 0 || batch_purged < batch_size {
            break;
        }
    }

    Ok(total_migrated)
}

async fn cleanup_usage_header_fields(
    pool: &PostgresPool,
    cutoff_time: DateTime<Utc>,
    batch_size: usize,
    newer_than: Option<DateTime<Utc>>,
) -> Result<usize, DataLayerError> {
    if matches!(newer_than, Some(value) if value >= cutoff_time) {
        warn!(
            cutoff_time = %cutoff_time,
            newer_than = ?newer_than,
            "usage cleanup header sweep skipped due to invalid window"
        );
        return Ok(0);
    }

    let mut total_cleaned = 0usize;
    loop {
        let mut stream = sqlx::query(SELECT_USAGE_HEADER_BATCH_SQL)
            .bind(cutoff_time)
            .bind(newer_than)
            .bind(i64::try_from(batch_size).unwrap_or(i64::MAX))
            .fetch(pool);
        let mut rows = Vec::new();
        while let Some(row) = stream.try_next().await.map_err(postgres_error)? {
            rows.push(UsageBodyCleanupRow {
                id: row.try_get::<String, _>("id").map_err(postgres_error)?,
                request_id: row
                    .try_get::<String, _>("request_id")
                    .map_err(postgres_error)?,
            });
        }
        if rows.is_empty() {
            break;
        }
        let ids = rows.iter().map(|row| row.id.clone()).collect::<Vec<_>>();
        let request_ids = rows
            .iter()
            .map(|row| row.request_id.clone())
            .collect::<Vec<_>>();

        let cleaned = sqlx::query(CLEAR_USAGE_HEADER_FIELDS_SQL)
            .bind(ids)
            .execute(pool)
            .await
            .map_err(postgres_error)?
            .rows_affected();
        sqlx::query(CLEAR_USAGE_HTTP_AUDIT_HEADERS_SQL)
            .bind(&request_ids)
            .execute(pool)
            .await
            .map_err(postgres_error)?;
        sqlx::query(DELETE_EMPTY_USAGE_HTTP_AUDITS_SQL)
            .bind(request_ids)
            .execute(pool)
            .await
            .map_err(postgres_error)?;
        let cleaned = usize::try_from(cleaned).unwrap_or(usize::MAX);
        total_cleaned += cleaned;
        if rows.len() < batch_size {
            break;
        }
    }
    Ok(total_cleaned)
}

async fn cleanup_usage_stale_body_fields(
    pool: &PostgresPool,
    cutoff_time: DateTime<Utc>,
    batch_size: usize,
    newer_than: Option<DateTime<Utc>>,
) -> Result<usize, DataLayerError> {
    if matches!(newer_than, Some(value) if value >= cutoff_time) {
        warn!(
            cutoff_time = %cutoff_time,
            newer_than = ?newer_than,
            "usage cleanup body sweep skipped due to invalid window"
        );
        return Ok(0);
    }

    let mut total_cleaned = 0usize;
    loop {
        let mut stream = sqlx::query(SELECT_USAGE_STALE_BODY_BATCH_SQL)
            .bind(cutoff_time)
            .bind(newer_than)
            .bind(i64::try_from(batch_size).unwrap_or(i64::MAX))
            .fetch(pool);
        let mut rows = Vec::new();
        while let Some(row) = stream.try_next().await.map_err(postgres_error)? {
            rows.push(UsageBodyCleanupRow {
                id: row.try_get::<String, _>("id").map_err(postgres_error)?,
                request_id: row
                    .try_get::<String, _>("request_id")
                    .map_err(postgres_error)?,
            });
        }
        if rows.is_empty() {
            break;
        }
        let ids = rows.iter().map(|row| row.id.clone()).collect::<Vec<_>>();
        let request_ids = rows
            .iter()
            .map(|row| row.request_id.clone())
            .collect::<Vec<_>>();

        let cleaned = sqlx::query(CLEAR_USAGE_BODY_FIELDS_SQL)
            .bind(ids)
            .execute(pool)
            .await
            .map_err(postgres_error)?
            .rows_affected();
        sqlx::query(DELETE_USAGE_BODY_BLOBS_SQL)
            .bind(&request_ids)
            .execute(pool)
            .await
            .map_err(postgres_error)?;
        sqlx::query(CLEAR_USAGE_HTTP_AUDIT_BODY_REFS_SQL)
            .bind(&request_ids)
            .execute(pool)
            .await
            .map_err(postgres_error)?;
        sqlx::query(DELETE_EMPTY_USAGE_HTTP_AUDITS_SQL)
            .bind(request_ids)
            .execute(pool)
            .await
            .map_err(postgres_error)?;
        let cleaned = usize::try_from(cleaned).unwrap_or(usize::MAX);
        total_cleaned += cleaned;
        if rows.len() < batch_size {
            break;
        }
    }
    Ok(total_cleaned)
}

async fn purge_usage_detail_body_fields(
    pool: &PostgresPool,
    cutoff_time: DateTime<Utc>,
    batch_size: usize,
    newer_than: Option<DateTime<Utc>>,
) -> Result<usize, DataLayerError> {
    if matches!(newer_than, Some(value) if value >= cutoff_time) {
        warn!(
            cutoff_time = %cutoff_time,
            newer_than = ?newer_than,
            "usage cleanup detail body purge skipped due to invalid window"
        );
        return Ok(0);
    }

    let mut total_purged = 0usize;
    loop {
        let mut stream = sqlx::query(SELECT_USAGE_BODY_COMPRESSION_BATCH_SQL)
            .bind(cutoff_time)
            .bind(newer_than)
            .bind(i64::try_from(batch_size).unwrap_or(i64::MAX))
            .fetch(pool);
        let mut rows = Vec::new();
        while let Some(row) = stream.try_next().await.map_err(postgres_error)? {
            rows.push(UsageBodyCleanupRow {
                id: row.try_get::<String, _>("id").map_err(postgres_error)?,
                request_id: row
                    .try_get::<String, _>("request_id")
                    .map_err(postgres_error)?,
            });
        }
        if rows.is_empty() {
            break;
        }
        let row_count = rows.len();
        let ids = rows.iter().map(|row| row.id.clone()).collect::<Vec<_>>();
        let request_ids = rows
            .iter()
            .map(|row| row.request_id.clone())
            .collect::<Vec<_>>();
        let mut tx = pool.begin().await.map_err(postgres_error)?;
        let updated = sqlx::query(CLEAR_USAGE_BODY_FIELDS_SQL)
            .bind(ids)
            .execute(&mut *tx)
            .await
            .map_err(postgres_error)?
            .rows_affected();
        sqlx::query(DELETE_USAGE_BODY_BLOBS_SQL)
            .bind(&request_ids)
            .execute(&mut *tx)
            .await
            .map_err(postgres_error)?;
        sqlx::query(CLEAR_USAGE_HTTP_AUDIT_BODY_REFS_SQL)
            .bind(&request_ids)
            .execute(&mut *tx)
            .await
            .map_err(postgres_error)?;
        sqlx::query(DELETE_EMPTY_USAGE_HTTP_AUDITS_SQL)
            .bind(request_ids)
            .execute(&mut *tx)
            .await
            .map_err(postgres_error)?;
        tx.commit().await.map_err(postgres_error)?;
        total_purged = total_purged.saturating_add(usize::try_from(updated).unwrap_or(usize::MAX));
        if row_count < batch_size {
            break;
        }
    }
    Ok(total_purged)
}

async fn cleanup_expired_api_keys(
    pool: &PostgresPool,
    auto_delete_expired_keys: bool,
) -> Result<usize, DataLayerError> {
    let mut expired_keys = sqlx::query(SELECT_EXPIRED_ACTIVE_API_KEYS_SQL).fetch(pool);
    let mut cleaned = 0usize;
    while let Some(row) = expired_keys.try_next().await.map_err(postgres_error)? {
        let api_key_id = row.try_get::<String, _>("id").map_err(postgres_error)?;
        let key = ExpiredApiKeyRow {
            id: api_key_id.as_str(),
            auto_delete_on_expiry: row
                .try_get::<Option<bool>, _>("auto_delete_on_expiry")
                .map_err(postgres_error)?,
        };
        let should_delete = key
            .auto_delete_on_expiry
            .unwrap_or(auto_delete_expired_keys);
        if should_delete {
            sqlx::query(DISABLE_EXPIRED_API_KEY_WALLET_SQL)
                .bind(key.id)
                .execute(pool)
                .await
                .map_err(postgres_error)?;
            let deleted = sqlx::query(DELETE_EXPIRED_API_KEY_SQL)
                .bind(key.id)
                .execute(pool)
                .await
                .map_err(postgres_error)?
                .rows_affected();
            if deleted > 0 {
                cleaned += 1;
            }
        } else {
            let updated = sqlx::query(DISABLE_EXPIRED_API_KEY_SQL)
                .bind(key.id)
                .bind(Utc::now())
                .execute(pool)
                .await
                .map_err(postgres_error)?
                .rows_affected();
            if updated > 0 {
                cleaned += 1;
            }
        }
    }
    Ok(cleaned)
}

const UPDATE_USAGE_REQUEST_METADATA_SQL: &str = r#"
UPDATE usage
SET request_metadata = $2::json,
    updated_at = NOW()
WHERE id = $1
"#;

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{
        purge_legacy_body_ref_metadata_plan, SELECT_USAGE_BODY_COMPRESSION_BATCH_SQL,
        SELECT_USAGE_LEGACY_BODY_REF_METADATA_BATCH_SQL,
    };

    #[test]
    fn legacy_body_ref_cleanup_index_is_embedded_and_matches_batch_predicate() {
        const MIGRATION_VERSION: i64 = 20_260_715_000_000;
        const LEGACY_BODY_REF_KEYS: [&str; 4] = [
            "request_body_ref",
            "provider_request_body_ref",
            "response_body_ref",
            "client_response_body_ref",
        ];

        let migration = crate::migrations::POSTGRES_MIGRATOR
            .iter()
            .find(|migration| migration.version == MIGRATION_VERSION)
            .expect("legacy body-ref cleanup index migration should be embedded");
        assert_eq!(
            migration.description.as_ref(),
            "add usage legacy body ref cleanup index"
        );
        let index_sql = migration.sql.as_ref();
        assert!(index_sql.contains("CREATE INDEX CONCURRENTLY IF NOT EXISTS"));
        assert!(index_sql.contains("idx_usage_legacy_body_ref_cleanup_created_at"));
        assert!(index_sql.contains("ON usage (created_at, id)"));
        assert!(index_sql.contains("WHERE request_metadata IS NOT NULL"));

        for key in LEGACY_BODY_REF_KEYS {
            let predicate = format!("request_metadata::jsonb ? '{key}'");
            assert!(
                SELECT_USAGE_LEGACY_BODY_REF_METADATA_BATCH_SQL.contains(&predicate),
                "cleanup batch predicate should include {key}"
            );
            assert!(
                index_sql.contains(&predicate),
                "partial index predicate should include {key}"
            );
        }
        assert_eq!(
            SELECT_USAGE_LEGACY_BODY_REF_METADATA_BATCH_SQL
                .matches("request_metadata::jsonb ? '")
                .count(),
            LEGACY_BODY_REF_KEYS.len()
        );
        assert_eq!(
            index_sql.matches("request_metadata::jsonb ? '").count(),
            LEGACY_BODY_REF_KEYS.len()
        );
    }

    #[test]
    fn detail_body_cleanup_selects_detached_capture_for_deletion() {
        assert!(SELECT_USAGE_BODY_COMPRESSION_BATCH_SQL.contains("usage_body_blobs"));
        assert!(SELECT_USAGE_BODY_COMPRESSION_BATCH_SQL.contains("usage_http_audits"));
        assert!(!SELECT_USAGE_BODY_COMPRESSION_BATCH_SQL.contains("payload_gzip"));
    }

    #[test]
    fn legacy_body_ref_metadata_purge_strips_all_ref_keys() {
        let plan = purge_legacy_body_ref_metadata_plan(Some(json!({
            "trace_id": "trace-1",
            "request_body_ref": "usage://request/req-1/request_body",
            "response_body_ref": "usage://request/req-1/response_body"
        })))
        .expect("migration plan should exist");

        assert_eq!(
            plan.request_metadata,
            Some(json!({
                "trace_id": "trace-1"
            }))
        );
    }

    #[test]
    fn legacy_body_ref_metadata_purge_does_not_preserve_untrusted_refs() {
        let plan = purge_legacy_body_ref_metadata_plan(Some(json!({
            "request_body_ref": "blob://legacy-request",
            "provider_request_body_ref": "usage://request/req-other/provider_request_body",
            "candidate_index": 2
        })))
        .expect("migration plan should exist");

        assert_eq!(
            plan.request_metadata,
            Some(json!({
                "candidate_index": 2
            }))
        );
    }
}
