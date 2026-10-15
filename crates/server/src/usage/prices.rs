//! Token counts and what they cost: the user's prices over the published
//! ones, and how a cache write is charged.

use super::*;

#[derive(Debug, Clone, Copy, Default, Serialize)]
pub struct Tokens {
    /// Input that was neither read from nor written to the cache.
    pub input: u64,
    pub cache_read: u64,
    pub cache_write_5m: u64,
    pub cache_write_1h: u64,
    pub output: u64,
    /// Thinking or reasoning, already part of `output`.
    pub reasoning: u64,
}
impl Tokens {
    pub(super) fn add(&mut self, other: &Tokens) {
        self.input += other.input;
        self.cache_read += other.cache_read;
        self.cache_write_5m += other.cache_write_5m;
        self.cache_write_1h += other.cache_write_1h;
        self.output += other.output;
        self.reasoning += other.reasoning;
    }
    pub fn total(&self) -> u64 {
        self.input + self.cache_read + self.cache_write_5m + self.cache_write_1h + self.output
    }
}

/// A price per million tokens: the published one (price_list.rs), or one
/// the user set for a model. A cache write is priced only where the provider
/// charges for one; otherwise see `tokens_cost`.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Price {
    pub input: f64,
    pub output: f64,
    pub cache_read: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_write: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_write_1h: Option<f64>,
}

/// The one price list: what the user set, else the published price. A model
/// reached through a gateway (or any model without a published price) costs
/// nothing until the user says what it costs them.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Prices {
    #[serde(default)]
    pub models: BTreeMap<String, Price>,
}
impl Prices {
    pub fn of(&self, model: &str) -> Option<(Price, bool)> {
        if let Some(custom) = self.models.get(&model.to_ascii_lowercase()) {
            return Some((*custom, true));
        }
        crate::price_list::list().lookup(model).map(|listed| {
            (
                Price {
                    input: listed.input,
                    output: listed.output,
                    cache_read: listed.cache_read,
                    cache_write: listed.cache_write,
                    cache_write_1h: listed.cache_write_1h,
                },
                false,
            )
        })
    }
    pub(super) fn cost(&self, call: &Call) -> Option<f64> {
        self.cost_of(call.provider, &call.model, &call.tokens)
    }
    pub fn cost_of(&self, provider: ProviderKind, model: &str, tokens: &Tokens) -> Option<f64> {
        self.of(model)
            .map(|(price, _)| tokens_cost(price, provider, tokens))
    }
}

pub(super) fn prices_path(state_dir: &Path) -> PathBuf {
    state_dir.join("prices.json")
}
pub(super) fn load_prices(state_dir: &Path) -> Prices {
    std::fs::read(prices_path(state_dir))
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default()
}

/// A cache write without a price of its own costs what its client's vendor
/// charges (adapters' `cache_write_rates`).
fn tokens_cost(price: Price, provider: ProviderKind, tokens: &Tokens) -> f64 {
    let Price {
        input,
        output,
        cache_read: read,
        cache_write,
        cache_write_1h,
    } = price;
    let (five, hour) = crate::adapters::adapter(provider)
        .map_or((input, input), |client| client.cache_write_rates(input));
    let (write, write_hour) = (cache_write.unwrap_or(five), cache_write_1h.unwrap_or(hour));
    (tokens.input as f64 * input
        + tokens.cache_write_5m as f64 * write
        + tokens.cache_write_1h as f64 * write_hour
        + tokens.cache_read as f64 * read
        + tokens.output as f64 * output)
        / 1_000_000.0
}
