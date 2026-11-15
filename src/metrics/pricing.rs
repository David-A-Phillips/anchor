//! Model prices and exact cost arithmetic.
//!
//! All money is an integer number of **nano-dollars** (`1 USD = 1_000_000_000`), so sums in the
//! ledger are exact. A price of `P` US dollars per million tokens is `P x 1000` nano-dollars per
//! token; configured prices are rounded to the nearest whole nano-dollar per token (that is, to
//! a tenth of a cent per million tokens).
//!
//! # Built-in prices
//!
//! The built-in table holds Anthropic first-party API prices (provider `"anthropic"`) **as
//! published on 2026-11-03**. Prices change: override or extend them with `[[metrics.pricing]]`
//! entries, which always take precedence. No prices are built in for other providers.

use crate::base::{Locality, TokenUsage};

use super::config::{MetricsConfig, ModelPrice};

/// Nano-dollars in one US dollar.
pub const NANOS_PER_USD: i64 = 1_000_000_000;

/// Provider name of the built-in price table.
pub const ANTHROPIC_PROVIDER: &str = "anthropic";

/// Baseline cloud model used to value local calls when the configured System 2 model has no
/// known price.
pub const DEFAULT_BASELINE_MODEL: &str = "claude-opus-5-5";

/// Second price tier for long prompts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LongPromptTier {
    /// Calls whose prompt has more than this many tokens use this tier.
    pub threshold_tokens: u64,
    /// Nano-dollars per prompt token.
    pub input_nanos_per_token: u64,
    /// Nano-dollars per completion token.
    pub output_nanos_per_token: u64,
}

/// Price of one model in nano-dollars per token.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Price {
    /// Nano-dollars per prompt token.
    pub input_nanos_per_token: u64,
    /// Nano-dollars per completion token.
    pub output_nanos_per_token: u64,
    /// Optional long-prompt tier.
    pub long_prompt: Option<LongPromptTier>,
}

impl Price {
    /// A flat price from nano-dollars per token.
    pub const fn flat(input_nanos_per_token: u64, output_nanos_per_token: u64) -> Self {
        Self {
            input_nanos_per_token,
            output_nanos_per_token,
            long_prompt: None,
        }
    }

    /// Exact cost of `usage` in nano-dollars, saturating at `i64::MAX`.
    ///
    /// When the prompt exceeds the long-prompt threshold the whole call (input and output) is
    /// charged at the long-prompt prices.
    pub fn cost_nanos(&self, usage: TokenUsage) -> i64 {
        let (input, output) = match self.long_prompt {
            Some(tier) if usage.prompt_tokens > tier.threshold_tokens => {
                (tier.input_nanos_per_token, tier.output_nanos_per_token)
            }
            _ => (self.input_nanos_per_token, self.output_nanos_per_token),
        };
        // Each product fits in a u128; only their sum can overflow.
        let total = (u128::from(usage.prompt_tokens) * u128::from(input))
            .saturating_add(u128::from(usage.completion_tokens) * u128::from(output));
        i64::try_from(total).unwrap_or(i64::MAX)
    }
}

/// Converts US dollars per million tokens into nano-dollars per token, rounding to the nearest
/// nano-dollar. `None` for negative, non-finite or absurdly large prices.
pub fn usd_per_mtok_to_nanos_per_token(usd_per_mtok: f64) -> Option<u64> {
    if !usd_per_mtok.is_finite() || !(0.0..=1.0e12).contains(&usd_per_mtok) {
        return None;
    }
    // Bounded above by 1e15, so the conversion cannot overflow or lose the integer part.
    Some((usd_per_mtok * 1000.0).round() as u64)
}

/// Nano-dollars as (inexact) US dollars, for display and budget comparisons.
pub fn nanos_to_usd(nanos: i64) -> f64 {
    nanos as f64 / NANOS_PER_USD as f64
}

/// Formats nano-dollars with a fixed number of decimals (0 to 9), rounding half away from zero,
/// e.g. `format_usd_fixed(1_234_500_000, 2) == "$1.23"`.
pub fn format_usd_fixed(nanos: i64, decimals: u32) -> String {
    let decimals = decimals.min(9);
    let unit = 10u128.pow(9 - decimals);
    let magnitude = u128::from(nanos.unsigned_abs());
    let scaled = (magnitude + unit / 2) / unit;
    let scale = 10u128.pow(decimals);
    let sign = if nanos < 0 && scaled > 0 { "-" } else { "" };
    if decimals == 0 {
        format!("{sign}${scaled}")
    } else {
        format!(
            "{sign}${}.{:0width$}",
            scaled / scale,
            scaled % scale,
            width = decimals as usize
        )
    }
}

