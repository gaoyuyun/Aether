use crate::provider::ProviderPoolAdapter;

#[derive(Debug, Clone, Copy)]
pub struct UnsupportedQuotaProviderPoolAdapter {
    provider_type: &'static str,
    quota_refresh_unsupported_message: &'static str,
}

impl UnsupportedQuotaProviderPoolAdapter {
    pub const fn new(
        provider_type: &'static str,
        quota_refresh_unsupported_message: &'static str,
    ) -> Self {
        Self {
            provider_type,
            quota_refresh_unsupported_message,
        }
    }
}

impl ProviderPoolAdapter for UnsupportedQuotaProviderPoolAdapter {
    fn provider_type(&self) -> &'static str {
        self.provider_type
    }

    fn quota_refresh_unsupported_message(&self) -> String {
        self.quota_refresh_unsupported_message.to_string()
    }
}

/// Claude Code 没有主动的额度查询接口；5h / 7d 窗口从每次响应的
/// `anthropic-ratelimit-unified-*` 头被动采集。`quota_refresh` 打开只是让管理端能
/// 触发一次「重新物化快照」，不会向上游发请求。
#[derive(Debug, Clone, Copy)]
pub struct ClaudeCodePassiveQuotaProviderPoolAdapter;

impl ProviderPoolAdapter for ClaudeCodePassiveQuotaProviderPoolAdapter {
    fn provider_type(&self) -> &'static str {
        "claude_code"
    }

    fn capabilities(&self) -> crate::capability::ProviderPoolCapabilities {
        crate::capability::ProviderPoolCapabilities {
            quota_refresh: true,
            ..crate::capability::ProviderPoolCapabilities::default()
        }
    }

    fn quota_refresh_endpoint(
        &self,
        endpoints: &[aether_data_contracts::repository::provider_catalog::StoredProviderCatalogEndpoint],
        include_inactive: bool,
    ) -> Option<aether_data_contracts::repository::provider_catalog::StoredProviderCatalogEndpoint>
    {
        crate::provider::provider_pool_matching_endpoint(endpoints, include_inactive, |endpoint| {
            crate::provider::provider_pool_endpoint_format_matches(endpoint, "claude:messages")
        })
    }

    fn quota_refresh_missing_endpoint_message(&self) -> String {
        "找不到有效的 claude:messages 端点".to_string()
    }

    fn quota_refresh_unsupported_message(&self) -> String {
        "Claude Code 额度为被动采集：5h / 7d 窗口来自请求响应头，不支持主动查询".to_string()
    }
}

pub const CLAUDE_CODE_PROVIDER_POOL_ADAPTER: ClaudeCodePassiveQuotaProviderPoolAdapter =
    ClaudeCodePassiveQuotaProviderPoolAdapter;

pub const VERTEX_AI_PROVIDER_POOL_ADAPTER: UnsupportedQuotaProviderPoolAdapter =
    UnsupportedQuotaProviderPoolAdapter::new(
        "vertex_ai",
        "Vertex AI 暂不支持自动刷新额度：额度属于 Google Cloud 项目/区域配额",
    );

pub const GROK_BUILD_PROVIDER_POOL_ADAPTER: UnsupportedQuotaProviderPoolAdapter =
    UnsupportedQuotaProviderPoolAdapter::new(
        "grok_build",
        "Grok Build 暂不支持自动刷新额度：cli-chat-proxy 没有账号额度查询接口",
    );
