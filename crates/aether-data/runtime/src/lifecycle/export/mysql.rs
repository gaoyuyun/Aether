use super::*;
use sqlx::Acquire;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct MysqlImportColumns {
    names: ImportColumnNames,
    data_types: BTreeMap<String, String>,
    primary_key: Vec<String>,
}

pub async fn export_mysql_core_jsonl(
    pool: &crate::driver::mysql::MysqlPool,
    created_at_unix_secs: u64,
) -> Result<String, DataLayerError> {
    export_mysql_jsonl(pool, mysql_core_export_domains(), created_at_unix_secs).await
}

pub async fn export_mysql_jsonl(
    pool: &crate::driver::mysql::MysqlPool,
    domains: Vec<ExportDomain>,
    created_at_unix_secs: u64,
) -> Result<String, DataLayerError> {
    let mut connection = pool.acquire().await.map_sql_err()?;
    sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ")
        .execute(&mut *connection)
        .await
        .map_sql_err()?;
    let mut tx = connection.begin().await.map_sql_err()?;
    let manifest = DataExportManifest::new(
        created_at_unix_secs,
        Some(DatabaseDriver::Mysql),
        domains.clone(),
    );
    let mut records = vec![DataExportRecord::manifest(manifest)];

    for domain in domains {
        if domain == ExportDomain::Auxiliary {
            export_mysql_auxiliary_records(&mut tx, &mut records).await?;
            continue;
        }
        if domain == ExportDomain::Billing {
            export_mysql_billing_records(&mut tx, &mut records).await?;
            continue;
        }
        if domain == ExportDomain::Wallets {
            export_mysql_wallet_records(&mut tx, &mut records).await?;
            continue;
        }
        let (table_name, id_column) = mysql_domain_table(domain)?;
        let order_by = export_order_by(domain, id_column);
        let sql = format!("SELECT * FROM {table_name} ORDER BY {order_by}");
        let rows = sqlx::query(&sql).fetch_all(&mut *tx).await.map_sql_err()?;
        for row in rows {
            let id = mysql_export_row_id(domain, &row, id_column)?;
            records.push(DataExportRecord::row(domain, id, mysql_row_payload(&row)?));
        }
    }

    tx.commit().await.map_sql_err()?;
    encode_jsonl(&records)
}

pub async fn import_mysql_jsonl(
    pool: &crate::driver::mysql::MysqlPool,
    input: &str,
) -> Result<usize, DataLayerError> {
    let plan = build_import_plan(input)?;
    import_mysql_plan(pool, &plan).await
}

pub async fn import_mysql_plan(
    pool: &crate::driver::mysql::MysqlPool,
    plan: &DataImportPlan,
) -> Result<usize, DataLayerError> {
    let identity_scope = IdentityImportScope::from_plan(plan)?;
    let mut tx = pool.begin().await.map_sql_err()?;
    let identity_state = capture_mysql_identity_import_state(&mut tx, &identity_scope).await?;
    let mut imported = 0usize;
    let mut column_cache = BTreeMap::<String, MysqlImportColumns>::new();
    for domain in &plan.manifest.domains {
        if *domain == ExportDomain::Auxiliary {
            for row in plan.rows(*domain) {
                import_mysql_auxiliary_row(&mut tx, row, &mut column_cache).await?;
                imported = imported.saturating_add(1);
            }
            continue;
        }
        if *domain == ExportDomain::Billing {
            for row in plan.rows(*domain) {
                import_mysql_billing_row(&mut tx, row, &mut column_cache).await?;
                imported = imported.saturating_add(1);
            }
            continue;
        }
        if *domain == ExportDomain::Wallets {
            for row in plan.rows(*domain) {
                import_mysql_wallet_row(&mut tx, row, &mut column_cache).await?;
                imported = imported.saturating_add(1);
            }
            continue;
        }
        let (table_name, _id_column) = mysql_domain_table(*domain)?;
        let target_columns =
            mysql_import_columns_cached(&mut tx, &mut column_cache, table_name).await?;
        for row in plan.rows(*domain) {
            import_mysql_row(&mut tx, table_name, *domain, row, &target_columns).await?;
            imported = imported.saturating_add(1);
        }
    }
    enforce_mysql_identity_import_invariants(&mut tx, &identity_scope, identity_state).await?;
    tx.commit().await.map_sql_err()?;
    Ok(imported)
}

