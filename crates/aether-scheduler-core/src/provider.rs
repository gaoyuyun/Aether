use std::collections::BTreeMap;

use aether_data_contracts::repository::provider_catalog::StoredProviderCatalogProvider;
use aether_data_contracts::repository::quota::StoredProviderQuotaSnapshot;
use aether_wallet::{
    quota_windows_from_config, ProviderBillingType, ProviderQuotaSnapshot, ProviderQuotaWindow,
};
use serde_json::Value;

pub fn should_skip_provider_quota(
    quota: &StoredProviderQuotaSnapshot,
    _now_unix_secs: u64,
) -> bool {
    let snapshot = ProviderQuotaSnapshot {
        provider_id: quota.provider_id.clone(),
        billing_type: ProviderBillingType::parse(&quota.billing_type),
        monthly_quota_usd: quota.monthly_quota_usd,
        monthly_used_usd: quota.monthly_used_usd,
        quota_reset_day: quota.quota_reset_day,
        quota_last_reset_at_unix_secs: quota.quota_last_reset_at_unix_secs,
        quota_expires_at_unix_secs: quota.quota_expires_at_unix_secs,
        is_active: quota.is_active,
    };

    match snapshot.billing_type {
        ProviderBillingType::MonthlyQuota => {
            !quota.is_active
                || quota.quota_last_reset_at_unix_secs.is_none()
                || quota
                    .quota_subscription_started_at_unix_secs
                    .is_some_and(|start| start > _now_unix_secs)
                || quota
                    .quota_last_reset_at_unix_secs
                    .is_some_and(|start_at| start_at > _now_unix_secs)
                || quota
                    .quota_reset_day
                    .is_some_and(|days| days == 0 || days > 30)
                || quota
                    .pending_quota_reset_at_unix_secs
                    .is_some_and(|effective_at| effective_at <= _now_unix_secs)
                || snapshot.is_expired(_now_unix_secs)
                || snapshot
                    .remaining_quota_usd()
                    .is_some_and(|remaining| remaining <= 0.0)
        }
        ProviderBillingType::PayAsYouGo
        | ProviderBillingType::FreeTier
        | ProviderBillingType::Unknown => false,
    }
}

/// Evaluates the cycle quota and optional provider-configured rolling windows.
pub fn should_skip_provider_quota_with_windows(
    quota: &StoredProviderQuotaSnapshot,
    _now_unix_secs: u64,
    windows: &[ProviderQuotaWindow],
    window_usage_usd: &[f64],
) -> bool {
    let billing_type = ProviderBillingType::parse(&quota.billing_type);
    if billing_type != ProviderBillingType::MonthlyQuota {
        return false;
    }
    if should_skip_provider_quota(quota, _now_unix_secs) {
        return true;
    }
    let snapshot = ProviderQuotaSnapshot {
        provider_id: quota.provider_id.clone(),
        billing_type,
        monthly_quota_usd: quota.monthly_quota_usd,
        monthly_used_usd: quota.monthly_used_usd,
        quota_reset_day: quota.quota_reset_day,
        quota_last_reset_at_unix_secs: quota.quota_last_reset_at_unix_secs,
        quota_expires_at_unix_secs: quota.quota_expires_at_unix_secs,
        is_active: quota.is_active,
    };
    snapshot.is_blocked_by_quota_windows(_now_unix_secs, windows, window_usage_usd)
}

/// Convenience parser for callers that own a provider catalog row.
pub fn provider_quota_windows(config: Option<&Value>) -> Vec<ProviderQuotaWindow> {
    quota_windows_from_config(config)
}

pub fn provider_quota_windows_config_is_valid(config: Option<&Value>) -> bool {
    aether_wallet::quota_windows_config_is_valid(config)
}

