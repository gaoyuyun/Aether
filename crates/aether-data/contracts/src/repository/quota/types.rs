use async_trait::async_trait;

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct StoredProviderQuotaSnapshot {
    pub provider_id: String,
    pub billing_type: String,
    pub monthly_quota_usd: Option<f64>,
    pub monthly_used_usd: f64,
    pub quota_reset_day: Option<u64>,
    pub quota_last_reset_at_unix_secs: Option<u64>,
    #[serde(default)]
    pub pending_quota_reset_at_unix_secs: Option<u64>,
    #[serde(default)]
    pub quota_subscription_started_at_unix_secs: Option<u64>,
    #[serde(default)]
    pub quota_cycle_start_at_unix_secs: Option<u64>,
    #[serde(default)]
    pub pending_adjustment: Option<ProviderQuotaAdjustment>,
    pub quota_expires_at_unix_secs: Option<u64>,
    pub is_active: bool,
}

impl StoredProviderQuotaSnapshot {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        provider_id: String,
        billing_type: String,
        monthly_quota_usd: Option<f64>,
        monthly_used_usd: f64,
        quota_reset_day: Option<i32>,
        quota_last_reset_at_unix_secs: Option<i64>,
        quota_expires_at_unix_secs: Option<i64>,
        is_active: bool,
    ) -> Result<Self, crate::DataLayerError> {
        if provider_id.trim().is_empty() || billing_type.trim().is_empty() {
            return Err(crate::DataLayerError::UnexpectedValue(
                "provider quota identity is empty".to_string(),
            ));
        }
        if !monthly_used_usd.is_finite() || monthly_quota_usd.is_some_and(|v| !v.is_finite()) {
            return Err(crate::DataLayerError::UnexpectedValue(
                "provider quota value is not finite".to_string(),
            ));
        }
        Ok(Self {
            provider_id,
            billing_type,
            monthly_quota_usd,
            monthly_used_usd,
            quota_reset_day: quota_reset_day.map(|value| value as u64),
            quota_last_reset_at_unix_secs: quota_last_reset_at_unix_secs.map(|value| value as u64),
            pending_quota_reset_at_unix_secs: None,
            quota_subscription_started_at_unix_secs: quota_last_reset_at_unix_secs
                .map(|v| v as u64),
            quota_cycle_start_at_unix_secs: quota_last_reset_at_unix_secs.map(|v| v as u64),
            pending_adjustment: None,
            quota_expires_at_unix_secs: quota_expires_at_unix_secs.map(|value| value as u64),
            is_active,
        })
    }
}

#[async_trait]
pub trait ProviderQuotaReadRepository: Send + Sync {
    async fn find_by_provider_id(
        &self,
        provider_id: &str,
    ) -> Result<Option<StoredProviderQuotaSnapshot>, crate::DataLayerError>;

    async fn find_by_provider_ids(
        &self,
        provider_ids: &[String],
    ) -> Result<Vec<StoredProviderQuotaSnapshot>, crate::DataLayerError>;
}

#[async_trait]
pub trait ProviderQuotaWriteRepository: Send + Sync {
    async fn reset_due(&self, now_unix_secs: u64) -> Result<usize, crate::DataLayerError>;

    async fn request_reset(
        &self,
        provider_id: &str,
        effective_at_unix_secs: u64,
    ) -> Result<bool, crate::DataLayerError> {
        let _ = (provider_id, effective_at_unix_secs);
        Ok(false)
    }

    async fn request_adjustment(
        &self,
        provider_id: &str,
        adjustment: &ProviderQuotaAdjustment,
    ) -> Result<bool, crate::DataLayerError> {
        let _ = (provider_id, adjustment);
        Ok(false)
    }

    async fn recover_attempts(
        &self,
        provider_id: Option<&str>,
        limit: usize,
        now_unix_secs: u64,
        dry_run: bool,
    ) -> Result<Vec<ProviderQuotaRecovery>, crate::DataLayerError> {
        let _ = (provider_id, limit, now_unix_secs, dry_run);
        Ok(Vec::new())
    }

