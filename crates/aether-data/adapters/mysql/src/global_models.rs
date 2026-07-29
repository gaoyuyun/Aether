use async_trait::async_trait;
use sqlx::{mysql::MySqlRow, MySql, QueryBuilder, Row};

use aether_data_contracts::repository::global_models::{
    metadata_supports_embedding, AdminGlobalModelListQuery, AdminProviderModelListQuery,
    CreateAdminGlobalModelRecord, GlobalModelReadRepository, GlobalModelWriteRepository,
    PublicCatalogModelListQuery, PublicCatalogModelSearchQuery, PublicGlobalModelQuery,
    StoredAdminGlobalModel, StoredAdminGlobalModelPage, StoredAdminProviderModel,
    StoredGlobalModelIdentity, StoredProviderActiveGlobalModel, StoredProviderModelStats,
    StoredPublicCatalogModel, StoredPublicGlobalModel, StoredPublicGlobalModelPage,
    UpdateAdminGlobalModelRecord, UpsertAdminProviderModelRecord,
};
use aether_data_contracts::DataLayerError;

use crate::error::SqlResultExt;
use crate::MysqlPool;

const LIST_PUBLIC_GLOBAL_MODELS_PREFIX: &str = r#"
SELECT
  id,
  name,
  display_name,
  is_active,
  CAST(default_price_per_request AS DOUBLE) AS default_price_per_request,
  CAST(default_tiered_pricing AS CHAR) AS default_tiered_pricing,
  CAST(supported_capabilities AS CHAR) AS supported_capabilities,
  CAST(config AS CHAR) AS config,
  CAST(0 AS SIGNED) AS usage_count
FROM global_models
"#;

const COUNT_PUBLIC_GLOBAL_MODELS_PREFIX: &str = r#"
SELECT CAST(COUNT(id) AS SIGNED) AS total
FROM global_models
"#;

const LIST_PUBLIC_CATALOG_MODELS_PREFIX: &str = r#"
SELECT
  m.id,
  m.provider_id,
  p.name AS provider_name,
  p.is_active AS provider_is_active,
  m.provider_model_name,
  COALESCE(gm.name, m.provider_model_name) AS name,
  COALESCE(NULLIF(gm.display_name, ''), m.provider_model_name) AS display_name,
  CAST(gm.config AS CHAR) AS global_model_config,
  CAST(gm.supported_capabilities AS CHAR) AS global_model_supported_capabilities,
  CAST(m.config AS CHAR) AS model_config,
  CAST(m.tiered_pricing AS CHAR) AS tiered_pricing,
  CAST(gm.default_tiered_pricing AS CHAR) AS default_tiered_pricing,
  COALESCE(
    m.supports_vision,
    CASE
      WHEN JSON_EXTRACT(gm.config, '$.vision') IS NULL THEN NULL
      WHEN LOWER(JSON_UNQUOTE(JSON_EXTRACT(gm.config, '$.vision'))) IN ('true', '1') THEN 1
      ELSE 0
    END,
    0
  ) AS supports_vision,
  COALESCE(
    m.supports_function_calling,
    CASE
      WHEN JSON_EXTRACT(gm.config, '$.function_calling') IS NULL THEN NULL
      WHEN LOWER(JSON_UNQUOTE(JSON_EXTRACT(gm.config, '$.function_calling'))) IN ('true', '1') THEN 1
      ELSE 0
    END,
    0
  ) AS supports_function_calling,
  COALESCE(
    m.supports_streaming,
    CASE
      WHEN JSON_EXTRACT(gm.config, '$.streaming') IS NULL THEN NULL
      WHEN LOWER(JSON_UNQUOTE(JSON_EXTRACT(gm.config, '$.streaming'))) IN ('true', '1') THEN 1
      ELSE 0
    END,
    1
  ) AS supports_streaming,
  m.is_active,
  gm.is_active AS global_model_is_active
FROM models m
JOIN providers p ON p.id = m.provider_id
LEFT JOIN global_models gm ON gm.id = m.global_model_id
"#;

const LIST_PROVIDER_MODEL_STATS_PREFIX: &str = r#"
SELECT
  provider_id,
  CAST(COUNT(id) AS SIGNED) AS total_models,
  CAST(COALESCE(SUM(CASE WHEN is_active = 1 THEN 1 ELSE 0 END), 0) AS SIGNED) AS active_models
