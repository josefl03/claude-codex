use crate::{
    anthropic::{json_error, schema::MessagesRequest},
    config::AliasProvider,
    provider::{CliHandlers, Provider, RequestContext},
};
use anyhow::{Result, anyhow};
use async_trait::async_trait;
use axum::{http::StatusCode, response::Response};
use std::collections::BTreeMap;
use std::sync::{Arc, RwLock};

pub const ANTHROPIC_STYLE_ALIASES: &[&str] = &[
    "haiku",
    "claude-haiku-4-5",
    "claude-haiku-4-5-20251001",
    "claude-haiku-5-5",
    "sonnet",
    "claude-sonnet-4-6",
    "claude-sonnet-5",
    "claude-sonnet-5-5",
    "opus",
    "claude-opus-4-7",
    "claude-opus-4-8",
    "claude-opus-5",
    "claude-opus-5-5",
    "fable",
    "claude-fable-5",
    "claude-fable-5-1",
];

pub const CURSOR_PREFIXES: &[&str] = &["cursor:", "cursor-plan:", "cursor-ask:"];

const CURSOR_LEGACY_MODELS: &[&str] = &[
    "cursor",
    "cursor-agent",
    "cursor-composer",
    "cursor-composer-fast",
    "cursor-plan",
    "cursor-ask",
    "composer-2.5",
    "composer-2.5-fast",
];

pub(crate) const KIMI_MODELS: &[&str] = &["kimi-for-coding", "kimi-k2.6", "kimi-k3", "k2.6", "k3"];
pub(crate) const GROK_MODELS: &[&str] =
    &["grok-composer-2.5-fast", "grok-4.5", "grok-4.6", "grok-4.7"];

pub struct Registry {
    alias_provider: AliasProvider,
    models: RwLock<BTreeMap<String, Vec<String>>>,
    /// Whether `refresh` may rebuild the model lists (false for injected providers).
    dynamic: bool,
    handlers: BTreeMap<String, Arc<dyn Provider>>,
}

impl Registry {
    pub fn new(alias_provider: AliasProvider) -> Self {
        let mut models: BTreeMap<String, Vec<String>> = BTreeMap::new();
        models.insert(
            "anthropic".into(),
            ANTHROPIC_STYLE_ALIASES
                .iter()
                .map(|alias| (*alias).to_string())
                .collect(),
        );
        models.insert(
            "codex".into(),
            crate::providers::codex::translate::model_allowlist::listed_models(),
        );
        models.insert(
            "kimi".into(),
            KIMI_MODELS.iter().map(|m| (*m).to_string()).collect(),
        );
        models.insert("cursor".into(), build_cursor_models());
        models.insert(
            "grok".into(),
            GROK_MODELS
                .iter()
                .map(|model| (*model).to_string())
                .collect(),
        );
        let mut handlers = BTreeMap::new();
        for (name, entries) in &models {
            let handler: Arc<dyn Provider> = match name.as_str() {
                "anthropic" => Arc::new(crate::providers::anthropic::AnthropicProvider::new()),
                "codex" => Arc::new(crate::providers::codex::CodexProvider::new()),
                "kimi" => Arc::new(crate::providers::kimi::KimiProvider::new()),
                "cursor" => Arc::new(crate::providers::cursor::CursorProvider::new()),
                "grok" => Arc::new(crate::providers::grok::GrokProvider::new()),
                _ => Arc::new(PlaceholderProvider::new(name, entries.clone())),
            };
            handlers.insert(name.clone(), handler);
        }

        Self {
            alias_provider,
            models: RwLock::new(models),
            dynamic: true,
            handlers,
        }
    }

    pub fn with_default_alias() -> Self {
        Self::new(crate::config::alias_provider())
    }

    pub fn from_providers(
        alias_provider: AliasProvider,
        providers: impl IntoIterator<Item = Arc<dyn Provider>>,
    ) -> Self {
        let mut models = BTreeMap::new();
        let mut handlers = BTreeMap::new();
        for provider in providers {
            let name = provider.name().to_string();
            models.insert(name.clone(), provider.supported_models());
            handlers.insert(name, provider);
        }
        Self {
            alias_provider,
            models: RwLock::new(models),
            dynamic: false,
            handlers,
        }
    }

    /// Re-read the model lists that can change while the proxy runs (the
    /// proxy's own `codex-models.json` and the Codex CLI's `models_cache.json`).
    /// Returns whether anything changed.
    pub fn refresh(&self) -> bool {
        if !self.dynamic {
            return false;
        }
        let codex = crate::providers::codex::translate::model_allowlist::listed_models();
        let mut models = self.models.write().unwrap_or_else(|e| e.into_inner());
        if models.get("codex") == Some(&codex) {
            return false;
        }
        models.insert("codex".into(), codex);
        true
    }

