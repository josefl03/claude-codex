//! One-shot, idempotent setup: `claude-codex setup`.
//!
//! `cargo install` cannot run post-install hooks, so this subcommand does what
//! an installer would: install and start the autostart service, sync the
//! subagent cards once, and point Claude Code at the proxy by merging
//! `ANTHROPIC_BASE_URL` into the user's `settings.json`. What changed is
//! recorded in `<state dir>/setup.json` so `setup --undo` reverts exactly that.
//!
//! Safety for tests: when `CCP_SETUP_SANDBOX` is set, `setup` refuses to run
//! unless every location it can touch is redirected (see [`SANDBOX_REQUIRED`]).
//! The integration tests always set it through a single helper.

use crate::linger::{self, EnableOutcome, Loginctl};
use crate::{agent_cards, config, paths, registry::Registry, service};
use anyhow::{Context, Result, bail};
use clap::Args;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// When set, `setup` requires every variable in [`SANDBOX_REQUIRED`].
pub const SANDBOX_ENV: &str = "CCP_SETUP_SANDBOX";
/// Variables that must be set (non-empty) in sandbox mode so no real path is touched.
pub const SANDBOX_REQUIRED: [&str; 8] = [
    "HOME",
    "XDG_STATE_HOME",
    "CCP_CONFIG_DIR",
    "CCP_SERVICE_DIR",
    "CCP_AGENTS_DIR",
    "CLAUDE_CONFIG_DIR",
    "CCP_CODEX_AUTH_FILE",
    "CCP_AGENT_CARDS",
];
pub const CLAUDE_CONFIG_DIR_ENV: &str = "CLAUDE_CONFIG_DIR";
pub const BACKUP_SUFFIX: &str = ".claude-codex.bak";
pub const STATE_FILE: &str = "setup.json";

pub const BASE_URL_KEY: &str = "ANTHROPIC_BASE_URL";
pub const DISCOVERY_KEY: &str = "CLAUDE_CODE_ENABLE_GATEWAY_MODEL_DISCOVERY";
/// Credentials must never be written by setup.
const FORBIDDEN_KEYS: [&str; 2] = ["ANTHROPIC_API_KEY", "ANTHROPIC_AUTH_TOKEN"];

#[derive(Debug, Clone, Args)]
pub struct SetupArgs {
    /// Show what would change without changing anything
    #[arg(long)]
    pub dry_run: bool,
    /// Do not install the autostart service
    #[arg(long)]
    pub no_service: bool,
    /// Do not sync the subagent cards
    #[arg(long)]
    pub no_agents: bool,
    /// Do not touch Claude Code's settings.json
    #[arg(long)]
    pub no_claude_settings: bool,
    /// Overwrite an ANTHROPIC_BASE_URL that points somewhere else
    #[arg(long)]
    pub force: bool,
    /// Revert what a previous setup did
    #[arg(long)]
    pub undo: bool,
}

/// What `setup` changed, persisted so `--undo` can revert only that.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct SetupState {
    #[serde(default)]
    pub settings_file: Option<String>,
    /// Keys setup added to `env`, with the value it wrote.
    #[serde(default)]
    pub added: BTreeMap<String, String>,
    /// Keys setup replaced (`--force`), with the previous value.
    #[serde(default)]
    pub replaced: BTreeMap<String, Replaced>,
    /// Setup created the `env` object itself.
    #[serde(default)]
    pub env_created: bool,
    #[serde(default)]
    pub backup_file: Option<String>,
    /// `settings.json` did not exist and setup created it.
    #[serde(default)]
    pub settings_created: bool,
    #[serde(default)]
    pub service_installed: bool,
    #[serde(default)]
    pub agents_synced: bool,
    /// Setup turned systemd linger on (Linux), so `disable`/`uninstall` turn it
    /// off again. Absent in records written by older versions.
    #[serde(default)]
    pub linger_enabled_by_setup: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Replaced {
    pub previous: Value,
    pub value: String,
}

#[derive(Debug, Clone, PartialEq)]
pub enum EnvChange {
    Added {
        key: String,
        value: String,
    },
    Replaced {
        key: String,
        previous: Value,
        value: String,
    },
    AlreadySet {
        key: String,
    },
    Conflict {
        key: String,
        existing: Value,
    },
}

#[derive(Debug, Default)]
pub struct MergeResult {
    pub changes: Vec<EnvChange>,
    pub env_created: bool,
}

impl MergeResult {
    pub fn modified(&self) -> bool {
        self.changes
            .iter()
            .any(|c| matches!(c, EnvChange::Added { .. } | EnvChange::Replaced { .. }))
    }
    pub fn base_url_conflict(&self) -> Option<&Value> {
        self.changes.iter().find_map(|c| match c {
            EnvChange::Conflict { key, existing } if key == BASE_URL_KEY => Some(existing),
            _ => None,
        })
    }
}

/// The env entries Claude Code needs to use this proxy.
pub fn wanted_env(port: u16) -> Vec<(&'static str, String)> {
    vec![
        (BASE_URL_KEY, format!("http://127.0.0.1:{port}")),
        (DISCOVERY_KEY, "1".to_string()),
    ]
}

fn normalize_url(url: &str) -> String {
    url.trim().trim_end_matches('/').to_ascii_lowercase()
}

