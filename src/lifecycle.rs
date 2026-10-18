//! `claude-codex enable`, `disable` and `status`.
//!
//! * `enable` is idempotent: it is `setup` under another name (service installed,
//!   enabled and started; cards synced; `ANTHROPIC_BASE_URL` merged into Claude
//!   Code's `settings.json` with the same safety rules) plus removing the
//!   `disabled` marker.
//! * `disable` switches the proxy off without uninstalling it: the service is
//!   stopped and disabled (its unit file stays), the managed agent cards go, and
//!   the `settings.json` keys that setup added are removed again (the previous
//!   value is restored when there was one). The last part is not optional:
//!   leaving `ANTHROPIC_BASE_URL` pointing at a stopped proxy would break Claude
//!   Code. The binary, credentials, caches and `setup.json` stay. A `disabled`
//!   marker in the state directory stops `serve` from syncing cards meanwhile.
//! * `status` prints one screen and changes nothing.
//!
//! Like `setup`, every command here resolves its locations through
//! [`setup::Locations`], so the sandbox guard (`CCP_SETUP_SANDBOX`) applies.

use crate::setup::{self, BASE_URL_KEY, Locations, Report, SetupArgs, SetupState};
use crate::{agent_cards, config, paths, service, uninstall};
use anyhow::{Context, Result, bail};
use clap::Args;
use serde_json::Value;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::PathBuf;
use std::time::Duration;

/// File in the state directory that marks the proxy as disabled.
pub const DISABLED_MARKER_FILE: &str = "disabled";
const PROBE_TIMEOUT: Duration = Duration::from_millis(700);

#[derive(Debug, Clone, Args)]
pub struct EnableArgs {
    /// Show what would change without changing anything
    #[arg(long)]
    pub dry_run: bool,
    /// Overwrite an ANTHROPIC_BASE_URL that points somewhere else
    #[arg(long)]
    pub force: bool,
}

#[derive(Debug, Clone, Args)]
pub struct DisableArgs {
    /// Show what would change without changing anything
    #[arg(long)]
    pub dry_run: bool,
}

// ---------- the disabled marker ----------

pub fn marker_path() -> PathBuf {
    paths::state_dir().join(DISABLED_MARKER_FILE)
}

/// True while `claude-codex disable` is in effect.
pub fn is_disabled() -> bool {
    marker_path().exists()
}

fn write_marker() -> Result<()> {
    let path = marker_path();
    setup::write_atomic(
        &path,
        "claude-codex is disabled. `claude-codex enable` removes this file.\n",
    )
}

pub(crate) fn clear_disabled_marker() -> Result<()> {
    let path = marker_path();
    match std::fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err).with_context(|| format!("removing {}", path.display())),
    }
}

// ---------- enable ----------

pub fn run_enable(args: EnableArgs) -> Result<()> {
    let setup_args = SetupArgs {
        dry_run: args.dry_run,
        no_service: false,
        no_agents: false,
        no_claude_settings: false,
        force: args.force,
        undo: false,
    };
    setup::run_setup_as(setup_args, "Enable", "enable")
}

// ---------- disable ----------

fn proxy_url() -> String {
    format!("http://127.0.0.1:{}", config::port())
}

/// Remove from `settings.json` what setup added. Returns once the file is
/// consistent; the state is only cleared by the caller after a real write.
fn disable_settings(
    loc: &Locations,
    state: &SetupState,
    dry_run: bool,
    report: &mut Report,
) -> Result<()> {
    let prefix = if dry_run { "would " } else { "" };
    let path = state
        .settings_file
        .as_deref()
        .map(PathBuf::from)
        .unwrap_or_else(|| loc.settings_file.clone());
    let file = setup::read_settings(&path)?;
    if !file.existed {
        report.done.push(format!(
            "{} does not exist; nothing to revert",
            path.display()
        ));
        return Ok(());
    }
    let mut value = file.value;
    if state.added.is_empty() && state.replaced.is_empty() {
        report.done.push(format!(
            "no settings entries recorded by setup; {} left as is",
            path.display()
        ));
    } else {
        let notes = setup::unmerge_env(&mut value, state)?;
        let nothing_left = value.as_object().is_some_and(|o| o.is_empty());
        if state.settings_created && nothing_left {
            // Setup created this file and nothing else lives in it.
            if !dry_run {
                std::fs::remove_file(&path)
                    .with_context(|| format!("removing {}", path.display()))?;
            }
            report.done.push(format!(
                "{prefix}removed {} (setup had created it)",
                path.display()
            ));
        } else {
            let text = uninstall::restored_text(&value, &file.indent, state);
            let current = std::fs::read_to_string(&path)
                .with_context(|| format!("reading {}", path.display()))?;
            if text != current && !dry_run {
                setup::write_atomic(&path, &text)?;
            }
            report
                .done
                .push(format!("{prefix}reverted {}", path.display()));
        }
        for note in notes {
            report.done.push(format!("{prefix}{note}"));
        }
    }
    // A base URL for this proxy that setup did not write (the user set it) is
    // not ours to remove, but it breaks Claude Code while the proxy is off.
    if let Some(current) = value
        .get("env")
        .and_then(|env| env.get(BASE_URL_KEY))
        .and_then(Value::as_str)
        && setup::same_base_url(current, &proxy_url())
    {
        report.pending.push(format!(
            "{} still sets {BASE_URL_KEY}={current}, which points at this proxy. claude-codex did not add it, so it left it; remove it yourself or Claude Code will fail while the proxy is off",
            path.display()
        ));
    }
    Ok(())
}

