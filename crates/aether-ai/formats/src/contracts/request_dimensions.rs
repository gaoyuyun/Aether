use serde::{Deserialize, Serialize};

/// Client behavior profile detected at the ingress boundary.
///
/// This is deliberately independent from the credential carrier: an
/// Anthropic SDK may use bearer auth, and Claude Code may be authenticated by
/// an Aether API key.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClientSurface {
    ClaudeCode,
    AnthropicSdk,
    GenericCompatible,
}

impl ClientSurface {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ClaudeCode => "claude_code",
            Self::AnthropicSdk => "anthropic_sdk",
            Self::GenericCompatible => "generic_compatible",
        }
    }
}

/// Semantic operation carried over an API wire format.
///
/// Operations must not be represented as additional API formats: both
/// Anthropic message creation and token counting use the `claude:messages`
/// request contract.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApiOperation {
    ClaudeMessagesCreate,
    ClaudeCountTokens,
    OpenAiResponsesCompact,
}

impl ApiOperation {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ClaudeMessagesCreate => "messages",
            Self::ClaudeCountTokens => "count_tokens",
            Self::OpenAiResponsesCompact => "compact",
        }
    }

    /// Resolves an operation back from the name [`ApiOperation::as_str`] produces.
    ///
    /// A planner report context carries the operation as that name, so a usage
    /// writer reading the context has only the string to work from.
    pub fn from_wire_name(value: &str) -> Option<Self> {
        let value = value.trim();
        [
            Self::ClaudeMessagesCreate,
            Self::ClaudeCountTokens,
            Self::OpenAiResponsesCompact,
        ]
        .into_iter()
        .find(|operation| value.eq_ignore_ascii_case(operation.as_str()))
    }

    /// The usage audit `request_type` this operation has to be recorded as.
    ///
    /// `None` means the operation carries no distinct audit identity, so the
    /// request type stays derived from the API format and the request body.
    /// Token counting is the one operation whose identity cannot be recovered
    /// from either signal: it shares the `claude:messages` format with a chat
    /// completion and its body is an ordinary message list. Recording it as
    /// `chat` is what made token counting rows impossible to filter out.
    pub const fn usage_request_type(self) -> Option<&'static str> {
        match self {
            Self::ClaudeCountTokens => Some(Self::ClaudeCountTokens.as_str()),
            Self::ClaudeMessagesCreate | Self::OpenAiResponsesCompact => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{ApiOperation, ClientSurface};

    #[test]
    fn request_dimensions_have_stable_external_names() {
        assert_eq!(ClientSurface::ClaudeCode.as_str(), "claude_code");
        assert_eq!(ApiOperation::ClaudeMessagesCreate.as_str(), "messages");
        assert_eq!(ApiOperation::ClaudeCountTokens.as_str(), "count_tokens");
    }

    #[test]
    fn api_operations_round_trip_through_their_wire_name() {
        for operation in [
            ApiOperation::ClaudeMessagesCreate,
            ApiOperation::ClaudeCountTokens,
            ApiOperation::OpenAiResponsesCompact,
        ] {
            assert_eq!(
                ApiOperation::from_wire_name(operation.as_str()),
                Some(operation)
            );
        }
        assert_eq!(
            ApiOperation::from_wire_name("  COUNT_TOKENS "),
            Some(ApiOperation::ClaudeCountTokens)
        );
        assert_eq!(ApiOperation::from_wire_name("chat"), None);
        assert_eq!(ApiOperation::from_wire_name(""), None);
    }

    #[test]
    fn only_token_counting_overrides_the_audit_request_type() {
        assert_eq!(
            ApiOperation::ClaudeCountTokens.usage_request_type(),
            Some("count_tokens")
        );
        assert_eq!(
            ApiOperation::ClaudeMessagesCreate.usage_request_type(),
            None
        );
        assert_eq!(
            ApiOperation::OpenAiResponsesCompact.usage_request_type(),
            None
        );
    }
}