async fn capture_mysql_identity_import_state(
    tx: &mut sqlx::Transaction<'_, sqlx::MySql>,
    scope: &IdentityImportScope,
) -> Result<IdentityImportState, DataLayerError> {
    let mut affected_user_ids = if scope.finalizes_oauth_links {
        scope.user_ids.iter().cloned().collect::<BTreeSet<_>>()
    } else {
        BTreeSet::new()
    };
    for link_id in &scope.oauth_link_ids {
        if let Some(user_id) =
            sqlx::query_scalar::<_, String>("SELECT user_id FROM user_oauth_links WHERE id = ?")
                .bind(link_id)
                .fetch_optional(&mut **tx)
                .await
                .map_sql_err()?
        {
            affected_user_ids.insert(user_id);
        }
    }
    for provider_type in &scope.oauth_provider_types {
        let user_ids = sqlx::query_scalar::<_, String>(
            "SELECT user_id FROM user_oauth_links WHERE provider_type = ?",
        )
        .bind(provider_type)
        .fetch_all(&mut **tx)
        .await
        .map_sql_err()?;
        affected_user_ids.extend(user_ids);
    }
    Ok(IdentityImportState { affected_user_ids })
}

async fn enforce_mysql_identity_import_invariants(
    tx: &mut sqlx::Transaction<'_, sqlx::MySql>,
    scope: &IdentityImportScope,
    mut state: IdentityImportState,
) -> Result<(), DataLayerError> {
    for user_id in &scope.user_ids {
        let auth_source =
            sqlx::query_scalar::<_, String>("SELECT auth_source FROM users WHERE id = ? LIMIT 1")
                .bind(user_id)
                .fetch_optional(&mut **tx)
                .await
                .map_sql_err()?
                .ok_or_else(|| {
                    DataLayerError::InvalidInput(format!(
                        "imported users row '{user_id}' did not produce a user record"
                    ))
                })?;
        if !matches!(auth_source.as_str(), "local" | "ldap" | "oauth") {
            return Err(DataLayerError::InvalidInput(format!(
                "imported user '{user_id}' has unsupported auth_source '{auth_source}'"
            )));
        }
        if auth_source == "oauth" {
            sqlx::query("UPDATE users SET email_verified = 0 WHERE id = ?")
                .bind(user_id)
                .execute(&mut **tx)
                .await
                .map_sql_err()?;
        }
    }

    for link_id in &scope.oauth_link_ids {
        let user_id = sqlx::query_scalar::<_, String>(
            "SELECT user_id FROM user_oauth_links WHERE id = ? LIMIT 1",
        )
        .bind(link_id)
        .fetch_optional(&mut **tx)
        .await
        .map_sql_err()?
        .ok_or_else(|| {
            DataLayerError::InvalidInput(format!(
                "imported OAuth link row '{link_id}' did not produce a link record"
            ))
        })?;
        state.affected_user_ids.insert(user_id);
    }

    for link_id in &scope.oauth_link_ids {
        if let Some((provider_type, provider_user_id)) = sqlx::query_as::<_, (String, String)>(
            r#"
SELECT imported.provider_type, imported.provider_user_id
FROM user_oauth_links imported
JOIN user_oauth_links duplicate
  ON duplicate.provider_type = imported.provider_type
 AND duplicate.provider_user_id = imported.provider_user_id
 AND duplicate.id <> imported.id
WHERE imported.id = ?
LIMIT 1
"#,
        )
        .bind(link_id)
        .fetch_optional(&mut **tx)
        .await
        .map_sql_err()?
        {
            return Err(DataLayerError::InvalidInput(format!(
                "OAuth import assigns provider identity '{provider_type}:{provider_user_id}' more than once"
            )));
        }
    }

    for link_id in &scope.oauth_link_ids {
        if let Some((user_id, provider_type)) = sqlx::query_as::<_, (String, String)>(
            r#"
SELECT imported.user_id, imported.provider_type
FROM user_oauth_links imported
JOIN user_oauth_links duplicate
  ON duplicate.user_id = imported.user_id
 AND duplicate.provider_type = imported.provider_type
 AND duplicate.id <> imported.id
WHERE imported.id = ?
LIMIT 1
"#,
        )
        .bind(link_id)
        .fetch_optional(&mut **tx)
        .await
        .map_sql_err()?
        {
            return Err(DataLayerError::InvalidInput(format!(
                "OAuth import links user '{user_id}' to provider '{provider_type}' more than once"
            )));
        }
    }

    for link_id in &scope.oauth_link_ids {
        if let Some(invalid_id) = sqlx::query_scalar::<_, String>(
            r#"
SELECT links.id
FROM user_oauth_links links
LEFT JOIN users ON users.id = links.user_id
LEFT JOIN oauth_providers providers ON providers.provider_type = links.provider_type
WHERE links.id = ?
  AND (
       users.id IS NULL
    OR providers.provider_type IS NULL
    OR BINARY links.provider_type <> BINARY LOWER(TRIM(links.provider_type))
    OR links.provider_type = ''
    OR BINARY links.provider_user_id <> BINARY TRIM(links.provider_user_id)
    OR links.provider_user_id = ''
    OR BINARY providers.provider_type <> BINARY LOWER(TRIM(providers.provider_type))
  )
LIMIT 1
"#,
        )
        .bind(link_id)
        .fetch_optional(&mut **tx)
        .await
        .map_sql_err()?
        {
            return Err(DataLayerError::InvalidInput(format!(
                "OAuth import produced invalid or orphaned link '{invalid_id}'"
            )));
        }
    }

    if !scope.validates_oauth_login_methods {
        return Ok(());
    }
    for user_id in state.affected_user_ids {
        if sqlx::query_scalar::<_, String>(
            r#"
SELECT users.id
FROM users
WHERE users.id = ?
  AND users.auth_source = 'oauth'
  AND users.is_active = 1
  AND users.is_deleted = 0
  AND NOT EXISTS (
    SELECT 1
    FROM user_oauth_links links
    JOIN oauth_providers providers ON providers.provider_type = links.provider_type
    WHERE links.user_id = users.id
      AND providers.is_enabled = 1
      AND BINARY links.provider_type = BINARY LOWER(TRIM(links.provider_type))
      AND BINARY links.provider_user_id = BINARY TRIM(links.provider_user_id)
      AND links.provider_user_id <> ''
  )
LIMIT 1
"#,
        )
        .bind(&user_id)
        .fetch_optional(&mut **tx)
        .await
        .map_sql_err()?
        .is_some()
        {
            return Err(DataLayerError::InvalidInput(format!(
                "OAuth import would leave active user '{user_id}' without an enabled identity binding"
            )));
        }
    }

    Ok(())
}

