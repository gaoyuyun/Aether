use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::quantize_money;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ProviderBillingType {
    MonthlyQuota,
    PayAsYouGo,
    FreeTier,
    Unknown,
}

/// A periodic spending window layered on top of a provider's cycle quota.
///
/// Window definitions are intentionally kept in the provider `config` JSON so they can be
/// added without changing the provider schema. Usage is folded asynchronously into bounded
/// current-window counters so candidate selection does not scan historical request records.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct ProviderQuotaWindow {
    pub duration_secs: u64,
    pub limit_usd: f64,
}

impl ProviderQuotaWindow {
    pub const MIN_DURATION_SECS: u64 = 60;
    pub const MAX_DURATION_SECS: u64 = 366 * 24 * 60 * 60;
    pub const MAX_WINDOWS: usize = 8;

    pub fn new(duration_secs: u64, limit_usd: f64) -> Option<Self> {
        if !(Self::MIN_DURATION_SECS..=Self::MAX_DURATION_SECS).contains(&duration_secs)
            || !limit_usd.is_finite()
            || limit_usd < 0.0
        {
            return None;
        }
        Some(Self {
            duration_secs,
            limit_usd,
        })
    }
}

/// Reads the portable `config.quota_windows` representation used by the admin API.
pub fn quota_windows_from_config(config: Option<&Value>) -> Vec<ProviderQuotaWindow> {
    let windows = config
        .and_then(Value::as_object)
        .and_then(|object| object.get("quota_windows"))
        .and_then(Value::as_array);

    windows
        .into_iter()
        .flatten()
        .take(ProviderQuotaWindow::MAX_WINDOWS)
        .filter_map(|value| {
            let object = value.as_object()?;
            let duration_secs = object
                .get("duration_secs")
                .and_then(|value| value.as_u64())?;
            let limit_usd = object.get("limit_usd").and_then(|value| value.as_f64())?;
            ProviderQuotaWindow::new(duration_secs, limit_usd)
        })
        .collect()
}

/// Returns the start of the current fixed window. A configured cycle start anchors the windows;
/// legacy rows without an anchor use a stable Unix-epoch-aligned bucket until they are repaired.
pub fn quota_window_start_unix_secs(
    now_unix_secs: u64,
    anchor_unix_secs: Option<u64>,
    duration_secs: u64,
) -> u64 {
    if duration_secs == 0 {
        return now_unix_secs;
    }
    let Some(anchor) = anchor_unix_secs else {
        return now_unix_secs.saturating_sub(now_unix_secs % duration_secs);
    };
    if now_unix_secs < anchor {
        return anchor;
    }
    anchor.saturating_add(
        now_unix_secs
            .saturating_sub(anchor)
            .saturating_div(duration_secs)
            .saturating_mul(duration_secs),
    )
}

