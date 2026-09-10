use async_trait::async_trait;
use sqlx::{mysql::MySqlRow, Row};

use aether_data_contracts::repository::proxy_nodes::{
    bucket_start_unix_secs, build_tunnel_error_event_detail, build_tunnel_metrics_sample,
    merge_proxy_metadata_for_registration, normalize_heartbeat_proxy_metadata,
    normalize_proxy_metadata, proxy_metadata_has_explicit_tunnel_security,
    reconcile_remote_config_after_heartbeat, ProxyNodeEventQuery, ProxyNodeHeartbeatMutation,
    ProxyNodeManualCreateMutation, ProxyNodeManualUpdateMutation, ProxyNodeMetricsCleanupSummary,
    ProxyNodeMetricsStep, ProxyNodeReadRepository, ProxyNodeRegistrationMutation,
    ProxyNodeRemoteConfigMutation, ProxyNodeTrafficMutation, ProxyNodeTunnelStatusMutation,
    ProxyNodeWriteRepository, StoredProxyFleetMetricsBucket, StoredProxyNode, StoredProxyNodeEvent,
    StoredProxyNodeMetricsBucket, TunnelErrorEventRecord, TunnelMetricsSample,
    PROXY_NODE_EVENT_TYPE_TUNNEL_ERROR,
};
use aether_data_contracts::DataLayerError;

use crate::error::SqlResultExt;
use crate::MysqlPool;

const PROXY_NODE_REGISTRATION_CAS_RETRIES: usize = 8;

fn log_reported_tunnel_error_event(
    node_id: &str,
    event: &TunnelErrorEventRecord,
    received_at_unix_secs: u64,
) {
    tracing::warn!(
        event_name = "proxy_tunnel_error_reported",
        source = "heartbeat",
        node_id = %node_id,
        category = %event.category,
        message = %event.message,
        severity = ?event.severity,
        component = ?event.component,
        summary = ?event.summary,
        operator_action = ?event.operator_action,
        error_reported_at_unix_secs = event.timestamp_unix_secs,
        error_reported_at_unix_ms = ?event.timestamp_unix_ms,
        report_received_at_unix_secs = received_at_unix_secs,
        "proxy reported tunnel error via heartbeat"
    );
}

#[derive(Debug, Clone)]
pub struct MysqlProxyNodeReadRepository {
    pool: MysqlPool,
}

impl MysqlProxyNodeReadRepository {
    pub fn new(pool: MysqlPool) -> Self {
        Self { pool }
    }

    async fn write_node(
        &self,
        node: &StoredProxyNode,
        update_existing: bool,
    ) -> Result<(), DataLayerError> {
        let now = current_unix_secs();
        let upsert_sql = r#"
INSERT INTO proxy_nodes (
  id, tunnel_generation, name, ip, port, region, status, registered_by, last_heartbeat_at,
  heartbeat_interval, active_connections, total_requests, avg_latency_ms,
  is_manual, proxy_url, proxy_username, proxy_password, created_at,
  updated_at, remote_config, config_version, hardware_info,
  estimated_max_concurrency, tunnel_mode, tunnel_connected, tunnel_connected_at,
  failed_requests, dns_failures, stream_errors, proxy_metadata
)
SELECT ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?
WHERE ? OR NOT EXISTS (
  SELECT 1 FROM proxy_nodes WHERE ip = ? AND port = ? AND is_manual = 0
)
ON DUPLICATE KEY UPDATE
  name = VALUES(name),
  ip = VALUES(ip),
  port = VALUES(port),
  region = VALUES(region),
  status = VALUES(status),
  registered_by = VALUES(registered_by),
  last_heartbeat_at = VALUES(last_heartbeat_at),
  heartbeat_interval = VALUES(heartbeat_interval),
  active_connections = VALUES(active_connections),
  total_requests = VALUES(total_requests),
  avg_latency_ms = VALUES(avg_latency_ms),
  is_manual = VALUES(is_manual),
  proxy_url = VALUES(proxy_url),
  proxy_username = VALUES(proxy_username),
  proxy_password = VALUES(proxy_password),
  updated_at = VALUES(updated_at),
  remote_config = VALUES(remote_config),
  config_version = VALUES(config_version),
  hardware_info = VALUES(hardware_info),
  estimated_max_concurrency = VALUES(estimated_max_concurrency),
  tunnel_mode = VALUES(tunnel_mode),
  tunnel_connected = VALUES(tunnel_connected),
  tunnel_connected_at = VALUES(tunnel_connected_at),
  failed_requests = VALUES(failed_requests),
  dns_failures = VALUES(dns_failures),
  stream_errors = VALUES(stream_errors),
  proxy_metadata = VALUES(proxy_metadata)
"#;
        let sql = if update_existing {
            upsert_sql
        } else {
            upsert_sql
                .split_once("\nON DUPLICATE KEY UPDATE")
                .map(|(insert_sql, _)| insert_sql)
                .expect("proxy node upsert SQL should contain its conflict clause")
        };
        let result = sqlx::query(sql)
            .bind(&node.id)
            .bind(&node.tunnel_generation)
            .bind(&node.name)
            .bind(&node.ip)
            .bind(node.port)
            .bind(&node.region)
            .bind(&node.status)
            .bind(&node.registered_by)
            .bind(optional_i64_from_u64(
                node.last_heartbeat_at_unix_secs,
                "proxy_nodes.last_heartbeat_at",
            )?)
            .bind(node.heartbeat_interval)
            .bind(node.active_connections)
            .bind(node.total_requests)
            .bind(node.avg_latency_ms)
            .bind(node.is_manual)
            .bind(&node.proxy_url)
            .bind(&node.proxy_username)
            .bind(&node.proxy_password)
            .bind(node.created_at_unix_ms.unwrap_or(now) as i64)
            .bind(node.updated_at_unix_secs.unwrap_or(now) as i64)
            .bind(optional_json_to_string(
                &node.remote_config,
                "proxy_nodes.remote_config",
            )?)
            .bind(node.config_version)
            .bind(optional_json_to_string(
                &node.hardware_info,
                "proxy_nodes.hardware_info",
            )?)
            .bind(node.estimated_max_concurrency)
            .bind(node.tunnel_mode)
            .bind(node.tunnel_connected)
            .bind(optional_i64_from_u64(
                node.tunnel_connected_at_unix_secs,
                "proxy_nodes.tunnel_connected_at",
            )?)
            .bind(node.failed_requests)
            .bind(node.dns_failures)
            .bind(node.stream_errors)
            .bind(optional_json_to_string(
                &node.proxy_metadata,
                "proxy_nodes.proxy_metadata",
            )?)
            .bind(update_existing || node.is_manual)
            .bind(&node.ip)
            .bind(node.port)
            .execute(&self.pool)
            .await
            .map_sql_err()?;
        if result.rows_affected() == 0 {
            return Err(proxy_node_registration_changed_error());
        }
        Ok(())
    }

    async fn insert_node(&self, node: &StoredProxyNode) -> Result<(), DataLayerError> {
        self.write_node(node, false).await
    }

    async fn update_existing_registration_if_unchanged(
        &self,
        mutation: &ProxyNodeRegistrationMutation,
        existing: &StoredProxyNode,
        replacement_proxy_metadata: Option<&serde_json::Value>,
        now: u64,
    ) -> Result<bool, DataLayerError> {
        let hardware_info =
            optional_json_to_string(&mutation.hardware_info, "proxy_nodes.hardware_info")?;
        let replacement_proxy_metadata_json = optional_json_to_string(
            &replacement_proxy_metadata.cloned(),
            "proxy_nodes.proxy_metadata",
        )?;
        let expected_proxy_metadata =
            optional_json_to_string(&existing.proxy_metadata, "proxy_nodes.proxy_metadata")?;
        let result = sqlx::query(UPDATE_PROXY_NODE_REGISTRATION_SQL)
            .bind(&mutation.name)
            .bind(&mutation.ip)
            .bind(mutation.port)
            .bind(mutation.region.as_deref())
            .bind(mutation.registered_by.as_deref())
            .bind(now as i64)
            .bind(mutation.heartbeat_interval)
            .bind(mutation.active_connections)
            .bind(mutation.total_requests)
            .bind(mutation.avg_latency_ms)
            .bind(hardware_info)
            .bind(mutation.estimated_max_concurrency)
            .bind(mutation.tunnel_mode)
            .bind(replacement_proxy_metadata_json)
            .bind(now as i64)
            .bind(&existing.id)
            .bind(&existing.tunnel_generation)
            .bind(&existing.ip)
            .bind(existing.port)
            .bind(expected_proxy_metadata.as_deref())
            .bind(expected_proxy_metadata.as_deref())
            .bind(expected_proxy_metadata.as_deref())
            .execute(&self.pool)
            .await
            .map_sql_err()?;
        if result.rows_affected() != 0 {
            return Ok(true);
        }

        let Some(current) = self.find_proxy_node(&existing.id).await? else {
            return Ok(false);
        };
        Ok(proxy_node_registration_matches(
            &current,
            mutation,
            existing,
            replacement_proxy_metadata,
            now,
        ))
    }

    async fn find_duplicate_proxy_node(
        &self,
        ip: &str,
        port: i32,
        excluding_node_id: Option<&str>,
    ) -> Result<Option<StoredProxyNode>, DataLayerError> {
        let row = if let Some(excluding_node_id) = excluding_node_id {
            sqlx::query(&format!(
                "{PROXY_NODE_COLUMNS} WHERE ip = ? AND port = ? AND id <> ? LIMIT 1"
            ))
            .bind(ip)
            .bind(port)
            .bind(excluding_node_id)
            .fetch_optional(&self.pool)
            .await
            .map_sql_err()?
        } else {
            sqlx::query(&format!(
                "{PROXY_NODE_COLUMNS} WHERE ip = ? AND port = ? LIMIT 1"
            ))
            .bind(ip)
            .bind(port)
            .fetch_optional(&self.pool)
            .await
            .map_sql_err()?
        };
        row.as_ref().map(map_proxy_node_row).transpose()
    }

