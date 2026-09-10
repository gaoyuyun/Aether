use aether_data_contracts::repository::usage::StoredUsageBodyPayload;
use std::io::Write;

use aether_data_contracts::repository::usage::{
    canonical_usage_body_ref_for, parse_usage_body_ref, read_decompressed_usage_json,
    usage_body_ref, StoredRequestUsageAudit, UpsertUsageRecord, UsageBodyCaptureState,
    UsageBodyField,
};
use flate2::{read::GzDecoder, write::GzEncoder, Compression};
use serde_json::{Map, Value};
use sqlx::{mysql::MySqlRow, Row};

use crate::error::SqlResultExt;
use crate::{DataLayerError, MysqlPool};

#[derive(Debug)]
pub(crate) struct PreparedUsageHttpCapture {
    request_headers: Option<String>,
    provider_request_headers: Option<String>,
    response_headers: Option<String>,
    client_response_headers: Option<String>,
    request_body: PreparedBody,
    provider_request_body: PreparedBody,
    response_body: PreparedBody,
    client_response_body: PreparedBody,
    refs: HttpAuditRefs,
    states: HttpAuditStates,
    capture_mode: &'static str,
}

#[derive(Debug)]
struct PreparedBody {
    field: UsageBodyField,
    payload_gzip: Option<Vec<u8>>,
    clear_existing: bool,
}

#[derive(Debug, Default)]
struct HttpAuditRefs {
    request_body_ref: Option<String>,
    provider_request_body_ref: Option<String>,
    response_body_ref: Option<String>,
    client_response_body_ref: Option<String>,
}

impl HttpAuditRefs {
    fn any_present(&self) -> bool {
        self.request_body_ref.is_some()
            || self.provider_request_body_ref.is_some()
            || self.response_body_ref.is_some()
            || self.client_response_body_ref.is_some()
    }
}

#[derive(Debug, Default)]
struct HttpAuditStates {
    request_body_state: Option<UsageBodyCaptureState>,
    provider_request_body_state: Option<UsageBodyCaptureState>,
    response_body_state: Option<UsageBodyCaptureState>,
    client_response_body_state: Option<UsageBodyCaptureState>,
}

pub(crate) fn capture_update_allowed(
    previous: Option<&StoredRequestUsageAudit>,
    incoming_status: &str,
) -> bool {
    let Some(previous) = previous else {
        return true;
    };
    if previous.billing_status != "pending" {
        return false;
    }
    if matches!(
        previous.status.as_str(),
        "completed" | "failed" | "cancelled"
    ) && matches!(incoming_status, "pending" | "streaming")
    {
        return false;
    }
    !(previous.status == "streaming" && incoming_status == "pending")
}

pub(crate) fn apply_previous_metadata_tombstones(
    usage: &mut UpsertUsageRecord,
    previous: Option<&StoredRequestUsageAudit>,
) {
    if usage.request_metadata.is_some() {
        return;
    }
    let clear_request = usage.request_body_state == Some(UsageBodyCaptureState::None);
    let clear_provider_request =
        usage.provider_request_body_state == Some(UsageBodyCaptureState::None);
    if !clear_request && !clear_provider_request {
        return;
    }
    let mut metadata = previous
        .and_then(|previous| previous.request_metadata.as_ref())
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    if clear_request {
        metadata.remove("requested_reasoning_effort");
        metadata.remove("request_body_ref");
    }
    if clear_provider_request {
        metadata.remove("provider_reasoning_effort");
        metadata.remove("provider_service_tier");
        metadata.remove("provider_cache_ttl_minutes");
        metadata.remove("provider_request_body_ref");
    }
    usage.request_metadata = Some(Value::Object(metadata));
}

