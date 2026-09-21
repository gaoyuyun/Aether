use sha2::{Digest, Sha256};

pub(super) fn pool_sticky_pattern(provider_id: &str) -> String {
    format!("ap:{provider_id}:sticky:*")
}

pub(super) fn pool_sticky_key(provider_id: &str, session_token: &str) -> String {
    let digest = Sha256::digest(
        format!(
            "aether-provider-pool-sticky-v1\0{}\0{}",
            provider_id.trim(),
            session_token.trim()
        )
        .as_bytes(),
    );
    format!("ap:{provider_id}:sticky:v1:{digest:x}")
}

pub(super) fn pool_lru_key(provider_id: &str) -> String {
    format!("ap:{provider_id}:lru")
}

pub(super) fn pool_cooldown_key(provider_id: &str, key_id: &str) -> String {
    format!("ap:{provider_id}:cooldown:{key_id}")
}

pub(super) fn pool_cooldown_index_key(provider_id: &str) -> String {
    format!("ap:{provider_id}:cooldown_idx")
}

/// 与 `pool_cooldown_key` 同生命周期的决策元数据（来源、截止时刻、退避等级）。
pub(super) fn pool_cooldown_meta_key(provider_id: &str, key_id: &str) -> String {
    format!("ap:{provider_id}:cooldown_meta:{key_id}")
}

/// 无提示 429 的退避等级；在冷却窗口结束后仍保留一段时间，窗口内再失败继续升级。
pub(super) fn pool_cooldown_backoff_key(provider_id: &str, key_id: &str) -> String {
    format!("ap:{provider_id}:cooldown_backoff:{key_id}")
}

/// Key+模型 级冷却。模型名做一次稳定哈希，避免把用户输入直接拼进 KV 键。
pub(super) fn pool_model_cooldown_key(provider_id: &str, key_id: &str, model: &str) -> String {
    format!(
        "ap:{provider_id}:cooldown_model:{key_id}:{}",
        pool_model_cooldown_scope_id(model)
    )
}

pub(super) fn pool_model_cooldown_meta_key(provider_id: &str, key_id: &str, model: &str) -> String {
    format!(
        "ap:{provider_id}:cooldown_model_meta:{key_id}:{}",
        pool_model_cooldown_scope_id(model)
    )
}

/// 每把 Key 当前处于模型级冷却的模型集合（成员是模型名原文，供管理端展示）。
pub(super) fn pool_model_cooldown_index_key(provider_id: &str, key_id: &str) -> String {
    format!("ap:{provider_id}:cooldown_model_idx:{key_id}")
}

pub(crate) fn pool_model_cooldown_scope_id(model: &str) -> String {
    let normalized = model.trim().to_ascii_lowercase();
    let digest = Sha256::digest(normalized.as_bytes());
    format!("{digest:x}")[..24].to_string()
}

pub(super) fn pool_cost_key(provider_id: &str, key_id: &str) -> String {
    format!("ap:{provider_id}:cost:{key_id}")
}

pub(super) fn pool_latency_key(provider_id: &str, key_id: &str) -> String {
    format!("ap:{provider_id}:latency:{key_id}")
}

pub(super) fn pool_stream_timeout_key(provider_id: &str, key_id: &str) -> String {
    format!("ap:{provider_id}:stream_timeout:{key_id}")
}

pub(super) fn parse_pool_cost_member(member: &str) -> u64 {
    member
        .rsplit_once(':')
        .and_then(|(_, suffix)| suffix.parse::<u64>().ok())
        .unwrap_or(0)
}

pub(super) fn parse_pool_latency_member(member: &str) -> u64 {
    member
        .rsplit_once(':')
        .and_then(|(_, suffix)| suffix.parse::<u64>().ok())
        .unwrap_or(0)
}

pub(super) fn pool_cooldown_keys(provider_id: &str, key_ids: &[String]) -> Vec<String> {
    key_ids
        .iter()
        .map(|key_id| pool_cooldown_key(provider_id, key_id))
        .collect()
}

pub(super) fn pool_cooldown_meta_keys(provider_id: &str, key_ids: &[String]) -> Vec<String> {
    key_ids
        .iter()
        .map(|key_id| pool_cooldown_meta_key(provider_id, key_id))
        .collect()
}

pub(super) fn pool_cost_keys(provider_id: &str, key_ids: &[String]) -> Vec<String> {
    key_ids
        .iter()
        .map(|key_id| pool_cost_key(provider_id, key_id))
        .collect()
}

pub(super) fn pool_latency_keys(provider_id: &str, key_ids: &[String]) -> Vec<String> {
    key_ids
        .iter()
        .map(|key_id| pool_latency_key(provider_id, key_id))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::pool_sticky_key;

    #[test]
    fn sticky_key_does_not_expose_client_session_identifier() {
        let session_token = "customer@example.test/private-conversation";
        let key = pool_sticky_key("provider-a", session_token);

        assert_eq!(key.len(), "ap:provider-a:sticky:v1:".len() + 64);
        assert!(!key.contains(session_token));
        assert_eq!(key, pool_sticky_key("provider-a", session_token));
        assert_ne!(key, pool_sticky_key("provider-b", session_token));
    }
}
