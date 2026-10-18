use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::SystemTime;

use once_cell::sync::Lazy;
use serde_json::Value;

use crate::config;
use crate::providers::codex::auth::token_store::codex_auth_file;

use super::request::ServiceTier;

const BUNDLED_MODELS: &[&str] = &[
    "gpt-5.2",
    "gpt-5.3-codex",
    "gpt-5.3-codex-spark",
    "gpt-5.4",
    "gpt-5.4-mini",
    "gpt-5.5",
    "gpt-5.6-luna",
    "gpt-5.6-sol",
    "gpt-5.6-terra",
    "gpt-6-astra",
    "gpt-6-luna",
    "gpt-6-sol",
    "gpt-6.1-sol",
];

/// Cached parse of `models_cache.json`, keyed by path, mtime and size so a
/// rewrite by the Codex CLI is picked up without restarting the proxy.
#[derive(Default)]
struct CatalogCache {
    stamp: Option<(PathBuf, SystemTime, u64)>,
    models: Vec<String>,
}

static CLI_CACHE: Lazy<Mutex<CatalogCache>> = Lazy::new(Mutex::default);
/// Same shape for the proxy's own `codex-models.json`, written by
/// `model_discovery` after fetching the catalog itself.
static OWN_CACHE: Lazy<Mutex<CatalogCache>> = Lazy::new(Mutex::default);

/// Whether a catalog entry is usable through this proxy: listed in the model
/// picker and callable through the API. The endpoint and the Codex CLI cache
/// share this shape (`ModelsResponse`), so one rule serves both.
pub fn is_listed_api_model(entry: &Value) -> bool {
    entry["visibility"] == "list" && entry["supported_in_api"] == true
}

/// Listed, API-capable slugs in the cache file at `path`. Re-reads only when
/// the file changed since the last call; empty when it is missing or invalid.
fn load_cli_models(cache: &Mutex<CatalogCache>, path: &Path) -> Vec<String> {
    let mut cache = cache.lock().unwrap_or_else(|e| e.into_inner());
    let stamp = std::fs::metadata(path)
        .ok()
        .and_then(|meta| Some((path.to_path_buf(), meta.modified().ok()?, meta.len())));
    if stamp.is_some() && stamp == cache.stamp {
        return cache.models.clone();
    }
    let parsed = stamp
        .as_ref()
        .and_then(|_| std::fs::read(path).ok())
        .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok());
    match (&stamp, parsed) {
        (_, Some(cache_file)) => {
            cache.models = cache_file["models"]
                .as_array()
                .into_iter()
                .flatten()
                .filter(|m| is_listed_api_model(m))
                .filter_map(|m| m["slug"].as_str().map(str::to_string))
                .collect();
        }
        // Missing file: no catalog. Unreadable or half-written file: keep the
        // last good catalog rather than dropping models (and their agent cards).
        (None, None) => cache.models.clear(),
        (Some(_), None) => {}
    }
    cache.stamp = stamp;
    cache.models.clone()
}

/// Listed, API-capable slugs the Codex CLI last fetched for this account into
/// `models_cache.json` next to `auth.json`. Empty without a cache, and in unit
/// tests so they never depend on the developer's account.
fn cli_models() -> Vec<String> {
    if cfg!(test) {
        return Vec::new();
    }
    load_cli_models(
        &CLI_CACHE,
        &codex_auth_file().with_file_name("models_cache.json"),
    )
}

/// Path of the proxy's own catalog cache (`codex-models.json` in the state
/// directory). Never the Codex CLI's `models_cache.json`.
pub fn own_cache_file() -> PathBuf {
    crate::paths::state_dir().join("codex-models.json")
}

/// Listed, API-capable slugs the proxy itself last fetched. Empty in unit
/// tests so they never read the developer's state directory.
fn own_models() -> Vec<String> {
    if cfg!(test) {
        return Vec::new();
    }
    load_cli_models(&OWN_CACHE, &own_cache_file())
}

/// Forget the cached parse of the proxy's own cache so the next read goes to
/// disk even if a rewrite landed inside one mtime tick.
pub fn invalidate_own_cache() {
    OWN_CACHE.lock().unwrap_or_else(|e| e.into_inner()).stamp = None;
}

