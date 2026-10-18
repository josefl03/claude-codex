//! Claude Code subagent cards for every non-Claude model:
//! `claude-codex agents <sync|list|clean>` and the background sync run by `serve`.
//!
//! Claude Code treats a Markdown file in `~/.claude/agents/` with a `model:`
//! frontmatter as a subagent and sends that model id as-is, which the proxy
//! already routes by name. Cards are generated from the registry, so a model
//! that appears or disappears is added or removed without a release.
//!
//! A card is `<sanitized model>.md`. Only `*.md` files that carry the managed
//! marker (see [`is_managed`]) are ever rewritten or deleted; anything else in
//! the directory is left alone, and a model whose card name is taken by an
//! unmarked file or is a reserved name is skipped.
//! Content generation and diffing are pure; `CCP_AGENTS_DIR` redirects the
//! directory (used by tests).

use crate::latest::LatestAlias;
use crate::logging::create_logger;
use crate::registry::{ANTHROPIC_STYLE_ALIASES, Registry};
use anyhow::{Context, Result};
use clap::{Args, Subcommand};
use serde_json::{Map, json};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

/// Overrides the agents directory (default `~/.claude/agents`).
pub const AGENTS_DIR_ENV: &str = "CCP_AGENTS_DIR";
/// `0` disables the background sync inside `serve`.
pub const AGENT_CARDS_ENV: &str = "CCP_AGENT_CARDS";
/// Seconds between background syncs; `0` syncs only at startup. The default
/// stays below the model refresh interval (`model_discovery`), and a refresh
/// that changes the model list triggers a sync right away anyway.
pub const SYNC_INTERVAL_ENV: &str = "CCP_AGENT_SYNC_INTERVAL_SECS";
const DEFAULT_SYNC_INTERVAL_SECS: u64 = 300;
/// Comma-separated providers that get cards (default `codex`). Managed cards of
/// any other provider are removed by the next sync.
pub const CARD_PROVIDERS_ENV: &str = "CCP_AGENT_CARD_PROVIDERS";
const DEFAULT_CARD_PROVIDERS: &str = "codex";

/// Names that are never generated: Claude Code's built-in agents and anything
/// that could shadow a Claude model (`claude-*` and the model aliases).
pub const RESERVED_NAMES: [&str; 5] = [
    "general-purpose",
    "explore",
    "plan",
    "statusline-setup",
    "claude-code-guide",
];
/// Cards bigger than this are never read as ours.
const MAX_CARD_BYTES: u64 = 256 * 1024;
pub const MANAGED_MARKER: &str = "<!-- managed-by: claude-codex; do not edit -->";
/// Marker written by earlier versions (Spanish text). Still recognized as ours,
/// so a sync rewrites those cards with the current marker.
pub const LEGACY_MANAGED_MARKER: &str = "<!-- managed-by: claude-codex; no editar -->";

#[derive(Debug, Clone, Copy, Subcommand)]
pub enum AgentsCommand {
    /// Create, update and remove cards so they match the available models
    Sync(AgentsArgs),
    /// List the cards managed by claude-codex
    List,
    /// Remove every card managed by claude-codex
    Clean(AgentsArgs),
}

#[derive(Debug, Clone, Copy, Args)]
pub struct AgentsArgs {
    /// Show what would change without writing anything
    #[arg(long)]
    pub dry_run: bool,
}

/// A model that gets a card.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CardModel {
    pub model: String,
    pub provider: String,
    /// Set for a `<family>-latest` alias card: `model` is then the alias itself
    /// (it never changes) and the card text names the current target.
    pub latest: Option<LatestAlias>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    Create {
        file: String,
        content: String,
    },
    Update {
        file: String,
        content: String,
    },
    Remove {
        file: String,
    },
    /// A desired card collides with a file we do not manage; it is left alone.
    Conflict {
        file: String,
    },
    /// The model's card name is reserved; no card is written.
    Reserved {
        file: String,
    },
}

impl Action {
    pub fn file(&self) -> &str {
        match self {
            Action::Create { file, .. }
            | Action::Update { file, .. }
            | Action::Remove { file }
            | Action::Conflict { file }
            | Action::Reserved { file } => file,
        }
    }

    fn verb(&self) -> &'static str {
        match self {
            Action::Create { .. } => "create",
            Action::Update { .. } => "update",
            Action::Remove { .. } => "remove",
            Action::Conflict { .. } => "skip (unmanaged file exists)",
            Action::Reserved { .. } => "skip (reserved name)",
        }
    }
}

