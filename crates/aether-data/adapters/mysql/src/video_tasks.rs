use async_trait::async_trait;
use sqlx::{mysql::MySqlRow, MySql, QueryBuilder, Row};

use aether_data_contracts::repository::video_tasks::{
    StoredVideoTask, UpsertVideoTask, VideoTaskLookupKey, VideoTaskModelCount,
    VideoTaskQueryFilter, VideoTaskReadRepository, VideoTaskStatus, VideoTaskStatusCount,
    VideoTaskWriteRepository,
};
use aether_data_contracts::DataLayerError;

use crate::error::SqlResultExt;
use crate::MysqlPool;

const VIDEO_TASK_COLUMNS: &str = r#"
SELECT
  id,
  short_id,
  request_id,
  user_id,
  api_key_id,
  username,
  api_key_name,
  external_task_id,
  provider_id,
  endpoint_id,
  key_id,
  client_api_format,
  provider_api_format,
  format_converted,
  model,
  prompt,
  original_request_body,
  duration_seconds,
  resolution,
  aspect_ratio,
  size,
  status,
  progress_percent,
  progress_message,
  retry_count,
  poll_interval_seconds,
  next_poll_at AS next_poll_at_unix_secs,
  poll_count,
  max_poll_count,
  created_at AS created_at_unix_ms,
  submitted_at AS submitted_at_unix_secs,
  completed_at AS completed_at_unix_secs,
  updated_at AS updated_at_unix_secs,
  error_code,
  error_message,
  video_url,
  request_metadata
FROM video_tasks
"#;

#[derive(Debug, Clone)]
pub struct MysqlVideoTaskRepository {
    pool: MysqlPool,
}

impl MysqlVideoTaskRepository {
    pub fn new(pool: MysqlPool) -> Self {
        Self { pool }
    }

    async fn find_by_id(&self, id: &str) -> Result<Option<StoredVideoTask>, DataLayerError> {
        let row = sqlx::query(&format!("{VIDEO_TASK_COLUMNS} WHERE id = ? LIMIT 1"))
            .bind(id)
            .fetch_optional(&self.pool)
            .await
            .map_sql_err()?;
        row.as_ref().map(map_video_task_row).transpose()
    }

    async fn find_by_id_for_user(
        &self,
        id: &str,
        user_id: &str,
    ) -> Result<Option<StoredVideoTask>, DataLayerError> {
        let row = sqlx::query(&format!(
            "{VIDEO_TASK_COLUMNS} WHERE BINARY id = BINARY ? AND BINARY user_id = BINARY ? LIMIT 1"
        ))
        .bind(id)
        .bind(user_id)
        .fetch_optional(&self.pool)
        .await
        .map_sql_err()?;
        row.as_ref().map(map_video_task_row).transpose()
    }

    async fn find_by_short_id(
        &self,
        short_id: &str,
    ) -> Result<Option<StoredVideoTask>, DataLayerError> {
        let row = sqlx::query(&format!("{VIDEO_TASK_COLUMNS} WHERE short_id = ? LIMIT 1"))
            .bind(short_id)
            .fetch_optional(&self.pool)
            .await
            .map_sql_err()?;
        row.as_ref().map(map_video_task_row).transpose()
    }

    async fn find_by_short_id_for_user(
        &self,
        short_id: &str,
        user_id: &str,
    ) -> Result<Option<StoredVideoTask>, DataLayerError> {
        let row = sqlx::query(&format!(
            "{VIDEO_TASK_COLUMNS} WHERE BINARY short_id = BINARY ? AND BINARY user_id = BINARY ? LIMIT 1"
        ))
        .bind(short_id)
        .bind(user_id)
        .fetch_optional(&self.pool)
        .await
        .map_sql_err()?;
        row.as_ref().map(map_video_task_row).transpose()
    }

    async fn find_by_user_external(
        &self,
        user_id: &str,
        external_task_id: &str,
    ) -> Result<Option<StoredVideoTask>, DataLayerError> {
        let row = sqlx::query(&format!(
            "{VIDEO_TASK_COLUMNS} WHERE BINARY user_id = BINARY ? AND BINARY external_task_id = BINARY ? LIMIT 1"
        ))
        .bind(user_id)
        .bind(external_task_id)
        .fetch_optional(&self.pool)
        .await
        .map_sql_err()?;
        row.as_ref().map(map_video_task_row).transpose()
    }
}

#[async_trait]
impl VideoTaskReadRepository for MysqlVideoTaskRepository {
    async fn find(
        &self,
        key: VideoTaskLookupKey<'_>,
    ) -> Result<Option<StoredVideoTask>, DataLayerError> {
        match key {
            VideoTaskLookupKey::Id(id) => self.find_by_id(id).await,
            VideoTaskLookupKey::ShortId(short_id) => self.find_by_short_id(short_id).await,
            VideoTaskLookupKey::UserExternal {
                user_id,
                external_task_id,
            } => self.find_by_user_external(user_id, external_task_id).await,
        }
    }