/// Effective port of an `http` URL that points at this machine's loopback
/// interface: host `localhost`, `127.0.0.1` or `[::1]`, no credentials, no path
/// (a single trailing slash is fine), query or fragment. Anything else is `None`.
fn loopback_http_port(text: &str) -> Option<u16> {
    let url = url::Url::parse(text.trim()).ok()?;
    if url.scheme() != "http"
        || !url.username().is_empty()
        || url.password().is_some()
        || !matches!(url.path(), "" | "/")
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return None;
    }
    let loopback = match url.host()? {
        url::Host::Domain(name) => name.eq_ignore_ascii_case("localhost"),
        url::Host::Ipv4(ip) => ip == std::net::Ipv4Addr::LOCALHOST,
        url::Host::Ipv6(ip) => ip == std::net::Ipv6Addr::LOCALHOST,
    };
    if loopback {
        url.port_or_known_default()
    } else {
        None
    }
}

/// True when two base URLs reach the same listener: the loopback aliases with
/// the same effective port are one address; anything else must match textually.
pub fn same_base_url(existing: &str, wanted: &str) -> bool {
    match (loopback_http_port(existing), loopback_http_port(wanted)) {
        (Some(a), Some(b)) => a == b,
        _ => normalize_url(existing) == normalize_url(wanted),
    }
}

fn same_value(key: &str, existing: &Value, wanted: &str) -> bool {
    let Some(text) = existing.as_str() else {
        return false;
    };
    match key {
        BASE_URL_KEY => same_base_url(text, wanted),
        DISCOVERY_KEY => matches!(text.trim(), "1" | "true"),
        _ => text == wanted,
    }
}

/// Merge `wanted` into `root["env"]`. Existing keys and their order are kept.
/// A different `ANTHROPIC_BASE_URL` is a conflict: nothing is applied unless
/// `force`. Other conflicting keys are left alone.
pub fn merge_env(root: &mut Value, wanted: &[(&str, String)], force: bool) -> Result<MergeResult> {
    if let Some((key, _)) = wanted.iter().find(|(k, _)| FORBIDDEN_KEYS.contains(k)) {
        bail!("refusing to write credential variable {key}");
    }
    let Some(top) = root.as_object_mut() else {
        bail!("settings.json must contain a JSON object at the top level");
    };
    let env_exists = match top.get("env") {
        None => false,
        Some(Value::Object(_)) => true,
        Some(_) => bail!("settings.json has an \"env\" key that is not an object"),
    };

    let mut result = MergeResult::default();
    let empty = Map::new();
    let current = top.get("env").and_then(Value::as_object).unwrap_or(&empty);
    for (key, value) in wanted {
        result.changes.push(match current.get(*key) {
            None => EnvChange::Added {
                key: key.to_string(),
                value: value.clone(),
            },
            Some(existing) if same_value(key, existing, value) => EnvChange::AlreadySet {
                key: key.to_string(),
            },
            Some(existing) => EnvChange::Conflict {
                key: key.to_string(),
                existing: existing.clone(),
            },
        });
    }

    let blocked = result.base_url_conflict().is_some() && !force;
    if blocked {
        // Keep the report, apply nothing: a different base URL makes the rest pointless.
        result
            .changes
            .retain(|c| !matches!(c, EnvChange::Added { .. }));
        return Ok(result);
    }
    if force {
        for change in &mut result.changes {
            if let EnvChange::Conflict { key, existing } = change {
                let value = wanted
                    .iter()
                    .find(|(k, _)| k == key)
                    .map(|(_, v)| v.clone())
                    .unwrap_or_default();
                *change = EnvChange::Replaced {
                    key: key.clone(),
                    previous: existing.clone(),
                    value,
                };
            }
        }
    }
    if !result.modified() {
        return Ok(result);
    }
    if !env_exists {
        top.insert("env".into(), Value::Object(Map::new()));
        result.env_created = true;
    }
    let env = top
        .get_mut("env")
        .and_then(Value::as_object_mut)
        .expect("env object exists");
    for change in &result.changes {
        match change {
            EnvChange::Added { key, value } | EnvChange::Replaced { key, value, .. } => {
                env.insert(key.clone(), Value::String(value.clone()));
            }
            _ => {}
        }
    }
    Ok(result)
}

/// Fold a merge result into the persisted state (keeps what earlier runs recorded).
pub fn record_merge(state: &mut SetupState, result: &MergeResult) {
    for change in &result.changes {
        match change {
            EnvChange::Added { key, value } => {
                state.added.insert(key.clone(), value.clone());
            }
            EnvChange::Replaced {
                key,
                previous,
                value,
            } => {
                if let Some(added) = state.added.get_mut(key) {
                    *added = value.clone();
                } else if let Some(old) = state.replaced.get_mut(key) {
                    old.value = value.clone();
                } else {
                    state.replaced.insert(
                        key.clone(),
                        Replaced {
                            previous: previous.clone(),
                            value: value.clone(),
                        },
                    );
                }
            }
            _ => {}
        }
    }
    state.env_created |= result.env_created;
}

