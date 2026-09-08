use async_trait::async_trait;
use serde_json::Value;
use sqlx::{sqlite::SqliteRow, Row};

use aether_data_contracts::repository::routing_profiles::*;
use aether_data_contracts::DataLayerError;

use crate::error::SqlResultExt;
use crate::pool::SqlitePool;

const ROUTING_GROUP_SELECT: &str = r#"
SELECT
  id,
  name,
  description,
  enabled,
  is_system_default,
  sort_order,
  config_json,
  version,
  created_at,
  updated_at,
  published_at
FROM routing_groups
"#;

const ROUTING_GROUP_BINDING_SELECT: &str = r#"
SELECT
  id,
  group_id,
  subject_type,
  subject_id,
  is_default,
  allow_explicit_select,
  created_at,
  updated_at
FROM routing_group_bindings
"#;

const ROUTING_GROUP_VERSION_SELECT: &str = r#"
SELECT
  id,
  group_id,
  version,
  config_json,
  created_at,
  created_by
FROM routing_group_versions
"#;

#[derive(Debug, Clone)]
pub struct SqliteRoutingGroupRepository {
    pool: SqlitePool,
}

impl SqliteRoutingGroupRepository {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }
}

#[async_trait]
impl RoutingGroupReadRepository for SqliteRoutingGroupRepository {
    async fn list_routing_groups(&self) -> Result<Vec<StoredRoutingGroup>, DataLayerError> {
        let rows = sqlx::query(&format!(
            "{ROUTING_GROUP_SELECT} ORDER BY enabled DESC, sort_order ASC, name ASC, id ASC"
        ))
        .fetch_all(&self.pool)
        .await
        .map_sql_err()?;
        rows.iter().map(map_group_row).collect()
    }

    async fn find_routing_group(
        &self,
        lookup: RoutingGroupLookupKey<'_>,
    ) -> Result<Option<StoredRoutingGroup>, DataLayerError> {
        let row = match lookup {
            RoutingGroupLookupKey::Id(id) => sqlx::query(&format!(
                "{ROUTING_GROUP_SELECT} WHERE id = ? LIMIT 1"
            ))
            .bind(id)
            .fetch_optional(&self.pool)
            .await
            .map_sql_err()?,
            RoutingGroupLookupKey::Name(name) => sqlx::query(&format!(
                "{ROUTING_GROUP_SELECT} WHERE name = ? LIMIT 1"
            ))
            .bind(name)
            .fetch_optional(&self.pool)
            .await
            .map_sql_err()?,
            RoutingGroupLookupKey::SystemDefault => sqlx::query(&format!(
                "{ROUTING_GROUP_SELECT} WHERE is_system_default = 1 AND enabled = 1 ORDER BY updated_at DESC, id ASC LIMIT 1"
            ))
            .fetch_optional(&self.pool)
            .await
            .map_sql_err()?,
        };
        row.as_ref().map(map_group_row).transpose()
    }

    async fn list_routing_group_bindings(
        &self,
        query: &RoutingGroupBindingQuery,
    ) -> Result<Vec<StoredRoutingGroupBinding>, DataLayerError> {
        let rows = sqlx::query(&format!(
            r#"
{ROUTING_GROUP_BINDING_SELECT}
WHERE (? IS NULL OR group_id = ?)
  AND (? IS NULL OR subject_type = ?)
  AND (? IS NULL OR subject_id = ?)
ORDER BY created_at ASC, id ASC
"#
        ))
        .bind(query.group_id.as_deref())
        .bind(query.group_id.as_deref())
        .bind(query.subject_type.map(binding_subject_to_database))
        .bind(query.subject_type.map(binding_subject_to_database))
        .bind(query.subject_id.as_deref())
        .bind(query.subject_id.as_deref())
        .fetch_all(&self.pool)
        .await
        .map_sql_err()?;
        rows.iter().map(map_binding_row).collect()
    }

    async fn has_any_routing_group_binding(&self) -> Result<bool, DataLayerError> {
        let row = sqlx::query("SELECT 1 FROM routing_group_bindings LIMIT 1")
            .fetch_optional(&self.pool)
            .await
            .map_sql_err()?;
        Ok(row.is_some())
    }