    async fn find_for_user(
        &self,
        key: VideoTaskLookupKey<'_>,
        user_id: &str,
    ) -> Result<Option<StoredVideoTask>, DataLayerError> {
        match key {
            VideoTaskLookupKey::Id(id) => self.find_by_id_for_user(id, user_id).await,
            VideoTaskLookupKey::ShortId(short_id) => {
                self.find_by_short_id_for_user(short_id, user_id).await
            }
            VideoTaskLookupKey::UserExternal {
                user_id: lookup_user_id,
                external_task_id,
            } if lookup_user_id == user_id => {
                self.find_by_user_external(user_id, external_task_id).await
            }
            VideoTaskLookupKey::UserExternal { .. } => Ok(None),
        }
    }

    async fn list_active(&self, limit: usize) -> Result<Vec<StoredVideoTask>, DataLayerError> {
        if limit == 0 {
            return Ok(Vec::new());
        }
        let rows = sqlx::query(&format!(
            "{VIDEO_TASK_COLUMNS} WHERE status IN ('pending', 'submitted', 'queued', 'processing') ORDER BY updated_at DESC LIMIT ?"
        ))
        .bind(limit_i64(limit, "active video task limit")?)
        .fetch_all(&self.pool)
        .await
        .map_sql_err()?;
        rows.iter().map(map_video_task_row).collect()
    }

    async fn list_due(
        &self,
        now_unix_secs: u64,
        limit: usize,
    ) -> Result<Vec<StoredVideoTask>, DataLayerError> {
        if limit == 0 {
            return Ok(Vec::new());
        }
        let rows = sqlx::query(&format!(
            "{VIDEO_TASK_COLUMNS} WHERE status IN ('submitted', 'queued', 'processing') AND next_poll_at IS NOT NULL AND next_poll_at <= ? AND poll_count < max_poll_count ORDER BY next_poll_at ASC, updated_at ASC LIMIT ?"
        ))
        .bind(u64_to_i64(now_unix_secs, "video task now")?)
        .bind(limit_i64(limit, "due video task limit")?)
        .fetch_all(&self.pool)
        .await
        .map_sql_err()?;
        rows.iter().map(map_video_task_row).collect()
    }

    async fn list_page(
        &self,
        filter: &VideoTaskQueryFilter,
        offset: usize,
        limit: usize,
    ) -> Result<Vec<StoredVideoTask>, DataLayerError> {
        if limit == 0 {
            return Ok(Vec::new());
        }
        let mut builder = QueryBuilder::<MySql>::new(VIDEO_TASK_COLUMNS);
        push_filter(&mut builder, filter, None);
        builder
            .push(" ORDER BY created_at DESC, updated_at DESC LIMIT ")
            .push_bind(limit_i64(limit, "video task page limit")?)
            .push(" OFFSET ")
            .push_bind(limit_i64(offset, "video task page offset")?);
        let rows = builder.build().fetch_all(&self.pool).await.map_sql_err()?;
        rows.iter().map(map_video_task_row).collect()
    }

    async fn list_page_summary(
        &self,
        filter: &VideoTaskQueryFilter,
        offset: usize,
        limit: usize,
    ) -> Result<Vec<StoredVideoTask>, DataLayerError> {
        self.list_page(filter, offset, limit).await
    }

    async fn count(&self, filter: &VideoTaskQueryFilter) -> Result<u64, DataLayerError> {
        let mut builder = QueryBuilder::<MySql>::new("SELECT COUNT(id) AS total FROM video_tasks");
        push_filter(&mut builder, filter, None);
        count_query(builder, &self.pool).await
    }

    async fn count_by_status(
        &self,
        filter: &VideoTaskQueryFilter,
    ) -> Result<Vec<VideoTaskStatusCount>, DataLayerError> {
        let mut builder =
            QueryBuilder::<MySql>::new("SELECT status, COUNT(id) AS total FROM video_tasks");
        push_filter(&mut builder, filter, None);
        builder.push(" GROUP BY status ORDER BY status ASC");
        let rows = builder.build().fetch_all(&self.pool).await.map_sql_err()?;
        rows.iter()
            .map(|row| {
                Ok(VideoTaskStatusCount {
                    status: VideoTaskStatus::from_database(
                        row.try_get::<String, _>("status").map_sql_err()?.as_str(),
                    )?,
                    count: count_value(row.try_get("total").map_sql_err()?)?,
                })
            })
            .collect()
    }

