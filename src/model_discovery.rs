//! Own model discovery, so a model OpenAI adds shows up without the user
//! running the Codex CLI.
//!
//! The Codex CLI learns its catalog from `GET <codex api>/models?client_version=`
//! (codex-rs `codex-api`, `endpoint/models.rs`), authenticated with the same
//! OAuth bearer and `ChatGPT-Account-Id` this proxy already sends. The proxy
//! calls that endpoint itself and keeps the result in `codex-models.json` in
//! its state directory (never in the Codex CLI's `models_cache.json`).
//!
//! The effective Codex list is bundled models, the Codex CLI cache, and this
//! cache (see `model_allowlist`). A failed refresh (network, 401, 429, a
//! changed JSON shape) logs and keeps the last good cache untouched, so a
//! hiccup never empties the list or removes agent cards.
//!
//! One background loop (`spawn_background_tasks`) refreshes models first and
//! syncs the agent cards afterwards. Not every provider has a usable listing
//! endpoint; see the report that came with this module: Kimi, Grok and Cursor
//! stay on their static lists.

use crate::agent_cards;
use crate::logging::{Logger, create_logger};
use crate::providers::codex::client::CodexHttpClient;
use crate::providers::codex::translate::model_allowlist::{
    allowed_models, invalidate_own_cache, is_listed_api_model, own_cache_file,
};
use crate::registry::Registry;
use anyhow::{Context, Result};
use serde_json::{Map, Value, json};
use std::collections::BTreeSet;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// `0` disables discovery entirely (the Codex CLI cache is still read).
pub const DISCOVERY_ENV: &str = "CCP_MODEL_DISCOVERY";
/// Seconds between refreshes; `0` refreshes only at startup.
pub const REFRESH_INTERVAL_ENV: &str = "CCP_MODEL_REFRESH_INTERVAL_SECS";
/// Overrides the `client_version` sent to the catalog endpoint.
pub const CLIENT_VERSION_ENV: &str = "CCP_CODEX_CLIENT_VERSION";
pub const DEFAULT_REFRESH_INTERVAL_SECS: u64 = 900;

/// Used when neither the Codex CLI cache nor its version file says better.
const FALLBACK_CLIENT_VERSION: &str = "0.163.0";
const FETCH_TIMEOUT: Duration = Duration::from_secs(20);
const MAX_BODY_BYTES: usize = 4 * 1024 * 1024;
const MAX_SLUG_LEN: usize = 96;
const RETRY_BASE: Duration = Duration::from_secs(30);
const RETRY_CAP: Duration = Duration::from_secs(15 * 60);
/// With `CCP_MODEL_REFRESH_INTERVAL_SECS=0`, how many failed startup attempts
/// are retried before giving up.
const MAX_STARTUP_ATTEMPTS: u32 = 5;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DiscoveryError {
    /// No usable login, or the backend answered 401/403.
    Auth(String),
    RateLimited(Option<String>),
    Http(u16),
    Network(String),
    /// The body was not the expected `{"models":[...]}` shape.
    Format(String),
    Cache(String),
}

impl std::fmt::Display for DiscoveryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Auth(detail) => write!(f, "not authorized: {detail}"),
            Self::RateLimited(Some(after)) => write!(f, "rate limited (retry after {after})"),
            Self::RateLimited(None) => write!(f, "rate limited"),
            Self::Http(status) => write!(f, "HTTP {status}"),
            Self::Network(detail) => write!(f, "network error: {detail}"),
            Self::Format(detail) => write!(f, "unexpected response: {detail}"),
            Self::Cache(detail) => write!(f, "cannot write the model cache: {detail}"),
        }
    }
}

impl std::error::Error for DiscoveryError {}

/// What a successful refresh found.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RefreshReport {
    /// Listed, API-capable slugs the endpoint returned.
    pub endpoint_models: Vec<String>,
    /// Slugs in the effective Codex list now but not before the refresh.
    pub added: Vec<String>,
    /// Slugs that were in the effective list before and are gone now.
    pub removed: Vec<String>,
    /// Whether `codex-models.json` changed on disk.
    pub cache_written: bool,
}

// --- configuration ---------------------------------------------------------

fn env_flag_off(name: &str) -> bool {
    matches!(
        std::env::var(name).ok().as_deref().map(str::trim),
        Some("0" | "false" | "off" | "no")
    )
}

pub fn discovery_enabled() -> bool {
    !env_flag_off(DISCOVERY_ENV)
}