    async fn find_registered_proxy_node_by_endpoint(
        &self,
        ip: &str,
        port: i32,
    ) -> Result<Option<StoredProxyNode>, DataLayerError> {
        let row = sqlx::query(&format!(
            "{PROXY_NODE_COLUMNS} WHERE BINARY ip = BINARY ? AND port = ? AND is_manual = 0 ORDER BY created_at ASC, id ASC LIMIT 1"
        ))
        .bind(ip)
        .bind(port)
        .fetch_optional(&self.pool)
        .await
        .map_sql_err()?;
        row.as_ref().map(map_proxy_node_row).transpose()
    }

    async fn insert_event(
        &self,
        node_id: &str,
        expected_tunnel_generation: Option<&str>,
        event_type: &str,
        detail: Option<&str>,
        event_metadata: Option<&serde_json::Value>,
        created_at_unix_secs: Option<u64>,
    ) -> Result<(), DataLayerError> {
        sqlx::query(
            r#"
INSERT INTO proxy_node_events (node_id, event_type, detail, event_metadata, created_at)
SELECT id, ?, ?, ?, ?
FROM proxy_nodes
WHERE id = ? AND (? IS NULL OR BINARY tunnel_generation = BINARY ?)
"#,
        )
        .bind(event_type)
        .bind(detail)
        .bind(optional_json_to_string(
            &event_metadata.cloned(),
            "proxy_node_events.event_metadata",
        )?)
        .bind(created_at_unix_secs.unwrap_or_else(current_unix_secs) as i64)
        .bind(node_id)
        .bind(expected_tunnel_generation)
        .bind(expected_tunnel_generation)
        .execute(&self.pool)
        .await
        .map_sql_err()?;
        Ok(())
    }

    async fn upsert_metrics_bucket(
        &self,
        table: &str,
        node_id: &str,
        expected_tunnel_generation: Option<&str>,
        bucket_start: u64,
        sample: &TunnelMetricsSample,
    ) -> Result<(), DataLayerError> {
        sqlx::query(&format!(
            r#"
INSERT INTO {table} (
  node_id,
  bucket_start_unix_secs,
  samples,
  uptime_samples,
  active_connections_sum,
  active_connections_max,
  heartbeat_rtt_ms_sum,
  heartbeat_rtt_ms_max,
  connect_errors_delta,
  disconnects_delta,
  error_events_delta,
  ws_in_bytes_delta,
  ws_out_bytes_delta,
  ws_in_frames_delta,
  ws_out_frames_delta
)
SELECT ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?
FROM proxy_nodes
WHERE id = ? AND (? IS NULL OR BINARY tunnel_generation = BINARY ?)
ON DUPLICATE KEY UPDATE
  samples = samples + VALUES(samples),
  uptime_samples = uptime_samples + VALUES(uptime_samples),
  active_connections_sum = active_connections_sum + VALUES(active_connections_sum),
  active_connections_max = GREATEST(active_connections_max, VALUES(active_connections_max)),
  heartbeat_rtt_ms_sum = heartbeat_rtt_ms_sum + VALUES(heartbeat_rtt_ms_sum),
  heartbeat_rtt_ms_max = GREATEST(heartbeat_rtt_ms_max, VALUES(heartbeat_rtt_ms_max)),
  connect_errors_delta = connect_errors_delta + VALUES(connect_errors_delta),
  disconnects_delta = disconnects_delta + VALUES(disconnects_delta),
  error_events_delta = error_events_delta + VALUES(error_events_delta),
  ws_in_bytes_delta = ws_in_bytes_delta + VALUES(ws_in_bytes_delta),
  ws_out_bytes_delta = ws_out_bytes_delta + VALUES(ws_out_bytes_delta),
  ws_in_frames_delta = ws_in_frames_delta + VALUES(ws_in_frames_delta),
  ws_out_frames_delta = ws_out_frames_delta + VALUES(ws_out_frames_delta)
"#
        ))
        .bind(node_id)
        .bind(i64::try_from(bucket_start).unwrap_or(i64::MAX))
        .bind(sample.samples)
        .bind(sample.uptime_samples)
        .bind(sample.active_connections_sum)
        .bind(sample.active_connections_max)
        .bind(sample.heartbeat_rtt_ms_sum)
        .bind(sample.heartbeat_rtt_ms_max)
        .bind(sample.connect_errors_delta)
        .bind(sample.disconnects_delta)
        .bind(sample.error_events_delta)
        .bind(sample.ws_in_bytes_delta)
        .bind(sample.ws_out_bytes_delta)
        .bind(sample.ws_in_frames_delta)
        .bind(sample.ws_out_frames_delta)
        .bind(node_id)
        .bind(expected_tunnel_generation)
        .bind(expected_tunnel_generation)
        .execute(&self.pool)
        .await
        .map_sql_err()?;
        Ok(())
    }

    fn normalize_remote_config(
        mutation: &ProxyNodeRemoteConfigMutation,
        existing: Option<&serde_json::Value>,
    ) -> Option<serde_json::Value> {
        let mut config = match existing {
            Some(serde_json::Value::Object(map)) => map.clone(),
            _ => serde_json::Map::new(),
        };

        if let Some(node_name) = mutation.node_name.as_ref() {
            config.insert(
                "node_name".to_string(),
                serde_json::Value::String(node_name.clone()),
            );
        }
        if let Some(allowed_ports) = mutation.allowed_ports.as_ref() {
            config.insert(
                "allowed_ports".to_string(),
                serde_json::json!(allowed_ports),
            );
        }
        if let Some(log_level) = mutation.log_level.as_ref() {
            config.insert(
                "log_level".to_string(),
                serde_json::Value::String(log_level.clone()),
            );
        }
        if let Some(heartbeat_interval) = mutation.heartbeat_interval {
            config.insert(
                "heartbeat_interval".to_string(),
                serde_json::json!(heartbeat_interval),
            );
        }
        if let Some(scheduling_state) = mutation.scheduling_state.as_ref() {
            match scheduling_state {
                Some(state) => {
                    config.insert(
                        "scheduling_state".to_string(),
                        serde_json::Value::String(state.clone()),
                    );
                }
                None => {
                    config.remove("scheduling_state");
                }
            }
        }
        if let Some(upgrade_to) = mutation.upgrade_to.as_ref() {
            match upgrade_to {
                Some(version) => {
                    config.insert(
                        "upgrade_to".to_string(),
                        serde_json::Value::String(version.clone()),
                    );
                }
                None => {
                    config.remove("upgrade_to");
                }
            }
        }

        (!config.is_empty()).then_some(serde_json::Value::Object(config))
    }
}

const PROXY_NODE_COLUMNS: &str = r#"
SELECT
  id,
  tunnel_generation,
  name,
  ip,
  port,
  region,
  is_manual,
  proxy_url,
  proxy_username,
  proxy_password,
  status,
  registered_by,
  last_heartbeat_at AS last_heartbeat_at_unix_secs,
  heartbeat_interval,
  active_connections,
  total_requests,
  avg_latency_ms,
  failed_requests,
  dns_failures,
  stream_errors,
  proxy_metadata,
  hardware_info,
  estimated_max_concurrency,
  tunnel_mode,
  tunnel_connected,
  tunnel_connected_at AS tunnel_connected_at_unix_secs,
  remote_config,
  config_version,
  created_at AS created_at_unix_ms,
  updated_at AS updated_at_unix_secs
FROM proxy_nodes
"#;

const APPLY_HEARTBEAT_SQL: &str = r#"
UPDATE proxy_nodes
SET last_heartbeat_at = ?,
    tunnel_connected_at = CASE
      WHEN status <> 'online' OR tunnel_connected = 0 THEN ?
      ELSE tunnel_connected_at
    END,
    updated_at = CASE
      WHEN status <> 'online' OR tunnel_connected = 0 THEN ?
      ELSE updated_at
    END,
    status = 'online',
    tunnel_connected = 1,
    heartbeat_interval = COALESCE(?, heartbeat_interval),
    active_connections = COALESCE(?, active_connections),
    avg_latency_ms = COALESCE(?, avg_latency_ms),
    total_requests = total_requests + GREATEST(COALESCE(?, 0), 0),
    failed_requests = failed_requests + GREATEST(COALESCE(?, 0), 0),
    dns_failures = dns_failures + GREATEST(COALESCE(?, 0), 0),
    stream_errors = stream_errors + GREATEST(COALESCE(?, 0), 0)
WHERE id = ?
  AND tunnel_mode = 1
  AND BINARY tunnel_generation = BINARY ?
"#;

const CAS_HEARTBEAT_PROXY_METADATA_SQL: &str = r#"
UPDATE proxy_nodes
SET proxy_metadata = ?, updated_at = ?
WHERE id = ? AND BINARY tunnel_generation = BINARY ?
  AND (
    (proxy_metadata IS NULL AND ? IS NULL)
    OR (
      proxy_metadata IS NOT NULL AND ? IS NOT NULL
      AND JSON_VALID(proxy_metadata) = 1
      AND CAST(proxy_metadata AS JSON) = CAST(? AS JSON)
    )
  )
"#;

const UPDATE_TUNNEL_STATUS_SQL: &str = r#"
UPDATE proxy_nodes
SET tunnel_connected = ?,
    active_connections = CASE WHEN ? THEN active_connections ELSE 0 END,
    tunnel_connected_at = ?,
    status = CASE WHEN ? THEN 'online' ELSE 'offline' END,
    updated_at = ?
WHERE id = ?
  AND BINARY tunnel_generation = BINARY ?
  AND (tunnel_connected_at IS NULL OR tunnel_connected_at <= ?)
"#;

const UPDATE_MANUAL_PROXY_NODE_SQL: &str = r#"
UPDATE proxy_nodes
SET name = COALESCE(?, name),
    ip = COALESCE(?, ip),
    port = COALESCE(?, port),
    region = COALESCE(?, region),
    proxy_url = COALESCE(?, proxy_url),
    proxy_username = COALESCE(?, proxy_username),
    proxy_password = COALESCE(?, proxy_password),
    updated_at = ?
WHERE id = ? AND is_manual = 1
  AND BINARY tunnel_generation = BINARY ?
"#;