pub(crate) fn prepare_usage_http_capture(
    usage: &mut UpsertUsageRecord,
) -> Result<PreparedUsageHttpCapture, DataLayerError> {
    let clear_request = usage.request_body_state == Some(UsageBodyCaptureState::None);
    let clear_provider_request =
        usage.provider_request_body_state == Some(UsageBodyCaptureState::None);
    let clear_response = usage.response_body_state == Some(UsageBodyCaptureState::None);
    let clear_client_response =
        usage.client_response_body_state == Some(UsageBodyCaptureState::None);

    let request_body_value = (!clear_request)
        .then_some(usage.request_body.as_ref())
        .flatten();
    let provider_request_body_value = (!clear_provider_request)
        .then_some(usage.provider_request_body.as_ref())
        .flatten();
    let response_body_value = (!clear_response)
        .then_some(usage.response_body.as_ref())
        .flatten();
    let client_response_body_value = (!clear_client_response)
        .then_some(usage.client_response_body.as_ref())
        .flatten();

    let request_body = prepare_body(
        UsageBodyField::RequestBody,
        request_body_value,
        clear_request,
    )?;
    let provider_request_body = prepare_body(
        UsageBodyField::ProviderRequestBody,
        provider_request_body_value,
        clear_provider_request,
    )?;
    let response_body = prepare_body(
        UsageBodyField::ResponseBody,
        response_body_value,
        clear_response,
    )?;
    let client_response_body = prepare_body(
        UsageBodyField::ClientResponseBody,
        client_response_body_value,
        clear_client_response,
    )?;

    let refs = HttpAuditRefs {
        request_body_ref: resolved_write_ref(
            (!clear_request)
                .then_some(usage.request_body_ref.as_deref())
                .flatten(),
            &usage.request_id,
            UsageBodyField::RequestBody,
            request_body.payload_gzip.is_some(),
        ),
        provider_request_body_ref: resolved_write_ref(
            (!clear_provider_request)
                .then_some(usage.provider_request_body_ref.as_deref())
                .flatten(),
            &usage.request_id,
            UsageBodyField::ProviderRequestBody,
            provider_request_body.payload_gzip.is_some(),
        ),
        response_body_ref: resolved_write_ref(
            (!clear_response)
                .then_some(usage.response_body_ref.as_deref())
                .flatten(),
            &usage.request_id,
            UsageBodyField::ResponseBody,
            response_body.payload_gzip.is_some(),
        ),
        client_response_body_ref: resolved_write_ref(
            (!clear_client_response)
                .then_some(usage.client_response_body_ref.as_deref())
                .flatten(),
            &usage.request_id,
            UsageBodyField::ClientResponseBody,
            client_response_body.payload_gzip.is_some(),
        ),
    };
    let states = HttpAuditStates {
        request_body_state: state_for_storage(
            usage.request_body_state,
            &request_body,
            refs.request_body_ref.as_deref(),
        ),
        provider_request_body_state: state_for_storage(
            usage.provider_request_body_state,
            &provider_request_body,
            refs.provider_request_body_ref.as_deref(),
        ),
        response_body_state: state_for_storage(
            usage.response_body_state,
            &response_body,
            refs.response_body_ref.as_deref(),
        ),
        client_response_body_state: state_for_storage(
            usage.client_response_body_state,
            &client_response_body,
            refs.client_response_body_ref.as_deref(),
        ),
    };

    usage.request_metadata = prepare_metadata_for_body_storage(
        usage.request_metadata.take(),
        [
            (
                UsageBodyField::RequestBody,
                request_body_value.is_some(),
                usage.request_body_ref.as_deref(),
            ),
            (
                UsageBodyField::ProviderRequestBody,
                provider_request_body_value.is_some(),
                usage.provider_request_body_ref.as_deref(),
            ),
            (
                UsageBodyField::ResponseBody,
                response_body_value.is_some(),
                usage.response_body_ref.as_deref(),
            ),
            (
                UsageBodyField::ClientResponseBody,
                client_response_body_value.is_some(),
                usage.client_response_body_ref.as_deref(),
            ),
        ],
    );

    let capture_mode = if refs.any_present() {
        "ref_backed"
    } else if [
        request_body_value,
        provider_request_body_value,
        response_body_value,
        client_response_body_value,
    ]
    .iter()
    .any(Option::is_some)
    {
        "inline_legacy"
    } else {
        "none"
    };

    Ok(PreparedUsageHttpCapture {
        request_headers: json_text(usage.request_headers.as_ref())?,
        provider_request_headers: json_text(usage.provider_request_headers.as_ref())?,
        response_headers: json_text(usage.response_headers.as_ref())?,
        client_response_headers: json_text(usage.client_response_headers.as_ref())?,
        request_body,
        provider_request_body,
        response_body,
        client_response_body,
        refs,
        states,
        capture_mode,
    })
}