    async fn count_distinct_users(
        &self,
        filter: &VideoTaskQueryFilter,
    ) -> Result<u64, DataLayerError> {
        let mut builder =
            QueryBuilder::<MySql>::new("SELECT COUNT(DISTINCT user_id) AS total FROM video_tasks");
        push_filter(&mut builder, filter, None);
        push_clause(&mut builder, "user_id IS NOT NULL");
        push_clause(&mut builder, "user_id <> ''");
        count_query(builder, &self.pool).await
    }

    async fn top_models(
        &self,
        filter: &VideoTaskQueryFilter,
        limit: usize,
    ) -> Result<Vec<VideoTaskModelCount>, DataLayerError> {
        if limit == 0 {
            return Ok(Vec::new());
        }
        let mut builder =
            QueryBuilder::<MySql>::new("SELECT model, COUNT(id) AS total FROM video_tasks");
        push_filter(&mut builder, filter, None);
        push_clause(&mut builder, "model IS NOT NULL");
        push_clause(&mut builder, "model <> ''");
        builder
            .push(" GROUP BY model ORDER BY total DESC, model ASC LIMIT ")
            .push_bind(limit_i64(limit, "video task top models limit")?);
        let rows = builder.build().fetch_all(&self.pool).await.map_sql_err()?;
        rows.iter()
            .map(|row| {
                Ok(VideoTaskModelCount {
                    model: row.try_get("model").map_sql_err()?,
                    count: count_value(row.try_get("total").map_sql_err()?)?,
                })
            })
            .collect()
    }

    async fn count_created_since(
        &self,
        filter: &VideoTaskQueryFilter,
        created_since_unix_secs: u64,
    ) -> Result<u64, DataLayerError> {
        let mut builder = QueryBuilder::<MySql>::new("SELECT COUNT(id) AS total FROM video_tasks");
        push_filter(&mut builder, filter, Some(created_since_unix_secs));
        count_query(builder, &self.pool).await
    }
}

#[async_trait]
impl VideoTaskWriteRepository for MysqlVideoTaskRepository {
    async fn upsert(&self, mut task: UpsertVideoTask) -> Result<StoredVideoTask, DataLayerError> {
        task.sanitize_for_persistence();
        let id = task.id.clone();
        let expected_identity = task.clone();
        bind_task(sqlx::query(upsert_sql()), task, true, false)?
            .execute(&self.pool)
            .await
            .map_sql_err()?;
        let stored = self.find_by_id(&id).await?.ok_or_else(|| {
            DataLayerError::InvalidInput(format!(
                "video task {id} conflicts with persisted immutable identity"
            ))
        })?;
        stored.ensure_immutable_identity_matches(&expected_identity)?;
        Ok(stored)
    }

    async fn update_if_active(
        &self,
        task: UpsertVideoTask,
    ) -> Result<Option<StoredVideoTask>, DataLayerError> {
        let id = task.id.clone();
        let rows_affected = bind_task(sqlx::query(UPDATE_IF_ACTIVE_SQL), task, false, true)?
            .execute(&self.pool)
            .await
            .map_sql_err()?
            .rows_affected();
        if rows_affected == 0 {
            return Ok(None);
        }
        self.find_by_id(&id).await
    }

    async fn claim_due(
        &self,
        now_unix_secs: u64,
        claim_until_unix_secs: u64,
        limit: usize,
    ) -> Result<Vec<StoredVideoTask>, DataLayerError> {
        if limit == 0 {
            return Ok(Vec::new());
        }
        let now = u64_to_i64(now_unix_secs, "video task now")?;
        let claim_until = u64_to_i64(claim_until_unix_secs, "video task claim_until")?;
        let limit = limit_i64(limit, "due video task claim limit")?;
        let mut tx = self.pool.begin().await.map_sql_err()?;
        let due_rows = sqlx::query(
            r#"
SELECT id
FROM video_tasks
WHERE status IN ('submitted', 'queued', 'processing')
  AND next_poll_at IS NOT NULL
  AND next_poll_at <= ?
  AND poll_count < max_poll_count
ORDER BY next_poll_at ASC, updated_at ASC
LIMIT ?
FOR UPDATE SKIP LOCKED
"#,
        )
        .bind(now)
        .bind(limit)
        .fetch_all(&mut *tx)
        .await
        .map_sql_err()?;
        let ids = due_rows
            .iter()
            .map(|row| row.try_get::<String, _>("id").map_sql_err())
            .collect::<Result<Vec<_>, _>>()?;
        if ids.is_empty() {
            tx.commit().await.map_sql_err()?;
            return Ok(Vec::new());
        }

        let mut update = QueryBuilder::<MySql>::new("UPDATE video_tasks SET next_poll_at = ");
        update
            .push_bind(claim_until)
            .push(", updated_at = GREATEST(updated_at, ")
            .push_bind(now)
            .push(") WHERE id IN (");
        {
            let mut separated = update.separated(", ");
            for id in &ids {
                separated.push_bind(id);
            }
        }
        update
            .push(") AND status IN ('submitted', 'queued', 'processing')")
            .push(" AND next_poll_at IS NOT NULL AND next_poll_at <= ")
            .push_bind(now)
            .push(" AND poll_count < max_poll_count");
        update.build().execute(&mut *tx).await.map_sql_err()?;

        let mut reload = QueryBuilder::<MySql>::new(VIDEO_TASK_COLUMNS);
        reload.push(" WHERE id IN (");
        {
            let mut separated = reload.separated(", ");
            for id in &ids {
                separated.push_bind(id);
            }
        }
        reload.push(")");
        let rows = reload.build().fetch_all(&mut *tx).await.map_sql_err()?;
        let mut tasks = rows
            .iter()
            .map(map_video_task_row)
            .collect::<Result<Vec<_>, _>>()?;
        tasks.sort_by(|left, right| {
            left.next_poll_at_unix_secs
                .cmp(&right.next_poll_at_unix_secs)
                .then_with(|| left.updated_at_unix_secs.cmp(&right.updated_at_unix_secs))
        });
        tx.commit().await.map_sql_err()?;
        Ok(tasks)
    }
}

