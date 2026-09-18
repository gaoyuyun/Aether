//! Availability-oriented estimates for provider subscription admission. These are
//! reservations, never a replacement for measured usage or a hard output limit.
use serde::Deserialize;
use serde_json::Value;

use crate::{
    BillingModelPricingSnapshot, BillingService, BillingSnapshotStatus, BillingUsageInput,
};

#[derive(Debug, Clone, Default)]
pub struct ProviderQuotaReservationInput {
    pub input_tokens: i64,
    pub max_output_tokens: Option<u64>,
    pub processing_tier: Option<String>,
    pub reasoning_effort: Option<String>,
    pub choices: u64,
}

impl ProviderQuotaReservationInput {
    pub fn from_body(body: Option<&Value>) -> Self {
        let Some(body) = body else {
            return Self::default();
        };
        fn tokens(value: &Value) -> u64 {
            match value {
                Value::String(text) if text.starts_with("data:image/") => 4096,
                Value::String(text) => {
                    let ascii = text.bytes().filter(u8::is_ascii).count() as u64;
                    // UTF-8 text and code have different token densities. This is
                    // deliberately an estimate and does not trust client token counts.
                    ascii.div_ceil(3) + (text.len() as u64 - ascii).div_ceil(2)
                }
                Value::Array(values) => values.iter().map(tokens).sum(),
                Value::Object(values) => values.values().map(tokens).sum(),
                _ => 0,
            }
        }
        let input_tokens = [
            "input",
            "messages",
            "instructions",
            "system",
            "tools",
            "prompt",
            "contents",
        ]
        .iter()
        .filter_map(|key| body.get(key))
        .map(tokens)
        .sum::<u64>();
        Self {
            input_tokens: input_tokens.min(i64::MAX as u64) as i64,
            max_output_tokens: ["max_output_tokens", "max_completion_tokens", "max_tokens"]
                .iter()
                .filter_map(|key| body.get(key).and_then(Value::as_u64))
                .max(),
            processing_tier: body
                .get("service_tier")
                .and_then(Value::as_str)
                .map(str::to_owned),
            reasoning_effort: body
                .pointer("/reasoning/effort")
                .or_else(|| body.get("reasoning_effort"))
                .and_then(Value::as_str)
                .map(str::to_owned),
            choices: body
                .get("n")
                .and_then(Value::as_u64)
                .unwrap_or(1)
                .clamp(1, 128),
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct ReservationPolicy {
    minimum_usd: f64,
    fallback_usd: f64,
    output_tokens: u64,
    safety_multiplier: f64,
    /// Share of the request input expected to be served from the provider's
    /// prompt cache and therefore billed at the cache-read rate. Agent clients
    /// resend almost the same context every turn, so pricing the whole input at
    /// the fresh rate over-reserves rolling windows by an order of magnitude.
    cached_input_ratio: f64,
}

impl Default for ReservationPolicy {
    fn default() -> Self {
        Self {
            minimum_usd: 0.01,
            fallback_usd: 0.5,
            output_tokens: 4096,
            safety_multiplier: 1.25,
            cached_input_ratio: 0.0,
        }
    }
}

impl ReservationPolicy {
    fn is_valid(&self) -> bool {
        self.minimum_usd.is_finite()
            && self.fallback_usd.is_finite()
            && self.safety_multiplier.is_finite()
            && self.cached_input_ratio.is_finite()
            && self.minimum_usd >= 0.0
            && self.fallback_usd > 0.0
            && (1.0..=10.0).contains(&self.safety_multiplier)
            && (1..=1_000_000).contains(&self.output_tokens)
            && (0.0..=1.0).contains(&self.cached_input_ratio)
    }
}

/// Splits the estimated input into fresh and cache-read tokens according to the
/// configured ratio. The total context size is unchanged so tier selection still
/// sees the full prompt.
fn split_cached_input_tokens(input_tokens: i64, cached_input_ratio: f64) -> (i64, i64) {
    let input_tokens = input_tokens.max(0);
    if input_tokens == 0 || cached_input_ratio <= 0.0 {
        return (input_tokens, 0);
    }
    let cache_read_tokens = ((input_tokens as f64) * cached_input_ratio.min(1.0)).floor() as i64;
    let cache_read_tokens = cache_read_tokens.clamp(0, input_tokens);
    (input_tokens - cache_read_tokens, cache_read_tokens)
}

pub fn estimate_provider_quota_reservation(
    pricing: &BillingModelPricingSnapshot,
    api_format: &str,
    input: &ProviderQuotaReservationInput,
    config: Option<&Value>,
) -> Result<f64, String> {
    if api_format.eq_ignore_ascii_case("openai:search") {
        return Ok(0.0);
    }
    let policy: ReservationPolicy = config
        .and_then(|v| v.get("quota_reservation"))
        .cloned()
        .map(serde_json::from_value)
        .transpose()
        .map_err(|e| e.to_string())?
        .unwrap_or_default();
    if !policy.is_valid() {
        return Err("invalid provider quota reservation policy".to_owned());
    }
    let budget = policy.output_tokens.saturating_mul(
        if matches!(
            input.reasoning_effort.as_deref(),
            Some("high" | "xhigh" | "max" | "ultra")
        ) {
            2
        } else {
            1
        },
    );
    let output_tokens = input
        .max_output_tokens
        .map(|n| n.min(budget))
        .unwrap_or(budget)
        .saturating_mul(input.choices.max(1))
        .min(i64::MAX as u64) as i64;
    let (fresh_input_tokens, cache_read_tokens) =
        split_cached_input_tokens(input.input_tokens, policy.cached_input_ratio);
    let mut usage = BillingUsageInput::new("chat");
    usage.api_format = Some(api_format.to_owned());
    // OpenAI-style accounting reports cached tokens inside `input_tokens`; the
    // billing normalizer subtracts them again. Claude/Gemini report them apart.
    usage.input_tokens = if api_format
        .split(':')
        .next()
        .is_some_and(|family| family.eq_ignore_ascii_case("openai"))
    {
        fresh_input_tokens.saturating_add(cache_read_tokens)
    } else {
        fresh_input_tokens
    };
    usage.cache_read_tokens = cache_read_tokens;
    usage.output_tokens = output_tokens;
    usage.requested_processing_tier = input.processing_tier.clone();
    let result = BillingService::new()
        .calculate(pricing, &usage)
        .map_err(|e| e.to_string())?;
    let estimate =
        if result.cost_result.status == BillingSnapshotStatus::Complete && input.input_tokens > 0 {
            result.provider_quota_cost_usd
        } else {
            policy.fallback_usd
        };
    let reserved = (estimate * policy.safety_multiplier).max(policy.minimum_usd);
    if !reserved.is_finite() || reserved > 1_000_000_000.0 {
        return Err("provider quota reservation is outside the supported range".to_owned());
    }
    Ok((reserved * 100_000_000.0).ceil() / 100_000_000.0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn pricing() -> BillingModelPricingSnapshot {
        serde_json::from_value(json!({
            "provider_id":"monthly", "provider_billing_type":"monthly_quota",
            "global_model_id":"model", "global_model_name":"model",
            "default_tiered_pricing":{"tiers":[{"up_to":null,"input_price_per_1m":10,"output_price_per_1m":50}]}
        })).unwrap()
    }

    #[test]
    fn missing_output_limit_is_estimated_without_changing_the_request() {
        let body = json!({"input":"a".repeat(3000)});
        let input = ProviderQuotaReservationInput::from_body(Some(&body));
        let cost =
            estimate_provider_quota_reservation(&pricing(), "openai:responses", &input, None)
                .unwrap();
        assert!(cost > 0.2 && cost < 1.0);
        assert!(body.get("max_output_tokens").is_none());
        let explicit = ProviderQuotaReservationInput::from_body(Some(
            &json!({"input":"a".repeat(3000),"max_output_tokens":128000}),
        ));
        assert_eq!(
            cost,
            estimate_provider_quota_reservation(&pricing(), "openai:responses", &explicit, None)
                .unwrap()
        );
    }

    #[test]
    fn request_scale_output_and_model_prices_change_the_reservation() {
        let input = ProviderQuotaReservationInput::from_body(Some(
            &json!({"input":"a".repeat(3000),"max_output_tokens":100}),
        ));
        let small =
            estimate_provider_quota_reservation(&pricing(), "openai:responses", &input, None)
                .unwrap();
        let large = ProviderQuotaReservationInput::from_body(Some(
            &json!({"input":"a".repeat(300000),"max_output_tokens":100}),
        ));
        assert!(
            estimate_provider_quota_reservation(&pricing(), "openai:responses", &large, None)
                .unwrap()
                > small * 10.0
        );
        let mut expensive = pricing();
        expensive.provider_api_key_rate_multipliers = Some(json!({"openai:responses":2}));
        assert!(
            estimate_provider_quota_reservation(&expensive, "openai:responses", &input, None)
                .unwrap()
                > small
        );
    }

    #[test]
    fn cached_input_ratio_prices_the_cached_share_at_the_cache_read_rate() {
        let pricing: BillingModelPricingSnapshot = serde_json::from_value(json!({
            "provider_id":"monthly", "provider_billing_type":"monthly_quota",
            "global_model_id":"model", "global_model_name":"model",
            "default_tiered_pricing":{"tiers":[{"up_to":null,"input_price_per_1m":10,"output_price_per_1m":50,"cache_read_price_per_1m":1}]}
        })).unwrap();
        // ~100k input tokens, explicit small output limit so the input dominates.
        let input = ProviderQuotaReservationInput::from_body(Some(
            &json!({"input":"a".repeat(300_000),"max_output_tokens":10}),
        ));
        let fresh = estimate_provider_quota_reservation(&pricing, "openai:responses", &input, None)
            .unwrap();
        let mostly_cached = estimate_provider_quota_reservation(
            &pricing,
            "openai:responses",
            &input,
            Some(&json!({"quota_reservation":{"cached_input_ratio":0.9}})),
        )
        .unwrap();
        let claude_cached = estimate_provider_quota_reservation(
            &pricing,
            "claude:messages",
            &input,
            Some(&json!({"quota_reservation":{"cached_input_ratio":0.9}})),
        )
        .unwrap();

        // 100% fresh: 100k * $10/1M * 1.25 = $1.25 (+ tiny output).
        assert!(fresh > 1.2 && fresh < 1.3, "fresh estimate was {fresh}");
        // 90% cached: (10k * $10 + 90k * $1) / 1M * 1.25 = $0.2375 (+ tiny output).
        assert!(
            mostly_cached > 0.23 && mostly_cached < 0.26,
            "cached estimate was {mostly_cached}"
        );
        assert!((claude_cached - mostly_cached).abs() < 0.001);
        assert!(estimate_provider_quota_reservation(
            &pricing,
            "openai:responses",
            &input,
            Some(&json!({"quota_reservation":{"cached_input_ratio":1.5}}))
        )
        .is_err());
        assert!(estimate_provider_quota_reservation(
            &pricing,
            "openai:responses",
            &input,
            Some(&json!({"quota_reservation":{"cached_input_ratio":-0.1}}))
        )
        .is_err());
    }

    #[test]
    fn free_search_and_unavailable_input_have_explicit_policies() {
        let input = ProviderQuotaReservationInput::default();
        assert_eq!(
            estimate_provider_quota_reservation(&pricing(), "openai:search", &input, None).unwrap(),
            0.0
        );
        assert_eq!(
            estimate_provider_quota_reservation(&pricing(), "openai:responses", &input, None)
                .unwrap(),
            0.625
        );
        assert!(estimate_provider_quota_reservation(
            &pricing(),
            "openai:responses",
            &input,
            Some(&json!({"quota_reservation":{"safety_multiplier":0}}))
        )
        .is_err());
    }
}
