//! Autostart service management: `claude-codex service <install|uninstall|status|restart>`.
//!
//! Linux uses a systemd user unit, macOS a LaunchAgent. Windows is unsupported.
//! Content generation is pure so it can be tested without touching the real
//! home directory. Setting `CCP_SERVICE_DIR` redirects the unit/plist to that
//! directory and skips every call to systemctl/launchctl/loginctl (file-only
//! mode, used by tests).

use anyhow::{Context, Result, bail};
use clap::Subcommand;
use std::path::{Path, PathBuf};
use std::process::Command;

pub const SERVICE_NAME: &str = "claude-codex";
pub const LAUNCHD_LABEL: &str = "com.claude-codex.proxy";
/// Overrides the directory where the unit/plist is written; implies file-only mode.
pub const SERVICE_DIR_ENV: &str = "CCP_SERVICE_DIR";

#[derive(Debug, Clone, Copy, Subcommand)]
pub enum ServiceCommand {
    /// Install and start the autostart service
    Install,
    /// Stop and disable the autostart service but keep its unit file
    Disable,
    /// Stop and remove the autostart service
    Uninstall,
    /// Show whether the service is installed and running
    Status,
    /// Restart the running service
    Restart,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Platform {
    Linux,
    MacOs,
    Other,
}

impl Platform {
    pub fn current() -> Self {
        match std::env::consts::OS {
            "linux" => Platform::Linux,
            "macos" => Platform::MacOs,
            _ => Platform::Other,
        }
    }
}

/// Everything needed to render a unit or plist.
#[derive(Debug, Clone)]
pub struct ServiceSpec {
    pub binary: PathBuf,
    /// Optional `KEY=VALUE` file inherited by the service when present.
    pub env_file: Option<PathBuf>,
    /// Extra environment pairs baked into the definition.
    pub env: Vec<(String, String)>,
    pub log_file: PathBuf,
}

fn systemd_quote(value: &str) -> String {
    let escaped = value.replace('\\', "\\\\").replace('"', "\\\"");
    format!("\"{escaped}\"")
}

pub fn render_systemd_unit(spec: &ServiceSpec) -> String {
    let mut out = String::new();
    out.push_str("[Unit]\n");
    out.push_str("Description=claude-codex proxy for Claude Code\n");
    out.push_str("After=network-online.target\n");
    out.push_str("Wants=network-online.target\n\n");
    out.push_str("[Service]\n");
    out.push_str("Type=simple\n");
    out.push_str(&format!(
        "ExecStart={} serve --no-monitor\n",
        systemd_quote(&spec.binary.to_string_lossy())
    ));
    out.push_str("Restart=on-failure\n");
    out.push_str("RestartSec=3\n");
    if let Some(file) = &spec.env_file {
        // The leading dash makes the file optional.
        out.push_str(&format!("EnvironmentFile=-{}\n", file.to_string_lossy()));
    }
    for (key, value) in &spec.env {
        out.push_str(&format!(
            "Environment={}\n",
            systemd_quote(&format!("{key}={value}"))
        ));
    }
    out.push_str("\n[Install]\nWantedBy=default.target\n");
    out
}

fn xml_escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

pub fn render_launchd_plist(spec: &ServiceSpec) -> String {
    let mut out = String::new();
    out.push_str("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n");
    out.push_str("<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n");
    out.push_str("<plist version=\"1.0\">\n<dict>\n");
    out.push_str(&format!(
        "  <key>Label</key>\n  <string>{LAUNCHD_LABEL}</string>\n"
    ));
    out.push_str("  <key>ProgramArguments</key>\n  <array>\n");
    out.push_str(&format!(
        "    <string>{}</string>\n",
        xml_escape(&spec.binary.to_string_lossy())
    ));
    out.push_str("    <string>serve</string>\n    <string>--no-monitor</string>\n  </array>\n");
    out.push_str("  <key>RunAtLoad</key>\n  <true/>\n");
    out.push_str("  <key>KeepAlive</key>\n  <true/>\n");
    if !spec.env.is_empty() {
        out.push_str("  <key>EnvironmentVariables</key>\n  <dict>\n");
        for (key, value) in &spec.env {
            out.push_str(&format!(
                "    <key>{}</key>\n    <string>{}</string>\n",
                xml_escape(key),
                xml_escape(value)
            ));
        }
        out.push_str("  </dict>\n");
    }
    let log = xml_escape(&spec.log_file.to_string_lossy());
    out.push_str(&format!(
        "  <key>StandardOutPath</key>\n  <string>{log}</string>\n  <key>StandardErrorPath</key>\n  <string>{log}</string>\n"
    ));
    out.push_str("</dict>\n</plist>\n");
    out
}

/// Parses a dotenv-like file: `KEY=VALUE` lines, `#` comments, optional quotes.
pub fn parse_env_file(content: &str) -> Vec<(String, String)> {
    content
        .lines()
        .filter_map(|line| {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                return None;
            }
            let line = line.strip_prefix("export ").unwrap_or(line);
            let (key, value) = line.split_once('=')?;
            let value = value.trim();
            let value = value
                .strip_prefix('"')
                .and_then(|v| v.strip_suffix('"'))
                .or_else(|| value.strip_prefix('\'').and_then(|v| v.strip_suffix('\'')))
                .unwrap_or(value);
            Some((key.trim().to_string(), value.to_string()))
        })
        .collect()
}