fn mysql_domain_table(
    domain: ExportDomain,
) -> Result<(&'static str, &'static str), DataLayerError> {
    match domain {
        ExportDomain::Users => Ok(("users", "id")),
        ExportDomain::ApiKeys => Ok(("api_keys", "id")),
        ExportDomain::Providers => Ok(("providers", "id")),
        ExportDomain::ProviderKeys => Ok(("provider_api_keys", "id")),
        ExportDomain::Endpoints => Ok(("provider_endpoints", "id")),
        ExportDomain::Models => Ok(("models", "id")),
        ExportDomain::GlobalModels => Ok(("global_models", "id")),
        ExportDomain::AuthModules => Ok(("auth_modules", "id")),
        ExportDomain::OAuthProviders => Ok(("oauth_providers", "provider_type")),
        ExportDomain::UserOAuthLinks => Ok(("user_oauth_links", "id")),
        ExportDomain::UserGroups => Ok(("user_groups", "id")),
        ExportDomain::UserGroupMembers => Ok(("user_group_members", "group_id")),
        ExportDomain::ProxyNodes => Ok(("proxy_nodes", "id")),
        ExportDomain::SystemConfigs => Ok(("system_configs", "id")),
        ExportDomain::Wallets => Err(DataLayerError::InvalidInput(
            "mysql wallet export uses multiple tables and must be handled as a domain".to_string(),
        )),
        ExportDomain::Usage => Ok(("`usage`", "request_id")),
        ExportDomain::Billing => Err(DataLayerError::InvalidInput(
            "mysql billing export uses multiple tables and must be handled as a domain".to_string(),
        )),
        ExportDomain::Auxiliary => Err(DataLayerError::InvalidInput(
            "mysql auxiliary export uses multiple tables and must be handled as a domain"
                .to_string(),
        )),
    }
}

