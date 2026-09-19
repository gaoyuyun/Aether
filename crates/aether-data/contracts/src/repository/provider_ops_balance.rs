use async_trait::async_trait;

/// Last known upstream balance for a provider plus the bookkeeping the
/// background refresher needs (attempt times, failure streak, backoff).
///
/// The admin page only ever reads this snapshot; upstream queries happen in
/// the background so one unreachable provider can never block the others.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct StoredProviderOpsBalanceSnapshot {
    pub provider_id: String,
    /// Projected payload of the most recent successful query. Kept across
    /// failed attempts so operators still see the last good value.
    #[serde(default)]
    pub payload_json: Option<serde_json::Value>,
    #[serde(default)]
    pub last_success_at_unix_secs: Option<u64>,
    #[serde(default)]
    pub last_attempt_at_unix_secs: Option<u64>,
    /// Status of the most recent attempt (`success`, `network_error`, ...).
    #[serde(default)]
    pub last_status: Option<String>,
    /// Sanitized message of the most recent failed attempt.
    #[serde(default)]
    pub last_error: Option<String>,
    #[serde(default)]
    pub consecutive_failures: u32,
    /// Earliest time a non-forced refresh may run again.
    #[serde(default)]
    pub next_refresh_at_unix_secs: Option<u64>,
    pub updated_at_unix_secs: u64,
}

#[async_trait]
pub trait ProviderOpsBalanceSnapshotReadRepository: Send + Sync {
    async fn find_by_provider_id(
        &self,
        provider_id: &str,
    ) -> Result<Option<StoredProviderOpsBalanceSnapshot>, crate::DataLayerError>;

    async fn list_by_provider_ids(
        &self,
        provider_ids: &[String],
    ) -> Result<Vec<StoredProviderOpsBalanceSnapshot>, crate::DataLayerError>;
}

#[async_trait]
pub trait ProviderOpsBalanceSnapshotWriteRepository: Send + Sync {
    async fn upsert(
        &self,
        snapshot: &StoredProviderOpsBalanceSnapshot,
    ) -> Result<(), crate::DataLayerError>;

    async fn delete_by_provider_id(&self, provider_id: &str)
        -> Result<bool, crate::DataLayerError>;
}

pub trait ProviderOpsBalanceSnapshotRepository:
    ProviderOpsBalanceSnapshotReadRepository + ProviderOpsBalanceSnapshotWriteRepository
{
}

impl<T> ProviderOpsBalanceSnapshotRepository for T where
    T: ProviderOpsBalanceSnapshotReadRepository + ProviderOpsBalanceSnapshotWriteRepository
{
}