pub fn run_disable(args: DisableArgs) -> Result<()> {
    let loc = Locations::resolve()?;
    let dry_run = args.dry_run;
    let prefix = if dry_run { "would " } else { "" };
    let mut report = Report::default();
    if dry_run {
        println!("Dry run: nothing will be changed.");
    }
    let state_existed = loc.state_file.exists();
    let (mut state, state_ok) = match setup::load_state(&loc.state_file) {
        Ok(state) => (state, true),
        Err(err) => {
            report.failed.push(format!(
                "setup record: {err:#}. Without it the settings.json entries cannot be reverted safely"
            ));
            (SetupState::default(), false)
        }
    };

    // 1. The marker first, so nothing recreates the cards behind our back.
    if dry_run {
        report
            .done
            .push(format!("would write {}", marker_path().display()));
    } else {
        match write_marker() {
            Ok(()) => report.done.push(format!(
                "disabled marker written ({})",
                marker_path().display()
            )),
            Err(err) => report.failed.push(format!("disabled marker: {err:#}")),
        }
    }

    // 2. settings.json: take the proxy out of Claude Code's way.
    let mut settings_reverted = false;
    if state_ok {
        match disable_settings(&loc, &state, dry_run, &mut report) {
            Ok(()) => settings_reverted = true,
            Err(err) => report.failed.push(format!("Claude Code settings: {err:#}")),
        }
    }

    // 3. Agent cards.
    let cards = agent_cards::agents_dir()
        .and_then(|dir| agent_cards::clean_once(&dir, dry_run).map(|actions| (dir, actions)));
    match cards {
        Ok((dir, actions)) => report.done.push(format!(
            "{prefix}removed {} managed agent card(s) from {}",
            actions.len(),
            dir.display()
        )),
        Err(err) => report.failed.push(format!("agent cards: {err:#}")),
    }

    // 4. Service: stop and disable, keep the unit file.
    if service::Platform::current() == service::Platform::Other {
        report
            .done
            .push("no autostart service on this OS; nothing to stop".into());
    } else if dry_run {
        report
            .done
            .push("would stop and disable the autostart service (the unit file stays)".into());
    } else {
        match service::run_service_command(service::ServiceCommand::Disable) {
            Ok(()) => report
                .done
                .push("autostart service stopped and disabled (unit file kept)".into()),
            Err(err) => report.failed.push(format!("service: {err:#}")),
        }
    }

    // 5. Forget the entries that were reverted; keep the record itself.
    if !dry_run && settings_reverted && state_existed {
        state.added.clear();
        state.replaced.clear();
        state.env_created = false;
        if let Err(err) = setup::save_state(&loc.state_file, &state) {
            report.failed.push(format!("setup record: {err:#}"));
        }
    }

    // 6. A `serve` started by hand is not ours to stop.
    if !dry_run && listener_present(config::port()) {
        report.pending.push(format!(
            "Something is still listening on 127.0.0.1:{} (a manual `claude-codex serve`?). It was not stopped; stop it yourself",
            config::port()
        ));
    }

    report.pending.push(
        "Restart Claude Code so it stops using the proxy. To switch back, run `claude-codex enable` and restart Claude Code again"
            .into(),
    );
    report.print(&if dry_run {
        "Disable (dry run):".to_string()
    } else {
        "Disable finished:".to_string()
    });
    if report.failed.is_empty() {
        Ok(())
    } else {
        bail!(
            "disable finished with {} failed step(s); fix the cause and rerun `claude-codex disable`",
            report.failed.len()
        )
    }
}

// ---------- status ----------

fn probe_addr(port: u16) -> SocketAddr {
    SocketAddr::from(([127, 0, 0, 1], port))
}

/// True when something accepts connections on the proxy port.
fn listener_present(port: u16) -> bool {
    TcpStream::connect_timeout(&probe_addr(port), PROBE_TIMEOUT).is_ok()
}