    async fn list_routing_group_versions(
        &self,
        group_id: &str,
    ) -> Result<Vec<StoredRoutingGroupVersion>, DataLayerError> {
        let rows = sqlx::query(&format!(
            "{ROUTING_GROUP_VERSION_SELECT} WHERE group_id = ? ORDER BY version DESC, created_at DESC, id ASC"
        ))
        .bind(group_id)
        .fetch_all(&self.pool)
        .await
        .map_sql_err()?;
        rows.iter().map(map_version_row).collect()
    }
}

#[async_trait]
impl RoutingGroupWriteRepository for SqliteRoutingGroupRepository {
    async fn create_routing_group(
        &self,
        record: CreateRoutingGroupRecord,
    ) -> Result<StoredRoutingGroup, DataLayerError> {
        let group = StoredRoutingGroup::new(record)?;
        let mut tx = self.pool.begin().await.map_sql_err()?;
        sqlx::query("UPDATE routing_groups SET is_system_default = is_system_default WHERE 0")
            .execute(&mut *tx)
            .await
            .map_sql_err()?;
        if group.is_system_default {
            sqlx::query(
                "UPDATE routing_groups SET is_system_default = 0 WHERE is_system_default = 1",
            )
            .execute(&mut *tx)
            .await
            .map_sql_err()?;
        }
        sqlx::query(
            r#"
INSERT INTO routing_groups (
  id, name, description, enabled, is_system_default, sort_order, config_json,
  version, created_at, updated_at, published_at
)
VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
"#,
        )
        .bind(&group.id)
        .bind(&group.name)
        .bind(&group.description)
        .bind(group.enabled)
        .bind(group.is_system_default)
        .bind(group.sort_order)
        .bind(json_to_string(
            &group.config_json,
            "routing_groups.config_json",
        )?)
        .bind(group.version)
        .bind(group.created_at)
        .bind(group.updated_at)
        .bind(group.published_at)
        .execute(&mut *tx)
        .await
        .map_sql_err()?;
        tx.commit().await.map_sql_err()?;
        Ok(group)
    }

    async fn update_routing_group(
        &self,
        id: &str,
        patch: UpdateRoutingGroupRecord,
    ) -> Result<Option<StoredRoutingGroup>, DataLayerError> {
        let mut tx = self.pool.begin().await.map_sql_err()?;
        sqlx::query("UPDATE routing_groups SET is_system_default = is_system_default WHERE 0")
            .execute(&mut *tx)
            .await
            .map_sql_err()?;
        let row = sqlx::query(&format!("{ROUTING_GROUP_SELECT} WHERE id = ? LIMIT 1"))
            .bind(id)
            .fetch_optional(&mut *tx)
            .await
            .map_sql_err()?;
        let Some(mut group) = row.as_ref().map(map_group_row).transpose()? else {
            return Ok(None);
        };
        apply_group_patch(&mut group, patch)?;
        if group.is_system_default {
            sqlx::query(
                "UPDATE routing_groups SET is_system_default = 0 WHERE is_system_default = 1 AND id <> ?",
            )
            .bind(id)
            .execute(&mut *tx)
            .await
            .map_sql_err()?;
        }
        sqlx::query(
            r#"
UPDATE routing_groups
SET name = ?,
    description = ?,
    enabled = ?,
    is_system_default = ?,
    sort_order = ?,
    config_json = ?,
    version = ?,
    updated_at = ?,
    published_at = ?
WHERE id = ?
"#,
        )
        .bind(&group.name)
        .bind(&group.description)
        .bind(group.enabled)
        .bind(group.is_system_default)
        .bind(group.sort_order)
        .bind(json_to_string(
            &group.config_json,
            "routing_groups.config_json",
        )?)
        .bind(group.version)
        .bind(group.updated_at)
        .bind(group.published_at)
        .bind(id)
        .execute(&mut *tx)
        .await
        .map_sql_err()?;
        tx.commit().await.map_sql_err()?;
        Ok(Some(group))
    }

    async fn delete_routing_group(&self, id: &str) -> Result<bool, DataLayerError> {
        let mut tx = self.pool.begin().await.map_sql_err()?;
        sqlx::query("DELETE FROM routing_group_bindings WHERE group_id = ?")
            .bind(id)
            .execute(&mut *tx)
            .await
            .map_sql_err()?;
        sqlx::query("DELETE FROM routing_group_versions WHERE group_id = ?")
            .bind(id)
            .execute(&mut *tx)
            .await
            .map_sql_err()?;
        let rows_affected = sqlx::query("DELETE FROM routing_groups WHERE id = ?")
            .bind(id)
            .execute(&mut *tx)
            .await
            .map_sql_err()?
            .rows_affected();
        tx.commit().await.map_sql_err()?;
        Ok(rows_affected > 0)
    }

