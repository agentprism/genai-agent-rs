//! Cumulative usage and integer monetary arithmetic from Architecture v2 part
//! 1 §3.9 and part 2 §5.2.

use serde::{Deserialize, Serialize};
use std::fmt;

/// Number of tokens in the denominator of every [`MoneyRate`].
pub const TOKENS_PER_PRICE_RATE: i128 = 1_000_000;

/// Provenance of a cumulative usage total (Architecture v2 part 1 §3.9).
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UsageSource {
    /// Values were reported by the provider.
    ProviderReported,
    /// Values were estimated locally.
    Estimated,
    /// The total combines provider-reported and locally estimated fields.
    Mixed,
    /// The source is unavailable, such as for an imported legacy record.
    Unknown,
}

/// Cumulative token usage for one model response (Architecture v2 part 1 §3.9).
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Usage {
    /// Non-cache input tokens.
    pub input_tokens: u64,
    /// Output tokens, including reasoning tokens where the provider does so.
    pub output_tokens: u64,
    /// Provider-reported reasoning subset of output, when available.
    pub reasoning_tokens: Option<u64>,
    /// Cache-read input tokens, when available.
    pub cache_read_tokens: Option<u64>,
    /// Cache-write input tokens, when available.
    pub cache_write_tokens: Option<u64>,
    /// One-hour-retention subset of cache-write tokens, when reported.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_write_one_hour_tokens: Option<u64>,
    /// Provenance of these cumulative values.
    pub source: UsageSource,
}

impl Usage {
    /// Creates a zero cumulative usage value.
    pub const fn zero(source: UsageSource) -> Self {
        Self {
            input_tokens: 0,
            output_tokens: 0,
            reasoning_tokens: None,
            cache_read_tokens: None,
            cache_write_tokens: None,
            cache_write_one_hour_tokens: None,
            source,
        }
    }

    /// Returns all input-side tokens used for request-wide tier selection.
    ///
    /// This matches Pi's `input + cacheRead + cacheWrite` selection rule.
    pub fn request_input_tokens(&self) -> u128 {
        u128::from(self.input_tokens)
            + u128::from(self.cache_read_tokens.unwrap_or(0))
            + u128::from(self.cache_write_tokens.unwrap_or(0))
    }

    /// Returns the cumulative token total without double-counting reasoning.
    pub fn total_tokens(&self) -> u128 {
        self.request_input_tokens() + u128::from(self.output_tokens)
    }
}

/// An open ISO-style currency code (Architecture v2 part 1 §3.9).
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Currency(
    /// Currency code, conventionally `USD` for built-in model pricing.
    pub String,
);

impl Currency {
    /// Creates an open currency code.
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// Returns the currency code.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Returns United States dollars.
    pub fn usd() -> Self {
        Self::new("USD")
    }
}

impl fmt::Display for Currency {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// Integer monetary cost in millionths of a currency unit
/// (Architecture v2 part 1 §3.9).
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Cost {
    /// Currency in which the cost is denominated.
    pub currency: Currency,
    /// Cost in millionths of the currency unit.
    pub micros: i128,
}

/// Integer micro-currency units per million tokens
/// (Architecture v2 part 2 §5.2).
#[derive(Clone, Copy, Debug, Default, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(transparent)]
pub struct MoneyRate(
    /// Micro-currency units charged per million tokens.
    pub i128,
);

impl MoneyRate {
    /// Creates an integer price rate.
    pub const fn new(micros_per_million_tokens: i128) -> Self {
        Self(micros_per_million_tokens)
    }