/// True when `GET /healthz` on the proxy port answers 200.
fn healthz_ok(port: u16) -> bool {
    let Ok(mut stream) = TcpStream::connect_timeout(&probe_addr(port), PROBE_TIMEOUT) else {
        return false;
    };
    let _ = stream.set_read_timeout(Some(PROBE_TIMEOUT));
    let _ = stream.set_write_timeout(Some(PROBE_TIMEOUT));
    let request =
        format!("GET /healthz HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n");
    if stream.write_all(request.as_bytes()).is_err() {
        return false;
    }
    let mut head = [0u8; 256];
    let n = stream.read(&mut head).unwrap_or(0);
    String::from_utf8_lossy(&head[..n])
        .lines()
        .next()
        .is_some_and(|line| line.contains(" 200"))
}

/// What `settings.json` says about the base URL.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BaseUrl {
    NoFile,
    NotSet,
    /// Points at this proxy.
    Ours(String),
    Elsewhere(String),
    Unreadable(String),
}

/// Everything `status` prints, gathered first so the text is pure.
#[derive(Debug, Clone)]
pub struct StatusReport {
    pub version: String,
    pub disabled: bool,
    pub marker: PathBuf,
    pub service: service::ServiceInfo,
    pub port: u16,
    pub listening: bool,
    pub responds: bool,
    pub settings_file: PathBuf,
    pub base_url: BaseUrl,
    pub cards: Result<usize, String>,
    pub cards_dir: Option<PathBuf>,
    pub codex_login: bool,
}

fn yes_no(value: bool) -> &'static str {
    if value { "yes" } else { "no" }
}

fn tri(value: Option<bool>) -> &'static str {
    match value {
        Some(true) => "yes",
        Some(false) => "no",
        None => "unknown",
    }
}

pub fn render_status(report: &StatusReport) -> String {
    let mut out = String::new();
    let mut line = |label: &str, text: String| {
        out.push_str(&format!("{label:<14}{text}\n"));
    };
    line("claude-codex", report.version.clone());
    line(
        "state",
        if report.disabled {
            format!("disabled (marker {})", report.marker.display())
        } else {
            "enabled".to_string()
        },
    );
    let svc = &report.service;
    line(
        "service",
        if !svc.supported {
            "not supported on this OS".to_string()
        } else if svc.unmanaged {
            format!(
                "installed: {}, service manager calls are off (CCP_SERVICE_DIR is set)",
                yes_no(svc.installed)
            )
        } else {
            format!(
                "installed: {}, active: {}, enabled: {}",
                yes_no(svc.installed),
                tri(svc.active),
                tri(svc.enabled)
            )
        },
    );
    if let Some(file) = &svc.file {
        line("unit file", file.display().to_string());
    }
    if cfg!(target_os = "linux") && !svc.unmanaged && svc.supported {
        line(
            "linger",
            match svc.linger {
                Some(true) => "on (starts at boot)".to_string(),
                Some(false) => {
                    "off (starts at login; sudo loginctl enable-linger $USER)".to_string()
                }
                None => "unknown".to_string(),
            },
        );
    }
    line(
        "proxy",
        format!(
            "port {}, listening: {}, responds: {}",
            report.port,
            yes_no(report.listening),
            yes_no(report.responds)
        ),
    );
    line(
        "Claude Code",
        match &report.base_url {
            BaseUrl::NoFile => format!(
                "{} does not exist; {BASE_URL_KEY} is not set",
                report.settings_file.display()
            ),
            BaseUrl::NotSet => format!(
                "{BASE_URL_KEY} is not set in {}",
                report.settings_file.display()
            ),
            BaseUrl::Ours(url) => format!(
                "{BASE_URL_KEY}={url} (this proxy) in {}",
                report.settings_file.display()
            ),
            BaseUrl::Elsewhere(url) => format!(
                "{BASE_URL_KEY}={url} (not this proxy) in {}",
                report.settings_file.display()
            ),
            BaseUrl::Unreadable(why) => {
                format!("{} is unreadable: {why}", report.settings_file.display())
            }
        },
    );
    line(
        "agent cards",
        match (&report.cards, &report.cards_dir) {
            (Ok(n), Some(dir)) => format!("{n} managed in {}", dir.display()),
            (Ok(n), None) => format!("{n} managed"),
            (Err(why), _) => format!("unknown ({why})"),
        },
    );
    line(
        "Codex login",
        if report.codex_login {
            "found".to_string()
        } else {
            "not found (run `codex login`)".to_string()
        },
    );

    let ours = matches!(report.base_url, BaseUrl::Ours(_));
    let mut hints = Vec::new();
    if report.disabled && ours {
        hints.push(
            "disabled, but Claude Code is still pointed at the proxy: run `claude-codex disable` again or remove the setting",
        );
    }
    if !report.disabled && !ours && matches!(report.base_url, BaseUrl::NotSet | BaseUrl::NoFile) {
        hints.push("Claude Code is not pointed at the proxy: run `claude-codex enable`");
    }
    if !report.disabled
        && !report.service.unmanaged
        && report.service.supported
        && (!report.service.installed
            || report.service.active == Some(false)
            || report.service.enabled == Some(false))
    {
        hints.push("the service is not installed, enabled and running: run `claude-codex enable`");
    }
    for hint in hints {
        out.push_str(&format!("\nhint: {hint}\n"));
    }
    out
}

