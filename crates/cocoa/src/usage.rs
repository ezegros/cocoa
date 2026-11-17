//! Composer accounting from durable requests, independent of render frequency.

use std::{collections::BTreeMap, time::Duration};

use cocoa_core::model::Usage;
use cocoa_llm::ProviderKind;
use serde_json::Value;
use tokio::time::Instant;

use crate::persistence::{PersistenceError, SessionAttachment, StoredSessionSettings, UsageLedger};

/// Unknown values stay absent. USD is an advertised-rate approximation, not a bill.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct UsageSnapshot {
    pub cache_hit_percent: Option<f64>,
    pub approximate_usd: Option<f64>,
}

/// Bounded advertised rates retained across background polls. This contains no
/// credentials. Successful refreshes last an hour; failures retry after five
/// minutes. At most 8,192 exact provider/model entries are retained.
#[derive(Default)]
pub struct PricingState {
    rates: BTreeMap<(String, String), Rates>,
    refresh_after: Option<Instant>,
}

/// Read durable usage for a background job, normally once every two seconds.
/// Do not call from rendering. All session requests participate, including child,
/// compaction and retry attempts from earlier runtime epochs. Missing final usage
/// or incomplete ledger coverage hides the entire price, never a partial total.
/// Subscription plans, local providers and custom endpoints have no price.
/// Catalogue failures leave price unknown without failing the usage read.
///
/// # Errors
/// Propagate bounded persistence read and task failures without changing storage.
pub async fn snapshot(
    session: &SessionAttachment,
    pricing: &mut PricingState,
) -> Result<UsageSnapshot, PersistenceError> {
    let ledger = session.usage_ledger().await?;
    if price_eligible(&ledger)
        && pricing
            .refresh_after
            .is_none_or(|deadline| Instant::now() >= deadline)
    {
        pricing.refresh_after = Some(Instant::now() + Duration::from_secs(300));
        if let Some(metadata) =
            crate::catalogue::fetch("https://models.dev/api.json", None, 16 * 1024 * 1024).await
        {
            *pricing =
                tokio::task::spawn_blocking(move || PricingState::from_metadata(&metadata)).await?;
        }
    }
    Ok(pricing.evaluate(&ledger))
}

impl PricingState {
    /// Parse models.dev fixture or fetched JSON without network access. Only exact
    /// provider keys and matching model `id` fields count. Invalid, tiered or
    /// unsupported prices stay unknown. Retain at most 8,192 entries with model
    /// IDs of at most 256 bytes, without retaining the source document.
    #[must_use]
    pub fn from_metadata(metadata: &Value) -> Self {
        let mut rates = BTreeMap::new();
        for provider in ProviderKind::ALL
            .into_iter()
            .filter(|provider| api_key_provider(*provider))
        {
            let provider_name = provider.to_string();
            let provider_key = match provider {
                ProviderKind::Together => "togetherai",
                ProviderKind::Fireworks => "fireworks-ai",
                ProviderKind::OpenCodeZen => "opencode",
                ProviderKind::VercelAiGateway => "vercel",
                _ => &provider_name,
            };
            let Some(models) = metadata
                .get(provider_key)
                .and_then(|entry| entry.get("models"))
                .and_then(Value::as_object)
            else {
                continue;
            };
            for (model, entry) in models.iter().take(8192) {
                if rates.len() == 8192 {
                    break;
                }
                if model.is_empty()
                    || model.len() > 256
                    || entry.get("id").and_then(Value::as_str) != Some(model.as_str())
                {
                    continue;
                }
                if let Some(price) = Rates::from_entry(entry) {
                    rates.insert((provider_name.clone(), model.clone()), price);
                }
            }
        }
        Self {
            rates,
            refresh_after: Some(Instant::now() + Duration::from_secs(3600)),
        }
    }

    /// Compute a snapshot using only the supplied durable read and cached rates.
    /// No network, database or credential access occurs. An unknown cache count
    /// prevents a price whenever its advertised rate differs from ordinary input.
    #[must_use]
    pub fn evaluate(&self, ledger: &UsageLedger) -> UsageSnapshot {
        let cache_hit_percent = ledger
            .latest_root_usage
            .as_ref()
            .and_then(cache_hit_percent);
        let approximate_usd = self.total(ledger);
        UsageSnapshot {
            cache_hit_percent,
            approximate_usd,
        }
    }