/// Slugs the account's catalogs list: the Codex CLI's cache plus the proxy's
/// own fetched one.
fn catalog_models() -> Vec<String> {
    let mut models = cli_models();
    models.extend(own_models());
    models.sort_unstable();
    models.dedup();
    models
}

/// Models the Codex backend accepts: the bundled list plus the account's
/// catalogs (the proxy's own and the Codex CLI's), so a model the account
/// gains is usable without a release or a restart.
pub fn allowed_models() -> Vec<String> {
    let mut models: Vec<String> = BUNDLED_MODELS.iter().map(|m| m.to_string()).collect();
    models.extend(catalog_models());
    models.sort_unstable();
    models.dedup();
    models
}

/// Every accepted model plus its `-fast` priority alias, as listed to clients.
pub fn listed_models() -> Vec<String> {
    let mut models: Vec<String> = allowed_models()
        .into_iter()
        .flat_map(|m| [m.clone(), format!("{m}-fast")])
        .collect();
    models.sort_unstable();
    models
}

/// Alias targets in preference order. The first one the account's CLI catalog
/// lists wins, so an account without `gpt-6-sol` still gets a working Opus
/// slot. Without a catalog the first entry is used.
const OPUS_TARGETS: &[&str] = &["gpt-6-sol", "gpt-6.1-sol", "gpt-6-astra", "gpt-5.6-sol"];
const SONNET_TARGETS: &[&str] = &["gpt-5.6-terra"];
const HAIKU_TARGETS: &[&str] = &["gpt-6-luna", "gpt-5.6-luna"];

pub const MODEL_ALIASES: &[(&str, &[&str])] = &[
    ("haiku", HAIKU_TARGETS),
    ("claude-haiku-4-5", HAIKU_TARGETS),
    ("claude-haiku-4-5-20251001", HAIKU_TARGETS),
    ("claude-haiku-5-5", HAIKU_TARGETS),
    ("sonnet", SONNET_TARGETS),
    ("claude-sonnet-4-6", SONNET_TARGETS),
    ("claude-sonnet-5", SONNET_TARGETS),
    ("claude-sonnet-5-5", SONNET_TARGETS),
    ("opus", OPUS_TARGETS),
    ("claude-opus-4-7", OPUS_TARGETS),
    ("claude-opus-4-8", OPUS_TARGETS),
    ("claude-opus-5", OPUS_TARGETS),
    ("claude-opus-5-5", OPUS_TARGETS),
    ("fable", OPUS_TARGETS),
    ("claude-fable-5", OPUS_TARGETS),
    ("claude-fable-5-1", OPUS_TARGETS),
];

/// Resolve a Claude-style alias to the preferred model the account has.
pub fn alias_target(model: &str) -> Option<&'static str> {
    alias_target_in(model, &catalog_models())
}

fn alias_target_in(model: &str, catalog: &[String]) -> Option<&'static str> {
    let targets = MODEL_ALIASES.iter().find(|(alias, _)| *alias == model)?.1;
    Some(
        targets
            .iter()
            .copied()
            .find(|target| catalog.iter().any(|m| m == target))
            .or_else(|| {
                targets
                    .iter()
                    .copied()
                    .find(|target| BUNDLED_MODELS.contains(target))
            })
            .unwrap_or(targets[0]),
    )
}

#[derive(Debug, Clone)]
pub struct ResolvedModel {
    pub model: String,
    pub service_tier: Option<ServiceTier>,
}

fn fast_model_aliases() -> HashSet<String> {
    allowed_models()
        .iter()
        .map(|m| format!("{m}-fast"))
        .collect()
}

fn resolve_fast_model_alias(model: &str) -> ResolvedModel {
    let fast_set = fast_model_aliases();
    if fast_set.contains(model) {
        let base = model.trim_end_matches("-fast");
        ResolvedModel {
            model: base.to_string(),
            service_tier: Some(ServiceTier::Priority),
        }
    } else {
        ResolvedModel {
            model: model.to_string(),
            service_tier: None,
        }
    }
}

pub fn resolve_model_request(model: &str) -> ResolvedModel {
    resolve_model_request_with_config_override(model, true)
}

