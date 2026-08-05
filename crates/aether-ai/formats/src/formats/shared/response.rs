use std::collections::BTreeMap;

use serde_json::Value;

use crate::contracts::core_success_background_report_kind;
use crate::formats::shared::request::UPSTREAM_IS_STREAM_KEY;

pub fn generation_api_format_requires_visible_output(api_format: &str) -> bool {
    matches!(
        crate::normalize_api_format_alias(api_format).as_str(),
        "openai:chat"
            | "openai:responses"
            | "openai:responses:compact"
            | "openai:search"
            | "claude:messages"
            | "gemini:generate_content"
    )
}

pub fn generation_response_has_visible_output(api_format: &str, body: &Value) -> Option<bool> {
    match crate::normalize_api_format_alias(api_format).as_str() {
        "openai:chat" => Some(openai_chat_response_has_visible_output(body)),
        "openai:responses" | "openai:responses:compact" => {
            Some(openai_responses_body_has_visible_output(body))
        }
        "openai:search" => Some(
            body.get("output")
                .and_then(Value::as_str)
                .is_some_and(|value| !value.trim().is_empty()),
        ),
        "claude:messages" => Some(claude_message_has_visible_output(body)),
        "gemini:generate_content" => Some(
            crate::formats::gemini::generate_content::response::from_raw(body).is_some()
                || openai_chat_response_has_visible_output(body)
                || openai_responses_body_has_visible_output(body),
        ),
        _ => None,
    }
}

fn openai_chat_response_has_visible_output(body: &Value) -> bool {
    body.get("choices")
        .and_then(Value::as_array)
        .is_some_and(|choices| {
            choices.iter().any(|choice| {
                choice
                    .get("message")
                    .or_else(|| choice.get("delta"))
                    .is_some_and(openai_message_has_visible_output)
                    || value_has_non_empty_text(choice.get("text"))
            })
        })
}

fn openai_message_has_visible_output(message: &Value) -> bool {
    value_has_non_empty_text(message.get("content"))
        || value_has_non_empty_text(message.get("refusal"))
        || message
            .get("tool_calls")
            .and_then(Value::as_array)
            .is_some_and(|items| !items.is_empty())
        || message
            .get("function_call")
            .is_some_and(non_empty_structured_value)
        || message.get("audio").is_some_and(non_empty_structured_value)
}

fn openai_responses_body_has_visible_output(body: &Value) -> bool {
    value_has_non_empty_text(body.get("output_text"))
        || body
            .get("output")
            .and_then(Value::as_array)
            .is_some_and(|items| items.iter().any(openai_responses_item_has_visible_output))
}

fn openai_responses_item_has_visible_output(item: &Value) -> bool {
    let item_type = item.get("type").and_then(Value::as_str).unwrap_or_default();
    if item_type == "reasoning" {
        return false;
    }
    if matches!(
        item_type,
        "function_call"
            | "custom_tool_call"
            | "image_generation_call"
            | "computer_call"
            | "file_search_call"
            | "web_search_call"
            | "code_interpreter_call"
            | "local_shell_call"
            | "shell_call"
            | "apply_patch_call"
            | "mcp_call"
    ) {
        return true;
    }
    item.get("content")
        .and_then(Value::as_array)
        .is_some_and(|content| {
            content
                .iter()
                .any(openai_responses_content_has_visible_output)
        })
        || value_has_non_empty_text(item.get("output"))
        || (item_type != "message"
            && !item_type.is_empty()
            && item.as_object().is_some_and(|object| object.len() > 1))
}

fn openai_responses_content_has_visible_output(content: &Value) -> bool {
    value_has_non_empty_text(content.get("text"))
        || value_has_non_empty_text(content.get("refusal"))
        || content
            .get("type")
            .and_then(Value::as_str)
            .is_some_and(|kind| matches!(kind, "output_image" | "output_audio" | "output_file"))
}

fn claude_message_has_visible_output(body: &Value) -> bool {
    match body.get("content") {
        Some(Value::String(text)) => !text.trim().is_empty(),
        Some(Value::Array(content)) => content.iter().any(|block| {
            let Some(object) = block.as_object() else {
                return false;
            };
            match object.get("type").and_then(Value::as_str) {
                Some("text") => value_has_non_empty_text(object.get("text")),
                Some("thinking" | "redacted_thinking") | None => false,
                Some(_) => object.len() > 1,
            }
        }),
        _ => false,
    }
}