fn prepare_body(
    field: UsageBodyField,
    value: Option<&Value>,
    clear_existing: bool,
) -> Result<PreparedBody, DataLayerError> {
    Ok(PreparedBody {
        field,
        payload_gzip: value.map(compress_json).transpose()?,
        clear_existing,
    })
}

fn resolved_write_ref(
    explicit_ref: Option<&str>,
    request_id: &str,
    field: UsageBodyField,
    has_blob: bool,
) -> Option<String> {
    explicit_ref
        .and_then(|body_ref| canonical_usage_body_ref_for(body_ref, request_id, field))
        .or_else(|| has_blob.then(|| usage_body_ref(request_id, field)))
}

fn state_for_storage(
    incoming: Option<UsageBodyCaptureState>,
    body: &PreparedBody,
    body_ref: Option<&str>,
) -> Option<UsageBodyCaptureState> {
    if matches!(
        incoming,
        Some(
            UsageBodyCaptureState::Disabled
                | UsageBodyCaptureState::Unavailable
                | UsageBodyCaptureState::None
        )
    ) {
        return incoming;
    }
    if body.payload_gzip.is_some() || body_ref.is_some() {
        return Some(UsageBodyCaptureState::Reference);
    }
    incoming
}

fn json_text(value: Option<&Value>) -> Result<Option<String>, DataLayerError> {
    value
        .map(|value| {
            serde_json::to_string(value).map_err(|err| {
                DataLayerError::UnexpectedValue(format!("failed to serialize usage JSON: {err}"))
            })
        })
        .transpose()
}

fn prepare_metadata_for_body_storage(
    metadata: Option<Value>,
    body_fields: [(UsageBodyField, bool, Option<&str>); 4],
) -> Option<Value> {
    let mut object = match metadata {
        Some(Value::Object(object)) => object,
        Some(value) => {
            let mut object = Map::new();
            object.insert("request_metadata".to_string(), value);
            object
        }
        None => Map::new(),
    };
    let should_replace = !object.is_empty()
        || body_fields
            .iter()
            .any(|(_, has_value, explicit_ref)| *has_value || explicit_ref.is_some());
    if !should_replace {
        return None;
    }
    for (field, has_value, explicit_ref) in body_fields {
        if has_value || explicit_ref.is_some() {
            object.remove(field.as_ref_key());
        }
    }
    Some(Value::Object(object))
}

