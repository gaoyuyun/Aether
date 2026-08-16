use std::collections::BTreeSet;

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
    pub const MAX_DURATION_SECS: u64 = 30 * 24 * 60 * 60;
    pub const MAX_WINDOWS: usize = 8;

    pub fn new(duration_secs: u64, limit_usd: f64) -> Option<Self> {
        if !(Self::MIN_DURATION_SECS..=Self::MAX_DURATION_SECS).contains(&duration_secs)
            || duration_secs % 60 != 0
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

/// Validates the complete stored window policy without silently dropping malformed entries.
pub fn quota_windows_config_is_valid(config: Option<&Value>) -> bool {
    let Some(raw) = config
        .and_then(Value::as_object)
        .and_then(|object| object.get("quota_windows"))
    else {
        return true;
    };
    let Some(entries) = raw.as_array() else {
        return false;
    };
    if entries.len() > ProviderQuotaWindow::MAX_WINDOWS {
        return false;
    }
    let mut durations = BTreeSet::new();
    entries.iter().all(|value| {
        let Some(object) = value.as_object() else {
            return false;
        };
        let Some(duration_secs) = object.get("duration_secs").and_then(Value::as_u64) else {
            return false;
        };
        let Some(limit_usd) = object.get("limit_usd").and_then(Value::as_f64) else {
            return false;
        };
        ProviderQuotaWindow::new(duration_secs, limit_usd).is_some()
            && durations.insert(duration_secs)
    })
}

pub fn quota_clock_minute(now_unix_secs: u64) -> u64 {
    now_unix_secs / 60 * 60
}

/// Returns the inclusive lower bound of a rolling quota window.
///
/// Quota epochs and rolling windows are minute-precise. A missing epoch is fail-safe and uses the
/// current minute, so callers never accidentally include usage from an unknown historical cycle.
pub fn quota_window_start_unix_secs(
    now_unix_secs: u64,
    quota_epoch_start_unix_secs: Option<u64>,
    duration_secs: u64,
) -> u64 {
    let clock_minute = quota_clock_minute(now_unix_secs);
    let epoch_start = quota_epoch_start_unix_secs
        .map(quota_clock_minute)
        .unwrap_or(clock_minute);
    epoch_start.max(clock_minute.saturating_sub(duration_secs))
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

    /// Evaluates the cycle quota plus one or more rolling spending windows.
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
                ) <= quota_clock_minute(now_unix_secs)
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
    fn rolling_window_is_minute_precise_and_clamped_to_epoch() {
        assert_eq!(quota_window_start_unix_secs(100_001, None, 86_400), 99_960);
        assert_eq!(
            quota_window_start_unix_secs(100_001, Some(90_000), 7_200),
            92_760
        );
        assert_eq!(
            quota_window_start_unix_secs(100_001, Some(99_980), 7_200),
            99_960
        );
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
            3_600
        );
    }

    #[test]
    fn rejects_non_minute_and_over_thirty_day_windows() {
        assert!(super::ProviderQuotaWindow::new(61, 1.0).is_none());
        assert!(super::ProviderQuotaWindow::new(30 * 24 * 60 * 60 + 60, 1.0).is_none());
        assert!(super::ProviderQuotaWindow::new(5 * 60, 1.0).is_some());
        assert!(!super::quota_windows_config_is_valid(Some(&json!({
            "quota_windows": [
                {"duration_secs": 300, "limit_usd": 1.0},
                {"duration_secs": 300, "limit_usd": 2.0}
            ]
        }))));
        assert!(!super::quota_windows_config_is_valid(Some(&json!({
            "quota_windows": [{"duration_secs": 61, "limit_usd": 1.0}]
        }))));
    }
}
