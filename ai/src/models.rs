//! Pure model helpers ⇐ pi `src/models.ts`; grows into the `Models` router in later phases.

use crate::types::{Model, ModelThinkingLevel, ThinkingLevelMap, Usage, UsageCost};

const EXTENDED_THINKING_LEVELS: [ModelThinkingLevel; 7] = [
    ModelThinkingLevel::Off,
    ModelThinkingLevel::Minimal,
    ModelThinkingLevel::Low,
    ModelThinkingLevel::Medium,
    ModelThinkingLevel::High,
    ModelThinkingLevel::Xhigh,
    ModelThinkingLevel::Max,
];

pub fn calculate_cost<'a>(model: &Model, usage: &'a mut Usage) -> &'a UsageCost {
    let input_tokens =
        u128::from(usage.input) + u128::from(usage.cache_read) + u128::from(usage.cache_write);
    let mut rates = &model.cost.rates;
    let mut matched_threshold = None;
    for tier in model.cost.tiers.iter().flatten() {
        let threshold = u128::from(tier.input_tokens_above);
        if input_tokens > threshold && matched_threshold.is_none_or(|matched| threshold > matched) {
            rates = &tier.rates;
            matched_threshold = Some(threshold);
        }
    }

    let long_write = usage.cache_write_1h.unwrap_or(0) as f64;
    let short_write = usage.cache_write as f64 - long_write;
    usage.cost.input = rates.input / 1_000_000.0 * usage.input as f64;
    usage.cost.output = rates.output / 1_000_000.0 * usage.output as f64;
    usage.cost.cache_read = rates.cache_read / 1_000_000.0 * usage.cache_read as f64;
    usage.cost.cache_write =
        (rates.cache_write * short_write + rates.input * 2.0 * long_write) / 1_000_000.0;
    usage.cost.total =
        usage.cost.input + usage.cost.output + usage.cost.cache_read + usage.cost.cache_write;
    &usage.cost
}

fn mapped_level(
    map: Option<&ThinkingLevelMap>,
    level: ModelThinkingLevel,
) -> Option<&Option<String>> {
    let map = map?;
    match level {
        ModelThinkingLevel::Off => map.off.as_ref(),
        ModelThinkingLevel::Minimal => map.minimal.as_ref(),
        ModelThinkingLevel::Low => map.low.as_ref(),
        ModelThinkingLevel::Medium => map.medium.as_ref(),
        ModelThinkingLevel::High => map.high.as_ref(),
        ModelThinkingLevel::Xhigh => map.xhigh.as_ref(),
        ModelThinkingLevel::Max => map.max.as_ref(),
    }
}

pub fn get_supported_thinking_levels(model: &Model) -> Vec<ModelThinkingLevel> {
    if !model.reasoning {
        return vec![ModelThinkingLevel::Off];
    }
    EXTENDED_THINKING_LEVELS
        .iter()
        .copied()
        .filter(|level| {
            let mapped = mapped_level(model.thinking_level_map.as_ref(), *level);
            if mapped == Some(&None) {
                return false;
            }
            !matches!(level, ModelThinkingLevel::Xhigh | ModelThinkingLevel::Max)
                || mapped.is_some()
        })
        .collect()
}