pub(crate) async fn sync_usage_http_capture(
    tx: &mut sqlx::Transaction<'_, sqlx::MySql>,
    request_id: &str,
    prepared: &PreparedUsageHttpCapture,
) -> Result<(), DataLayerError> {
    for body in [
        &prepared.request_body,
        &prepared.provider_request_body,
        &prepared.response_body,
        &prepared.client_response_body,
    ] {
        sync_body(tx, request_id, body).await?;
    }
    let headers_present = prepared.request_headers.is_some()
        || prepared.provider_request_headers.is_some()
        || prepared.response_headers.is_some()
        || prepared.client_response_headers.is_some();
    if !headers_present
        && !prepared.refs.any_present()
        && !prepared.states.any_present()
        && prepared.capture_mode == "none"
    {
        return Ok(());
    }

    sqlx::query(
        r#"
INSERT INTO usage_http_audits (
  request_id,
  request_headers,
  provider_request_headers,
  response_headers,
  client_response_headers,
  request_body_ref,
  provider_request_body_ref,
  response_body_ref,
  client_response_body_ref,
  request_body_state,
  provider_request_body_state,
  response_body_state,
  client_response_body_state,
  body_capture_mode
)
VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
ON DUPLICATE KEY UPDATE
  request_headers = COALESCE(VALUES(request_headers), request_headers),
  provider_request_headers = COALESCE(
    VALUES(provider_request_headers),
    provider_request_headers
  ),
  response_headers = COALESCE(VALUES(response_headers), response_headers),
  client_response_headers = COALESCE(
    VALUES(client_response_headers),
    client_response_headers
  ),
  request_body_ref = CASE
    WHEN VALUES(request_body_state) = 'none' THEN NULL
    ELSE COALESCE(VALUES(request_body_ref), request_body_ref)
  END,
  provider_request_body_ref = CASE
    WHEN VALUES(provider_request_body_state) = 'none' THEN NULL
    ELSE COALESCE(VALUES(provider_request_body_ref), provider_request_body_ref)
  END,
  response_body_ref = CASE
    WHEN VALUES(response_body_state) = 'none' THEN NULL
    ELSE COALESCE(VALUES(response_body_ref), response_body_ref)
  END,
  client_response_body_ref = CASE
    WHEN VALUES(client_response_body_state) = 'none' THEN NULL
    ELSE COALESCE(VALUES(client_response_body_ref), client_response_body_ref)
  END,
  request_body_state = COALESCE(VALUES(request_body_state), request_body_state),
  provider_request_body_state = COALESCE(
    VALUES(provider_request_body_state),
    provider_request_body_state
  ),
  response_body_state = COALESCE(VALUES(response_body_state), response_body_state),
  client_response_body_state = COALESCE(
    VALUES(client_response_body_state),
    client_response_body_state
  ),
  body_capture_mode = CASE
    WHEN VALUES(body_capture_mode) = 'none'
      AND (
        VALUES(request_body_state) = 'none'
        OR VALUES(provider_request_body_state) = 'none'
        OR VALUES(response_body_state) = 'none'
        OR VALUES(client_response_body_state) = 'none'
      )
      THEN 'none'
    ELSE COALESCE(
      NULLIF(VALUES(body_capture_mode), 'none'),
      body_capture_mode,
      'none'
    )
  END,
  updated_at = UNIX_TIMESTAMP()
"#,
    )
    .bind(request_id)
    .bind(&prepared.request_headers)
    .bind(&prepared.provider_request_headers)
    .bind(&prepared.response_headers)
    .bind(&prepared.client_response_headers)
    .bind(prepared.refs.request_body_ref.as_deref())
    .bind(prepared.refs.provider_request_body_ref.as_deref())
    .bind(prepared.refs.response_body_ref.as_deref())
    .bind(prepared.refs.client_response_body_ref.as_deref())
    .bind(
        prepared
            .states
            .request_body_state
            .map(UsageBodyCaptureState::as_str),
    )
    .bind(
        prepared
            .states
            .provider_request_body_state
            .map(UsageBodyCaptureState::as_str),
    )
    .bind(
        prepared
            .states
            .response_body_state
            .map(UsageBodyCaptureState::as_str),
    )
    .bind(
        prepared
            .states
            .client_response_body_state
            .map(UsageBodyCaptureState::as_str),
    )
    .bind(prepared.capture_mode)
    .execute(&mut **tx)
    .await
    .map_sql_err()?;
    Ok(())
}

pub(crate) fn hydrate_usage_row(
    row: &MySqlRow,
    usage: &mut StoredRequestUsageAudit,
    resolve_legacy_compressed: bool,
) -> Result<(), DataLayerError> {
    usage.request_headers = optional_json(row, "request_headers")?;
    usage.provider_request_headers = optional_json(row, "provider_request_headers")?;
    usage.response_headers = optional_json(row, "response_headers")?;
    usage.client_response_headers = optional_json(row, "client_response_headers")?;

    let request_body = legacy_body_column(
        row,
        "request_body",
        "request_body_compressed",
        resolve_legacy_compressed,
    )?;
    let provider_request_body = legacy_body_column(
        row,
        "provider_request_body",
        "provider_request_body_compressed",
        resolve_legacy_compressed,
    )?;
    let response_body = legacy_body_column(
        row,
        "response_body",
        "response_body_compressed",
        resolve_legacy_compressed,
    )?;
    let client_response_body = legacy_body_column(
        row,
        "client_response_body",
        "client_response_body_compressed",
        resolve_legacy_compressed,
    )?;
    usage.request_body = request_body.0;
    usage.provider_request_body = provider_request_body.0;
    usage.response_body = response_body.0;
    usage.client_response_body = client_response_body.0;

    let metadata = usage.request_metadata.as_ref().and_then(Value::as_object);
    usage.request_body_ref = resolved_read_ref(
        row.try_get("http_request_body_ref").map_sql_err()?,
        metadata,
        &usage.request_id,
        UsageBodyField::RequestBody,
        request_body.1,
    );
    usage.provider_request_body_ref = resolved_read_ref(
        row.try_get("http_provider_request_body_ref")
            .map_sql_err()?,
        metadata,
        &usage.request_id,
        UsageBodyField::ProviderRequestBody,
        provider_request_body.1,
    );
    usage.response_body_ref = resolved_read_ref(
        row.try_get("http_response_body_ref").map_sql_err()?,
        metadata,
        &usage.request_id,
        UsageBodyField::ResponseBody,
        response_body.1,
    );
    usage.client_response_body_ref = resolved_read_ref(
        row.try_get("http_client_response_body_ref").map_sql_err()?,
        metadata,
        &usage.request_id,
        UsageBodyField::ClientResponseBody,
        client_response_body.1,
    );
    usage.request_body_state = optional_state(row, "http_request_body_state")?;
    usage.provider_request_body_state = optional_state(row, "http_provider_request_body_state")?;
    usage.response_body_state = optional_state(row, "http_response_body_state")?;
    usage.client_response_body_state = optional_state(row, "http_client_response_body_state")?;
    Ok(())
}

