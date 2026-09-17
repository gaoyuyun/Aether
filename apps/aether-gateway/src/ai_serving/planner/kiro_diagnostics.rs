use serde_json::Value;

use crate::ai_serving::transport::kiro::{try_build_kiro_provider_request_body, KiroAuthConfig};
use crate::ai_serving::{CandidateFailureDiagnostic, CandidateFailureDiagnosticKind};

/// Explains why the Kiro request envelope could not be built for a candidate.
///
/// The envelope build already failed once; running it again costs one more
/// conversion of an already rejected candidate and turns "包装失败" into the
/// field path an operator can act on (for example the trailing message whose
/// role Kiro cannot represent). Falls back to the generic hint if the rebuild
/// unexpectedly succeeds.
#[allow(clippy::too_many_arguments)]
pub(crate) fn kiro_envelope_failure_diagnostic(
    body_json: &Value,
    mapped_model: &str,
    auth_config: &KiroAuthConfig,
    body_rules: Option<&Value>,
    request_headers: Option<&http::HeaderMap>,
    client_api_format: &str,
    provider_api_format: &str,
    source: &str,
) -> CandidateFailureDiagnostic {
    let (path, message) = match try_build_kiro_provider_request_body(
        body_json,
        mapped_model,
        auth_config,
        body_rules,
        request_headers,
    ) {
        Err(error) => (error.path(), error.message()),
        Ok(_) => (
            "$".to_string(),
            "Kiro 反代请求体包装失败；请检查 Kiro auth_config 与 Endpoint Body 规则".to_string(),
        ),
    };
    CandidateFailureDiagnostic::new(CandidateFailureDiagnosticKind::EnvelopeBuild, path, message)
        .formats(client_api_format, provider_api_format)
        .source(source)
}

#[cfg(test)]
mod tests {
    use super::kiro_envelope_failure_diagnostic;
    use crate::ai_serving::transport::kiro::KiroAuthConfig;

    fn auth_config() -> KiroAuthConfig {
        KiroAuthConfig {
            auth_method: None,
            refresh_token: Some("r".repeat(128)),
            expires_at: None,
            profile_arn: None,
            region: None,
            auth_region: None,
            api_region: Some("us-east-1".to_string()),
            client_id: None,
            client_secret: None,
            machine_id: None,
            kiro_version: None,
            system_version: None,
            node_version: None,
            access_token: Some("cached-token".to_string()),
        }
    }

    #[test]
    fn names_the_trailing_message_kiro_cannot_represent() {
        let extra_data = kiro_envelope_failure_diagnostic(
            &serde_json::json!({
                "model": "claude-opus-5",
                "messages": [
                    {"role": "user", "content": "hi"},
                    {"role": "assistant", "content": "hello"},
                    {"role": "tool", "content": "unsupported"}
                ]
            }),
            "claude-opus-5",
            &auth_config(),
            None,
            None,
            "claude:messages",
            "claude:messages",
            "kiro_envelope",
        )
        .to_extra_data();

        assert_eq!(extra_data["failure_diagnostic"]["kind"], "envelope_build");
        assert_eq!(
            extra_data["failure_diagnostic"]["path"],
            "$.messages[2].role"
        );
        assert!(extra_data["failure_diagnostic"]["message"]
            .as_str()
            .expect("message")
            .contains("tool"));
        assert_eq!(extra_data["failure_diagnostic"]["source"], "kiro_envelope");
        assert_eq!(extra_data["failure_diagnostic"]["safe_to_show"], true);
    }

    #[test]
    fn names_missing_messages() {
        let extra_data = kiro_envelope_failure_diagnostic(
            &serde_json::json!({"model": "claude-opus-5"}),
            "claude-opus-5",
            &auth_config(),
            None,
            None,
            "openai:chat",
            "claude:messages",
            "openai_chat_kiro_envelope",
        )
        .to_extra_data();

        assert_eq!(extra_data["failure_diagnostic"]["path"], "$.messages");
        assert_eq!(
            extra_data["failure_diagnostic"]["client_api_format"],
            "openai:chat"
        );
    }
}