/// `None` means "only at startup".
pub fn refresh_interval() -> Option<Duration> {
    parse_interval(std::env::var(REFRESH_INTERVAL_ENV).ok().as_deref())
}

fn parse_interval(raw: Option<&str>) -> Option<Duration> {
    let secs = raw
        .and_then(|raw| raw.trim().parse::<u64>().ok())
        .unwrap_or(DEFAULT_REFRESH_INTERVAL_SECS);
    (secs > 0).then(|| Duration::from_secs(secs))
}

/// Delay before the next model refresh after `failures` consecutive failures
/// (0 = the last one worked). Quick early retries cover a proxy that starts
/// before the network is up; later ones back off but never wait longer than
/// the normal cadence. Without an interval only the first few startup
/// failures are retried.
fn next_model_attempt(failures: u32, interval: Option<Duration>) -> Option<Duration> {
    if failures == 0 {
        return interval;
    }
    let retry = RETRY_BASE
        .saturating_mul(1u32 << (failures - 1).min(10))
        .min(RETRY_CAP);
    match interval {
        Some(interval) => Some(retry.min(interval)),
        None => (failures < MAX_STARTUP_ATTEMPTS).then_some(retry),
    }
}

fn version_key(version: &str) -> Vec<u64> {
    version
        .split('.')
        .map(|part| part.parse::<u64>().unwrap_or(0))
        .collect()
}

/// Newest of the versions the local Codex CLI knows (its models cache and its
/// update check) and the built-in fallback: the endpoint hides models whose
/// minimum client version is newer than the one sent.
fn client_version() -> String {
    if let Ok(explicit) = std::env::var(CLIENT_VERSION_ENV)
        && !explicit.trim().is_empty()
    {
        return explicit.trim().to_string();
    }
    let dir = crate::providers::codex::auth::token_store::codex_auth_file();
    let read = |name: &str, key: &str| -> Option<String> {
        let bytes = std::fs::read(dir.with_file_name(name)).ok()?;
        let value: Value = serde_json::from_slice(&bytes).ok()?;
        value[key].as_str().map(str::to_string)
    };
    [
        Some(FALLBACK_CLIENT_VERSION.to_string()),
        read("models_cache.json", "client_version"),
        read("version.json", "latest_version"),
    ]
    .into_iter()
    .flatten()
    .filter(|v| {
        !v.is_empty()
            && v.chars().all(|c| c.is_ascii_digit() || c == '.')
            && v.chars().next().is_some_and(|c| c.is_ascii_digit())
    })
    .max_by_key(|v| version_key(v))
    .unwrap_or_else(|| FALLBACK_CLIENT_VERSION.to_string())
}

// --- parsing and cache -----------------------------------------------------

fn valid_slug(slug: &str) -> bool {
    !slug.is_empty()
        && slug.len() <= MAX_SLUG_LEN
        && slug
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_'))
}

/// Parse a `ModelsResponse` body into slim cache entries (`slug`,
/// `display_name`, `visibility`, `supported_in_api`). Entries without a sane
/// slug are dropped; a body with no listed, API-capable model at all is
/// rejected so a broken response can never empty the list.
pub fn parse_catalog(body: &[u8]) -> Result<Vec<Value>, DiscoveryError> {
    let value: Value = serde_json::from_slice(body)
        .map_err(|e| DiscoveryError::Format(format!("invalid JSON ({})", e.classify_name())))?;
    let models = value["models"]
        .as_array()
        .ok_or_else(|| DiscoveryError::Format("no `models` array".into()))?;
    let entries: Vec<Value> = models
        .iter()
        .filter_map(|m| {
            let slug = m["slug"].as_str().filter(|s| valid_slug(s))?;
            Some(json!({
                "slug": slug,
                "display_name": m["display_name"].as_str().unwrap_or(slug),
                "visibility": m["visibility"].as_str().unwrap_or(""),
                "supported_in_api": m["supported_in_api"].as_bool().unwrap_or(false),
            }))
        })
        .collect();
    if !entries.iter().any(is_listed_api_model) {
        return Err(DiscoveryError::Format(
            "no listed, API-capable model".into(),
        ));
    }
    Ok(entries)
}

trait ClassifyName {
    fn classify_name(&self) -> &'static str;
}

impl ClassifyName for serde_json::Error {
    fn classify_name(&self) -> &'static str {
        match self.classify() {
            serde_json::error::Category::Io => "io",
            serde_json::error::Category::Syntax => "syntax",
            serde_json::error::Category::Data => "data",
            serde_json::error::Category::Eof => "truncated",
        }
    }
}