async fn export_mysql_auxiliary_records(
    tx: &mut sqlx::Transaction<'_, sqlx::MySql>,
    records: &mut Vec<DataExportRecord>,
) -> Result<(), DataLayerError> {
    for table in AUXILIARY_TABLES {
        let table_sql = mysql_quote_identifier(table.name)?;
        let order_sql = table
            .primary_key
            .iter()
            .map(|column| mysql_quote_identifier(column).map(|column| format!("{column} ASC")))
            .collect::<Result<Vec<_>, _>>()?
            .join(", ");
        let rows = sqlx::query(&format!("SELECT * FROM {table_sql} ORDER BY {order_sql}"))
            .fetch_all(&mut **tx)
            .await
            .map_sql_err()?;
        for row in rows {
            let payload = mysql_row_payload(&row)?;
            let id = auxiliary_row_id(*table, &payload)?;
            records.push(DataExportRecord::row(
                ExportDomain::Auxiliary,
                id,
                payload_with_table(payload, table.name)?,
            ));
        }
    }
    Ok(())
}

fn mysql_export_row_id(
    domain: ExportDomain,
    row: &sqlx::mysql::MySqlRow,
    id_column: &str,
) -> Result<String, DataLayerError> {
    if domain == ExportDomain::UserGroupMembers {
        let group_id = mysql_required_export_text(row, "group_id", domain)?;
        let user_id = mysql_required_export_text(row, "user_id", domain)?;
        return Ok(format!("{group_id}:{user_id}"));
    }
    mysql_required_export_text(row, id_column, domain)
}

fn mysql_required_export_text(
    row: &sqlx::mysql::MySqlRow,
    column: &str,
    domain: ExportDomain,
) -> Result<String, DataLayerError> {
    row.try_get::<Option<String>, _>(column)
        .map_sql_err()?
        .ok_or_else(|| {
            DataLayerError::UnexpectedValue(format!(
                "{} export row has null id column '{}'",
                domain.as_str(),
                column
            ))
        })
}

async fn export_mysql_billing_records(
    tx: &mut sqlx::Transaction<'_, sqlx::MySql>,
    records: &mut Vec<DataExportRecord>,
) -> Result<(), DataLayerError> {
    for (table_name, id_column) in [
        ("billing_rules", "id"),
        ("dimension_collectors", "id"),
        ("usage_settlement_snapshots", "request_id"),
    ] {
        let sql = format!("SELECT * FROM {table_name} ORDER BY {id_column} ASC");
        let rows = sqlx::query(&sql).fetch_all(&mut **tx).await.map_sql_err()?;
        for row in rows {
            let id = row
                .try_get::<Option<String>, _>(id_column)
                .map_sql_err()?
                .ok_or_else(|| {
                    DataLayerError::UnexpectedValue(format!(
                        "billing export row in table '{table_name}' has null id"
                    ))
                })?;
            records.push(DataExportRecord::row(
                ExportDomain::Billing,
                format!("{table_name}:{id}"),
                payload_with_table(mysql_row_payload(&row)?, table_name)?,
            ));
        }
    }
    Ok(())
}

async fn export_mysql_wallet_records(
    tx: &mut sqlx::Transaction<'_, sqlx::MySql>,
    records: &mut Vec<DataExportRecord>,
) -> Result<(), DataLayerError> {
    for (table_name, id_column) in mysql_wallet_tables() {
        let sql = format!("SELECT * FROM {table_name} ORDER BY {id_column} ASC");
        let rows = sqlx::query(&sql).fetch_all(&mut **tx).await.map_sql_err()?;
        for row in rows {
            let id = row
                .try_get::<Option<String>, _>(id_column)
                .map_sql_err()?
                .ok_or_else(|| {
                    DataLayerError::UnexpectedValue(format!(
                        "wallet export row in table '{table_name}' has null id"
                    ))
                })?;
            records.push(DataExportRecord::row(
                ExportDomain::Wallets,
                format!("{table_name}:{id}"),
                payload_with_table(mysql_row_payload(&row)?, table_name)?,
            ));
        }
    }
    Ok(())
}