    pub fn list_provider_names(&self) -> Vec<String> {
        let mut names: Vec<String> = self.handlers.keys().cloned().collect();
        names.sort_unstable();
        names
    }

    pub fn provider(&self, name: &str) -> Option<Arc<dyn Provider>> {
        self.handlers.get(name).cloned()
    }

    pub fn supported_models_for(&self, provider: &str) -> Vec<String> {
        let mut models = self
            .models
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .get(provider)
            .cloned()
            .unwrap_or_default();
        if provider == self.alias_provider.as_str() {
            for alias in ANTHROPIC_STYLE_ALIASES {
                if !models.iter().any(|value| value == alias) {
                    models.push((*alias).to_string());
                }
            }
        }
        models.sort_unstable();
        models
    }

    pub fn all_supported_models(&self) -> Vec<(String, String)> {
        self.refresh();
        let mut out = Vec::new();
        for provider in self.handlers.keys() {
            for model in self.supported_models_for(provider) {
                out.push((model, provider.clone()));
            }
        }
        out
    }

    pub fn grouped_models(&self) -> BTreeMap<String, Vec<String>> {
        self.refresh();
        let mut out = BTreeMap::new();
        for provider in self.handlers.keys() {
            out.insert(provider.clone(), self.supported_models_for(provider));
        }
        out
    }

    /// `(id, provider)` pairs the `-latest` aliases are computed from: the real
    /// Codex model table, without the Anthropic-style aliases.
    fn latest_candidates(&self) -> Vec<(String, String)> {
        let table = self.models.read().unwrap_or_else(|e| e.into_inner());
        table
            .iter()
            .filter(|(name, _)| name.as_str() == "codex")
            .flat_map(|(name, models)| models.iter().map(|m| (m.clone(), name.clone())))
            .collect()
    }

    /// Every `<family>-latest` / `<family>-<tier>-latest` alias that currently
    /// has a target, sorted by alias. Empty when `CCP_LATEST_ALIASES=0`.
    pub fn latest_aliases(&self) -> Vec<crate::latest::LatestAlias> {
        if !crate::latest::enabled() {
            return Vec::new();
        }
        self.refresh();
        crate::latest::compute(&self.latest_candidates())
    }

    /// Resolve a requested id that may be a `-latest` alias. Call it before
    /// `provider_for_model` and use the target id from then on.
    pub fn resolve_latest(&self, requested: &str) -> crate::latest::Resolution {
        if !crate::latest::enabled() || crate::latest::parse_alias(requested).is_none() {
            return crate::latest::Resolution::NotAlias;
        }
        self.refresh();
        crate::latest::resolve(&self.latest_candidates(), requested)
    }

    pub fn provider_for_model(
        &self,
        raw_model: &str,
        session_affinity: Option<&AliasProvider>,
    ) -> Option<Arc<dyn Provider>> {
        let normalized = normalize_incoming_model(raw_model);
        // Claude-shaped models always resolve to the configured alias target (the
        // Anthropic passthrough by default). Session affinity is deliberately NOT
        // consulted here: a codex request earlier in the same session must never drag
        // the opus/haiku slots off the Anthropic backend. This is what lets the opus
        // slot stay on Max while the sonnet slot runs on codex within one session.
        let _ = session_affinity;
        if is_anthropic_alias(&normalized) || normalized.starts_with("claude-") {
            return self.handlers.get(self.alias_provider.as_str()).cloned();
        }
        if is_cursor_model(&normalized) {
            return self.handlers.get("cursor").cloned();
        }

        // Exact model-name match reaches a specific backend regardless of the alias
        // target: this is how `ANTHROPIC_DEFAULT_SONNET_MODEL=gpt-5.6-terra` sends the
        // sonnet slot to codex even while aliases default to the Anthropic passthrough.
        self.provider_in_table(&normalized).or_else(|| {
            // A model the Codex CLI catalog gained since startup.
            self.refresh()
                .then(|| self.provider_in_table(&normalized))?
        })
    }

    fn provider_in_table(&self, normalized: &str) -> Option<Arc<dyn Provider>> {
        let table = self.models.read().unwrap_or_else(|e| e.into_inner());
        table
            .iter()
            .filter(|(name, _)| name.as_str() != "anthropic")
            .find(|(_, models)| models.iter().any(|candidate| candidate == normalized))
            .and_then(|(name, _)| self.handlers.get(name).cloned())
    }

    /// Message for an alias with no model behind it (a 400 for the client).
    pub fn no_latest_candidates_message(&self, alias: &str) -> String {
        let available: Vec<String> = self.latest_aliases().into_iter().map(|a| a.alias).collect();
        let list = if available.is_empty() {
            "none right now".to_string()
        } else {
            available.join(", ")
        };
        format!("No model matches the alias \"{alias}\". Available latest aliases: {list}.")
    }