pub fn clamp_thinking_level(model: &Model, level: ModelThinkingLevel) -> ModelThinkingLevel {
    let available = get_supported_thinking_levels(model);
    if available.contains(&level) {
        return level;
    }
    let requested = EXTENDED_THINKING_LEVELS
        .iter()
        .position(|candidate| *candidate == level);
    let Some(requested) = requested else {
        return available
            .first()
            .copied()
            .unwrap_or(ModelThinkingLevel::Off);
    };
    for candidate in &EXTENDED_THINKING_LEVELS[requested..] {
        if available.contains(candidate) {
            return *candidate;
        }
    }
    for candidate in EXTENDED_THINKING_LEVELS[..requested].iter().rev() {
        if available.contains(candidate) {
            return *candidate;
        }
    }
    ModelThinkingLevel::Off
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::ModelCostRates;
    use crate::types::*;

    fn model() -> Model {
        Model {
            id: "test".to_owned(),
            name: "Test".to_owned(),
            api: "anthropic-messages".into(),
            provider: "anthropic".into(),
            base_url: "https://example.test".to_owned(),
            reasoning: true,
            thinking_level_map: None,
            input: vec![ModelInput::Text],
            cost: ModelCost::default(),
            context_window: 128_000,
            max_tokens: 4_096,
            sampling_params: None,
            headers: None,
            compat: None,
        }
    }

    /// Ports the cost-tier case in pi `test/models-runtime.test.ts:122-160`.
    #[test]
    fn tiered_costs_and_one_hour_cache_writes_match_pi() {
        let mut model = model();
        model.cost = ModelCost {
            rates: ModelCostRates {
                input: 5.0,
                output: 30.0,
                cache_read: 0.5,
                cache_write: 6.25,
            },
            tiers: Some(vec![ModelCostTier {
                rates: ModelCostRates {
                    input: 10.0,
                    output: 45.0,
                    cache_read: 1.0,
                    cache_write: 12.5,
                },
                input_tokens_above: 272_000,
            }]),
        };
        let usage = |cache_write| Usage {
            input: 200_000,
            output: 100_000,
            cache_read: 72_000,
            cache_write,
            cache_write_1h: None,
            reasoning: None,
            total_tokens: 372_000 + cache_write,
            cost: UsageCost::default(),
        };
        let mut short = usage(0);
        let short_cost = calculate_cost(&model, &mut short);
        assert_eq!(short_cost.input, 1.0);
        assert_eq!(short_cost.output, 3.0);
        assert_eq!(short_cost.cache_read, 0.036);
        assert_eq!(short_cost.cache_write, 0.0);

        let mut long = usage(1);
        let long_cost = calculate_cost(&model, &mut long);
        assert_eq!(long_cost.input, 2.0);
        assert_eq!(long_cost.output, 4.5);
        assert_eq!(long_cost.cache_read, 0.072);
        assert_eq!(long_cost.cache_write, 0.000_012_5);
    }

    /// Derived from pi `src/models.ts:889-896` for the one-hour cache-write split.
    #[test]
    fn one_hour_cache_writes_use_twice_the_base_input_rate() {
        let mut model = model();
        model.cost.rates = ModelCostRates {
            input: 5.0,
            output: 0.0,
            cache_read: 0.0,
            cache_write: 6.25,
        };
        let mut usage = Usage {
            cache_write: 100,
            cache_write_1h: Some(40),
            ..Usage::default()
        };
        assert_eq!(calculate_cost(&model, &mut usage).cache_write, 0.000_775);
    }

    /// Derived from pi `src/models.ts:900-930` and the model-helper test cases.
    #[test]
    fn supported_levels_preserve_missing_vs_explicit_null() {
        let mut model = model();
        assert_eq!(
            get_supported_thinking_levels(&model),
            vec![
                ModelThinkingLevel::Off,
                ModelThinkingLevel::Minimal,
                ModelThinkingLevel::Low,
                ModelThinkingLevel::Medium,
                ModelThinkingLevel::High,
            ]
        );
        assert_eq!(
            clamp_thinking_level(&model, ModelThinkingLevel::Max),
            ModelThinkingLevel::High
        );

        model.thinking_level_map = Some(ThinkingLevelMap {
            off: Some(None),
            minimal: Some(None),
            low: Some(None),
            medium: Some(Some("medium".to_owned())),
            high: Some(Some("high".to_owned())),
            xhigh: Some(Some("xhigh".to_owned())),
            max: Some(None),
        });
        assert_eq!(
            get_supported_thinking_levels(&model),
            vec![
                ModelThinkingLevel::Medium,
                ModelThinkingLevel::High,
                ModelThinkingLevel::Xhigh,
            ]
        );
        assert_eq!(
            clamp_thinking_level(&model, ModelThinkingLevel::Low),
            ModelThinkingLevel::Medium
        );
        assert_eq!(
            clamp_thinking_level(&model, ModelThinkingLevel::Max),
            ModelThinkingLevel::Xhigh
        );

        model.reasoning = false;
        assert_eq!(
            get_supported_thinking_levels(&model),
            vec![ModelThinkingLevel::Off]
        );
    }
}