/// Human-friendly money: two decimals from one cent up, four decimals below that, six decimals
/// for amounts under a hundredth of a cent, and `$0.00` for zero.
pub fn format_usd(nanos: i64) -> String {
    let magnitude = nanos.unsigned_abs();
    if magnitude == 0 || magnitude >= 10_000_000 {
        format_usd_fixed(nanos, 2)
    } else if magnitude >= 100_000 {
        format_usd_fixed(nanos, 4)
    } else {
        format_usd_fixed(nanos, 6)
    }
}

/// Formats a token count with thousands separators (`1,234,567`).
pub fn format_tokens(tokens: u64) -> String {
    let digits = tokens.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (index, ch) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index).is_multiple_of(3) {
            out.push(',');
        }
        out.push(ch);
    }
    out
}

/// Anthropic first-party prices as published on 2026-11-03, in nano-dollars per token
/// (= thousandths of a dollar per million tokens).
const BUILTIN: &[(&str, Price)] = &[
    ("claude-fable-5-2", Price::flat(8_000, 40_000)),
    ("claude-mythos-5-2", Price::flat(8_000, 40_000)),
    ("claude-fable-5-1", Price::flat(10_000, 50_000)),
    ("claude-mythos-5-1", Price::flat(10_000, 50_000)),
    ("claude-fable-5", Price::flat(10_000, 50_000)),
    ("claude-opus-5-5", Price::flat(4_000, 20_000)),
    ("claude-opus-5", Price::flat(5_000, 25_000)),
    ("claude-opus-4-8", Price::flat(5_000, 25_000)),
    ("claude-opus-4-7", Price::flat(5_000, 25_000)),
    ("claude-opus-4-6", Price::flat(5_000, 25_000)),
    ("claude-sonnet-5-5", Price::flat(2_000, 10_000)),
    ("claude-sonnet-5", Price::flat(2_000, 10_000)),
    ("claude-sonnet-4-6", Price::flat(3_000, 15_000)),
    (
        "claude-haiku-5-5",
        Price {
            input_nanos_per_token: 100,
            output_nanos_per_token: 500,
            long_prompt: Some(LongPromptTier {
                threshold_tokens: 100_000,
                input_nanos_per_token: 500,
                output_nanos_per_token: 2_500,
            }),
        },
    ),
    ("claude-haiku-4-5", Price::flat(1_000, 5_000)),
];

/// The built-in price of an Anthropic model: an exact identifier, or an identifier followed by
/// a dated snapshot suffix (`-YYYYMMDD`). Nothing else matches, so a model that is merely
/// similar to a known one is never given an invented price.
fn builtin_price(provider: &str, model: &str) -> Option<Price> {
    if !provider.eq_ignore_ascii_case(ANTHROPIC_PROVIDER) {
        return None;
    }
    let model = model.to_ascii_lowercase();
    BUILTIN.iter().find_map(|(id, price)| {
        let rest = model.strip_prefix(id)?;
        let dated = rest
            .strip_prefix('-')
            .is_some_and(|date| date.len() == 8 && date.bytes().all(|b| b.is_ascii_digit()));
        (rest.is_empty() || dated).then_some(*price)
    })
}

/// How well a pattern matches: `None` no match, exact matches rank above every wildcard and
/// longer wildcard prefixes above shorter ones.
fn match_rank(pattern: &str, value: &str) -> Option<usize> {
    match pattern.strip_suffix('*') {
        Some(prefix) => {
            let head = value.get(..prefix.len())?;
            head.eq_ignore_ascii_case(prefix).then_some(prefix.len())
        }
        None => pattern.eq_ignore_ascii_case(value).then_some(usize::MAX),
    }
}

#[derive(Clone, Debug)]
struct Override {
    provider: String,
    model: String,
    price: Price,
}

/// What one call cost and what it would have cost in the cloud.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CostEstimate {
    /// Cost actually incurred, nano-dollars. Always 0 for local calls and unpriced models.
    pub cost_nanos: i64,
    /// Cost of the same tokens on a metered cloud model: the call's own cost for cloud calls,
    /// the baseline model's price for local calls.
    pub cloud_equivalent_nanos: i64,
    /// `false` when the call was a cloud call to a model without a known price.
    pub priced: bool,
}

