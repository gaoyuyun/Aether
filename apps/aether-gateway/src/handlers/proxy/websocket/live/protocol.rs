//! Minimal validation at the Codex Live trust boundary.
//!
//! Live events remain opaque after the initial discriminator. This module owns
//! only bounded identifiers, multipart framing and the first `session.update`
//! check; it intentionally does not copy the evolving Codex event schema.

use axum::http::{HeaderMap, StatusCode};
use serde_json::Value;

const MAX_MODEL_BYTES: usize = 256;
const MAX_CALL_ID_BYTES: usize = 256;
const MAX_BOUNDARY_BYTES: usize = 70;
const MAX_MULTIPART_BODY_BYTES: usize = 1024 * 1024;
const MAX_SDP_BYTES: usize = 512 * 1024;
const MAX_SESSION_BYTES: usize = 256 * 1024;
const MAX_PART_HEADERS_BYTES: usize = 8 * 1024;

pub(super) const LEGACY_LIVE_CALL_PATH: &str = "/v1/live";
pub(super) const REALTIME_CALLS_PATH: &str = "/v1/realtime/calls";
pub(super) const REALTIME_SIDEBAND_PATH: &str = "/v1/realtime";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum LiveRouteDialect {
    LegacyLive,
    Realtime,
    /// OpenAI Realtime v2, which is the current Codex default.  Unlike the
    /// legacy Codex realtime dialect it deliberately omits the
    /// `intent=quicksilver` query selector and `openai-alpha` header.
    RealtimeV2,
}

impl LiveRouteDialect {
    pub(super) fn from_call_create_path(path: &str) -> Option<Self> {
        match path {
            LEGACY_LIVE_CALL_PATH => Some(Self::LegacyLive),
            REALTIME_CALLS_PATH => Some(Self::Realtime),
            _ => None,
        }
    }

