use aether_data_contracts::repository::usage::{
    UsageCleanupExecutionMode, UsageCleanupPreviewCounts, UsageCleanupSummary, UsageCleanupTargets,
    UsageCleanupWindow,
};
use chrono::{DateTime, Utc};
use serde_json::Value;
use sqlx::Row;
use tracing::warn;

use crate::error::SqlResultExt;
use crate::{DataLayerError, SqlitePool};

const RAW_BODY_PREDICATE: &str = r#"
request_body IS NOT NULL
OR response_body IS NOT NULL
OR provider_request_body IS NOT NULL
OR client_response_body IS NOT NULL
"#;

const COMPRESSED_BODY_PREDICATE: &str = r#"
request_body_compressed IS NOT NULL
OR response_body_compressed IS NOT NULL
OR provider_request_body_compressed IS NOT NULL
OR client_response_body_compressed IS NOT NULL
OR EXISTS (
  SELECT 1 FROM usage_body_blobs
  WHERE usage_body_blobs.request_id = "usage".request_id
)
OR EXISTS (
  SELECT 1 FROM usage_http_audits
  WHERE usage_http_audits.request_id = "usage".request_id
    AND (
      usage_http_audits.request_body_ref IS NOT NULL
      OR usage_http_audits.provider_request_body_ref IS NOT NULL
      OR usage_http_audits.response_body_ref IS NOT NULL
      OR usage_http_audits.client_response_body_ref IS NOT NULL
    )
)
"#;

const ALL_BODY_PREDICATE: &str = r#"
request_body IS NOT NULL
OR response_body IS NOT NULL
OR provider_request_body IS NOT NULL
OR client_response_body IS NOT NULL
OR request_body_compressed IS NOT NULL
OR response_body_compressed IS NOT NULL
OR provider_request_body_compressed IS NOT NULL
OR client_response_body_compressed IS NOT NULL
OR EXISTS (
  SELECT 1 FROM usage_body_blobs
  WHERE usage_body_blobs.request_id = "usage".request_id
)
OR EXISTS (
  SELECT 1 FROM usage_http_audits
  WHERE usage_http_audits.request_id = "usage".request_id
    AND (
      usage_http_audits.request_body_ref IS NOT NULL
      OR usage_http_audits.provider_request_body_ref IS NOT NULL
      OR usage_http_audits.response_body_ref IS NOT NULL
      OR usage_http_audits.client_response_body_ref IS NOT NULL
    )
)
"#;

const HEADER_PREDICATE: &str = r#"
request_headers IS NOT NULL
OR response_headers IS NOT NULL
OR provider_request_headers IS NOT NULL
OR client_response_headers IS NOT NULL
OR EXISTS (
  SELECT 1 FROM usage_http_audits
  WHERE usage_http_audits.request_id = "usage".request_id
    AND (
      usage_http_audits.request_headers IS NOT NULL
      OR usage_http_audits.response_headers IS NOT NULL
      OR usage_http_audits.provider_request_headers IS NOT NULL
      OR usage_http_audits.client_response_headers IS NOT NULL
    )
)
"#;

const LEGACY_BODY_REF_PREDICATE: &str = r#"
request_metadata IS NOT NULL
AND json_valid(request_metadata)
AND (
  json_type(CASE WHEN json_valid(request_metadata) THEN request_metadata ELSE '{}' END, '$.request_body_ref') IS NOT NULL
  OR json_type(CASE WHEN json_valid(request_metadata) THEN request_metadata ELSE '{}' END, '$.provider_request_body_ref') IS NOT NULL
  OR json_type(CASE WHEN json_valid(request_metadata) THEN request_metadata ELSE '{}' END, '$.response_body_ref') IS NOT NULL
  OR json_type(CASE WHEN json_valid(request_metadata) THEN request_metadata ELSE '{}' END, '$.client_response_body_ref') IS NOT NULL
)
"#;

