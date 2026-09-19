use aether_data::repository::provider_ops_balance::StoredProviderOpsBalanceSnapshot;
use aether_data_contracts::DataLayerError;

use super::GatewayDataState;

impl GatewayDataState {
    pub(crate) async fn find_provider_ops_balance_snapshot(
        &self,
        provider_id: &str,
    ) -> Result<Option<StoredProviderOpsBalanceSnapshot>, DataLayerError> {
        match &self.provider_ops_balance_snapshot_reader {
            Some(repository) => repository.find_by_provider_id(provider_id).await,
            None => Ok(None),
        }
    }

    pub(crate) async fn list_provider_ops_balance_snapshots(
        &self,
        provider_ids: &[String],
    ) -> Result<Vec<StoredProviderOpsBalanceSnapshot>, DataLayerError> {
        match &self.provider_ops_balance_snapshot_reader {
            Some(repository) if !provider_ids.is_empty() => {
                repository.list_by_provider_ids(provider_ids).await
            }
            _ => Ok(Vec::new()),
        }
    }

    /// Returns `true` when a durable store accepted the snapshot. Deployments
    /// without a catalog database keep snapshots in the runtime KV only.
    pub(crate) async fn upsert_provider_ops_balance_snapshot(
        &self,
        snapshot: &StoredProviderOpsBalanceSnapshot,
    ) -> Result<bool, DataLayerError> {
        match &self.provider_ops_balance_snapshot_writer {
            Some(repository) => repository.upsert(snapshot).await.map(|_| true),
            None => Ok(false),
        }
    }

    pub(crate) async fn delete_provider_ops_balance_snapshot(
        &self,
        provider_id: &str,
    ) -> Result<bool, DataLayerError> {
        match &self.provider_ops_balance_snapshot_writer {
            Some(repository) => repository.delete_by_provider_id(provider_id).await,
            None => Ok(false),
        }
    }
}