async fn import_mysql_row(
    tx: &mut sqlx::Transaction<'_, sqlx::MySql>,
    table_name: &str,
    domain: ExportDomain,
    row: &ExportRow,
    target_columns: &MysqlImportColumns,
) -> Result<(), DataLayerError> {
    let mut object =
        filter_import_payload("mysql", table_name, domain, row, &target_columns.names)?;
    deactivate_imported_credentials(table_name, &mut object, |column_name| {
        target_columns.names.contains(column_name)
    });

    let columns = object.keys().map(String::as_str).collect::<Vec<_>>();
    for primary_key in &target_columns.primary_key {
        if object.get(primary_key).is_none_or(Value::is_null) {
            return Err(DataLayerError::InvalidInput(format!(
                "{} export row '{}' is missing non-null primary key column '{}' for mysql table '{}'",
                domain.as_str(),
                row.id,
                primary_key,
                table_name
            )));
        }
    }

    let primary_key_predicate = target_columns
        .primary_key
        .iter()
        .map(|column| mysql_quote_identifier(column).map(|column| format!("{column} = ?")))
        .collect::<Result<Vec<_>, _>>()?
        .join(" AND ");
    let lock_sql =
        format!("SELECT 1 FROM {table_name} WHERE {primary_key_predicate} LIMIT 1 FOR UPDATE");
    let mut lock_query = sqlx::query(&lock_sql);
    for column in &target_columns.primary_key {
        lock_query =
            bind_mysql_import_column(lock_query, &object, target_columns, table_name, column)?;
    }
    let exists = lock_query
        .fetch_optional(&mut **tx)
        .await
        .map_sql_err()?
        .is_some();

    if exists {
        let update_columns = columns
            .iter()
            .copied()
            .filter(|column| !target_columns.primary_key.iter().any(|key| key == column))
            .collect::<Vec<_>>();
        if update_columns.is_empty() {
            return Ok(());
        }
        let update_sql = update_columns
            .iter()
            .map(|column| mysql_quote_identifier(column).map(|column| format!("{column} = ?")))
            .collect::<Result<Vec<_>, _>>()?
            .join(", ");
        let sql = format!("UPDATE {table_name} SET {update_sql} WHERE {primary_key_predicate}");
        let mut query = sqlx::query(&sql);
        for column in update_columns {
            query = bind_mysql_import_column(query, &object, target_columns, table_name, column)?;
        }
        for column in &target_columns.primary_key {
            query = bind_mysql_import_column(query, &object, target_columns, table_name, column)?;
        }
        query.execute(&mut **tx).await.map_sql_err()?;
        return Ok(());
    }

    let column_sql = columns
        .iter()
        .map(|column| mysql_quote_identifier(column))
        .collect::<Result<Vec<_>, _>>()?
        .join(", ");
    let placeholder_sql = vec!["?"; columns.len()].join(", ");
    let sql = format!("INSERT INTO {table_name} ({column_sql}) VALUES ({placeholder_sql})");
    let mut query = sqlx::query(&sql);
    for column in columns {
        query = bind_mysql_import_column(query, &object, target_columns, table_name, column)?;
    }
    query.execute(&mut **tx).await.map_sql_err()?;
    Ok(())
}

fn bind_mysql_import_column<'q>(
    query: sqlx::query::Query<'q, sqlx::MySql, sqlx::mysql::MySqlArguments>,
    object: &'q serde_json::Map<String, Value>,
    target_columns: &MysqlImportColumns,
    table_name: &str,
    column: &str,
) -> Result<sqlx::query::Query<'q, sqlx::MySql, sqlx::mysql::MySqlArguments>, DataLayerError> {
    let value = object
        .get(column)
        .expect("column name came from payload object keys");
    let data_type = target_columns
        .data_types
        .get(column)
        .map(String::as_str)
        .unwrap_or_default();
    bind_mysql_import_value(query, value, table_name, column, data_type)
}