pub fn systemd_unit_path(dir: &Path) -> PathBuf {
    dir.join(format!("{SERVICE_NAME}.service"))
}

pub fn launchd_plist_path(dir: &Path) -> PathBuf {
    dir.join(format!("{LAUNCHD_LABEL}.plist"))
}

fn home_dir() -> Result<PathBuf> {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .context("HOME is not set")
}

/// Destination directory and whether service-manager calls are allowed.
fn target_dir(platform: Platform) -> Result<(PathBuf, bool)> {
    if let Some(dir) = std::env::var_os(SERVICE_DIR_ENV) {
        return Ok((PathBuf::from(dir), false));
    }
    let dir = match platform {
        Platform::Linux => {
            let base = match std::env::var_os("XDG_CONFIG_HOME") {
                Some(xdg) if !xdg.is_empty() => PathBuf::from(xdg),
                _ => home_dir()?.join(".config"),
            };
            base.join("systemd").join("user")
        }
        Platform::MacOs => home_dir()?.join("Library").join("LaunchAgents"),
        Platform::Other => bail!(unsupported()),
    };
    Ok((dir, true))
}

/// Path of the unit/plist that `service install` writes (honors `CCP_SERVICE_DIR`).
pub fn definition_file() -> Result<PathBuf> {
    let platform = Platform::current();
    let (dir, _) = target_dir(platform)?;
    Ok(definition_path(platform, &dir))
}

fn unsupported() -> String {
    format!(
        "service autostart is not supported on {} (only Linux/systemd and macOS/launchd)",
        std::env::consts::OS
    )
}

fn build_spec() -> Result<ServiceSpec> {
    let binary = std::env::current_exe().context("cannot resolve current executable")?;
    let env_path = crate::paths::config_dir().join("service.env");
    let mut env = Vec::new();
    if let Some(dir) = std::env::var_os("CCP_CONFIG_DIR") {
        env.push((
            "CCP_CONFIG_DIR".to_string(),
            dir.to_string_lossy().into_owned(),
        ));
    }
    let env_file = env_path.is_file().then_some(env_path);
    Ok(ServiceSpec {
        binary,
        env_file,
        env,
        log_file: crate::paths::log_file(),
    })
}

fn run(program: &str, args: &[&str]) -> Result<std::process::Output> {
    Command::new(program)
        .args(args)
        .output()
        .with_context(|| format!("failed to run `{program} {}`", args.join(" ")))
}