FROM models
WHERE provider_id IN (
"#;

const LIST_ADMIN_PROVIDER_MODELS_PREFIX: &str = r#"
SELECT
  m.id,
  m.provider_id,
  m.global_model_id,
  m.provider_model_name,
  CAST(m.provider_model_mappings AS CHAR) AS provider_model_mappings,
  CAST(m.price_per_request AS DOUBLE) AS price_per_request,
  CAST(m.tiered_pricing AS CHAR) AS tiered_pricing,
  m.supports_vision,
  m.supports_function_calling,
  m.supports_streaming,
  m.supports_extended_thinking,
  m.supports_image_generation,
  m.is_active,
  COALESCE(m.is_available, 1) AS is_available,
  CAST(m.config AS CHAR) AS config,
  m.created_at AS created_at_unix_ms,
  m.updated_at AS updated_at_unix_secs,
  gm.name AS global_model_name,
  gm.display_name AS global_model_display_name,
  CAST(gm.default_price_per_request AS DOUBLE) AS global_model_default_price_per_request,
  CAST(gm.default_tiered_pricing AS CHAR) AS global_model_default_tiered_pricing,
  CAST(gm.supported_capabilities AS CHAR) AS global_model_supported_capabilities,
  CAST(gm.config AS CHAR) AS global_model_config
FROM models m
LEFT JOIN global_models gm ON gm.id = m.global_model_id
"#;

const LIST_ADMIN_GLOBAL_MODELS_PREFIX: &str = r#"
SELECT
  gm.id,
  gm.name,
  COALESCE(NULLIF(gm.display_name, ''), gm.name) AS display_name,
  gm.is_active,
  CAST(gm.default_price_per_request AS DOUBLE) AS default_price_per_request,
  CAST(gm.default_tiered_pricing AS CHAR) AS default_tiered_pricing,
  CAST(gm.supported_capabilities AS CHAR) AS supported_capabilities,
  CAST(gm.config AS CHAR) AS config,
  COALESCE(gm_stats.provider_count, 0) AS provider_count,
  COALESCE(gm_stats.active_provider_count, 0) AS active_provider_count,
  CAST(COALESCE(gm.usage_count, 0) AS SIGNED) AS usage_count,
  gm.created_at AS created_at_unix_ms,
  gm.updated_at AS updated_at_unix_secs
FROM global_models gm
LEFT JOIN (
  SELECT
    m.global_model_id,
    CAST(COUNT(DISTINCT m.provider_id) AS SIGNED) AS provider_count,
    CAST(COUNT(
      DISTINCT CASE
        WHEN m.is_active = 1 AND COALESCE(m.is_available, 1) = 1 AND p.is_active = 1 THEN m.provider_id
        ELSE NULL
      END
    ) AS SIGNED) AS active_provider_count
  FROM models m
  JOIN providers p ON p.id = m.provider_id
  GROUP BY m.global_model_id
) gm_stats ON gm_stats.global_model_id = gm.id
"#;

const COUNT_ADMIN_GLOBAL_MODELS_PREFIX: &str = r#"
SELECT CAST(COUNT(id) AS SIGNED) AS total
FROM global_models gm
"#;

const LIST_ACTIVE_GLOBAL_MODEL_IDS_BY_PROVIDER_IDS_PREFIX: &str = r#"
SELECT DISTINCT
  provider_id,
  global_model_id
FROM models
WHERE provider_id IN (
"#;

#[derive(Debug, Clone)]
pub struct MysqlGlobalModelReadRepository {
    pool: MysqlPool,
}

impl MysqlGlobalModelReadRepository {
    pub fn new(pool: MysqlPool) -> Self {
        Self { pool }
    }

    pub async fn create_admin_provider_model(
        &self,
        record: &UpsertAdminProviderModelRecord,
    ) -> Result<Option<StoredAdminProviderModel>, DataLayerError> {
        let now = current_unix_secs();
        sqlx::query(
            r#"
INSERT INTO models (
  id,
  provider_id,
  global_model_id,
  provider_model_name,
  provider_model_mappings,
  price_per_request,
  tiered_pricing,
  supports_vision,
  supports_function_calling,
  supports_streaming,
  supports_extended_thinking,
  supports_image_generation,
  is_active,
  is_available,
  config,
  created_at,
  updated_at
)
VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
"#,
        )
        .bind(&record.id)
        .bind(&record.provider_id)
        .bind(&record.global_model_id)
        .bind(&record.provider_model_name)
        .bind(optional_json_to_string(
            &record.provider_model_mappings,
            "models.provider_model_mappings",
        )?)
        .bind(record.price_per_request)
        .bind(optional_json_to_string(
            &record.tiered_pricing,
            "models.tiered_pricing",
        )?)
        .bind(record.supports_vision)
        .bind(record.supports_function_calling)
        .bind(record.supports_streaming)
        .bind(record.supports_extended_thinking)
        .bind(record.supports_image_generation)
        .bind(record.is_active)
        .bind(record.is_available)
        .bind(optional_json_to_string(&record.config, "models.config")?)
        .bind(now as i64)
        .bind(now as i64)
        .execute(&self.pool)
        .await
        .map_sql_err()?;

        self.get_admin_provider_model(&record.provider_id, &record.id)
            .await
    }

    pub async fn update_admin_provider_model(
        &self,
        record: &UpsertAdminProviderModelRecord,
    ) -> Result<Option<StoredAdminProviderModel>, DataLayerError> {
        let now = current_unix_secs();
        let updated = sqlx::query(
            r#"
UPDATE models
SET
  global_model_id = ?,
  provider_model_name = ?,
  provider_model_mappings = ?,
  price_per_request = ?,
  tiered_pricing = ?,
  supports_vision = ?,
  supports_function_calling = ?,
  supports_streaming = ?,
  supports_extended_thinking = ?,
  supports_image_generation = ?,
  is_active = ?,
  is_available = ?,
  config = ?,
  updated_at = ?
WHERE id = ?
  AND provider_id = ?
"#,
        )
        .bind(&record.global_model_id)
        .bind(&record.provider_model_name)
        .bind(optional_json_to_string(
            &record.provider_model_mappings,
            "models.provider_model_mappings",
        )?)
        .bind(record.price_per_request)
        .bind(optional_json_to_string(
            &record.tiered_pricing,
            "models.tiered_pricing",
        )?)
        .bind(record.supports_vision)
        .bind(record.supports_function_calling)
        .bind(record.supports_streaming)
        .bind(record.supports_extended_thinking)
        .bind(record.supports_image_generation)
        .bind(record.is_active)
        .bind(record.is_available)
        .bind(optional_json_to_string(&record.config, "models.config")?)
        .bind(now as i64)
        .bind(&record.id)
        .bind(&record.provider_id)
        .execute(&self.pool)
        .await
        .map_sql_err()?;

        if updated.rows_affected() == 0 {
            return Ok(None);
        }

        self.get_admin_provider_model(&record.provider_id, &record.id)
            .await
    }

    pub async fn delete_admin_provider_model(
        &self,
        provider_id: &str,
        model_id: &str,
    ) -> Result<bool, DataLayerError> {
        let deleted = sqlx::query(
            r#"
DELETE FROM models
WHERE provider_id = ?
  AND id = ?
"#,
        )
        .bind(provider_id)
        .bind(model_id)
        .execute(&self.pool)
        .await
        .map_sql_err()?;

        Ok(deleted.rows_affected() > 0)
    }

    pub async fn create_admin_global_model(
        &self,
        record: &CreateAdminGlobalModelRecord,
    ) -> Result<Option<StoredAdminGlobalModel>, DataLayerError> {
        let now = current_unix_secs();
        let usage_count =
            optional_admin_global_model_usage_count_i64(record.usage_count)?.unwrap_or_default();
        sqlx::query(
            r#"
INSERT INTO global_models (
  id,
  name,
  display_name,
  is_active,
  default_price_per_request,
  default_tiered_pricing,
  supported_capabilities,
  usage_count,
  config,
  created_at,
  updated_at
)
VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
"#,
        )
        .bind(&record.id)
        .bind(&record.name)
        .bind(&record.display_name)
        .bind(record.is_active)
        .bind(record.default_price_per_request)
        .bind(optional_json_to_string(
            &record.default_tiered_pricing,
            "global_models.default_tiered_pricing",
        )?)
        .bind(optional_json_to_string(
            &record.supported_capabilities,
            "global_models.supported_capabilities",
        )?)
        .bind(usage_count)
        .bind(optional_json_to_string(
            &record.config,
            "global_models.config",
        )?)
        .bind(now as i64)
        .bind(now as i64)
        .execute(&self.pool)
        .await
        .map_sql_err()?;

        self.get_admin_global_model_by_id(&record.id).await
    }

    pub async fn update_admin_global_model(
        &self,
        record: &UpdateAdminGlobalModelRecord,
    ) -> Result<Option<StoredAdminGlobalModel>, DataLayerError> {
        let now = current_unix_secs();
        let usage_count = optional_admin_global_model_usage_count_i64(record.usage_count)?;
        let updated = sqlx::query(
            r#"
UPDATE global_models
SET
  display_name = ?,
  is_active = ?,
  default_price_per_request = ?,
  default_tiered_pricing = ?,
  supported_capabilities = ?,
  config = ?,
  usage_count = COALESCE(?, usage_count),
  updated_at = ?
WHERE id = ?
"#,
        )
        .bind(&record.display_name)
        .bind(record.is_active)
        .bind(record.default_price_per_request)
        .bind(optional_json_to_string(
            &record.default_tiered_pricing,
            "global_models.default_tiered_pricing",
        )?)
        .bind(optional_json_to_string(
            &record.supported_capabilities,
            "global_models.supported_capabilities",
        )?)
        .bind(optional_json_to_string(
            &record.config,
            "global_models.config",
        )?)
        .bind(usage_count)
        .bind(now as i64)
        .bind(&record.id)
        .execute(&self.pool)
        .await
        .map_sql_err()?;

        if updated.rows_affected() == 0 {
            return Ok(None);
        }

        self.get_admin_global_model_by_id(&record.id).await
    }

    pub async fn delete_admin_global_model(
        &self,
        global_model_id: &str,
    ) -> Result<bool, DataLayerError> {
        let mut tx = self.pool.begin().await.map_sql_err()?;

        sqlx::query(
            r#"
DELETE FROM models
WHERE global_model_id = ?
"#,
        )
        .bind(global_model_id)
        .execute(&mut *tx)
        .await
        .map_sql_err()?;

        let deleted = sqlx::query(
            r#"
DELETE FROM global_models
WHERE id = ?
"#,
        )
        .bind(global_model_id)
        .execute(&mut *tx)
        .await
        .map_sql_err()?;

        tx.commit().await.map_sql_err()?;

        Ok(deleted.rows_affected() > 0)
    }
}

#[async_trait]
impl GlobalModelReadRepository for MysqlGlobalModelReadRepository {
    async fn list_public_models(
        &self,
        query: &PublicGlobalModelQuery,
    ) -> Result<StoredPublicGlobalModelPage, DataLayerError> {
        let mut count_builder = QueryBuilder::<MySql>::new(COUNT_PUBLIC_GLOBAL_MODELS_PREFIX);
        apply_public_model_filters(&mut count_builder, query);
        let count_row = count_builder
            .build()
            .fetch_one(&self.pool)
            .await
            .map_sql_err()?;
        let total = count_row
            .try_get::<i64, _>("total")
            .map(|value| value.max(0) as usize)
            .map_sql_err()?;

        let mut list_builder = QueryBuilder::<MySql>::new(LIST_PUBLIC_GLOBAL_MODELS_PREFIX);
        apply_public_model_filters(&mut list_builder, query);
        list_builder
            .push(" ORDER BY name ASC LIMIT ")
            .push_bind(query.limit as i64)
            .push(" OFFSET ")
            .push_bind(query.offset as i64);
        let rows = list_builder
            .build()
            .fetch_all(&self.pool)
            .await
            .map_sql_err()?;
        let items = rows
            .iter()
            .map(map_public_global_model_row)
            .collect::<Result<_, _>>()?;

        Ok(StoredPublicGlobalModelPage { items, total })
    }

    async fn list_active_global_model_identities(
        &self,
        limit: usize,
    ) -> Result<Vec<StoredGlobalModelIdentity>, DataLayerError> {
        let rows = sqlx::query_as::<_, (String, String)>(
            "SELECT id, name FROM global_models WHERE is_active = TRUE ORDER BY name ASC LIMIT ?",
        )
        .bind(limit.min(i64::MAX as usize) as i64)
        .fetch_all(&self.pool)
        .await
        .map_sql_err()?;
        Ok(rows
            .into_iter()
            .map(|(id, name)| StoredGlobalModelIdentity { id, name })
            .collect())
    }

    async fn get_public_model_by_name(
        &self,
        model_name: &str,
    ) -> Result<Option<StoredPublicGlobalModel>, DataLayerError> {
        let row = sqlx::query(
            r#"
SELECT
  id,
  name,
  display_name,
  is_active,
  CAST(default_price_per_request AS DOUBLE) AS default_price_per_request,
  CAST(default_tiered_pricing AS CHAR) AS default_tiered_pricing,
  CAST(supported_capabilities AS CHAR) AS supported_capabilities,
  CAST(config AS CHAR) AS config,
  CAST(0 AS SIGNED) AS usage_count
FROM global_models
WHERE name = ? AND is_active = 1
LIMIT 1
            "#,
        )
        .bind(model_name)
        .fetch_optional(&self.pool)
        .await
        .map_sql_err()?;

        row.as_ref().map(map_public_global_model_row).transpose()
    }

    async fn list_public_catalog_models(
        &self,
        query: &PublicCatalogModelListQuery,
    ) -> Result<Vec<StoredPublicCatalogModel>, DataLayerError> {
        let mut builder = QueryBuilder::<MySql>::new(LIST_PUBLIC_CATALOG_MODELS_PREFIX);
        apply_public_catalog_model_filters(&mut builder, query.provider_id.as_deref(), None);
        builder
            .push(" ORDER BY p.provider_priority ASC, p.name ASC, COALESCE(gm.name, m.provider_model_name) ASC, m.id ASC LIMIT ")
            .push_bind(query.limit as i64)
            .push(" OFFSET ")
            .push_bind(query.offset as i64);
        let rows = builder.build().fetch_all(&self.pool).await.map_sql_err()?;
        rows.iter().map(map_public_catalog_model_row).collect()
    }

    async fn search_public_catalog_models(
        &self,
        query: &PublicCatalogModelSearchQuery,
    ) -> Result<Vec<StoredPublicCatalogModel>, DataLayerError> {
        let mut builder = QueryBuilder::<MySql>::new(LIST_PUBLIC_CATALOG_MODELS_PREFIX);
        apply_public_catalog_model_filters(
            &mut builder,
            query.provider_id.as_deref(),
            Some(query.search.as_str()),
        );
        builder
            .push(" ORDER BY p.provider_priority ASC, p.name ASC, COALESCE(gm.name, m.provider_model_name) ASC, m.id ASC LIMIT ")
            .push_bind(query.limit as i64);
        let rows = builder.build().fetch_all(&self.pool).await.map_sql_err()?;
        rows.iter().map(map_public_catalog_model_row).collect()
    }

    async fn list_admin_global_models(
        &self,
        query: &AdminGlobalModelListQuery,
    ) -> Result<StoredAdminGlobalModelPage, DataLayerError> {
        let mut count_builder = QueryBuilder::<MySql>::new(COUNT_ADMIN_GLOBAL_MODELS_PREFIX);
        apply_admin_global_model_filters(&mut count_builder, query);
        let count_row = count_builder
            .build()
            .fetch_one(&self.pool)
            .await
            .map_sql_err()?;
        let total = count_row
            .try_get::<i64, _>("total")
            .map(|value| value.max(0) as usize)
            .map_sql_err()?;

        let mut list_builder = QueryBuilder::<MySql>::new(LIST_ADMIN_GLOBAL_MODELS_PREFIX);
        apply_admin_global_model_filters(&mut list_builder, query);
        list_builder
            .push(" ORDER BY gm.name ASC LIMIT ")
            .push_bind(query.limit as i64)
            .push(" OFFSET ")
            .push_bind(query.offset as i64);
        let rows = list_builder
            .build()
            .fetch_all(&self.pool)
            .await
            .map_sql_err()?;
        let items = rows
            .iter()
            .map(map_admin_global_model_row)
            .collect::<Result<_, _>>()?;
        Ok(StoredAdminGlobalModelPage { items, total })
    }

    async fn list_admin_provider_models(
        &self,
        query: &AdminProviderModelListQuery,
    ) -> Result<Vec<StoredAdminProviderModel>, DataLayerError> {
        let mut builder = QueryBuilder::<MySql>::new(LIST_ADMIN_PROVIDER_MODELS_PREFIX);
        builder
            .push(" WHERE m.provider_id = ")
            .push_bind(query.provider_id.trim().to_string());
        if let Some(is_active) = query.is_active {
            builder.push(" AND m.is_active = ").push_bind(is_active);
        }
        builder
            .push(" ORDER BY m.created_at DESC, m.id ASC LIMIT ")
            .push_bind(query.limit as i64)
            .push(" OFFSET ")
            .push_bind(query.offset as i64);
        let rows = builder.build().fetch_all(&self.pool).await.map_sql_err()?;
        rows.iter().map(map_admin_provider_model_row).collect()
    }

    async fn list_admin_provider_available_source_models(
        &self,
        provider_id: &str,
    ) -> Result<Vec<StoredAdminProviderModel>, DataLayerError> {
        let rows = sqlx::query(&format!(
            r#"
{LIST_ADMIN_PROVIDER_MODELS_PREFIX}
WHERE m.provider_id = ?
  AND m.is_active = 1
  AND gm.is_active = 1
ORDER BY gm.name ASC, m.created_at DESC, m.id ASC
            "#
        ))
        .bind(provider_id)
        .fetch_all(&self.pool)
        .await
        .map_sql_err()?;
        rows.iter().map(map_admin_provider_model_row).collect()
    }

    async fn get_admin_provider_model(
        &self,
        provider_id: &str,
        model_id: &str,
    ) -> Result<Option<StoredAdminProviderModel>, DataLayerError> {
        let row = sqlx::query(&format!(
            r#"
{LIST_ADMIN_PROVIDER_MODELS_PREFIX}
WHERE m.provider_id = ?
  AND m.id = ?
LIMIT 1
            "#
        ))
        .bind(provider_id)
        .bind(model_id)
        .fetch_optional(&self.pool)
        .await
        .map_sql_err()?;

        row.as_ref().map(map_admin_provider_model_row).transpose()
    }

    async fn get_admin_global_model_by_id(
        &self,
        global_model_id: &str,
    ) -> Result<Option<StoredAdminGlobalModel>, DataLayerError> {
        let row = sqlx::query(&format!(
            r#"
{LIST_ADMIN_GLOBAL_MODELS_PREFIX}
WHERE gm.id = ?
LIMIT 1
            "#
        ))
        .bind(global_model_id)
        .fetch_optional(&self.pool)
        .await
        .map_sql_err()?;

        row.as_ref().map(map_admin_global_model_row).transpose()
    }

    async fn get_admin_global_model_by_name(
        &self,
        model_name: &str,
    ) -> Result<Option<StoredAdminGlobalModel>, DataLayerError> {
        let row = sqlx::query(&format!(
            r#"
{LIST_ADMIN_GLOBAL_MODELS_PREFIX}
WHERE gm.name = ?
LIMIT 1
            "#
        ))
        .bind(model_name)
        .fetch_optional(&self.pool)
        .await
        .map_sql_err()?;

        row.as_ref().map(map_admin_global_model_row).transpose()
    }

    async fn list_admin_provider_models_by_global_model_id(
        &self,
        global_model_id: &str,
    ) -> Result<Vec<StoredAdminProviderModel>, DataLayerError> {
        let rows = sqlx::query(&format!(
            r#"
{LIST_ADMIN_PROVIDER_MODELS_PREFIX}
WHERE m.global_model_id = ?
ORDER BY m.created_at DESC, m.id ASC
            "#
        ))
        .bind(global_model_id)
        .fetch_all(&self.pool)
        .await
        .map_sql_err()?;
        rows.iter().map(map_admin_provider_model_row).collect()
    }

    async fn list_provider_model_stats(
        &self,
        provider_ids: &[String],
    ) -> Result<Vec<StoredProviderModelStats>, DataLayerError> {
        if provider_ids.is_empty() {
            return Ok(Vec::new());
        }

        let mut builder = build_provider_id_list_query(
            LIST_PROVIDER_MODEL_STATS_PREFIX,
            provider_ids,
            ")\nGROUP BY provider_id\nORDER BY provider_id ASC",
        );
        let rows = builder.build().fetch_all(&self.pool).await.map_sql_err()?;
        rows.iter().map(map_provider_model_stats_row).collect()
    }

    async fn list_active_global_model_ids_by_provider_ids(
        &self,
        provider_ids: &[String],
    ) -> Result<Vec<StoredProviderActiveGlobalModel>, DataLayerError> {
        if provider_ids.is_empty() {
            return Ok(Vec::new());
        }

        let mut builder = build_provider_id_list_query(
            LIST_ACTIVE_GLOBAL_MODEL_IDS_BY_PROVIDER_IDS_PREFIX,
            provider_ids,
            ")\nAND is_active = 1\nAND global_model_id IS NOT NULL\nORDER BY provider_id ASC, global_model_id ASC",
        );
        let rows = builder.build().fetch_all(&self.pool).await.map_sql_err()?;
        rows.iter().map(map_active_global_model_row).collect()
    }
}

#[async_trait]
impl GlobalModelWriteRepository for MysqlGlobalModelReadRepository {
    async fn create_admin_provider_model(
        &self,
        record: &UpsertAdminProviderModelRecord,
    ) -> Result<Option<StoredAdminProviderModel>, DataLayerError> {
        Self::create_admin_provider_model(self, record).await
    }

    async fn update_admin_provider_model(
        &self,
        record: &UpsertAdminProviderModelRecord,
    ) -> Result<Option<StoredAdminProviderModel>, DataLayerError> {
        Self::update_admin_provider_model(self, record).await
    }

    async fn delete_admin_provider_model(
        &self,
        provider_id: &str,
        model_id: &str,
    ) -> Result<bool, DataLayerError> {
        Self::delete_admin_provider_model(self, provider_id, model_id).await
    }

    async fn create_admin_global_model(
        &self,
        record: &CreateAdminGlobalModelRecord,
    ) -> Result<Option<StoredAdminGlobalModel>, DataLayerError> {
        Self::create_admin_global_model(self, record).await
    }

    async fn update_admin_global_model(
        &self,
        record: &UpdateAdminGlobalModelRecord,
    ) -> Result<Option<StoredAdminGlobalModel>, DataLayerError> {
        Self::update_admin_global_model(self, record).await
    }

    async fn delete_admin_global_model(
        &self,
        global_model_id: &str,
    ) -> Result<bool, DataLayerError> {
        Self::delete_admin_global_model(self, global_model_id).await
    }
}

fn current_unix_secs() -> u64 {
    chrono::Utc::now().timestamp().max(0) as u64
}

fn optional_json_to_string(
    value: &Option<serde_json::Value>,
    field_name: &str,
) -> Result<Option<String>, DataLayerError> {
    value
        .as_ref()
        .map(|value| {
            serde_json::to_string(value).map_err(|err| {
                DataLayerError::UnexpectedValue(format!(
                    "{field_name} contains unserializable JSON: {err}"
                ))
            })
        })
        .transpose()
}

fn optional_json_from_string(
    value: Option<String>,
    field_name: &str,
) -> Result<Option<serde_json::Value>, DataLayerError> {
    value
        .map(|value| {
            serde_json::from_str(&value).map_err(|err| {
                DataLayerError::UnexpectedValue(format!(
                    "{field_name} contains invalid JSON: {err}"
                ))
            })
        })
        .transpose()
}

fn optional_u64(value: Option<i64>, field_name: &str) -> Result<Option<u64>, DataLayerError> {
    value
        .map(|value| {
            u64::try_from(value).map_err(|_| {
                DataLayerError::UnexpectedValue(format!("invalid {field_name}: {value}"))
            })
        })
        .transpose()
}

fn first_tier_price(value: Option<&serde_json::Value>, key: &str) -> Option<f64> {
    value
        .and_then(|value| value.get("tiers"))
        .and_then(serde_json::Value::as_array)
        .and_then(|tiers| tiers.first())
        .and_then(|tier| tier.get(key))
        .and_then(serde_json::Value::as_f64)
}

fn apply_public_model_filters(
    builder: &mut QueryBuilder<'_, MySql>,
    query: &PublicGlobalModelQuery,
) {
    builder.push(" WHERE ");
    match query.is_active {
        Some(is_active) => {
            builder.push("is_active = ").push_bind(is_active);
        }
        None => {
            builder.push("is_active = 1");
        }
    }

    if let Some(search) = query
        .search
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        let pattern = format!("%{}%", search.to_ascii_lowercase());
        builder
            .push(" AND (LOWER(name) LIKE ")
            .push_bind(pattern.clone())
            .push(" OR LOWER(display_name) LIKE ")
            .push_bind(pattern)
            .push(")");
    }
}

fn apply_admin_global_model_filters(
    builder: &mut QueryBuilder<'_, MySql>,
    query: &AdminGlobalModelListQuery,
) {
    builder.push(" WHERE 1=1");
    if let Some(is_active) = query.is_active {
        builder.push(" AND gm.is_active = ").push_bind(is_active);
    }
    if let Some(search) = query
        .search
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        let pattern = format!("%{}%", search.to_ascii_lowercase());
        builder
            .push(" AND (LOWER(gm.name) LIKE ")
            .push_bind(pattern.clone())
            .push(" OR LOWER(gm.display_name) LIKE ")
            .push_bind(pattern)
            .push(")");
    }
}