pub fn build_provider_concurrent_limit_map(
    providers: Vec<StoredProviderCatalogProvider>,
) -> BTreeMap<String, usize> {
    providers
        .into_iter()
        .filter_map(|provider| {
            provider
                .concurrent_limit
                .and_then(|limit| usize::try_from(limit).ok())
                .filter(|limit| *limit > 0)
                .map(|limit| (provider.id, limit))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::{
        build_provider_concurrent_limit_map, should_skip_provider_quota,
        should_skip_provider_quota_with_windows,
    };
    use aether_data_contracts::repository::provider_catalog::StoredProviderCatalogProvider;
    use aether_data_contracts::repository::quota::StoredProviderQuotaSnapshot;
    use aether_wallet::ProviderQuotaWindow;

    fn sample_provider(id: &str, concurrent_limit: Option<i32>) -> StoredProviderCatalogProvider {
        StoredProviderCatalogProvider::new(
            id.to_string(),
            format!("provider-{id}"),
            Some("https://example.com".to_string()),
            "custom".to_string(),
        )
        .expect("provider should build")
        .with_transport_fields(
            true,
            false,
            false,
            concurrent_limit,
            None,
            None,
            None,
            None,
            None,
        )
    }

    #[test]
    fn skips_only_exhausted_monthly_quota_provider() {
        let inactive = StoredProviderQuotaSnapshot::new(
            "provider-1".to_string(),
            "monthly_quota".to_string(),
            Some(10.0),
            1.0,
            Some(30),
            Some(1_000),
            None,
            false,
        )
        .expect("quota should build");
        assert!(should_skip_provider_quota(&inactive, 2_000));

        let expired = StoredProviderQuotaSnapshot::new(
            "provider-1".to_string(),
            "monthly_quota".to_string(),
            Some(10.0),
            1.0,
            Some(30),
            Some(1_000),
            Some(1_500),
            true,
        )
        .expect("quota should build");
        assert!(should_skip_provider_quota(&expired, 2_000));

        let exhausted = StoredProviderQuotaSnapshot::new(
            "provider-1".to_string(),
            "monthly_quota".to_string(),
            Some(10.0),
            10.0,
            Some(30),
            Some(1_000),
            None,
            true,
        )
        .expect("quota should build");
        assert!(should_skip_provider_quota(&exhausted, 2_000));

        let payg = StoredProviderQuotaSnapshot::new(
            "provider-1".to_string(),
            "pay_as_you_go".to_string(),
            None,
            10.0,
            None,
            None,
            None,
            true,
        )
        .expect("quota should build");
        assert!(!should_skip_provider_quota(&payg, 2_000));

        let free = StoredProviderQuotaSnapshot::new(
            "provider-1".to_string(),
            "free_tier".to_string(),
            Some(10.0),
            10.0,
            Some(30),
            Some(1_000),
            None,
            true,
        )
        .expect("quota should build");
        assert!(!should_skip_provider_quota(&free, 2_000));
    }

    #[test]
    fn skips_monthly_quota_before_subscription_start() {
        let mut future = StoredProviderQuotaSnapshot::new(
            "provider-future".to_string(),
            "monthly_quota".to_string(),
            Some(10.0),
            0.0,
            Some(30),
            Some(3_000),
            None,
            true,
        )
        .expect("quota should build");
        future.quota_subscription_started_at_unix_secs = Some(3_000);
        future.quota_cycle_start_at_unix_secs = Some(3_000);
        assert!(should_skip_provider_quota(&future, 2_000));
        assert!(!should_skip_provider_quota(&future, 3_000));
    }

    #[test]
    fn builds_provider_concurrent_limit_map_for_positive_limits_only() {
        let limits = build_provider_concurrent_limit_map(vec![
            sample_provider("provider-a", Some(10)),
            sample_provider("provider-b", Some(0)),
            sample_provider("provider-c", None),
        ]);

        assert_eq!(limits.get("provider-a"), Some(&10));
        assert!(!limits.contains_key("provider-b"));
        assert!(!limits.contains_key("provider-c"));
    }

    #[test]
    fn applies_windows_only_to_subscription_billing() {
        let monthly = StoredProviderQuotaSnapshot::new(
            "provider-1".to_string(),
            "monthly_quota".to_string(),
            Some(100.0),
            1.0,
            Some(30),
            Some(1_000),
            None,
            true,
        )
        .expect("quota should build");
        let window = ProviderQuotaWindow::new(86_400, 5.0).expect("window should build");

        assert!(!should_skip_provider_quota_with_windows(
            &monthly,
            90_000,
            &[window],
            &[4.99]
        ));
        assert!(should_skip_provider_quota_with_windows(
            &monthly,
            90_000,
            &[window],
            &[5.0]
        ));

        let payg = StoredProviderQuotaSnapshot::new(
            "provider-1".to_string(),
            "pay_as_you_go".to_string(),
            None,
            50.0,
            None,
            None,
            None,
            true,
        )
        .expect("quota should build");
        assert!(!should_skip_provider_quota_with_windows(
            &payg,
            90_000,
            &[window],
            &[50.0]
        ));
    }
}