fn optional_json(row: &MySqlRow, column: &str) -> Result<Option<Value>, DataLayerError> {
    row.try_get::<Option<String>, _>(column)
        .map_sql_err()?
        .map(|raw| {
            serde_json::from_str(&raw).map_err(|err| {
                DataLayerError::UnexpectedValue(format!("invalid usage JSON in {column}: {err}"))
            })
        })
        .transpose()
}

fn legacy_body_column(
    row: &MySqlRow,
    inline_column: &str,
    compressed_column: &str,
    resolve_compressed: bool,
) -> Result<(Option<Value>, bool), DataLayerError> {
    let inline = optional_json(row, inline_column)?;
    if inline.is_some() {
        return Ok((inline, false));
    }
    let compressed = row
        .try_get::<Option<Vec<u8>>, _>(compressed_column)
        .map_sql_err()?;
    let has_compressed = compressed.is_some();
    let value = if resolve_compressed {
        compressed.map(|bytes| inflate_json(&bytes)).transpose()?
    } else {
        None
    };
    Ok((value, has_compressed))
}

fn resolved_read_ref(
    audit_ref: Option<String>,
    metadata: Option<&Map<String, Value>>,
    request_id: &str,
    field: UsageBodyField,
    has_compressed: bool,
) -> Option<String> {
    audit_ref
        .and_then(|body_ref| canonical_usage_body_ref_for(&body_ref, request_id, field))
        .or_else(|| has_compressed.then(|| usage_body_ref(request_id, field)))
        .or_else(|| metadata_body_ref(metadata, request_id, field))
}

fn metadata_body_ref(
    metadata: Option<&Map<String, Value>>,
    request_id: &str,
    field: UsageBodyField,
) -> Option<String> {
    metadata
        .and_then(|metadata| metadata.get(field.as_ref_key()))
        .and_then(Value::as_str)
        .and_then(|body_ref| canonical_usage_body_ref_for(body_ref, request_id, field))
}

fn optional_state(
    row: &MySqlRow,
    column: &str,
) -> Result<Option<UsageBodyCaptureState>, DataLayerError> {
    Ok(row
        .try_get::<Option<String>, _>(column)
        .map_sql_err()?
        .as_deref()
        .and_then(parse_state))
}

fn parse_state(value: &str) -> Option<UsageBodyCaptureState> {
    match value.trim() {
        "none" => Some(UsageBodyCaptureState::None),
        "inline" => Some(UsageBodyCaptureState::Inline),
        "reference" => Some(UsageBodyCaptureState::Reference),
        "truncated" => Some(UsageBodyCaptureState::Truncated),
        "disabled" => Some(UsageBodyCaptureState::Disabled),
        "unavailable" => Some(UsageBodyCaptureState::Unavailable),
        _ => None,
    }
}

pub(crate) async fn hydrate_usage_body_refs(
    pool: &MysqlPool,
    mut usage: StoredRequestUsageAudit,
) -> Result<StoredRequestUsageAudit, DataLayerError> {
    for field in [
        UsageBodyField::RequestBody,
        UsageBodyField::ProviderRequestBody,
        UsageBodyField::ResponseBody,
        UsageBodyField::ClientResponseBody,
    ] {
        if usage.body_value(field).is_some() {
            continue;
        }
        let Some(body_ref) = usage.body_ref(field) else {
            continue;
        };
        let Some(body_ref) = canonical_usage_body_ref_for(body_ref, &usage.request_id, field)
        else {
            continue;
        };
        let value = resolve_body_ref(pool, &body_ref).await?;
        match field {
            UsageBodyField::RequestBody => usage.request_body = value,
            UsageBodyField::ProviderRequestBody => usage.provider_request_body = value,
            UsageBodyField::ResponseBody => usage.response_body = value,
            UsageBodyField::ClientResponseBody => usage.client_response_body = value,
        }
    }
    Ok(usage)
}