    pub(super) fn downstream_location(self, call_id: &str) -> String {
        match self {
            Self::LegacyLive => format!("{LEGACY_LIVE_CALL_PATH}/{call_id}"),
            Self::Realtime => format!("{REALTIME_CALLS_PATH}/{call_id}"),
            // V2 does not create WebRTC calls through this route today (Codex
            // pins AVAS call creation to V1).  Keep a valid sideband location
            // for defensive callers rather than synthesising a new path.
            Self::RealtimeV2 => format!("{REALTIME_SIDEBAND_PATH}?call_id={call_id}"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub(super) enum LiveProtocolError {
    #[error("missing Live upstream URL")]
    MissingUpstreamUrl,
    #[error("invalid Live upstream URL")]
    InvalidUpstreamUrl,
    #[error("ChatGPT OAuth does not support direct Codex Live WebSocket")]
    OauthDirectWebSocketUnsupported,
    #[error("ChatGPT OAuth Codex Live requires the official backend origin")]
    OauthUpstreamUnsupported,
    #[error("invalid Live model query")]
    InvalidModelQuery,
    #[error("invalid Live websocket intent")]
    InvalidLiveIntent,
    #[error("invalid Live WebRTC architecture")]
    InvalidLiveArchitecture,
    #[error("invalid Live model")]
    InvalidModel,
    #[error("invalid Live call ID")]
    InvalidCallId,
    #[error("unsupported Live media type")]
    UnsupportedMediaType,
    #[error("invalid Live multipart boundary")]
    InvalidBoundary,
    #[error("Live multipart body is too large")]
    MultipartBodyTooLarge,
    #[error("malformed Live multipart body")]
    MalformedMultipart,
    #[error("unexpected Live multipart part")]
    UnexpectedMultipartPart,
    #[error("duplicate Live multipart part")]
    DuplicateMultipartPart,
    #[error("missing Live SDP part")]
    MissingSdp,
    #[error("invalid Live SDP part")]
    InvalidSdp,
    #[error("Live SDP is too large")]
    SdpTooLarge,
    #[error("missing Live session part")]
    MissingSession,
    #[error("invalid Live session JSON")]
    InvalidSession,
    #[error("Live session is too large")]
    SessionTooLarge,
    #[error("invalid initial Live JSON event")]
    InvalidInitialEvent,
    #[error("initial Live event must be session.update")]
    ExpectedSessionUpdate,
    #[error("initial Live event must be text")]
    InitialEventMustBeText,
    #[error("initial Live client read failed")]
    InitialClientReadFailed,
    #[error("timed out waiting for initial Live session.update")]
    InitialSessionUpdateTimeout,
    #[error("invalid Live call location")]
    InvalidCallLocation,
}

impl LiveProtocolError {
    pub(super) const fn status_code(&self) -> StatusCode {
        match self {
            Self::MultipartBodyTooLarge | Self::SdpTooLarge | Self::SessionTooLarge => {
                StatusCode::PAYLOAD_TOO_LARGE
            }
            Self::UnsupportedMediaType => StatusCode::UNSUPPORTED_MEDIA_TYPE,
            Self::MissingUpstreamUrl
            | Self::InvalidUpstreamUrl
            | Self::OauthUpstreamUnsupported
            | Self::InvalidCallLocation => StatusCode::BAD_GATEWAY,
            Self::InitialSessionUpdateTimeout => StatusCode::REQUEST_TIMEOUT,
            _ => StatusCode::BAD_REQUEST,
        }
    }

    pub(super) const fn code(&self) -> &'static str {
        match self {
            Self::MissingUpstreamUrl => "codex_live_upstream_url_missing",
            Self::InvalidUpstreamUrl => "codex_live_upstream_url_invalid",
            Self::OauthDirectWebSocketUnsupported => "codex_live_oauth_direct_unsupported",
            Self::OauthUpstreamUnsupported => "codex_live_oauth_upstream_unsupported",
            Self::InvalidModelQuery => "codex_live_model_query_invalid",
            Self::InvalidLiveIntent => "codex_live_intent_invalid",
            Self::InvalidLiveArchitecture => "codex_live_architecture_invalid",
            Self::InvalidModel => "codex_live_model_invalid",
            Self::InvalidCallId => "codex_live_call_id_invalid",
            Self::UnsupportedMediaType => "codex_live_media_type_unsupported",
            Self::InvalidBoundary => "codex_live_boundary_invalid",
            Self::MultipartBodyTooLarge => "codex_live_body_too_large",
            Self::MalformedMultipart => "codex_live_multipart_invalid",
            Self::UnexpectedMultipartPart => "codex_live_multipart_part_unexpected",
            Self::DuplicateMultipartPart => "codex_live_multipart_part_duplicate",
            Self::MissingSdp => "codex_live_sdp_missing",
            Self::InvalidSdp => "codex_live_sdp_invalid",
            Self::SdpTooLarge => "codex_live_sdp_too_large",
            Self::MissingSession => "codex_live_session_missing",
            Self::InvalidSession => "codex_live_session_invalid",
            Self::SessionTooLarge => "codex_live_session_too_large",
            Self::InvalidInitialEvent => "codex_live_initial_event_invalid",
            Self::ExpectedSessionUpdate => "codex_live_expected_session_update",
            Self::InitialEventMustBeText => "codex_live_initial_event_must_be_text",
            Self::InitialClientReadFailed => "codex_live_initial_client_read_failed",
            Self::InitialSessionUpdateTimeout => "codex_live_initial_session_update_timeout",
            Self::InvalidCallLocation => "codex_live_call_location_invalid",
        }
    }

    pub(super) const fn client_message(&self) -> &'static str {
        match self {
            Self::MissingUpstreamUrl | Self::InvalidUpstreamUrl => {
                "Codex Live provider URL is invalid"
            }
            Self::OauthDirectWebSocketUnsupported => {
                "Direct Codex Live WebSocket requires an API-key provider; use WebRTC for ChatGPT OAuth"
            }
            Self::OauthUpstreamUnsupported => {
                "ChatGPT OAuth Codex Live requires the official ChatGPT backend"
            }
            Self::InvalidModelQuery => {
                "Codex Live WebSocket requires exactly one model query parameter"
            }
            Self::InvalidLiveIntent => {
                "Codex Live WebSocket requires intent=quicksilver"
            }
            Self::InvalidLiveArchitecture => {
                "Codex Live WebRTC call creation requires architecture=avas"
            }
            Self::InvalidModel => {
                "Codex Live model must be a non-empty identifier no longer than 256 bytes"
            }
            Self::InvalidCallId => "Codex Live call ID is invalid",
            Self::UnsupportedMediaType => {
                "Codex Live WebRTC call creation requires multipart/form-data"
            }
            Self::InvalidBoundary => "Codex Live multipart boundary is invalid",
            Self::MultipartBodyTooLarge => "Codex Live WebRTC offer exceeds the 1 MiB limit",
            Self::MalformedMultipart => "Codex Live multipart body is malformed",
            Self::UnexpectedMultipartPart => {
                "Codex Live multipart body may contain only sdp and session parts"
            }
            Self::DuplicateMultipartPart => "Codex Live multipart part is duplicated",
            Self::MissingSdp => "Codex Live multipart body is missing the sdp part",
            Self::InvalidSdp => "Codex Live sdp part must be non-empty UTF-8",
            Self::SdpTooLarge => "Codex Live sdp part exceeds the 512 KiB limit",
            Self::MissingSession => "Codex Live multipart body is missing the session part",
            Self::InvalidSession => "Codex Live session part must be a JSON object",
            Self::SessionTooLarge => "Codex Live session exceeds the 256 KiB limit",
            Self::InvalidInitialEvent => {
                "The initial Codex Live WebSocket text message must be a JSON object"
            }
            Self::ExpectedSessionUpdate => {
                "Codex Live WebSocket must start with a session.update event"
            }
            Self::InitialEventMustBeText => {
                "The initial Codex Live session.update event must be a text message"
            }
            Self::InitialClientReadFailed => {
                "Failed to read the initial Codex Live session.update event"
            }
            Self::InitialSessionUpdateTimeout => {
                "Timed out waiting for the initial Codex Live session.update event"
            }
            Self::InvalidCallLocation => {
                "Codex Live upstream returned an invalid call location"
            }
        }
    }

    pub(super) const fn is_timeout(&self) -> bool {
        matches!(self, Self::InitialSessionUpdateTimeout)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub(super) struct LiveMultipart {
    pub(super) sdp: String,
    pub(super) session: Value,
}

pub(super) fn validate_model(model: &str) -> Result<(), LiveProtocolError> {
    if model.is_empty()
        || model.len() > MAX_MODEL_BYTES
        || model.trim() != model
        || model.chars().any(char::is_control)
    {
        return Err(LiveProtocolError::InvalidModel);
    }
    Ok(())
}

pub(super) fn validate_call_id(call_id: &str) -> Result<(), LiveProtocolError> {
    if call_id.is_empty()
        || call_id.len() > MAX_CALL_ID_BYTES
        || matches!(call_id, "." | "..")
        || !call_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.'))
    {
        return Err(LiveProtocolError::InvalidCallId);
    }
    Ok(())
}

/// Validate the current Codex AVAS WebRTC call-create selector.
///
/// The shared `/v1/realtime/calls` route is selected as Codex Live by its
/// unique quicksilver intent. Require the accompanying architecture here so a
/// malformed client request is rejected instead of being silently rewritten
/// into a different upstream transport contract. Unknown non-sensitive query
/// fields remain opaque for forward compatibility.
pub(super) fn validate_realtime_call_create_query(
    query: Option<&str>,
) -> Result<(), LiveProtocolError> {
    let mut intent_seen = false;
    let mut architecture_seen = false;
    for (name, value) in url::form_urlencoded::parse(query.unwrap_or_default().as_bytes()) {
        if name.eq_ignore_ascii_case("intent") {
            if intent_seen || !value.eq_ignore_ascii_case("quicksilver") {
                return Err(LiveProtocolError::InvalidLiveIntent);
            }
            intent_seen = true;
        } else if name.eq_ignore_ascii_case("architecture") {
            if architecture_seen || !value.eq_ignore_ascii_case("avas") {
                return Err(LiveProtocolError::InvalidLiveArchitecture);
            }
            architecture_seen = true;
        } else if live_query_parameter_is_sensitive(name.as_ref()) {
            return Err(LiveProtocolError::InvalidModelQuery);
        }
    }
    if !intent_seen {
        return Err(LiveProtocolError::InvalidLiveIntent);
    }
    if !architecture_seen {
        return Err(LiveProtocolError::InvalidLiveArchitecture);
    }
    Ok(())
}

fn is_live_call_id_segment(call_id: &str) -> bool {
    if validate_call_id(call_id).is_err() {
        return false;
    }
    if call_id.starts_with("rtc_") && call_id.len() > "rtc_".len() {
        return true;
    }
    call_id.len() == 36
        && call_id
            .bytes()
            .enumerate()
            .all(|(index, byte)| match index {
                8 | 13 | 18 | 23 => byte == b'-',
                _ => byte.is_ascii_hexdigit(),
            })
}

pub(super) fn direct_model_from_query(query: Option<&str>) -> Result<String, LiveProtocolError> {
    let mut model = None;
    for (name, value) in url::form_urlencoded::parse(query.unwrap_or_default().as_bytes()) {
        if name.eq_ignore_ascii_case("model") {
            if model.is_some() {
                return Err(LiveProtocolError::InvalidModelQuery);
            }
            validate_model(value.as_ref())?;
            model = Some(value.into_owned());
            continue;
        }
        // Latest Codex preserves provider query parameters while rewriting
        // `/v1/realtime` to `/v1/live`. They are downstream transport hints,
        // not Aether routing authority, so accept and ignore non-credential
        // parameters. Credential values should already have been consumed by
        // ingress; rejecting them here keeps this parser safe in isolation.
        if live_query_parameter_is_sensitive(name.as_ref()) {
            return Err(LiveProtocolError::InvalidModelQuery);
        }
    }
    model.ok_or(LiveProtocolError::InvalidModelQuery)
}

/// Parse the standalone Codex realtime WebSocket query shape.
///
/// Current Codex clients use `/v1/realtime?intent=quicksilver&model=...` for
/// the direct WebSocket transport.  The same path is also used by OpenAI's
/// ordinary Realtime API, so the intent marker is part of the trust boundary:
/// callers must not be able to select the Codex Live planner merely by
/// choosing a model name.
pub(super) fn direct_realtime_model_from_query(
    query: Option<&str>,
) -> Result<String, LiveProtocolError> {
    let mut intent_seen = false;
    for (name, value) in url::form_urlencoded::parse(query.unwrap_or_default().as_bytes()) {
        if name.eq_ignore_ascii_case("intent") {
            if intent_seen || !value.eq_ignore_ascii_case("quicksilver") {
                return Err(LiveProtocolError::InvalidLiveIntent);
            }
            intent_seen = true;
        }
    }
    if !intent_seen {
        return Err(LiveProtocolError::InvalidLiveIntent);
    }
    direct_model_from_query(query)
}

/// Parse the Codex Realtime v2 direct WebSocket query shape.
///
/// Realtime v2 intentionally has no `intent` marker.  The caller must use the
/// Codex originator header as the additional trust-boundary discriminator
/// before invoking this parser; this function only validates the query itself.
pub(super) fn direct_realtime_v2_model_from_query(
    query: Option<&str>,
) -> Result<String, LiveProtocolError> {
    for (name, _) in url::form_urlencoded::parse(query.unwrap_or_default().as_bytes()) {
        if name.eq_ignore_ascii_case("intent") {
            return Err(LiveProtocolError::InvalidLiveIntent);
        }
        // A call_id identifies a WebRTC sideband socket, not a standalone
        // v2 conversation.  Reject it here as well as at the session branch
        // so this parser cannot accidentally be reused as a direct route.
        if name.eq_ignore_ascii_case("call_id") {
            return Err(LiveProtocolError::InvalidModelQuery);
        }
    }
    direct_model_from_query(query)
}

/// Return whether the request carries a first-party Codex originator.
///
/// The default Codex CLI sends `codex_cli_rs` (optionally with a version),
/// while Desktop/Web/Mobile use their stable `codex_work_*` values.  Keep the
/// allowlist narrow so a normal OpenAI Realtime v2 socket is not routed into
/// the Codex Live planner merely because it has a `model` query parameter.
pub(super) fn is_codex_realtime_originator(headers: &HeaderMap) -> bool {
    let Some(originator) = headers
        .get("originator")
        .and_then(|value| value.to_str().ok())
    else {
        return false;
    };
    originator.split_whitespace().next().is_some_and(|value| {
        value.eq_ignore_ascii_case("codex_cli_rs")
            || value.to_ascii_lowercase().starts_with("codex_cli_rs/")
            || value.eq_ignore_ascii_case("codex_work_desktop")
            || value.eq_ignore_ascii_case("codex_work_web")
            || value.eq_ignore_ascii_case("codex_work_mobile")
    })
}

/// Query + header discriminator for a Codex Realtime v2 direct socket.
pub(super) fn realtime_v2_request_is_codex(query: Option<&str>, headers: &HeaderMap) -> bool {
    let mut has_model = false;
    for (name, value) in url::form_urlencoded::parse(query.unwrap_or_default().as_bytes()) {
        if name.eq_ignore_ascii_case("intent") || name.eq_ignore_ascii_case("call_id") {
            return false;
        }
        if name.eq_ignore_ascii_case("model") && !value.trim().is_empty() {
            has_model = true;
        }
    }
    has_model && is_codex_realtime_originator(headers)
}

fn live_query_parameter_is_sensitive(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        "key"
            | "api_key"
            | "api-key"
            | "x-api-key"
            | "x-goog-api-key"
            | "access_token"
            | "authorization"
            | "token"
            | "oauth_token"
            | "client_secret"
            | "secret_key"
            | "signature"
            | "sig"
    )
}

pub(super) fn call_id_from_path(path: &str) -> Result<String, LiveProtocolError> {
    let call_id = path
        .strip_prefix("/v1/live/")
        .ok_or(LiveProtocolError::InvalidCallId)?;
    if call_id.contains('/') {
        return Err(LiveProtocolError::InvalidCallId);
    }
    validate_call_id(call_id)?;
    Ok(call_id.to_string())
}

pub(super) fn realtime_sideband_query_has_call_id(query: Option<&str>) -> bool {
    url::form_urlencoded::parse(query.unwrap_or_default().as_bytes())
        .any(|(name, _)| name.eq_ignore_ascii_case("call_id"))
}

pub(super) fn sideband_call_from_request(
    path: &str,
    query: Option<&str>,
) -> Result<(LiveRouteDialect, String), LiveProtocolError> {
    if path != REALTIME_SIDEBAND_PATH {
        return call_id_from_path(path).map(|call_id| (LiveRouteDialect::LegacyLive, call_id));
    }

    let mut call_id = None;
    for (name, value) in url::form_urlencoded::parse(query.unwrap_or_default().as_bytes()) {
        if name.eq_ignore_ascii_case("call_id") {
            if call_id.is_some() {
                return Err(LiveProtocolError::InvalidCallId);
            }
            validate_call_id(value.as_ref())?;
            call_id = Some(value.into_owned());
            continue;
        }
        if live_query_parameter_is_sensitive(name.as_ref()) {
            return Err(LiveProtocolError::InvalidCallId);
        }
    }
    call_id
        .map(|call_id| (LiveRouteDialect::Realtime, call_id))
        .ok_or(LiveProtocolError::InvalidCallId)
}

pub(super) fn validate_initial_session_update(raw: &str) -> Result<(), LiveProtocolError> {
    if raw.len() > MAX_SESSION_BYTES {
        return Err(LiveProtocolError::SessionTooLarge);
    }
    let value: Value =
        serde_json::from_str(raw).map_err(|_| LiveProtocolError::InvalidInitialEvent)?;
    if !value.is_object() {
        return Err(LiveProtocolError::InvalidInitialEvent);
    }
    if value.get("type").and_then(Value::as_str) != Some("session.update") {
        return Err(LiveProtocolError::ExpectedSessionUpdate);
    }
    Ok(())
}

pub(super) fn event_type(raw: &str) -> Option<String> {
    serde_json::from_str::<Value>(raw)
        .ok()?
        .get("type")?
        .as_str()
        .map(str::to_string)
}

pub(super) fn parse_live_multipart(
    content_type: &str,
    body: &[u8],
) -> Result<LiveMultipart, LiveProtocolError> {
    if body.len() > MAX_MULTIPART_BODY_BYTES {
        return Err(LiveProtocolError::MultipartBodyTooLarge);
    }
    let boundary = multipart_boundary(content_type)?;
    let parts = parse_multipart_parts(body, boundary.as_bytes())?;
    let mut sdp = None;
    let mut session = None;
    for part in parts {
        match part.name.as_str() {
            "sdp" => {
                if sdp.is_some() {
                    return Err(LiveProtocolError::DuplicateMultipartPart);
                }
                if part.body.len() > MAX_SDP_BYTES {
                    return Err(LiveProtocolError::SdpTooLarge);
                }
                let value =
                    std::str::from_utf8(part.body).map_err(|_| LiveProtocolError::InvalidSdp)?;
                if value.trim().is_empty() {
                    return Err(LiveProtocolError::InvalidSdp);
                }
                sdp = Some(value.to_string());
            }
            "session" => {
                if session.is_some() {
                    return Err(LiveProtocolError::DuplicateMultipartPart);
                }
                if part.body.len() > MAX_SESSION_BYTES {
                    return Err(LiveProtocolError::SessionTooLarge);
                }
                let value: Value = serde_json::from_slice(part.body)
                    .map_err(|_| LiveProtocolError::InvalidSession)?;
                if !value.is_object() {
                    return Err(LiveProtocolError::InvalidSession);
                }
                session = Some(value);
            }
            _ => return Err(LiveProtocolError::UnexpectedMultipartPart),
        }
    }
    Ok(LiveMultipart {
        sdp: sdp.ok_or(LiveProtocolError::MissingSdp)?,
        session: session.ok_or(LiveProtocolError::MissingSession)?,
    })
}

pub(super) fn build_live_multipart(sdp: &str, session: &Value) -> (String, Vec<u8>) {
    let boundary = format!("aether-live-{}", uuid::Uuid::new_v4().simple());
    let session = serde_json::to_vec(session).expect("a JSON value must serialize");
    let mut body = Vec::with_capacity(sdp.len() + session.len() + 320);
    append_part(
        &mut body,
        boundary.as_str(),
        "sdp",
        "application/sdp",
        sdp.as_bytes(),
    );
    append_part(
        &mut body,
        boundary.as_str(),
        "session",
        "application/json",
        session.as_slice(),
    );
    body.extend_from_slice(format!("--{boundary}--\r\n").as_bytes());
    (format!("multipart/form-data; boundary={boundary}"), body)
}

pub(super) fn extract_call_id_from_location(location: &str) -> Result<String, LiveProtocolError> {
    let location = location.trim();
    if location.is_empty() {
        return Err(LiveProtocolError::InvalidCallLocation);
    }
    let path = if let Ok(url) = url::Url::parse(location) {
        if url.fragment().is_some() {
            return Err(LiveProtocolError::InvalidCallLocation);
        }
        url.path().to_string()
    } else {
        if !location.starts_with('/') || location.contains('#') {
            return Err(LiveProtocolError::InvalidCallLocation);
        }
        location
            .split_once('?')
            .map_or(location, |(path, _)| path)
            .to_string()
    };
    let segments = path
        .split('/')
        .filter(|segment| !segment.is_empty())
        .collect::<Vec<_>>();
    let (call_id, resource) = segments
        .split_last()
        .ok_or(LiveProtocolError::InvalidCallLocation)?;
    let valid_resource = matches!(
        resource,
        ["v1", "live"]
            | ["v1", "realtime", "calls"]
            | ["v1", "realtime", "calls", "calls"]
            | ["backend-api", "codex", "realtime", "calls"]
            | ["backend-api", "codex", "realtime", "calls", "calls"]
    );
    if !valid_resource {
        return Err(LiveProtocolError::InvalidCallLocation);
    }
    if !is_live_call_id_segment(call_id) {
        return Err(LiveProtocolError::InvalidCallLocation);
    }
    Ok(call_id.to_string())
}

struct MultipartPart<'a> {
    name: String,
    body: &'a [u8],
}

fn multipart_boundary(content_type: &str) -> Result<String, LiveProtocolError> {
    let mut values = content_type.split(';');
    if !values
        .next()
        .is_some_and(|value| value.trim().eq_ignore_ascii_case("multipart/form-data"))
    {
        return Err(LiveProtocolError::UnsupportedMediaType);
    }
    let mut boundary = None;
    for parameter in values {
        let Some((name, value)) = parameter.trim().split_once('=') else {
            continue;
        };
        if !name.trim().eq_ignore_ascii_case("boundary") {
            continue;
        }
        if boundary.is_some() {
            return Err(LiveProtocolError::InvalidBoundary);
        }
        let value = value.trim();
        let value = if value.starts_with('"') && value.ends_with('"') && value.len() >= 2 {
            &value[1..value.len() - 1]
        } else {
            value
        };
        boundary = Some(value.to_string());
    }
    let boundary = boundary.ok_or(LiveProtocolError::InvalidBoundary)?;
    if boundary.is_empty()
        || boundary.len() > MAX_BOUNDARY_BYTES
        || !boundary.bytes().all(|byte| {
            byte.is_ascii_alphanumeric()
                || matches!(
                    byte,
                    b'\''
                        | b'('
                        | b')'
                        | b'+'
                        | b'_'
                        | b','
                        | b'-'
                        | b'.'
                        | b'/'
                        | b':'
                        | b'='
                        | b'?'
                )
        })
    {
        return Err(LiveProtocolError::InvalidBoundary);
    }
    Ok(boundary)
}

fn parse_multipart_parts<'a>(
    body: &'a [u8],
    boundary: &[u8],
) -> Result<Vec<MultipartPart<'a>>, LiveProtocolError> {
    let delimiter = [b"--".as_slice(), boundary].concat();
    if !body.starts_with(delimiter.as_slice()) {
        return Err(LiveProtocolError::MalformedMultipart);
    }
    let mut cursor = delimiter.len();
    let mut parts = Vec::new();
    loop {
        if body.get(cursor..cursor + 2) == Some(b"--") {
            cursor += 2;
            if body
                .get(cursor..)
                .is_some_and(|tail| tail.is_empty() || tail == b"\r\n")
            {
                return Ok(parts);
            }
            return Err(LiveProtocolError::MalformedMultipart);
        }
        if body.get(cursor..cursor + 2) != Some(b"\r\n") {
            return Err(LiveProtocolError::MalformedMultipart);
        }
        cursor += 2;
        let header_end = find_bytes(&body[cursor..], b"\r\n\r\n")
            .ok_or(LiveProtocolError::MalformedMultipart)?;
        if header_end > MAX_PART_HEADERS_BYTES {
            return Err(LiveProtocolError::MalformedMultipart);
        }
        let headers = &body[cursor..cursor + header_end];
        cursor += header_end + 4;
        let marker = [b"\r\n--".as_slice(), boundary].concat();
        let body_end = find_bytes(&body[cursor..], marker.as_slice())
            .ok_or(LiveProtocolError::MalformedMultipart)?;
        let part_body = &body[cursor..cursor + body_end];
        let name = multipart_part_name(headers)?;
        parts.push(MultipartPart {
            name,
            body: part_body,
        });
        if parts.len() > 2 {
            return Err(LiveProtocolError::UnexpectedMultipartPart);
        }
        cursor += body_end + 2 + delimiter.len();
    }
}