const IMMUTABLE_IDENTITY_MATCH_SQL: &str = r#"BINARY id <=> BINARY VALUES(id)
    AND BINARY short_id <=> BINARY VALUES(short_id)
    AND BINARY request_id <=> BINARY VALUES(request_id)
    AND BINARY user_id <=> BINARY VALUES(user_id)
    AND BINARY api_key_id <=> BINARY VALUES(api_key_id)
    AND BINARY external_task_id <=> BINARY VALUES(external_task_id)
    AND BINARY provider_id <=> BINARY VALUES(provider_id)
    AND BINARY endpoint_id <=> BINARY VALUES(endpoint_id)
    AND BINARY key_id <=> BINARY VALUES(key_id)
    AND BINARY client_api_format <=> BINARY VALUES(client_api_format)
    AND BINARY provider_api_format <=> BINARY VALUES(provider_api_format)
    AND format_converted <=> VALUES(format_converted)
    AND BINARY model <=> BINARY VALUES(model)
    AND duration_seconds <=> VALUES(duration_seconds)
    AND BINARY resolution <=> BINARY VALUES(resolution)
    AND BINARY aspect_ratio <=> BINARY VALUES(aspect_ratio)
    AND BINARY size <=> BINARY VALUES(size)"#;

const UPSERT_UPDATE_COLUMNS: &[&str] = &[
    "short_id",
    "request_id",
    "user_id",
    "api_key_id",
    "username",
    "api_key_name",
    "external_task_id",
    "provider_id",
    "endpoint_id",
    "key_id",
    "client_api_format",
    "provider_api_format",
    "format_converted",
    "model",
    "prompt",
    "original_request_body",
    "duration_seconds",
    "resolution",
    "aspect_ratio",
    "size",
    "status",
    "progress_percent",
    "progress_message",
    "retry_count",
    "poll_interval_seconds",
    "next_poll_at",
    "poll_count",
    "max_poll_count",
    "video_url",
    "error_code",
    "error_message",
    "request_metadata",
    "submitted_at",
    "completed_at",
    "updated_at",
];

fn upsert_sql() -> &'static str {
    static SQL: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    SQL.get_or_init(|| {
        let guarded_updates = UPSERT_UPDATE_COLUMNS
            .iter()
            .map(|column| {
                format!(
                    "  {column} = IF(({IMMUTABLE_IDENTITY_MATCH_SQL}), VALUES({column}), {column})"
                )
            })
            .collect::<Vec<_>>()
            .join(",\n");
        format!(
            r#"
INSERT INTO video_tasks (
  id, short_id, request_id, user_id, api_key_id, username, api_key_name,
  external_task_id, provider_id, endpoint_id, key_id, client_api_format,
  provider_api_format, format_converted, model, prompt, original_request_body,
  duration_seconds, resolution, aspect_ratio, size, status, progress_percent,
  progress_message, retry_count, poll_interval_seconds, next_poll_at, poll_count,
  max_poll_count, video_url, error_code, error_message, request_metadata,
  created_at, submitted_at, completed_at, updated_at
)
VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
ON DUPLICATE KEY UPDATE
{guarded_updates}
"#
        )
    })
}