const UPDATE_PROXY_NODE_REGISTRATION_SQL: &str = r#"
UPDATE proxy_nodes
SET name = ?, ip = ?, port = ?, region = ?, registered_by = ?,
    last_heartbeat_at = ?, heartbeat_interval = ?,
    active_connections = COALESCE(?, active_connections),
    total_requests = COALESCE(?, total_requests),
    avg_latency_ms = COALESCE(?, avg_latency_ms),
    hardware_info = COALESCE(?, hardware_info),
    estimated_max_concurrency = COALESCE(?, estimated_max_concurrency),
    tunnel_mode = ?, proxy_metadata = COALESCE(?, proxy_metadata), updated_at = ?
WHERE BINARY id = BINARY ? AND BINARY tunnel_generation = BINARY ?
  AND is_manual = 0 AND BINARY ip = BINARY ? AND port = ?
  AND (
    (proxy_metadata IS NULL AND ? IS NULL)
    OR (
      proxy_metadata IS NOT NULL AND ? IS NOT NULL
      AND JSON_VALID(proxy_metadata) = 1
      AND CAST(proxy_metadata AS JSON) = CAST(? AS JSON)
    )
  )
"#;

const UPDATE_PROXY_NODE_REMOTE_CONFIG_SQL: &str = r#"
UPDATE proxy_nodes
SET name = COALESCE(?, name), remote_config = ?,
    config_version = config_version + 1, updated_at = ?
WHERE id = ? AND BINARY tunnel_generation = BINARY ? AND config_version = ?
  AND is_manual = 0
"#;

const RECORD_PROXY_NODE_TRAFFIC_SQL: &str = r#"
UPDATE proxy_nodes
SET total_requests = total_requests + GREATEST(?, 0),
    failed_requests = failed_requests + GREATEST(?, 0),
    dns_failures = dns_failures + GREATEST(?, 0),
    stream_errors = stream_errors + GREATEST(?, 0),
    updated_at = ?
WHERE id = ? AND is_manual = 1
  AND BINARY tunnel_generation = BINARY ?
"#;

const INCREMENT_MANUAL_PROXY_NODE_REQUESTS_SQL: &str = r#"
UPDATE proxy_nodes
SET total_requests = total_requests + GREATEST(?, 0),
    failed_requests = failed_requests + GREATEST(?, 0),
    avg_latency_ms = COALESCE(?, avg_latency_ms),
    updated_at = ?
WHERE id = ? AND is_manual = 1
  AND BINARY tunnel_generation = BINARY ?
"#;

const UNREGISTER_PROXY_NODE_SQL: &str = r#"
UPDATE proxy_nodes
SET status = 'offline', tunnel_connected = 0, active_connections = 0,
    tunnel_connected_at = ?, updated_at = ?
WHERE id = ?
  AND BINARY tunnel_generation = BINARY ?
"#;

// Run after the parent delete commits so delete never waits on an outbox row
// already claimed by the flusher (which acquires locks in the opposite order).
const RETIRE_PROXY_NODE_PENDING_COUNTERS_SQL: &str = r#"
DELETE FROM usage_counter_deltas
WHERE kind = 'proxy_node'
  AND target_id = ?
  AND BINARY target_tunnel_generation = BINARY ?
  AND processed_at IS NULL
"#;

#[async_trait]
impl ProxyNodeReadRepository for MysqlProxyNodeReadRepository {
    async fn list_proxy_nodes(&self) -> Result<Vec<StoredProxyNode>, DataLayerError> {
        let rows = sqlx::query(&format!("{PROXY_NODE_COLUMNS} ORDER BY name ASC, id ASC"))
            .fetch_all(&self.pool)
            .await
            .map_sql_err()?;
        rows.iter().map(map_proxy_node_row).collect()
    }

    async fn find_proxy_node(
        &self,
        node_id: &str,
    ) -> Result<Option<StoredProxyNode>, DataLayerError> {
        let row = sqlx::query(&format!("{PROXY_NODE_COLUMNS} WHERE id = ? LIMIT 1"))
            .bind(node_id)
            .fetch_optional(&self.pool)
            .await
            .map_sql_err()?;
        row.as_ref().map(map_proxy_node_row).transpose()
    }

    async fn list_proxy_node_events(
        &self,
        node_id: &str,
        limit: usize,
    ) -> Result<Vec<StoredProxyNodeEvent>, DataLayerError> {
        let rows = sqlx::query(
            r#"
SELECT
  id,
  node_id,
  event_type,
  detail,
  event_metadata,
  created_at AS created_at_unix_ms
FROM proxy_node_events
WHERE node_id = ?
ORDER BY created_at DESC, id DESC
LIMIT ?
"#,
        )
        .bind(node_id)
        .bind(i64::try_from(limit).unwrap_or(i64::MAX))
        .fetch_all(&self.pool)
        .await
        .map_sql_err()?;
        rows.iter().map(map_proxy_node_event_row).collect()
    }

    async fn list_proxy_node_events_filtered(
        &self,
        node_id: &str,
        query: &ProxyNodeEventQuery,
    ) -> Result<Vec<StoredProxyNodeEvent>, DataLayerError> {
        let rows = sqlx::query(
            r#"
SELECT
  id,
  node_id,
  event_type,
  detail,
  event_metadata,
  created_at AS created_at_unix_ms
FROM proxy_node_events
WHERE node_id = ?
  AND (? IS NULL OR created_at >= ?)
  AND (? IS NULL OR created_at <= ?)
  AND (? IS NULL OR LOWER(event_type) = LOWER(?))
ORDER BY created_at DESC, id DESC
LIMIT ?
"#,
        )
        .bind(node_id)
        .bind(
            query
                .from_unix_secs
                .map(|v| i64::try_from(v).unwrap_or(i64::MAX)),
        )
        .bind(
            query
                .from_unix_secs
                .map(|v| i64::try_from(v).unwrap_or(i64::MAX)),
        )
        .bind(
            query
                .to_unix_secs
                .map(|v| i64::try_from(v).unwrap_or(i64::MAX)),
        )
        .bind(
            query
                .to_unix_secs
                .map(|v| i64::try_from(v).unwrap_or(i64::MAX)),
        )
        .bind(query.event_type.as_deref())
        .bind(query.event_type.as_deref())
        .bind(i64::try_from(query.limit).unwrap_or(i64::MAX))
        .fetch_all(&self.pool)
        .await
        .map_sql_err()?;
        rows.iter().map(map_proxy_node_event_row).collect()
    }

    async fn list_proxy_node_metrics(
        &self,
        node_id: &str,
        step: ProxyNodeMetricsStep,
        from_unix_secs: u64,
        to_unix_secs: u64,
        limit: usize,
    ) -> Result<Vec<StoredProxyNodeMetricsBucket>, DataLayerError> {
        let table = match step {
            ProxyNodeMetricsStep::OneMinute => "proxy_node_metrics_1m",
            ProxyNodeMetricsStep::OneHour => "proxy_node_metrics_1h",
        };
        let rows = sqlx::query(&format!(
            r#"
SELECT
  node_id,
  bucket_start_unix_secs,
  samples,
  uptime_samples,
  active_connections_sum,
  active_connections_max,
  heartbeat_rtt_ms_sum,
  heartbeat_rtt_ms_max,
  connect_errors_delta,
  disconnects_delta,
  error_events_delta,
  ws_in_bytes_delta,
  ws_out_bytes_delta,
  ws_in_frames_delta,
  ws_out_frames_delta
FROM {table}
WHERE node_id = ?
  AND bucket_start_unix_secs >= ?
  AND bucket_start_unix_secs <= ?
ORDER BY bucket_start_unix_secs ASC
LIMIT ?
"#
        ))
        .bind(node_id)
        .bind(i64::try_from(from_unix_secs).unwrap_or(i64::MAX))
        .bind(i64::try_from(to_unix_secs).unwrap_or(i64::MAX))
        .bind(i64::try_from(limit).unwrap_or(i64::MAX))
        .fetch_all(&self.pool)
        .await
        .map_sql_err()?;
        rows.iter().map(map_proxy_node_metric_row).collect()
    }

    async fn list_proxy_fleet_metrics(
        &self,
        step: ProxyNodeMetricsStep,
        from_unix_secs: u64,
        to_unix_secs: u64,
        limit: usize,
    ) -> Result<Vec<StoredProxyFleetMetricsBucket>, DataLayerError> {
        let table = match step {
            ProxyNodeMetricsStep::OneMinute => "proxy_node_metrics_1m",
            ProxyNodeMetricsStep::OneHour => "proxy_node_metrics_1h",
        };
        let rows = sqlx::query(&format!(
            r#"
SELECT
  bucket_start_unix_secs,
  SUM(samples) AS samples,
  SUM(uptime_samples) AS uptime_samples,
  SUM(active_connections_sum) AS active_connections_sum,
  MAX(active_connections_max) AS active_connections_max,
  SUM(heartbeat_rtt_ms_sum) AS heartbeat_rtt_ms_sum,
  MAX(heartbeat_rtt_ms_max) AS heartbeat_rtt_ms_max,
  SUM(connect_errors_delta) AS connect_errors_delta,
  SUM(disconnects_delta) AS disconnects_delta,
  SUM(error_events_delta) AS error_events_delta,
  SUM(ws_in_bytes_delta) AS ws_in_bytes_delta,
  SUM(ws_out_bytes_delta) AS ws_out_bytes_delta,
  SUM(ws_in_frames_delta) AS ws_in_frames_delta,
  SUM(ws_out_frames_delta) AS ws_out_frames_delta
FROM {table}
WHERE bucket_start_unix_secs >= ?
  AND bucket_start_unix_secs <= ?
GROUP BY bucket_start_unix_secs
ORDER BY bucket_start_unix_secs ASC
LIMIT ?
"#
        ))
        .bind(i64::try_from(from_unix_secs).unwrap_or(i64::MAX))
        .bind(i64::try_from(to_unix_secs).unwrap_or(i64::MAX))
        .bind(i64::try_from(limit).unwrap_or(i64::MAX))
        .fetch_all(&self.pool)
        .await
        .map_sql_err()?;
        rows.iter().map(map_proxy_fleet_metric_row).collect()
    }
}