fn multipart_part_name(headers: &[u8]) -> Result<String, LiveProtocolError> {
    let headers =
        std::str::from_utf8(headers).map_err(|_| LiveProtocolError::MalformedMultipart)?;
    let mut disposition = None;
    for line in headers.split("\r\n") {
        let Some((name, value)) = line.split_once(':') else {
            return Err(LiveProtocolError::MalformedMultipart);
        };
        if name.trim().eq_ignore_ascii_case("content-disposition") {
            if disposition.is_some() {
                return Err(LiveProtocolError::MalformedMultipart);
            }
            disposition = Some(value.trim());
        }
    }
    let disposition = disposition.ok_or(LiveProtocolError::MalformedMultipart)?;
    let mut parameters = disposition.split(';');
    if !parameters
        .next()
        .is_some_and(|value| value.trim().eq_ignore_ascii_case("form-data"))
    {
        return Err(LiveProtocolError::MalformedMultipart);
    }
    let mut part_name = None;
    for parameter in parameters {
        let Some((name, value)) = parameter.trim().split_once('=') else {
            return Err(LiveProtocolError::MalformedMultipart);
        };
        if name.trim().eq_ignore_ascii_case("filename") {
            return Err(LiveProtocolError::UnexpectedMultipartPart);
        }
        if name.trim().eq_ignore_ascii_case("name") {
            if part_name.is_some() {
                return Err(LiveProtocolError::MalformedMultipart);
            }
            let value = value.trim();
            if !(value.starts_with('"') && value.ends_with('"') && value.len() >= 2) {
                return Err(LiveProtocolError::MalformedMultipart);
            }
            part_name = Some(value[1..value.len() - 1].to_string());
        }
    }
    part_name.ok_or(LiveProtocolError::MalformedMultipart)
}