async fn import_mysql_billing_row(
    tx: &mut sqlx::Transaction<'_, sqlx::MySql>,
    row: &ExportRow,
    column_cache: &mut BTreeMap<String, MysqlImportColumns>,
) -> Result<(), DataLayerError> {
    let (table_name, payload) = billing_payload_table(row)?;
    let table_name = mysql_billing_table_name(&table_name)?;
    let target_columns = mysql_import_columns_cached(tx, column_cache, table_name).await?;
    import_mysql_row(
        tx,
        table_name,
        ExportDomain::Billing,
        &ExportRow {
            id: row.id.clone(),
            payload,
        },
        &target_columns,
    )
    .await
}

async fn import_mysql_auxiliary_row(
    tx: &mut sqlx::Transaction<'_, sqlx::MySql>,
    row: &ExportRow,
    column_cache: &mut BTreeMap<String, MysqlImportColumns>,
) -> Result<(), DataLayerError> {
    let (table_name, payload) = domain_payload_table(row, "auxiliary", None)?;
    let table = auxiliary_table(&table_name)?;
    let target_columns = mysql_import_columns_cached(tx, column_cache, table.name).await?;
    import_mysql_row(
        tx,
        table.name,
        ExportDomain::Auxiliary,
        &ExportRow {
            id: row.id.clone(),
            payload,
        },
        &target_columns,
    )
    .await
}

fn mysql_billing_table_name(table_name: &str) -> Result<&'static str, DataLayerError> {
    match table_name {
        "billing_rules" => Ok("billing_rules"),
        "dimension_collectors" => Ok("dimension_collectors"),
        "usage_settlement_snapshots" => Ok("usage_settlement_snapshots"),
        other => Err(DataLayerError::InvalidInput(format!(
            "unsupported mysql billing export table '{other}'"
        ))),
    }
}

async fn import_mysql_wallet_row(
    tx: &mut sqlx::Transaction<'_, sqlx::MySql>,
    row: &ExportRow,
    column_cache: &mut BTreeMap<String, MysqlImportColumns>,
) -> Result<(), DataLayerError> {
    let (table_name, payload) = domain_payload_table(row, "wallet", Some("wallets"))?;
    let table_name = mysql_wallet_table_name(&table_name)?;
    let target_columns = mysql_import_columns_cached(tx, column_cache, table_name).await?;
    import_mysql_row(
        tx,
        table_name,
        ExportDomain::Wallets,
        &ExportRow {
            id: row.id.clone(),
            payload,
        },
        &target_columns,
    )
    .await
}

fn mysql_wallet_tables() -> &'static [(&'static str, &'static str)] {
    &[
        ("wallets", "id"),
        ("wallet_transactions", "id"),
        ("wallet_daily_usage_ledgers", "id"),
        ("payment_orders", "id"),
        ("payment_callbacks", "id"),
        ("refund_requests", "id"),
        ("redeem_code_batches", "id"),
        ("redeem_codes", "id"),
    ]
}

fn mysql_wallet_table_name(table_name: &str) -> Result<&'static str, DataLayerError> {
    mysql_wallet_tables()
        .iter()
        .find(|(candidate, _)| *candidate == table_name)
        .map(|(table, _)| *table)
        .ok_or_else(|| {
            DataLayerError::InvalidInput(format!(
                "unsupported mysql wallet export table '{table_name}'"
            ))
        })
}

async fn mysql_import_columns_cached(
    tx: &mut sqlx::Transaction<'_, sqlx::MySql>,
    cache: &mut BTreeMap<String, MysqlImportColumns>,
    table_name: &str,
) -> Result<MysqlImportColumns, DataLayerError> {
    if let Some(columns) = cache.get(table_name) {
        return Ok(columns.clone());
    }

    let columns = load_mysql_import_columns(tx, table_name).await?;
    cache.insert(table_name.to_string(), columns.clone());
    Ok(columns)
}

