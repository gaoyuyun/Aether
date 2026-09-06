use std::collections::BTreeMap;
use std::sync::RwLock;

use async_trait::async_trait;

use super::{
    ProviderQuotaReadRepository, ProviderQuotaWriteRepository, StoredProviderQuotaSnapshot,
};
use crate::DataLayerError;
use aether_data_contracts::repository::quota::ProviderQuotaAdjustment;
use aether_wallet::ProviderBillingType;

#[derive(Debug, Default)]
pub struct InMemoryProviderQuotaRepository {
    by_provider_id: RwLock<BTreeMap<String, StoredProviderQuotaSnapshot>>,
}

impl InMemoryProviderQuotaRepository {
    pub fn seed<I>(items: I) -> Self
    where
        I: IntoIterator<Item = StoredProviderQuotaSnapshot>,
    {
        let mut by_provider_id = BTreeMap::new();
        for item in items {
            by_provider_id.insert(item.provider_id.clone(), item);
        }
        Self {
            by_provider_id: RwLock::new(by_provider_id),
        }
    }
}

#[async_trait]
impl ProviderQuotaReadRepository for InMemoryProviderQuotaRepository {
    async fn find_by_provider_id(
        &self,
        provider_id: &str,
    ) -> Result<Option<StoredProviderQuotaSnapshot>, DataLayerError> {
        Ok(self
            .by_provider_id
            .read()
            .expect("quota repository lock")
            .get(provider_id)
            .cloned())
    }

    async fn find_by_provider_ids(
        &self,
        provider_ids: &[String],
    ) -> Result<Vec<StoredProviderQuotaSnapshot>, DataLayerError> {
        let quotas = self.by_provider_id.read().expect("quota repository lock");
        Ok(provider_ids
            .iter()
            .filter_map(|provider_id| quotas.get(provider_id).cloned())
            .collect())
    }
}

#[async_trait]
impl ProviderQuotaWriteRepository for InMemoryProviderQuotaRepository {
    async fn reset_due(&self, now_unix_secs: u64) -> Result<usize, DataLayerError> {
        let mut count = 0usize;
        let mut quotas = self.by_provider_id.write().expect("quota repository lock");
        for quota in quotas.values_mut() {
            if !quota.is_active {
                continue;
            }
            if let Some(change) = quota.due_transition(now_unix_secs) {
                quota.quota_subscription_started_at_unix_secs = quota
                    .quota_subscription_started_at_unix_secs
                    .or(quota.quota_last_reset_at_unix_secs)
                    .or(Some(change.cycle_start));
                quota.quota_cycle_start_at_unix_secs = Some(change.cycle_start);
                quota.quota_last_reset_at_unix_secs = Some(change.epoch_start);
                quota.quota_reset_day = Some(change.cycle_days);
                if change.reset_usage {
                    quota.monthly_used_usd = 0.0;
                }
                if change.applied_pending {
                    quota.pending_adjustment = None;
                    quota.pending_quota_reset_at_unix_secs = None;
                }
                count += 1;
            }
        }
        Ok(count)
    }

    async fn request_reset(
        &self,
        provider_id: &str,
        effective_at_unix_secs: u64,
    ) -> Result<bool, DataLayerError> {
        self.request_adjustment(
            provider_id,
            &ProviderQuotaAdjustment::cycle(effective_at_unix_secs),
        )
        .await
    }

    async fn request_adjustment(
        &self,
        provider_id: &str,
        adjustment: &ProviderQuotaAdjustment,
    ) -> Result<bool, DataLayerError> {
        adjustment.validate()?;
        let mut quotas = self.by_provider_id.write().expect("quota repository lock");
        let Some(quota) = quotas.get_mut(provider_id) else {
            return Ok(false);
        };
        if ProviderBillingType::parse(&quota.billing_type) != ProviderBillingType::MonthlyQuota {
            return Ok(false);
        }
        quota.pending_quota_reset_at_unix_secs = Some(adjustment.effective_at_unix_secs);
        quota.pending_adjustment = Some(adjustment.clone());
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::InMemoryProviderQuotaRepository;
    use crate::repository::quota::{
        ProviderQuotaReadRepository, ProviderQuotaWriteRepository, StoredProviderQuotaSnapshot,
    };

    fn sample_quota() -> StoredProviderQuotaSnapshot {
        StoredProviderQuotaSnapshot::new(
            "provider-1".to_string(),
            "monthly_quota".to_string(),
            Some(20.0),
            5.0,
            Some(7),
            Some(1_000),
            None,
            true,
        )
        .expect("quota should build")
    }

    #[tokio::test]
    async fn resets_due_monthly_quota() {
        let repository = InMemoryProviderQuotaRepository::seed(vec![sample_quota()]);
        let reset = repository
            .reset_due(1_000 + 7 * 24 * 60 * 60)
            .await
            .expect("reset should succeed");
        assert_eq!(reset, 1);
        let stored = repository
            .find_by_provider_id("provider-1")
            .await
            .expect("lookup should succeed")
            .expect("quota should exist");
        assert_eq!(stored.monthly_used_usd, 0.0);
    }

    #[tokio::test]
    async fn finds_quotas_by_provider_ids() {
        let repository = InMemoryProviderQuotaRepository::seed(vec![
            sample_quota(),
            StoredProviderQuotaSnapshot::new(
                "provider-2".to_string(),
                "payg".to_string(),
                None,
                1.5,
                None,
                None,
                None,
                true,
            )
            .expect("quota should build"),
        ]);

        let stored = repository
            .find_by_provider_ids(&[
                "provider-2".to_string(),
                "missing".to_string(),
                "provider-1".to_string(),
            ])
            .await
            .expect("lookup should succeed");

        assert_eq!(stored.len(), 2);
        assert_eq!(stored[0].provider_id, "provider-2");
        assert_eq!(stored[1].provider_id, "provider-1");
    }
}
