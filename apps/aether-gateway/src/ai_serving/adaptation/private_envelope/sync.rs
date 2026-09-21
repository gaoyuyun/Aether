use aether_usage_runtime::decode_internal_report_body_base64;
use base64::Engine as _;
use serde_json::Value;

use crate::{usage::GatewaySyncReportRequest, GatewayError};

use super::{
    maybe_build_provider_private_stream_normalizer, normalize_provider_private_report_context,
    normalize_provider_private_response_value, provider_private_response_allows_sync_finalize,
    stream_body_contains_error_event, ProviderPrivateStreamNormalizer,
};

pub(crate) fn maybe_normalize_provider_private_sync_report_payload(
    payload: &GatewaySyncReportRequest,
) -> Result<Option<GatewaySyncReportRequest>, GatewayError> {
    let Some(report_context) = payload.report_context.as_ref() else {
        return Ok(Some(payload.clone()));
    };
    if !report_context
        .get("has_envelope")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        return Ok(Some(payload.clone()));
    }
    if !provider_private_response_allows_sync_finalize(report_context) {
        return Ok(None);
    }

    let mut normalized = payload.clone();
    normalized.report_context = normalize_provider_private_report_context(Some(report_context));
    if let (Some(body_json), Some(context)) = (
        payload.body_json.as_ref(),
        normalized.report_context.as_mut(),
    ) {
        maybe_attach_gemini_cli_v1internal_credits_context(report_context, body_json, context);
    }

    if let Some(body_json) = payload.body_json.clone() {
        normalized.body_json = normalize_provider_private_response_value(body_json, report_context);
        if normalized.body_json.is_none() {
            return Ok(None);
        }
    }

    if let Some(body_base64) = payload.body_base64.as_deref() {
        let body_bytes =
            decode_internal_report_body_base64(body_base64).map_err(GatewayError::Internal)?;
        let Some(normalized_bytes) =
            normalize_provider_private_stream_bytes(report_context, &body_bytes)?
        else {
            return Ok(None);
        };
        if stream_body_contains_error_event(&normalized_bytes) {
            return Ok(None);
        }
        normalized.body_base64 = (!normalized_bytes.is_empty())
            .then(|| base64::engine::general_purpose::STANDARD.encode(normalized_bytes));
    }

    Ok(Some(normalized))
}

fn maybe_attach_gemini_cli_v1internal_credits_context(
    original_report_context: &Value,
    body_json: &Value,
    normalized_report_context: &mut Value,
) {
    if !original_report_context
        .get("envelope_name")
        .and_then(Value::as_str)
        .is_some_and(|value| value.eq_ignore_ascii_case("gemini_cli:v1internal"))
    {
        return;
    }

    let mut credits = serde_json::Map::new();
    for (source, target) in [
        ("remainingCredits", "remainingCredits"),
        ("consumedCredits", "consumedCredits"),
        ("traceId", "traceId"),
    ] {
        if let Some(value) = body_json
            .get(source)
            .cloned()
            .filter(|value| !value.is_null())
        {
            credits.insert(target.to_string(), value);
        }
    }
    if credits.is_empty() {
        return;
    }
    if let Some(object) = normalized_report_context.as_object_mut() {
        object.insert(
            "gemini_cli_v1internal_credits".to_string(),
            Value::Object(credits),
        );
    }
}

fn normalize_provider_private_stream_bytes(
    report_context: &Value,
    body: &[u8],
) -> Result<Option<Vec<u8>>, GatewayError> {
    let Some(mut normalizer): Option<ProviderPrivateStreamNormalizer<'_>> =
        maybe_build_provider_private_stream_normalizer(Some(report_context))
    else {
        return Ok(Some(body.to_vec()));
    };
    let mut normalized = normalizer.push_chunk(body).map_err(GatewayError::from)?;
    normalized.extend(normalizer.finish().map_err(GatewayError::from)?);
    Ok(Some(normalized))
}

#[cfg(test)]
mod tests {
    use base64::Engine as _;
    use serde_json::{json, Value};

    use super::maybe_normalize_provider_private_sync_report_payload;
    use crate::usage::GatewaySyncReportRequest;

    fn sample_payload(
        report_context: Option<Value>,
        body_json: Option<Value>,
    ) -> GatewaySyncReportRequest {
        GatewaySyncReportRequest {
            trace_id: "trace-sync-1".to_string(),
            report_kind: "sync".to_string(),
            report_context,
            status_code: 200,
            headers: Default::default(),
            body_json,
            client_body_json: None,
            body_base64: None,
            telemetry: None,
        }
    }

    fn gemini_cli_context() -> Value {
        json!({
            "has_envelope": true,
            "envelope_name": "gemini_cli:v1internal",
            "provider_api_format": "gemini:generate_content",
            "mapped_model": "gemini-2.5-pro"
        })
    }

    fn encode(body: &str) -> String {
        base64::engine::general_purpose::STANDARD.encode(body.as_bytes())
    }

    #[test]
    fn payloads_without_an_envelope_pass_through_unchanged() {
        let payload = sample_payload(None, Some(json!({ "candidates": [] })));
        let normalized = maybe_normalize_provider_private_sync_report_payload(&payload)
            .expect("normalization should succeed")
            .expect("payload should be kept");
        assert_eq!(normalized.body_json, payload.body_json);
        assert_eq!(normalized.report_context, None);

        let payload = sample_payload(
            Some(json!({ "has_envelope": false, "provider_api_format": "openai:chat" })),
            Some(json!({ "choices": [] })),
        );
        let normalized = maybe_normalize_provider_private_sync_report_payload(&payload)
            .expect("normalization should succeed")
            .expect("payload should be kept");
        assert_eq!(normalized.body_json, payload.body_json);
        assert_eq!(normalized.report_context, payload.report_context);
    }