#[async_trait]
impl ProxyNodeWriteRepository for MysqlProxyNodeReadRepository {
    async fn reset_stale_tunnel_statuses(&self) -> Result<usize, DataLayerError> {
        let now = current_unix_secs() as i64;
        let result = sqlx::query(
            r#"
UPDATE proxy_nodes
SET tunnel_connected = 0,
    status = 'offline',
    active_connections = 0,
    tunnel_connected_at = ?,
    updated_at = ?
WHERE is_manual = 0
  AND tunnel_connected = 1
"#,
        )
        .bind(now)
        .bind(now)
        .execute(&self.pool)
        .await
        .map_sql_err()?;
        Ok(result.rows_affected() as usize)
    }

    async fn compare_and_set_proxy_password(
        &self,
        node_id: &str,
        expected: &str,
        replacement: &str,
    ) -> Result<bool, DataLayerError> {
        let result = sqlx::query(
            r#"
UPDATE proxy_nodes
SET proxy_password = ?, updated_at = ?
WHERE id = ? AND BINARY proxy_password = BINARY ?
"#,
        )
        .bind(replacement)
        .bind(current_unix_secs() as i64)
        .bind(node_id)
        .bind(expected)
        .execute(&self.pool)
        .await
        .map_sql_err()?;
        Ok(result.rows_affected() == 1)
    }

    async fn compare_and_set_proxy_metadata(
        &self,
        node_id: &str,
        expected: &serde_json::Value,
        replacement: &serde_json::Value,
    ) -> Result<bool, DataLayerError> {
        let expected = serde_json::to_string(expected).map_err(|err| {
            DataLayerError::InvalidInput(format!("proxy_nodes.proxy_metadata is invalid: {err}"))
        })?;
        let replacement = serde_json::to_string(replacement).map_err(|err| {
            DataLayerError::InvalidInput(format!("proxy_nodes.proxy_metadata is invalid: {err}"))
        })?;
        let result = sqlx::query(
            r#"
UPDATE proxy_nodes
SET proxy_metadata = ?, updated_at = ?
WHERE id = ?
  AND proxy_metadata IS NOT NULL
  AND JSON_VALID(proxy_metadata) = 1
  AND CAST(proxy_metadata AS JSON) = CAST(? AS JSON)
"#,
        )
        .bind(replacement)
        .bind(current_unix_secs() as i64)
        .bind(node_id)
        .bind(&expected)
        .execute(&self.pool)
        .await
        .map_sql_err()?;
        Ok(result.rows_affected() == 1)
    }

    async fn create_manual_node(
        &self,
        mutation: &ProxyNodeManualCreateMutation,
    ) -> Result<StoredProxyNode, DataLayerError> {
        let node_id = requested_proxy_node_id(mutation.node_id.as_deref())?
            .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
        if let Some(existing) = self.find_proxy_node(&node_id).await? {
            return Err(proxy_node_id_in_use_error(&existing));
        }
        let now = Some(current_unix_secs());
        let node = StoredProxyNode::new(
            node_id,
            mutation.name.clone(),
            mutation.ip.clone(),
            mutation.port,
            true,
            "online".to_string(),
            0,
            0,
            0,
            0,
            0,
            0,
            false,
            false,
            0,
        )?
        .with_manual_proxy_fields(
            Some(mutation.proxy_url.clone()),
            mutation.proxy_username.clone(),
            mutation.proxy_password.clone(),
        )
        .with_runtime_fields(
            mutation.region.clone(),
            mutation.registered_by.clone(),
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            now,
            now,
        );

        if let Err(error) = self.insert_node(&node).await {
            if let Some(duplicate) = self
                .find_duplicate_proxy_node(&mutation.ip, mutation.port, None)
                .await?
            {
                return Err(duplicate_proxy_node_error(&duplicate));
            }
            if let Some(owner) = self.find_proxy_node(&node.id).await? {
                return Err(proxy_node_id_in_use_error(&owner));
            }
            return Err(error);
        }
        Ok(node)
    }

    async fn update_manual_node(
        &self,
        mutation: &ProxyNodeManualUpdateMutation,
    ) -> Result<Option<StoredProxyNode>, DataLayerError> {
        let Some(existing) = self.find_proxy_node(&mutation.node_id).await? else {
            return Ok(None);
        };
        if !existing.is_manual {
            return Err(DataLayerError::InvalidInput(
                "只能编辑手动添加的代理节点".to_string(),
            ));
        }

        let next_ip = mutation.ip.as_deref().unwrap_or(existing.ip.as_str());
        let next_port = mutation.port.unwrap_or(existing.port);
        let result = sqlx::query(UPDATE_MANUAL_PROXY_NODE_SQL)
            .bind(mutation.name.as_deref())
            .bind(mutation.ip.as_deref())
            .bind(mutation.port)
            .bind(mutation.region.as_deref())
            .bind(mutation.proxy_url.as_deref())
            .bind(mutation.proxy_username.as_deref())
            .bind(mutation.proxy_password.as_deref())
            .bind(current_unix_secs() as i64)
            .bind(&mutation.node_id)
            .bind(&existing.tunnel_generation)
            .execute(&self.pool)
            .await;
        let result = match result {
            Ok(result) => result,
            Err(error) => {
                if let Some(duplicate) = self
                    .find_duplicate_proxy_node(next_ip, next_port, Some(&mutation.node_id))
                    .await?
                {
                    return Err(duplicate_proxy_node_error(&duplicate));
                }
                return Err(DataLayerError::sql(error));
            }
        };
        if result.rows_affected() == 0 {
            return Ok(None);
        }
        self.find_proxy_node(&mutation.node_id).await
    }

    async fn register_node(
        &self,
        mutation: &ProxyNodeRegistrationMutation,
    ) -> Result<StoredProxyNode, DataLayerError> {
        let requested_id = requested_proxy_node_id(mutation.node_id.as_deref())?;
        let normalized_proxy_metadata = normalize_proxy_metadata(
            mutation.proxy_metadata.as_ref(),
            mutation.proxy_version.as_deref(),
        );
        let rotates_tunnel_security =
            proxy_metadata_has_explicit_tunnel_security(normalized_proxy_metadata.as_ref());

        let Some(initial_existing) = self
            .find_registered_proxy_node_by_endpoint(&mutation.ip, mutation.port)
            .await?
        else {
            let node_id = requested_id
                .clone()
                .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
            if let Some(existing) = self.find_proxy_node(&node_id).await? {
                return Err(proxy_node_id_in_use_error(&existing));
            }
            let now = Some(current_unix_secs());
            let node = StoredProxyNode::new(
                node_id,
                mutation.name.clone(),
                mutation.ip.clone(),
                mutation.port,
                false,
                "offline".to_string(),
                mutation.heartbeat_interval,
                mutation.active_connections.unwrap_or(0),
                mutation.total_requests.unwrap_or(0),
                0,
                0,
                0,
                mutation.tunnel_mode,
                false,
                0,
            )?
            .with_runtime_fields(
                mutation.region.clone(),
                mutation.registered_by.clone(),
                now,
                mutation.avg_latency_ms,
                merge_proxy_metadata_for_registration(None, normalized_proxy_metadata.clone()),
                mutation.hardware_info.clone(),
                mutation.estimated_max_concurrency,
                None,
                None,
                now,
                now,
            );
            if let Err(error) = self.insert_node(&node).await {
                if let Some(winner) = self
                    .find_duplicate_proxy_node(&mutation.ip, mutation.port, None)
                    .await?
                {
                    if winner.is_manual {
                        return Err(duplicate_proxy_node_error(&winner));
                    }
                    if let Some(requested_id) = requested_id.as_deref() {
                        if requested_id != winner.id {
                            return Err(proxy_node_registration_identity_error(
                                requested_id,
                                &winner.id,
                            ));
                        }
                    }
                    return Ok(winner);
                }
                if let Some(owner) = self.find_proxy_node(&node.id).await? {
                    return Err(proxy_node_id_in_use_error(&owner));
                }
                return Err(error);
            }
            return Ok(node);
        };

        if let Some(requested_id) = requested_id.as_deref() {
            if requested_id != initial_existing.id {
                return Err(proxy_node_registration_identity_error(
                    requested_id,
                    &initial_existing.id,
                ));
            }
        }

        let pinned_id = initial_existing.id.clone();
        let pinned_generation = initial_existing.tunnel_generation.clone();
        let mut existing = initial_existing;
        for attempt in 0..PROXY_NODE_REGISTRATION_CAS_RETRIES {
            if attempt != 0 {
                existing = self
                    .find_registered_proxy_node_by_endpoint(&mutation.ip, mutation.port)
                    .await?
                    .ok_or_else(proxy_node_registration_changed_error)?;
            }
            if existing.id != pinned_id || existing.tunnel_generation != pinned_generation {
                return Err(proxy_node_registration_changed_error());
            }

            let replacement_proxy_metadata = merge_proxy_metadata_for_registration(
                existing.proxy_metadata.as_ref(),
                normalized_proxy_metadata.clone(),
            );
            let now = current_unix_secs();
            if self
                .update_existing_registration_if_unchanged(
                    mutation,
                    &existing,
                    replacement_proxy_metadata.as_ref(),
                    now,
                )
                .await?
            {
                return self
                    .find_proxy_node(&pinned_id)
                    .await?
                    .filter(|current| current.tunnel_generation == pinned_generation)
                    .ok_or_else(proxy_node_registration_changed_error);
            }
            if rotates_tunnel_security {
                return Err(DataLayerError::UnexpectedValue(
                    "proxy node changed during explicit tunnel security rotation".to_string(),
                ));
            }
        }

        Err(DataLayerError::UnexpectedValue(
            "proxy node registration changed during every CAS retry".to_string(),
        ))
    }