pub(crate) async fn read_body_payload(
    pool: &MysqlPool,
    body_ref: &str,
) -> Result<Option<StoredUsageBodyPayload>, DataLayerError> {
    let json_limit =
        aether_data_contracts::repository::usage::MAX_DECOMPRESSED_USAGE_JSON_BYTES as i64;
    let encoded_limit = json_limit + 1024 * 1024;
    let Some((request_id, field)) = parse_usage_body_ref(body_ref) else {
        return Ok(None);
    };
    let canonical_ref = usage_body_ref(&request_id, field);
    if let Some(row) = sqlx::query(
        "SELECT CASE WHEN LENGTH(payload_gzip) <= ? THEN payload_gzip END AS payload_gzip FROM usage_body_blobs WHERE body_ref = ? AND request_id = ? AND body_field = ? LIMIT 1",
    )
    .bind(encoded_limit)
    .bind(&canonical_ref)
    .bind(&request_id)
    .bind(field.as_storage_field())
    .fetch_optional(pool)
    .await
    .map_sql_err()?
    {
        return row.try_get::<Option<Vec<u8>>, _>("payload_gzip")
            .map_sql_err()?
            .map(|bytes| Some(StoredUsageBodyPayload::Gzip(bytes)))
            .ok_or_else(|| DataLayerError::UnexpectedValue(format!("encoded usage json exceeds {encoded_limit} bytes")));
    }
    let (inline_column, compressed_column) = usage_body_sql_columns(field);
    let row = sqlx::query(&format!(
        "SELECT CASE WHEN OCTET_LENGTH({inline_column}) <= ? THEN CAST({inline_column} AS CHAR) END AS inline_body, CASE WHEN LENGTH({compressed_column}) <= ? THEN {compressed_column} END AS compressed_body, (COALESCE(OCTET_LENGTH({inline_column}) > ?, false) OR ({inline_column} IS NULL AND COALESCE(LENGTH({compressed_column}) > ?, false))) AS too_large FROM `usage` WHERE request_id = ? LIMIT 1"
    ))
    .bind(json_limit)
    .bind(encoded_limit)
    .bind(json_limit)
    .bind(encoded_limit)
    .bind(request_id)
    .fetch_optional(pool)
    .await
    .map_sql_err()?;
    let Some(row) = row else {
        return Ok(None);
    };
    if row.try_get::<bool, _>("too_large").map_sql_err()? {
        return Err(DataLayerError::UnexpectedValue(
            "encoded usage json exceeds preview limit".to_string(),
        ));
    }
    if let Some(body) = row
        .try_get::<Option<String>, _>("inline_body")
        .map_sql_err()?
    {
        return Ok(Some(StoredUsageBodyPayload::Json(body.into_bytes())));
    }
    Ok(row
        .try_get::<Option<Vec<u8>>, _>("compressed_body")
        .map_sql_err()?
        .map(StoredUsageBodyPayload::Gzip))
}

static USAGE_BODY_DECODE_SLOTS: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(4);

pub(crate) async fn resolve_body_ref(
    pool: &MysqlPool,
    body_ref: &str,
) -> Result<Option<Value>, DataLayerError> {
    let Some(payload) = read_body_payload(pool, body_ref).await? else {
        return Ok(None);
    };
    let permit = USAGE_BODY_DECODE_SLOTS.acquire().await.map_err(|error| {
        DataLayerError::UnexpectedValue(format!("usage body decoder unavailable: {error}"))
    })?;
    tokio::task::spawn_blocking(move || {
        let _permit = permit;
        match payload {
            StoredUsageBodyPayload::Gzip(bytes) => inflate_json(&bytes).map(Some),
            StoredUsageBodyPayload::Json(bytes) => {
                let bytes = aether_data_contracts::repository::usage::read_decompressed_usage_json(
                    std::io::Cursor::new(bytes),
                )?;
                serde_json::from_slice(&bytes).map(Some).map_err(|error| {
                    DataLayerError::UnexpectedValue(format!(
                        "invalid inline usage body JSON: {error}"
                    ))
                })
            }
        }
    })
    .await
    .map_err(|error| {
        DataLayerError::UnexpectedValue(format!("usage body decoder failed: {error}"))
    })?
}