    async fn create_routing_group_binding(
        &self,
        record: CreateRoutingGroupBindingRecord,
    ) -> Result<StoredRoutingGroupBinding, DataLayerError> {
        let binding = StoredRoutingGroupBinding::new(record)?;
        let mut tx = self.pool.begin().await.map_sql_err()?;
        sqlx::query("UPDATE routing_group_bindings SET is_default = is_default WHERE 0")
            .execute(&mut *tx)
            .await
            .map_sql_err()?;
        if binding.is_default {
            sqlx::query(
                r#"
UPDATE routing_group_bindings
SET is_default = 0
WHERE is_default = 1 AND subject_type = ? AND subject_id = ?
"#,
            )
            .bind(binding_subject_to_database(binding.subject_type))
            .bind(&binding.subject_id)
            .execute(&mut *tx)
            .await
            .map_sql_err()?;
        }
        sqlx::query(
            r#"
INSERT INTO routing_group_bindings (
  id, group_id, subject_type, subject_id, is_default,
  allow_explicit_select, created_at, updated_at
)
VALUES (?, ?, ?, ?, ?, ?, ?, ?)
"#,
        )
        .bind(&binding.id)
        .bind(&binding.group_id)
        .bind(binding_subject_to_database(binding.subject_type))
        .bind(&binding.subject_id)
        .bind(binding.is_default)
        .bind(binding.allow_explicit_select)
        .bind(binding.created_at)
        .bind(binding.updated_at)
        .execute(&mut *tx)
        .await
        .map_sql_err()?;
        tx.commit().await.map_sql_err()?;
        Ok(binding)
    }

    async fn delete_routing_group_binding(&self, id: &str) -> Result<bool, DataLayerError> {
        Ok(
            sqlx::query("DELETE FROM routing_group_bindings WHERE id = ?")
                .bind(id)
                .execute(&self.pool)
                .await
                .map_sql_err()?
                .rows_affected()
                > 0,
        )
    }

    async fn update_routing_group_binding(
        &self,
        id: &str,
        patch: UpdateRoutingGroupBindingRecord,
    ) -> Result<Option<StoredRoutingGroupBinding>, DataLayerError> {
        let mut tx = self.pool.begin().await.map_sql_err()?;
        sqlx::query("UPDATE routing_group_bindings SET is_default = is_default WHERE 0")
            .execute(&mut *tx)
            .await
            .map_sql_err()?;
        let row = sqlx::query(&format!(
            "{ROUTING_GROUP_BINDING_SELECT} WHERE id = ? LIMIT 1"
        ))
        .bind(id)
        .fetch_optional(&mut *tx)
        .await
        .map_sql_err()?;
        let Some(mut binding) = row.as_ref().map(map_binding_row).transpose()? else {
            return Ok(None);
        };
        apply_binding_patch(&mut binding, patch)?;
        if binding.is_default {
            sqlx::query(
                r#"
UPDATE routing_group_bindings
SET is_default = 0
WHERE is_default = 1
  AND subject_type = ?
  AND subject_id = ?
  AND id <> ?
"#,
            )
            .bind(binding_subject_to_database(binding.subject_type))
            .bind(&binding.subject_id)
            .bind(id)
            .execute(&mut *tx)
            .await
            .map_sql_err()?;
        }
        sqlx::query(
            r#"
UPDATE routing_group_bindings
SET group_id = ?,
    subject_type = ?,
    subject_id = ?,
    is_default = ?,
    allow_explicit_select = ?,
    updated_at = ?
WHERE id = ?
"#,
        )
        .bind(&binding.group_id)
        .bind(binding_subject_to_database(binding.subject_type))
        .bind(&binding.subject_id)
        .bind(binding.is_default)
        .bind(binding.allow_explicit_select)
        .bind(binding.updated_at)
        .bind(id)
        .execute(&mut *tx)
        .await
        .map_sql_err()?;
        tx.commit().await.map_sql_err()?;
        Ok(Some(binding))
    }