    pub fn unknown_model_message(&self) -> String {
        let mut parts = Vec::new();
        for (provider, models) in self.grouped_models() {
            let mut models = models;
            models.sort_unstable();
            parts.push(format!("{}: {}", provider, models.join(", ")));
        }
        format!("Supported: {}.", parts.join("; "))
    }
}

pub fn normalize_incoming_model(model: &str) -> String {
    let suffix = "[1m]";
    if model.len() >= suffix.len() && model.to_ascii_lowercase().ends_with(suffix) {
        return model[..model.len() - suffix.len()].to_string();
    }
    model.to_string()
}

pub fn is_anthropic_alias(model: &str) -> bool {
    ANTHROPIC_STYLE_ALIASES.contains(&model)
}

pub fn is_cursor_model(model: &str) -> bool {
    if CURSOR_LEGACY_MODELS.contains(&model) {
        return true;
    }

    CURSOR_PREFIXES
        .iter()
        .any(|prefix| model.starts_with(prefix))
}

struct PlaceholderProvider {
    name: &'static str,
    models: Vec<String>,
}

impl PlaceholderProvider {
    fn new(name: &str, models: Vec<String>) -> Self {
        let name = match name {
            "codex" => "codex",
            "kimi" => "kimi",
            "cursor" => "cursor",
            "grok" => "grok",
            _ => "codex",
        };
        Self { name, models }
    }
}

#[async_trait]
impl Provider for PlaceholderProvider {
    fn name(&self) -> &'static str {
        self.name
    }

    fn supported_models(&self) -> Vec<String> {
        self.models.clone()
    }

    fn cli(&self) -> &'static dyn CliHandlers {
        match self.name {
            "codex" => &CODEX_CLI,
            "kimi" => &KIMI_CLI,
            "cursor" => &CURSOR_CLI,
            "grok" => &GROK_CLI,
            _ => &CODEX_CLI,
        }
    }

    async fn handle_messages(&self, _body: MessagesRequest, ctx: RequestContext) -> Response {
        placeholder_provider_response("messages", &ctx.provider)
    }

    async fn handle_count_tokens(&self, _body: MessagesRequest, ctx: RequestContext) -> Response {
        placeholder_provider_response("count_tokens", &ctx.provider)
    }
}

fn placeholder_provider_response(route: &str, provider: &str) -> Response {
    let _ = route;
    json_error(
        StatusCode::NOT_IMPLEMENTED,
        "unsupported_provider_error",
        format!("provider '{}' is not yet implemented", provider),
    )
}

#[derive(Clone, Copy)]
struct PlaceholderCli {
    provider: &'static str,
}

impl CliHandlers for PlaceholderCli {
    fn login(&self) -> Result<()> {
        Err(anyhow!("{}: browser login not supported", self.provider))
    }

    fn device(&self) -> Result<()> {
        Err(anyhow!("{}: device login not supported", self.provider))
    }

    fn status(&self) -> Result<()> {
        use serde_json::Value;
        let path = crate::paths::provider_auth_file(self.provider);
        let legacy = crate::paths::provider_legacy_auth_file(self.provider);
        if crate::auth::load_auth_file_with_legacy::<Value>(&path, &legacy).is_some() {
            Ok(())
        } else {
            Err(anyhow!("Not authenticated"))
        }
    }

    fn logout(&self) -> Result<()> {
        let path = crate::paths::provider_auth_file(self.provider);
        let legacy = crate::paths::provider_legacy_auth_file(self.provider);
        let _ = crate::auth::delete_auth_file(&path, &legacy);
        Ok(())
    }
}