impl ProviderBillingType {
    pub fn parse(value: &str) -> Self {
        match value.trim().to_ascii_lowercase().as_str() {
            "monthly_quota" => Self::MonthlyQuota,
            "pay_as_you_go" => Self::PayAsYouGo,
            "free_tier" => Self::FreeTier,
            _ => Self::Unknown,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProviderQuotaSnapshot {
    pub provider_id: String,
    pub billing_type: ProviderBillingType,
    pub monthly_quota_usd: Option<f64>,
    pub monthly_used_usd: f64,
    pub quota_reset_day: Option<u64>,
    pub quota_last_reset_at_unix_secs: Option<u64>,
    pub quota_expires_at_unix_secs: Option<u64>,
    pub is_active: bool,
}

impl ProviderQuotaSnapshot {
    pub fn remaining_quota_usd(&self) -> Option<f64> {
        self.monthly_quota_usd
            .map(|quota| quantize_money(quota - self.monthly_used_usd))
    }

    pub fn is_expired(&self, now_unix_secs: u64) -> bool {
        self.quota_expires_at_unix_secs
            .is_some_and(|expires_at| expires_at <= now_unix_secs)
    }

    pub fn should_reset(&self, now_unix_secs: u64) -> bool {
        if self.billing_type != ProviderBillingType::MonthlyQuota || !self.is_active {
            return false;
        }
        let Some(reset_day) = self.quota_reset_day.filter(|value| *value > 0) else {
            return false;
        };
        let Some(last_reset) = self.quota_last_reset_at_unix_secs else {
            return true;
        };
        now_unix_secs.saturating_sub(last_reset) >= reset_day.saturating_mul(24 * 60 * 60)
    }

    /// Evaluates the legacy cycle quota plus one or more fixed-period spending windows.
    pub fn is_blocked_by_quota_windows(
        &self,
        now_unix_secs: u64,
        windows: &[ProviderQuotaWindow],
        window_usage_usd: &[f64],
    ) -> bool {
        if !self.is_active {
            return false;
        }
        if self
            .remaining_quota_usd()
            .is_some_and(|remaining| remaining <= 0.0)
        {
            return true;
        }
        windows.iter().enumerate().any(|(index, window)| {
            let usage = window_usage_usd.get(index).copied().unwrap_or(0.0);
            usage.is_finite()
                && usage >= window.limit_usd
                && quota_window_start_unix_secs(
                    now_unix_secs,
                    self.quota_last_reset_at_unix_secs,
                    window.duration_secs,
                ) <= now_unix_secs
        })
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{
        quota_window_start_unix_secs, quota_windows_from_config, ProviderBillingType,
        ProviderQuotaSnapshot,
    };

    #[test]
    fn monthly_quota_resets_after_period() {
        let snapshot = ProviderQuotaSnapshot {
            provider_id: "provider-1".to_string(),
            billing_type: ProviderBillingType::MonthlyQuota,
            monthly_quota_usd: Some(20.0),
            monthly_used_usd: 5.0,
            quota_reset_day: Some(7),
            quota_last_reset_at_unix_secs: Some(1_000),
            quota_expires_at_unix_secs: None,
            is_active: true,
        };

        assert!(!snapshot.should_reset(1_000 + 6 * 24 * 60 * 60));
        assert!(snapshot.should_reset(1_000 + 7 * 24 * 60 * 60));
        assert_eq!(snapshot.remaining_quota_usd(), Some(15.0));
    }

    #[test]
    fn parses_daily_weekly_and_custom_windows_from_provider_config() {
        let windows = quota_windows_from_config(Some(&json!({
            "quota_windows": [
                {"duration_secs": 86_400, "limit_usd": 5.0},
                {"duration_secs": 604_800, "limit_usd": 20.0},
                {"duration_secs": 9_000, "limit_usd": 2.5}
            ]
        })));
        assert_eq!(windows.len(), 3);
        assert_eq!(windows[0].duration_secs, 86_400);
        assert_eq!(windows[1].limit_usd, 20.0);
        assert_eq!(windows[2].duration_secs, 9_000);
    }

    #[test]
    fn missing_anchor_uses_a_stable_epoch_aligned_window() {
        assert_eq!(quota_window_start_unix_secs(100_000, None, 86_400), 86_400);
        assert_eq!(quota_window_start_unix_secs(100_001, None, 86_400), 86_400);
    }

    #[test]
    fn blocks_when_any_window_is_exhausted() {
        let snapshot = ProviderQuotaSnapshot {
            provider_id: "provider-1".to_string(),
            billing_type: ProviderBillingType::MonthlyQuota,
            monthly_quota_usd: Some(100.0),
            monthly_used_usd: 1.0,
            quota_reset_day: Some(30),
            quota_last_reset_at_unix_secs: Some(1_000),
            quota_expires_at_unix_secs: None,
            is_active: true,
        };
        let windows = quota_windows_from_config(Some(&json!({
            "quota_windows": [{"duration_secs": 86_400, "limit_usd": 5.0}]
        })));
        assert!(!snapshot.is_blocked_by_quota_windows(90_000, &windows, &[4.99]));
        assert!(snapshot.is_blocked_by_quota_windows(90_000, &windows, &[5.0]));
        assert_eq!(
            quota_window_start_unix_secs(90_000, Some(1_000), 86_400),
            87_400
        );
    }
}