fn usage_body_sql_columns(field: UsageBodyField) -> (&'static str, &'static str) {
    match field {
        UsageBodyField::RequestBody => ("request_body", "request_body_compressed"),
        UsageBodyField::ProviderRequestBody => {
            ("provider_request_body", "provider_request_body_compressed")
        }
        UsageBodyField::ResponseBody => ("response_body", "response_body_compressed"),
        UsageBodyField::ClientResponseBody => {
            ("client_response_body", "client_response_body_compressed")
        }
    }
}

fn inflate_json(bytes: &[u8]) -> Result<Value, DataLayerError> {
    let decoded = read_decompressed_usage_json(GzDecoder::new(bytes))?;
    serde_json::from_slice(&decoded).map_err(|err| {
        DataLayerError::UnexpectedValue(format!("failed to decode usage body JSON: {err}"))
    })
}

fn compress_json(value: &Value) -> Result<Vec<u8>, DataLayerError> {
    let bytes = serde_json::to_vec(value).map_err(|err| {
        DataLayerError::UnexpectedValue(format!("failed to serialize usage body: {err}"))
    })?;
    let mut encoder = GzEncoder::new(Vec::new(), Compression::new(6));
    encoder.write_all(&bytes).map_err(|err| {
        DataLayerError::UnexpectedValue(format!("failed to gzip usage body: {err}"))
    })?;
    encoder.finish().map_err(|err| {
        DataLayerError::UnexpectedValue(format!("failed to finish usage body gzip: {err}"))
    })
}

async fn sync_body(
    tx: &mut sqlx::Transaction<'_, sqlx::MySql>,
    request_id: &str,
    body: &PreparedBody,
) -> Result<(), DataLayerError> {
    let body_ref = usage_body_ref(request_id, body.field);
    if body.clear_existing || body.payload_gzip.is_some() {
        sqlx::query(clear_legacy_body_sql(body.field))
            .bind(request_id)
            .execute(&mut **tx)
            .await
            .map_sql_err()?;
    }
    if body.clear_existing {
        sqlx::query("DELETE FROM usage_body_blobs WHERE body_ref = ?")
            .bind(body_ref)
            .execute(&mut **tx)
            .await
            .map_sql_err()?;
        return Ok(());
    }
    if let Some(payload_gzip) = body.payload_gzip.as_deref() {
        sqlx::query(
            r#"
INSERT INTO usage_body_blobs (body_ref, request_id, body_field, payload_gzip)
VALUES (?, ?, ?, ?)
ON DUPLICATE KEY UPDATE
  request_id = VALUES(request_id),
  body_field = VALUES(body_field),
  payload_gzip = VALUES(payload_gzip),
  updated_at = UNIX_TIMESTAMP()
"#,
        )
        .bind(body_ref)
        .bind(request_id)
        .bind(body.field.as_storage_field())
        .bind(payload_gzip)
        .execute(&mut **tx)
        .await
        .map_sql_err()?;
    }
    Ok(())
}

fn clear_legacy_body_sql(field: UsageBodyField) -> &'static str {
    match field {
        UsageBodyField::RequestBody => {
            "UPDATE `usage` SET request_body = NULL, request_body_compressed = NULL WHERE request_id = ?"
        }
        UsageBodyField::ProviderRequestBody => {
            "UPDATE `usage` SET provider_request_body = NULL, provider_request_body_compressed = NULL WHERE request_id = ?"
        }
        UsageBodyField::ResponseBody => {
            "UPDATE `usage` SET response_body = NULL, response_body_compressed = NULL WHERE request_id = ?"
        }
        UsageBodyField::ClientResponseBody => {
            "UPDATE `usage` SET client_response_body = NULL, client_response_body_compressed = NULL WHERE request_id = ?"
        }
    }
}

impl HttpAuditStates {
    fn any_present(&self) -> bool {
        self.request_body_state.is_some()
            || self.provider_request_body_state.is_some()
            || self.response_body_state.is_some()
            || self.client_response_body_state.is_some()
    }
}