/// Remove what `state` says setup added, only where the value is still ours.
/// Returns human-readable notes about each key.
pub fn unmerge_env(root: &mut Value, state: &SetupState) -> Result<Vec<String>> {
    let mut notes = Vec::new();
    let Some(top) = root.as_object_mut() else {
        bail!("settings.json must contain a JSON object at the top level");
    };
    let Some(env) = top.get_mut("env").and_then(Value::as_object_mut) else {
        notes.push("no \"env\" object in settings.json; nothing to remove".into());
        return Ok(notes);
    };
    for (key, value) in &state.added {
        if env.get(key).and_then(Value::as_str) == Some(value.as_str()) {
            env.shift_remove(key);
            notes.push(format!("removed {key}"));
        } else {
            notes.push(format!("left {key}: it changed since setup wrote it"));
        }
    }
    for (key, rep) in &state.replaced {
        if env.get(key).and_then(Value::as_str) == Some(rep.value.as_str()) {
            env.insert(key.clone(), rep.previous.clone());
            notes.push(format!("restored previous {key}"));
        } else {
            notes.push(format!("left {key}: it changed since setup wrote it"));
        }
    }
    if env.is_empty() && state.env_created {
        top.shift_remove("env");
    }
    Ok(notes)
}

// ---------- settings.json IO ----------

pub(crate) struct SettingsFile {
    pub(crate) value: Value,
    pub(crate) existed: bool,
    pub(crate) indent: String,
}

fn detect_indent(text: &str) -> String {
    for line in text.lines() {
        let trimmed = line.trim_start_matches([' ', '\t']);
        if trimmed.len() != line.len() && !trimmed.is_empty() {
            return line[..line.len() - trimmed.len()].to_string();
        }
    }
    "  ".to_string()
}

pub(crate) fn read_settings(path: &Path) -> Result<SettingsFile> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            return Ok(SettingsFile {
                value: Value::Object(Map::new()),
                existed: false,
                indent: "  ".into(),
            });
        }
        Err(err) => return Err(err).with_context(|| format!("reading {}", path.display())),
    };
    if text.trim().is_empty() {
        return Ok(SettingsFile {
            value: Value::Object(Map::new()),
            existed: true,
            indent: "  ".into(),
        });
    }
    let value: Value = serde_json::from_str(&text).map_err(|err| {
        anyhow::anyhow!(
            "{} is not valid JSON ({err}). It was left untouched; fix it or rerun with --no-claude-settings",
            path.display()
        )
    })?;
    Ok(SettingsFile {
        value,
        existed: true,
        indent: detect_indent(&text),
    })
}

pub(crate) fn render_json(value: &Value, indent: &str) -> Result<String> {
    use serde::Serialize as _;
    let mut buf = Vec::new();
    let formatter = serde_json::ser::PrettyFormatter::with_indent(indent.as_bytes());
    let mut ser = serde_json::Serializer::with_formatter(&mut buf, formatter);
    value.serialize(&mut ser)?;
    let mut text = String::from_utf8(buf)?;
    text.push('\n');
    Ok(text)
}

pub(crate) fn write_atomic(path: &Path, content: &str) -> Result<()> {
    let dir = path.parent().context("path has no parent directory")?;
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let tmp = dir.join(format!(
        ".{}.tmp-{}",
        path.file_name().and_then(|n| n.to_str()).unwrap_or("file"),
        std::process::id()
    ));
    std::fs::write(&tmp, content).with_context(|| format!("writing {}", tmp.display()))?;
    if let Ok(meta) = std::fs::metadata(path) {
        let _ = std::fs::set_permissions(&tmp, meta.permissions());
    }
    std::fs::rename(&tmp, path).inspect_err(|_| {
        let _ = std::fs::remove_file(&tmp);
    })?;
    Ok(())
}

// ---------- locations and sandbox guard ----------

#[derive(Debug, Clone)]
pub struct Locations {
    pub settings_file: PathBuf,
    pub state_file: PathBuf,
}

/// Fails in sandbox mode when any [`SANDBOX_REQUIRED`] variable is missing.
pub fn check_sandbox(get: impl Fn(&str) -> Option<std::ffi::OsString>) -> Result<()> {
    if get(SANDBOX_ENV).is_none_or(|v| v.is_empty() || v == "0") {
        return Ok(());
    }
    let missing: Vec<&str> = SANDBOX_REQUIRED
        .iter()
        .copied()
        .filter(|name| get(name).is_none_or(|v| v.is_empty()))
        .collect();
    if !missing.is_empty() {
        bail!(
            "{SANDBOX_ENV} is set but these variables are not: {}. Refusing to touch real paths",
            missing.join(", ")
        );
    }
    Ok(())
}

impl Locations {
    pub fn resolve() -> Result<Self> {
        check_sandbox(|name| std::env::var_os(name))?;
        let claude_dir = match std::env::var_os(CLAUDE_CONFIG_DIR_ENV) {
            Some(dir) if !dir.is_empty() => PathBuf::from(dir),
            _ => PathBuf::from(
                std::env::var_os("HOME")
                    .or_else(|| std::env::var_os("USERPROFILE"))
                    .context("cannot locate the home directory; set CLAUDE_CONFIG_DIR")?,
            )
            .join(".claude"),
        };
        Ok(Self {
            settings_file: claude_dir.join("settings.json"),
            state_file: paths::state_dir().join(STATE_FILE),
        })
    }
}

pub(crate) fn backup_path(settings: &Path) -> PathBuf {
    let mut name = settings.file_name().unwrap_or_default().to_os_string();
    name.push(BACKUP_SUFFIX);
    settings.with_file_name(name)
}

