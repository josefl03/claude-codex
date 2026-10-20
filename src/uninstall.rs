//! `claude-codex uninstall`: remove everything `setup` and `install.sh` put on
//! the machine.
//!
//! Steps run in this order and are independent: a step that fails does not stop
//! the others, the summary marks each one, and the exit code is non-zero when
//! any failed. Running it again is harmless.
//!
//! 1. the autostart service (stopped, disabled, unit/plist removed);
//! 2. the subagent cards carrying the managed marker;
//! 3. the `settings.json` keys recorded in `setup.json`, restored exactly;
//! 4. with `--purge`, the proxy's own state directory; with
//!    `--purge-credentials`, also the proxy's config directory (its provider
//!    logins). `~/.codex` belongs to the Codex CLI and is never touched;
//! 5. the binary itself, only from a known install location and never from a
//!    `target/` directory (`--keep-binary` to keep it).
//!
//! Safety for tests: the same `CCP_SETUP_SANDBOX` guard as `setup` applies, and
//! in sandbox mode the system-wide install location is not a candidate.
//! Deletions use exact computed paths; the only recursive delete is the proxy's
//! own state directory, and only when it is named `claude-code-proxy`.

use crate::linger::{self, Loginctl};
use crate::{agent_cards, config, paths, service, setup};
use anyhow::{Result, bail};
use clap::Args;
use serde_json::Value;
use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};

/// Overrides the directory the installer used; its binary may be removed.
pub const INSTALL_DIR_ENV: &str = "CLAUDE_CODEX_INSTALL_DIR";
const BINARY_NAME: &str = "claude-codex";
/// Name of the proxy's state directory; `--purge` refuses any other name.
const STATE_DIR_NAME: &str = "claude-code-proxy";
/// Provider login directories inside the proxy's config directory.
const PROVIDER_DIRS: [&str; 4] = ["codex", "kimi", "cursor", "grok"];
/// Files inside a provider directory that hold a login or device identity.
const PROVIDER_FILES: [&str; 2] = ["auth.json", "device_id"];

#[derive(Debug, Clone, Args)]
pub struct UninstallArgs {
    /// Show what would be done without changing anything
    #[arg(long)]
    pub dry_run: bool,
    /// Do not ask for confirmation
    #[arg(long, short = 'y')]
    pub yes: bool,
    /// Also delete the proxy's state directory (model cache, logs, setup record). Never credentials
    #[arg(long)]
    pub purge: bool,
    /// Also delete the proxy's config directory with its provider logins (implies --purge)
    #[arg(long)]
    pub purge_credentials: bool,
    /// Do not delete the claude-codex binary
    #[arg(long)]
    pub keep_binary: bool,
}