pub fn agents_dir() -> Result<PathBuf> {
    if let Some(dir) = std::env::var_os(AGENTS_DIR_ENV) {
        return Ok(PathBuf::from(dir));
    }
    let home = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .context("cannot locate the home directory; set CCP_AGENTS_DIR")?;
    Ok(home.join(".claude").join("agents"))
}

/// Lowercase, `[a-z0-9-]` only, no repeated or edge dashes.
pub fn sanitize(model: &str) -> String {
    let mut out = String::new();
    for c in model.chars().flat_map(char::to_lowercase) {
        if c.is_ascii_alphanumeric() {
            out.push(c);
        } else if !out.ends_with('-') {
            out.push('-');
        }
    }
    out.trim_matches('-').to_string()
}

/// Card name, equal to the file stem: the sanitized model id.
pub fn card_name(model: &str) -> String {
    sanitize(model)
}

/// True for a name a card must never take (see [`RESERVED_NAMES`]).
pub fn is_reserved_name(name: &str) -> bool {
    name.starts_with("claude-")
        || RESERVED_NAMES.contains(&name)
        || ANTHROPIC_STYLE_ALIASES.contains(&name)
}

pub fn card_file(model: &str) -> String {
    format!("{}.md", card_name(model))
}

pub fn render_card(card: &CardModel) -> String {
    let name = card_name(&card.model);
    if let Some(alias) = &card.latest {
        let description = format!(
            "Always the {scope}, currently {target} (provider: {provider}), through the \
             claude-codex proxy. Delegate a self-contained task to it to get that model's answer.",
            scope = alias.scope(),
            target = alias.target,
            provider = alias.provider,
        );
        return format!(
            "---\nname: {name}\ndescription: {description_json}\nmodel: {model}\n---\n\
             {MANAGED_MARKER}\n\n\
             You are a subagent running on `{model}`, which the claude-codex proxy resolves on \
             every request to the {scope} (currently `{target}`, {provider}). \
             Complete the task you are given and reply with the result only.\n",
            description_json = json!(description),
            model = card.model,
            scope = alias.scope(),
            target = alias.target,
            provider = alias.provider,
        );
    }
    let description = format!(
        "Subagent that runs on {model} (provider: {provider}) through the claude-codex proxy. \
         Delegate a self-contained task to it to get that model's answer.",
        model = card.model,
        provider = card.provider,
    );
    format!(
        "---\nname: {name}\ndescription: {description_json}\nmodel: {model}\n---\n\
         {MANAGED_MARKER}\n\n\
         You are a subagent running on the model `{model}` ({provider}). \
         Complete the task you are given and reply with the result only.\n",
        description_json = json!(description),
        model = card.model,
        provider = card.provider,
    )
}

/// Cards for the `<family>-latest` aliases (see [`crate::latest`]). The card
/// name and `model:` are the alias, so they never change; only the text that
/// names the current target does.
pub fn alias_cards(aliases: &[LatestAlias]) -> Vec<CardModel> {
    aliases
        .iter()
        .map(|alias| CardModel {
            model: alias.alias.clone(),
            provider: alias.provider.clone(),
            latest: Some(alias.clone()),
        })
        .collect()
}

/// Providers whose models get cards, from `CCP_AGENT_CARD_PROVIDERS`.
pub fn card_providers() -> Vec<String> {
    parse_providers(std::env::var(CARD_PROVIDERS_ENV).ok().as_deref())
}

fn parse_providers(raw: Option<&str>) -> Vec<String> {
    let list: Vec<String> = raw
        .unwrap_or(DEFAULT_CARD_PROVIDERS)
        .split(',')
        .map(|p| p.trim().to_ascii_lowercase())
        .filter(|p| !p.is_empty())
        .collect();
    if list.is_empty() {
        vec![DEFAULT_CARD_PROVIDERS.to_string()]
    } else {
        list
    }
}