const UPDATE_IF_ACTIVE_SQL: &str = r#"
UPDATE video_tasks SET
  short_id = ?,
  request_id = ?,
  user_id = ?,
  api_key_id = ?,
  username = ?,
  api_key_name = ?,
  external_task_id = ?,
  provider_id = ?,
  endpoint_id = ?,
  key_id = ?,
  client_api_format = ?,
  provider_api_format = ?,
  format_converted = ?,
  model = ?,
  prompt = ?,
  original_request_body = ?,
  duration_seconds = ?,
  resolution = ?,
  aspect_ratio = ?,
  size = ?,
  status = ?,
  progress_percent = ?,
  progress_message = ?,
  retry_count = ?,
  poll_interval_seconds = ?,
  next_poll_at = ?,
  poll_count = ?,
  max_poll_count = ?,
  video_url = ?,
  error_code = ?,
  error_message = ?,
  request_metadata = ?,
  created_at = COALESCE(created_at, ?),
  submitted_at = ?,
  completed_at = ?,
  updated_at = ?
WHERE id = ?
  AND status IN ('pending', 'submitted', 'queued', 'processing')
  AND BINARY short_id <=> BINARY ?
  AND BINARY request_id <=> BINARY ?
  AND BINARY user_id <=> BINARY ?
  AND BINARY api_key_id <=> BINARY ?
  AND BINARY external_task_id <=> BINARY ?
  AND BINARY provider_id <=> BINARY ?
  AND BINARY endpoint_id <=> BINARY ?
  AND BINARY key_id <=> BINARY ?
  AND BINARY client_api_format <=> BINARY ?
  AND BINARY provider_api_format <=> BINARY ?
  AND format_converted <=> ?
  AND BINARY model <=> BINARY ?
  AND duration_seconds <=> ?
  AND BINARY resolution <=> BINARY ?
  AND BINARY aspect_ratio <=> BINARY ?
  AND BINARY size <=> BINARY ?
"#;

fn bind_task<'q>(
    query: sqlx::query::Query<'q, MySql, sqlx::mysql::MySqlArguments>,
    mut task: UpsertVideoTask,
    include_insert_id: bool,
    include_update_id: bool,
) -> Result<sqlx::query::Query<'q, MySql, sqlx::mysql::MySqlArguments>, DataLayerError> {
    task.sanitize_for_persistence();
    let identity = task.clone();
    let original_request_body = json_to_string(&task.original_request_body)?;
    let request_metadata = json_to_string(&task.request_metadata)?;
    let query = if include_insert_id {
        query.bind(task.id.clone())
    } else {
        query
    };
    let bound = query
        .bind(task.short_id)
        .bind(task.request_id)
        .bind(task.user_id)
        .bind(task.api_key_id)
        .bind(task.username)
        .bind(task.api_key_name)
        .bind(task.external_task_id)
        .bind(task.provider_id)
        .bind(task.endpoint_id)
        .bind(task.key_id)
        .bind(task.client_api_format)
        .bind(task.provider_api_format)
        .bind(task.format_converted)
        .bind(task.model)
        .bind(task.prompt)
        .bind(original_request_body)
        .bind(optional_u32_to_i32(
            task.duration_seconds,
            "video task duration_seconds",
        )?)
        .bind(task.resolution)
        .bind(task.aspect_ratio)
        .bind(task.size)
        .bind(status_to_database(task.status))
        .bind(i32::from(task.progress_percent))
        .bind(task.progress_message)
        .bind(u32_to_i32(task.retry_count, "video task retry_count")?)
        .bind(u32_to_i32(
            task.poll_interval_seconds,
            "video task poll_interval_seconds",
        )?)
        .bind(optional_u64_to_i64(
            task.next_poll_at_unix_secs,
            "video task next_poll_at",
        )?)
        .bind(u32_to_i32(task.poll_count, "video task poll_count")?)
        .bind(u32_to_i32(
            task.max_poll_count,
            "video task max_poll_count",
        )?)
        .bind(task.video_url)
        .bind(task.error_code)
        .bind(task.error_message)
        .bind(request_metadata)
        .bind(u64_to_i64(
            task.created_at_unix_ms,
            "video task created_at",
        )?)
        .bind(optional_u64_to_i64(
            task.submitted_at_unix_secs,
            "video task submitted_at",
        )?)
        .bind(optional_u64_to_i64(
            task.completed_at_unix_secs,
            "video task completed_at",
        )?)
        .bind(u64_to_i64(
            task.updated_at_unix_secs,
            "video task updated_at",
        )?);
    if include_update_id {
        bind_identity_guard(bound.bind(task.id), identity)
    } else {
        Ok(bound)
    }
}