fn apply_public_catalog_model_filters(
    builder: &mut QueryBuilder<'_, MySql>,
    provider_id: Option<&str>,
    search: Option<&str>,
) {
    builder.push(" WHERE m.is_active = 1 AND COALESCE(m.is_available, 1) = 1 AND p.is_active = 1 AND COALESCE(gm.is_active, 1) = 1");

    if let Some(provider_id) = provider_id.map(str::trim).filter(|value| !value.is_empty()) {
        builder
            .push(" AND m.provider_id = ")
            .push_bind(provider_id.to_string());
    }

    if let Some(search) = search.map(str::trim).filter(|value| !value.is_empty()) {
        let pattern = format!("%{}%", search.to_ascii_lowercase());
        builder
            .push(" AND (LOWER(m.provider_model_name) LIKE ")
            .push_bind(pattern.clone())
            .push(" OR LOWER(gm.name) LIKE ")
            .push_bind(pattern.clone())
            .push(" OR LOWER(gm.display_name) LIKE ")
            .push_bind(pattern)
            .push(")");
    }
}

fn build_provider_id_list_query<'a>(
    prefix: &'static str,
    provider_ids: &'a [String],
    suffix: &'static str,
) -> QueryBuilder<'a, MySql> {
    let mut builder = QueryBuilder::<MySql>::new(prefix);
    let mut separated = builder.separated(", ");
    for provider_id in provider_ids {
        separated.push_bind(provider_id);
    }
    separated.push_unseparated(suffix);
    builder
}