impl UninstallArgs {
    fn purge_state(&self) -> bool {
        self.purge || self.purge_credentials
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Status {
    Ok,
    Skipped,
    Failed,
}

#[derive(Debug)]
struct Step {
    name: &'static str,
    status: Status,
    summary: String,
    details: Vec<String>,
}

impl Step {
    fn new(name: &'static str, status: Status, summary: impl Into<String>) -> Self {
        Self {
            name,
            status,
            summary: summary.into(),
            details: Vec::new(),
        }
    }
    fn ok(name: &'static str, summary: impl Into<String>) -> Self {
        Self::new(name, Status::Ok, summary)
    }
    fn skipped(name: &'static str, summary: impl Into<String>) -> Self {
        Self::new(name, Status::Skipped, summary)
    }
    fn failed(name: &'static str, summary: impl Into<String>) -> Self {
        Self::new(name, Status::Failed, summary)
    }
    fn detail(mut self, line: impl Into<String>) -> Self {
        self.details.push(line.into());
        self
    }
}

/// What the run knows up front.
struct Context {
    dry_run: bool,
    loc: setup::Locations,
    state_exists: bool,
    state: std::result::Result<setup::SetupState, String>,
    /// Set when linger could not be turned off: the setup record then stays.
    linger_pending: std::cell::Cell<bool>,
}

pub fn run_uninstall(args: UninstallArgs) -> Result<()> {
    let loc = setup::Locations::resolve()?;
    let state_exists = loc.state_file.exists();
    let state = setup::load_state(&loc.state_file).map_err(|err| format!("{err:#}"));
    let ctx = Context {
        dry_run: args.dry_run,
        loc,
        state_exists,
        state,
        linger_pending: std::cell::Cell::new(false),
    };

    if args.dry_run {
        println!("Dry run: nothing will be changed.");
    } else if !args.yes {
        if !std::io::stdin().is_terminal() {
            bail!(
                "refusing to uninstall without confirmation: stdin is not a terminal. \
                 Rerun with --yes to proceed, or --dry-run to preview"
            );
        }
        println!("claude-codex uninstall will:");
        for line in plan_lines(&args, &ctx) {
            println!("  - {line}");
        }
        print!("Proceed? [y/N] ");
        std::io::stdout().flush().ok();
        let mut answer = String::new();
        std::io::stdin().read_line(&mut answer)?;
        if !is_yes(&answer) {
            println!("Aborted. Nothing was changed.");
            return Ok(());
        }
    }

    let mut notes = Vec::new();
    let steps = vec![
        service_step(&ctx, &mut notes),
        linger_step(&ctx),
        agents_step(&ctx),
        settings_step(&ctx, &mut notes),
        state_dir_step(&args, &ctx),
        credentials_step(&args, &ctx),
        binary_step(&args, &ctx),
    ];
    if !args.dry_run {
        // The marker left by `claude-codex disable` has no meaning once removed.
        let _ = crate::lifecycle::clear_disabled_marker();
    }
    print_summary(&steps, &notes, args.dry_run);
    let failed = steps.iter().filter(|s| s.status == Status::Failed).count();
    if failed > 0 {
        bail!(
            "uninstall finished with {failed} failed step(s); fix the cause and rerun `claude-codex uninstall`"
        );
    }
    Ok(())
}

fn is_yes(answer: &str) -> bool {
    matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes")
}

fn print_summary(steps: &[Step], notes: &[String], dry_run: bool) {
    println!();
    println!(
        "{}",
        if dry_run {
            "Uninstall (dry run):"
        } else {
            "Uninstall finished:"
        }
    );
    for step in steps {
        let mark = match step.status {
            Status::Ok => "\u{2713}",
            Status::Skipped => "-",
            Status::Failed => "\u{2717}",
        };
        println!("  {mark} {}: {}", step.name, step.summary);
        for line in &step.details {
            println!("      {line}");
        }
    }
    if !notes.is_empty() {
        println!();
        println!("Notes:");
        for note in notes {
            println!("  - {note}");
        }
    }
}

// ---------- the plan shown before asking ----------

fn plan_lines(args: &UninstallArgs, ctx: &Context) -> Vec<String> {
    let mut lines = Vec::new();
    lines.push(match service::definition_file() {
        Ok(path) if service::Platform::current() != service::Platform::Other => {
            format!(
                "stop and disable the autostart service and delete {}",
                path.display()
            )
        }
        _ => "skip the autostart service (not supported here)".to_string(),
    });
    lines.push(match agent_cards::agents_dir() {
        Ok(dir) => format!("remove the managed subagent cards in {}", dir.display()),
        Err(_) => "remove the managed subagent cards".to_string(),
    });
    lines.push(match &ctx.state {
        Ok(state) if ctx.state_exists && !(state.added.is_empty() && state.replaced.is_empty()) => {
            format!(
                "revert what setup changed in {}",
                state
                    .settings_file
                    .as_deref()
                    .unwrap_or("Claude Code's settings.json")
            )
        }
        Ok(_) if ctx.state_exists => {
            "leave settings.json alone (setup changed nothing there)".into()
        }
        _ => "leave settings.json alone (no setup record)".to_string(),
    });
    if args.purge_state() {
        lines.push(format!(
            "delete the state directory {}",
            paths::state_dir().display()
        ));
    }
    if args.purge_credentials {
        lines.push(format!(
            "delete the proxy config directory contents (provider logins) in {}",
            paths::config_dir().display()
        ));
    }
    lines.push(match std::env::current_exe() {
        _ if args.keep_binary => "keep the claude-codex binary".to_string(),
        Ok(exe) => match binary_verdict(&exe, &install_context()) {
            Ok(()) => format!("delete the binary {}", exe.display()),
            Err(reason) => format!("keep the binary ({reason})"),
        },
        Err(_) => "keep the binary (cannot locate it)".to_string(),
    });
    lines
}

// ---------- 1: service ----------

fn service_step(ctx: &Context, notes: &mut Vec<String>) -> Step {
    const NAME: &str = "autostart service";
    if service::Platform::current() == service::Platform::Other {
        return Step::skipped(NAME, "not supported on this OS");
    }
    let path = match service::definition_file() {
        Ok(path) => path,
        Err(err) => return Step::failed(NAME, format!("{err:#}")),
    };
    let step = if !path.exists() {
        Step::ok(
            NAME,
            format!("not installed ({} does not exist)", path.display()),
        )
    } else if ctx.dry_run {
        Step::ok(
            NAME,
            format!("would stop, disable and remove {}", path.display()),
        )
    } else {
        match service::run_service_command(service::ServiceCommand::Uninstall) {
            Ok(()) if path.exists() => {
                Step::failed(NAME, format!("{} is still there", path.display()))
            }
            Ok(()) => Step::ok(
                NAME,
                format!("stopped, disabled and removed {}", path.display()),
            ),
            Err(err) => Step::failed(NAME, format!("{err:#}")),
        }
    };
    if step.status != Status::Failed
        && let Some(note) = listener_note(ctx.dry_run)
    {
        notes.push(note);
    }
    step
}

/// A warning when something still listens on the configured port. Only the
/// service is managed here; a manual `serve` is never stopped.
fn listener_note(dry_run: bool) -> Option<String> {
    use std::net::{SocketAddr, TcpStream, ToSocketAddrs};
    let port = config::port();
    let host = match config::bind_address().as_str() {
        "" | "0.0.0.0" | "::" => "127.0.0.1".to_string(),
        other => other.to_string(),
    };
    let addr: SocketAddr = (host.as_str(), port).to_socket_addrs().ok()?.next()?;
    TcpStream::connect_timeout(&addr, std::time::Duration::from_millis(300)).ok()?;
    Some(if dry_run {
        format!(
            "something is listening on {host}:{port}. If it is the autostart service it will be stopped; \
             a manually started `claude-codex serve` is never stopped by uninstall"
        )
    } else {
        format!(
            "something is still listening on {host}:{port}: probably a manual `claude-codex serve`, \
             which uninstall does not stop. Stop it yourself"
        )
    })
}

// ---------- 1b: linger ----------

/// Turn linger off again, but only when the setup record says setup turned it on.
fn linger_step(ctx: &Context) -> Step {
    const NAME: &str = "linger";
    if service::Platform::current() != service::Platform::Linux {
        return Step::skipped(NAME, "not applicable on this OS");
    }
    let mut recorded = match &ctx.state {
        Ok(state) => state.linger_enabled_by_setup,
        Err(_) => false,
    };
    let system = linger::system();
    let backend = system.as_ref().map(|s| s as &dyn Loginctl);
    match setup::release_linger(backend, &mut recorded, ctx.dry_run) {
        Some((true, text)) => {
            ctx.linger_pending.set(true);
            Step::failed(NAME, text)
        }
        Some((false, text)) if text.starts_with("linger not touched") => Step::skipped(NAME, text),
        Some((false, text)) => Step::ok(NAME, text),
        None => Step::skipped(NAME, "not applicable on this OS"),
    }
}

// ---------- 2: agent cards ----------

fn agents_step(ctx: &Context) -> Step {
    const NAME: &str = "subagent cards";
    let result = agent_cards::agents_dir()
        .and_then(|dir| agent_cards::clean_once(&dir, ctx.dry_run).map(|actions| (dir, actions)));
    match result {
        Ok((dir, actions)) => {
            let verb = if ctx.dry_run {
                "would remove"
            } else {
                "removed"
            };
            Step::ok(
                NAME,
                format!(
                    "{verb} {} managed card(s) in {}",
                    actions.len(),
                    dir.display()
                ),
            )
        }
        Err(err) => Step::failed(NAME, format!("{err:#}")),
    }
}

// ---------- 3: Claude Code settings ----------

fn settings_step(ctx: &Context, notes: &mut Vec<String>) -> Step {
    const NAME: &str = "Claude Code settings";
    if !ctx.state_exists {
        notes.push(format!(
            "There is no setup record ({}), so {} was not touched. If it sets ANTHROPIC_BASE_URL \
             for this proxy and you want Claude Code to stop using it, remove that line yourself",
            ctx.loc.state_file.display(),
            ctx.loc.settings_file.display()
        ));
        return Step::skipped(NAME, "no setup record; settings.json not touched");
    }
    let state = match &ctx.state {
        Ok(state) => state,
        Err(err) => {
            return Step::failed(NAME, format!("the setup record is unreadable: {err}"));
        }
    };
    let Some(path) = state.settings_file.as_deref().map(PathBuf::from) else {
        return finish_record(ctx, Step::ok(NAME, "setup did not change settings.json"));
    };
    if state.added.is_empty() && state.replaced.is_empty() {
        return finish_record(
            ctx,
            Step::ok(
                NAME,
                format!("setup added nothing to {}; left as is", path.display()),
            ),
        );
    }
    match revert_settings(&path, state, ctx.dry_run) {
        Ok(mut step) => {
            if step.status == Status::Ok && !ctx.dry_run {
                notes.push("Restart Claude Code so it drops the proxy settings".into());
            }
            if let Some(backup) = state
                .backup_file
                .as_deref()
                .filter(|b| Path::new(b).exists())
            {
                step = step.detail(format!(
                    "the pre-setup backup stays at {backup}; delete it yourself if you no longer need it"
                ));
            }
            finish_record(ctx, step)
        }
        Err(err) => Step::failed(NAME, format!("{err:#}")),
    }
}

/// Remove the setup record once its job is done, so a second run starts clean.
fn finish_record(ctx: &Context, step: Step) -> Step {
    if ctx.dry_run || step.status == Status::Failed {
        return step;
    }
    if ctx.linger_pending.get() {
        return step.detail("setup record kept: linger is still recorded; rerun uninstall");
    }
    match std::fs::remove_file(&ctx.loc.state_file) {
        Ok(()) => step.detail("setup record removed"),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => step,
        Err(err) => Step::failed(
            step.name,
            format!("could not remove {}: {err}", ctx.loc.state_file.display()),
        ),
    }
}

fn revert_settings(path: &Path, state: &setup::SetupState, dry_run: bool) -> Result<Step> {
    const NAME: &str = "Claude Code settings";
    let file = setup::read_settings(path)?;
    if !file.existed {
        return Ok(Step::ok(
            NAME,
            format!("{} is gone; nothing to revert", path.display()),
        ));
    }
    let mut value = file.value;
    let notes = setup::unmerge_env(&mut value, state)?;
    let mut step;
    let nothing_left = value.as_object().is_some_and(|o| o.is_empty());
    if state.settings_created && nothing_left {
        // Setup created this file and nothing else lives in it.
        if !dry_run {
            std::fs::remove_file(path)?;
        }
        let verb = if dry_run { "would remove" } else { "removed" };
        step = Step::ok(
            NAME,
            format!("{verb} {} (setup had created it)", path.display()),
        );
    } else {
        let text = restored_text(&value, &file.indent, state);
        let current = std::fs::read_to_string(path)?;
        if text != current && !dry_run {
            setup::write_atomic(path, &text)?;
        }
        let verb = if dry_run { "would revert" } else { "reverted" };
        step = Step::ok(NAME, format!("{verb} {}", path.display()));
    }
    for note in notes {
        step = step.detail(if dry_run {
            format!("dry run: {note}")
        } else {
            note
        });
    }
    Ok(step)
}

/// The text to write back. When the reverted settings are the same document as
/// the pre-setup backup (same keys, values and order), the backup's own bytes
/// are used so the file returns to its original form exactly.
pub(crate) fn restored_text(value: &Value, indent: &str, state: &setup::SetupState) -> String {
    if let Some(backup) = state.backup_file.as_deref()
        && let Ok(text) = std::fs::read_to_string(backup)
        && let Ok(original) = serde_json::from_str::<Value>(&text)
        && serde_json::to_string(&original).ok() == serde_json::to_string(value).ok()
    {
        return text;
    }
    setup::render_json(value, indent).unwrap_or_default()
}

// ---------- 4: purge ----------

fn state_dir_step(args: &UninstallArgs, ctx: &Context) -> Step {
    const NAME: &str = "state directory";
    if !args.purge_state() {
        return Step::skipped(
            NAME,
            "kept (use --purge to delete caches, logs and the setup record)",
        );
    }
    let dir = paths::state_dir();
    if dir.file_name().and_then(|n| n.to_str()) != Some(STATE_DIR_NAME) {
        return Step::failed(
            NAME,
            format!(
                "refusing to delete {}: it is not a directory named {STATE_DIR_NAME}",
                dir.display()
            ),
        );
    }
    match dir.symlink_metadata() {
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            Step::ok(NAME, format!("{} does not exist", dir.display()))
        }
        Err(err) => Step::failed(NAME, format!("{}: {err}", dir.display())),
        Ok(meta) if !meta.is_dir() => Step::failed(
            NAME,
            format!(
                "refusing to delete {}: it is not a plain directory",
                dir.display()
            ),
        ),
        Ok(_) if ctx.dry_run => Step::ok(NAME, format!("would delete {}", dir.display())),
        Ok(_) => match std::fs::remove_dir_all(&dir) {
            Ok(()) => Step::ok(NAME, format!("deleted {}", dir.display())),
            Err(err) => Step::failed(NAME, format!("{}: {err}", dir.display())),
        },
    }
}

/// True when `dir` must never be emptied: the filesystem root, the home
/// directory, or anything inside the Codex CLI's `.codex`.
fn protected_dir(dir: &Path, home: Option<&Path>) -> bool {
    dir.parent().is_none()
        || home.is_some_and(|h| dir == h)
        || dir.components().any(|c| c.as_os_str() == ".codex")
}

fn credentials_step(args: &UninstallArgs, ctx: &Context) -> Step {
    const NAME: &str = "credentials and config";
    if !args.purge_credentials {
        return Step::skipped(
            NAME,
            "kept (use --purge-credentials to delete the proxy's provider logins and config)",
        );
    }
    let dir = paths::config_dir();
    let home = std::env::var_os("HOME").map(PathBuf::from);
    if protected_dir(&dir, home.as_deref()) {
        return Step::failed(NAME, format!("refusing to touch {}", dir.display()));
    }
    let mut targets: Vec<PathBuf> = Vec::new();
    for provider in PROVIDER_DIRS {
        for file in PROVIDER_FILES {
            targets.push(dir.join(provider).join(file));
        }
    }
    targets.push(dir.join("config.json"));
    targets.push(dir.join("service.env"));

    let mut removed = 0usize;
    let mut errors = Vec::new();
    for target in &targets {
        match target.symlink_metadata() {
            Ok(meta) if meta.is_file() || meta.file_type().is_symlink() => {
                if ctx.dry_run {
                    removed += 1;
                } else {
                    match std::fs::remove_file(target) {
                        Ok(()) => removed += 1,
                        Err(err) => errors.push(format!("{}: {err}", target.display())),
                    }
                }
            }
            _ => {}
        }
    }
    if !ctx.dry_run {
        // Only empty directories go; anything else the user keeps there stays.
        for provider in PROVIDER_DIRS {
            let _ = std::fs::remove_dir(dir.join(provider));
        }
        let _ = std::fs::remove_dir(&dir);
    }
    let verb = if ctx.dry_run {
        "would delete"
    } else {
        "deleted"
    };
    let mut step = if errors.is_empty() {
        Step::ok(
            NAME,
            format!("{verb} {removed} file(s) in {}", dir.display()),
        )
    } else {
        Step::failed(NAME, errors.join("; "))
    };
    step = step.detail("the Codex CLI's own ~/.codex is never touched");
    if cfg!(target_os = "macos") {
        step = step.detail(
            "macOS Keychain entries (kimi, cursor) are not removed; delete them in Keychain Access",
        );
    }
    step
}

// ---------- 5: the binary ----------

/// Where the binary may be removed from.
struct InstallContext {
    home: Option<PathBuf>,
    install_dir: Option<PathBuf>,
    sandbox: bool,
}

fn install_context() -> InstallContext {
    let get = |name: &str| {
        std::env::var_os(name)
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
    };
    InstallContext {
        home: get("HOME").or_else(|| get("USERPROFILE")),
        install_dir: get(INSTALL_DIR_ENV),
        sandbox: std::env::var_os(setup::SANDBOX_ENV).is_some_and(|v| !v.is_empty() && v != "0"),
    }
}

fn same_dir(a: &Path, b: &Path) -> bool {
    let a = a.canonicalize().unwrap_or_else(|_| a.to_path_buf());
    let b = b.canonicalize().unwrap_or_else(|_| b.to_path_buf());
    a == b
}

/// `Ok` when `exe` may be deleted, otherwise the reason it is kept.
fn binary_verdict(exe: &Path, ctx: &InstallContext) -> std::result::Result<(), String> {
    let name = exe.file_name().and_then(|n| n.to_str()).unwrap_or("");
    if name != BINARY_NAME && name != format!("{BINARY_NAME}.exe") {
        return Err(format!("{} is not named {BINARY_NAME}", exe.display()));
    }
    if exe.components().any(|c| c.as_os_str() == "target") {
        return Err("it is inside a target/ build directory".to_string());
    }
    let Some(parent) = exe.parent() else {
        return Err("it has no parent directory".to_string());
    };
    let mut known: Vec<PathBuf> = Vec::new();
    known.extend(ctx.install_dir.clone());
    if let Some(home) = &ctx.home {
        known.push(home.join(".local").join("bin"));
        known.push(home.join(".cargo").join("bin"));
    }
    if !ctx.sandbox {
        known.push(PathBuf::from("/usr/local/bin"));
    }
    if known.iter().any(|dir| same_dir(dir, parent)) {
        Ok(())
    } else {
        Err(format!(
            "{} is not a known install location",
            parent.display()
        ))
    }
}

fn binary_step(args: &UninstallArgs, ctx: &Context) -> Step {
    const NAME: &str = "binary";
    if args.keep_binary {
        return Step::skipped(NAME, "kept (--keep-binary)");
    }
    let exe = match std::env::current_exe() {
        Ok(exe) => exe,
        Err(err) => return Step::failed(NAME, format!("cannot locate the running binary: {err}")),
    };
    if let Err(reason) = binary_verdict(&exe, &install_context()) {
        return Step::skipped(NAME, format!("kept {} ({reason})", exe.display()));
    }
    if !exe.exists() {
        return Step::ok(NAME, format!("{} is already gone", exe.display()));
    }
    if ctx.dry_run {
        return Step::ok(NAME, format!("would delete {}", exe.display()));
    }
    match std::fs::remove_file(&exe) {
        Ok(()) => Step::ok(NAME, format!("deleted {}", exe.display())),
        Err(err) => Step::failed(NAME, format!("{}: {err}", exe.display())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx(home: &str, install: Option<&str>, sandbox: bool) -> InstallContext {
        InstallContext {
            home: Some(PathBuf::from(home)),
            install_dir: install.map(PathBuf::from),
            sandbox,
        }
    }

    #[test]
    fn confirmation_answers() {
        for yes in ["y", "Y", "yes", " YES \n"] {
            assert!(is_yes(yes), "{yes:?}");
        }
        for no in ["", "n", "no", "\n", "yep", "ok"] {
            assert!(!is_yes(no), "{no:?}");
        }
    }

    #[test]
    fn binary_is_removed_only_from_known_locations() {
        let c = ctx("/home/u", None, false);
        assert!(binary_verdict(Path::new("/home/u/.local/bin/claude-codex"), &c).is_ok());
        assert!(binary_verdict(Path::new("/home/u/.cargo/bin/claude-codex"), &c).is_ok());
        assert!(binary_verdict(Path::new("/usr/local/bin/claude-codex"), &c).is_ok());
        assert!(binary_verdict(Path::new("/opt/bin/claude-codex"), &c).is_err());
        assert!(binary_verdict(Path::new("/home/u/bin/claude-codex"), &c).is_err());
        let c = ctx("/home/u", Some("/opt/bin"), false);
        assert!(binary_verdict(Path::new("/opt/bin/claude-codex"), &c).is_ok());
    }

    #[test]
    fn binary_inside_target_is_never_removed() {
        let c = ctx("/home/u", Some("/work/proj/target/release"), false);
        for exe in [
            "/work/proj/target/release/claude-codex",
            "/home/u/.cargo/bin/target/claude-codex",
            "/home/u/target/.local/bin/claude-codex",
        ] {
            let err = binary_verdict(Path::new(exe), &c).unwrap_err();
            assert!(err.contains("target/"), "{exe}: {err}");
        }
    }

    #[test]
    fn only_a_binary_with_our_name_is_removed() {
        let c = ctx("/home/u", None, false);
        assert!(binary_verdict(Path::new("/home/u/.local/bin/other"), &c).is_err());
        assert!(binary_verdict(Path::new("/home/u/.local/bin/claude"), &c).is_err());
    }

    #[test]
    fn sandbox_ignores_the_system_wide_location() {
        let c = ctx("/tmp/sandbox/home", None, true);
        assert!(binary_verdict(Path::new("/usr/local/bin/claude-codex"), &c).is_err());
        assert!(binary_verdict(Path::new("/tmp/sandbox/home/.local/bin/claude-codex"), &c).is_ok());
    }

    #[test]
    fn protected_directories() {
        let home = PathBuf::from("/home/u");
        assert!(protected_dir(Path::new("/"), Some(&home)));
        assert!(protected_dir(Path::new("/home/u"), Some(&home)));
        assert!(protected_dir(Path::new("/home/u/.codex"), Some(&home)));
        assert!(protected_dir(
            Path::new("/home/u/.codex/proxy"),
            Some(&home)
        ));
        assert!(!protected_dir(
            Path::new("/home/u/.config/claude-code-proxy"),
            Some(&home)
        ));
    }
}