fn listed_slugs(entries: &[Value]) -> Vec<String> {
    let mut slugs: Vec<String> = entries
        .iter()
        .filter(|m| is_listed_api_model(m))
        .filter_map(|m| m["slug"].as_str().map(str::to_string))
        .collect();
    slugs.sort_unstable();
    slugs.dedup();
    slugs
}

/// Write `entries` to `path` atomically unless the stored `models` are
/// already identical. Returns whether the file changed.
fn write_cache(path: &Path, entries: &[Value], client_version: &str) -> Result<bool> {
    if let Ok(bytes) = std::fs::read(path)
        && let Ok(existing) = serde_json::from_slice::<Value>(&bytes)
        && existing["models"] == json!(entries)
    {
        return Ok(false);
    }
    let fetched_at = time::OffsetDateTime::now_utc()
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_default();
    let doc = json!({
        "fetched_at": fetched_at,
        "client_version": client_version,
        "models": entries,
    });
    let dir = path.parent().context("cache path has no parent")?;
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let tmp = dir.join(format!(".codex-models.tmp-{}", std::process::id()));
    std::fs::write(&tmp, serde_json::to_vec_pretty(&doc)?)
        .with_context(|| format!("writing {}", tmp.display()))?;
    std::fs::rename(&tmp, path).inspect_err(|_| {
        let _ = std::fs::remove_file(&tmp);
    })?;
    Ok(true)
}

// --- fetching --------------------------------------------------------------

async fn fetch_body(client: &CodexHttpClient, version: &str) -> Result<Vec<u8>, DiscoveryError> {
    let mut response = client
        .get_models(version, FETCH_TIMEOUT)
        .await
        .map_err(|e| match e.origin {
            crate::providers::codex::client::CodexErrorOrigin::Auth => {
                DiscoveryError::Auth(e.detail.unwrap_or(e.message))
            }
            _ => DiscoveryError::Network(e.detail.unwrap_or(e.message)),
        })?;
    let status = response.status();
    match status.as_u16() {
        200..=299 => {}
        401 | 403 => {
            return Err(DiscoveryError::Auth(format!("HTTP {}", status.as_u16())));
        }
        429 => {
            let after = response
                .headers()
                .get(reqwest::header::RETRY_AFTER)
                .and_then(|v| v.to_str().ok())
                .map(str::to_string);
            return Err(DiscoveryError::RateLimited(after));
        }
        other => return Err(DiscoveryError::Http(other)),
    }
    let mut body = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|e| DiscoveryError::Network(e.without_url().to_string()))?
    {
        if body.len() + chunk.len() > MAX_BODY_BYTES {
            return Err(DiscoveryError::Format("response too large".into()));
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

/// Fetch the Codex catalog and store it in the proxy's own cache. On any
/// error nothing on disk changes.
pub async fn refresh_codex() -> Result<RefreshReport, DiscoveryError> {
    let client = CodexHttpClient::new();
    refresh_codex_with(&client, &own_cache_file()).await
}

pub async fn refresh_codex_with(
    client: &CodexHttpClient,
    cache_path: &Path,
) -> Result<RefreshReport, DiscoveryError> {
    let version = client_version();
    let body = fetch_body(client, &version).await?;
    let entries = parse_catalog(&body)?;
    let before: BTreeSet<String> = allowed_models().into_iter().collect();
    let cache_written = write_cache(cache_path, &entries, &version)
        .map_err(|e| DiscoveryError::Cache(format!("{e:#}")))?;
    invalidate_own_cache();
    let after: BTreeSet<String> = allowed_models().into_iter().collect();
    Ok(RefreshReport {
        endpoint_models: listed_slugs(&entries),
        added: after.difference(&before).cloned().collect(),
        removed: before.difference(&after).cloned().collect(),
        cache_written,
    })
}

// --- CLI -------------------------------------------------------------------

/// Refresh now for a CLI command and print what changed. `Err` carries the
/// reason; the last good lists stay in place.
pub fn refresh_cli() -> Result<RefreshReport, DiscoveryError> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| DiscoveryError::Network(e.to_string()))?;
    runtime.block_on(refresh_codex())
}