    async fn create_routing_group_version(
        &self,
        record: CreateRoutingGroupVersionRecord,
    ) -> Result<StoredRoutingGroupVersion, DataLayerError> {
        let version = StoredRoutingGroupVersion::new(record)?;
        sqlx::query(
            r#"
INSERT INTO routing_group_versions (
  id, group_id, version, config_json, created_at, created_by
)
VALUES (?, ?, ?, ?, ?, ?)
"#,
        )
        .bind(&version.id)
        .bind(&version.group_id)
        .bind(version.version)
        .bind(json_to_string(
            &version.config_json,
            "routing_group_versions.config_json",
        )?)
        .bind(version.created_at)
        .bind(&version.created_by)
        .execute(&self.pool)
        .await
        .map_sql_err()?;
        Ok(version)
    }
}

fn map_group_row(row: &SqliteRow) -> Result<StoredRoutingGroup, DataLayerError> {
    Ok(StoredRoutingGroup {
        id: row.try_get("id").map_sql_err()?,
        name: row.try_get("name").map_sql_err()?,
        description: row.try_get("description").map_sql_err()?,
        enabled: row.try_get("enabled").map_sql_err()?,
        is_system_default: row.try_get("is_system_default").map_sql_err()?,
        sort_order: row.try_get("sort_order").map_sql_err()?,
        config_json: json_from_string(
            row.try_get("config_json").map_sql_err()?,
            "routing_groups.config_json",
        )?,
        version: row.try_get("version").map_sql_err()?,
        created_at: row.try_get("created_at").map_sql_err()?,
        updated_at: row.try_get("updated_at").map_sql_err()?,
        published_at: row.try_get("published_at").map_sql_err()?,
    })
}

fn map_binding_row(row: &SqliteRow) -> Result<StoredRoutingGroupBinding, DataLayerError> {
    Ok(StoredRoutingGroupBinding {
        id: row.try_get("id").map_sql_err()?,
        group_id: row.try_get("group_id").map_sql_err()?,
        subject_type: binding_subject_from_database(row.try_get("subject_type").map_sql_err()?)?,
        subject_id: row.try_get("subject_id").map_sql_err()?,
        is_default: row.try_get("is_default").map_sql_err()?,
        allow_explicit_select: row.try_get("allow_explicit_select").map_sql_err()?,
        created_at: row.try_get("created_at").map_sql_err()?,
        updated_at: row.try_get("updated_at").map_sql_err()?,
    })
}

fn map_version_row(row: &SqliteRow) -> Result<StoredRoutingGroupVersion, DataLayerError> {
    Ok(StoredRoutingGroupVersion {
        id: row.try_get("id").map_sql_err()?,
        group_id: row.try_get("group_id").map_sql_err()?,
        version: row.try_get("version").map_sql_err()?,
        config_json: json_from_string(
            row.try_get("config_json").map_sql_err()?,
            "routing_group_versions.config_json",
        )?,
        created_at: row.try_get("created_at").map_sql_err()?,
        created_by: row.try_get("created_by").map_sql_err()?,
    })
}

fn json_to_string(value: &Value, field_name: &str) -> Result<String, DataLayerError> {
    serde_json::to_string(value).map_err(|err| {
        DataLayerError::UnexpectedValue(format!("{field_name} contains unserializable JSON: {err}"))
    })
}