pub fn load_state(path: &Path) -> Result<SetupState> {
    match std::fs::read_to_string(path) {
        Ok(text) => serde_json::from_str(&text)
            .with_context(|| format!("{} is not a valid setup state file", path.display())),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(SetupState::default()),
        Err(err) => Err(err).with_context(|| format!("reading {}", path.display())),
    }
}

pub(crate) fn save_state(path: &Path, state: &SetupState) -> Result<()> {
    write_atomic(path, &(serde_json::to_string_pretty(state)? + "\n"))
}

/// True when `setup` has never recorded a run (used for the `serve` hint).
pub fn never_ran() -> bool {
    !paths::state_dir().join(STATE_FILE).exists()
}

// ---------- report ----------

#[derive(Default)]
pub(crate) struct Report {
    pub(crate) done: Vec<String>,
    pub(crate) pending: Vec<String>,
    pub(crate) failed: Vec<String>,
}

impl Report {
    pub(crate) fn print(&self, title: &str) {
        println!();
        println!("{title}");
        for line in &self.done {
            println!("  [ok] {line}");
        }
        for line in &self.failed {
            println!("  [failed] {line}");
        }
        if !self.pending.is_empty() {
            println!();
            println!("Still to do:");
            for line in &self.pending {
                println!("  - {line}");
            }
        }
    }
}

// ---------- steps ----------

fn codex_login_step(report: &mut Report) {
    let registry = Registry::with_default_alias();
    let status = registry.provider("codex").map(|p| p.cli().status());
    match status {
        Some(Ok(())) => report.done.push("Codex login found".into()),
        _ => report.pending.push(
            "No Codex login found. Run `codex login`, then check with `claude-codex codex auth status`"
                .into(),
        ),
    }
}

fn settings_step(
    args: &SetupArgs,
    loc: &Locations,
    state: &mut SetupState,
    report: &mut Report,
) -> Result<()> {
    let file = read_settings(&loc.settings_file)?;
    let mut value = file.value;
    let port = config::port();
    let result = merge_env(&mut value, &wanted_env(port), args.force)?;
    let shown = loc.settings_file.display();

    if let Some(existing) = result.base_url_conflict()
        && !args.force
    {
        report.pending.push(format!(
            "{shown} already sets {BASE_URL_KEY} to {existing}; left as is. Rerun with --force to point it at this proxy"
        ));
    }
    for change in &result.changes {
        match change {
            EnvChange::Added { key, value } => {
                let verb = if args.dry_run { "would add" } else { "added" };
                report
                    .done
                    .push(format!("{verb} env.{key}={value} in {shown}"));
            }
            EnvChange::Replaced {
                key,
                previous,
                value,
            } => {
                let verb = if args.dry_run {
                    "would replace"
                } else {
                    "replaced"
                };
                report.done.push(format!(
                    "{verb} env.{key} ({previous}) with {value} in {shown}"
                ));
            }
            EnvChange::AlreadySet { key } => {
                report.done.push(format!("env.{key} already configured"))
            }
            EnvChange::Conflict { key, existing } if key != BASE_URL_KEY => report
                .pending
                .push(format!("env.{key} is {existing}; left as is (use --force)")),
            EnvChange::Conflict { .. } => {}
        }
    }
    if let Some(env) = value.get("env").and_then(Value::as_object) {
        for key in FORBIDDEN_KEYS {
            if env.contains_key(key) {
                report.pending.push(format!(
                    "{shown} sets {key}; remove it, it overrides your Claude login on the claude-* route (401)"
                ));
            }
        }
    }
    if result.modified() {
        report
            .pending
            .push("Restart Claude Code so it picks up the new settings".into());
        if !args.dry_run {
            let backup = backup_path(&loc.settings_file);
            if file.existed && !backup.exists() {
                std::fs::copy(&loc.settings_file, &backup)
                    .with_context(|| format!("backing up to {}", backup.display()))?;
                state.backup_file = Some(backup.to_string_lossy().into_owned());
                report
                    .done
                    .push(format!("backup written to {}", backup.display()));
            }
            write_atomic(&loc.settings_file, &render_json(&value, &file.indent)?)?;
            state.settings_created |= !file.existed;
            state.settings_file = Some(loc.settings_file.to_string_lossy().into_owned());
            record_merge(state, &result);
        }
    }
    Ok(())
}

fn service_step(args: &SetupArgs, state: &mut SetupState, report: &mut Report) -> Result<()> {
    if service::Platform::current() == service::Platform::Other {
        report.pending.push(
            "Autostart service is not supported on this OS; run `claude-codex serve` yourself"
                .into(),
        );
        return Ok(());
    }
    let existed = service::definition_file().is_ok_and(|p| p.exists());
    if args.dry_run {
        report
            .done
            .push("would install and start the autostart service".into());
        if service::Platform::current() == service::Platform::Linux {
            let system = linger::system();
            linger_step(
                system.as_ref().map(|s| s as &dyn Loginctl),
                true,
                state,
                report,
            );
        }
        return Ok(());
    }
    service::install_quiet()?;
    state.service_installed |= !existed;
    report
        .done
        .push("autostart service installed and started".into());
    if service::Platform::current() == service::Platform::Linux {
        let system = linger::system();
        linger_step(
            system.as_ref().map(|s| s as &dyn Loginctl),
            false,
            state,
            report,
        );
    }
    Ok(())
}