pub fn resolve_model_request_with_config_override(
    model: &str,
    apply_config_override: bool,
) -> ResolvedModel {
    let alias = alias_target(model).unwrap_or(model);

    let requested = resolve_fast_model_alias(alias);

    let override_model = apply_config_override.then(config::codex_model).flatten();
    let resolved = match override_model {
        Some(ref val) if !val.is_empty() => resolve_fast_model_alias(val),
        _ => requested.clone(),
    };

    ResolvedModel {
        model: resolved.model,
        service_tier: if requested.service_tier == Some(ServiceTier::Priority)
            || resolved.service_tier == Some(ServiceTier::Priority)
        {
            Some(ServiceTier::Priority)
        } else {
            resolved.service_tier
        },
    }
}

pub fn resolve_model(model: &str) -> String {
    resolve_model_request(model).model
}

#[derive(Debug, Clone)]
pub struct ModelNotAllowedError {
    pub model: String,
}

impl std::fmt::Display for ModelNotAllowedError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Model not allowed: {}", self.model)
    }
}

pub fn assert_allowed_model(model: &str) -> Result<(), ModelNotAllowedError> {
    if allowed_models().iter().any(|m| m == model) {
        Ok(())
    } else {
        Err(ModelNotAllowedError {
            model: model.to_string(),
        })
    }
}

/// Everything from GPT-5.6 on, including models discovered from the Codex CLI
/// cache, runs on the Responses Lite lane. Only the older full-lane models are
/// named.
pub fn uses_responses_lite(model: &str) -> bool {
    !matches!(
        model,
        "gpt-5.2"
            | "gpt-5.3-codex"
            | "gpt-5.3-codex-spark"
            | "gpt-5.4"
            | "gpt-5.4-mini"
            | "gpt-5.5"
    )
}

/// Luna models exist only behind the Responses Lite lane; the full
/// Responses API resolves them to a `-free` variant and returns 404 (Model not
/// found gpt-5.6-luna-free-...). Hosted web_search requests must run on the
/// full lane, so luna is upgraded to its nearest full-lane sibling.
pub fn full_lane_web_search_model(model: &str) -> &str {
    match model {
        "gpt-5.6-luna" => "gpt-5.6-sol",
        "gpt-6-luna" => "gpt-6-sol",
        _ => model,
    }
}