fn map_public_global_model_row(row: &MySqlRow) -> Result<StoredPublicGlobalModel, DataLayerError> {
    StoredPublicGlobalModel::new(
        row.try_get("id").map_sql_err()?,
        row.try_get("name").map_sql_err()?,
        row.try_get("display_name").map_sql_err()?,
        row.try_get("is_active").map_sql_err()?,
        row.try_get("default_price_per_request").map_sql_err()?,
        optional_json_from_string(
            row.try_get("default_tiered_pricing").map_sql_err()?,
            "global_models.default_tiered_pricing",
        )?,
        optional_json_from_string(
            row.try_get("supported_capabilities").map_sql_err()?,
            "global_models.supported_capabilities",
        )?,
        optional_json_from_string(row.try_get("config").map_sql_err()?, "global_models.config")?,
        row.try_get::<i64, _>("usage_count").map_sql_err()?.max(0) as u64,
    )
}

fn map_admin_global_model_row(row: &MySqlRow) -> Result<StoredAdminGlobalModel, DataLayerError> {
    let provider_count = row
        .try_get::<i64, _>("provider_count")
        .map_sql_err()?
        .max(0) as u64;
    let active_provider_count = row
        .try_get::<i64, _>("active_provider_count")
        .map_sql_err()?
        .max(0) as u64;
    let usage_count = row.try_get::<i64, _>("usage_count").map_sql_err()?.max(0) as u64;

    StoredAdminGlobalModel::new(
        row.try_get("id").map_sql_err()?,
        row.try_get("name").map_sql_err()?,
        row.try_get("display_name").map_sql_err()?,
        row.try_get("is_active").map_sql_err()?,
        row.try_get("default_price_per_request").map_sql_err()?,
        optional_json_from_string(
            row.try_get("default_tiered_pricing").map_sql_err()?,
            "global_models.default_tiered_pricing",
        )?,
        optional_json_from_string(
            row.try_get("supported_capabilities").map_sql_err()?,
            "global_models.supported_capabilities",
        )?,
        optional_json_from_string(row.try_get("config").map_sql_err()?, "global_models.config")?,
        provider_count,
        active_provider_count,
        usage_count,
        optional_u64(
            row.try_get("created_at_unix_ms").map_sql_err()?,
            "global_models.created_at",
        )?,
        optional_u64(
            row.try_get("updated_at_unix_secs").map_sql_err()?,
            "global_models.updated_at",
        )?,
    )
}