/// Models that get a card: the models of the given providers that are not Claude (Claude Code already
/// knows those). A `-fast` entry is a priority-tier alias of its base model
/// and is skipped when the base is listed too.
pub fn eligible_models(all: &[(String, String)], providers: &[String]) -> Vec<CardModel> {
    let present: BTreeSet<&str> = all.iter().map(|(m, _)| m.as_str()).collect();
    let mut out: Vec<CardModel> = all
        .iter()
        .filter(|(model, provider)| {
            provider != "anthropic"
                && providers.iter().any(|p| p == provider)
                && !model.starts_with("claude-")
                && !ANTHROPIC_STYLE_ALIASES.contains(&model.as_str())
                && !sanitize(model).is_empty()
                && !model
                    .strip_suffix("-fast")
                    .is_some_and(|base| present.contains(base))
        })
        .map(|(model, provider)| CardModel {
            model: model.clone(),
            provider: provider.clone(),
            latest: None,
        })
        .collect();
    out.sort_by(|a, b| a.model.cmp(&b.model));
    out
}

/// True when `content` is a card written by claude-codex: a `.md` file whose
/// body (the first non-blank line after the frontmatter) is the current or the
/// legacy managed marker. The file name does not matter.
pub fn is_managed(file: &str, content: &str) -> bool {
    if !file.ends_with(".md") {
        return false;
    }
    let mut lines = content.lines();
    if lines.next().map(str::trim) != Some("---") {
        return false;
    }
    if !lines.any(|line| line.trim() == "---") {
        return false;
    }
    lines
        .find(|line| !line.trim().is_empty())
        .map(str::trim)
        .is_some_and(|line| line == MANAGED_MARKER || line == LEGACY_MANAGED_MARKER)
}

/// Diff between the cards we want and what is on disk.
/// `managed` maps file name to content for files we own; `foreign` names the
/// other `*.md` files, which are never touched.
pub fn plan(
    models: &[CardModel],
    managed: &BTreeMap<String, String>,
    foreign: &BTreeSet<String>,
) -> Vec<Action> {
    let mut desired: BTreeMap<String, String> = BTreeMap::new();
    let mut actions = Vec::new();
    for card in models {
        let name = card_name(&card.model);
        let file = card_file(&card.model);
        if is_reserved_name(&name) {
            if !actions.contains(&Action::Reserved { file: file.clone() }) {
                actions.push(Action::Reserved { file });
            }
            continue;
        }
        // Two models can sanitize to the same name; the first one wins.
        desired.entry(file).or_insert_with(|| render_card(card));
    }
    for (file, content) in &desired {
        if foreign.contains(file) {
            actions.push(Action::Conflict { file: file.clone() });
        } else {
            match managed.get(file) {
                None => actions.push(Action::Create {
                    file: file.clone(),
                    content: content.clone(),
                }),
                Some(existing) if existing != content => actions.push(Action::Update {
                    file: file.clone(),
                    content: content.clone(),
                }),
                Some(_) => {}
            }
        }
    }
    for file in managed.keys() {
        if !desired.contains_key(file) {
            actions.push(Action::Remove { file: file.clone() });
        }
    }
    actions
}

/// Scan `dir`: `(managed, foreign)` as expected by [`plan`]. A missing
/// directory is empty.
pub fn scan(dir: &Path) -> Result<(BTreeMap<String, String>, BTreeSet<String>)> {
    let mut managed = BTreeMap::new();
    let mut foreign = BTreeSet::new();
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            return Ok((managed, foreign));
        }
        Err(err) => return Err(err).with_context(|| format!("reading {}", dir.display())),
    };
    for entry in entries {
        let entry = entry?;
        let Some(file) = entry.file_name().to_str().map(str::to_string) else {
            continue;
        };
        // Symlinks and directories are never ours.
        let Ok(meta) = entry.path().symlink_metadata() else {
            continue;
        };
        if !file.ends_with(".md") || !meta.is_file() {
            continue;
        }
        let content = if meta.len() <= MAX_CARD_BYTES {
            std::fs::read_to_string(entry.path()).ok()
        } else {
            None
        };
        match content {
            Some(content) if is_managed(&file, &content) => {
                managed.insert(file, content);
            }
            _ => {
                foreign.insert(file);
            }
        }
    }
    Ok((managed, foreign))
}

fn write_atomic(path: &Path, content: &str) -> Result<()> {
    let dir = path.parent().context("card path has no parent")?;
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let tmp = dir.join(format!(
        ".{}.tmp-{}",
        path.file_name().and_then(|n| n.to_str()).unwrap_or("card"),
        std::process::id()
    ));
    std::fs::write(&tmp, content).with_context(|| format!("writing {}", tmp.display()))?;
    std::fs::rename(&tmp, path).inspect_err(|_| {
        let _ = std::fs::remove_file(&tmp);
    })?;
    Ok(())
}