const DETAIL_BODY_PREDICATE: &str = r#"
request_body IS NOT NULL
OR response_body IS NOT NULL
OR provider_request_body IS NOT NULL
OR client_response_body IS NOT NULL
OR request_body_compressed IS NOT NULL
OR response_body_compressed IS NOT NULL
OR provider_request_body_compressed IS NOT NULL
OR client_response_body_compressed IS NOT NULL
OR EXISTS (
  SELECT 1 FROM usage_body_blobs
  WHERE usage_body_blobs.request_id = "usage".request_id
)
OR EXISTS (
  SELECT 1 FROM usage_http_audits
  WHERE usage_http_audits.request_id = "usage".request_id
    AND (
      usage_http_audits.request_body_ref IS NOT NULL
      OR usage_http_audits.provider_request_body_ref IS NOT NULL
      OR usage_http_audits.response_body_ref IS NOT NULL
      OR usage_http_audits.client_response_body_ref IS NOT NULL
    )
)
OR (
  request_metadata IS NOT NULL
  AND json_valid(request_metadata)
  AND (
    json_type(CASE WHEN json_valid(request_metadata) THEN request_metadata ELSE '{}' END, '$.request_body_ref') IS NOT NULL
    OR json_type(CASE WHEN json_valid(request_metadata) THEN request_metadata ELSE '{}' END, '$.provider_request_body_ref') IS NOT NULL
    OR json_type(CASE WHEN json_valid(request_metadata) THEN request_metadata ELSE '{}' END, '$.response_body_ref') IS NOT NULL
    OR json_type(CASE WHEN json_valid(request_metadata) THEN request_metadata ELSE '{}' END, '$.client_response_body_ref') IS NOT NULL
  )
)
"#;

#[derive(Debug)]
struct CleanupRow {
    id: String,
    request_id: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BodyCleanupKind {
    Raw,
    Compressed,
    All,
}

impl BodyCleanupKind {
    fn predicate(self) -> &'static str {
        match self {
            Self::Raw => RAW_BODY_PREDICATE,
            Self::Compressed => COMPRESSED_BODY_PREDICATE,
            Self::All => ALL_BODY_PREDICATE,
        }
    }
}