pub fn print_refresh(result: &Result<RefreshReport, DiscoveryError>) {
    match result {
        Ok(report) => {
            println!(
                "codex models refreshed: {} listed by the endpoint",
                report.endpoint_models.len()
            );
            for model in &report.added {
                println!("  new model discovered: {model}");
            }
            for model in &report.removed {
                println!("  model no longer available: {model}");
            }
            if report.added.is_empty() && report.removed.is_empty() {
                println!("  no change");
            }
        }
        Err(err) => eprintln!("codex model refresh failed: {err} (keeping the last known list)"),
    }
}

/// `agents sync` refreshes first, but only when discovery is enabled, and a
/// failure never blocks the sync.
pub fn refresh_before_sync() {
    if discovery_enabled() {
        print_refresh(&refresh_cli());
    }
}

// --- background loop -------------------------------------------------------

fn fields(pairs: &[(&str, Value)]) -> Option<Map<String, Value>> {
    Some(
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), v.clone()))
            .collect(),
    )
}

fn log_refresh(log: &Logger, result: &Result<RefreshReport, DiscoveryError>) {
    match result {
        Ok(report) => log.debug(
            "codex models refreshed",
            fields(&[("listed", json!(report.endpoint_models.len()))]),
        ),
        Err(err) => log.warn(
            "codex model refresh failed; keeping the last known list",
            fields(&[("error", json!(err.to_string()))]),
        ),
    }
}

/// Log appearing and disappearing models, saying what happened to their cards.
fn log_model_changes(
    log: &Logger,
    added: &[String],
    removed: &[String],
    actions: &[agent_cards::Action],
) {
    let has = |file: String, create: bool| {
        actions.iter().any(|a| {
            a.file() == file
                && matches!(
                    (a, create),
                    (agent_cards::Action::Create { .. }, true)
                        | (agent_cards::Action::Remove { .. }, false)
                )
        })
    };
    for model in added {
        let message = if has(agent_cards::card_file(model), true) {
            format!("new model discovered: {model}, card created")
        } else {
            format!("new model discovered: {model}")
        };
        log.info(&message, fields(&[("model", json!(model))]));
    }
    for model in removed {
        let message = if has(agent_cards::card_file(model), false) {
            format!("model no longer available: {model}, card removed")
        } else {
            format!("model no longer available: {model}")
        };
        log.info(&message, fields(&[("model", json!(model))]));
    }
}

/// Aborts the background loop when dropped, so it stops with the server.
pub struct BackgroundTasks(tokio::task::JoinHandle<()>);

impl BackgroundTasks {
    /// True once the loop ended on its own (nothing recurring is configured).
    pub fn is_finished(&self) -> bool {
        self.0.is_finished()
    }
}