fn bind_identity_guard<'q>(
    query: sqlx::query::Query<'q, MySql, sqlx::mysql::MySqlArguments>,
    identity: UpsertVideoTask,
) -> Result<sqlx::query::Query<'q, MySql, sqlx::mysql::MySqlArguments>, DataLayerError> {
    Ok(query
        .bind(identity.short_id)
        .bind(identity.request_id)
        .bind(identity.user_id)
        .bind(identity.api_key_id)
        .bind(identity.external_task_id)
        .bind(identity.provider_id)
        .bind(identity.endpoint_id)
        .bind(identity.key_id)
        .bind(identity.client_api_format)
        .bind(identity.provider_api_format)
        .bind(identity.format_converted)
        .bind(identity.model)
        .bind(optional_u32_to_i32(
            identity.duration_seconds,
            "video task duration_seconds",
        )?)
        .bind(identity.resolution)
        .bind(identity.aspect_ratio)
        .bind(identity.size))
}

fn push_filter<'args>(
    builder: &mut QueryBuilder<'args, MySql>,
    filter: &'args VideoTaskQueryFilter,
    created_since_unix_secs: Option<u64>,
) {
    if let Some(user_id) = filter.user_id.as_deref() {
        push_clause(builder, "BINARY user_id = BINARY ");
        builder.push_bind(user_id);
    }
    if let Some(status) = filter.status {
        push_clause(builder, "status = ");
        builder.push_bind(status_to_database(status));
    }
    if let Some(model_substring) = filter.model_substring.as_deref() {
        push_clause(builder, "LOWER(model) LIKE ");
        builder.push_bind(format!(
            "%{}%",
            escape_like_pattern(&model_substring.trim().to_ascii_lowercase())
        ));
        builder.push(" ESCAPE '\\'");
    }
    if let Some(client_api_format) = filter.client_api_format.as_deref() {
        push_clause(builder, "client_api_format = ");
        builder.push_bind(client_api_format);
    }
    if let Some(created_since_unix_secs) = created_since_unix_secs {
        push_clause(builder, "created_at >= ");
        builder.push_bind(created_since_unix_secs as i64);
    }
}

fn push_clause<'args>(builder: &mut QueryBuilder<'args, MySql>, clause: &str) {
    let sql = builder.sql();
    if sql.contains(" WHERE ") || sql.contains("\nWHERE ") {
        builder.push(" AND ");
    } else {
        builder.push(" WHERE ");
    }
    builder.push(clause);
}

async fn count_query(
    mut builder: QueryBuilder<'_, MySql>,
    pool: &MysqlPool,
) -> Result<u64, DataLayerError> {
    let row = builder.build().fetch_one(pool).await.map_sql_err()?;
    count_value(row.try_get("total").map_sql_err()?)
}

fn count_value(value: i64) -> Result<u64, DataLayerError> {
    u64::try_from(value).map_err(|_| {
        DataLayerError::UnexpectedValue(format!("invalid video task count result: {value}"))
    })
}

fn map_video_task_row(row: &MySqlRow) -> Result<StoredVideoTask, DataLayerError> {
    StoredVideoTask::new(
        row.try_get("id").map_sql_err()?,
        row.try_get("short_id").map_sql_err()?,
        row.try_get("request_id").map_sql_err()?,
        row.try_get("user_id").map_sql_err()?,
        row.try_get("api_key_id").map_sql_err()?,
        row.try_get("username").map_sql_err()?,
        row.try_get("api_key_name").map_sql_err()?,
        row.try_get("external_task_id").map_sql_err()?,
        row.try_get("provider_id").map_sql_err()?,
        row.try_get("endpoint_id").map_sql_err()?,
        row.try_get("key_id").map_sql_err()?,
        row.try_get("client_api_format").map_sql_err()?,
        row.try_get("provider_api_format").map_sql_err()?,
        row.try_get("format_converted").map_sql_err()?,
        row.try_get("model").map_sql_err()?,
        row.try_get("prompt").map_sql_err()?,
        parse_json(row.try_get("original_request_body").ok().flatten())?,
        row.try_get("duration_seconds").map_sql_err()?,
        row.try_get("resolution").map_sql_err()?,
        row.try_get("aspect_ratio").map_sql_err()?,
        row.try_get("size").map_sql_err()?,
        VideoTaskStatus::from_database(row.try_get::<String, _>("status").map_sql_err()?.as_str())?,
        row.try_get("progress_percent").map_sql_err()?,
        row.try_get("progress_message").map_sql_err()?,
        row.try_get("retry_count").map_sql_err()?,
        row.try_get("poll_interval_seconds").map_sql_err()?,
        row.try_get("next_poll_at_unix_secs").map_sql_err()?,
        row.try_get("poll_count").map_sql_err()?,
        row.try_get("max_poll_count").map_sql_err()?,
        row.try_get("created_at_unix_ms").map_sql_err()?,
        row.try_get("submitted_at_unix_secs").map_sql_err()?,
        row.try_get("completed_at_unix_secs").map_sql_err()?,
        row.try_get("updated_at_unix_secs").map_sql_err()?,
        row.try_get("error_code").map_sql_err()?,
        row.try_get("error_message").map_sql_err()?,
        row.try_get("video_url").map_sql_err()?,
        parse_json(row.try_get("request_metadata").ok().flatten())?,
    )
}