pub(crate) async fn cleanup_usage(
    pool: &SqlitePool,
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
            cleanup_body_fields(
                pool,
                window.detail_cutoff,
                None,
                batch_size,
                BodyCleanupKind::Raw,
            )
            .await?
        } else {
            0
        };
        let body_cleaned = if targets.compressed_body {
            cleanup_body_fields(
                pool,
                window.compressed_cutoff,
                None,
                batch_size,
                BodyCleanupKind::Compressed,
            )
            .await?
        } else {
            0
        };
        return Ok(UsageCleanupSummary {
            body_externalized,
            body_cleaned,
            ..UsageCleanupSummary::default()
        });
    }

    let records_deleted = if targets.records {
        delete_old_usage_records(pool, window.log_cutoff, batch_size).await?
    } else {
        0
    };
    let record_cutoff = targets.records.then_some(window.log_cutoff);
    let header_cleaned = if targets.headers {
        cleanup_headers(pool, window.header_cutoff, record_cutoff, batch_size).await?
    } else {
        0
    };
    let body_cleaned = if targets.compressed_body {
        cleanup_body_fields(
            pool,
            window.compressed_cutoff,
            record_cutoff,
            batch_size,
            BodyCleanupKind::All,
        )
        .await?
    } else {
        0
    };
    let detail_newer_than = detail_body_newer_than(window, targets);
    let legacy_body_refs_migrated = if targets.detail_body {
        purge_legacy_body_refs(pool, window.detail_cutoff, detail_newer_than, batch_size).await?
    } else {
        0
    };
    let body_externalized = if targets.detail_body {
        cleanup_body_fields(
            pool,
            window.detail_cutoff,
            detail_newer_than,
            batch_size,
            BodyCleanupKind::All,
        )
        .await?
    } else {
        0
    };
    let keys_cleaned = if targets.expired_keys {
        match cleanup_expired_api_keys(pool, auto_delete_expired_keys).await {
            Ok(count) => count,
            Err(err) => {
                warn!(error = %err, "SQLite usage cleanup expired api key sweep failed");
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

pub(crate) async fn preview_usage_cleanup(
    pool: &SqlitePool,
    window: &UsageCleanupWindow,
    targets: UsageCleanupTargets,
    mode: UsageCleanupExecutionMode,
) -> Result<UsageCleanupPreviewCounts, DataLayerError> {
    if mode == UsageCleanupExecutionMode::BeforeNowBodyFields {
        let detail = if targets.detail_body {
            count_candidates(pool, RAW_BODY_PREDICATE, window.detail_cutoff, None).await?
        } else {
            0
        };
        let compressed = if targets.compressed_body {
            count_candidates(
                pool,
                COMPRESSED_BODY_PREDICATE,
                window.compressed_cutoff,
                None,
            )
            .await?
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

    let record_cutoff = targets.records.then_some(window.log_cutoff);
    let detail = if targets.detail_body {
        count_candidates(
            pool,
            DETAIL_BODY_PREDICATE,
            window.detail_cutoff,
            detail_body_newer_than(window, targets),
        )
        .await?
    } else {
        0
    };
    let compressed = if targets.compressed_body {
        count_candidates(
            pool,
            ALL_BODY_PREDICATE,
            window.compressed_cutoff,
            record_cutoff,
        )
        .await?
    } else {
        0
    };
    let header = if targets.headers {
        count_candidates(pool, HEADER_PREDICATE, window.header_cutoff, record_cutoff).await?
    } else {
        0
    };
    let log = if targets.records {
        let count: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM \"usage\" WHERE created_at_unix_ms < ?")
                .bind(window.log_cutoff.timestamp())
                .fetch_one(pool)
                .await
                .map_sql_err()?;
        u64::try_from(count).unwrap_or(0)
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

async fn count_candidates(
    pool: &SqlitePool,
    predicate: &str,
    cutoff: DateTime<Utc>,
    newer_than: Option<DateTime<Utc>>,
) -> Result<u64, DataLayerError> {
    if invalid_window(cutoff, newer_than) {
        return Ok(0);
    }
    let sql = format!(
        r#"
SELECT COUNT(*)
FROM "usage"
WHERE created_at_unix_ms < ?
  AND (? IS NULL OR created_at_unix_ms >= ?)
  AND ({predicate})
"#
    );
    let newer_than = newer_than.map(|value| value.timestamp());
    let count: i64 = sqlx::query_scalar(&sql)
        .bind(cutoff.timestamp())
        .bind(newer_than)
        .bind(newer_than)
        .fetch_one(pool)
        .await
        .map_sql_err()?;
    Ok(u64::try_from(count).unwrap_or(0))
}

async fn fetch_cleanup_rows(
    pool: &SqlitePool,
    predicate: &str,
    cutoff: DateTime<Utc>,
    newer_than: Option<DateTime<Utc>>,
    batch_size: usize,
) -> Result<Vec<CleanupRow>, DataLayerError> {
    if invalid_window(cutoff, newer_than) {
        return Ok(Vec::new());
    }
    let sql = format!(
        r#"
SELECT id, request_id
FROM "usage"
WHERE created_at_unix_ms < ?
  AND (? IS NULL OR created_at_unix_ms >= ?)
  AND ({predicate})
ORDER BY created_at_unix_ms ASC, id ASC
LIMIT ?
"#
    );
    let newer_than = newer_than.map(|value| value.timestamp());
    sqlx::query(&sql)
        .bind(cutoff.timestamp())
        .bind(newer_than)
        .bind(newer_than)
        .bind(i64::try_from(batch_size).unwrap_or(i64::MAX))
        .fetch_all(pool)
        .await
        .map_sql_err()?
        .into_iter()
        .map(|row| {
            Ok(CleanupRow {
                id: row.try_get("id").map_sql_err()?,
                request_id: row.try_get("request_id").map_sql_err()?,
            })
        })
        .collect()
}

fn invalid_window(cutoff: DateTime<Utc>, newer_than: Option<DateTime<Utc>>) -> bool {
    matches!(newer_than, Some(value) if value >= cutoff)
}

async fn delete_old_usage_records(
    pool: &SqlitePool,
    cutoff: DateTime<Utc>,
    batch_size: usize,
) -> Result<usize, DataLayerError> {
    let mut total = 0usize;
    loop {
        let rows = fetch_cleanup_rows(pool, "1 = 1", cutoff, None, batch_size).await?;
        if rows.is_empty() {
            break;
        }
        let row_count = rows.len();
        let mut tx = pool.begin().await.map_sql_err()?;
        let mut deleted = 0usize;
        for row in rows {
            deleted += usize::try_from(
                sqlx::query("DELETE FROM \"usage\" WHERE id = ?")
                    .bind(row.id)
                    .execute(&mut *tx)
                    .await
                    .map_sql_err()?
                    .rows_affected(),
            )
            .unwrap_or(usize::MAX);
        }
        tx.commit().await.map_sql_err()?;
        total = total.saturating_add(deleted);
        if row_count < batch_size {
            break;
        }
    }
    Ok(total)
}

async fn cleanup_headers(
    pool: &SqlitePool,
    cutoff: DateTime<Utc>,
    newer_than: Option<DateTime<Utc>>,
    batch_size: usize,
) -> Result<usize, DataLayerError> {
    if invalid_window(cutoff, newer_than) {
        warn!(%cutoff, ?newer_than, "SQLite usage header cleanup skipped due to invalid window");
        return Ok(0);
    }
    let mut total = 0usize;
    loop {
        let rows =
            fetch_cleanup_rows(pool, HEADER_PREDICATE, cutoff, newer_than, batch_size).await?;
        if rows.is_empty() {
            break;
        }
        let row_count = rows.len();
        let mut tx = pool.begin().await.map_sql_err()?;
        for row in rows {
            sqlx::query(
                r#"
UPDATE "usage"
SET request_headers = NULL,
    response_headers = NULL,
    provider_request_headers = NULL,
    client_response_headers = NULL
WHERE id = ?
"#,
            )
            .bind(&row.id)
            .execute(&mut *tx)
            .await
            .map_sql_err()?;
            sqlx::query(
                r#"
UPDATE usage_http_audits
SET request_headers = NULL,
    response_headers = NULL,
    provider_request_headers = NULL,
    client_response_headers = NULL,
    updated_at = CAST(strftime('%s', 'now') AS INTEGER)
WHERE request_id = ?
"#,
            )
            .bind(&row.request_id)
            .execute(&mut *tx)
            .await
            .map_sql_err()?;
            delete_empty_http_audit(&mut tx, &row.request_id).await?;
        }
        tx.commit().await.map_sql_err()?;
        total = total.saturating_add(row_count);
        if row_count < batch_size {
            break;
        }
    }
    Ok(total)
}

async fn cleanup_body_fields(
    pool: &SqlitePool,
    cutoff: DateTime<Utc>,
    newer_than: Option<DateTime<Utc>>,
    batch_size: usize,
    kind: BodyCleanupKind,
) -> Result<usize, DataLayerError> {
    if invalid_window(cutoff, newer_than) {
        warn!(%cutoff, ?newer_than, "SQLite usage body cleanup skipped due to invalid window");
        return Ok(0);
    }
    let mut total = 0usize;
    loop {
        let rows =
            fetch_cleanup_rows(pool, kind.predicate(), cutoff, newer_than, batch_size).await?;
        if rows.is_empty() {
            break;
        }
        let row_count = rows.len();
        let mut tx = pool.begin().await.map_sql_err()?;
        for row in rows {
            if kind == BodyCleanupKind::All {
                sqlx::query(
                    r#"
UPDATE "usage"
SET request_body = NULL,
    response_body = NULL,
    provider_request_body = NULL,
    client_response_body = NULL,
    request_body_compressed = NULL,
    response_body_compressed = NULL,
    provider_request_body_compressed = NULL,
    client_response_body_compressed = NULL
WHERE id = ?
"#,
                )
                .bind(&row.id)
                .execute(&mut *tx)
                .await
                .map_sql_err()?;
            } else if kind == BodyCleanupKind::Compressed {
                sqlx::query(
                    r#"
UPDATE "usage"
SET request_body_compressed = NULL,
    response_body_compressed = NULL,
    provider_request_body_compressed = NULL,
    client_response_body_compressed = NULL
WHERE id = ?
"#,
                )
                .bind(&row.id)
                .execute(&mut *tx)
                .await
                .map_sql_err()?;
            } else {
                sqlx::query(
                    r#"
UPDATE "usage"
SET request_body = NULL,
    response_body = NULL,
    provider_request_body = NULL,
    client_response_body = NULL
WHERE id = ?
"#,
                )
                .bind(&row.id)
                .execute(&mut *tx)
                .await
                .map_sql_err()?;
            }

            sqlx::query("DELETE FROM usage_body_blobs WHERE request_id = ?")
                .bind(&row.request_id)
                .execute(&mut *tx)
                .await
                .map_sql_err()?;
            sqlx::query(
                r#"
UPDATE usage_http_audits
SET request_body_ref = NULL,
    provider_request_body_ref = NULL,
    response_body_ref = NULL,
    client_response_body_ref = NULL,
    body_capture_mode = 'none',
    updated_at = CAST(strftime('%s', 'now') AS INTEGER)
WHERE request_id = ?
"#,
            )
            .bind(&row.request_id)
            .execute(&mut *tx)
            .await
            .map_sql_err()?;
            delete_empty_http_audit(&mut tx, &row.request_id).await?;
        }
        tx.commit().await.map_sql_err()?;
        total = total.saturating_add(row_count);
        if row_count < batch_size {
            break;
        }
    }
    Ok(total)
}

async fn delete_empty_http_audit(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    request_id: &str,
) -> Result<(), DataLayerError> {
    sqlx::query(
        r#"
DELETE FROM usage_http_audits
WHERE request_id = ?
  AND request_headers IS NULL
  AND response_headers IS NULL
  AND provider_request_headers IS NULL
  AND client_response_headers IS NULL
  AND request_body_ref IS NULL
  AND provider_request_body_ref IS NULL
  AND response_body_ref IS NULL
  AND client_response_body_ref IS NULL
"#,
    )
    .bind(request_id)
    .execute(&mut **tx)
    .await
    .map_sql_err()?;
    Ok(())
}

async fn purge_legacy_body_refs(
    pool: &SqlitePool,
    cutoff: DateTime<Utc>,
    newer_than: Option<DateTime<Utc>>,
    batch_size: usize,
) -> Result<usize, DataLayerError> {
    if invalid_window(cutoff, newer_than) {
        warn!(%cutoff, ?newer_than, "SQLite usage legacy body-ref purge skipped due to invalid window");
        return Ok(0);
    }
    let mut total = 0usize;
    loop {
        let rows = fetch_cleanup_rows(
            pool,
            LEGACY_BODY_REF_PREDICATE,
            cutoff,
            newer_than,
            batch_size,
        )
        .await?;
        if rows.is_empty() {
            break;
        }
        let row_count = rows.len();
        let mut tx = pool.begin().await.map_sql_err()?;
        let mut purged = 0usize;
        for row in rows {
            let metadata: Option<String> =
                sqlx::query_scalar("SELECT request_metadata FROM \"usage\" WHERE id = ? LIMIT 1")
                    .bind(&row.id)
                    .fetch_optional(&mut *tx)
                    .await
                    .map_sql_err()?
                    .flatten();
            let Some(metadata) = legacy_body_ref_purge_plan(metadata.as_deref())? else {
                continue;
            };
            let updated = sqlx::query(
                r#"
UPDATE "usage"
SET request_metadata = ?,
    updated_at_unix_secs = CAST(strftime('%s', 'now') AS INTEGER)
WHERE id = ?
"#,
            )
            .bind(metadata)
            .bind(row.id)
            .execute(&mut *tx)
            .await
            .map_sql_err()?
            .rows_affected();
            purge_detached_body_capture(&mut tx, &row.request_id).await?;
            if updated > 0 {
                purged += 1;
            }
        }
        tx.commit().await.map_sql_err()?;
        total = total.saturating_add(purged);
        if row_count < batch_size || purged == 0 {
            break;
        }
    }
    Ok(total)
}

fn legacy_body_ref_purge_plan(
    metadata: Option<&str>,
) -> Result<Option<Option<String>>, DataLayerError> {
    let Some(metadata) = metadata else {
        return Ok(None);
    };
    let value: Value = serde_json::from_str(metadata).map_err(|err| {
        DataLayerError::UnexpectedValue(format!("invalid usage request_metadata JSON: {err}"))
    })?;
    let Value::Object(mut object) = value else {
        return Ok(None);
    };
    let mut removed = false;
    for key in [
        "request_body_ref",
        "provider_request_body_ref",
        "response_body_ref",
        "client_response_body_ref",
    ] {
        if object.remove(key).is_some() {
            removed = true;
        }
    }
    if !removed {
        return Ok(None);
    }
    let metadata = if object.is_empty() {
        None
    } else {
        Some(
            serde_json::to_string(&Value::Object(object)).map_err(|err| {
                DataLayerError::UnexpectedValue(format!(
                    "failed to serialize request_metadata: {err}"
                ))
            })?,
        )
    };
    Ok(Some(metadata))
}

async fn purge_detached_body_capture(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    request_id: &str,
) -> Result<(), DataLayerError> {
    sqlx::query("DELETE FROM usage_body_blobs WHERE request_id = ?")
        .bind(request_id)
        .execute(&mut **tx)
        .await
        .map_sql_err()?;
    sqlx::query(
        r#"
UPDATE usage_http_audits
SET request_body_ref = NULL,
    provider_request_body_ref = NULL,
    response_body_ref = NULL,
    client_response_body_ref = NULL,
    body_capture_mode = 'none',
  updated_at = CAST(strftime('%s', 'now') AS INTEGER)
WHERE request_id = ?
"#,
    )
    .bind(request_id)
    .execute(&mut **tx)
    .await
    .map_sql_err()?;
    delete_empty_http_audit(tx, request_id).await
}

async fn cleanup_expired_api_keys(
    pool: &SqlitePool,
    auto_delete_expired_keys: bool,
) -> Result<usize, DataLayerError> {
    let now = Utc::now().timestamp();
    let rows = sqlx::query(
        r#"
SELECT id, auto_delete_on_expiry
FROM api_keys
WHERE expires_at <= ?
  AND is_active = 1
ORDER BY expires_at ASC, id ASC
"#,
    )
    .bind(now)
    .fetch_all(pool)
    .await
    .map_sql_err()?;
    let mut cleaned = 0usize;
    for row in rows {
        let id: String = row.try_get("id").map_sql_err()?;
        let auto_delete = row
            .try_get::<Option<i64>, _>("auto_delete_on_expiry")
            .map_sql_err()?
            .map(|value| value != 0)
            .unwrap_or(auto_delete_expired_keys);
        let mut tx = pool.begin().await.map_sql_err()?;
        let affected = if auto_delete {
            sqlx::query(
                "UPDATE wallets SET status = 'disabled', updated_at = ? WHERE api_key_id = ? AND status <> 'disabled'",
            )
            .bind(now)
            .bind(&id)
            .execute(&mut *tx)
            .await
            .map_sql_err()?;
            sqlx::query("DELETE FROM api_keys WHERE id = ?")
                .bind(&id)
                .execute(&mut *tx)
                .await
                .map_sql_err()?
                .rows_affected()
        } else {
            sqlx::query(
                "UPDATE api_keys SET is_active = 0, updated_at = ? WHERE id = ? AND is_active = 1",
            )
            .bind(now)
            .bind(&id)
            .execute(&mut *tx)
            .await
            .map_sql_err()?
            .rows_affected()
        };
        tx.commit().await.map_sql_err()?;
        if affected > 0 {
            cleaned += 1;
        }
    }
    Ok(cleaned)
}