    #[test]
    fn gemini_cli_envelopes_are_unwrapped_and_credits_are_attached_to_the_context() {
        let payload = sample_payload(
            Some(gemini_cli_context()),
            Some(json!({
                "response": {
                    "candidates": [{ "content": { "parts": [{ "text": "hi" }] } }],
                    "usageMetadata": { "totalTokenCount": 3 }
                },
                "remainingCredits": 41.5,
                "consumedCredits": 0.5,
                "traceId": "upstream-trace",
                "ignoredNull": null
            })),
        );

        let normalized = maybe_normalize_provider_private_sync_report_payload(&payload)
            .expect("normalization should succeed")
            .expect("payload should be kept");

        let body = normalized.body_json.expect("body");
        assert_eq!(body["candidates"][0]["content"]["parts"][0]["text"], "hi");
        assert!(body.get("response").is_none());
        assert!(body.get("remainingCredits").is_none());

        let context = normalized.report_context.expect("report context");
        assert_eq!(context["has_envelope"], false);
        assert!(context.get("envelope_name").is_none());
        assert_eq!(context["mapped_model"], "gemini-2.5-pro");
        assert_eq!(
            context["gemini_cli_v1internal_credits"],
            json!({
                "remainingCredits": 41.5,
                "consumedCredits": 0.5,
                "traceId": "upstream-trace"
            })
        );
    }

    #[test]
    fn credits_context_is_omitted_when_the_envelope_carries_none() {
        let payload = sample_payload(
            Some(gemini_cli_context()),
            Some(json!({ "response": { "candidates": [] } })),
        );

        let normalized = maybe_normalize_provider_private_sync_report_payload(&payload)
            .expect("normalization should succeed")
            .expect("payload should be kept");

        let context = normalized.report_context.expect("report context");
        assert!(context.get("gemini_cli_v1internal_credits").is_none());
    }

    #[test]
    fn envelopes_that_cannot_finalize_synchronously_are_dropped() {
        let payload = sample_payload(
            Some(json!({
                "has_envelope": true,
                "envelope_name": "unknown:envelope",
                "provider_api_format": "gemini:generate_content"
            })),
            Some(json!({ "response": {} })),
        );

        assert!(
            maybe_normalize_provider_private_sync_report_payload(&payload)
                .expect("normalization should succeed")
                .is_none()
        );
    }

    #[test]
    fn client_side_private_envelopes_are_preserved_verbatim() {
        let mut context = gemini_cli_context();
        context["client_envelope_name"] = json!("gemini_cli:v1internal");
        let body = json!({ "response": { "candidates": [] }, "traceId": "keep-me" });
        let payload = sample_payload(Some(context.clone()), Some(body.clone()));

        let normalized = maybe_normalize_provider_private_sync_report_payload(&payload)
            .expect("normalization should succeed")
            .expect("payload should be kept");

        // 客户端自己就是 v1internal 信封：正文原样保留、信封标记不清除，只补记额度事实。
        assert_eq!(normalized.body_json, Some(body));
        let normalized_context = normalized.report_context.expect("report context");
        assert_eq!(normalized_context["has_envelope"], true);
        assert_eq!(normalized_context["envelope_name"], "gemini_cli:v1internal");
        assert_eq!(
            normalized_context["client_envelope_name"],
            "gemini_cli:v1internal"
        );
        assert_eq!(
            normalized_context["gemini_cli_v1internal_credits"],
            json!({ "traceId": "keep-me" })
        );
    }

    #[test]
    fn stream_bodies_are_unwrapped_line_by_line() {
        let mut payload = sample_payload(Some(gemini_cli_context()), None);
        payload.body_base64 = Some(encode(
            "data: {\"response\":{\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"a\"}]}}]}}\n\ndata: {\"response\":{\"candidates\":[{\"finishReason\":\"STOP\"}]}}\n\n",
        ));

        let normalized = maybe_normalize_provider_private_sync_report_payload(&payload)
            .expect("normalization should succeed")
            .expect("payload should be kept");

        let bytes = base64::engine::general_purpose::STANDARD
            .decode(normalized.body_base64.expect("stream body"))
            .expect("stream body should decode");
        let text = String::from_utf8(bytes).expect("utf-8");
        assert!(
            text.contains("data: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"a\"}]}}]}"),
            "{text}"
        );
        assert!(!text.contains("\"response\""), "{text}");
    }

    #[test]
    fn stream_bodies_with_an_error_event_are_dropped() {
        let mut payload = sample_payload(Some(gemini_cli_context()), None);
        payload.body_base64 = Some(encode(
            "event: error\ndata: {\"error\":{\"code\":429,\"message\":\"quota\"}}\n\n",
        ));

        assert!(
            maybe_normalize_provider_private_sync_report_payload(&payload)
                .expect("normalization should succeed")
                .is_none()
        );
    }

    #[test]
    fn invalid_base64_stream_bodies_are_reported_as_errors() {
        let mut payload = sample_payload(Some(gemini_cli_context()), None);
        payload.body_base64 = Some("%%%not-base64%%%".to_string());

        assert!(maybe_normalize_provider_private_sync_report_payload(&payload).is_err());
    }
}