fn map_admin_provider_model_row(
    row: &MySqlRow,
) -> Result<StoredAdminProviderModel, DataLayerError> {
    StoredAdminProviderModel::new(
        row.try_get("id").map_sql_err()?,
        row.try_get("provider_id").map_sql_err()?,
        row.try_get("global_model_id").map_sql_err()?,
        row.try_get("provider_model_name").map_sql_err()?,
        optional_json_from_string(
            row.try_get("provider_model_mappings").map_sql_err()?,
            "models.provider_model_mappings",
        )?,
        row.try_get("price_per_request").map_sql_err()?,
        optional_json_from_string(
            row.try_get("tiered_pricing").map_sql_err()?,
            "models.tiered_pricing",
        )?,
        row.try_get("supports_vision").map_sql_err()?,
        row.try_get("supports_function_calling").map_sql_err()?,
        row.try_get("supports_streaming").map_sql_err()?,
        row.try_get("supports_extended_thinking").map_sql_err()?,
        row.try_get("supports_image_generation").map_sql_err()?,
        row.try_get("is_active").map_sql_err()?,
        row.try_get("is_available").map_sql_err()?,
        optional_json_from_string(row.try_get("config").map_sql_err()?, "models.config")?,
        optional_u64(
            row.try_get("created_at_unix_ms").map_sql_err()?,
            "models.created_at",
        )?,
        optional_u64(
            row.try_get("updated_at_unix_secs").map_sql_err()?,
            "models.updated_at",
        )?,
        row.try_get("global_model_name").map_sql_err()?,
        row.try_get("global_model_display_name").map_sql_err()?,
        row.try_get("global_model_default_price_per_request")
            .map_sql_err()?,
        optional_json_from_string(
            row.try_get("global_model_default_tiered_pricing")
                .map_sql_err()?,
            "global_models.default_tiered_pricing",
        )?,
        optional_json_from_string(
            row.try_get("global_model_supported_capabilities")
                .map_sql_err()?,
            "global_models.supported_capabilities",
        )?,
        optional_json_from_string(
            row.try_get("global_model_config").map_sql_err()?,
            "global_models.config",
        )?,
    )
}

