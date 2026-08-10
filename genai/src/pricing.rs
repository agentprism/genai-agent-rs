//! Application-supplied model pricing and per-response cost computation.
//!
//! `genai` carries no price metadata (its [`genai::ModelIden`] only names a target), so monetary
//! cost accounting is opt-in. An application implements [`PriceCatalog`] over its own model
//! catalog and hands it to [`crate::GenaiStreamFn::with_price_catalog`]; the crate then applies
//! [`compute_cost`] where usage is finalized and stores the result on [`crate::AgentUsage::cost`].
//!
//! This is the provider-neutral analogue of pi-ai's model-catalog cost step
//! (`packages/ai/src/models.ts` `calculateCost`). The tier rule is copied faithfully; the one
//! deliberate generalization is 1h-retention cache-write pricing, which pi-ai hardcodes to `2x`
//! base input for Anthropic and this crate exposes as an explicit, catalog-supplied rate.

use crate::{AgentCost, AgentUsage};
use genai::ModelIden;

/// Per-token price rates for one model or pricing tier, in dollars per million tokens.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct ModelCostRates {
    /// Price per million fresh (non-cached) input tokens.
    pub input: f64,
    /// Price per million output tokens.
    pub output: f64,
    /// Price per million cache-read (cache-hit) input tokens.
    pub cache_read: f64,
    /// Price per million cache-write (cache-creation) input tokens at default retention.
    pub cache_write: f64,
    /// Price per million cache-write tokens written with 1h retention, when the provider reports
    /// that split. `None` falls back to [`Self::cache_write`].
    ///
    /// This generalizes pi-ai's hardcoded Anthropic rule (1h writes billed at `2x` base input)
    /// into an explicit, catalog-supplied rate: no multiplier is baked into the crate.
    pub cache_write_1h: Option<f64>,
}

/// A request-wide pricing tier keyed by an input-token threshold.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct ModelCostTier {
    /// The tier applies to a request whose input token count is strictly greater than this value.
    pub input_tokens_above: u64,
    /// Rates charged for the whole request when this tier is the highest one that matches.
    pub rates: ModelCostRates,
}

/// Full pricing for one model: base rates plus optional request-wide tiers.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ModelCost {
    /// Base rates, used when no tier matches.
    pub rates: ModelCostRates,
    /// Request-wide tiers. The highest tier whose threshold is strictly below the request's input
    /// token count applies its rates to the entire request.
    pub tiers: Vec<ModelCostTier>,
}

impl ModelCost {
    /// Construct pricing with the given base rates and no tiers.
    pub fn new(rates: ModelCostRates) -> Self {
        Self {
            rates,
            tiers: Vec::new(),
        }
    }
}

/// Application-supplied source of model pricing.
///
/// Because `genai` provides no price catalog, cost accounting is opt-in: an application implements
/// this trait over its own model metadata and hands it to
/// [`crate::GenaiStreamFn::with_price_catalog`] (or carries it on
/// `AgentConfig::price_catalog` in the agent crate as a convenience).
pub trait PriceCatalog: Send + Sync {
    /// Return pricing for `model`, or `None` when the model is unknown or unpriced.
    fn cost_model(&self, model: &ModelIden) -> Option<ModelCost>;
}