/// Apply `actions` to `dir`. Re-checks the marker before removing, so a file
/// the user took over since the scan survives.
pub fn apply(dir: &Path, actions: &[Action]) -> Result<()> {
    for action in actions {
        let path = dir.join(action.file());
        match action {
            Action::Create { content, .. } | Action::Update { content, .. } => {
                // Never overwrite a file that is not ours, even if it appeared
                // after the scan.
                let taken = match std::fs::read_to_string(&path) {
                    Ok(existing) => !is_managed(action.file(), &existing),
                    Err(err) => err.kind() != std::io::ErrorKind::NotFound,
                };
                if !taken {
                    write_atomic(&path, content)?
                }
            }
            Action::Remove { file } => {
                let is_plain_file = path.symlink_metadata().is_ok_and(|m| m.is_file());
                if is_plain_file
                    && std::fs::read_to_string(&path).is_ok_and(|c| is_managed(file, &c))
                {
                    std::fs::remove_file(&path)
                        .with_context(|| format!("removing {}", path.display()))?;
                }
            }
            Action::Conflict { .. } | Action::Reserved { .. } => {}
        }
    }
    Ok(())
}

/// One sync pass: refresh the registry, diff, and apply unless `dry_run`.
pub fn sync_once(registry: &Registry, dir: &Path, dry_run: bool) -> Result<Vec<Action>> {
    registry.refresh();
    let mut models = eligible_models(&registry.all_supported_models(), &card_providers());
    models.extend(alias_cards(&registry.latest_aliases()));
    models.sort_by(|a, b| a.model.cmp(&b.model));
    let (managed, foreign) = scan(dir)?;
    let actions = plan(&models, &managed, &foreign);
    if !dry_run {
        apply(dir, &actions)?;
    }
    Ok(actions)
}

pub fn clean_once(dir: &Path, dry_run: bool) -> Result<Vec<Action>> {
    let (managed, _) = scan(dir)?;
    let actions: Vec<Action> = managed
        .into_keys()
        .map(|file| Action::Remove { file })
        .collect();
    if !dry_run {
        apply(dir, &actions)?;
    }
    Ok(actions)
}

fn print_actions(actions: &[Action], dry_run: bool) {
    let prefix = if dry_run { "would " } else { "" };
    for action in actions {
        println!("{prefix}{}: {}", action.verb(), action.file());
    }
    if actions.is_empty() {
        println!("agent cards already up to date");
    }
}

pub fn run_agents_command(command: AgentsCommand) -> Result<()> {
    let dir = agents_dir()?;
    match command {
        AgentsCommand::Sync(args) => {
            // Learn about new models first so their cards are created now.
            // A dry run must not write the model cache.
            if !args.dry_run {
                crate::model_discovery::refresh_before_sync();
            }
            let registry = Registry::with_default_alias();
            let actions = sync_once(&registry, &dir, args.dry_run)?;
            println!("agents dir: {}", dir.display());
            print_actions(&actions, args.dry_run);
            if crate::lifecycle::is_disabled() {
                println!(
                    "note: claude-codex is disabled; `serve` will not keep these cards in sync. Run `claude-codex enable` to switch it back on"
                );
            }
        }
        AgentsCommand::List => {
            let (managed, _) = scan(&dir)?;
            println!("agents dir: {}", dir.display());
            for file in managed.keys() {
                println!("{file}");
            }
            if managed.is_empty() {
                println!("no managed agent cards");
            }
        }
        AgentsCommand::Clean(args) => {
            let actions = clean_once(&dir, args.dry_run)?;
            println!("agents dir: {}", dir.display());
            print_actions(&actions, args.dry_run);
        }
    }
    Ok(())
}

pub fn cards_enabled() -> bool {
    !matches!(
        std::env::var(AGENT_CARDS_ENV)
            .ok()
            .as_deref()
            .map(str::trim),
        Some("0" | "false" | "off" | "no")
    )
}

pub fn sync_interval() -> Option<Duration> {
    let secs = std::env::var(SYNC_INTERVAL_ENV)
        .ok()
        .and_then(|raw| raw.trim().parse::<u64>().ok())
        .unwrap_or(DEFAULT_SYNC_INTERVAL_SECS);
    (secs > 0).then(|| Duration::from_secs(secs))
}

