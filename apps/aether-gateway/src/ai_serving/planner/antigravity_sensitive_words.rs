//! P6：Antigravity 请求的敏感词零宽混淆收口。
//!
//! Antigravity 没有「原生客户端」的概念：只要供应商 `config.cloak.sensitive_words`（或 Key 级
//! `auth_config.cloak_sensitive_words` 覆盖）非空，就对 v1internal 信封里的
//! `request.systemInstruction.parts[].text` 做混淆。作用范围由 transport 门面的
//! `apply_sensitive_word_obfuscation` 保证：绝不碰 tool 定义、`functionCall` /
//! `functionResponse`、thought 部分。
//!
//! 所有构建 Antigravity 信封的路径（跨格式三条走 `planner::antigravity`，同格式透传走
//! `passthrough/provider/family/request.rs`）在信封构建成功后各调用一次
//! [`apply_antigravity_sensitive_words`]，返回值写入 report_context 顶层
//! `sensitive_words_obfuscation`（见 `report_context::insert_sensitive_words_obfuscation_report`）。

use serde_json::Value;

use crate::ai_serving::transport::{
    apply_sensitive_word_obfuscation, resolve_sensitive_word_list, GatewayProviderTransportSnapshot,
};

/// 对已构建好的 Antigravity v1internal 信封做敏感词混淆。
///
/// 词表为空时不改动 body 并返回 `None`；否则返回 `{applied, replaced, fields}` 报告
/// （没有命中时 `applied=false`，由调用方决定是否写入 report_context）。
pub(crate) fn apply_antigravity_sensitive_words(
    transport: &GatewayProviderTransportSnapshot,
    envelope: &mut Value,
) -> Option<Value> {
    let word_list = resolve_sensitive_word_list(
        transport.provider.config.as_ref(),
        transport.key.decrypted_auth_config.as_deref(),
    );
    if word_list.is_empty() {
        return None;
    }
    Some(
        apply_sensitive_word_obfuscation(envelope, &transport.provider.provider_type, &word_list)
            .to_json(),
    )
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::apply_antigravity_sensitive_words;
    use aether_provider_transport::snapshot::{
        GatewayProviderTransportEndpoint, GatewayProviderTransportKey,
        GatewayProviderTransportProvider, GatewayProviderTransportSnapshot,
    };

    fn transport(
        provider_config: Option<serde_json::Value>,
        auth_config: Option<&str>,
    ) -> GatewayProviderTransportSnapshot {
        GatewayProviderTransportSnapshot {
            provider: GatewayProviderTransportProvider {
                id: "provider-antigravity".to_string(),
                name: "antigravity".to_string(),
                provider_type: "antigravity".to_string(),
                website: None,
                is_active: true,
                keep_priority_on_conversion: false,
                enable_format_conversion: true,
                concurrent_limit: None,
                max_retries: None,
                proxy: None,
                request_timeout_secs: None,
                stream_first_byte_timeout_secs: None,
                config: provider_config,
            },
            endpoint: GatewayProviderTransportEndpoint {
                id: "endpoint-antigravity".to_string(),
                provider_id: "provider-antigravity".to_string(),
                api_format: "gemini:generate_content".to_string(),
                api_family: Some("gemini".to_string()),
                endpoint_kind: Some("cli".to_string()),
                is_active: true,
                base_url: "https://antigravity.googleapis.com".to_string(),
                header_rules: None,
                body_rules: None,
                max_retries: None,
                custom_path: None,
                config: None,
                format_acceptance_config: None,
                proxy: None,
            },
            key: GatewayProviderTransportKey {
                id: "key-antigravity".to_string(),
                provider_id: "provider-antigravity".to_string(),
                name: "key".to_string(),
                auth_type: "bearer".to_string(),
                is_active: true,
                api_formats: None,
                auth_type_by_format: None,
                allow_auth_channel_mismatch_formats: None,
                allowed_models: None,
                capabilities: None,
                rate_multipliers: None,
                global_priority_by_format: None,
                expires_at_unix_secs: None,
                proxy: None,
                fingerprint: None,
                upstream_metadata: None,
                decrypted_api_key: "token".to_string(),
                decrypted_auth_config: auth_config.map(ToOwned::to_owned),
            },
        }
    }

    fn envelope() -> serde_json::Value {
        json!({
            "project": "project-1",
            "requestId": "trace-1",
            "request": {
                "systemInstruction": {
                    "role": "user",
                    "parts": [{"text": "You are a proxy for the API."}]
                },
                "contents": [
                    {"role": "user", "parts": [{"text": "call the proxy tool"}]},
                    {"role": "model", "parts": [{"functionCall": {"name": "proxy_lookup", "args": {"query": "proxy API"}}}]},
                    {"role": "user", "parts": [{"functionResponse": {"name": "proxy_lookup", "response": {"text": "proxy API"}}}]}
                ],
                "tools": [{"functionDeclarations": [{"name": "proxy_lookup", "description": "proxy API lookup"}]}]
            },
            "model": "claude-sonnet-4-5",
            "userAgent": "vscode/1.X.X (Antigravity/4.3.0)",
            "requestType": "agent"
        })
    }

    #[test]
    fn provider_word_list_obfuscates_system_instruction_only() {
        let transport = transport(
            Some(json!({"cloak": {"sensitive_words": ["proxy", "API"]}})),
            None,
        );
        let mut body = envelope();
        let report = apply_antigravity_sensitive_words(&transport, &mut body)
            .expect("non-empty word list produces a report");

        assert_eq!(
            report,
            json!({
                "applied": true,
                "replaced": 2,
                "fields": ["request.systemInstruction.parts[0]"]
            })
        );
        assert_eq!(
            body["request"]["systemInstruction"]["parts"][0]["text"],
            "You are a p\u{200B}roxy for the A\u{200B}PI."
        );
        // contents、functionCall、functionResponse、tools 全部原样。
        let original = envelope();
        assert_eq!(body["request"]["contents"], original["request"]["contents"]);
        assert_eq!(body["request"]["tools"], original["request"]["tools"]);
        assert_eq!(body["userAgent"], original["userAgent"]);
    }

    #[test]
    fn key_override_replaces_provider_list_and_empty_override_disables() {
        let provider_config = Some(json!({"cloak": {"sensitive_words": ["proxy"]}}));
        let override_transport = transport(
            provider_config.clone(),
            Some(r#"{"cloak_sensitive_words":["API"]}"#),
        );
        let mut body = envelope();
        let report = apply_antigravity_sensitive_words(&override_transport, &mut body)
            .expect("override list produces a report");
        assert_eq!(report["replaced"], 1);
        assert_eq!(
            body["request"]["systemInstruction"]["parts"][0]["text"],
            "You are a proxy for the A\u{200B}PI."
        );

        let disabled = transport(provider_config, Some(r#"{"cloak_sensitive_words":[]}"#));
        let mut untouched = envelope();
        assert!(apply_antigravity_sensitive_words(&disabled, &mut untouched).is_none());
        assert_eq!(untouched, envelope());
    }

    #[test]
    fn missing_word_list_is_a_no_op() {
        let transport = transport(None, None);
        let mut body = envelope();
        assert!(apply_antigravity_sensitive_words(&transport, &mut body).is_none());
        assert_eq!(body, envelope());
    }
}