fn run_checked(program: &str, args: &[&str]) -> Result<()> {
    let out = run(program, args)?;
    if !out.status.success() {
        bail!(
            "`{program} {}` failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(())
}

fn succeeds(program: &str, args: &[&str]) -> bool {
    run(program, args)
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn launchd_domain() -> Result<String> {
    let out = run("id", &["-u"])?;
    Ok(format!(
        "gui/{}",
        String::from_utf8_lossy(&out.stdout).trim()
    ))
}

pub fn linger_enabled() -> Option<bool> {
    let user = std::env::var("USER").ok()?;
    let out = run("loginctl", &["show-user", &user, "-p", "Linger"]).ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).trim() == "Linger=yes")
}

pub fn run_service_command(command: ServiceCommand) -> Result<()> {
    let platform = Platform::current();
    if platform == Platform::Other {
        bail!(unsupported());
    }
    let (dir, manage) = target_dir(platform)?;
    match command {
        ServiceCommand::Install => install(platform, &dir, manage, true),
        ServiceCommand::Disable => disable(platform, &dir, manage),
        ServiceCommand::Uninstall => uninstall(platform, &dir, manage),
        ServiceCommand::Status => status(platform, &dir, manage),
        ServiceCommand::Restart => restart(platform, &dir, manage),
    }
}

fn definition_path(platform: Platform, dir: &Path) -> PathBuf {
    match platform {
        Platform::MacOs => launchd_plist_path(dir),
        _ => systemd_unit_path(dir),
    }
}

/// Install and start the service without the linger hint: `setup` and `enable`
/// handle linger themselves right after this.
pub fn install_quiet() -> Result<()> {
    let platform = Platform::current();
    if platform == Platform::Other {
        bail!(unsupported());
    }
    let (dir, manage) = target_dir(platform)?;
    install(platform, &dir, manage, false)
}

fn install(platform: Platform, dir: &Path, manage: bool, linger_hint: bool) -> Result<()> {
    let spec = build_spec()?;
    let path = definition_path(platform, dir);
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let content = match platform {
        Platform::MacOs => {
            let mut spec = spec;
            if let Some(file) = spec.env_file.take()
                && let Ok(text) = std::fs::read_to_string(&file)
            {
                spec.env.extend(parse_env_file(&text));
            }
            if let Some(parent) = spec.log_file.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            render_launchd_plist(&spec)
        }
        _ => render_systemd_unit(&spec),
    };
    std::fs::write(&path, content).with_context(|| format!("writing {}", path.display()))?;
    println!("Wrote {}", path.display());

    if !manage {
        println!("{SERVICE_DIR_ENV} is set: skipped service manager calls");
        return Ok(());
    }
    match platform {
        Platform::Linux => {
            run_checked("systemctl", &["--user", "daemon-reload"])?;
            run_checked(
                "systemctl",
                &[
                    "--user",
                    "enable",
                    "--now",
                    &format!("{SERVICE_NAME}.service"),
                ],
            )?;
            if linger_hint && linger_enabled() == Some(false) {
                println!(
                    "Linger is off: the service starts at login, not at boot.\n\
                     `claude-codex enable` turns it on, or run: loginctl enable-linger"
                );
            }
        }
        Platform::MacOs => {
            let domain = launchd_domain()?;
            let _ = run(
                "launchctl",
                &["bootout", &format!("{domain}/{LAUNCHD_LABEL}")],
            );
            run_checked(
                "launchctl",
                &["bootstrap", &domain, &path.to_string_lossy()],
            )?;
        }
        Platform::Other => unreachable!(),
    }
    println!("Installed. Port: {}", crate::config::port());
    Ok(())
}

/// Stop and disable the service without deleting its unit file or plist
/// (`systemctl --user disable --now`; `launchctl bootout` on macOS).
fn disable(platform: Platform, dir: &Path, manage: bool) -> Result<()> {
    let path = definition_path(platform, dir);
    if !path.exists() {
        println!("Not installed ({} does not exist)", path.display());
        return Ok(());
    }
    if !manage {
        println!("{SERVICE_DIR_ENV} is set: skipped service manager calls");
        return Ok(());
    }
    match platform {
        Platform::Linux => run_checked(
            "systemctl",
            &[
                "--user",
                "disable",
                "--now",
                &format!("{SERVICE_NAME}.service"),
            ],
        )?,
        Platform::MacOs => {
            let domain = launchd_domain()?;
            // Not loaded is fine: the goal is that it is not running.
            let _ = run(
                "launchctl",
                &["bootout", &format!("{domain}/{LAUNCHD_LABEL}")],
            );
        }
        Platform::Other => unreachable!(),
    }
    println!("Disabled (kept {})", path.display());
    Ok(())
}

fn uninstall(platform: Platform, dir: &Path, manage: bool) -> Result<()> {
    let path = definition_path(platform, dir);
    if manage {
        match platform {
            Platform::Linux => {
                let unit = format!("{SERVICE_NAME}.service");
                let _ = run("systemctl", &["--user", "disable", "--now", &unit]);
            }
            Platform::MacOs => {
                let domain = launchd_domain()?;
                let _ = run(
                    "launchctl",
                    &["bootout", &format!("{domain}/{LAUNCHD_LABEL}")],
                );
            }
            Platform::Other => unreachable!(),
        }
    }
    if path.exists() {
        std::fs::remove_file(&path).with_context(|| format!("removing {}", path.display()))?;
        println!("Removed {}", path.display());
    } else {
        println!("Not installed ({} does not exist)", path.display());
    }
    if manage && platform == Platform::Linux {
        let _ = run("systemctl", &["--user", "daemon-reload"]);
    }
    Ok(())
}

fn is_active(platform: Platform) -> bool {
    match platform {
        Platform::Linux => succeeds(
            "systemctl",
            &[
                "--user",
                "is-active",
                "--quiet",
                &format!("{SERVICE_NAME}.service"),
            ],
        ),
        Platform::MacOs => launchd_domain()
            .map(|d| succeeds("launchctl", &["print", &format!("{d}/{LAUNCHD_LABEL}")]))
            .unwrap_or(false),
        Platform::Other => false,
    }
}

/// What the service manager says about the autostart service.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServiceInfo {
    pub supported: bool,
    pub installed: bool,
    pub file: Option<PathBuf>,
    /// `None` when service manager calls are off (`CCP_SERVICE_DIR`) or fail.
    pub active: Option<bool>,
    pub enabled: Option<bool>,
    pub linger: Option<bool>,
    /// True when `CCP_SERVICE_DIR` turned the service manager calls off.
    pub unmanaged: bool,
}