fn parse_json(value: Option<String>) -> Result<Option<serde_json::Value>, DataLayerError> {
    value
        .filter(|value| !value.trim().is_empty())
        .map(|value| {
            serde_json::from_str(&value).map_err(|err| {
                DataLayerError::UnexpectedValue(format!("video task JSON field is invalid: {err}"))
            })
        })
        .transpose()
}

fn json_to_string(value: &Option<serde_json::Value>) -> Result<Option<String>, DataLayerError> {
    value
        .as_ref()
        .map(|value| {
            serde_json::to_string(value).map_err(|err| {
                DataLayerError::UnexpectedValue(format!(
                    "video task JSON field is unserializable: {err}"
                ))
            })
        })
        .transpose()
}

fn status_to_database(status: VideoTaskStatus) -> &'static str {
    match status {
        VideoTaskStatus::Pending => "pending",
        VideoTaskStatus::Submitted => "submitted",
        VideoTaskStatus::Queued => "queued",
        VideoTaskStatus::Processing => "processing",
        VideoTaskStatus::Completed => "completed",
        VideoTaskStatus::Failed => "failed",
        VideoTaskStatus::Cancelled => "cancelled",
        VideoTaskStatus::Expired => "expired",
        VideoTaskStatus::Deleted => "deleted",
    }
}

fn escape_like_pattern(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_")
}

fn limit_i64(value: usize, name: &str) -> Result<i64, DataLayerError> {
    i64::try_from(value)
        .map_err(|_| DataLayerError::UnexpectedValue(format!("invalid {name}: {value}")))
}

fn u64_to_i64(value: u64, name: &str) -> Result<i64, DataLayerError> {
    i64::try_from(value).map_err(|_| DataLayerError::UnexpectedValue(format!("{name} overflow")))
}

fn optional_u64_to_i64(value: Option<u64>, name: &str) -> Result<Option<i64>, DataLayerError> {
    value.map(|value| u64_to_i64(value, name)).transpose()
}

fn u32_to_i32(value: u32, name: &str) -> Result<i32, DataLayerError> {
    i32::try_from(value).map_err(|_| DataLayerError::UnexpectedValue(format!("{name} overflow")))
}

fn optional_u32_to_i32(value: Option<u32>, name: &str) -> Result<Option<i32>, DataLayerError> {
    value.map(|value| u32_to_i32(value, name)).transpose()
}

#[cfg(test)]
mod tests {
    use super::{
        upsert_sql, MysqlVideoTaskRepository, IMMUTABLE_IDENTITY_MATCH_SQL, UPDATE_IF_ACTIVE_SQL,
        UPSERT_UPDATE_COLUMNS,
    };
    use crate::run_migrations;
    use aether_data_contracts::repository::video_tasks::{
        UpsertVideoTask, VideoTaskStatus, VideoTaskWriteRepository,
    };
    use std::sync::Arc;

    #[test]
    fn mysql_write_sql_atomically_guards_immutable_identity() {
        assert!(IMMUTABLE_IDENTITY_MATCH_SQL.contains("BINARY id <=> BINARY VALUES(id)"));
        for column in [
            "short_id",
            "request_id",
            "user_id",
            "api_key_id",
            "external_task_id",
            "provider_id",
            "endpoint_id",
            "key_id",
            "client_api_format",
            "provider_api_format",
            "model",
            "resolution",
            "aspect_ratio",
            "size",
        ] {
            assert!(
                IMMUTABLE_IDENTITY_MATCH_SQL
                    .contains(&format!("BINARY {column} <=> BINARY VALUES({column})")),
                "upsert identity predicate should guard {column}"
            );
        }
        for column in ["format_converted", "duration_seconds"] {
            assert!(
                IMMUTABLE_IDENTITY_MATCH_SQL.contains(&format!("{column} <=> VALUES({column})")),
                "upsert identity predicate should guard {column}"
            );
        }

        let upsert = upsert_sql();
        assert!(!UPSERT_UPDATE_COLUMNS.contains(&"created_at"));
        for column in UPSERT_UPDATE_COLUMNS {
            assert!(
                upsert.contains(&format!("{column} = IF((BINARY id <=> BINARY VALUES(id)")),
                "upsert assignment should be conditional for {column}"
            );
        }
        for column in [
            "short_id",
            "request_id",
            "user_id",
            "api_key_id",
            "external_task_id",
            "provider_id",
            "endpoint_id",
            "key_id",
            "client_api_format",
            "provider_api_format",
            "model",
            "resolution",
            "aspect_ratio",
            "size",
        ] {
            assert!(
                UPDATE_IF_ACTIVE_SQL.contains(&format!("BINARY {column} <=> BINARY ?")),
                "active update should guard {column}"
            );
        }
        for column in ["format_converted", "duration_seconds"] {
            assert!(
                UPDATE_IF_ACTIVE_SQL.contains(&format!("{column} <=> ?")),
                "active update should guard {column}"
            );
        }
        assert!(UPDATE_IF_ACTIVE_SQL.contains("created_at = COALESCE(created_at, ?)"));
    }