/// Turn linger on so the service starts at boot, and record it when setup did.
/// A failure is a pending item, never a failed step. `backend` is `None` where
/// linger does not apply or must not be touched (sandbox), and then nothing is
/// printed.
pub(crate) fn linger_step(
    backend: Option<&dyn Loginctl>,
    dry_run: bool,
    state: &mut SetupState,
    report: &mut Report,
) {
    let Some(backend) = backend else { return };
    match linger::ensure(backend, dry_run) {
        EnableOutcome::AlreadyOn => report
            .done
            .push("linger already on (the service starts at boot)".into()),
        EnableOutcome::WouldEnable => report.done.push(
            "would enable linger with `loginctl enable-linger` so the service starts at boot"
                .into(),
        ),
        EnableOutcome::Enabled => {
            state.linger_enabled_by_setup = true;
            report
                .done
                .push("linger enabled: the service starts at boot".into());
        }
        EnableOutcome::Failed(why) => report.pending.push(format!(
            "Linger is off ({why}), so the service starts at login, not at boot. To start it at boot, {}",
            linger::MANUAL_HINT
        )),
    }
}

/// Undo [`linger_step`] for `disable`, `uninstall` and `setup --undo`: linger is
/// turned off only when `recorded` says setup turned it on. Returns
/// `(failed, text)` for the report, or `None` when there is nothing to say
/// (macOS). `recorded` is cleared when linger was turned off (or already off).
pub(crate) fn release_linger(
    backend: Option<&dyn Loginctl>,
    recorded: &mut bool,
    dry_run: bool,
) -> Option<(bool, String)> {
    if !*recorded {
        return (service::Platform::current() == service::Platform::Linux)
            .then(|| (false, "linger not touched: setup did not turn it on".into()));
    }
    let Some(backend) = backend else {
        return Some((
            false,
            format!(
                "linger left as it is: service manager calls are off ({} is set)",
                service::SERVICE_DIR_ENV
            ),
        ));
    };
    let outcome = linger::release(backend, true, dry_run);
    if matches!(
        outcome,
        linger::ReleaseOutcome::Released | linger::ReleaseOutcome::AlreadyOff
    ) {
        *recorded = false;
    }
    let failed = matches!(outcome, linger::ReleaseOutcome::Failed(_));
    linger::release_message(&outcome).map(|text| (failed, text))
}

fn agents_step(args: &SetupArgs, state: &mut SetupState, report: &mut Report) -> Result<()> {
    let dir = agent_cards::agents_dir()?;
    let registry = Registry::with_default_alias();
    let actions = agent_cards::sync_once(&registry, &dir, args.dry_run)?;
    let verb = if args.dry_run { "would sync" } else { "synced" };
    report.done.push(format!(
        "{verb} subagent cards in {} ({} change(s))",
        dir.display(),
        actions.len()
    ));
    if !args.dry_run {
        state.agents_synced = true;
    }
    Ok(())
}

pub fn run_setup(args: SetupArgs) -> Result<()> {
    run_setup_as(args, "Setup", "setup")
}

/// `setup` and `enable` share one implementation; `noun` only names it in the
/// report and the error.
pub(crate) fn run_setup_as(args: SetupArgs, title: &str, noun: &str) -> Result<()> {
    let loc = Locations::resolve()?;
    if args.undo {
        return run_undo(&args, &loc);
    }
    // Enabling means cards may be synced again by a running `serve`.
    if !args.dry_run
        && !args.no_agents
        && let Err(err) = crate::lifecycle::clear_disabled_marker()
    {
        eprintln!("warning: could not remove the disabled marker: {err:#}");
    }
    let mut state = load_state(&loc.state_file)?;
    let mut report = Report::default();
    if args.dry_run {
        println!("Dry run: nothing will be changed.");
    }

    if args.no_service {
        report.done.push("service skipped (--no-service)".into());
    } else if let Err(err) = service_step(&args, &mut state, &mut report) {
        report.failed.push(format!("service: {err:#}"));
    }
    if args.no_agents {
        report.done.push("agent cards skipped (--no-agents)".into());
    } else if let Err(err) = agents_step(&args, &mut state, &mut report) {
        report.failed.push(format!("agent cards: {err:#}"));
    }
    if args.no_claude_settings {
        report
            .done
            .push("Claude Code settings skipped (--no-claude-settings)".into());
    } else if let Err(err) = settings_step(&args, &loc, &mut state, &mut report) {
        report.failed.push(format!("Claude Code settings: {err:#}"));
    }
    if !args.dry_run {
        save_state(&loc.state_file, &state)?;
    }
    codex_login_step(&mut report);

    report.print(&if args.dry_run {
        format!("{title} (dry run):")
    } else {
        format!("{title} finished:")
    });
    if report.failed.is_empty() {
        Ok(())
    } else {
        bail!(
            "{noun} finished with {} failed step(s); fix the cause and rerun `claude-codex {noun}`",
            report.failed.len()
        )
    }
}