    /// Calculates the micro-unit charge for a token count.
    ///
    /// Sub-micro-unit fractions are truncated, matching integer fixed-point
    /// arithmetic without ever overstating the provider charge.
    pub fn cost_for_tokens(self, tokens: u64) -> Result<i128, CostArithmeticError> {
        self.0
            .checked_mul(i128::from(tokens))
            .map(|value| value / TOKENS_PER_PRICE_RATE)
            .ok_or(CostArithmeticError::Overflow)
    }
}

/// Per-token-category price rates (Architecture v2 part 2 §5.2).
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct TokenPriceRates {
    /// Non-cache input token rate.
    pub input: MoneyRate,
    /// Output token rate.
    pub output: MoneyRate,
    /// Cache-read token rate.
    pub cache_read: MoneyRate,
    /// Default cache-write token rate.
    pub cache_write: MoneyRate,
}

/// A request-wide price tier selected by total input usage
/// (Architecture v2 part 2 §5.2).
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct RequestWidePriceTier {
    /// Strict lower bound for selecting this tier, matching Pi's `>` rule.
    pub input_tokens_above: u64,
    /// Rates applied to the entire request after the tier matches.
    pub rates: TokenPriceRates,
}

/// Cache-write rates that depend on requested retention
/// (Architecture v2 part 2 §5.2).
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct CacheWriteRetentionPricing {
    /// Optional short-retention cache-write override.
    pub short: Option<MoneyRate>,
    /// Optional one-hour cache-write override.
    pub one_hour: Option<MoneyRate>,
}

/// Retention class used when pricing cache-write tokens
/// (Architecture v2 part 2 §5.2).
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CacheWriteRetention {
    /// Use the selected tier's ordinary cache-write rate.
    #[default]
    Default,
    /// Use the short-retention override when configured.
    Short,
    /// Use the one-hour override when configured.
    OneHour,
}

/// Complete integer model pricing (Architecture v2 part 2 §5.2).
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ModelPricing {
    /// Default rates when no request-wide tier matches.
    pub default: TokenPriceRates,
    /// Request-wide tiers; the highest matching threshold wins.
    pub request_wide_tiers: Vec<RequestWidePriceTier>,
    /// Retention-specific cache-write overrides.
    pub cache_write_retention: CacheWriteRetentionPricing,
}

impl ModelPricing {
    /// Selects rates using Pi's highest-strictly-exceeded threshold rule.
    pub fn rates_for(&self, usage: &Usage) -> &TokenPriceRates {
        let input_tokens = usage.request_input_tokens();
        self.request_wide_tiers
            .iter()
            .filter(|tier| input_tokens > u128::from(tier.input_tokens_above))
            .max_by_key(|tier| tier.input_tokens_above)
            .map_or(&self.default, |tier| &tier.rates)
    }

    /// Calculates a response cost using checked integer arithmetic.
    pub fn calculate_cost(
        &self,
        usage: &Usage,
        currency: Currency,
        retention: CacheWriteRetention,
    ) -> Result<Cost, CostArithmeticError> {
        self.calculate_cost_with_multiplier(usage, currency, retention, 1, 1)
    }

    /// Calculates a response cost and applies an exact rational multiplier.
    ///
    /// API families use this for request-wide service tiers such as one-half
    /// (`flex`) and five-halves (`gpt-5.5` priority) without floating-point
    /// money or premature per-rate rounding.
    pub fn calculate_cost_with_multiplier(
        &self,
        usage: &Usage,
        currency: Currency,
        retention: CacheWriteRetention,
        multiplier_numerator: i128,
        multiplier_denominator: i128,
    ) -> Result<Cost, CostArithmeticError> {
        if multiplier_denominator <= 0 {
            return Err(CostArithmeticError::InvalidMultiplier);
        }
        let rates = self.rates_for(usage);
        let ordinary_cache_write_rate = match retention {
            CacheWriteRetention::Default => rates.cache_write,
            CacheWriteRetention::Short => self
                .cache_write_retention
                .short
                .unwrap_or(rates.cache_write),
            CacheWriteRetention::OneHour => rates.cache_write,
        };

        let total_cache_write = usage.cache_write_tokens.unwrap_or(0);
        let one_hour_tokens = usage.cache_write_one_hour_tokens.unwrap_or(0);
        let one_hour_rate = self
            .cache_write_retention
            .one_hour
            .unwrap_or(rates.cache_write);
        let one_hour_uplift = MoneyRate::new(
            one_hour_rate
                .0
                .checked_sub(ordinary_cache_write_rate.0)
                .ok_or(CostArithmeticError::Overflow)?,
        );
        let priced_tokens = [
            (rates.input, usage.input_tokens),
            (rates.output, usage.output_tokens),
            (rates.cache_read, usage.cache_read_tokens.unwrap_or(0)),
            (ordinary_cache_write_rate, total_cache_write),
            (one_hour_uplift, one_hour_tokens),
        ];
        let numerator = priced_tokens
            .into_iter()
            .try_fold(0_i128, |total, (rate, tokens)| {
                let part = rate
                    .0
                    .checked_mul(i128::from(tokens))
                    .ok_or(CostArithmeticError::Overflow)?;
                total.checked_add(part).ok_or(CostArithmeticError::Overflow)
            })?;
        let scaled_numerator = numerator
            .checked_mul(multiplier_numerator)
            .ok_or(CostArithmeticError::Overflow)?;
        let denominator = TOKENS_PER_PRICE_RATE
            .checked_mul(multiplier_denominator)
            .ok_or(CostArithmeticError::Overflow)?;
        let micros = scaled_numerator / denominator;

        Ok(Cost { currency, micros })
    }
}

/// Failure from checked integer price arithmetic
/// (Architecture v2 part 2 §5.2).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CostArithmeticError {
    /// A multiplication or addition exceeded `u128`.
    Overflow,
    /// A rational multiplier used a zero or negative denominator.
    InvalidMultiplier,
}

impl fmt::Display for CostArithmeticError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Overflow => formatter.write_str("integer cost arithmetic overflowed"),
            Self::InvalidMultiplier => {
                formatter.write_str("cost multiplier denominator must be positive")
            }
        }
    }
}

impl std::error::Error for CostArithmeticError {}
