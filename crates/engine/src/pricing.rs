//! Device-local LiteLLM pricing cache used for ingestion-time cost estimates.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock, RwLockReadGuard, RwLockWriteGuard};
use std::time::{Duration, SystemTime};

const CACHE_FILE: &str = "model-prices.json";
const REFRESH_INTERVAL: Duration = Duration::from_secs(24 * 60 * 60);
const MAX_CATALOG_BYTES: usize = 8 * 1024 * 1024;
const SOURCES: &[&str] = &[
    "https://cdn.jsdelivr.net/gh/BerriAI/litellm@main/model_prices_and_context_window.json",
    "https://raw.githubusercontent.com/BerriAI/litellm/main/model_prices_and_context_window.json",
];

#[derive(Debug, Clone, Copy, PartialEq)]
struct TokenRate {
    input: f64,
    output: f64,
    cache_read: f64,
    cache_write: f64,
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct ModelRate {
    standard: TokenRate,
    priority: Option<TokenRate>,
}

#[derive(Clone)]
pub(crate) struct PricingCatalog {
    rates: Arc<RwLock<HashMap<String, ModelRate>>>,
    cache_path: Arc<PathBuf>,
    refresh_delay: Duration,
}

fn read_lock<T>(lock: &RwLock<T>) -> RwLockReadGuard<'_, T> {
    lock.read()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn write_lock<T>(lock: &RwLock<T>) -> RwLockWriteGuard<'_, T> {
    lock.write()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn finite_rate(value: Option<&serde_json::Value>) -> Option<f64> {
    value
        .and_then(serde_json::Value::as_f64)
        .filter(|value| value.is_finite() && *value >= 0.0)
}

fn parse_rates(bytes: &[u8]) -> Result<HashMap<String, ModelRate>, serde_json::Error> {
    let document: serde_json::Value = serde_json::from_slice(bytes)?;
    let mut rates = HashMap::new();
    let Some(entries) = document.as_object() else {
        return Ok(rates);
    };
    for (model, raw) in entries {
        let Some(entry) = raw.as_object() else {
            continue;
        };
        if entry
            .get("litellm_provider")
            .and_then(serde_json::Value::as_str)
            != Some("openai")
        {
            continue;
        }
        let Some(input) = finite_rate(entry.get("input_cost_per_token")) else {
            continue;
        };
        let Some(output) = finite_rate(entry.get("output_cost_per_token")) else {
            continue;
        };
        let priority =
            finite_rate(entry.get("input_cost_per_token_priority")).and_then(|priority_input| {
                Some(TokenRate {
                    input: priority_input,
                    output: finite_rate(entry.get("output_cost_per_token_priority"))?,
                    cache_read: finite_rate(entry.get("cache_read_input_token_cost_priority"))
                        .unwrap_or(priority_input),
                    cache_write: finite_rate(entry.get("cache_creation_input_token_cost_priority"))
                        .unwrap_or(priority_input),
                })
            });
        rates.insert(
            model.clone(),
            ModelRate {
                standard: TokenRate {
                    input,
                    output,
                    cache_read: finite_rate(entry.get("cache_read_input_token_cost"))
                        .unwrap_or(input),
                    cache_write: finite_rate(entry.get("cache_creation_input_token_cost"))
                        .unwrap_or(input),
                },
                priority,
            },
        );
    }
    Ok(rates)
}

fn cache_backup(path: &Path) -> PathBuf {
    path.with_extension("json.old")
}

fn load_rates(path: &Path) -> Option<HashMap<String, ModelRate>> {
    std::fs::read(path)
        .ok()
        .filter(|bytes| bytes.len() <= MAX_CATALOG_BYTES)
        .and_then(|bytes| parse_rates(&bytes).ok())
        .filter(|rates| !rates.is_empty())
}

fn cache_age(path: &Path) -> Option<Duration> {
    std::fs::metadata(path)
        .and_then(|metadata| metadata.modified())
        .ok()
        .and_then(|modified| SystemTime::now().duration_since(modified).ok())
}

impl PricingCatalog {
    pub(crate) fn load(data_dir: &Path) -> Self {
        let cache_path = data_dir.join(CACHE_FILE);
        let primary_rates = load_rates(&cache_path);
        let refresh_delay = if primary_rates.is_some() {
            REFRESH_INTERVAL.saturating_sub(cache_age(&cache_path).unwrap_or(REFRESH_INTERVAL))
        } else {
            Duration::ZERO
        };
        let rates = primary_rates
            .or_else(|| load_rates(&cache_backup(&cache_path)))
            .unwrap_or_default();
        Self {
            rates: Arc::new(RwLock::new(rates)),
            cache_path: Arc::new(cache_path),
            refresh_delay,
        }
    }

    /// Refresh after the cached catalog reaches 24 hours, then every 24 hours,
    /// without delaying engine startup. The loaded catalog remains usable if
    /// every source or cache write fails.
    pub(crate) fn start_refresh_loop(&self) {
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            tracing::debug!("pricing refresh skipped without an async runtime");
            return;
        };
        let catalog = self.clone();
        runtime.spawn(async move {
            tokio::time::sleep(catalog.refresh_delay).await;
            loop {
                if let Err(error) = catalog.refresh().await {
                    tracing::warn!(%error, "LiteLLM pricing refresh failed; retaining cached rates");
                }
                tokio::time::sleep(REFRESH_INTERVAL).await;
            }
        });
    }