async fn load_mysql_import_columns(
    tx: &mut sqlx::Transaction<'_, sqlx::MySql>,
    table_name: &str,
) -> Result<MysqlImportColumns, DataLayerError> {
    let relation_name = table_name.trim_matches('`');
    let rows = sqlx::query(
        r#"
SELECT
    CAST(COLUMN_NAME AS CHAR) AS column_name,
    CAST(DATA_TYPE AS CHAR) AS data_type,
    CAST(COLUMN_KEY AS CHAR) AS column_key,
  ORDINAL_POSITION AS ordinal_position
FROM information_schema.columns
WHERE table_schema = DATABASE()
  AND table_name = ?
"#,
    )
    .bind(relation_name)
    .fetch_all(&mut **tx)
    .await
    .map_sql_err()?;

    let mut columns = MysqlImportColumns::default();
    let mut primary_key = BTreeMap::new();
    for row in rows {
        let name = row.try_get::<String, _>("column_name").map_sql_err()?;
        let data_type = row
            .try_get::<String, _>("data_type")
            .map_sql_err()?
            .to_ascii_lowercase();
        columns.names.insert(name.clone());
        columns.data_types.insert(name.clone(), data_type);
        if row
            .try_get::<String, _>("column_key")
            .map_sql_err()?
            .eq_ignore_ascii_case("PRI")
        {
            primary_key.insert(
                row.try_get::<u32, _>("ordinal_position").map_sql_err()?,
                name,
            );
        }
    }

    if columns.names.is_empty() {
        return Err(DataLayerError::UnexpectedValue(format!(
            "mysql import target table '{table_name}' has no visible columns"
        )));
    }
    if primary_key.is_empty() {
        return Err(DataLayerError::UnexpectedValue(format!(
            "mysql import target table '{table_name}' has no primary key"
        )));
    }
    columns.primary_key = primary_key.into_values().collect();

    Ok(columns)
}

fn bind_mysql_import_value<'q>(
    query: sqlx::query::Query<'q, sqlx::MySql, sqlx::mysql::MySqlArguments>,
    json_value: &'q Value,
    table_name: &str,
    column_name: &str,
    data_type: &str,
) -> Result<sqlx::query::Query<'q, sqlx::MySql, sqlx::mysql::MySqlArguments>, DataLayerError> {
    if matches!(
        data_type,
        "binary" | "varbinary" | "blob" | "tinyblob" | "mediumblob" | "longblob"
    ) {
        return match normalize_imported_binary("mysql", column_name, json_value)? {
            Some(bytes) => Ok(query.bind(bytes)),
            None => Ok(query.bind(Option::<Vec<u8>>::None)),
        };
    }
    if matches!(data_type, "decimal" | "numeric") {
        return match normalize_mysql_decimal_value(column_name, json_value)? {
            Some(value) => Ok(query.bind(value)),
            None => Ok(query.bind(Option::<String>::None)),
        };
    }
    let has_integer_type = matches!(
        data_type,
        "tinyint" | "smallint" | "mediumint" | "int" | "integer" | "bigint"
    );
    if !has_integer_type || !import_column_stores_timestamp(column_name) {
        return bind_mysql_json_value(query, json_value);
    }

    match normalize_imported_integer_timestamp("mysql", table_name, column_name, json_value)? {
        Some(timestamp) => Ok(query.bind(timestamp)),
        None => Ok(query.bind(Option::<i64>::None)),
    }
}

fn normalize_mysql_decimal_value(
    column_name: &str,
    value: &Value,
) -> Result<Option<String>, DataLayerError> {
    match value {
        Value::Null => Ok(None),
        Value::Number(value) => Ok(Some(value.to_string())),
        Value::String(value) => Ok(Some(value.clone())),
        Value::Bool(_) | Value::Array(_) | Value::Object(_) => {
            Err(DataLayerError::InvalidInput(format!(
                "mysql decimal import column '{column_name}' must contain a number or numeric string"
            )))
        }
    }
}

fn mysql_quote_identifier(identifier: &str) -> Result<String, DataLayerError> {
    if identifier.trim().is_empty() {
        return Err(DataLayerError::InvalidInput(
            "mysql import column name cannot be empty".to_string(),
        ));
    }
    if !identifier
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || ch == '_')
    {
        return Err(DataLayerError::InvalidInput(format!(
            "mysql import column name '{identifier}' contains unsupported characters"
        )));
    }
    Ok(format!("`{identifier}`"))
}