fn is_enabled(platform: Platform) -> bool {
    match platform {
        Platform::Linux => succeeds(
            "systemctl",
            &[
                "--user",
                "is-enabled",
                "--quiet",
                &format!("{SERVICE_NAME}.service"),
            ],
        ),
        // A LaunchAgent that is bootstrapped is loaded and starts at login.
        Platform::MacOs => is_active(platform),
        Platform::Other => false,
    }
}

/// Read-only look at the service. Never changes anything.
pub fn inspect() -> ServiceInfo {
    let platform = Platform::current();
    let target = if platform == Platform::Other {
        None
    } else {
        target_dir(platform).ok()
    };
    let Some((dir, manage)) = target else {
        return ServiceInfo {
            supported: false,
            installed: false,
            file: None,
            active: None,
            enabled: None,
            linger: None,
            unmanaged: false,
        };
    };
    let path = definition_path(platform, &dir);
    let installed = path.exists();
    ServiceInfo {
        supported: true,
        installed,
        file: Some(path),
        active: manage.then(|| is_active(platform)),
        enabled: (manage && installed).then(|| is_enabled(platform)),
        linger: (manage && platform == Platform::Linux)
            .then(linger_enabled)
            .flatten(),
        unmanaged: !manage,
    }
}

fn status(platform: Platform, dir: &Path, manage: bool) -> Result<()> {
    let path = definition_path(platform, dir);
    let installed = path.exists();
    println!("installed: {}", if installed { "yes" } else { "no" });
    println!("file: {}", path.display());
    if manage {
        println!("active: {}", if is_active(platform) { "yes" } else { "no" });
        if platform == Platform::Linux {
            match linger_enabled() {
                Some(true) => println!("linger: on (starts at boot)"),
                Some(false) => {
                    println!("linger: off (starts at login; run `claude-codex enable`)")
                }
                None => println!("linger: unknown"),
            }
        }
    } else {
        println!("active: unknown ({SERVICE_DIR_ENV} is set)");
    }
    println!("port: {}", crate::config::port());
    Ok(())
}