fn run_undo(args: &SetupArgs, loc: &Locations) -> Result<()> {
    if !loc.state_file.exists() {
        println!(
            "Nothing to undo: no setup record at {}",
            loc.state_file.display()
        );
        return Ok(());
    }
    let state = load_state(&loc.state_file)?;
    let mut report = Report::default();
    let prefix = if args.dry_run { "would " } else { "" };
    if args.dry_run {
        println!("Dry run: nothing will be changed.");
    }

    if state.service_installed && !args.no_service {
        if args.dry_run {
            report
                .done
                .push("would uninstall the autostart service".into());
        } else {
            match service::run_service_command(service::ServiceCommand::Uninstall) {
                Ok(()) => report.done.push("autostart service uninstalled".into()),
                Err(err) => report.failed.push(format!("service: {err:#}")),
            }
        }
    }
    if state.linger_enabled_by_setup && !args.no_service {
        let mut recorded = true;
        let system = linger::system();
        let backend = system.as_ref().map(|s| s as &dyn Loginctl);
        if let Some((failed, text)) = release_linger(backend, &mut recorded, args.dry_run) {
            if failed {
                report.failed.push(text);
            } else {
                report.done.push(text);
            }
        }
    }
    if state.agents_synced && !args.no_agents {
        let result = agent_cards::agents_dir()
            .and_then(|dir| agent_cards::clean_once(&dir, args.dry_run).map(|a| (dir, a)));
        match result {
            Ok((dir, actions)) => report.done.push(format!(
                "{prefix}removed {} managed agent card(s) from {}",
                actions.len(),
                dir.display()
            )),
            Err(err) => report.failed.push(format!("agent cards: {err:#}")),
        }
    }
    if !args.no_claude_settings
        && let Some(path) = state.settings_file.as_deref().map(PathBuf::from)
        && (!state.added.is_empty() || !state.replaced.is_empty())
    {
        let outcome = (|| -> Result<Vec<String>> {
            let file = read_settings(&path)?;
            let mut value = file.value;
            let notes = unmerge_env(&mut value, &state)?;
            if !args.dry_run && file.existed {
                let text = crate::uninstall::restored_text(&value, &file.indent, &state);
                write_atomic(&path, &text)?;
            }
            Ok(notes)
        })();
        match outcome {
            Ok(notes) => {
                for note in notes {
                    report
                        .done
                        .push(format!("{prefix}{note} in {}", path.display()));
                }
                if let Some(backup) = &state.backup_file {
                    report.pending.push(format!(
                        "The pre-setup backup is still at {backup}; delete it if you no longer need it"
                    ));
                }
                report
                    .pending
                    .push("Restart Claude Code so it drops the proxy settings".into());
            }
            Err(err) => report.failed.push(format!("Claude Code settings: {err:#}")),
        }
    }
    if !args.dry_run && report.failed.is_empty() {
        let _ = crate::lifecycle::clear_disabled_marker();
        let _ = std::fs::remove_file(&loc.state_file);
        report.done.push("setup record removed".into());
    }
    report.print(if args.dry_run {
        "Undo (dry run):"
    } else {
        "Undo finished:"
    });
    if report.failed.is_empty() {
        Ok(())
    } else {
        bail!("undo finished with {} failed step(s)", report.failed.len())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::linger::fake::FakeLoginctl;
    use serde_json::json;

    fn linger_step_with(fake: &FakeLoginctl, dry_run: bool) -> (SetupState, Report) {
        let mut state = SetupState::default();
        let mut report = Report::default();
        linger_step(Some(fake), dry_run, &mut state, &mut report);
        (state, report)
    }

    #[test]
    fn linger_already_on_is_not_recorded() {
        let fake = FakeLoginctl::new(Some(true));
        let (state, report) = linger_step_with(&fake, false);
        assert!(!state.linger_enabled_by_setup);
        assert!(report.done[0].contains("linger already on"));
        assert!(report.pending.is_empty());
        assert!(!fake.calls.borrow().contains(&"enable"));
    }

    #[test]
    fn linger_enabled_by_setup_is_recorded() {
        let fake = FakeLoginctl::new(Some(false));
        let (state, report) = linger_step_with(&fake, false);
        assert!(state.linger_enabled_by_setup);
        assert!(report.done[0].contains("linger enabled"));
        assert_eq!(fake.on.get(), Some(true));
    }

    #[test]
    fn linger_failure_is_pending_with_the_manual_commands() {
        let mut fake = FakeLoginctl::new(Some(false));
        fake.enable_fails = true;
        let (state, report) = linger_step_with(&fake, false);
        assert!(!state.linger_enabled_by_setup);
        assert!(report.failed.is_empty() && report.done.is_empty());
        let text = &report.pending[0];
        assert!(text.contains("`loginctl enable-linger`"), "{text}");
        assert!(text.contains("sudo loginctl enable-linger $USER"), "{text}");
    }

    #[test]
    fn linger_dry_run_changes_and_records_nothing() {
        let fake = FakeLoginctl::new(Some(false));
        let (state, report) = linger_step_with(&fake, true);
        assert!(!state.linger_enabled_by_setup);
        assert!(report.done[0].contains("would enable linger"));
        assert_eq!(fake.on.get(), Some(false));
    }

    #[test]
    fn linger_without_a_backend_prints_nothing() {
        let mut state = SetupState::default();
        let mut report = Report::default();
        linger_step(None, false, &mut state, &mut report);
        assert!(report.done.is_empty() && report.pending.is_empty());
    }

    #[test]
    fn release_reverts_only_what_setup_recorded() {
        let fake = FakeLoginctl::new(Some(true));
        let mut recorded = false;
        let out = release_linger(Some(&fake), &mut recorded, false);
        assert!(fake.calls.borrow().is_empty());
        assert_eq!(fake.on.get(), Some(true));
        if let Some((failed, text)) = out {
            assert!(
                !failed && text.contains("setup did not turn it on"),
                "{text}"
            );
        }

        let mut recorded = true;
        let (failed, text) = release_linger(Some(&fake), &mut recorded, false).unwrap();
        assert!(!failed && text.contains("turned off"), "{text}");
        assert!(!recorded);
        assert_eq!(fake.on.get(), Some(false));
    }

    #[test]
    fn release_dry_run_and_failure_keep_the_record() {
        let fake = FakeLoginctl::new(Some(true));
        let mut recorded = true;
        let (_, text) = release_linger(Some(&fake), &mut recorded, true).unwrap();
        assert!(text.starts_with("would turn linger off"), "{text}");
        assert!(recorded && fake.on.get() == Some(true));

        let mut fake = FakeLoginctl::new(Some(true));
        fake.disable_fails = true;
        let (failed, _) = release_linger(Some(&fake), &mut recorded, false).unwrap();
        assert!(failed && recorded);
    }

    #[test]
    fn release_without_a_backend_leaves_linger_and_record() {
        let mut recorded = true;
        let (failed, text) = release_linger(None, &mut recorded, false).unwrap();
        assert!(!failed && recorded, "{text}");
    }

    #[test]
    fn old_state_without_the_linger_field_still_reads() {
        let state: SetupState =
            serde_json::from_str(r#"{"service_installed":true,"agents_synced":true}"#).unwrap();
        assert!(state.service_installed && !state.linger_enabled_by_setup);
        let text = serde_json::to_string(&state).unwrap();
        let back: SetupState = serde_json::from_str(&text).unwrap();
        assert_eq!(back, state);
    }

    fn wanted() -> Vec<(&'static str, String)> {
        wanted_env(18765)
    }

    #[test]
    fn merge_into_empty_object_creates_env() {
        let mut v = json!({});
        let r = merge_env(&mut v, &wanted(), false).unwrap();
        assert!(r.env_created && r.modified());
        assert_eq!(v["env"][BASE_URL_KEY], "http://127.0.0.1:18765");
        assert_eq!(v["env"][DISCOVERY_KEY], "1");
    }

    #[test]
    fn merge_keeps_previous_keys_and_order() {
        let mut v: Value = serde_json::from_str(
            r#"{"model":"opus","env":{"FOO":"1"},"permissions":{"allow":["x"]}}"#,
        )
        .unwrap();
        merge_env(&mut v, &wanted(), false).unwrap();
        let keys: Vec<_> = v.as_object().unwrap().keys().cloned().collect();
        assert_eq!(keys, ["model", "env", "permissions"]);
        let env_keys: Vec<_> = v["env"].as_object().unwrap().keys().cloned().collect();
        assert_eq!(env_keys, ["FOO", BASE_URL_KEY, DISCOVERY_KEY]);
        assert_eq!(v["permissions"]["allow"][0], "x");
    }

    #[test]
    fn foreign_base_url_is_not_overwritten() {
        let mut v = json!({"env":{BASE_URL_KEY:"https://gateway.example"}});
        let r = merge_env(&mut v, &wanted(), false).unwrap();
        assert!(!r.modified());
        assert!(r.base_url_conflict().is_some());
        assert_eq!(v["env"][BASE_URL_KEY], "https://gateway.example");
        assert!(v["env"].get(DISCOVERY_KEY).is_none());
    }

    #[test]
    fn force_replaces_foreign_base_url_and_records_previous() {
        let mut v = json!({"env":{BASE_URL_KEY:"https://gateway.example"}});
        let r = merge_env(&mut v, &wanted(), true).unwrap();
        assert_eq!(v["env"][BASE_URL_KEY], "http://127.0.0.1:18765");
        let mut state = SetupState::default();
        record_merge(&mut state, &r);
        assert_eq!(
            state.replaced[BASE_URL_KEY].previous,
            json!("https://gateway.example")
        );
        let notes = unmerge_env(&mut v, &state).unwrap();
        assert!(!notes.is_empty());
        assert_eq!(v["env"][BASE_URL_KEY], "https://gateway.example");
    }

    #[test]
    fn loopback_aliases_count_as_ours_and_change_nothing() {
        for url in [
            "http://localhost:18765",
            "http://localhost:18765/",
            "http://127.0.0.1:18765/",
            "http://[::1]:18765",
            "http://[::1]:18765/",
            "HTTP://LocalHost:18765",
        ] {
            let mut v = json!({"env":{BASE_URL_KEY:url}});
            let r = merge_env(
                &mut v,
                &[(BASE_URL_KEY, "http://127.0.0.1:18765".into())],
                false,
            )
            .unwrap();
            assert!(r.base_url_conflict().is_none() && !r.modified(), "{url}");
            assert_eq!(v["env"][BASE_URL_KEY], url);
            let mut state = SetupState::default();
            record_merge(&mut state, &r);
            assert!(state.added.is_empty() && state.replaced.is_empty(), "{url}");
        }
    }

    #[test]
    fn same_base_url_uses_the_effective_port() {
        let ours = "http://127.0.0.1:18765";
        assert!(same_base_url("http://localhost:18765/", ours));
        assert!(same_base_url("http://[::1]:18765", ours));
        // The wanted side is normalized too.
        assert!(same_base_url(
            "http://127.0.0.1:18765",
            "http://[::1]:18765/"
        ));
        // Default port 80 is the effective port when none is written.
        assert!(same_base_url("http://localhost", "http://127.0.0.1:80"));
        assert!(same_base_url("http://localhost:80", "http://127.0.0.1"));
        // Different port, scheme, host or extra parts are different addresses.
        assert!(!same_base_url("http://localhost:18766", ours));
        assert!(!same_base_url("https://localhost:18765", ours));
        assert!(!same_base_url("http://localhost", ours));
        assert!(!same_base_url("http://192.168.1.5:18765", ours));
        assert!(!same_base_url("http://0.0.0.0:18765", ours));
        assert!(!same_base_url("http://localhost:18765/v1", ours));
        assert!(!same_base_url("http://user@localhost:18765", ours));
        assert!(!same_base_url("http://localhost:18765/?x=1", ours));
        assert!(!same_base_url("https://gateway.example", ours));
        assert!(!same_base_url("not a url", ours));
        // Non-loopback URLs still compare textually, ignoring case and a trailing slash.
        assert!(same_base_url(
            "https://Gateway.example/",
            "https://gateway.example"
        ));
    }

    #[test]
    fn loopback_url_on_another_port_is_a_conflict() {
        let mut v = json!({"env":{BASE_URL_KEY:"http://localhost:9999"}});
        let r = merge_env(&mut v, &wanted(), false).unwrap();
        assert!(r.base_url_conflict().is_some() && !r.modified());
    }

    #[test]
    fn merge_is_idempotent() {
        let mut v = json!({"a":1});
        merge_env(&mut v, &wanted(), false).unwrap();
        let first = v.clone();
        let r = merge_env(&mut v, &wanted(), false).unwrap();
        assert!(!r.modified());
        assert_eq!(v, first);
    }

    #[test]
    fn bad_shapes_are_errors() {
        assert!(merge_env(&mut json!([1]), &wanted(), false).is_err());
        assert!(merge_env(&mut json!({"env": 3}), &wanted(), false).is_err());
    }

    #[test]
    fn credentials_are_refused() {
        let mut v = json!({});
        let w = vec![("ANTHROPIC_API_KEY", "x".to_string())];
        assert!(merge_env(&mut v, &w, false).is_err());
        assert!(merge_env(&mut v, &[("ANTHROPIC_AUTH_TOKEN", "x".into())], true).is_err());
        assert!(wanted().iter().all(|(k, _)| !FORBIDDEN_KEYS.contains(k)));
    }

    #[test]
    fn unmerge_removes_only_ours_and_empty_created_env() {
        let mut v = json!({});
        let r = merge_env(&mut v, &wanted(), false).unwrap();
        let mut state = SetupState::default();
        record_merge(&mut state, &r);
        unmerge_env(&mut v, &state).unwrap();
        assert_eq!(v, json!({}));

        let mut v = json!({"env":{"FOO":"1"}});
        let r = merge_env(&mut v, &wanted(), false).unwrap();
        let mut state = SetupState::default();
        record_merge(&mut state, &r);
        v["env"][BASE_URL_KEY] = json!("http://other");
        let notes = unmerge_env(&mut v, &state).unwrap();
        assert!(notes.iter().any(|n| n.contains("left ANTHROPIC_BASE_URL")));
        assert_eq!(v["env"]["FOO"], "1");
        assert!(v["env"].get(DISCOVERY_KEY).is_none());
    }

    #[test]
    fn invalid_json_is_a_clear_error_and_file_is_untouched() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        std::fs::write(&path, "{ not json").unwrap();
        let err = read_settings(&path).err().unwrap().to_string();
        assert!(err.contains("not valid JSON") && err.contains("untouched"));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "{ not json");
    }

    #[test]
    fn empty_file_is_treated_as_empty_object() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        std::fs::write(&path, "  \n").unwrap();
        assert_eq!(read_settings(&path).unwrap().value, json!({}));
    }

    #[test]
    fn indent_is_detected_and_reused() {
        assert_eq!(detect_indent("{\n    \"a\": 1\n}"), "    ");
        assert_eq!(detect_indent("{\n\t\"a\": 1\n}"), "\t");
        assert_eq!(detect_indent("{}"), "  ");
        let text = render_json(&json!({"a":{"b":1}}), "    ").unwrap();
        assert_eq!(text, "{\n    \"a\": {\n        \"b\": 1\n    }\n}\n");
    }

    #[test]
    fn sandbox_guard_requires_every_variable() {
        let none = |_: &str| None;
        assert!(check_sandbox(none).is_ok());
        let partial = |name: &str| match name {
            SANDBOX_ENV | "HOME" => Some("/tmp/x".into()),
            _ => None,
        };
        let err = check_sandbox(partial).unwrap_err().to_string();
        assert!(err.contains("CLAUDE_CONFIG_DIR") && err.contains("CCP_SERVICE_DIR"));
        let full = |_: &str| Some("/tmp/x".into());
        assert!(check_sandbox(full).is_ok());
    }

    #[test]
    fn backup_name_is_next_to_settings() {
        assert_eq!(
            backup_path(Path::new("/a/settings.json")),
            PathBuf::from("/a/settings.json.claude-codex.bak")
        );
    }
}