fn bind_mysql_json_value<'q>(
    query: sqlx::query::Query<'q, sqlx::MySql, sqlx::mysql::MySqlArguments>,
    value: &'q Value,
) -> Result<sqlx::query::Query<'q, sqlx::MySql, sqlx::mysql::MySqlArguments>, DataLayerError> {
    Ok(match value {
        Value::Null => query.bind(Option::<String>::None),
        Value::Bool(value) => query.bind(i64::from(*value)),
        Value::Number(value) => {
            if let Some(value) = value.as_i64() {
                query.bind(value)
            } else if let Some(value) = value.as_u64() {
                let value = i64::try_from(value).map_err(|_| {
                    DataLayerError::InvalidInput(format!(
                        "mysql import integer value {value} exceeds i64"
                    ))
                })?;
                query.bind(value)
            } else if let Some(value) = value.as_f64() {
                query.bind(value)
            } else {
                return Err(DataLayerError::InvalidInput(
                    "mysql import number is not representable".to_string(),
                ));
            }
        }
        Value::String(value) => query.bind(value),
        Value::Array(_) | Value::Object(_) => {
            let value = serde_json::to_string(value)
                .map_err(|err| DataLayerError::UnexpectedValue(err.to_string()))?;
            query.bind(value)
        }
    })
}

fn mysql_row_payload(row: &sqlx::mysql::MySqlRow) -> Result<Value, DataLayerError> {
    let mut object = serde_json::Map::new();
    for (index, column) in row.columns().iter().enumerate() {
        object.insert(column.name().to_string(), mysql_value_to_json(row, index)?);
    }
    Ok(Value::Object(object))
}

fn mysql_value_to_json(row: &sqlx::mysql::MySqlRow, index: usize) -> Result<Value, DataLayerError> {
    let raw = row.try_get_raw(index).map_sql_err()?;
    if raw.is_null() {
        return Ok(Value::Null);
    }

    match raw.type_info().name().to_ascii_uppercase().as_str() {
        "BOOL" | "BOOLEAN" => Ok(Value::Bool(row.try_get::<bool, _>(index).map_sql_err()?)),
        "TINYINT" | "TINY" | "SMALLINT" | "SHORT" | "MEDIUMINT" | "INT24" | "INT" | "INTEGER"
        | "LONG" | "BIGINT" | "LONGLONG" | "YEAR" => {
            Ok(Value::from(row.try_get::<i64, _>(index).map_sql_err()?))
        }
        "FLOAT" | "DOUBLE" => {
            let value = row.try_get::<f64, _>(index).map_sql_err()?;
            serde_json::Number::from_f64(value)
                .map(Value::Number)
                .ok_or_else(|| {
                    DataLayerError::UnexpectedValue(format!(
                        "mysql export column {} contains non-finite float",
                        index
                    ))
                })
        }
        "DECIMAL" | "NEWDECIMAL" => Ok(Value::String(
            row.try_get::<sqlx::types::BigDecimal, _>(index)
                .map_sql_err()?
                .to_string(),
        )),
        "VARCHAR" | "VAR_STRING" | "STRING" | "TEXT" | "TINYTEXT" | "MEDIUMTEXT" | "LONGTEXT"
        | "JSON" | "ENUM" | "SET" | "DATE" | "DATETIME" | "TIMESTAMP" | "TIME" => Ok(
            Value::String(row.try_get::<String, _>(index).map_sql_err()?),
        ),
        "BLOB" | "TINYBLOB" | "MEDIUMBLOB" | "LONGBLOB" | "BIT" | "GEOMETRY" => {
            let bytes = row.try_get::<Vec<u8>, _>(index).map_sql_err()?;
            Ok(Value::Array(bytes.into_iter().map(Value::from).collect()))
        }
        other => Err(DataLayerError::UnexpectedValue(format!(
            "unsupported mysql export column type '{other}' at index {index}"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::normalize_mysql_decimal_value;
    use serde_json::json;

    #[test]
    fn decimal_import_binds_numbers_and_strings_as_decimal_text() {
        let value = json!(12345.12345678);
        assert_eq!(
            normalize_mysql_decimal_value("billing_total_cost_usd", &value)
                .expect("decimal value should normalize")
                .as_deref(),
            Some("12345.12345678")
        );
        assert_eq!(
            normalize_mysql_decimal_value(
                "billing_total_cost_usd",
                &json!("123456789012.12345678")
            )
            .expect("decimal string should normalize")
            .as_deref(),
            Some("123456789012.12345678")
        );
        assert!(normalize_mysql_decimal_value("billing_total_cost_usd", &json!(true)).is_err());
    }
}