    async fn apply_heartbeat(
        &self,
        mutation: &ProxyNodeHeartbeatMutation,
    ) -> Result<Option<StoredProxyNode>, DataLayerError> {
        let Some(existing) = self.find_proxy_node(&mutation.node_id).await? else {
            return Ok(None);
        };
        if mutation
            .expected_tunnel_generation
            .as_deref()
            .is_some_and(|expected| expected != existing.tunnel_generation)
        {
            return Ok(None);
        }
        if !existing.tunnel_mode {
            return Err(DataLayerError::InvalidInput(
                "non-tunnel mode is no longer supported, please upgrade aether-tunnel to use tunnel mode"
                    .to_string(),
            ));
        }

        let tunnel_generation = existing.tunnel_generation.clone();
        let now_unix_secs = current_unix_secs();
        let now = i64::try_from(now_unix_secs).unwrap_or(i64::MAX);
        let has_proxy_metadata_update = normalize_heartbeat_proxy_metadata(
            None,
            mutation.proxy_metadata.as_ref(),
            mutation.proxy_version.as_deref(),
        )
        .is_some();

        let result = sqlx::query(APPLY_HEARTBEAT_SQL)
            .bind(now)
            .bind(now)
            .bind(now)
            .bind(mutation.heartbeat_interval)
            .bind(mutation.active_connections)
            .bind(mutation.avg_latency_ms)
            .bind(mutation.total_requests_delta)
            .bind(mutation.failed_requests_delta)
            .bind(mutation.dns_failures_delta)
            .bind(mutation.stream_errors_delta)
            .bind(&mutation.node_id)
            .bind(&tunnel_generation)
            .execute(&self.pool)
            .await
            .map_sql_err()?;
        if result.rows_affected() == 0 {
            return Ok(None);
        }

        let mut updated = None;
        let mut tunnel_metrics_sample = None;
        if has_proxy_metadata_update {
            for _ in 0..8 {
                let Some(current) = self.find_proxy_node(&mutation.node_id).await? else {
                    return Ok(None);
                };
                if current.tunnel_generation != tunnel_generation {
                    return Ok(None);
                }
                let Some(replacement) = normalize_heartbeat_proxy_metadata(
                    current.proxy_metadata.as_ref(),
                    mutation.proxy_metadata.as_ref(),
                    mutation.proxy_version.as_deref(),
                ) else {
                    break;
                };
                if current.proxy_metadata.as_ref() == Some(&replacement) {
                    tunnel_metrics_sample = build_tunnel_metrics_sample(
                        current.proxy_metadata.as_ref(),
                        Some(&replacement),
                        current.active_connections,
                        current.tunnel_connected,
                    );
                    updated = Some(current);
                    break;
                }

                let expected =
                    optional_json_to_string(&current.proxy_metadata, "proxy_nodes.proxy_metadata")?;
                let replacement_json = serde_json::to_string(&replacement).map_err(|error| {
                    DataLayerError::UnexpectedValue(format!(
                        "proxy_nodes.proxy_metadata contains unserializable JSON: {error}"
                    ))
                })?;
                let result = sqlx::query(CAS_HEARTBEAT_PROXY_METADATA_SQL)
                    .bind(replacement_json)
                    .bind(now)
                    .bind(&mutation.node_id)
                    .bind(&tunnel_generation)
                    .bind(expected.as_deref())
                    .bind(expected.as_deref())
                    .bind(expected.as_deref())
                    .execute(&self.pool)
                    .await
                    .map_sql_err()?;
                if result.rows_affected() == 0 {
                    continue;
                }
                let Some(after_cas) = self.find_proxy_node(&mutation.node_id).await? else {
                    return Ok(None);
                };
                if after_cas.tunnel_generation != tunnel_generation {
                    return Ok(None);
                }
                tunnel_metrics_sample = build_tunnel_metrics_sample(
                    current.proxy_metadata.as_ref(),
                    after_cas.proxy_metadata.as_ref(),
                    after_cas.active_connections,
                    after_cas.tunnel_connected,
                );
                updated = Some(after_cas);
                break;
            }
        }
        let updated = if let Some(updated) = updated {
            updated
        } else {
            let Some(current) = self.find_proxy_node(&mutation.node_id).await? else {
                return Ok(None);
            };
            if current.tunnel_generation != tunnel_generation {
                return Ok(None);
            }
            current
        };

        if let Some(sample) = tunnel_metrics_sample.as_ref() {
            self.upsert_metrics_bucket(
                "proxy_node_metrics_1m",
                &updated.id,
                Some(tunnel_generation.as_str()),
                bucket_start_unix_secs(now_unix_secs, ProxyNodeMetricsStep::OneMinute),
                sample,
            )
            .await?;
            self.upsert_metrics_bucket(
                "proxy_node_metrics_1h",
                &updated.id,
                Some(tunnel_generation.as_str()),
                bucket_start_unix_secs(now_unix_secs, ProxyNodeMetricsStep::OneHour),
                sample,
            )
            .await?;

            for error in &sample.recent_error_events {
                log_reported_tunnel_error_event(&updated.id, error, now_unix_secs);
                let detail = build_tunnel_error_event_detail(error);
                let event_metadata = serde_json::json!({
                    "source": "heartbeat",
                    "category": error.category,
                    "message": error.message,
                    "severity": error.severity.as_deref(),
                    "component": error.component.as_deref(),
                    "summary": error.summary.as_deref(),
                    "operator_action": error.operator_action.as_deref(),
                    "timestamp_unix_secs": error.timestamp_unix_secs,
                    "timestamp_unix_ms": error.timestamp_unix_ms,
                });
                self.insert_event(
                    &updated.id,
                    Some(tunnel_generation.as_str()),
                    PROXY_NODE_EVENT_TYPE_TUNNEL_ERROR,
                    Some(detail.as_str()),
                    Some(&event_metadata),
                    Some(if error.timestamp_unix_secs == 0 {
                        now_unix_secs
                    } else {
                        error.timestamp_unix_secs
                    }),
                )
                .await?;
            }
        }
        if reconcile_remote_config_after_heartbeat(
            updated.remote_config.as_ref(),
            mutation.proxy_version.as_deref(),
        ) != updated.remote_config
        {
            return self
                .update_remote_config(&ProxyNodeRemoteConfigMutation {
                    node_id: mutation.node_id.clone(),
                    expected_tunnel_generation: Some(tunnel_generation),
                    node_name: None,
                    allowed_ports: None,
                    log_level: None,
                    heartbeat_interval: None,
                    scheduling_state: None,
                    upgrade_to: Some(None),
                })
                .await;
        }

        Ok(Some(updated))
    }

    async fn record_traffic(
        &self,
        mutation: &ProxyNodeTrafficMutation,
    ) -> Result<bool, DataLayerError> {
        let mut tx = self.pool.begin().await.map_sql_err()?;
        let row = sqlx::query(&format!(
            "{PROXY_NODE_COLUMNS} WHERE id = ? LIMIT 1 FOR UPDATE"
        ))
        .bind(&mutation.node_id)
        .fetch_optional(&mut *tx)
        .await
        .map_sql_err()?;
        let Some(row) = row else {
            tx.rollback().await.map_sql_err()?;
            return Ok(false);
        };
        let generation = map_proxy_node_row(&row)?.tunnel_generation;
        let Some(expected_generation) = mutation.expected_tunnel_generation.as_deref() else {
            tx.rollback().await.map_sql_err()?;
            return Ok(false);
        };
        if expected_generation != generation {
            tx.rollback().await.map_sql_err()?;
            return Ok(false);
        }
        let result = sqlx::query(RECORD_PROXY_NODE_TRAFFIC_SQL)
            .bind(mutation.total_requests_delta)
            .bind(mutation.failed_requests_delta)
            .bind(mutation.dns_failures_delta)
            .bind(mutation.stream_errors_delta)
            .bind(current_unix_secs() as i64)
            .bind(&mutation.node_id)
            .bind(expected_generation)
            .execute(&mut *tx)
            .await
            .map_sql_err()?;
        let applied = result.rows_affected() > 0;
        tx.commit().await.map_sql_err()?;
        Ok(applied)
    }

    async fn update_tunnel_status(
        &self,
        mutation: &ProxyNodeTunnelStatusMutation,
    ) -> Result<Option<StoredProxyNode>, DataLayerError> {
        let Some(node) = self.find_proxy_node(&mutation.node_id).await? else {
            return Ok(None);
        };
        if mutation
            .expected_tunnel_generation
            .as_deref()
            .is_some_and(|expected| expected != node.tunnel_generation)
        {
            return Ok(None);
        }

        let event_time = mutation
            .observed_at_unix_secs
            .unwrap_or_else(current_unix_secs);
        let event_type = if mutation.connected {
            "connected"
        } else {
            "disconnected"
        };
        let event_detail = mutation.detail.clone().unwrap_or_else(|| {
            format!(
                "[tunnel_node_status] conn_count={}",
                i32::max(mutation.conn_count, 0)
            )
        });

        let event_time_i64 = i64::try_from(event_time).unwrap_or(i64::MAX);
        let result = sqlx::query(UPDATE_TUNNEL_STATUS_SQL)
            .bind(mutation.connected)
            .bind(mutation.connected)
            .bind(event_time_i64)
            .bind(mutation.connected)
            .bind(event_time_i64)
            .bind(&mutation.node_id)
            .bind(&node.tunnel_generation)
            .bind(event_time_i64)
            .execute(&self.pool)
            .await
            .map_sql_err()?;
        let Some(current) = self.find_proxy_node(&mutation.node_id).await? else {
            return Ok(None);
        };
        if current.tunnel_generation != node.tunnel_generation {
            return Ok(None);
        }
        let stale = result.rows_affected() == 0
            && current
                .tunnel_connected_at_unix_secs
                .is_some_and(|last_transition| event_time < last_transition);
        let persisted_detail = if stale {
            format!("[stale_ignored] {event_detail}")
        } else {
            event_detail
        };
        self.insert_event(
            &mutation.node_id,
            Some(node.tunnel_generation.as_str()),
            event_type,
            Some(&persisted_detail),
            None,
            Some(if stale {
                current_unix_secs()
            } else {
                event_time
            }),
        )
        .await?;
        Ok(Some(current))
    }