/// Compute the monetary cost of one response from its usage and the model's pricing.
///
/// Tier selection mirrors pi-ai (`models.ts` `calculateCost`): the request's input token count is
/// `input + cache_read + cache_write`, and the highest tier whose [`ModelCostTier::input_tokens_above`]
/// is strictly below that count applies its rates to the whole request. When no tier matches, the
/// base [`ModelCost::rates`] apply.
///
/// [`AgentUsage::cache_write_1h_tokens`] (when reported) is priced at
/// [`ModelCostRates::cache_write_1h`], falling back to [`ModelCostRates::cache_write`] when that
/// rate is `None`; the remaining cache-write tokens are priced at [`ModelCostRates::cache_write`].
/// [`AgentCost::total`] is the sum of the components.
pub fn compute_cost(usage: &AgentUsage, model_cost: &ModelCost) -> AgentCost {
    let input_tokens = usage.input_tokens + usage.cache_read_tokens + usage.cache_write_tokens;

    // Highest tier whose threshold is strictly below the request's input token count wins. The -1
    // sentinel lets a tier with `input_tokens_above: 0` still apply once input tokens exceed 0.
    let mut rates = &model_cost.rates;
    let mut matched_threshold: i128 = -1;
    for tier in &model_cost.tiers {
        if input_tokens > tier.input_tokens_above
            && i128::from(tier.input_tokens_above) > matched_threshold
        {
            rates = &tier.rates;
            matched_threshold = i128::from(tier.input_tokens_above);
        }
    }

    // 1h writes are a subset of cache writes; clamp defensively so the default-retention remainder
    // never underflows if a provider ever over-reports the split.
    let long_write = usage
        .cache_write_1h_tokens
        .unwrap_or(0)
        .min(usage.cache_write_tokens);
    let short_write = usage.cache_write_tokens - long_write;
    let cache_write_1h_rate = rates.cache_write_1h.unwrap_or(rates.cache_write);

    const PER_MILLION: f64 = 1_000_000.0;
    let input = rates.input / PER_MILLION * usage.input_tokens as f64;
    let output = rates.output / PER_MILLION * usage.output_tokens as f64;
    let cache_read = rates.cache_read / PER_MILLION * usage.cache_read_tokens as f64;
    let cache_write = (rates.cache_write * short_write as f64
        + cache_write_1h_rate * long_write as f64)
        / PER_MILLION;
    let total = input + output + cache_read + cache_write;

    AgentCost {
        input,
        output,
        cache_read,
        cache_write,
        total,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_close(actual: f64, expected: f64) {
        assert!(
            (actual - expected).abs() < 1e-12,
            "expected {expected}, got {actual}"
        );
    }

    fn base_rates() -> ModelCostRates {
        ModelCostRates {
            input: 3.0,
            output: 15.0,
            cache_read: 0.3,
            cache_write: 3.75,
            cache_write_1h: None,
        }
    }

    #[test]
    fn compute_cost_without_tiers_uses_base_rates() {
        let usage = AgentUsage {
            input_tokens: 1_000_000,
            output_tokens: 2_000_000,
            ..AgentUsage::default()
        };
        let model_cost = ModelCost::new(base_rates());

        let cost = compute_cost(&usage, &model_cost);

        assert_close(cost.input, 3.0);
        assert_close(cost.output, 30.0);
        assert_close(cost.cache_read, 0.0);
        assert_close(cost.cache_write, 0.0);
        assert_close(cost.total, 33.0);
    }

    #[test]
    fn compute_cost_applies_highest_matching_tier_to_whole_request() {
        // Input token count for tier selection is input + cache_read + cache_write = 250_000.
        let usage = AgentUsage {
            input_tokens: 250_000,
            output_tokens: 0,
            ..AgentUsage::default()
        };
        let model_cost = ModelCost {
            rates: base_rates(),
            tiers: vec![
                ModelCostTier {
                    input_tokens_above: 200_000,
                    rates: ModelCostRates {
                        input: 6.0,
                        ..base_rates()
                    },
                },
                // A higher, non-matching tier must be ignored (threshold above the request count).
                ModelCostTier {
                    input_tokens_above: 1_000_000,
                    rates: ModelCostRates {
                        input: 12.0,
                        ..base_rates()
                    },
                },
            ],
        };

        let cost = compute_cost(&usage, &model_cost);

        // Whole request priced at the 200k tier's $6/M rate, not the base $3/M or the 1M tier.
        assert_close(cost.input, 1.5);
        assert_close(cost.total, 1.5);
    }

    #[test]
    fn compute_cost_prices_1h_split_at_dedicated_rate() {
        let usage = AgentUsage {
            input_tokens: 0,
            output_tokens: 0,
            cache_write_tokens: 100,
            cache_write_1h_tokens: Some(40),
            ..AgentUsage::default()
        };
        let model_cost = ModelCost::new(ModelCostRates {
            cache_write: 3.75,
            cache_write_1h: Some(6.0),
            ..base_rates()
        });

        let cost = compute_cost(&usage, &model_cost);

        // 60 short-retention writes at $3.75/M + 40 1h writes at $6/M.
        let expected = (3.75 * 60.0 + 6.0 * 40.0) / 1_000_000.0;
        assert_close(cost.cache_write, expected);
        assert_close(cost.total, expected);
    }

    #[test]
    fn compute_cost_1h_rate_none_falls_back_to_cache_write_rate() {
        let split = AgentUsage {
            cache_write_tokens: 100,
            cache_write_1h_tokens: Some(40),
            ..AgentUsage::default()
        };
        let no_split = AgentUsage {
            cache_write_tokens: 100,
            cache_write_1h_tokens: None,
            ..AgentUsage::default()
        };
        let model_cost = ModelCost::new(ModelCostRates {
            cache_write: 3.75,
            cache_write_1h: None,
            ..base_rates()
        });

        let split_cost = compute_cost(&split, &model_cost);
        let no_split_cost = compute_cost(&no_split, &model_cost);

        // With no dedicated 1h rate, all 100 cache-write tokens price at cache_write regardless of
        // the split, so the two costs match.
        let expected = 3.75 * 100.0 / 1_000_000.0;
        assert_close(split_cost.cache_write, expected);
        assert_close(no_split_cost.cache_write, expected);
        assert_close(split_cost.total, no_split_cost.total);
    }
}