fn map_public_catalog_model_row(
    row: &MySqlRow,
) -> Result<StoredPublicCatalogModel, DataLayerError> {
    let global_model_config = optional_json_from_string(
        row.try_get("global_model_config").map_sql_err()?,
        "global_models.config",
    )?;
    let global_model_supported_capabilities = optional_json_from_string(
        row.try_get("global_model_supported_capabilities")
            .map_sql_err()?,
        "global_models.supported_capabilities",
    )?;
    let model_config =
        optional_json_from_string(row.try_get("model_config").map_sql_err()?, "models.config")?;
    let tiered_pricing = optional_json_from_string(
        row.try_get("tiered_pricing").map_sql_err()?,
        "models.tiered_pricing",
    )?;
    let default_tiered_pricing = optional_json_from_string(
        row.try_get("default_tiered_pricing").map_sql_err()?,
        "global_models.default_tiered_pricing",
    )?;
    let pricing = tiered_pricing.as_ref().or(default_tiered_pricing.as_ref());
    let global_model_is_active = row
        .try_get::<Option<bool>, _>("global_model_is_active")
        .map_sql_err()?
        .unwrap_or(true);
    let model_is_active: bool = row.try_get("is_active").map_sql_err()?;
    let provider_is_active: bool = row.try_get("provider_is_active").map_sql_err()?;

    StoredPublicCatalogModel::new(
        row.try_get("id").map_sql_err()?,
        row.try_get("provider_id").map_sql_err()?,
        row.try_get("provider_name").map_sql_err()?,
        row.try_get("provider_model_name").map_sql_err()?,
        row.try_get("name").map_sql_err()?,
        row.try_get("display_name").map_sql_err()?,
        global_model_config
            .as_ref()
            .and_then(|value| value.get("description"))
            .and_then(serde_json::Value::as_str)
            .map(ToString::to_string),
        global_model_config
            .as_ref()
            .and_then(|value| value.get("icon_url"))
            .and_then(serde_json::Value::as_str)
            .map(ToString::to_string),
        Some(first_tier_price(pricing, "input_price_per_1m").unwrap_or(0.0)),
        Some(first_tier_price(pricing, "output_price_per_1m").unwrap_or(0.0)),
        first_tier_price(pricing, "cache_creation_price_per_1m"),
        first_tier_price(pricing, "cache_read_price_per_1m"),
        row.try_get("supports_vision").map_sql_err()?,
        row.try_get("supports_function_calling").map_sql_err()?,
        row.try_get("supports_streaming").map_sql_err()?,
        metadata_supports_embedding(
            global_model_supported_capabilities.as_ref(),
            global_model_config.as_ref(),
            model_config.as_ref(),
        ),
        model_is_active && provider_is_active && global_model_is_active,
    )
}