const CODEX_CLI: PlaceholderCli = PlaceholderCli { provider: "codex" };
const KIMI_CLI: PlaceholderCli = PlaceholderCli { provider: "kimi" };
const CURSOR_CLI: PlaceholderCli = PlaceholderCli { provider: "cursor" };
const GROK_CLI: PlaceholderCli = PlaceholderCli { provider: "grok" };
fn build_cursor_models() -> Vec<String> {
    let mut out: Vec<String> = CURSOR_LEGACY_MODELS
        .iter()
        .map(|s| (*s).to_string())
        .collect();
    out.sort_unstable();
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::latest::Resolution;

    #[test]
    fn latest_aliases_resolve_against_the_bundled_list() {
        let registry = Registry::new(AliasProvider::Anthropic);
        let aliases = registry.latest_aliases();
        assert!(aliases.iter().any(|a| a.alias == "gpt-sol-latest"));
        assert!(
            aliases
                .iter()
                .all(|a| a.provider == "codex" && a.family == "gpt")
        );
        assert_eq!(registry.resolve_latest("grok-latest"), Resolution::NotAlias);
        // No alias collides with a real id, and Claude ids never get one.
        let real: Vec<String> = registry
            .all_supported_models()
            .into_iter()
            .map(|(m, _)| m)
            .collect();
        assert!(aliases.iter().all(|a| !real.contains(&a.alias)));
        assert!(aliases.iter().all(|a| !a.alias.starts_with("claude")));
        match registry.resolve_latest("gpt-sol-latest") {
            Resolution::Resolved(a) => assert_eq!(a.provider, "codex"),
            other => panic!("{other:?}"),
        }
        assert_eq!(registry.resolve_latest("opus"), Resolution::NotAlias);
        assert_eq!(registry.resolve_latest("gpt-6-sol"), Resolution::NotAlias);
        assert!(matches!(
            registry.resolve_latest("gpt-nova-latest"),
            Resolution::NoCandidates { .. }
        ));
        let message = registry.no_latest_candidates_message("gpt-nova-latest");
        assert!(message.contains("gpt-nova-latest") && message.contains("gpt-sol-latest"));
    }

    #[test]
    fn normalize_model_trims_hint() {
        assert_eq!(normalize_incoming_model("gpt-5.4-fast[1m]"), "gpt-5.4-fast");
        assert_eq!(normalize_incoming_model("gpt-5.4-fast"), "gpt-5.4-fast");
    }

    #[test]
    fn alias_routes_to_configured_provider() {
        let registry = Registry::new(AliasProvider::Kimi);
        let p = registry.provider_for_model("haiku", None);
        assert!(p.is_some());
        assert_eq!(p.expect("provider").name(), "kimi");
    }

    #[test]
    fn opus_4_8_routes_to_configured_provider() {
        let registry = Registry::new(AliasProvider::Codex);
        let p = registry.provider_for_model("claude-opus-4-8", None);
        assert!(p.is_some());
        assert_eq!(p.expect("provider").name(), "codex");
    }

    #[test]
    fn claude_5_aliases_route_to_configured_provider() {
        let registry = Registry::new(AliasProvider::Codex);
        for model in [
            "claude-sonnet-5",
            "claude-sonnet-5-5",
            "claude-opus-5",
            "claude-opus-5-5",
            "fable",
            "claude-fable-5",
            "claude-fable-5-1",
        ] {
            let p = registry.provider_for_model(model, None);
            assert!(p.is_some(), "{model} should route to a provider");
            assert_eq!(p.expect("provider").name(), "codex");
        }
    }

    #[test]
    fn claude_models_route_to_anthropic_passthrough_by_default() {
        let registry = Registry::new(AliasProvider::Anthropic);
        for model in [
            "opus",
            "claude-opus-4-8",
            "sonnet",
            "haiku",
            "claude-3-5-haiku-20241022",
        ] {
            let p = registry.provider_for_model(model, None);
            assert!(p.is_some(), "{model} should route");
            assert_eq!(p.expect("provider").name(), "anthropic", "{model}");
        }
    }

    #[test]
    fn explicit_codex_model_routes_to_codex_while_default_is_anthropic() {
        let registry = Registry::new(AliasProvider::Anthropic);
        let p = registry.provider_for_model("gpt-5.6-terra", None);
        assert_eq!(p.expect("provider").name(), "codex");
    }

    #[test]
    fn session_affinity_cannot_hijack_claude_slot() {
        // Even if a prior codex request set Codex affinity, claude aliases stay on anthropic.
        let registry = Registry::new(AliasProvider::Anthropic);
        let p = registry.provider_for_model("claude-opus-4-8", Some(&AliasProvider::Codex));
        assert_eq!(p.expect("provider").name(), "anthropic");
    }

    #[test]
    fn cursor_prefix_routes() {
        let registry = Registry::new(AliasProvider::Codex);
        assert_eq!(
            registry
                .provider_for_model("cursor:gpt-5.5", None)
                .unwrap()
                .name(),
            "cursor"
        );
        assert_eq!(
            registry
                .provider_for_model("cursor-plan:gpt-5.5", None)
                .unwrap()
                .name(),
            "cursor"
        );
        assert_eq!(
            registry
                .provider_for_model("cursor-ask:gpt-5.5", None)
                .unwrap()
                .name(),
            "cursor"
        );
    }

    #[test]
    fn grok_4_7_routes_to_grok() {
        let registry = Registry::new(AliasProvider::Codex);
        assert_eq!(
            registry
                .provider_for_model("grok-4.7", None)
                .unwrap()
                .name(),
            "grok"
        );
    }

    #[test]
    fn gpt_6_models_route_to_codex() {
        let registry = Registry::new(AliasProvider::Codex);
        for model in [
            "gpt-6-sol",
            "gpt-6-sol-fast",
            "gpt-6-luna",
            "gpt-6.1-sol",
            "gpt-6.1-sol-fast",
        ] {
            assert_eq!(
                registry.provider_for_model(model, None).unwrap().name(),
                "codex"
            );
        }
    }
}