fn gather_status(loc: &Locations) -> StatusReport {
    let port = config::port();
    let base_url = match setup::read_settings(&loc.settings_file) {
        Err(err) => BaseUrl::Unreadable(format!("{err:#}")),
        Ok(file) if !file.existed => BaseUrl::NoFile,
        Ok(file) => match file
            .value
            .get("env")
            .and_then(|env| env.get(BASE_URL_KEY))
            .and_then(Value::as_str)
        {
            None => BaseUrl::NotSet,
            Some(url) if setup::same_base_url(url, &proxy_url()) => BaseUrl::Ours(url.to_string()),
            Some(url) => BaseUrl::Elsewhere(url.to_string()),
        },
    };
    let cards_dir = agent_cards::agents_dir().ok();
    let cards = match &cards_dir {
        Some(dir) => agent_cards::scan(dir)
            .map(|(managed, _)| managed.len())
            .map_err(|err| format!("{err:#}")),
        None => Err("no home directory".to_string()),
    };
    let listening = listener_present(port);
    StatusReport {
        version: env!("CARGO_PKG_VERSION").to_string(),
        disabled: is_disabled(),
        marker: marker_path(),
        service: service::inspect(),
        port,
        listening,
        responds: listening && healthz_ok(port),
        settings_file: loc.settings_file.clone(),
        base_url,
        cards,
        cards_dir,
        codex_login: codex_login_present(),
    }
}

fn codex_login_present() -> bool {
    crate::providers::codex::auth::token_store::file_store()
        .load_auth()
        .is_ok_and(|auth| auth.is_some())
}

pub fn run_status() -> Result<()> {
    let loc = Locations::resolve()?;
    print!("{}", render_status(&gather_status(&loc)));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn report() -> StatusReport {
        StatusReport {
            version: "1.2.3".into(),
            disabled: false,
            marker: PathBuf::from("/state/disabled"),
            service: service::ServiceInfo {
                supported: true,
                installed: true,
                file: Some(PathBuf::from("/units/claude-codex.service")),
                active: Some(true),
                enabled: Some(true),
                linger: Some(true),
                unmanaged: false,
            },
            port: 18765,
            listening: true,
            responds: true,
            settings_file: PathBuf::from("/c/settings.json"),
            base_url: BaseUrl::Ours("http://127.0.0.1:18765".into()),
            cards: Ok(28),
            cards_dir: Some(PathBuf::from("/c/agents")),
            codex_login: true,
        }
    }

    #[test]
    fn status_text_covers_every_field() {
        let text = render_status(&report());
        for needle in [
            "1.2.3",
            "enabled",
            "installed: yes, active: yes, enabled: yes",
            "/units/claude-codex.service",
            "port 18765, listening: yes, responds: yes",
            "ANTHROPIC_BASE_URL=http://127.0.0.1:18765 (this proxy)",
            "28 managed in /c/agents",
            "found",
        ] {
            assert!(text.contains(needle), "{needle}\n{text}");
        }
        assert!(!text.contains("hint:"), "{text}");
    }

    #[test]
    fn status_flags_a_disabled_state_and_inconsistencies() {
        let mut r = report();
        r.disabled = true;
        r.service.active = Some(false);
        r.service.enabled = Some(false);
        r.listening = false;
        r.responds = false;
        r.cards = Ok(0);
        r.codex_login = false;
        let text = render_status(&r);
        assert!(text.contains("disabled (marker /state/disabled)"), "{text}");
        assert!(text.contains("active: no, enabled: no"), "{text}");
        assert!(text.contains("listening: no, responds: no"), "{text}");
        assert!(text.contains("not found"), "{text}");
        assert!(text.contains("still pointed at the proxy"), "{text}");

        let mut r = report();
        r.base_url = BaseUrl::NotSet;
        r.service.installed = false;
        let text = render_status(&r);
        assert!(text.contains("is not set"), "{text}");
        assert!(text.contains("not pointed at the proxy"), "{text}");
        assert!(text.contains("service is not installed"), "{text}");
    }

    #[test]
    fn unmanaged_service_reports_that_manager_calls_are_off() {
        let mut r = report();
        r.service.unmanaged = true;
        r.service.active = None;
        r.service.enabled = None;
        let text = render_status(&r);
        assert!(text.contains("service manager calls are off"), "{text}");
        assert!(!text.contains("linger"), "{text}");
    }
}