impl CostEstimate {
    /// What running locally saved: cloud-equivalent cost minus actual cost.
    pub fn savings_nanos(&self) -> i64 {
        self.cloud_equivalent_nanos.saturating_sub(self.cost_nanos)
    }
}

/// Built-in prices overridden by [`MetricsConfig::pricing`], plus the baseline cloud model used
/// to value local calls.
#[derive(Clone, Debug)]
pub struct PriceTable {
    overrides: Vec<Override>,
    baseline: (String, String),
    baseline_price: Price,
}

impl Default for PriceTable {
    fn default() -> Self {
        Self::new(&MetricsConfig::default())
    }
}

impl PriceTable {
    /// Builds the table. Configured entries with a negative or non-finite price are ignored
    /// (with a warning) rather than failing start-up. The baseline is
    /// `anthropic` / [`DEFAULT_BASELINE_MODEL`] until [`Self::with_baseline`] says otherwise.
    pub fn new(config: &MetricsConfig) -> Self {
        let overrides = config
            .pricing
            .iter()
            .filter_map(|entry| match price_from_config(entry) {
                Some(price) => Some(Override {
                    provider: entry.provider.trim().to_owned(),
                    model: entry.model.trim().to_owned(),
                    price,
                }),
                None => {
                    tracing::warn!(
                        provider = %entry.provider,
                        model = %entry.model,
                        "ignoring metrics.pricing entry with an invalid price"
                    );
                    None
                }
            })
            .collect();
        let mut table = Self {
            overrides,
            baseline: (
                ANTHROPIC_PROVIDER.to_owned(),
                DEFAULT_BASELINE_MODEL.to_owned(),
            ),
            baseline_price: Price::flat(4_000, 20_000),
        };
        // An override of the default baseline model must also apply to the baseline.
        if let Some(price) = table.price(ANTHROPIC_PROVIDER, DEFAULT_BASELINE_MODEL) {
            table.baseline_price = price;
        }
        table
    }

    /// Uses `provider` / `model` (normally the configured System 2 model) as the baseline for
    /// valuing local calls, when it has a known price; otherwise the current baseline is kept.
    pub fn with_baseline(mut self, provider: &str, model: &str) -> Self {
        if let Some(price) = self.price(provider, model) {
            self.baseline = (provider.to_owned(), model.to_owned());
            self.baseline_price = price;
        }
        self
    }

    /// `(provider, model)` of the baseline cloud model.
    pub fn baseline(&self) -> (&str, &str) {
        (&self.baseline.0, &self.baseline.1)
    }

    /// Price of the baseline cloud model.
    pub fn baseline_price(&self) -> Price {
        self.baseline_price
    }

    /// Looks up a price. Configured entries win over built-in ones; among configured entries an
    /// exact model beats a wildcard, a longer wildcard prefix beats a shorter one, and the
    /// earlier entry wins a tie. Provider and model compare case-insensitively; both accept a
    /// trailing `*`.
    pub fn price(&self, provider: &str, model: &str) -> Option<Price> {
        let mut best: Option<((usize, usize), Price)> = None;
        for entry in &self.overrides {
            let (Some(model_rank), Some(provider_rank)) = (
                match_rank(&entry.model, model),
                match_rank(&entry.provider, provider),
            ) else {
                continue;
            };
            let rank = (model_rank, provider_rank);
            if best.is_none_or(|(current, _)| rank > current) {
                best = Some((rank, entry.price));
            }
        }
        best.map(|(_, price)| price)
            .or_else(|| builtin_price(provider, model))
    }

    /// Costs one call. Local calls cost 0 and are valued at the baseline price; cloud calls to
    /// an unknown model cost 0 with `priced = false`.
    pub fn estimate(
        &self,
        locality: Locality,
        provider: &str,
        model: &str,
        usage: TokenUsage,
    ) -> CostEstimate {
        match locality {
            Locality::Local => CostEstimate {
                cost_nanos: 0,
                cloud_equivalent_nanos: self.baseline_price.cost_nanos(usage),
                priced: true,
            },
            Locality::Cloud => match self.price(provider, model) {
                Some(price) => {
                    let cost = price.cost_nanos(usage);
                    CostEstimate {
                        cost_nanos: cost,
                        cloud_equivalent_nanos: cost,
                        priced: true,
                    }
                }
                None => CostEstimate {
                    cost_nanos: 0,
                    cloud_equivalent_nanos: 0,
                    priced: false,
                },
            },
        }
    }
}