/// Warn about a skipped card, once per file for the life of the process so the
/// periodic sync does not repeat it.
fn warn_skipped_once(log: &crate::logging::Logger, actions: &[Action]) {
    static WARNED: std::sync::Mutex<BTreeSet<String>> = std::sync::Mutex::new(BTreeSet::new());
    for action in actions {
        let reason = match action {
            Action::Conflict { .. } => {
                "a file with that name exists and is not managed by claude-codex"
            }
            Action::Reserved { .. } => "the name is reserved",
            _ => continue,
        };
        let newly = WARNED
            .lock()
            .map(|mut set| set.insert(action.file().to_string()))
            .unwrap_or(false);
        if newly {
            log.warn(
                "agent card skipped",
                Some(Map::from_iter([
                    ("file".to_string(), json!(action.file())),
                    ("reason".to_string(), json!(reason)),
                ])),
            );
        }
    }
}

/// One background sync pass: runs the blocking sync off the async runtime,
/// logs the outcome, and returns the actions applied (empty on failure).
/// The caller refreshes the model lists first (see `model_discovery`).
pub async fn sync_logged(registry: Arc<Registry>) -> Vec<Action> {
    let log = create_logger("agent-cards");
    // `claude-codex disable` leaves a marker so that a `serve` started by hand
    // does not bring the cards back.
    if crate::lifecycle::is_disabled() {
        log.debug("agent card sync skipped: claude-codex is disabled", None);
        return Vec::new();
    }
    let result = tokio::task::spawn_blocking(move || {
        let dir = agents_dir()?;
        sync_once(&registry, &dir, false)
    })
    .await;
    match result {
        Ok(Ok(actions)) => {
            let changed: Vec<&str> = actions
                .iter()
                .filter(|a| !matches!(a, Action::Conflict { .. } | Action::Reserved { .. }))
                .map(Action::file)
                .collect();
            warn_skipped_once(&log, &actions);
            if !changed.is_empty() {
                log.info(
                    "agent cards synced",
                    Some(Map::from_iter([("changed".to_string(), json!(changed))])),
                );
            }
            actions
        }
        Ok(Err(err)) => {
            log.warn(
                "agent card sync failed",
                Some(Map::from_iter([(
                    "error".to_string(),
                    json!(format!("{err:#}")),
                )])),
            );
            Vec::new()
        }
        Err(err) => {
            log.warn(
                "agent card sync panicked",
                Some(Map::from_iter([(
                    "error".to_string(),
                    json!(err.to_string()),
                )])),
            );
            Vec::new()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn card(model: &str, provider: &str) -> CardModel {
        CardModel {
            model: model.into(),
            provider: provider.into(),
            latest: None,
        }
    }

    fn pair(model: &str, provider: &str) -> (String, String) {
        (model.into(), provider.into())
    }

    #[test]
    fn names_are_lowercase_dash_only() {
        assert_eq!(card_name("gpt-6-luna"), "gpt-6-luna");
        assert_eq!(card_name("gpt-5.6-Terra"), "gpt-5-6-terra");
        assert_eq!(card_name("cursor:gpt-5.5"), "cursor-gpt-5-5");
        assert_eq!(card_file("k2.6"), "k2-6.md");
    }

    #[test]
    fn card_has_frontmatter_marker_and_no_tools() {
        let text = render_card(&card("gpt-6-luna", "codex"));
        assert!(text.starts_with("---\nname: gpt-6-luna\ndescription: \""));
        assert!(text.contains("\nmodel: gpt-6-luna\n---\n"));
        assert!(text.contains("gpt-6-luna (provider: codex)"));
        assert!(!text.contains("tools:"));
        assert!(is_managed("gpt-6-luna.md", &text));
        assert!(is_managed("whatever.md", &text), "the name does not matter");
        assert!(!is_managed("gpt-6-luna.txt", &text));
    }

    #[test]
    fn legacy_marker_is_still_managed_and_sync_rewrites_it() {
        assert!(MANAGED_MARKER.contains("managed-by: claude-codex"));
        assert!(MANAGED_MARKER.ends_with("do not edit -->"));
        let luna = card("gpt-6-luna", "codex");
        let old = render_card(&luna).replace(MANAGED_MARKER, LEGACY_MANAGED_MARKER);
        assert!(old.contains("no editar"));
        assert!(is_managed("gpt-6-luna.md", &old));
        assert!(!is_managed("gpt-6-luna.md", "<!-- managed-by: other -->"));

        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("gpt-6-luna.md"), &old).unwrap();
        let (managed, foreign) = scan(dir.path()).unwrap();
        assert_eq!(managed.len(), 1);
        assert!(foreign.is_empty());
        let actions = plan(std::slice::from_ref(&luna), &managed, &foreign);
        assert_eq!(
            actions,
            vec![Action::Update {
                file: "gpt-6-luna.md".into(),
                content: render_card(&luna)
            }]
        );
        apply(dir.path(), &actions).unwrap();
        let text = std::fs::read_to_string(dir.path().join("gpt-6-luna.md")).unwrap();
        assert!(text.contains(MANAGED_MARKER) && !text.contains("no editar"));
    }

    #[test]
    fn claude_models_and_fast_aliases_get_no_card() {
        let all = vec![
            pair("gpt-6-luna", "codex"),
            pair("gpt-6-luna-fast", "codex"),
            pair("grok-composer-2.5-fast", "grok"),
            pair("haiku", "anthropic"),
            pair("claude-opus-5", "anthropic"),
            pair("claude-sonnet-5", "codex"),
            pair("opus", "codex"),
            pair("k3", "kimi"),
        ];
        let both = vec!["codex".to_string(), "grok".to_string(), "kimi".to_string()];
        let models: Vec<String> = eligible_models(&all, &both)
            .into_iter()
            .map(|c| c.model)
            .collect();
        assert_eq!(models, vec!["gpt-6-luna", "grok-composer-2.5-fast", "k3"]);
        // Default: only the Codex provider.
        let models: Vec<String> = eligible_models(&all, &parse_providers(None))
            .into_iter()
            .map(|c| c.model)
            .collect();
        assert_eq!(models, vec!["gpt-6-luna"]);
    }

    #[test]
    fn provider_list_parsing() {
        assert_eq!(parse_providers(None), ["codex"]);
        assert_eq!(parse_providers(Some("")), ["codex"]);
        assert_eq!(parse_providers(Some(" , ")), ["codex"]);
        assert_eq!(
            parse_providers(Some("Codex, kimi ,grok")),
            ["codex", "kimi", "grok"]
        );
    }

    #[test]
    fn plan_creates_updates_removes_and_spares_foreign_files() {
        let luna = card("gpt-6-luna", "codex");
        let sol = card("gpt-6-sol", "codex");
        let stale = render_card(&card("gpt-6-sol", "other"));
        let managed = BTreeMap::from([
            ("gpt-6-sol.md".to_string(), stale),
            ("gone.md".to_string(), render_card(&card("gone", "codex"))),
        ]);
        let foreign = BTreeSet::from(["foo.md".to_string()]);
        let actions = plan(&[luna.clone(), sol.clone()], &managed, &foreign);
        assert_eq!(
            actions,
            vec![
                Action::Create {
                    file: "gpt-6-luna.md".into(),
                    content: render_card(&luna)
                },
                Action::Update {
                    file: "gpt-6-sol.md".into(),
                    content: render_card(&sol)
                },
                Action::Remove {
                    file: "gone.md".into()
                },
            ]
        );
        // A model whose card name collides with an unmanaged file is skipped.
        let actions = plan(&[card("foo", "codex")], &BTreeMap::new(), &foreign);
        assert_eq!(
            actions,
            vec![Action::Conflict {
                file: "foo.md".into()
            }]
        );
    }

    #[test]
    fn apply_roundtrip_never_touches_unmanaged_files() {
        let dir = tempfile::tempdir().unwrap();
        let foreign = dir.path().join("foo.md");
        std::fs::write(&foreign, "mine, no marker").unwrap();
        let other = dir.path().join("luna.md");
        std::fs::write(&other, "also mine").unwrap();
        let notes = dir.path().join("notes.txt");
        std::fs::write(&notes, "not markdown").unwrap();

        let (managed, foreign_set) = scan(dir.path()).unwrap();
        assert!(managed.is_empty());
        assert_eq!(
            foreign_set,
            BTreeSet::from(["foo.md".to_string(), "luna.md".to_string()])
        );

        let models = [card("gpt-6-luna", "codex"), card("gpt-6-sol", "codex")];
        let actions = plan(&models, &managed, &foreign_set);
        apply(dir.path(), &actions).unwrap();
        assert!(dir.path().join("gpt-6-luna.md").exists());

        // Second pass is a no-op; dropping a model removes only its card.
        let (managed, foreign_set) = scan(dir.path()).unwrap();
        assert!(plan(&models, &managed, &foreign_set).is_empty());
        let actions = plan(&models[..1], &managed, &foreign_set);
        apply(dir.path(), &actions).unwrap();
        assert!(!dir.path().join("gpt-6-sol.md").exists());
        assert!(dir.path().join("gpt-6-luna.md").exists());

        let cleaned = clean_once(dir.path(), false).unwrap();
        assert_eq!(cleaned.len(), 1);
        assert_eq!(
            std::fs::read_to_string(&foreign).unwrap(),
            "mine, no marker"
        );
        assert_eq!(std::fs::read_to_string(&other).unwrap(), "also mine");
        assert!(notes.exists());
        // No temp files left behind.
        let leftovers = std::fs::read_dir(dir.path()).unwrap().count();
        assert_eq!(leftovers, 3);
    }

    #[test]
    fn dry_run_writes_nothing_and_missing_dir_is_empty() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("agents");
        let registry = Registry::new(crate::config::AliasProvider::Codex);
        let actions = sync_once(&registry, &target, true).unwrap();
        assert!(actions.iter().any(|a| a.file() == "gpt-6-luna.md"));
        assert!(!target.exists());
        sync_once(&registry, &target, false).unwrap();
        assert!(target.join("gpt-6-luna.md").exists());
        assert!(!target.join("haiku.md").exists());
    }

    #[test]
    fn old_prefixed_cards_migrate_to_the_new_names() {
        let luna = card("gpt-6-luna", "codex");
        let new_text = render_card(&luna);
        // What earlier versions wrote: prefixed name, current and Spanish marker.
        let old_current = new_text.replace("name: gpt-6-luna", "name: ccp-gpt-6-luna");
        let old_spanish = old_current.replace(MANAGED_MARKER, LEGACY_MANAGED_MARKER);
        assert!(old_spanish.contains("no editar"));

        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("ccp-gpt-6-luna.md"), &old_spanish).unwrap();
        std::fs::write(dir.path().join("ccp-gpt-6-sol.md"), &old_current).unwrap();
        std::fs::write(dir.path().join("ccp-mine.md"), "no marker, mine").unwrap();

        let (managed, foreign) = scan(dir.path()).unwrap();
        assert_eq!(managed.len(), 2);
        assert_eq!(foreign, BTreeSet::from(["ccp-mine.md".to_string()]));
        let models = [luna.clone()];
        let actions = plan(&models, &managed, &foreign);
        assert_eq!(
            actions,
            vec![
                Action::Create {
                    file: "gpt-6-luna.md".into(),
                    content: new_text.clone()
                },
                Action::Remove {
                    file: "ccp-gpt-6-luna.md".into()
                },
                Action::Remove {
                    file: "ccp-gpt-6-sol.md".into()
                },
            ]
        );
        apply(dir.path(), &actions).unwrap();
        let mut files: Vec<String> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        files.sort();
        assert_eq!(files, ["ccp-mine.md", "gpt-6-luna.md"]);
        assert_eq!(
            std::fs::read_to_string(dir.path().join("gpt-6-luna.md")).unwrap(),
            new_text
        );
        assert_eq!(
            std::fs::read_to_string(dir.path().join("ccp-mine.md")).unwrap(),
            "no marker, mine"
        );
        let (managed, foreign) = scan(dir.path()).unwrap();
        assert!(plan(&models, &managed, &foreign).is_empty());
    }

    #[test]
    fn an_unmarked_file_with_the_wanted_name_is_never_overwritten() {
        let dir = tempfile::tempdir().unwrap();
        let mine = dir.path().join("gpt-6-luna.md");
        std::fs::write(&mine, "my own agent").unwrap();
        let models = [card("gpt-6-luna", "codex")];
        let (managed, foreign) = scan(dir.path()).unwrap();
        let actions = plan(&models, &managed, &foreign);
        assert_eq!(
            actions,
            vec![Action::Conflict {
                file: "gpt-6-luna.md".into()
            }]
        );
        apply(dir.path(), &actions).unwrap();
        // Even a stale Create (file appeared after the scan) leaves it alone.
        let stale = [Action::Create {
            file: "gpt-6-luna.md".into(),
            content: "x".into(),
        }];
        apply(dir.path(), &stale).unwrap();
        assert_eq!(std::fs::read_to_string(&mine).unwrap(), "my own agent");
    }

    #[test]
    fn reserved_names_never_get_a_card() {
        for name in [
            "general-purpose",
            "Explore",
            "plan",
            "statusline-setup",
            "claude-code-guide",
            "claude.future-model",
            "sonnet",
        ] {
            assert!(is_reserved_name(&sanitize(name)), "{name}");
            let actions = plan(&[card(name, "codex")], &BTreeMap::new(), &BTreeSet::new());
            assert_eq!(
                actions,
                vec![Action::Reserved {
                    file: card_file(name)
                }],
                "{name}"
            );
        }
        assert!(!is_reserved_name("gpt-6-luna"));
        assert!(!is_reserved_name("planner"));
    }

    fn alias(name: &str, tier: Option<&str>, target: &str) -> LatestAlias {
        LatestAlias {
            alias: name.into(),
            family: "gpt".into(),
            tier: tier.map(str::to_string),
            target: target.into(),
            provider: "codex".into(),
        }
    }

    #[test]
    fn alias_card_names_the_alias_and_describes_the_current_target() {
        let sol = alias_cards(&[alias("gpt-sol-latest", Some("sol"), "gpt-6.1-sol")]);
        let text = render_card(&sol[0]);
        assert!(text.starts_with("---\nname: gpt-sol-latest\ndescription: \""));
        assert!(text.contains(
            "Always the newest sol model of the gpt family, currently gpt-6.1-sol (provider: codex)"
        ));
        assert!(text.contains("\nmodel: gpt-sol-latest\n---\n"));
        assert!(is_managed("gpt-sol-latest.md", &text));

        let any = alias_cards(&[alias("gpt-latest", None, "gpt-6.1-sol")]);
        assert!(
            render_card(&any[0]).contains("Always the newest model of the gpt family, currently")
        );

        // A new target changes the text but never the name or the model line.
        let newer = alias_cards(&[alias("gpt-sol-latest", Some("sol"), "gpt-7-sol")]);
        let newer_text = render_card(&newer[0]);
        assert_ne!(text, newer_text);
        assert!(newer_text.contains("currently gpt-7-sol"));
        assert_eq!(
            text.lines()
                .take(1)
                .chain(text.lines().skip(3).take(1))
                .collect::<Vec<_>>(),
            newer_text
                .lines()
                .take(1)
                .chain(newer_text.lines().skip(3).take(1))
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn alias_cards_follow_the_same_plan_rules() {
        let old = alias_cards(&[alias("gpt-sol-latest", Some("sol"), "gpt-6-sol")]);
        let managed = BTreeMap::from([("gpt-sol-latest.md".to_string(), render_card(&old[0]))]);
        let none = BTreeSet::new();

        // Target moved: an update of the same file.
        let moved = alias_cards(&[alias("gpt-sol-latest", Some("sol"), "gpt-7-sol")]);
        assert_eq!(
            plan(&moved, &managed, &none),
            vec![Action::Update {
                file: "gpt-sol-latest.md".into(),
                content: render_card(&moved[0])
            }]
        );
        // No candidates left: the alias is not wanted and its card is retired.
        assert_eq!(
            plan(&[], &managed, &none),
            vec![Action::Remove {
                file: "gpt-sol-latest.md".into()
            }]
        );
        // An unmanaged file with the alias name is never overwritten.
        let foreign = BTreeSet::from(["gpt-sol-latest.md".to_string()]);
        assert_eq!(
            plan(&old, &BTreeMap::new(), &foreign),
            vec![Action::Conflict {
                file: "gpt-sol-latest.md".into()
            }]
        );
        // Aliases are not reserved names.
        assert!(!is_reserved_name("gpt-sol-latest"));
    }

    #[test]
    fn the_marker_must_be_the_body_after_the_frontmatter() {
        let marker = MANAGED_MARKER;
        // Quoted inside a user's file, in the frontmatter, or without frontmatter.
        assert!(!is_managed(
            "a.md",
            &format!(
                "# notes
{marker}
"
            )
        ));
        assert!(!is_managed(
            "a.md",
            &format!(
                "---
name: a
{marker}
---
body
"
            )
        ));
        assert!(!is_managed(
            "a.md",
            &format!(
                "---
name: a
---
body
{marker}
"
            )
        ));
        assert!(!is_managed(
            "a.md",
            &format!(
                "---
name: a
{marker}
"
            )
        ));
        assert!(is_managed(
            "a.md",
            &format!(
                "---
name: a
---

{marker}
body
"
            )
        ));
    }
}