fn find_bytes(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    (!needle.is_empty())
        .then(|| {
            haystack
                .windows(needle.len())
                .position(|value| value == needle)
        })
        .flatten()
}

fn append_part(body: &mut Vec<u8>, boundary: &str, name: &str, content_type: &str, value: &[u8]) {
    body.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
    body.extend_from_slice(
        format!("Content-Disposition: form-data; name=\"{name}\"\r\n").as_bytes(),
    );
    body.extend_from_slice(format!("Content-Type: {content_type}\r\n\r\n").as_bytes());
    body.extend_from_slice(value);
    body.extend_from_slice(b"\r\n");
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn direct_query_requires_one_bounded_model() {
        assert_eq!(
            direct_model_from_query(Some("model=gpt-live%2Ffuture")).unwrap(),
            "gpt-live/future"
        );
        assert_eq!(
            direct_model_from_query(Some("foo=bar&model=gpt-live&trace=1")).unwrap(),
            "gpt-live"
        );
        assert_eq!(
            direct_model_from_query(Some("model=a&MODEL=b")),
            Err(LiveProtocolError::InvalidModelQuery)
        );
        assert_eq!(
            direct_model_from_query(Some("model=a&token=secret")),
            Err(LiveProtocolError::InvalidModelQuery)
        );
    }

    #[test]
    fn direct_realtime_query_requires_quicksilver_and_preserves_model_validation() {
        assert_eq!(
            direct_realtime_model_from_query(Some(
                "intent=quicksilver&model=gpt-realtime%2Ffuture&client=codex",
            ))
            .unwrap(),
            "gpt-realtime/future"
        );
        assert_eq!(
            direct_realtime_model_from_query(Some("INTENT=QUICKSILVER&model=gpt-live")).unwrap(),
            "gpt-live"
        );

        for query in [
            None,
            Some("model=gpt-live"),
            Some("intent=other&model=gpt-live"),
            Some("intent=quicksilver&intent=quicksilver&model=gpt-live"),
            Some("intent=quicksilver&intent=other&model=gpt-live"),
        ] {
            assert_eq!(
                direct_realtime_model_from_query(query),
                Err(LiveProtocolError::InvalidLiveIntent),
                "query should not select Codex Live: {query:?}"
            );
        }
        assert_eq!(
            direct_realtime_model_from_query(Some("intent=quicksilver&model=a&MODEL=b")),
            Err(LiveProtocolError::InvalidModelQuery)
        );
        assert_eq!(
            direct_realtime_model_from_query(Some(
                "intent=quicksilver&model=gpt-live&token=secret"
            )),
            Err(LiveProtocolError::InvalidModelQuery)
        );
    }

    #[test]
    fn realtime_call_create_requires_unique_quicksilver_avas_selectors() {
        assert_eq!(
            validate_realtime_call_create_query(Some(
                "intent=quicksilver&architecture=avas&future_hint=opaque"
            )),
            Ok(())
        );
        assert_eq!(
            validate_realtime_call_create_query(Some("architecture=avas")),
            Err(LiveProtocolError::InvalidLiveIntent)
        );
        for query in [
            "intent=other&architecture=avas",
            "intent=quicksilver&intent=quicksilver&architecture=avas",
        ] {
            assert_eq!(
                validate_realtime_call_create_query(Some(query)),
                Err(LiveProtocolError::InvalidLiveIntent)
            );
        }
        for query in [
            "intent=quicksilver",
            "intent=quicksilver&architecture=other",
            "intent=quicksilver&architecture=avas&ARCHITECTURE=avas",
        ] {
            assert_eq!(
                validate_realtime_call_create_query(Some(query)),
                Err(LiveProtocolError::InvalidLiveArchitecture)
            );
        }
        assert_eq!(
            validate_realtime_call_create_query(Some(
                "intent=quicksilver&architecture=avas&access_token=secret"
            )),
            Err(LiveProtocolError::InvalidModelQuery)
        );
    }

    #[test]
    fn direct_realtime_v2_query_omits_intent_and_rejects_sideband_selectors() {
        assert_eq!(
            direct_realtime_v2_model_from_query(Some("model=gpt-realtime%2Ffuture&client=codex"))
                .unwrap(),
            "gpt-realtime/future"
        );
        for query in [
            None,
            Some("intent=quicksilver&model=gpt-live"),
            Some("intent=other&model=gpt-live"),
            Some("model=gpt-live&call_id=rtc_1"),
            Some("model=a&MODEL=b"),
            Some("model=gpt-live&token=secret"),
        ] {
            assert!(
                direct_realtime_v2_model_from_query(query).is_err(),
                "v2 query should be rejected: {query:?}"
            );
        }
    }

    #[test]
    fn realtime_v2_request_requires_a_codex_originator() {
        let mut codex = HeaderMap::new();
        codex.insert("originator", "codex_work_desktop".parse().unwrap());
        assert!(realtime_v2_request_is_codex(
            Some("model=gpt-realtime-1.5"),
            &codex
        ));
        codex.insert("originator", "codex_cli_rs/0.145.2".parse().unwrap());
        assert!(realtime_v2_request_is_codex(
            Some("model=gpt-realtime-1.5"),
            &codex
        ));
        for (query, originator) in [
            ("model=gpt-realtime-1.5", "openai-python"),
            (
                "intent=quicksilver&model=gpt-realtime-1.5",
                "codex_work_desktop",
            ),
            ("model=gpt-realtime-1.5&call_id=rtc_1", "codex_work_desktop"),
            ("model=gpt-realtime-1.5", ""),
        ] {
            let mut headers = HeaderMap::new();
            if !originator.is_empty() {
                headers.insert("originator", originator.parse().unwrap());
            }
            assert!(!realtime_v2_request_is_codex(Some(query), &headers));
        }
    }

    #[test]
    fn direct_protocol_starts_with_session_update_not_response_create() {
        let opaque = r#"{"type":"session.update","session":{"future_capability":{"version":2}},"future_event_field":[1,2,3]}"#;
        validate_initial_session_update(opaque).unwrap();
        assert_eq!(
            serde_json::from_str::<Value>(opaque).unwrap()["future_event_field"],
            json!([1, 2, 3])
        );
        assert_eq!(
            validate_initial_session_update(r#"{"type":"response.create","model":"gpt"}"#),
            Err(LiveProtocolError::ExpectedSessionUpdate)
        );
        assert_eq!(
            validate_initial_session_update(r#"["session.update"]"#),
            Err(LiveProtocolError::InvalidInitialEvent)
        );
        assert_eq!(
            validate_initial_session_update("not-json"),
            Err(LiveProtocolError::InvalidInitialEvent)
        );
    }

    #[test]
    fn direct_session_update_uses_the_bounded_session_limit() {
        let oversized = format!(
            r#"{{"type":"session.update","session":{{"future":"{}"}}}}"#,
            "x".repeat(MAX_SESSION_BYTES)
        );
        assert_eq!(
            validate_initial_session_update(oversized.as_str()),
            Err(LiveProtocolError::SessionTooLarge)
        );
    }

    #[test]
    fn multipart_round_trip_preserves_unknown_session_fields() {
        let session = json!({
            "model": "gpt-future-live",
            "instructions": "opaque",
            "future_capability": {"enabled": true},
            "audio": {"input": {"format": "pcm16"}}
        });
        let (content_type, body) = build_live_multipart("v=0\r\no=test", &session);
        let parsed = parse_live_multipart(content_type.as_str(), body.as_slice()).unwrap();
        assert_eq!(parsed.sdp, "v=0\r\no=test");
        assert_eq!(parsed.session, session);
    }

    #[test]
    fn multipart_rejects_duplicate_or_unknown_parts() {
        let boundary = "test-boundary";
        let duplicate = format!(
            "--{boundary}\r\nContent-Disposition: form-data; name=\"sdp\"\r\n\r\nv=0\r\n--{boundary}\r\nContent-Disposition: form-data; name=\"sdp\"\r\n\r\nv=1\r\n--{boundary}--\r\n"
        );
        assert_eq!(
            parse_live_multipart(
                format!("multipart/form-data; boundary={boundary}").as_str(),
                duplicate.as_bytes(),
            ),
            Err(LiveProtocolError::DuplicateMultipartPart)
        );

        let unknown = format!(
            "--{boundary}\r\nContent-Disposition: form-data; name=\"sdp\"\r\n\r\nv=0\r\n--{boundary}\r\nContent-Disposition: form-data; name=\"credentials\"\r\n\r\nsecret\r\n--{boundary}--\r\n"
        );
        assert_eq!(
            parse_live_multipart(
                format!("multipart/form-data; boundary={boundary}").as_str(),
                unknown.as_bytes(),
            ),
            Err(LiveProtocolError::UnexpectedMultipartPart)
        );
    }

    #[test]
    fn multipart_enforces_total_sdp_and_session_limits() {
        let oversized_body = vec![b'x'; MAX_MULTIPART_BODY_BYTES + 1];
        assert_eq!(
            parse_live_multipart(
                "multipart/form-data; boundary=limit",
                oversized_body.as_slice(),
            ),
            Err(LiveProtocolError::MultipartBodyTooLarge)
        );

        let oversized_sdp = "x".repeat(MAX_SDP_BYTES + 1);
        let sdp_body = format!(
            "--limit\r\nContent-Disposition: form-data; name=\"sdp\"\r\n\r\n{oversized_sdp}\r\n--limit\r\nContent-Disposition: form-data; name=\"session\"\r\n\r\n{{}}\r\n--limit--\r\n"
        );
        assert_eq!(
            parse_live_multipart("multipart/form-data; boundary=limit", sdp_body.as_bytes()),
            Err(LiveProtocolError::SdpTooLarge)
        );

        let oversized_session = format!(r#"{{"future":"{}"}}"#, "x".repeat(MAX_SESSION_BYTES));
        let session_body = format!(
            "--limit\r\nContent-Disposition: form-data; name=\"sdp\"\r\n\r\nv=0\r\n--limit\r\nContent-Disposition: form-data; name=\"session\"\r\n\r\n{oversized_session}\r\n--limit--\r\n"
        );
        assert_eq!(
            parse_live_multipart(
                "multipart/form-data; boundary=limit",
                session_body.as_bytes(),
            ),
            Err(LiveProtocolError::SessionTooLarge)
        );
    }

    #[test]
    fn multipart_rejects_unbounded_or_ambiguous_boundaries() {
        let valid_body =
            b"--safe\r\nContent-Disposition: form-data; name=\"sdp\"\r\n\r\nv=0\r\n--safe--\r\n";
        assert_eq!(
            parse_live_multipart("application/json", valid_body),
            Err(LiveProtocolError::UnsupportedMediaType)
        );
        assert_eq!(
            parse_live_multipart(
                format!(
                    "multipart/form-data; boundary={}",
                    "x".repeat(MAX_BOUNDARY_BYTES + 1)
                )
                .as_str(),
                valid_body,
            ),
            Err(LiveProtocolError::InvalidBoundary)
        );
        assert_eq!(
            parse_live_multipart(
                "multipart/form-data; boundary=safe; boundary=other",
                valid_body,
            ),
            Err(LiveProtocolError::InvalidBoundary)
        );
    }

    #[test]
    fn extracts_only_realtime_call_ids_from_location() {
        for location in [
            "https://api.openai.com/v1/live/rtc_abc-123",
            "/v1/live/550e8400-e29b-41d4-a716-446655440000",
            "/v1/realtime/calls/rtc_current",
            "https://chatgpt.com/backend-api/codex/realtime/calls/rtc_backend",
            "/v1/realtime/calls/calls/rtc_forwarded",
        ] {
            assert!(extract_call_id_from_location(location).is_ok());
        }
        assert_eq!(
            extract_call_id_from_location("/v1/live/rtc%2Fescape"),
            Err(LiveProtocolError::InvalidCallLocation)
        );
        for location in [
            "/v1/live",
            "/v1/live/not-a-call-id",
            "/unrelated/rtc_opaque",
            "?call_id=rtc_query_only",
            "/v1/realtime/calls/rtc_valid#fragment",
        ] {
            assert_eq!(
                extract_call_id_from_location(location),
                Err(LiveProtocolError::InvalidCallLocation)
            );
        }
        for dot_segment in [".", ".."] {
            assert_eq!(
                validate_call_id(dot_segment),
                Err(LiveProtocolError::InvalidCallId)
            );
        }
    }

    #[test]
    fn recognizes_call_create_and_sideband_route_dialects() {
        assert_eq!(
            LiveRouteDialect::from_call_create_path("/v1/live"),
            Some(LiveRouteDialect::LegacyLive)
        );
        assert_eq!(
            LiveRouteDialect::from_call_create_path("/v1/realtime/calls"),
            Some(LiveRouteDialect::Realtime)
        );
        assert_eq!(
            LiveRouteDialect::from_call_create_path("/v1/realtime"),
            None
        );
        assert_eq!(
            sideband_call_from_request("/v1/live/rtc_legacy", None),
            Ok((LiveRouteDialect::LegacyLive, "rtc_legacy".to_string()))
        );
        assert_eq!(
            sideband_call_from_request(
                "/v1/realtime",
                Some("intent=quicksilver&call_id=rtc_current")
            ),
            Ok((LiveRouteDialect::Realtime, "rtc_current".to_string()))
        );
    }

    #[test]
    fn realtime_sideband_query_rejects_ambiguous_or_sensitive_call_ids() {
        assert!(realtime_sideband_query_has_call_id(Some("call_id=")));
        assert!(realtime_sideband_query_has_call_id(Some(
            "CALL_ID=rtc_one&call_id=rtc_two"
        )));
        for query in [
            "call_id=",
            "call_id=rtc_one&call_id=rtc_two",
            "call_id=rtc_valid&token=secret",
            "model=gpt-realtime",
        ] {
            assert_eq!(
                sideband_call_from_request("/v1/realtime", Some(query)),
                Err(LiveProtocolError::InvalidCallId)
            );
        }
    }

    #[test]
    fn opaque_event_discriminator_does_not_project_unknown_fields() {
        let raw = r#"{"type":"delegation.created","unknown":{"nested":[1,2,3]}}"#;
        assert_eq!(event_type(raw).as_deref(), Some("delegation.created"));
        assert_eq!(
            serde_json::from_str::<Value>(raw).unwrap()["unknown"]["nested"][2],
            3
        );
    }
}
