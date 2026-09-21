use super::keys::{
    pool_cooldown_backoff_key, pool_cooldown_index_key, pool_cooldown_key, pool_cooldown_meta_key,
    pool_model_cooldown_index_key, pool_model_cooldown_key, pool_model_cooldown_meta_key,
};
use crate::handlers::admin::request::AdminAppState;

/// 管理端「清除冷却」：连同决策元数据、退避等级与这把 Key 的全部模型级冷却一起清掉。
pub(crate) async fn clear_admin_provider_pool_cooldown(
    state: &AdminAppState<'_>,
    provider_id: &str,
    key_id: &str,
) {
    let runtime = state.runtime_state();
    let _ = runtime
        .kv_delete(&pool_cooldown_key(provider_id, key_id))
        .await;
    let _ = runtime
        .kv_delete(&pool_cooldown_meta_key(provider_id, key_id))
        .await;
    let _ = runtime
        .kv_delete(&pool_cooldown_backoff_key(provider_id, key_id))
        .await;
    let _ = runtime
        .set_remove(&pool_cooldown_index_key(provider_id), key_id)
        .await;
    let index_key = pool_model_cooldown_index_key(provider_id, key_id);
    for model in runtime.set_members(&index_key).await.unwrap_or_default() {
        let _ = runtime
            .kv_delete(&pool_model_cooldown_key(provider_id, key_id, &model))
            .await;
        let _ = runtime
            .kv_delete(&pool_model_cooldown_meta_key(provider_id, key_id, &model))
            .await;
    }
    let _ = runtime.kv_delete(&index_key).await;
}

pub(crate) async fn reset_admin_provider_pool_cost(
    state: &AdminAppState<'_>,
    provider_id: &str,
    key_id: &str,
) {
    let _ = state
        .runtime_state()
        .score_remove_by_score(&format!("ap:{provider_id}:cost:{key_id}"), f64::INFINITY)
        .await;
}