    fn total(&self, ledger: &UsageLedger) -> Option<f64> {
        if !price_eligible(ledger) {
            return None;
        }
        let mut total = 0.0;
        for request in &ledger.requests {
            if !request.update.usage_complete {
                return None;
            }
            let settings = request.settings.as_ref()?;
            if settings.endpoint_default != Some(true) {
                return None;
            }
            let rates = self
                .rates
                .get(&(settings.provider.clone(), settings.model.clone()))?;
            total += rates.cost(request.update.usage.as_ref()?)?;
            if !total.is_finite() {
                return None;
            }
        }
        Some(total)
    }
}

/// Reported read/input ratio. Missing counts, zero input and inconsistent counters
/// are unknown rather than a guessed zero or a clamped percentage.
#[must_use]
pub fn cache_hit_percent(usage: &Usage) -> Option<f64> {
    let read = usage.input_cache.as_ref()?.read_tokens?;
    if usage.input_tokens == 0 || read > usage.input_tokens {
        return None;
    }
    Some(read as f64 / usage.input_tokens as f64 * 100.0)
}

fn price_eligible(ledger: &UsageLedger) -> bool {
    let StoredSessionSettings::Bound { effective } = &ledger.settings else {
        return false;
    };
    ledger.complete
        && !ledger.requests.is_empty()
        && hosted_api_key(&effective.provider, effective.endpoint.as_deref())
}

fn hosted_api_key(provider: &str, endpoint: Option<&str>) -> bool {
    provider.parse::<ProviderKind>().is_ok_and(|provider| {
        api_key_provider(provider)
            && endpoint.is_none_or(|endpoint| endpoint == provider.endpoint_default())
    })
}

fn api_key_provider(provider: ProviderKind) -> bool {
    matches!(
        provider,
        ProviderKind::OpenAi
            | ProviderKind::Baseten
            | ProviderKind::OpenRouter
            | ProviderKind::XAi
            | ProviderKind::OpenCodeZen
            | ProviderKind::VercelAiGateway
            | ProviderKind::Together
            | ProviderKind::Fireworks
            | ProviderKind::Meta
            | ProviderKind::Groq
            | ProviderKind::DeepSeek
            | ProviderKind::Cerebras
            | ProviderKind::Nvidia
            | ProviderKind::HuggingFace
            | ProviderKind::AntLing
            | ProviderKind::MoonshotAi
            | ProviderKind::MoonshotAiCn
            | ProviderKind::Xiaomi
            | ProviderKind::Anthropic
    )
}

struct Rates {
    input: f64,
    output: f64,
    cache_read: Option<f64>,
    cache_write: Option<f64>,
}

impl Rates {
    fn from_entry(entry: &Value) -> Option<Self> {
        let cost = entry.get("cost")?.as_object()?;
        if cost.keys().any(|key| {
            !matches!(
                key.as_str(),
                "input" | "output" | "cache_read" | "cache_write"
            )
        }) {
            return None;
        }
        let rate = |key| {
            cost.get(key)?
                .as_f64()
                .filter(|rate| rate.is_finite() && *rate >= 0.0)
        };
        let optional = |key| match cost.get(key) {
            None => Some(None),
            Some(_) => rate(key).map(Some),
        };
        Some(Self {
            input: rate("input")?,
            output: rate("output")?,
            cache_read: optional("cache_read")?,
            cache_write: optional("cache_write")?,
        })
    }

    fn cost(&self, usage: &Usage) -> Option<f64> {
        let cache = usage.input_cache.as_ref();
        let read = cache.and_then(|cache| cache.read_tokens);
        let write = cache.and_then(|cache| cache.write_tokens);
        let read_known = read.unwrap_or(0);
        let write_known = write.unwrap_or(0);
        if read_known.checked_add(write_known)? > usage.input_tokens {
            return None;
        }
        let read_adjustment = self.cache_adjustment(read, self.cache_read)?;
        let write_adjustment = self.cache_adjustment(write, self.cache_write)?;
        let cost = (usage.input_tokens as f64 * self.input
            + usage.output_tokens as f64 * self.output
            + read_adjustment
            + write_adjustment)
            / 1_000_000.0;
        (cost.is_finite() && cost >= 0.0).then_some(cost)
    }

    fn cache_adjustment(&self, count: Option<u64>, rate: Option<f64>) -> Option<f64> {
        match (count, rate) {
            (Some(0), _) | (None, None) => Some(0.0),
            (Some(count), Some(rate)) => Some(count as f64 * (rate - self.input)),
            (None, Some(rate)) if rate == self.input => Some(0.0),
            _ => None,
        }
    }
}