fn price_from_config(entry: &ModelPrice) -> Option<Price> {
    let input = usd_per_mtok_to_nanos_per_token(entry.input_usd_per_mtok)?;
    let output = usd_per_mtok_to_nanos_per_token(entry.output_usd_per_mtok)?;
    let long_prompt = match entry.long_prompt_threshold_tokens {
        Some(threshold_tokens) => Some(LongPromptTier {
            threshold_tokens,
            input_nanos_per_token: match entry.long_input_usd_per_mtok {
                Some(value) => usd_per_mtok_to_nanos_per_token(value)?,
                None => input,
            },
            output_nanos_per_token: match entry.long_output_usd_per_mtok {
                Some(value) => usd_per_mtok_to_nanos_per_token(value)?,
                None => output,
            },
        }),
        None => None,
    };
    Some(Price {
        input_nanos_per_token: input,
        output_nanos_per_token: output,
        long_prompt,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn table(pricing: Vec<ModelPrice>) -> PriceTable {
        PriceTable::new(&MetricsConfig { pricing })
    }

    #[test]
    fn builtin_prices_are_exact_and_never_invented() {
        let prices = PriceTable::default();
        let fable = prices
            .price("anthropic", "claude-fable-5-2")
            .expect("fable");
        assert_eq!(fable, Price::flat(8_000, 40_000));
        // 1,000,000 in + 1,000,000 out = $8 + $40.
        assert_eq!(
            fable.cost_nanos(TokenUsage::new(1_000_000, 1_000_000)),
            48 * NANOS_PER_USD
        );
        assert_eq!(
            prices.price("Anthropic", "claude-haiku-4-5-20251001"),
            Some(Price::flat(1_000, 5_000))
        );
        assert_eq!(
            prices.price("anthropic", "claude-opus-5"),
            Some(Price::flat(5_000, 25_000))
        );
        assert_eq!(prices.price("anthropic", "claude-opus-5-9"), None);
        assert_eq!(prices.price("anthropic", "claude-opus-5-5-preview"), None);
        assert_eq!(prices.price("openai_compatible", "claude-opus-5-5"), None);

        let unknown = prices.estimate(
            Locality::Cloud,
            "openai_compatible",
            "gpt-x",
            TokenUsage::new(1000, 1000),
        );
        assert_eq!(
            unknown,
            CostEstimate {
                cost_nanos: 0,
                cloud_equivalent_nanos: 0,
                priced: false
            }
        );
    }

    #[test]
    fn long_prompt_tier_switches_the_whole_call_above_the_threshold() {
        let prices = PriceTable::default();
        let haiku = prices
            .price("anthropic", "claude-haiku-5-5")
            .expect("haiku");
        assert_eq!(
            haiku.cost_nanos(TokenUsage::new(100_000, 1_000)),
            100_000 * 100 + 1_000 * 500
        );
        assert_eq!(
            haiku.cost_nanos(TokenUsage::new(100_001, 1_000)),
            100_001 * 500 + 1_000 * 2_500
        );

        // The tier is generic: configurable, with missing long prices defaulting to the base.
        let mut entry = ModelPrice::new("acme", "big", 1.0, 2.0);
        entry.long_prompt_threshold_tokens = Some(10);
        entry.long_input_usd_per_mtok = Some(3.0);
        let custom = table(vec![entry]).price("acme", "big").expect("custom");
        assert_eq!(
            custom.cost_nanos(TokenUsage::new(10, 1)),
            10 * 1_000 + 2_000
        );
        assert_eq!(
            custom.cost_nanos(TokenUsage::new(11, 1)),
            11 * 3_000 + 2_000
        );

        let parsed: ModelPrice = serde_json::from_str(
            r#"{"provider":"p","model":"m","input_usd_per_mtok":1,"output_usd_per_mtok":2}"#,
        )
        .expect("old-style entry still parses");
        assert_eq!(parsed, ModelPrice::new("p", "m", 1.0, 2.0));
    }

    #[test]
    fn overrides_take_precedence_and_wildcards_rank_by_specificity() {
        let prices = table(vec![
            ModelPrice::new("acme", "*", 1.0, 1.0),
            ModelPrice::new("acme", "turbo-*", 2.0, 2.0),
            ModelPrice::new("acme", "turbo-mini", 3.0, 3.0),
            ModelPrice::new("anthropic", "claude-fable-5-2", 0.5, 0.25),
            ModelPrice::new("anthropic", "claude-opus-*", 7.0, 7.0),
            ModelPrice::new("broken", "x", f64::NAN, 1.0),
            ModelPrice::new("broken", "y", -1.0, 1.0),
        ]);
        assert_eq!(
            prices.price("acme", "other"),
            Some(Price::flat(1_000, 1_000))
        );
        assert_eq!(
            prices.price("acme", "turbo-max"),
            Some(Price::flat(2_000, 2_000))
        );
        assert_eq!(
            prices.price("ACME", "Turbo-Mini"),
            Some(Price::flat(3_000, 3_000))
        );
        assert_eq!(
            prices.price("anthropic", "claude-fable-5-2"),
            Some(Price::flat(500, 250))
        );
        // A configured wildcard beats the built-in exact entry: configuration always wins.
        assert_eq!(
            prices.price("anthropic", "claude-opus-5-5"),
            Some(Price::flat(7_000, 7_000))
        );
        assert_eq!(prices.baseline_price(), Price::flat(7_000, 7_000));
        assert_eq!(prices.price("broken", "x"), None);
        assert_eq!(prices.price("broken", "y"), None);
        assert_eq!(prices.price("other", "turbo-mini"), None);
    }

    #[test]
    fn local_calls_are_free_and_valued_at_the_baseline() {
        let usage = TokenUsage::new(2_000, 500);
        let default = PriceTable::default();
        assert_eq!(default.baseline(), ("anthropic", "claude-opus-5-5"));
        let estimate = default.estimate(Locality::Local, "ollama", "qwen", usage);
        assert_eq!(estimate.cost_nanos, 0);
        assert_eq!(
            estimate.cloud_equivalent_nanos,
            2_000 * 4_000 + 500 * 20_000
        );
        assert_eq!(estimate.savings_nanos(), 18_000_000);
        assert!(estimate.priced);

        let sonnet = PriceTable::default().with_baseline("anthropic", "claude-sonnet-5-5");
        assert_eq!(sonnet.baseline(), ("anthropic", "claude-sonnet-5-5"));
        assert_eq!(
            sonnet
                .estimate(Locality::Local, "ollama", "qwen", usage)
                .cloud_equivalent_nanos,
            2_000 * 2_000 + 500 * 10_000
        );
        // An unpriced System 2 model keeps the default baseline.
        let unknown = PriceTable::default().with_baseline("openai_compatible", "gpt-x");
        assert_eq!(unknown.baseline(), ("anthropic", "claude-opus-5-5"));
    }

    #[test]
    fn money_conversion_and_formatting() {
        assert_eq!(usd_per_mtok_to_nanos_per_token(0.10), Some(100));
        assert_eq!(usd_per_mtok_to_nanos_per_token(8.0), Some(8_000));
        assert_eq!(usd_per_mtok_to_nanos_per_token(-0.1), None);
        assert_eq!(usd_per_mtok_to_nanos_per_token(f64::INFINITY), None);
        assert_eq!(
            Price::flat(u64::MAX, u64::MAX).cost_nanos(TokenUsage::new(u64::MAX, u64::MAX)),
            i64::MAX
        );
        assert_eq!(format_usd(0), "$0.00");
        assert_eq!(format_usd(1_234_567_890), "$1.23");
        assert_eq!(format_usd(1_235_000_000), "$1.24");
        assert_eq!(format_usd(12_345_678), "$0.01");
        assert_eq!(format_usd(1_234_567), "$0.0012");
        assert_eq!(format_usd(18_000), "$0.000018");
        assert_eq!(format_usd(-2_500_000_000), "-$2.50");
        assert_eq!(format_usd_fixed(999_999_999, 0), "$1");
        assert_eq!(format_usd_fixed(1, 9), "$0.000000001");
        assert_eq!(format_usd_fixed(i64::MIN, 2), "-$9223372036.85");
        assert!((nanos_to_usd(1_500_000_000) - 1.5).abs() < 1e-12);
        assert_eq!(format_tokens(0), "0");
        assert_eq!(format_tokens(1_234_567), "1,234,567");
    }
}