    /// Exact Codex model-id and service-tier lookup. Standard uses LiteLLM's
    /// base OpenAI rates; Fast uses its API Priority rates rather than ChatGPT's
    /// unrelated subscription-credit multiplier.
    pub(crate) fn estimate_codex(
        &self,
        model: &str,
        input_tokens: u64,
        output_tokens: u64,
        cache_read_input_tokens: u64,
        cache_write_input_tokens: u64,
        service_tier: Option<&str>,
    ) -> Option<f64> {
        let model_rate = *read_lock(&self.rates).get(model)?;
        let rate = match service_tier {
            None | Some("default") => model_rate.standard,
            Some("fast") => model_rate.priority?,
            Some(_) => return None,
        };
        let cost = input_tokens as f64 * rate.input
            + output_tokens as f64 * rate.output
            + cache_read_input_tokens as f64 * rate.cache_read
            + cache_write_input_tokens as f64 * rate.cache_write;
        cost.is_finite().then_some(cost)
    }

    async fn refresh(&self) -> Result<(), String> {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(20))
            .user_agent("Jolt pricing cache")
            .build()
            .map_err(|error| error.to_string())?;
        let mut failures = Vec::new();
        for source in SOURCES {
            match download_catalog(&client, source).await {
                Ok((bytes, rates)) => {
                    if let Err(error) = replace_cache(&self.cache_path, &bytes).await {
                        tracing::warn!(%error, "could not persist LiteLLM pricing cache");
                    }
                    *write_lock(&self.rates) = rates;
                    return Ok(());
                }
                Err(error) => failures.push(format!("{source}: {error}")),
            }
        }
        Err(failures.join("; "))
    }
}

async fn download_catalog(
    client: &reqwest::Client,
    source: &str,
) -> Result<(Vec<u8>, HashMap<String, ModelRate>), String> {
    let response = client
        .get(source)
        .send()
        .await
        .and_then(reqwest::Response::error_for_status)
        .map_err(|error| error.to_string())?;
    if response
        .content_length()
        .is_some_and(|length| length > MAX_CATALOG_BYTES as u64)
    {
        return Err("catalog exceeds size limit".into());
    }
    let bytes = response.bytes().await.map_err(|error| error.to_string())?;
    if bytes.len() > MAX_CATALOG_BYTES {
        return Err("catalog exceeds size limit".into());
    }
    let rates = parse_rates(&bytes).map_err(|error| error.to_string())?;
    if rates.is_empty() {
        return Err("catalog contains no priced OpenAI models".into());
    }
    Ok((bytes.to_vec(), rates))
}

async fn replace_cache(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let temporary = path.with_extension("json.tmp");
    let backup = cache_backup(path);
    tokio::fs::write(&temporary, bytes).await?;
    let _ = tokio::fs::remove_file(&backup).await;
    if tokio::fs::rename(path, &backup).await.is_ok() {
        if let Err(error) = tokio::fs::rename(&temporary, path).await {
            let _ = tokio::fs::rename(&backup, path).await;
            return Err(error);
        }
        let _ = tokio::fs::remove_file(&backup).await;
    } else {
        tokio::fs::rename(&temporary, path).await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const CATALOG: &[u8] = br#"{
        "gpt-test": {
            "litellm_provider": "openai",
            "input_cost_per_token": 0.000002,
            "output_cost_per_token": 0.00001,
            "cache_read_input_token_cost": 0.0000002,
            "input_cost_per_token_priority": 0.000004,
            "output_cost_per_token_priority": 0.00002,
            "cache_read_input_token_cost_priority": 0.0000004
        },
        "chatgpt/gpt-test": {
            "litellm_provider": "chatgpt"
        },
        "half-priced": {
            "litellm_provider": "openai",
            "input_cost_per_token": 0.000002
        }
    }"#;

    #[test]
    fn parses_openai_rates_and_prices_exact_model_ids() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(CACHE_FILE), CATALOG).unwrap();
        let catalog = PricingCatalog::load(dir.path());

        assert_eq!(catalog.estimate_codex("GPT-TEST", 1, 1, 1, 1, None), None);
        assert_eq!(
            catalog.estimate_codex("half-priced", 1, 1, 1, 1, None),
            None
        );
        let standard = catalog
            .estimate_codex("gpt-test", 10, 2, 5, 3, None)
            .expect("priced model");
        assert!((standard - 0.000047).abs() < f64::EPSILON);
        let fast = catalog
            .estimate_codex("gpt-test", 10, 2, 5, 3, Some("fast"))
            .expect("priority-priced model");
        assert!((fast - 0.000094).abs() < f64::EPSILON);
    }

    #[test]
    fn falls_back_to_last_known_good_cache() {
        let dir = tempfile::tempdir().unwrap();
        let cache = dir.path().join(CACHE_FILE);
        std::fs::write(&cache, b"not json").unwrap();
        std::fs::write(cache_backup(&cache), CATALOG).unwrap();

        let catalog = PricingCatalog::load(dir.path());
        assert!(
            catalog
                .estimate_codex("gpt-test", 1, 1, 1, 1, None)
                .is_some()
        );
    }
}