    async fn unregister_node(
        &self,
        node_id: &str,
    ) -> Result<Option<StoredProxyNode>, DataLayerError> {
        let mut tx = self.pool.begin().await.map_sql_err()?;
        let row = sqlx::query(&format!(
            "{PROXY_NODE_COLUMNS} WHERE id = ? LIMIT 1 FOR UPDATE"
        ))
        .bind(node_id)
        .fetch_optional(&mut *tx)
        .await
        .map_sql_err()?;
        let Some(row) = row else {
            tx.rollback().await.map_sql_err()?;
            return Ok(None);
        };
        let generation = map_proxy_node_row(&row)?.tunnel_generation;
        let now = current_unix_secs() as i64;
        sqlx::query(UNREGISTER_PROXY_NODE_SQL)
            .bind(now)
            .bind(now)
            .bind(node_id)
            .bind(generation)
            .execute(&mut *tx)
            .await
            .map_sql_err()?;
        let updated = sqlx::query(&format!("{PROXY_NODE_COLUMNS} WHERE id = ? LIMIT 1"))
            .bind(node_id)
            .fetch_one(&mut *tx)
            .await
            .map_sql_err()
            .and_then(|row| map_proxy_node_row(&row))?;
        tx.commit().await.map_sql_err()?;
        Ok(Some(updated))
    }

    async fn delete_node(&self, node_id: &str) -> Result<Option<StoredProxyNode>, DataLayerError> {
        let mut tx = self.pool.begin().await.map_sql_err()?;
        let row = sqlx::query(&format!(
            "{PROXY_NODE_COLUMNS} WHERE id = ? LIMIT 1 FOR UPDATE"
        ))
        .bind(node_id)
        .fetch_optional(&mut *tx)
        .await
        .map_sql_err()?;
        let Some(row) = row else {
            tx.rollback().await.map_sql_err()?;
            return Ok(None);
        };
        let existing = map_proxy_node_row(&row)?;
        let generation = existing.tunnel_generation.as_str();

        // Child tables do not carry generation; retain the parent identity check
        // so cleanup cannot target a replacement row if schema constraints differ.
        sqlx::query(
            "DELETE FROM proxy_node_events WHERE node_id = ? AND EXISTS (SELECT 1 FROM proxy_nodes p WHERE p.id = ? AND BINARY p.tunnel_generation = BINARY ?)",
        )
                .bind(node_id)
        .bind(node_id)
        .bind(generation)
        .execute(&mut *tx)
                .await
                .map_sql_err()?;
        sqlx::query(
            "DELETE FROM proxy_node_metrics_1m WHERE node_id = ? AND EXISTS (SELECT 1 FROM proxy_nodes p WHERE p.id = ? AND BINARY p.tunnel_generation = BINARY ?)",
        )
                .bind(node_id)
        .bind(node_id)
        .bind(generation)
        .execute(&mut *tx)
                .await
                .map_sql_err()?;
        sqlx::query(
            "DELETE FROM proxy_node_metrics_1h WHERE node_id = ? AND EXISTS (SELECT 1 FROM proxy_nodes p WHERE p.id = ? AND BINARY p.tunnel_generation = BINARY ?)",
        )
                .bind(node_id)
        .bind(node_id)
        .bind(generation)
        .execute(&mut *tx)
                .await
                .map_sql_err()?;

        let deleted = sqlx::query(
            "DELETE FROM proxy_nodes WHERE id = ? AND BINARY tunnel_generation = BINARY ?",
        )
        .bind(node_id)
        .bind(generation)
        .execute(&mut *tx)
        .await
        .map_sql_err()?;
        if deleted.rows_affected() != 1 {
            tx.rollback().await.map_sql_err()?;
            return Ok(None);
        }
        tx.commit().await.map_sql_err()?;
        if let Err(error) = sqlx::query(RETIRE_PROXY_NODE_PENDING_COUNTERS_SQL)
            .bind(node_id)
            .bind(generation)
            .execute(&self.pool)
            .await
            .map_sql_err()
        {
            tracing::warn!(
                node_id = %node_id,
                tunnel_generation = %generation,
                error = ?error,
                "failed to retire deleted proxy node counter rows"
            );
        }
        Ok(Some(existing))
    }

    async fn update_remote_config(
        &self,
        mutation: &ProxyNodeRemoteConfigMutation,
    ) -> Result<Option<StoredProxyNode>, DataLayerError> {
        for _ in 0..8 {
            let Some(node) = self.find_proxy_node(&mutation.node_id).await? else {
                return Ok(None);
            };
            if mutation
                .expected_tunnel_generation
                .as_deref()
                .is_some_and(|expected| expected != node.tunnel_generation)
            {
                return Ok(None);
            }
            if node.is_manual {
                return Err(DataLayerError::InvalidInput(
                    "手动节点不支持远程配置下发".to_string(),
                ));
            }

            let remote_config =
                Self::normalize_remote_config(mutation, node.remote_config.as_ref());
            let remote_config =
                optional_json_to_string(&remote_config, "proxy_nodes.remote_config")?;
            let now = current_unix_secs() as i64;
            let result = sqlx::query(UPDATE_PROXY_NODE_REMOTE_CONFIG_SQL)
                .bind(mutation.node_name.as_deref())
                .bind(remote_config)
                .bind(now)
                .bind(&mutation.node_id)
                .bind(&node.tunnel_generation)
                .bind(node.config_version)
                .execute(&self.pool)
                .await
                .map_sql_err()?;
            if result.rows_affected() == 0 {
                continue;
            }

            let current = self.find_proxy_node(&mutation.node_id).await?;
            return Ok(
                current.filter(|current| current.tunnel_generation == node.tunnel_generation)
            );
        }

        Err(DataLayerError::UnexpectedValue(
            "proxy node remote config changed during every CAS retry".to_string(),
        ))
    }

    async fn increment_manual_node_requests(
        &self,
        node_id: &str,
        total_delta: i64,
        failed_delta: i64,
        latency_ms: Option<i64>,
    ) -> Result<(), DataLayerError> {
        let mut tx = self.pool.begin().await.map_sql_err()?;
        let row = sqlx::query(&format!(
            "{PROXY_NODE_COLUMNS} WHERE id = ? LIMIT 1 FOR UPDATE"
        ))
        .bind(node_id)
        .fetch_optional(&mut *tx)
        .await
        .map_sql_err()?;
        let Some(row) = row else {
            tx.rollback().await.map_sql_err()?;
            return Ok(());
        };
        let generation = map_proxy_node_row(&row)?.tunnel_generation;
        sqlx::query(INCREMENT_MANUAL_PROXY_NODE_REQUESTS_SQL)
            .bind(total_delta)
            .bind(failed_delta)
            .bind(latency_ms.map(|value| value as f64))
            .bind(current_unix_secs() as i64)
            .bind(node_id)
            .bind(generation)
            .execute(&mut *tx)
            .await
            .map_sql_err()?;
        tx.commit().await.map_sql_err()?;
        Ok(())
    }

    async fn cleanup_proxy_node_metrics(
        &self,
        retain_1m_from_unix_secs: u64,
        retain_1h_from_unix_secs: u64,
        delete_limit: usize,
    ) -> Result<ProxyNodeMetricsCleanupSummary, DataLayerError> {
        let delete_limit_i64 = i64::try_from(delete_limit.max(1)).unwrap_or(i64::MAX);
        let deleted_1m = sqlx::query(
            r#"
DELETE FROM proxy_node_metrics_1m
WHERE bucket_start_unix_secs < ?
ORDER BY bucket_start_unix_secs ASC
LIMIT ?
"#,
        )
        .bind(i64::try_from(retain_1m_from_unix_secs).unwrap_or(i64::MAX))
        .bind(delete_limit_i64)
        .execute(&self.pool)
        .await
        .map_sql_err()?
        .rows_affected() as usize;

        let deleted_1h = sqlx::query(
            r#"
DELETE FROM proxy_node_metrics_1h
WHERE bucket_start_unix_secs < ?
ORDER BY bucket_start_unix_secs ASC
LIMIT ?
"#,
        )
        .bind(i64::try_from(retain_1h_from_unix_secs).unwrap_or(i64::MAX))
        .bind(delete_limit_i64)
        .execute(&self.pool)
        .await
        .map_sql_err()?
        .rows_affected() as usize;

        Ok(ProxyNodeMetricsCleanupSummary {
            deleted_1m_rows: deleted_1m,
            deleted_1h_rows: deleted_1h,
        })
    }
}

fn optional_unix_secs(value: Option<i64>) -> Option<u64> {
    value.and_then(|value| u64::try_from(value).ok())
}

fn current_unix_secs() -> u64 {
    chrono::Utc::now().timestamp().max(0) as u64
}