    async fn clear_window_counters(&self, _provider_id: &str) -> Result<(), crate::DataLayerError> {
        Ok(())
    }
}

pub trait ProviderQuotaRepository:
    ProviderQuotaReadRepository + ProviderQuotaWriteRepository + Send + Sync
{
}

impl<T> ProviderQuotaRepository for T where
    T: ProviderQuotaReadRepository + ProviderQuotaWriteRepository + Send + Sync
{
}

/// A scheduled quota operation. Subscription activation and expiry never change.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderQuotaResetMode {
    #[default]
    Cycle,
    UsageOnly,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ProviderQuotaAdjustment {
    pub effective_at_unix_secs: u64,
    pub mode: ProviderQuotaResetMode,
    pub reset_usage: bool,
    pub cycle_days: Option<u64>,
}

impl ProviderQuotaAdjustment {
    pub fn cycle(effective_at_unix_secs: u64) -> Self {
        Self {
            effective_at_unix_secs: effective_at_unix_secs / 60 * 60,
            mode: ProviderQuotaResetMode::Cycle,
            reset_usage: true,
            cycle_days: None,
        }
    }

    pub fn validate(&self) -> Result<(), crate::DataLayerError> {
        if self.effective_at_unix_secs % 60 != 0
            || self
                .cycle_days
                .is_some_and(|days| !(1..=30).contains(&days))
            || (self.mode == ProviderQuotaResetMode::UsageOnly
                && (!self.reset_usage || self.cycle_days.is_some()))
        {
            return Err(crate::DataLayerError::InvalidInput(
                "invalid provider quota adjustment".to_string(),
            ));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderQuotaTransition {
    pub cycle_start: u64,
    pub epoch_start: u64,
    pub cycle_days: u64,
    pub reset_usage: bool,
    pub applied_pending: bool,
}

impl StoredProviderQuotaSnapshot {
    /// Natural boundaries advance on the original grid, even after a late worker run.
    pub fn due_transition(&self, now: u64) -> Option<ProviderQuotaTransition> {
        if self.billing_type != "monthly_quota" {
            return None;
        }
        let now = now / 60 * 60;
        let cycle = self
            .quota_cycle_start_at_unix_secs
            .or(self.quota_last_reset_at_unix_secs)
            .or(self.quota_subscription_started_at_unix_secs)
            .unwrap_or(now)
            / 60
            * 60;
        let epoch = self.quota_last_reset_at_unix_secs.unwrap_or(cycle) / 60 * 60;
        let mut days = self
            .quota_reset_day
            .filter(|days| (1..=30).contains(days))?;
        let pending = self.pending_adjustment.clone().or_else(|| {
            self.pending_quota_reset_at_unix_secs
                .map(ProviderQuotaAdjustment::cycle)
        });
        let mut result = ProviderQuotaTransition {
            cycle_start: cycle,
            epoch_start: epoch,
            cycle_days: days,
            reset_usage: false,
            applied_pending: false,
        };
        if let Some(op) = pending.filter(|op| op.effective_at_unix_secs <= now) {
            if op.mode == ProviderQuotaResetMode::Cycle {
                result.cycle_start = op.effective_at_unix_secs;
                days = op.cycle_days.unwrap_or(days);
                result.cycle_days = days;
            }
            if op.reset_usage {
                result.epoch_start = op.effective_at_unix_secs;
                result.reset_usage = true;
            }
            result.applied_pending = true;
        }
        let period = days * 86_400;
        if now >= result.cycle_start.saturating_add(period) {
            result.cycle_start += ((now - result.cycle_start) / period) * period;
            result.epoch_start = result.epoch_start.max(result.cycle_start);
            result.reset_usage = true;
        }
        if self.quota_last_reset_at_unix_secs.is_none() {
            result.reset_usage = true;
        }
        (result.reset_usage || result.applied_pending).then_some(result)
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ProviderQuotaRecovery {
    pub provider_id: String,
    pub candidate_id: String,
    pub delta_id: String,
    pub quota_epoch_start: u64,
    pub known_cost_usd: f64,
    pub reason: String,
    pub applied: bool,
}