fn json_from_string(value: String, field_name: &str) -> Result<Value, DataLayerError> {
    serde_json::from_str(&value).map_err(|err| {
        DataLayerError::UnexpectedValue(format!("{field_name} contains invalid JSON: {err}"))
    })
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::run_migrations as run_sqlite_migrations;

    #[tokio::test]
    async fn sqlite_routing_group_repository_round_trips() {
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .expect("sqlite pool should connect");
        run_sqlite_migrations(&pool)
            .await
            .expect("sqlite migrations should run");

        let repository = SqliteRoutingGroupRepository::new(pool);
        repository
            .create_routing_group(CreateRoutingGroupRecord {
                id: "routing-group-1".to_string(),
                name: "default".to_string(),
                description: Some("initial".to_string()),
                enabled: true,
                is_system_default: true,
                sort_order: 0,
                config_json: json!({"allowed_models": ["gpt-*"]}),
                version: 1,
                created_at: 10,
                updated_at: 10,
                published_at: None,
            })
            .await
            .expect("group should create");

        let system_default = repository
            .find_routing_group(RoutingGroupLookupKey::SystemDefault)
            .await
            .expect("group lookup should succeed")
            .expect("system default should exist");
        assert_eq!(system_default.id, "routing-group-1");

        repository
            .update_routing_group(
                "routing-group-1",
                UpdateRoutingGroupRecord {
                    description: Some(None),
                    version: Some(2),
                    updated_at: 20,
                    published_at: Some(Some(20)),
                    ..UpdateRoutingGroupRecord::default()
                },
            )
            .await
            .expect("group should update");

        let binding = repository
            .create_routing_group_binding(CreateRoutingGroupBindingRecord {
                id: "binding-1".to_string(),
                group_id: "routing-group-1".to_string(),
                subject_type: RoutingGroupBindingSubject::ApiKey,
                subject_id: "api-key-1".to_string(),
                is_default: true,
                allow_explicit_select: true,
                created_at: 10,
                updated_at: 10,
            })
            .await
            .expect("binding should create");

        assert_eq!(binding.subject_type, RoutingGroupBindingSubject::ApiKey);
        assert_eq!(
            repository
                .list_routing_group_bindings(&RoutingGroupBindingQuery {
                    group_id: Some("routing-group-1".to_string()),
                    subject_type: Some(RoutingGroupBindingSubject::ApiKey),
                    subject_id: Some("api-key-1".to_string()),
                })
                .await
                .expect("bindings should list")
                .len(),
            1
        );

        repository
            .create_routing_group_version(CreateRoutingGroupVersionRecord {
                id: "version-1".to_string(),
                group_id: "routing-group-1".to_string(),
                version: 2,
                config_json: json!({"allowed_models": ["gpt-*"]}),
                created_at: 20,
                created_by: Some("admin".to_string()),
            })
            .await
            .expect("version should create");

        assert_eq!(
            repository
                .list_routing_group_versions("routing-group-1")
                .await
                .expect("versions should list")
                .len(),
            1
        );
    }

    #[tokio::test]
    async fn sqlite_keeps_system_and_subject_defaults_unique() {
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .expect("sqlite pool should connect");
        run_sqlite_migrations(&pool)
            .await
            .expect("sqlite migrations should run");
        let repository = SqliteRoutingGroupRepository::new(pool);

        for (id, is_system_default) in [("group-1", true), ("group-2", true), ("group-3", false)] {
            repository
                .create_routing_group(group_record(id, is_system_default))
                .await
                .expect("group should create");
        }
        assert_eq!(system_default_ids(&repository).await, vec!["group-2"]);

        repository
            .update_routing_group(
                "group-1",
                UpdateRoutingGroupRecord {
                    is_system_default: Some(true),
                    updated_at: 2,
                    ..UpdateRoutingGroupRecord::default()
                },
            )
            .await
            .expect("group should update");
        assert_eq!(system_default_ids(&repository).await, vec!["group-1"]);

        repository
            .create_routing_group_binding(binding_record("binding-1", "group-1", "subject-1", true))
            .await
            .expect("binding should create");
        repository
            .create_routing_group_binding(binding_record("binding-2", "group-2", "subject-1", true))
            .await
            .expect("binding should create");
        repository
            .create_routing_group_binding(binding_record("binding-3", "group-3", "subject-2", true))
            .await
            .expect("binding should create");

        assert_eq!(
            default_binding_ids(&repository, "subject-1").await,
            vec!["binding-2"]
        );
        assert_eq!(
            default_binding_ids(&repository, "subject-2").await,
            vec!["binding-3"]
        );

        repository
            .update_routing_group_binding(
                "binding-1",
                UpdateRoutingGroupBindingRecord {
                    is_default: Some(true),
                    updated_at: 2,
                    ..UpdateRoutingGroupBindingRecord::default()
                },
            )
            .await
            .expect("binding should update");
        assert_eq!(
            default_binding_ids(&repository, "subject-1").await,
            vec!["binding-1"]
        );
        assert_eq!(
            default_binding_ids(&repository, "subject-2").await,
            vec!["binding-3"]
        );

        repository
            .update_routing_group_binding(
                "binding-3",
                UpdateRoutingGroupBindingRecord {
                    subject_id: Some("subject-1".to_string()),
                    updated_at: 3,
                    ..UpdateRoutingGroupBindingRecord::default()
                },
            )
            .await
            .expect("binding should move");
        assert_eq!(
            default_binding_ids(&repository, "subject-1").await,
            vec!["binding-3"]
        );
        assert!(default_binding_ids(&repository, "subject-2")
            .await
            .is_empty());
    }

    #[tokio::test]
    async fn sqlite_repair_migration_resolves_existing_duplicate_defaults() {
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .expect("sqlite pool should connect");
        run_sqlite_migrations(&pool)
            .await
            .expect("sqlite migrations should run");
        let repository = SqliteRoutingGroupRepository::new(pool.clone());
        sqlx::raw_sql(
            r#"
DROP INDEX routing_groups_one_system_default_key;
DROP INDEX routing_group_bindings_subject_default_key;
"#,
        )
        .execute(&pool)
        .await
        .expect("unique indexes should be removable to simulate a pre-repair database");

        for id in ["group-1", "group-2", "group-3"] {
            repository
                .create_routing_group(group_record(id, false))
                .await
                .expect("group should create");
        }
        sqlx::query(
            r#"
UPDATE routing_groups
SET is_system_default = 1,
    enabled = CASE id WHEN 'group-3' THEN 0 ELSE 1 END,
    updated_at = CASE id
        WHEN 'group-1' THEN 1
        WHEN 'group-2' THEN 2
        ELSE 3
    END
"#,
        )
        .execute(&pool)
        .await
        .expect("duplicate system defaults should seed");

        for (id, subject_id) in [
            ("binding-3", "subject-1"),
            ("binding-2", "subject-1"),
            ("binding-1", "subject-1"),
            ("binding-4", "subject-2"),
        ] {
            repository
                .create_routing_group_binding(binding_record(id, "group-1", subject_id, false))
                .await
                .expect("binding should create");
        }
        sqlx::query(
            r#"
UPDATE routing_group_bindings
SET is_default = 1,
    created_at = CASE id WHEN 'binding-3' THEN 2 ELSE 1 END
"#,
        )
        .execute(&pool)
        .await
        .expect("duplicate binding defaults should seed");

        let repair_migration =
            include_str!("../migrations/20260727000000_repair_routing_default_uniqueness.sql");
        for _ in 0..2 {
            sqlx::raw_sql(repair_migration)
                .execute(&pool)
                .await
                .expect("repair migration should be idempotent");
        }

        assert_eq!(system_default_ids(&repository).await, vec!["group-2"]);
        assert_eq!(
            default_binding_ids(&repository, "subject-1").await,
            vec!["binding-1"]
        );
        assert_eq!(
            default_binding_ids(&repository, "subject-2").await,
            vec!["binding-4"]
        );

        sqlx::query("UPDATE routing_groups SET is_system_default = 1 WHERE id = 'group-3'")
            .execute(&pool)
            .await
            .expect_err("database should reject a second system default");
        sqlx::query("UPDATE routing_group_bindings SET is_default = 1 WHERE id = 'binding-2'")
            .execute(&pool)
            .await
            .expect_err("database should reject a second default for the same subject");
    }

    fn group_record(id: &str, is_system_default: bool) -> CreateRoutingGroupRecord {
        CreateRoutingGroupRecord {
            id: id.to_string(),
            name: id.to_string(),
            description: None,
            enabled: true,
            is_system_default,
            sort_order: 0,
            config_json: json!({}),
            version: 1,
            created_at: 1,
            updated_at: 1,
            published_at: None,
        }
    }

    fn binding_record(
        id: &str,
        group_id: &str,
        subject_id: &str,
        is_default: bool,
    ) -> CreateRoutingGroupBindingRecord {
        CreateRoutingGroupBindingRecord {
            id: id.to_string(),
            group_id: group_id.to_string(),
            subject_type: RoutingGroupBindingSubject::ApiKey,
            subject_id: subject_id.to_string(),
            is_default,
            allow_explicit_select: true,
            created_at: 1,
            updated_at: 1,
        }
    }

    async fn system_default_ids(repository: &SqliteRoutingGroupRepository) -> Vec<String> {
        repository
            .list_routing_groups()
            .await
            .expect("groups should list")
            .into_iter()
            .filter(|group| group.is_system_default)
            .map(|group| group.id)
            .collect()
    }

    async fn default_binding_ids(
        repository: &SqliteRoutingGroupRepository,
        subject_id: &str,
    ) -> Vec<String> {
        repository
            .list_routing_group_bindings(&RoutingGroupBindingQuery {
                group_id: None,
                subject_type: Some(RoutingGroupBindingSubject::ApiKey),
                subject_id: Some(subject_id.to_string()),
            })
            .await
            .expect("bindings should list")
            .into_iter()
            .filter(|binding| binding.is_default)
            .map(|binding| binding.id)
            .collect()
    }
}