fn value_has_non_empty_text(value: Option<&Value>) -> bool {
    match value {
        Some(Value::String(text)) => !text.trim().is_empty(),
        Some(Value::Array(items)) => items.iter().any(|item| match item {
            Value::String(text) => !text.trim().is_empty(),
            Value::Object(object) => {
                value_has_non_empty_text(object.get("text"))
                    || value_has_non_empty_text(object.get("content"))
                    || value_has_non_empty_text(object.get("refusal"))
                    || object
                        .get("type")
                        .and_then(Value::as_str)
                        .is_some_and(|kind| {
                            matches!(
                                kind,
                                "image_url"
                                    | "input_image"
                                    | "output_image"
                                    | "input_audio"
                                    | "output_audio"
                                    | "file"
                            )
                        })
            }
            _ => false,
        }),
        _ => false,
    }
}

fn non_empty_structured_value(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::String(text) => !text.trim().is_empty(),
        Value::Array(items) => !items.is_empty(),
        Value::Object(object) => !object.is_empty(),
        Value::Bool(_) | Value::Number(_) => true,
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct LocalSyncReportParts {
    pub trace_id: String,
    pub report_kind: String,
    pub report_context: Option<Value>,
    pub status_code: u16,
    pub headers: BTreeMap<String, String>,
    pub body_json: Option<Value>,
    pub client_body_json: Option<Value>,
    pub body_base64: Option<String>,
}

pub fn build_generated_tool_call_id(index: usize) -> String {
    format!("call_auto_{index}")
}

pub fn canonicalize_tool_arguments(value: Option<Value>) -> String {
    match value {
        Some(Value::String(text)) => text,
        Some(other) => serde_json::to_string(&other).unwrap_or_else(|_| "null".to_string()),
        None => "{}".to_string(),
    }
}

pub fn remove_empty_pages_from_tool_arguments(tool_name: &str, arguments: &str) -> String {
    if tool_name != "Read" {
        return arguments.to_string();
    }
    let Ok(mut value) = serde_json::from_str::<Value>(arguments) else {
        return arguments.to_string();
    };
    let Some(object) = value.as_object_mut() else {
        return arguments.to_string();
    };
    if object.get("pages").and_then(Value::as_str) != Some("") {
        return arguments.to_string();
    }
    object.remove("pages");
    serde_json::to_string(&value).unwrap_or_else(|_| arguments.to_string())
}

pub fn remove_empty_pages_from_tool_input_value(tool_name: &str, input: &Value) -> Value {
    if tool_name != "Read" || input.get("pages").and_then(Value::as_str) != Some("") {
        return input.clone();
    }
    let Some(object) = input.as_object() else {
        return input.clone();
    };
    let mut object = object.clone();
    object.remove("pages");
    Value::Object(object)
}

pub fn sanitize_claude_read_tool_inputs(value: &mut Value) -> bool {
    let Some(content) = value.get_mut("content").and_then(Value::as_array_mut) else {
        return false;
    };
    let mut changed = false;
    for block in content {
        let Some(block_object) = block.as_object_mut() else {
            continue;
        };
        if block_object.get("type").and_then(Value::as_str) != Some("tool_use")
            || block_object.get("name").and_then(Value::as_str) != Some("Read")
        {
            continue;
        }
        let Some(input) = block_object.get("input") else {
            continue;
        };
        let sanitized = remove_empty_pages_from_tool_input_value("Read", input);
        if sanitized != *input {
            block_object.insert("input".to_string(), sanitized);
            changed = true;
        }
    }
    changed
}

pub fn prepare_local_success_response_parts(
    headers: &BTreeMap<String, String>,
    body_json: &Value,
) -> serde_json::Result<(Vec<u8>, BTreeMap<String, String>)> {
    prepare_local_success_response_parts_owned(headers.clone(), body_json)
}

pub fn prepare_local_success_response_parts_owned(
    mut headers: BTreeMap<String, String>,
    body_json: &Value,
) -> serde_json::Result<(Vec<u8>, BTreeMap<String, String>)> {
    headers.remove("content-encoding");
    headers.remove("content-length");
    headers.insert("content-type".to_string(), "application/json".to_string());
    let body_bytes = serde_json::to_vec(body_json)?;
    headers.insert("content-length".to_string(), body_bytes.len().to_string());
    Ok((body_bytes, headers))
}

fn should_capture_client_sync_success_body(payload: &LocalSyncReportParts) -> bool {
    payload
        .report_context
        .as_ref()
        .and_then(Value::as_object)
        .and_then(|context| context.get(UPSTREAM_IS_STREAM_KEY))
        .and_then(Value::as_bool)
        .unwrap_or(false)
}

pub fn build_local_success_background_report(
    payload: &LocalSyncReportParts,
    body_json: Value,
    headers: BTreeMap<String, String>,
) -> Option<LocalSyncReportParts> {
    let report_kind = core_success_background_report_kind(payload.report_kind.as_str())?;
    let upstream_is_stream = should_capture_client_sync_success_body(payload);
    let client_body_json = upstream_is_stream.then(|| body_json.clone());
    let provider_body_json = if upstream_is_stream {
        payload.body_json.clone()
    } else {
        Some(body_json)
    };
    let provider_body_base64 = if upstream_is_stream {
        payload.body_base64.clone()
    } else {
        None
    };

    Some(LocalSyncReportParts {
        trace_id: payload.trace_id.clone(),
        report_kind: report_kind.to_string(),
        report_context: payload.report_context.clone(),
        status_code: payload.status_code,
        headers,
        body_json: provider_body_json,
        client_body_json,
        body_base64: provider_body_base64,
    })
}

pub fn build_local_success_conversion_background_report(
    payload: &LocalSyncReportParts,
    client_body_json: Value,
    provider_body_json: Value,
) -> Option<LocalSyncReportParts> {
    let report_kind = core_success_background_report_kind(payload.report_kind.as_str())?;

    Some(LocalSyncReportParts {
        trace_id: payload.trace_id.clone(),
        report_kind: report_kind.to_string(),
        report_context: payload.report_context.clone(),
        status_code: payload.status_code,
        headers: payload.headers.clone(),
        body_json: Some(provider_body_json),
        client_body_json: Some(client_body_json),
        body_base64: None,
    })
}

#[cfg(test)]
mod tests {
    use base64::Engine as _;
    use serde_json::Value;

    use super::{
        build_generated_tool_call_id, build_local_success_background_report,
        build_local_success_conversion_background_report, canonicalize_tool_arguments,
        generation_api_format_requires_visible_output, generation_response_has_visible_output,
        prepare_local_success_response_parts, prepare_local_success_response_parts_owned,
        remove_empty_pages_from_tool_arguments, sanitize_claude_read_tool_inputs,
        LocalSyncReportParts,
    };
    use std::collections::BTreeMap;

    #[test]
    fn generated_tool_call_ids_are_stable() {
        assert_eq!(build_generated_tool_call_id(3), "call_auto_3");
    }

    #[test]
    fn generation_response_visibility_rejects_empty_standard_successes() {
        let cases = [
            (
                "openai:chat",
                serde_json::json!({
                    "choices": [{
                        "message": {"role": "assistant", "content": "  "},
                        "finish_reason": "stop"
                    }],
                    "usage": {"completion_tokens": 0}
                }),
            ),
            (
                "openai:responses",
                serde_json::json!({
                    "status": "completed",
                    "output": [{"type": "reasoning", "summary": []}],
                    "usage": {"output_tokens": 0}
                }),
            ),
            (
                "claude:messages",
                serde_json::json!({
                    "type": "message",
                    "content": [{"type": "thinking", "thinking": "internal only"}],
                    "stop_reason": "end_turn"
                }),
            ),
            (
                "gemini:generate_content",
                serde_json::json!({
                    "candidates": [{"content": {"role": "model"}, "finishReason": "STOP"}],
                    "usageMetadata": {"candidatesTokenCount": 0}
                }),
            ),
            ("openai:search", serde_json::json!({"output": "  "})),
        ];

        for (api_format, body) in cases {
            assert!(generation_api_format_requires_visible_output(api_format));
            assert_eq!(
                generation_response_has_visible_output(api_format, &body),
                Some(false),
                "{api_format} should reject an empty success response"
            );
        }
    }

    #[test]
    fn generation_response_visibility_accepts_text_refusals_tools_and_media() {
        let cases = [
            (
                "openai:chat",
                serde_json::json!({"choices": [{"message": {"refusal": "Cannot comply"}}]}),
            ),
            (
                "openai:responses:compact",
                serde_json::json!({"output": [{"type": "function_call", "name": "lookup"}]}),
            ),
            (
                "openai:responses",
                serde_json::json!({"output_text": "response text"}),
            ),
            (
                "claude:messages",
                serde_json::json!({"content": [{"type": "tool_use", "name": "lookup"}]}),
            ),
            (
                "gemini:generate_content",
                serde_json::json!({
                    "candidates": [{
                        "content": {"role": "model", "parts": [{"text": "hello"}]},
                        "finishReason": "STOP"
                    }]
                }),
            ),
            ("openai:search", serde_json::json!({"output": "result"})),
        ];

        for (api_format, body) in cases {
            assert_eq!(
                generation_response_has_visible_output(api_format, &body),
                Some(true),
                "{api_format} should accept visible output"
            );
        }

        assert_eq!(
            generation_response_has_visible_output("openai:embedding", &serde_json::json!({})),
            None
        );
    }

    #[test]
    fn canonicalizes_tool_arguments() {
        assert_eq!(
            canonicalize_tool_arguments(Some(serde_json::json!({"x": 1}))),
            "{\"x\":1}"
        );
        assert_eq!(canonicalize_tool_arguments(None), "{}");
    }

    #[test]
    fn removes_empty_pages_from_tool_arguments() {
        assert_eq!(
            remove_empty_pages_from_tool_arguments(
                "Read",
                r#"{"file_path":"/tmp/a.txt","offset":1,"limit":20,"pages":""}"#
            ),
            r#"{"file_path":"/tmp/a.txt","offset":1,"limit":20}"#
        );
        assert_eq!(
            remove_empty_pages_from_tool_arguments("Search", r#"{"query":"","pages":""}"#),
            r#"{"query":"","pages":""}"#
        );
        assert_eq!(
            remove_empty_pages_from_tool_arguments("Read", r#"{"pages":"1-2"}"#),
            r#"{"pages":"1-2"}"#
        );
        assert_eq!(
            remove_empty_pages_from_tool_arguments("Read", r#"{"pages":"#),
            r#"{"pages":"#
        );
    }

    #[test]
    fn sanitizes_claude_read_tool_inputs_only() {
        let mut value = serde_json::json!({
            "content": [
                {
                    "type": "tool_use",
                    "name": "Read",
                    "input": {
                        "file_path": "/tmp/a.txt",
                        "limit": 20,
                        "pages": ""
                    }
                },
                {
                    "type": "tool_use",
                    "name": "Search",
                    "input": {
                        "query": "",
                        "pages": ""
                    }
                },
                {
                    "type": "tool_use",
                    "name": "Read",
                    "input": {
                        "pages": "1-2"
                    }
                }
            ]
        });

        assert!(sanitize_claude_read_tool_inputs(&mut value));
        assert_eq!(
            value["content"][0]["input"],
            serde_json::json!({
                "file_path": "/tmp/a.txt",
                "limit": 20,
            })
        );
        assert_eq!(
            value["content"][1]["input"],
            serde_json::json!({
                "query": "",
                "pages": "",
            })
        );
        assert_eq!(
            value["content"][2]["input"],
            serde_json::json!({"pages": "1-2"})
        );
    }

    #[test]
    fn prepare_local_success_response_parts_normalizes_headers() {
        let headers = BTreeMap::from([
            ("content-encoding".to_string(), "gzip".to_string()),
            ("content-length".to_string(), "999".to_string()),
            ("x-test".to_string(), "1".to_string()),
        ]);
        let (body_bytes, normalized_headers) =
            prepare_local_success_response_parts(&headers, &serde_json::json!({"ok": true}))
                .expect("response parts should serialize");

        assert_eq!(
            serde_json::from_slice::<Value>(&body_bytes).expect("json body"),
            serde_json::json!({"ok": true})
        );
        assert_eq!(
            normalized_headers.get("content-type").map(String::as_str),
            Some("application/json")
        );
        assert!(!normalized_headers.contains_key("content-encoding"));
        let expected_length = body_bytes.len().to_string();
        assert_eq!(
            normalized_headers.get("content-length").map(String::as_str),
            Some(expected_length.as_str())
        );
        assert_eq!(
            normalized_headers.get("x-test").map(String::as_str),
            Some("1")
        );
    }

    #[test]
    fn prepare_local_success_response_parts_owned_normalizes_headers() {
        let headers = BTreeMap::from([
            ("content-encoding".to_string(), "gzip".to_string()),
            ("content-length".to_string(), "999".to_string()),
            ("x-test".to_string(), "1".to_string()),
        ]);
        let (body_bytes, normalized_headers) =
            prepare_local_success_response_parts_owned(headers, &serde_json::json!({"ok": true}))
                .expect("response parts should serialize");

        assert_eq!(
            serde_json::from_slice::<Value>(&body_bytes).expect("json body"),
            serde_json::json!({"ok": true})
        );
        assert_eq!(
            normalized_headers.get("content-type").map(String::as_str),
            Some("application/json")
        );
        assert!(!normalized_headers.contains_key("content-encoding"));
        let expected_length = body_bytes.len().to_string();
        assert_eq!(
            normalized_headers.get("content-length").map(String::as_str),
            Some(expected_length.as_str())
        );
        assert_eq!(
            normalized_headers.get("x-test").map(String::as_str),
            Some("1")
        );
    }

    #[test]
    fn build_local_success_background_report_maps_finalize_kind() {
        let payload = LocalSyncReportParts {
            trace_id: "trace-1".to_string(),
            report_kind: "openai_chat_sync_finalize".to_string(),
            report_context: Some(serde_json::json!({"request_id": "req-1"})),
            status_code: 200,
            headers: BTreeMap::from([("x-test".to_string(), "1".to_string())]),
            body_json: None,
            client_body_json: None,
            body_base64: None,
        };

        let report = build_local_success_background_report(
            &payload,
            serde_json::json!({"id": "resp-1"}),
            payload.headers.clone(),
        )
        .expect("success report should be built");

        assert_eq!(report.report_kind, "openai_chat_sync_success");
        assert_eq!(report.body_json, Some(serde_json::json!({"id": "resp-1"})));
        assert_eq!(report.client_body_json, None);
    }

    #[test]
    fn build_local_success_background_report_preserves_provider_stream_for_upstream_stream_sync() {
        let payload = LocalSyncReportParts {
            trace_id: "trace-1b".to_string(),
            report_kind: "openai_chat_sync_finalize".to_string(),
            report_context: Some(serde_json::json!({
                "request_id": "req-1b",
                "upstream_is_stream": true
            })),
            status_code: 200,
            headers: BTreeMap::from([("content-type".to_string(), "text/event-stream".to_string())]),
            body_json: None,
            client_body_json: None,
            body_base64: Some(base64::engine::general_purpose::STANDARD.encode(
                concat!(
                    "event: response.created\n",
                    "data: {\"type\":\"response.created\",\"response\":{\"id\":\"resp-1b\",\"object\":\"response\",\"status\":\"in_progress\",\"output\":[]}}\n\n",
                    "event: response.output_text.delta\n",
                    "data: {\"type\":\"response.output_text.delta\",\"delta\":\"hello\"}\n\n",
                    "event: response.completed\n",
                    "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp-1b\",\"object\":\"response\",\"status\":\"completed\",\"output\":[],\"usage\":{\"input_tokens\":1,\"output_tokens\":1,\"total_tokens\":2}}}\n\n",
                )
            )),
        };

        let report = build_local_success_background_report(
            &payload,
            serde_json::json!({"id": "resp-1b"}),
            payload.headers.clone(),
        )
        .expect("success report should be built");

        assert_eq!(report.body_json, None);
        assert_eq!(
            report.client_body_json,
            Some(serde_json::json!({"id": "resp-1b"}))
        );
        assert_eq!(report.body_base64, payload.body_base64);
    }

    #[test]
    fn build_local_success_conversion_background_report_maps_provider_body() {
        let payload = LocalSyncReportParts {
            trace_id: "trace-2".to_string(),
            report_kind: "openai_chat_sync_finalize".to_string(),
            report_context: Some(serde_json::json!({"request_id": "req-2"})),
            status_code: 200,
            headers: BTreeMap::from([("content-type".to_string(), "application/json".to_string())]),
            body_json: None,
            client_body_json: None,
            body_base64: None,
        };

        let report = build_local_success_conversion_background_report(
            &payload,
            serde_json::json!({"client": true}),
            serde_json::json!({"provider": true}),
        )
        .expect("conversion success report should be built");

        assert_eq!(report.report_kind, "openai_chat_sync_success");
        assert_eq!(
            report.body_json,
            Some(serde_json::json!({"provider": true}))
        );
        assert_eq!(
            report.client_body_json,
            Some(serde_json::json!({"client": true}))
        );
    }
}