fn optional_i64_from_u64(
    value: Option<u64>,
    field_name: &str,
) -> Result<Option<i64>, DataLayerError> {
    value
        .map(|value| {
            i64::try_from(value).map_err(|_| {
                DataLayerError::InvalidInput(format!("{field_name} exceeds i64: {value}"))
            })
        })
        .transpose()
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

fn duplicate_proxy_node_error(node: &StoredProxyNode) -> DataLayerError {
    DataLayerError::InvalidInput(format!(
        "已存在相同地址的代理节点: {} ({}:{})",
        node.name, node.ip, node.port
    ))
}

fn proxy_node_registration_matches(
    current: &StoredProxyNode,
    mutation: &ProxyNodeRegistrationMutation,
    expected: &StoredProxyNode,
    replacement_proxy_metadata: Option<&serde_json::Value>,
    now: u64,
) -> bool {
    current.id == expected.id
        && current.tunnel_generation == expected.tunnel_generation
        && !current.is_manual
        && current.name == mutation.name
        && current.ip == mutation.ip
        && current.port == mutation.port
        && current.region == mutation.region
        && current.registered_by == mutation.registered_by
        && current.last_heartbeat_at_unix_secs == Some(now)
        && current.heartbeat_interval == mutation.heartbeat_interval
        && mutation
            .active_connections
            .is_none_or(|value| current.active_connections == value)
        && mutation
            .total_requests
            .is_none_or(|value| current.total_requests == value)
        && mutation
            .avg_latency_ms
            .is_none_or(|value| current.avg_latency_ms == Some(value))
        && mutation
            .hardware_info
            .as_ref()
            .is_none_or(|value| current.hardware_info.as_ref() == Some(value))
        && mutation
            .estimated_max_concurrency
            .is_none_or(|value| current.estimated_max_concurrency == Some(value))
        && current.tunnel_mode == mutation.tunnel_mode
        && replacement_proxy_metadata
            .is_none_or(|value| current.proxy_metadata.as_ref() == Some(value))
        && current.updated_at_unix_secs == Some(now)
}

fn requested_proxy_node_id(value: Option<&str>) -> Result<Option<String>, DataLayerError> {
    let Some(value) = value else {
        return Ok(None);
    };
    if value.is_empty() || value.trim() != value {
        return Err(DataLayerError::InvalidInput(
            "proxy node id must be non-empty and unpadded".to_string(),
        ));
    }
    Ok(Some(value.to_string()))
}

fn proxy_node_registration_identity_error(requested_id: &str, existing_id: &str) -> DataLayerError {
    DataLayerError::InvalidInput(format!(
        "proxy node registration identity changed: requested {requested_id}, existing {existing_id}"
    ))
}

fn proxy_node_registration_changed_error() -> DataLayerError {
    DataLayerError::UnexpectedValue(
        "registered proxy node identity changed during registration".to_string(),
    )
}

fn proxy_node_id_in_use_error(node: &StoredProxyNode) -> DataLayerError {
    DataLayerError::InvalidInput(format!(
        "proxy node id is already in use: {} ({}:{})",
        node.id, node.ip, node.port
    ))
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

fn map_proxy_node_row(row: &MySqlRow) -> Result<StoredProxyNode, DataLayerError> {
    let tunnel_generation: String = row.try_get("tunnel_generation").map_sql_err()?;
    if tunnel_generation.trim().is_empty() {
        return Err(DataLayerError::UnexpectedValue(
            "proxy_nodes.tunnel_generation must not be empty".to_string(),
        ));
    }
    Ok(StoredProxyNode::new(
        row.try_get("id").map_sql_err()?,
        row.try_get("name").map_sql_err()?,
        row.try_get("ip").map_sql_err()?,
        row.try_get("port").map_sql_err()?,
        row.try_get("is_manual").map_sql_err()?,
        row.try_get("status").map_sql_err()?,
        row.try_get("heartbeat_interval").map_sql_err()?,
        row.try_get("active_connections").map_sql_err()?,
        row.try_get("total_requests").map_sql_err()?,
        row.try_get("failed_requests").map_sql_err()?,
        row.try_get("dns_failures").map_sql_err()?,
        row.try_get("stream_errors").map_sql_err()?,
        row.try_get("tunnel_mode").map_sql_err()?,
        row.try_get("tunnel_connected").map_sql_err()?,
        row.try_get("config_version").map_sql_err()?,
    )?
    .with_tunnel_generation(tunnel_generation)
    .with_manual_proxy_fields(
        row.try_get("proxy_url").map_sql_err()?,
        row.try_get("proxy_username").map_sql_err()?,
        row.try_get("proxy_password").map_sql_err()?,
    )
    .with_runtime_fields(
        row.try_get("region").map_sql_err()?,
        row.try_get("registered_by").map_sql_err()?,
        optional_unix_secs(row.try_get("last_heartbeat_at_unix_secs").map_sql_err()?),
        row.try_get("avg_latency_ms").map_sql_err()?,
        optional_json_from_string(
            row.try_get("proxy_metadata").map_sql_err()?,
            "proxy_nodes.proxy_metadata",
        )?,
        optional_json_from_string(
            row.try_get("hardware_info").map_sql_err()?,
            "proxy_nodes.hardware_info",
        )?,
        row.try_get("estimated_max_concurrency").map_sql_err()?,
        optional_unix_secs(row.try_get("tunnel_connected_at_unix_secs").map_sql_err()?),
        optional_json_from_string(
            row.try_get("remote_config").map_sql_err()?,
            "proxy_nodes.remote_config",
        )?,
        optional_unix_secs(row.try_get("created_at_unix_ms").map_sql_err()?),
        optional_unix_secs(row.try_get("updated_at_unix_secs").map_sql_err()?),
    ))
}

fn map_proxy_node_event_row(row: &MySqlRow) -> Result<StoredProxyNodeEvent, DataLayerError> {
    Ok(StoredProxyNodeEvent {
        id: row.try_get("id").map_sql_err()?,
        node_id: row.try_get("node_id").map_sql_err()?,
        event_type: row.try_get("event_type").map_sql_err()?,
        detail: row.try_get("detail").map_sql_err()?,
        event_metadata: optional_json_from_string(
            row.try_get("event_metadata").map_sql_err()?,
            "proxy_node_events.event_metadata",
        )?,
        created_at_unix_ms: optional_unix_secs(row.try_get("created_at_unix_ms").map_sql_err()?),
    })
}

fn map_proxy_node_metric_row(
    row: &MySqlRow,
) -> Result<StoredProxyNodeMetricsBucket, DataLayerError> {
    Ok(StoredProxyNodeMetricsBucket {
        node_id: row.try_get("node_id").map_sql_err()?,
        bucket_start_unix_secs: optional_unix_secs(
            row.try_get("bucket_start_unix_secs").map_sql_err()?,
        )
        .unwrap_or_default(),
        samples: row.try_get("samples").map_sql_err()?,
        uptime_samples: row.try_get("uptime_samples").map_sql_err()?,
        active_connections_sum: row.try_get("active_connections_sum").map_sql_err()?,
        active_connections_max: row.try_get("active_connections_max").map_sql_err()?,
        heartbeat_rtt_ms_sum: row.try_get("heartbeat_rtt_ms_sum").map_sql_err()?,
        heartbeat_rtt_ms_max: row.try_get("heartbeat_rtt_ms_max").map_sql_err()?,
        connect_errors_delta: row.try_get("connect_errors_delta").map_sql_err()?,
        disconnects_delta: row.try_get("disconnects_delta").map_sql_err()?,
        error_events_delta: row.try_get("error_events_delta").map_sql_err()?,
        ws_in_bytes_delta: row.try_get("ws_in_bytes_delta").map_sql_err()?,
        ws_out_bytes_delta: row.try_get("ws_out_bytes_delta").map_sql_err()?,
        ws_in_frames_delta: row.try_get("ws_in_frames_delta").map_sql_err()?,
        ws_out_frames_delta: row.try_get("ws_out_frames_delta").map_sql_err()?,
    })
}

fn map_proxy_fleet_metric_row(
    row: &MySqlRow,
) -> Result<StoredProxyFleetMetricsBucket, DataLayerError> {
    Ok(StoredProxyFleetMetricsBucket {
        bucket_start_unix_secs: optional_unix_secs(
            row.try_get("bucket_start_unix_secs").map_sql_err()?,
        )
        .unwrap_or_default(),
        samples: row.try_get("samples").map_sql_err()?,
        uptime_samples: row.try_get("uptime_samples").map_sql_err()?,
        active_connections_sum: row.try_get("active_connections_sum").map_sql_err()?,
        active_connections_max: row.try_get("active_connections_max").map_sql_err()?,
        heartbeat_rtt_ms_sum: row.try_get("heartbeat_rtt_ms_sum").map_sql_err()?,
        heartbeat_rtt_ms_max: row.try_get("heartbeat_rtt_ms_max").map_sql_err()?,
        connect_errors_delta: row.try_get("connect_errors_delta").map_sql_err()?,
        disconnects_delta: row.try_get("disconnects_delta").map_sql_err()?,
        error_events_delta: row.try_get("error_events_delta").map_sql_err()?,
        ws_in_bytes_delta: row.try_get("ws_in_bytes_delta").map_sql_err()?,
        ws_out_bytes_delta: row.try_get("ws_out_bytes_delta").map_sql_err()?,
        ws_in_frames_delta: row.try_get("ws_in_frames_delta").map_sql_err()?,
        ws_out_frames_delta: row.try_get("ws_out_frames_delta").map_sql_err()?,
    })
}

#[cfg(test)]
mod tests {
    use super::MysqlProxyNodeReadRepository;
    use crate::run_migrations;
    use aether_data_contracts::repository::proxy_nodes::{
        merge_proxy_metadata_for_registration, normalize_proxy_metadata,
        ProxyNodeManualCreateMutation, ProxyNodeReadRepository, ProxyNodeRegistrationMutation,
        ProxyNodeWriteRepository,
    };
    use serde_json::json;

    #[tokio::test]
    async fn repository_builds_from_lazy_pool() {
        let pool = sqlx::mysql::MySqlPoolOptions::new().connect_lazy_with(
            "mysql://user:pass@localhost:3306/aether"
                .parse()
                .expect("mysql options should parse"),
        );

        let _repository = MysqlProxyNodeReadRepository::new(pool);
    }

    #[test]
    fn proxy_node_mutation_sql_is_atomic_and_field_scoped() {
        assert!(super::APPLY_HEARTBEAT_SQL
            .contains("total_requests = total_requests + GREATEST(COALESCE(?, 0), 0)"));
        assert!(super::APPLY_HEARTBEAT_SQL
            .contains("failed_requests = failed_requests + GREATEST(COALESCE(?, 0), 0)"));
        assert!(!super::APPLY_HEARTBEAT_SQL.contains("remote_config ="));
        assert!(!super::APPLY_HEARTBEAT_SQL.contains("config_version ="));
        assert!(super::APPLY_HEARTBEAT_SQL.contains("BINARY tunnel_generation = BINARY ?"));

        assert!(super::UPDATE_TUNNEL_STATUS_SQL
            .contains("tunnel_connected_at IS NULL OR tunnel_connected_at <= ?"));
        assert!(super::UPDATE_TUNNEL_STATUS_SQL.contains("BINARY tunnel_generation = BINARY ?"));
        assert!(super::RECORD_PROXY_NODE_TRAFFIC_SQL
            .contains("total_requests = total_requests + GREATEST(?, 0)"));
        assert!(super::RECORD_PROXY_NODE_TRAFFIC_SQL.contains("is_manual = 1"));
        assert!(super::UPDATE_MANUAL_PROXY_NODE_SQL.contains("name = COALESCE(?, name)"));
        assert!(!super::UPDATE_MANUAL_PROXY_NODE_SQL.contains("remote_config"));
        assert!(super::UPDATE_PROXY_NODE_REGISTRATION_SQL
            .contains("BINARY id = BINARY ? AND BINARY tunnel_generation = BINARY ?"));
        assert!(super::UPDATE_PROXY_NODE_REGISTRATION_SQL
            .contains("is_manual = 0 AND BINARY ip = BINARY ? AND port = ?"));
        assert!(super::UPDATE_PROXY_NODE_REGISTRATION_SQL
            .contains("CAST(proxy_metadata AS JSON) = CAST(? AS JSON)"));
    }

    #[tokio::test]
    async fn mysql_concurrent_registration_keeps_one_endpoint_identity_when_url_is_set() {
        let Ok(database_url) = std::env::var("AETHER_TEST_MYSQL_URL") else {
            eprintln!("skipping MySQL registration race test: AETHER_TEST_MYSQL_URL is unset");
            return;
        };
        let pool = sqlx::mysql::MySqlPoolOptions::new()
            .max_connections(8)
            .connect(&database_url)
            .await
            .unwrap();
        run_migrations(&pool).await.unwrap();
        let repository = MysqlProxyNodeReadRepository::new(pool.clone());
        let endpoint = format!("registration-{}.test", uuid::Uuid::new_v4());
        let barrier = std::sync::Arc::new(tokio::sync::Barrier::new(8));
        let mut tasks = Vec::new();
        for _ in 0..8 {
            let repository = repository.clone();
            let ip = endpoint.clone();
            let barrier = barrier.clone();
            tasks.push(tokio::spawn(async move {
                barrier.wait().await;
                repository
                    .register_node(&ProxyNodeRegistrationMutation {
                        node_id: Some(uuid::Uuid::new_v4().to_string()),
                        name: "concurrent tunnel".to_string(),
                        ip,
                        port: 7042,
                        region: None,
                        heartbeat_interval: 30,
                        active_connections: None,
                        total_requests: None,
                        avg_latency_ms: None,
                        hardware_info: None,
                        estimated_max_concurrency: None,
                        proxy_metadata: None,
                        proxy_version: None,
                        registered_by: None,
                        tunnel_mode: true,
                    })
                    .await
            }));
        }
        let mut successes = 0;
        for task in tasks {
            match task.await.unwrap() {
                Ok(_) => successes += 1,
                Err(error) => assert!(
                    error.to_string().contains("identity changed"),
                    "unexpected concurrent registration error: {error}"
                ),
            }
        }
        assert_eq!(successes, 1);
        let count: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM proxy_nodes WHERE ip = ? AND port = ?")
                .bind(&endpoint)
                .bind(7042_i32)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(count, 1);
        sqlx::query("DELETE FROM proxy_nodes WHERE ip = ?")
            .bind(endpoint)
            .execute(&pool)
            .await
            .unwrap();
        pool.close().await;
    }

    #[tokio::test]
    async fn proxy_metadata_cas_distinguishes_duplicate_array_elements_when_url_is_set() {
        let Some(database_url) = std::env::var("AETHER_TEST_MYSQL_URL")
            .ok()
            .filter(|value| !value.trim().is_empty())
        else {
            eprintln!(
                "skipping mysql proxy metadata CAS test because AETHER_TEST_MYSQL_URL is unset"
            );
            return;
        };
        let pool = sqlx::mysql::MySqlPoolOptions::new()
            .max_connections(2)
            .connect(&database_url)
            .await
            .expect("mysql test pool should connect");
        run_migrations(&pool)
            .await
            .expect("mysql migrations should run");

        let suffix = uuid::Uuid::new_v4().simple().to_string();
        let repository = MysqlProxyNodeReadRepository::new(pool.clone());
        let node = repository
            .create_manual_node(&ProxyNodeManualCreateMutation {
                node_id: None,
                name: format!("metadata-cas-{suffix}"),
                ip: format!("metadata-cas-{suffix}"),
                port: 1,
                region: None,
                proxy_url: "http://127.0.0.1:1".to_string(),
                proxy_username: None,
                proxy_password: None,
                registered_by: None,
            })
            .await
            .expect("mysql proxy fixture should insert");
        let stored = json!({"nested": {"values": [1, 1]}});
        sqlx::query("UPDATE proxy_nodes SET proxy_metadata = ? WHERE id = ?")
            .bind(serde_json::to_string(&stored).expect("stored metadata should serialize"))
            .bind(&node.id)
            .execute(&pool)
            .await
            .expect("mysql proxy metadata fixture should update");

        let updated = repository
            .compare_and_set_proxy_metadata(
                &node.id,
                &json!({"nested": {"values": [1]}}),
                &json!({"replacement": true}),
            )
            .await
            .expect("mysql proxy metadata CAS should execute");
        let persisted = repository
            .find_proxy_node(&node.id)
            .await
            .expect("mysql proxy fixture should read")
            .expect("mysql proxy fixture should exist")
            .proxy_metadata;
        let cleanup = sqlx::query("DELETE FROM proxy_nodes WHERE id = ?")
            .bind(&node.id)
            .execute(&pool)
            .await;

        assert!(!updated, "different JSON arrays must not compare equal");
        assert_eq!(persisted, Some(stored));
        cleanup.expect("mysql proxy fixture should clean up");
    }

    #[tokio::test]
    async fn registration_cas_rejects_stale_security_snapshot() {
        let Some(database_url) = std::env::var("AETHER_TEST_MYSQL_URL")
            .ok()
            .filter(|value| !value.trim().is_empty())
        else {
            eprintln!(
                "skipping mysql registration CAS test because AETHER_TEST_MYSQL_URL is unset"
            );
            return;
        };
        let pool = sqlx::mysql::MySqlPoolOptions::new()
            .max_connections(2)
            .connect(&database_url)
            .await
            .expect("mysql test pool should connect");
        run_migrations(&pool)
            .await
            .expect("mysql migrations should run");

        let suffix = uuid::Uuid::new_v4().simple().to_string();
        let node_id = format!("registration-security-{suffix}");
        let endpoint = format!("registration-security-{suffix}");
        let repository = MysqlProxyNodeReadRepository::new(pool.clone());
        let first = repository
            .register_node(&ProxyNodeRegistrationMutation {
                node_id: Some(node_id.clone()),
                name: node_id.clone(),
                ip: endpoint.clone(),
                port: 7070,
                region: None,
                heartbeat_interval: 30,
                active_connections: None,
                total_requests: None,
                avg_latency_ms: None,
                hardware_info: None,
                estimated_max_concurrency: None,
                proxy_metadata: Some(json!({
                    "tunnel_security": {
                        "mode": "non_tls_required",
                        "encryption_key_encrypted": "aether-proxy-node-secret-v2:aether-runtime-secret-v1:sealed-old"
                    }
                })),
                proxy_version: Some("1.0.0".to_string()),
                registered_by: None,
                tunnel_mode: true,
            })
            .await
            .expect("initial mysql registration should succeed");
        let stale = first.clone();

        let rotated = repository
            .register_node(&ProxyNodeRegistrationMutation {
                node_id: Some(node_id.clone()),
                name: node_id.clone(),
                ip: endpoint.clone(),
                port: 7070,
                region: None,
                heartbeat_interval: 30,
                active_connections: None,
                total_requests: None,
                avg_latency_ms: None,
                hardware_info: None,
                estimated_max_concurrency: None,
                proxy_metadata: Some(json!({
                    "tunnel_security": {
                        "mode": "non_tls_required",
                        "encryption_key_encrypted": "aether-proxy-node-secret-v2:aether-runtime-secret-v1:sealed-new"
                    }
                })),
                proxy_version: Some("2.0.0".to_string()),
                registered_by: None,
                tunnel_mode: true,
            })
            .await
            .expect("mysql security rotation should succeed");

        let stale_refresh = ProxyNodeRegistrationMutation {
            node_id: Some(node_id.clone()),
            name: node_id.clone(),
            ip: endpoint,
            port: 7070,
            region: None,
            heartbeat_interval: 30,
            active_connections: None,
            total_requests: None,
            avg_latency_ms: None,
            hardware_info: None,
            estimated_max_concurrency: None,
            proxy_metadata: Some(json!({"runtime": "stale-writer"})),
            proxy_version: Some("2.1.0".to_string()),
            registered_by: None,
            tunnel_mode: true,
        };
        let stale_replacement = merge_proxy_metadata_for_registration(
            stale.proxy_metadata.as_ref(),
            normalize_proxy_metadata(
                stale_refresh.proxy_metadata.as_ref(),
                stale_refresh.proxy_version.as_deref(),
            ),
        );
        assert!(!repository
            .update_existing_registration_if_unchanged(
                &stale_refresh,
                &stale,
                stale_replacement.as_ref(),
                super::current_unix_secs(),
            )
            .await
            .expect("stale mysql registration CAS should execute"));

        let committed = repository
            .register_node(&ProxyNodeRegistrationMutation {
                name: format!("{node_id}-committed"),
                proxy_metadata: Some(json!({"runtime": "committed-after-rotation"})),
                ..stale_refresh
            })
            .await
            .expect("mysql metadata refresh should merge current security state");
        let cleanup = sqlx::query("DELETE FROM proxy_nodes WHERE id = ?")
            .bind(&node_id)
            .execute(&pool)
            .await;

        assert_eq!(
            rotated
                .proxy_metadata
                .as_ref()
                .and_then(|metadata| metadata.pointer("/tunnel_security/encryption_key_encrypted"))
                .and_then(serde_json::Value::as_str),
            Some("aether-proxy-node-secret-v2:aether-runtime-secret-v1:sealed-new")
        );
        assert_eq!(
            committed
                .proxy_metadata
                .as_ref()
                .and_then(|metadata| metadata.pointer("/tunnel_security/encryption_key_encrypted"))
                .and_then(serde_json::Value::as_str),
            Some("aether-proxy-node-secret-v2:aether-runtime-secret-v1:sealed-new")
        );
        assert_eq!(
            committed
                .proxy_metadata
                .as_ref()
                .and_then(|metadata| metadata.get("runtime")),
            Some(&json!("committed-after-rotation"))
        );
        cleanup.expect("mysql proxy fixture should clean up");
    }
}