pub fn is_valid_model_for_codex(model: &str) -> bool {
    if allowed_models().iter().any(|m| m == model) {
        return true;
    }
    let fast_set = fast_model_aliases();
    if fast_set.contains(model) {
        return true;
    }
    MODEL_ALIASES.iter().any(|(alias, _)| *alias == model)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn haiku_resolves_to_luna() {
        let r = resolve_model_request("haiku");
        assert_eq!(r.model, "gpt-6-luna");
    }

    #[test]
    fn web_search_upgrades_luna_to_full_lane_sibling() {
        assert_eq!(full_lane_web_search_model("gpt-5.6-luna"), "gpt-5.6-sol");
        assert_eq!(full_lane_web_search_model("gpt-5.6-sol"), "gpt-5.6-sol");
        assert_eq!(full_lane_web_search_model("gpt-5.6-terra"), "gpt-5.6-terra");
        assert_eq!(full_lane_web_search_model("gpt-5.4"), "gpt-5.4");
        assert_eq!(full_lane_web_search_model("gpt-6-luna"), "gpt-6-sol");
        assert_eq!(full_lane_web_search_model("gpt-6-sol"), "gpt-6-sol");
    }

    #[test]
    fn haiku_5_5_resolves_to_luna() {
        assert_eq!(
            resolve_model_request("claude-haiku-5-5").model,
            "gpt-6-luna"
        );
    }

    #[test]
    fn sonnet_resolves_to_terra() {
        let r = resolve_model_request("sonnet");
        assert_eq!(r.model, "gpt-5.6-terra");
    }

    #[test]
    fn sonnet_5_resolves_to_terra() {
        for model in ["claude-sonnet-5", "claude-sonnet-5-5"] {
            let r = resolve_model_request(model);
            assert_eq!(r.model, "gpt-5.6-terra");
        }
    }

    #[test]
    fn opus_resolves_to_sol() {
        let r = resolve_model_request("opus");
        assert_eq!(r.model, "gpt-6-sol");
    }

    #[test]
    fn opus_aliases_resolve_to_sol() {
        for model in ["claude-opus-4-8", "claude-opus-5", "claude-opus-5-5"] {
            let r = resolve_model_request(model);
            assert_eq!(r.model, "gpt-6-sol");
        }
    }

    #[test]
    fn fable_5_resolves_to_sol() {
        for model in ["fable", "claude-fable-5", "claude-fable-5-1"] {
            let r = resolve_model_request(model);
            assert_eq!(r.model, "gpt-6-sol");
        }
    }

    #[test]
    fn gpt_6_sol_fast_adds_priority() {
        for model in ["gpt-6-sol", "gpt-6.1-sol"] {
            let r = resolve_model_request(&format!("{model}-fast"));
            assert_eq!(r.model, model);
            assert_eq!(r.service_tier, Some(ServiceTier::Priority));
        }
    }

    #[test]
    fn gpt_6_models_use_responses_lite() {
        assert!(uses_responses_lite("gpt-6-sol"));
        assert!(uses_responses_lite("gpt-6-luna"));
        assert!(uses_responses_lite("gpt-6.1-sol"));
        assert!(uses_responses_lite("gpt-7-new"));
        assert!(!uses_responses_lite("gpt-5.5"));
    }

    #[test]
    fn alias_prefers_the_first_target_the_catalog_lists() {
        let catalog = ["gpt-6-astra".to_string(), "gpt-5.6-terra".to_string()];
        assert_eq!(alias_target_in("opus", &catalog), Some("gpt-6-astra"));
        assert_eq!(
            alias_target_in("claude-fable-5-1", &catalog),
            Some("gpt-6-astra")
        );
        assert_eq!(alias_target_in("haiku", &catalog), Some("gpt-6-luna"));
        assert_eq!(alias_target_in("opus", &[]), Some("gpt-6-sol"));
        assert_eq!(alias_target_in("gpt-6-astra", &catalog), None);
    }

    #[test]
    fn listed_models_pair_every_model_with_fast() {
        let listed = listed_models();
        for model in allowed_models() {
            assert!(listed.contains(&model));
            assert!(listed.contains(&format!("{model}-fast")));
        }
    }

    #[test]
    fn fast_suffix_adds_priority() {
        let r = resolve_model_request("gpt-5.6-sol-fast");
        assert_eq!(r.model, "gpt-5.6-sol");
        assert_eq!(r.service_tier, Some(ServiceTier::Priority));
    }

    #[test]
    fn allowed_models_accept_base() {
        assert!(assert_allowed_model("gpt-5.4").is_ok());
        assert!(assert_allowed_model("gpt-5.6-sol").is_ok());
        assert!(assert_allowed_model("gpt-5.6-terra").is_ok());
        assert!(assert_allowed_model("gpt-6-astra").is_ok());
        assert!(assert_allowed_model("gpt-5.6-luna").is_ok());
        assert!(assert_allowed_model("gpt-6.1-sol").is_ok());
    }

    #[test]
    fn not_allowed_rejected() {
        assert!(assert_allowed_model("not-a-model").is_err());
    }

    #[test]
    fn cli_catalog_reloads_when_the_cache_file_changes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("models_cache.json");
        let cache = Mutex::default();
        assert!(load_cli_models(&cache, &path).is_empty());

        let entry = |slug: &str, visibility: &str| serde_json::json!({"slug": slug, "visibility": visibility, "supported_in_api": true});
        let write = |models: Vec<Value>| {
            std::fs::write(&path, serde_json::json!({ "models": models }).to_string()).unwrap();
        };
        write(vec![
            entry("gpt-9-a", "list"),
            entry("gpt-9-hidden", "hide"),
        ]);
        assert_eq!(load_cli_models(&cache, &path), vec!["gpt-9-a"]);

        // Same-size rewrite still has to be seen once the mtime moves.
        write(vec![entry("gpt-9-a", "list"), entry("gpt-9-b", "list")]);
        assert_eq!(load_cli_models(&cache, &path), vec!["gpt-9-a", "gpt-9-b"]);

        write(vec![entry("gpt-9-b", "list")]);
        assert_eq!(load_cli_models(&cache, &path), vec!["gpt-9-b"]);

        std::fs::remove_file(&path).unwrap();
        assert!(load_cli_models(&cache, &path).is_empty());
    }
}