impl Drop for BackgroundTasks {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// Start the discovery and agent-card loop on the current tokio runtime, or
/// `None` when both are disabled. Each pass refreshes models first and syncs
/// cards afterwards; a pass that changed the model list syncs cards right
/// away. Failures are logged and never fatal.
pub fn spawn_background_tasks() -> Option<BackgroundTasks> {
    let discovery = discovery_enabled();
    let cards = agent_cards::cards_enabled();
    if !discovery && !cards {
        return None;
    }
    let model_interval = refresh_interval();
    let card_interval = agent_cards::sync_interval();
    Some(BackgroundTasks(tokio::spawn(async move {
        let log = create_logger("model-discovery");
        let registry = Arc::new(Registry::with_default_alias());
        let mut known: BTreeSet<String> = allowed_models().into_iter().collect();
        let mut next_models = discovery.then(Instant::now);
        let mut next_cards = cards.then(Instant::now);
        let mut failures = 0u32;
        loop {
            let now = Instant::now();
            let mut added = Vec::new();
            let mut removed = Vec::new();
            if next_models.is_some_and(|due| due <= now) {
                let result = refresh_codex().await;
                log_refresh(&log, &result);
                failures = if result.is_ok() { 0 } else { failures + 1 };
                next_models = next_model_attempt(failures, model_interval).map(|d| now + d);
            }
            // The Codex CLI cache can change too, so diff the effective list
            // on every pass instead of trusting only our own refresh.
            let current: BTreeSet<String> = allowed_models().into_iter().collect();
            if current != known {
                added = current.difference(&known).cloned().collect();
                removed = known.difference(&current).cloned().collect();
                known = current;
            }
            let changed = !added.is_empty() || !removed.is_empty();
            let mut actions = Vec::new();
            if cards && (changed || next_cards.is_some_and(|due| due <= now)) {
                actions = agent_cards::sync_logged(registry.clone()).await;
                next_cards = card_interval.map(|d| Instant::now() + d);
            }
            log_model_changes(&log, &added, &removed, &actions);
            let Some(wake) = [next_models, next_cards].into_iter().flatten().min() else {
                break;
            };
            tokio::time::sleep_until(wake.into()).await;
        }
    })))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn body(models: Value) -> Vec<u8> {
        json!({ "models": models }).to_string().into_bytes()
    }

    #[test]
    fn interval_defaults_and_zero_means_startup_only() {
        assert_eq!(parse_interval(None), Some(Duration::from_secs(900)));
        assert_eq!(parse_interval(Some(" 60 ")), Some(Duration::from_secs(60)));
        assert_eq!(
            parse_interval(Some("nonsense")),
            Some(Duration::from_secs(900))
        );
        assert_eq!(parse_interval(Some("0")), None);
    }

    #[test]
    fn retries_back_off_but_never_exceed_the_interval() {
        let interval = Some(Duration::from_secs(900));
        assert_eq!(next_model_attempt(0, interval), interval);
        assert_eq!(
            next_model_attempt(1, interval),
            Some(Duration::from_secs(30))
        );
        assert_eq!(
            next_model_attempt(2, interval),
            Some(Duration::from_secs(60))
        );
        assert_eq!(
            next_model_attempt(20, interval),
            Some(Duration::from_secs(900))
        );
        assert_eq!(
            next_model_attempt(3, Some(Duration::from_secs(10))),
            Some(Duration::from_secs(10))
        );
    }

    #[test]
    fn interval_zero_runs_once_and_retries_only_a_few_startup_failures() {
        assert_eq!(next_model_attempt(0, None), None);
        assert!(next_model_attempt(1, None).is_some());
        assert!(next_model_attempt(MAX_STARTUP_ATTEMPTS - 1, None).is_some());
        assert_eq!(next_model_attempt(MAX_STARTUP_ATTEMPTS, None), None);
    }

    #[test]
    fn parse_keeps_slim_entries_and_applies_the_listing_rules() {
        let entries = parse_catalog(&body(json!([
            {"slug":"gpt-9-new","display_name":"GPT 9","visibility":"list","supported_in_api":true,"huge":{"x":1}},
            {"slug":"gpt-9-hidden","visibility":"hide","supported_in_api":true},
            {"slug":"gpt-9-noapi","visibility":"list","supported_in_api":false},
            {"slug":"bad slug\n","visibility":"list","supported_in_api":true},
            {"visibility":"list"}
        ])))
        .unwrap();
        assert_eq!(listed_slugs(&entries), vec!["gpt-9-new"]);
        assert_eq!(entries.len(), 3);
        assert!(entries[0].get("huge").is_none());
    }

    #[test]
    fn parse_rejects_shapes_that_could_empty_the_list() {
        for bad in [
            b"not json".to_vec(),
            b"{}".to_vec(),
            br#"{"models":"x"}"#.to_vec(),
            body(json!([])),
            body(json!([{"slug":"a","visibility":"hide","supported_in_api":true}])),
        ] {
            assert!(matches!(
                parse_catalog(&bad),
                Err(DiscoveryError::Format(_))
            ));
        }
    }

    #[test]
    fn cache_write_is_atomic_and_skips_identical_content() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state/codex-models.json");
        let entries = parse_catalog(&body(json!([
            {"slug":"gpt-9-new","visibility":"list","supported_in_api":true}
        ])))
        .unwrap();
        assert!(write_cache(&path, &entries, "1.2.3").unwrap());
        assert!(!write_cache(&path, &entries, "1.2.3").unwrap());
        let doc: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(doc["client_version"], "1.2.3");
        assert_eq!(doc["models"][0]["slug"], "gpt-9-new");
        assert_eq!(
            std::fs::read_dir(path.parent().unwrap()).unwrap().count(),
            1
        );
    }

    #[test]
    fn models_url_hangs_off_the_codex_api_root() {
        use crate::providers::codex::client::models_endpoint;
        for base in [
            "https://chatgpt.com/backend-api/codex/responses",
            "https://chatgpt.com/backend-api/codex/",
            "https://chatgpt.com/backend-api/codex",
        ] {
            assert_eq!(
                models_endpoint(base, "0.163.0"),
                "https://chatgpt.com/backend-api/codex/models?client_version=0.163.0"
            );
        }
    }

    #[test]
    fn newest_known_client_version_wins() {
        assert!(version_key("0.170.2") > version_key("0.163.0"));
        assert!(version_key("0.9.0") < version_key("0.10.0"));
    }
}