    #[tokio::test]
    async fn repository_builds_from_lazy_pool() {
        let pool = sqlx::mysql::MySqlPoolOptions::new().connect_lazy_with(
            "mysql://user:pass@localhost:3306/aether"
                .parse()
                .expect("mysql options should parse"),
        );

        let _repository = MysqlVideoTaskRepository::new(pool);
    }

    #[tokio::test]
    async fn mysql_claim_due_does_not_return_one_task_to_multiple_workers_when_url_is_set() {
        let Some(database_url) = std::env::var("AETHER_TEST_MYSQL_URL")
            .ok()
            .filter(|value| !value.trim().is_empty())
        else {
            eprintln!(
                "skipping mysql claim_due concurrency test because AETHER_TEST_MYSQL_URL is unset"
            );
            return;
        };

        let pool = sqlx::mysql::MySqlPoolOptions::new()
            .max_connections(4)
            .connect(&database_url)
            .await
            .expect("mysql test pool should connect");
        run_migrations(&pool)
            .await
            .expect("mysql migrations should run");

        let suffix = uuid::Uuid::new_v4().simple().to_string();
        let task_id = format!("claim-{}", &suffix[..20]);
        let repository = MysqlVideoTaskRepository::new(pool.clone());
        repository
            .upsert(claimable_task(&task_id))
            .await
            .expect("claimable mysql task should insert");

        let barrier = Arc::new(tokio::sync::Barrier::new(3));
        let first_repository = repository.clone();
        let first_barrier = barrier.clone();
        let first = tokio::spawn(async move {
            first_barrier.wait().await;
            first_repository.claim_due(100, 130, 1).await
        });
        let second_repository = repository.clone();
        let second_barrier = barrier.clone();
        let second = tokio::spawn(async move {
            second_barrier.wait().await;
            second_repository.claim_due(100, 130, 1).await
        });
        barrier.wait().await;

        let first_result = first.await;
        let second_result = second.await;
        let followup_result = repository.claim_due(100, 130, 1).await;
        let cleanup_result = sqlx::query("DELETE FROM video_tasks WHERE id = ?")
            .bind(&task_id)
            .execute(&pool)
            .await;

        let first_claimed = first_result
            .expect("first mysql claim worker should join")
            .expect("first mysql claim should execute");
        let second_claimed = second_result
            .expect("second mysql claim worker should join")
            .expect("second mysql claim should execute");
        let followup_claimed = followup_result.expect("follow-up mysql claim should execute");
        cleanup_result.expect("mysql claim fixture should clean up");

        let claimed_ids = first_claimed
            .iter()
            .chain(&second_claimed)
            .map(|task| task.id.as_str())
            .collect::<Vec<_>>();
        assert_eq!(claimed_ids, vec![task_id.as_str()]);
        assert!(followup_claimed.is_empty());
    }

    fn claimable_task(id: &str) -> UpsertVideoTask {
        UpsertVideoTask {
            id: id.to_string(),
            short_id: None,
            request_id: format!("request-{id}"),
            user_id: None,
            api_key_id: None,
            username: None,
            api_key_name: None,
            external_task_id: Some(format!("external-{id}")),
            provider_id: None,
            endpoint_id: None,
            key_id: None,
            client_api_format: Some("openai:video".to_string()),
            provider_api_format: Some("openai:video".to_string()),
            format_converted: false,
            model: Some("sora-2".to_string()),
            prompt: Some("claim test".to_string()),
            original_request_body: None,
            duration_seconds: None,
            resolution: None,
            aspect_ratio: None,
            size: None,
            status: VideoTaskStatus::Submitted,
            progress_percent: 0,
            progress_message: None,
            retry_count: 0,
            poll_interval_seconds: 10,
            next_poll_at_unix_secs: Some(100),
            poll_count: 0,
            max_poll_count: 360,
            created_at_unix_ms: 90,
            submitted_at_unix_secs: Some(90),
            completed_at_unix_secs: None,
            updated_at_unix_secs: 100,
            error_code: None,
            error_message: None,
            video_url: None,
            request_metadata: None,
        }
    }
}