fn map_provider_model_stats_row(
    row: &MySqlRow,
) -> Result<StoredProviderModelStats, DataLayerError> {
    StoredProviderModelStats::new(
        row.try_get("provider_id").map_sql_err()?,
        row.try_get("total_models").map_sql_err()?,
        row.try_get::<Option<i64>, _>("active_models")
            .map_sql_err()?
            .unwrap_or(0),
    )
}

fn map_active_global_model_row(
    row: &MySqlRow,
) -> Result<StoredProviderActiveGlobalModel, DataLayerError> {
    StoredProviderActiveGlobalModel::new(
        row.try_get("provider_id").map_sql_err()?,
        row.try_get("global_model_id").map_sql_err()?,
    )
}

fn optional_admin_global_model_usage_count_i64(
    value: Option<u64>,
) -> Result<Option<i64>, DataLayerError> {
    value
        .map(|value| {
            i64::try_from(value).map_err(|_| {
                DataLayerError::InvalidInput(
                    "global_models.usage_count exceeds i64 range".to_string(),
                )
            })
        })
        .transpose()
}

#[cfg(test)]
mod tests {
    use sqlx::{MySql, QueryBuilder};

    use aether_data_contracts::repository::global_models::{
        AdminGlobalModelListQuery, AdminProviderModelListQuery, CreateAdminGlobalModelRecord,
        GlobalModelReadRepository, PublicCatalogModelListQuery, PublicCatalogModelSearchQuery,
        PublicGlobalModelQuery, UpsertAdminProviderModelRecord,
    };
    use serde_json::json;

    use super::{
        apply_admin_global_model_filters, apply_public_model_filters, build_provider_id_list_query,
        MysqlGlobalModelReadRepository, LIST_PROVIDER_MODEL_STATS_PREFIX,
    };
    use crate::run_migrations;

    #[test]
    fn mysql_global_model_builders_apply_filters_and_bound_lists() {
        let mut public = QueryBuilder::<MySql>::new("SELECT id FROM global_models");
        apply_public_model_filters(
            &mut public,
            &PublicGlobalModelQuery {
                offset: 0,
                limit: 25,
                is_active: Some(false),
                search: Some("GPT".to_string()),
            },
        );
        assert_eq!(
            public.sql(),
            "SELECT id FROM global_models WHERE is_active = ? AND (LOWER(name) LIKE ? OR LOWER(display_name) LIKE ?)"
        );

        let mut admin = QueryBuilder::<MySql>::new("SELECT gm.id FROM global_models gm");
        apply_admin_global_model_filters(
            &mut admin,
            &AdminGlobalModelListQuery {
                offset: 0,
                limit: 25,
                is_active: None,
                search: Some("embed".to_string()),
            },
        );
        assert_eq!(
            admin.sql(),
            "SELECT gm.id FROM global_models gm WHERE 1=1 AND (LOWER(gm.name) LIKE ? OR LOWER(gm.display_name) LIKE ?)"
        );

        let provider_ids = vec!["provider-a".to_string(), "provider-b".to_string()];
        let provider_stats = build_provider_id_list_query(
            LIST_PROVIDER_MODEL_STATS_PREFIX,
            &provider_ids,
            ") GROUP BY provider_id",
        );
        let normalized_sql = provider_stats
            .sql()
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        assert!(normalized_sql.ends_with("WHERE provider_id IN ( ?, ?) GROUP BY provider_id"));
    }