fn restart(platform: Platform, dir: &Path, manage: bool) -> Result<()> {
    if !manage {
        bail!("{SERVICE_DIR_ENV} is set: restart needs the real service manager");
    }
    if !definition_path(platform, dir).exists() {
        bail!("service is not installed; run `claude-codex service install`");
    }
    match platform {
        Platform::Linux => run_checked(
            "systemctl",
            &["--user", "restart", &format!("{SERVICE_NAME}.service")],
        )?,
        Platform::MacOs => run_checked(
            "launchctl",
            &[
                "kickstart",
                "-k",
                &format!("{}/{LAUNCHD_LABEL}", launchd_domain()?),
            ],
        )?,
        Platform::Other => unreachable!(),
    }
    println!("Restarted.");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec() -> ServiceSpec {
        ServiceSpec {
            binary: PathBuf::from("/opt/my bin/claude-codex"),
            env_file: Some(PathBuf::from("/cfg/service.env")),
            env: vec![("CCP_CONFIG_DIR".into(), "/cfg".into())],
            log_file: PathBuf::from("/state/proxy.log"),
        }
    }

    #[test]
    fn systemd_unit_has_expected_fields() {
        let unit = render_systemd_unit(&spec());
        assert!(unit.contains("ExecStart=\"/opt/my bin/claude-codex\" serve --no-monitor\n"));
        assert!(unit.contains("Restart=on-failure"));
        assert!(unit.contains("WantedBy=default.target"));
        assert!(unit.contains("EnvironmentFile=-/cfg/service.env"));
        assert!(unit.contains("Environment=\"CCP_CONFIG_DIR=/cfg\""));
    }

    #[test]
    fn systemd_unit_omits_optional_env() {
        let mut s = spec();
        s.env_file = None;
        s.env.clear();
        let unit = render_systemd_unit(&s);
        assert!(!unit.contains("EnvironmentFile"));
        assert!(!unit.contains("Environment="));
    }

    #[test]
    fn launchd_plist_has_expected_fields() {
        let mut s = spec();
        s.binary = PathBuf::from("/a&b/claude-codex");
        let plist = render_launchd_plist(&s);
        assert!(plist.contains("<string>com.claude-codex.proxy</string>"));
        assert!(plist.contains("<string>/a&amp;b/claude-codex</string>"));
        assert!(plist.contains("<string>--no-monitor</string>"));
        assert!(plist.contains("<key>RunAtLoad</key>\n  <true/>"));
        assert!(plist.contains("<key>KeepAlive</key>\n  <true/>"));
        assert!(plist.contains("<key>CCP_CONFIG_DIR</key>"));
    }

    #[test]
    fn env_file_parsing() {
        let parsed = parse_env_file("# c\n\nPORT=1234\nexport A=\"x y\"\nB='z'\nbad\n");
        assert_eq!(
            parsed,
            vec![
                ("PORT".into(), "1234".into()),
                ("A".into(), "x y".into()),
                ("B".into(), "z".into())
            ]
        );
    }
}