    #[tokio::test]
    async fn repository_builds_from_lazy_pool() {
        let pool = sqlx::mysql::MySqlPoolOptions::new().connect_lazy_with(
            "mysql://user:pass@localhost:3306/aether"
                .parse()
                .expect("mysql options should parse"),
        );

        let _repository = MysqlGlobalModelReadRepository::new(pool);
    }

    #[tokio::test]
    async fn mysql_global_model_queries_round_trip_when_url_is_set() {
        let Some(database_url) = std::env::var("AETHER_TEST_MYSQL_URL")
            .ok()
            .filter(|value| !value.trim().is_empty())
        else {
            eprintln!("skipping mysql global model test because AETHER_TEST_MYSQL_URL is unset");
            return;
        };

        let pool = sqlx::mysql::MySqlPoolOptions::new()
            .max_connections(1)
            .connect(&database_url)
            .await
            .expect("mysql test pool should connect");
        run_migrations(&pool)
            .await
            .expect("mysql migrations should run");

        let suffix = unique_suffix();
        let provider_id = format!("provider-{suffix}");
        let global_id = format!("global-{suffix}");
        let model_id = format!("model-{suffix}");
        let global_name = format!("global-name-{suffix}");
        sqlx::query(
            r#"
INSERT INTO providers (
  id, name, provider_type, is_active, provider_priority, created_at, updated_at
) VALUES (?, ?, 'custom', 1, 10, 1, 1)
"#,
        )
        .bind(&provider_id)
        .bind(format!("Provider {suffix}"))
        .execute(&pool)
        .await
        .expect("provider should seed");

        let repository = MysqlGlobalModelReadRepository::new(pool.clone());
        let mut global_record = CreateAdminGlobalModelRecord::new(
            global_id.clone(),
            global_name.clone(),
            format!("Global {suffix}"),
            true,
            None,
            Some(json!({"tiers":[{"input_price_per_1m":2.0}]})),
            Some(json!(["chat"])),
            Some(json!({"description":"mysql parity"})),
        )
        .expect("global model input should validate");
        global_record.usage_count = Some(7);
        repository
            .create_admin_global_model(&global_record)
            .await
            .expect("global model should create")
            .expect("created global model should return");

        let provider_model = UpsertAdminProviderModelRecord::new(
            model_id.clone(),
            provider_id.clone(),
            global_id.clone(),
            format!("upstream-{suffix}"),
            Some(json!([global_name])),
            None,
            None,
            Some(true),
            Some(true),
            Some(true),
            Some(false),
            Some(false),
            true,
            true,
            Some(json!({"routing":"primary"})),
        )
        .expect("provider model input should validate");
        repository
            .create_admin_provider_model(&provider_model)
            .await
            .expect("provider model should create")
            .expect("created provider model should return");

        let public = repository
            .list_public_models(&PublicGlobalModelQuery {
                offset: 0,
                limit: 10,
                is_active: None,
                search: Some(suffix.clone()),
            })
            .await
            .expect("public models should list");
        assert_eq!(public.total, 1);
        assert_eq!(public.items[0].usage_count, 0);

        let catalog = repository
            .search_public_catalog_models(&PublicCatalogModelSearchQuery {
                search: suffix.clone(),
                provider_id: Some(provider_id.clone()),
                limit: 10,
            })
            .await
            .expect("catalog should search");
        assert_eq!(catalog.len(), 1);
        assert_eq!(catalog[0].input_price_per_1m, Some(2.0));
        assert_eq!(catalog[0].output_price_per_1m, Some(0.0));

        let catalog_page = repository
            .list_public_catalog_models(&PublicCatalogModelListQuery {
                provider_id: Some(provider_id.clone()),
                offset: 0,
                limit: 1,
            })
            .await
            .expect("catalog should list");
        assert_eq!(catalog_page.len(), 1);

        let admin = repository
            .list_admin_global_models(&AdminGlobalModelListQuery {
                offset: 0,
                limit: 10,
                is_active: None,
                search: Some(suffix.clone()),
            })
            .await
            .expect("admin global models should list");
        assert_eq!(admin.total, 1);
        assert_eq!(admin.items[0].provider_count, 1);
        assert_eq!(admin.items[0].active_provider_count, 1);
        assert_eq!(admin.items[0].usage_count, 7);

        let admin_models = repository
            .list_admin_provider_models(&AdminProviderModelListQuery {
                provider_id: provider_id.clone(),
                is_active: Some(true),
                offset: 0,
                limit: 1,
            })
            .await
            .expect("admin provider models should list");
        assert_eq!(admin_models.len(), 1);
        assert_eq!(admin_models[0].id, model_id);

        let stats = repository
            .list_provider_model_stats(std::slice::from_ref(&provider_id))
            .await
            .expect("provider stats should list");
        assert_eq!(stats.len(), 1);
        assert_eq!(stats[0].total_models, 1);
        let active_refs = repository
            .list_active_global_model_ids_by_provider_ids(std::slice::from_ref(&provider_id))
            .await
            .expect("active global refs should list");
        assert_eq!(active_refs.len(), 1);
        assert_eq!(active_refs[0].global_model_id, global_id);

        assert!(repository
            .delete_admin_global_model(&global_id)
            .await
            .expect("global model should delete"));
        sqlx::query("DELETE FROM providers WHERE id = ?")
            .bind(&provider_id)
            .execute(&pool)
            .await
            .expect("provider should delete");
    }

    fn unique_suffix() -> String {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        format!("{}-{nanos}", std::process::id())
    }
}
