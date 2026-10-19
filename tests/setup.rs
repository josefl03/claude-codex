//! Integration tests for `claude-codex setup`.
//!
//! Every invocation goes through `Sandbox::cmd`, the only place a Command is
//! built. It sets `CCP_SETUP_SANDBOX=1` and every redirected location, so the
//! binary itself refuses to run if any of them were ever missing.

use assert_cmd::Command;
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use tempfile::TempDir;

struct Sandbox {
    root: TempDir,
    /// A free local port, so no test ever probes the proxy on the default port.
    port: u16,
}

impl Sandbox {
    fn new() -> Self {
        // Tests run in parallel and a port is released before the test binds it
        // again, so never hand the same port to two sandboxes of this process.
        static USED: std::sync::Mutex<Vec<u16>> = std::sync::Mutex::new(Vec::new());
        let port = loop {
            let port = std::net::TcpListener::bind("127.0.0.1:0")
                .unwrap()
                .local_addr()
                .unwrap()
                .port();
            let mut used = USED.lock().unwrap_or_else(|e| e.into_inner());
            if !used.contains(&port) {
                used.push(port);
                break port;
            }
        };
        Self {
            root: tempfile::tempdir().unwrap(),
            port,
        }
    }
    /// A sandbox whose port is already bound by the returned listener, so no
    /// parallel test can grab it between choosing and binding.
    fn with_listener() -> (Self, std::net::TcpListener) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        (
            Self {
                root: tempfile::tempdir().unwrap(),
                port,
            },
            listener,
        )
    }
    fn url(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }
    fn p(&self, name: &str) -> PathBuf {
        self.root.path().join(name)
    }
    fn settings(&self) -> PathBuf {
        self.p("claude/settings.json")
    }
    fn cmd(&self, args: &[&str]) -> Command {
        self.cmd_for(Command::cargo_bin("claude-codex").unwrap(), args)
    }
    /// Same sandbox, but a different executable (a copy of the binary placed
    /// where `uninstall` may delete it).
    fn cmd_with_binary(&self, binary: &Path, args: &[&str]) -> Command {
        self.cmd_for(Command::new(binary), args)
    }
    /// The only place a Command gets its environment.
    fn cmd_for(&self, mut cmd: Command, args: &[&str]) -> Command {
        cmd.env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env("CCP_SETUP_SANDBOX", "1")
            .env("HOME", self.p("home"))
            .env("XDG_STATE_HOME", self.p("state"))
            .env("XDG_CONFIG_HOME", self.p("xdgconfig"))
            .env("CCP_CONFIG_DIR", self.p("cfg"))
            .env("CCP_SERVICE_DIR", self.p("service"))
            .env("CCP_AGENTS_DIR", self.p("agents"))
            .env("CLAUDE_CONFIG_DIR", self.p("claude"))
            .env("CCP_CODEX_AUTH_FILE", self.p("codex-auth.json"))
            .env("CCP_AGENT_CARDS", "0")
            .env("PORT", self.port.to_string())
            .args(args);
        cmd
    }
    fn run(&self, args: &[&str]) -> (bool, String) {
        let out = self.cmd(args).output().unwrap();
        let text = format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        (out.status.success(), text)
    }
    fn read_settings(&self) -> Value {
        serde_json::from_str(&std::fs::read_to_string(self.settings()).unwrap()).unwrap()
    }
    fn write_settings(&self, text: &str) {
        std::fs::create_dir_all(self.p("claude")).unwrap();
        std::fs::write(self.settings(), text).unwrap();
    }
    fn state_file(&self) -> PathBuf {
        self.p("state/claude-code-proxy/setup.json")
    }
    fn tree(&self, dir: &str) -> Vec<String> {
        let mut out = Vec::new();
        fn walk(base: &Path, dir: &Path, out: &mut Vec<String>) {
            if let Ok(rd) = std::fs::read_dir(dir) {
                for e in rd.flatten() {
                    let path = e.path();
                    if path.is_dir() {
                        walk(base, &path, out);
                    } else {
                        out.push(path.strip_prefix(base).unwrap().display().to_string());
                    }
                }
            }
        }
        walk(&self.p(dir), &self.p(dir), &mut out);
        out.sort();
        out
    }
}

#[test]
fn refuses_to_run_when_a_sandbox_variable_is_missing() {
    let sb = Sandbox::new();
    let mut cmd = sb.cmd(&["setup"]);
    cmd.env_remove("CLAUDE_CONFIG_DIR");
    let out = cmd.output().unwrap();
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("CLAUDE_CONFIG_DIR"), "{err}");
    assert!(!sb.settings().exists());
}

#[test]
fn fresh_setup_configures_everything() {
    let sb = Sandbox::new();
    let (ok, out) = sb.run(&["setup"]);
    assert!(ok, "{out}");
    assert!(out.contains("Setup finished"), "{out}");
    assert!(out.contains("Restart Claude Code"), "{out}");
    assert!(out.contains("codex login"), "{out}");

    let v = sb.read_settings();
    assert_eq!(v["env"]["ANTHROPIC_BASE_URL"], sb.url().as_str());
    assert!(v["env"].get("ANTHROPIC_API_KEY").is_none());
    assert!(v["env"].get("ANTHROPIC_AUTH_TOKEN").is_none());
    assert!(
        sb.tree("service")
            .iter()
            .any(|f| f.ends_with("claude-codex.service") || f.ends_with(".plist"))
    );
    assert!(sb.tree("agents").iter().any(|f| f.ends_with(".md")));
    assert!(sb.state_file().exists());
}

#[test]
fn setup_is_idempotent() {
    let sb = Sandbox::new();
    sb.write_settings("{\n    \"model\": \"opus\",\n    \"env\": {\"FOO\": \"1\"}\n}\n");
    assert!(sb.run(&["setup"]).0);
    let settings1 = std::fs::read_to_string(sb.settings()).unwrap();
    let state1 = std::fs::read_to_string(sb.state_file()).unwrap();
    let files1 = (sb.tree("service"), sb.tree("agents"), sb.tree("claude"));

    let (ok, out) = sb.run(&["setup"]);
    assert!(ok, "{out}");
    assert_eq!(std::fs::read_to_string(sb.settings()).unwrap(), settings1);
    assert_eq!(std::fs::read_to_string(sb.state_file()).unwrap(), state1);
    assert_eq!(
        files1,
        (sb.tree("service"), sb.tree("agents"), sb.tree("claude"))
    );
    assert!(
        settings1.contains("    \"model\""),
        "indent kept: {settings1}"
    );
    assert!(!out.contains("Restart Claude Code"), "{out}");
}

#[test]
fn backup_is_made_once_with_the_original_content() {
    let sb = Sandbox::new();
    let original = "{\"model\":\"opus\"}\n";
    sb.write_settings(original);
    assert!(sb.run(&["setup", "--no-service", "--no-agents"]).0);
    let bak = sb.p("claude/settings.json.claude-codex.bak");
    assert_eq!(std::fs::read_to_string(&bak).unwrap(), original);
    assert!(
        sb.run(&["setup", "--no-service", "--no-agents", "--force"])
            .0
    );
    assert_eq!(std::fs::read_to_string(&bak).unwrap(), original);
}

#[test]
fn foreign_base_url_is_kept_unless_forced() {
    let sb = Sandbox::new();
    sb.write_settings(r#"{"env":{"ANTHROPIC_BASE_URL":"https://gateway.example"}}"#);
    let (ok, out) = sb.run(&["setup", "--no-service", "--no-agents"]);
    assert!(ok, "{out}");
    assert!(out.contains("--force"), "{out}");
    assert_eq!(
        sb.read_settings()["env"]["ANTHROPIC_BASE_URL"],
        "https://gateway.example"
    );

    assert!(
        sb.run(&["setup", "--no-service", "--no-agents", "--force"])
            .0
    );
    assert_eq!(
        sb.read_settings()["env"]["ANTHROPIC_BASE_URL"],
        sb.url().as_str()
    );

    assert!(sb.run(&["setup", "--undo"]).0);
    assert_eq!(
        sb.read_settings()["env"]["ANTHROPIC_BASE_URL"],
        "https://gateway.example"
    );
}

#[test]
fn invalid_json_fails_clearly_and_leaves_the_file_alone() {
    let sb = Sandbox::new();
    sb.write_settings("{ nope");
    let (ok, out) = sb.run(&["setup", "--no-service", "--no-agents"]);
    assert!(!ok);
    assert!(out.contains("not valid JSON"), "{out}");
    assert_eq!(std::fs::read_to_string(sb.settings()).unwrap(), "{ nope");
    assert!(!sb.p("claude/settings.json.claude-codex.bak").exists());
}

#[test]
fn dry_run_has_no_effects() {
    let sb = Sandbox::new();
    sb.write_settings("{\"a\":1}");
    let (ok, out) = sb.run(&["setup", "--dry-run"]);
    assert!(ok, "{out}");
    assert!(out.contains("would"), "{out}");
    assert_eq!(std::fs::read_to_string(sb.settings()).unwrap(), "{\"a\":1}");
    assert!(sb.tree("service").is_empty());
    assert!(sb.tree("agents").is_empty());
    assert!(sb.tree("state").is_empty());
    assert_eq!(sb.tree("claude"), ["settings.json"]);
}

#[test]
fn skip_flags_skip_their_step() {
    let sb = Sandbox::new();
    assert!(
        sb.run(&[
            "setup",
            "--no-service",
            "--no-agents",
            "--no-claude-settings"
        ])
        .0
    );
    assert!(sb.tree("service").is_empty());
    assert!(sb.tree("agents").is_empty());
    assert!(!sb.settings().exists());
}

#[test]
fn undo_reverts_only_what_setup_did() {
    let sb = Sandbox::new();
    sb.write_settings("{\n  \"model\": \"opus\",\n  \"env\": {\n    \"FOO\": \"1\"\n  }\n}\n");
    std::fs::create_dir_all(sb.p("agents")).unwrap();
    std::fs::write(sb.p("agents/mine.md"), "mine").unwrap();
    assert!(sb.run(&["setup"]).0);

    let (ok, out) = sb.run(&["setup", "--undo"]);
    assert!(ok, "{out}");
    assert_eq!(
        sb.read_settings(),
        json!({"model":"opus","env":{"FOO":"1"}})
    );
    assert!(sb.tree("service").is_empty());
    assert_eq!(sb.tree("agents"), ["mine.md"]);
    assert!(!sb.state_file().exists());

    let (ok, out) = sb.run(&["setup", "--undo"]);
    assert!(ok && out.contains("Nothing to undo"), "{out}");
}

#[test]
fn undo_dry_run_changes_nothing() {
    let sb = Sandbox::new();
    assert!(sb.run(&["setup"]).0);
    let before = (
        sb.tree("service"),
        sb.tree("agents"),
        std::fs::read_to_string(sb.settings()).unwrap(),
    );
    let (ok, out) = sb.run(&["setup", "--undo", "--dry-run"]);
    assert!(ok, "{out}");
    let after = (
        sb.tree("service"),
        sb.tree("agents"),
        std::fs::read_to_string(sb.settings()).unwrap(),
    );
    assert_eq!(before, after);
    assert!(sb.state_file().exists());
}

#[test]
fn undo_leaves_a_value_the_user_changed_since() {
    let sb = Sandbox::new();
    assert!(sb.run(&["setup", "--no-service", "--no-agents"]).0);
    let mut v = sb.read_settings();
    v["env"]["ANTHROPIC_BASE_URL"] = json!("http://changed");
    std::fs::write(sb.settings(), serde_json::to_string(&v).unwrap()).unwrap();
    let (ok, out) = sb.run(&["setup", "--undo"]);
    assert!(ok, "{out}");
    assert_eq!(
        sb.read_settings()["env"]["ANTHROPIC_BASE_URL"],
        "http://changed"
    );
}

// ---------- loopback aliases of the proxy address ----------

#[test]
fn loopback_aliases_of_our_address_count_as_configured() {
    for host in ["localhost", "127.0.0.1", "[::1]"] {
        for slash in ["", "/"] {
            let sb = Sandbox::new();
            let url = format!("http://{host}:{}{slash}", sb.port);
            let original = format!(
                "{{\n  \"model\": \"opus\",\n  \"env\": {{\n    \"ANTHROPIC_BASE_URL\": \"{url}\",\n    \"CLAUDE_CODE_ENABLE_GATEWAY_MODEL_DISCOVERY\": \"1\"\n  }}\n}}\n"
            );
            sb.write_settings(&original);
            let (ok, out) = sb.run(&["setup", "--no-service", "--no-agents"]);
            assert!(ok, "{url}: {out}");
            assert!(out.contains("already configured"), "{url}: {out}");
            assert!(!out.contains("--force"), "{url}: {out}");
            assert!(!out.contains("Restart Claude Code"), "{url}: {out}");
            // Nothing written: same bytes, no backup.
            assert_eq!(
                std::fs::read_to_string(sb.settings()).unwrap(),
                original,
                "{url}"
            );
            assert!(!sb.p("claude/settings.json.claude-codex.bak").exists());

            // Undo must not remove keys setup did not add.
            let (ok, out) = sb.run(&["setup", "--undo"]);
            assert!(ok, "{url}: {out}");
            assert_eq!(
                std::fs::read_to_string(sb.settings()).unwrap(),
                original,
                "{url}"
            );
        }
    }
}

#[test]
fn loopback_url_on_another_port_is_still_a_conflict() {
    let sb = Sandbox::new();
    let other = if sb.port == 9 { 10 } else { 9 };
    let original = format!(r#"{{"env":{{"ANTHROPIC_BASE_URL":"http://localhost:{other}"}}}}"#);
    sb.write_settings(&original);
    let (ok, out) = sb.run(&["setup", "--no-service", "--no-agents"]);
    assert!(ok, "{out}");
    assert!(out.contains("--force"), "{out}");
    assert_eq!(std::fs::read_to_string(sb.settings()).unwrap(), original);
}

// ---------- uninstall ----------

/// Every file under `dir` with its content, for before/after comparisons.
fn snapshot(sb: &Sandbox, dir: &str) -> Vec<(String, Vec<u8>)> {
    sb.tree(dir)
        .into_iter()
        .map(|f| {
            let bytes = std::fs::read(sb.p(dir).join(&f)).unwrap();
            (f, bytes)
        })
        .collect()
}

fn snapshot_all(sb: &Sandbox) -> Vec<(String, Vec<u8>)> {
    let mut all = Vec::new();
    for dir in ["service", "agents", "claude", "state", "cfg", "home"] {
        for (f, b) in snapshot(sb, dir) {
            all.push((format!("{dir}/{f}"), b));
        }
    }
    all
}

fn copy_binary(sb: &Sandbox, relative_dir: &str) -> PathBuf {
    let src = assert_cmd::cargo::cargo_bin("claude-codex");
    let dir = sb.p(relative_dir);
    std::fs::create_dir_all(&dir).unwrap();
    let dst = dir.join("claude-codex");
    std::fs::copy(src, &dst).unwrap();
    dst
}

fn uninstall(sb: &Sandbox, extra: &[&str]) -> (bool, String) {
    let mut args = vec!["uninstall", "--yes", "--keep-binary"];
    args.extend_from_slice(extra);
    sb.run(&args)
}

#[test]
fn setup_then_uninstall_restores_settings_byte_for_byte() {
    for original in [
        // Other keys, a custom indent and an existing env: order and bytes kept.
        "{\n    \"model\": \"opus\",\n    \"env\": {\n        \"FOO\": \"1\"\n    },\n    \"permissions\": {\"allow\": [\"x\"]}\n}\n",
        // Compact, no trailing newline: only the backup can restore these bytes.
        "{\"z\":1,\"model\":\"opus\",\"a\":{\"b\":[1,2]}}",
    ] {
        let sb = Sandbox::new();
        sb.write_settings(original);
        std::fs::create_dir_all(sb.p("agents")).unwrap();
        std::fs::write(sb.p("agents/mine.md"), "my agent").unwrap();
        assert!(sb.run(&["setup"]).0);
        assert_ne!(std::fs::read_to_string(sb.settings()).unwrap(), original);

        let (ok, out) = uninstall(&sb, &[]);
        assert!(ok, "{out}");
        assert!(out.contains("\u{2713} autostart service"), "{out}");
        assert!(out.contains("\u{2713} Claude Code settings"), "{out}");
        assert!(out.contains("backup stays at"), "{out}");
        assert_eq!(std::fs::read_to_string(sb.settings()).unwrap(), original);
        assert!(sb.tree("service").is_empty());
        assert_eq!(sb.tree("agents"), ["mine.md"]);
        assert_eq!(
            std::fs::read_to_string(sb.p("agents/mine.md")).unwrap(),
            "my agent"
        );
        assert!(!sb.state_file().exists());
        // The .bak stays where it was, with the original content.
        assert_eq!(
            std::fs::read_to_string(sb.p("claude/settings.json.claude-codex.bak")).unwrap(),
            original
        );
    }
}

#[test]
fn uninstall_removes_a_settings_file_that_setup_created() {
    let sb = Sandbox::new();
    assert!(!sb.settings().exists());
    assert!(sb.run(&["setup"]).0);
    assert!(sb.settings().exists());
    let (ok, out) = uninstall(&sb, &[]);
    assert!(ok, "{out}");
    assert!(!sb.settings().exists(), "{out}");
    assert!(sb.tree("claude").is_empty());
}

#[test]
fn uninstall_keeps_a_settings_file_the_user_extended_after_setup() {
    let sb = Sandbox::new();
    assert!(sb.run(&["setup"]).0);
    let mut v = sb.read_settings();
    v["model"] = json!("opus");
    std::fs::write(sb.settings(), serde_json::to_string_pretty(&v).unwrap()).unwrap();
    let (ok, out) = uninstall(&sb, &[]);
    assert!(ok, "{out}");
    assert_eq!(sb.read_settings(), json!({"model": "opus"}));
}

#[test]
fn uninstall_restores_a_forced_previous_url() {
    let sb = Sandbox::new();
    sb.write_settings(r#"{"env":{"ANTHROPIC_BASE_URL":"https://gateway.example"}}"#);
    assert!(sb.run(&["setup", "--force"]).0);
    let (ok, out) = uninstall(&sb, &[]);
    assert!(ok, "{out}");
    assert_eq!(
        sb.read_settings()["env"]["ANTHROPIC_BASE_URL"],
        "https://gateway.example"
    );
}

#[test]
fn uninstall_does_not_remove_a_url_setup_did_not_add() {
    let sb = Sandbox::new();
    let original = format!(
        "{{\n  \"env\": {{\n    \"ANTHROPIC_BASE_URL\": \"http://localhost:{}/\"\n  }}\n}}\n",
        sb.port
    );
    sb.write_settings(&original);
    assert!(sb.run(&["setup"]).0);
    let (ok, out) = uninstall(&sb, &[]);
    assert!(ok, "{out}");
    let after = sb.read_settings();
    assert_eq!(
        after["env"]["ANTHROPIC_BASE_URL"],
        format!("http://localhost:{}/", sb.port).as_str()
    );
    assert!(sb.tree("service").is_empty());
    assert!(sb.tree("agents").is_empty());
}

#[test]
fn uninstall_is_idempotent() {
    let sb = Sandbox::new();
    sb.write_settings("{\n  \"model\": \"opus\"\n}\n");
    assert!(sb.run(&["setup"]).0);
    let (ok, out) = uninstall(&sb, &[]);
    assert!(ok, "{out}");
    let after_first = snapshot_all(&sb);
    let (ok, out) = uninstall(&sb, &[]);
    assert!(ok, "second run: {out}");
    assert!(out.contains("not installed"), "{out}");
    assert_eq!(snapshot_all(&sb), after_first);
    let (ok, out) = uninstall(&sb, &["--purge"]);
    assert!(ok, "third run: {out}");
}

#[test]
fn uninstall_dry_run_has_no_effects() {
    let sb = Sandbox::new();
    sb.write_settings("{\"model\":\"opus\"}\n");
    assert!(sb.run(&["setup"]).0);
    std::fs::create_dir_all(sb.p("cfg/codex")).unwrap();
    std::fs::write(sb.p("cfg/codex/auth.json"), "secret").unwrap();
    let bin = copy_binary(&sb, "home/.local/bin");
    let before = snapshot_all(&sb);
    let out = sb
        .cmd_with_binary(&bin, &["uninstall", "--dry-run", "--purge-credentials"])
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&out.stdout).to_string();
    assert!(out.status.success(), "{text}");
    assert!(text.contains("would"), "{text}");
    assert!(text.contains("dry run"), "{text}");
    assert_eq!(snapshot_all(&sb), before);
    assert!(bin.exists());
}

#[test]
fn uninstall_refuses_without_a_terminal_or_yes() {
    let sb = Sandbox::new();
    assert!(sb.run(&["setup"]).0);
    let before = snapshot_all(&sb);
    let (ok, out) = sb.run(&["uninstall", "--keep-binary"]);
    assert!(!ok, "{out}");
    assert!(out.contains("--yes"), "{out}");
    assert!(out.contains("not a terminal"), "{out}");
    assert_eq!(snapshot_all(&sb), before);
}

#[test]
fn uninstall_deletes_the_binary_from_a_known_location() {
    let sb = Sandbox::new();
    let bin = copy_binary(&sb, "home/.local/bin");
    let (ok, out) = {
        let o = sb
            .cmd_with_binary(&bin, &["uninstall", "--yes"])
            .output()
            .unwrap();
        (
            o.status.success(),
            String::from_utf8_lossy(&o.stdout).to_string(),
        )
    };
    assert!(ok, "{out}");
    assert!(!bin.exists(), "{out}");
    assert!(out.contains("deleted"), "{out}");
}

#[test]
fn uninstall_honors_the_install_dir_variable() {
    let sb = Sandbox::new();
    let bin = copy_binary(&sb, "custom-install");
    let mut cmd = sb.cmd_with_binary(&bin, &["uninstall", "--yes"]);
    cmd.env("CLAUDE_CODEX_INSTALL_DIR", sb.p("custom-install"));
    let out = cmd.output().unwrap();
    assert!(out.status.success());
    assert!(!bin.exists());
}

#[test]
fn keep_binary_keeps_it() {
    let sb = Sandbox::new();
    let bin = copy_binary(&sb, "home/.local/bin");
    let o = sb
        .cmd_with_binary(&bin, &["uninstall", "--yes", "--keep-binary"])
        .output()
        .unwrap();
    assert!(o.status.success());
    assert!(bin.exists());
}

#[test]
fn a_binary_under_target_is_never_deleted() {
    let sb = Sandbox::new();
    let bin = copy_binary(&sb, "work/target/release");
    // Even when the directory is declared as the install directory.
    let mut cmd = sb.cmd_with_binary(&bin, &["uninstall", "--yes"]);
    cmd.env("CLAUDE_CODEX_INSTALL_DIR", sb.p("work/target/release"));
    let out = cmd.output().unwrap();
    let text = String::from_utf8_lossy(&out.stdout).to_string();
    assert!(out.status.success(), "{text}");
    assert!(bin.exists(), "{text}");
    assert!(text.contains("target/"), "{text}");
}

#[test]
fn a_binary_in_an_unknown_location_is_kept() {
    let sb = Sandbox::new();
    let bin = copy_binary(&sb, "elsewhere");
    let o = sb
        .cmd_with_binary(&bin, &["uninstall", "--yes"])
        .output()
        .unwrap();
    assert!(o.status.success());
    assert!(bin.exists());
    assert!(String::from_utf8_lossy(&o.stdout).contains("not a known install location"));
}

fn seed_state_and_credentials(sb: &Sandbox) {
    let write = |rel: &str, text: &str| {
        let path = sb.p(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    };
    write("state/claude-code-proxy/codex-models.json", "{}");
    write("state/claude-code-proxy/proxy.log", "log");
    write("state/claude-code-proxy/errors/1.json", "{}");
    write("cfg/codex/auth.json", "proxy-codex-login");
    write("cfg/kimi/auth.json", "kimi-login");
    write("cfg/kimi/device_id", "id");
    write("cfg/config.json", "{\"port\": 1}");
    write("cfg/service.env", "KEY=VALUE");
    write("cfg/my-notes.txt", "not ours");
    // The Codex CLI's files, real-looking but inside the sandbox.
    write("home/.codex/auth.json", "codex-cli-login");
    write("home/.codex/config.toml", "x = 1");
    write("codex-auth.json", "codex-auth-override");
}

fn codex_files(sb: &Sandbox) -> Vec<(String, Vec<u8>)> {
    let mut v = snapshot(sb, "home");
    v.push((
        "codex-auth.json".into(),
        std::fs::read(sb.p("codex-auth.json")).unwrap(),
    ));
    v
}

#[test]
fn purge_deletes_state_but_never_credentials() {
    let sb = Sandbox::new();
    assert!(sb.run(&["setup"]).0);
    seed_state_and_credentials(&sb);
    let codex_before = codex_files(&sb);
    let cfg_before = snapshot(&sb, "cfg");
    let (ok, out) = uninstall(&sb, &["--purge"]);
    assert!(ok, "{out}");
    assert!(!sb.p("state/claude-code-proxy").exists(), "{out}");
    assert_eq!(snapshot(&sb, "cfg"), cfg_before);
    assert_eq!(codex_files(&sb), codex_before);
}

#[test]
fn without_purge_the_state_directory_stays() {
    let sb = Sandbox::new();
    assert!(sb.run(&["setup"]).0);
    seed_state_and_credentials(&sb);
    let (ok, out) = uninstall(&sb, &[]);
    assert!(ok, "{out}");
    assert!(sb.p("state/claude-code-proxy/codex-models.json").exists());
    assert!(sb.p("state/claude-code-proxy/proxy.log").exists());
    assert!(sb.p("cfg/codex/auth.json").exists());
}

#[test]
fn purge_credentials_implies_purge_and_never_touches_the_codex_cli() {
    let sb = Sandbox::new();
    assert!(sb.run(&["setup"]).0);
    seed_state_and_credentials(&sb);
    let codex_before = codex_files(&sb);
    let (ok, out) = uninstall(&sb, &["--purge-credentials"]);
    assert!(ok, "{out}");
    assert!(!sb.p("state/claude-code-proxy").exists(), "{out}");
    for gone in [
        "cfg/codex/auth.json",
        "cfg/kimi/auth.json",
        "cfg/kimi/device_id",
        "cfg/config.json",
        "cfg/service.env",
    ] {
        assert!(!sb.p(gone).exists(), "{gone} should be gone: {out}");
    }
    // Unknown files stay, and so does the directory that holds them.
    assert_eq!(sb.tree("cfg"), ["my-notes.txt"]);
    assert!(out.contains("never touched"), "{out}");
    assert_eq!(codex_files(&sb), codex_before);
}

#[test]
fn purge_credentials_removes_the_config_directory_when_it_ends_up_empty() {
    let sb = Sandbox::new();
    std::fs::create_dir_all(sb.p("cfg/codex")).unwrap();
    std::fs::write(sb.p("cfg/codex/auth.json"), "x").unwrap();
    let (ok, out) = uninstall(&sb, &["--purge-credentials"]);
    assert!(ok, "{out}");
    assert!(!sb.p("cfg").exists(), "{out}");
}

#[test]
fn without_a_setup_record_it_cleans_what_it_can_and_leaves_settings_alone() {
    let sb = Sandbox::new();
    let original = r#"{"env":{"ANTHROPIC_BASE_URL":"http://127.0.0.1:1234"}}"#;
    sb.write_settings(original);
    assert!(sb.run(&["service", "install"]).0);
    assert!(sb.run(&["agents", "sync"]).0);
    assert!(!sb.tree("service").is_empty());
    assert!(!sb.tree("agents").is_empty());
    assert!(!sb.state_file().exists());

    let (ok, out) = uninstall(&sb, &[]);
    assert!(ok, "{out}");
    assert!(sb.tree("service").is_empty());
    assert!(sb.tree("agents").is_empty());
    assert_eq!(std::fs::read_to_string(sb.settings()).unwrap(), original);
    assert!(out.contains("remove that line yourself"), "{out}");
}

#[test]
fn a_failing_step_does_not_stop_the_others_and_sets_the_exit_code() {
    let sb = Sandbox::new();
    sb.write_settings("{\n  \"model\": \"opus\"\n}\n");
    assert!(sb.run(&["setup"]).0);
    // Make the cards step fail: the agents "directory" becomes a regular file.
    std::fs::remove_dir_all(sb.p("agents")).unwrap();
    std::fs::write(sb.p("agents"), "not a directory").unwrap();

    let (ok, out) = uninstall(&sb, &[]);
    assert!(!ok, "{out}");
    assert!(out.contains("\u{2717} subagent cards"), "{out}");
    assert!(out.contains("\u{2713} autostart service"), "{out}");
    assert!(out.contains("\u{2713} Claude Code settings"), "{out}");
    assert!(sb.tree("service").is_empty());
    assert_eq!(sb.read_settings(), json!({"model": "opus"}));
    assert_eq!(
        std::fs::read_to_string(sb.p("agents")).unwrap(),
        "not a directory"
    );
}

#[test]
fn a_manual_server_on_the_port_is_reported_not_stopped() {
    let (sb, listener) = Sandbox::with_listener();
    let (ok, out) = uninstall(&sb, &[]);
    assert!(ok, "{out}");
    assert!(out.contains("still listening"), "{out}");
    assert!(out.contains("manual"), "{out}");
    drop(listener);
}

#[test]
fn uninstall_refuses_to_run_when_a_sandbox_variable_is_missing() {
    let sb = Sandbox::new();
    let mut cmd = sb.cmd(&["uninstall", "--yes"]);
    cmd.env_remove("CCP_AGENTS_DIR");
    let out = cmd.output().unwrap();
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("CCP_AGENTS_DIR"));
}

// ---------- enable / disable / status ----------

const ORIGINAL_SETTINGS: &str = "{\n    \"model\": \"opus\",\n    \"env\": {\n        \"FOO\": \"1\"\n    },\n    \"permissions\": {\"allow\": [\"x\"]}\n}\n";

fn marker(sb: &Sandbox) -> PathBuf {
    sb.p("state/claude-code-proxy/disabled")
}

fn unit(sb: &Sandbox) -> PathBuf {
    sb.p("service/claude-codex.service")
}

fn managed_cards(sb: &Sandbox) -> usize {
    sb.tree("agents")
        .iter()
        .filter(|f| {
            std::fs::read_to_string(sb.p("agents").join(f))
                .is_ok_and(|t| t.contains("managed-by: claude-codex"))
        })
        .count()
}

/// Toy provider logins that no command here may touch.
fn plant_credentials(sb: &Sandbox) -> Vec<(String, Vec<u8>)> {
    std::fs::create_dir_all(sb.p("cfg/kimi")).unwrap();
    std::fs::write(sb.p("cfg/kimi/auth.json"), r#"{"access":"toy"}"#).unwrap();
    std::fs::write(sb.p("codex-auth.json"), r#"{"tokens":"toy"}"#).unwrap();
    let mut files = snapshot(sb, "cfg");
    files.push((
        "codex-auth.json".into(),
        std::fs::read(sb.p("codex-auth.json")).unwrap(),
    ));
    files
}

#[test]
fn enable_disable_enable_returns_to_the_same_states() {
    let sb = Sandbox::new();
    sb.write_settings(ORIGINAL_SETTINGS);
    std::fs::create_dir_all(sb.p("agents")).unwrap();
    std::fs::write(sb.p("agents/mine.md"), "my agent").unwrap();
    let credentials = plant_credentials(&sb);

    let (ok, out) = sb.run(&["enable"]);
    assert!(ok, "{out}");
    assert!(out.contains("Enable finished"), "{out}");
    let v = sb.read_settings();
    assert_eq!(v["env"]["ANTHROPIC_BASE_URL"], sb.url().as_str());
    assert_eq!(v["env"]["CLAUDE_CODE_ENABLE_GATEWAY_MODEL_DISCOVERY"], "1");
    assert!(unit(&sb).exists());
    assert!(managed_cards(&sb) > 20);
    assert!(!marker(&sb).exists());
    let enabled = snapshot_all(&sb);
    let unit_text = std::fs::read(unit(&sb)).unwrap();

    let (ok, out) = sb.run(&["disable"]);
    assert!(ok, "{out}");
    assert!(out.contains("Disable finished"), "{out}");
    assert!(out.contains("Restart Claude Code"), "{out}");
    assert!(out.contains("claude-codex enable"), "{out}");
    // settings.json is back to its original bytes.
    assert_eq!(
        std::fs::read_to_string(sb.settings()).unwrap(),
        ORIGINAL_SETTINGS
    );
    // The managed cards are gone, the user's card is not.
    assert_eq!(sb.tree("agents"), ["mine.md"]);
    assert_eq!(
        std::fs::read_to_string(sb.p("agents/mine.md")).unwrap(),
        "my agent"
    );
    // The unit file stays, unchanged; the marker and the record exist.
    assert_eq!(std::fs::read(unit(&sb)).unwrap(), unit_text);
    assert!(marker(&sb).exists());
    assert!(sb.state_file().exists());
    // Credentials are untouched.
    assert_eq!(snapshot(&sb, "cfg"), credentials[..credentials.len() - 1]);
    assert_eq!(
        std::fs::read_to_string(sb.p("codex-auth.json")).unwrap(),
        r#"{"tokens":"toy"}"#
    );

    let (ok, out) = sb.run(&["enable"]);
    assert!(ok, "{out}");
    assert!(out.contains("Restart Claude Code"), "{out}");
    assert!(!marker(&sb).exists());
    assert_eq!(snapshot_all(&sb), enabled);
}

#[test]
fn enable_is_idempotent_and_keeps_setup_compatible() {
    let sb = Sandbox::new();
    sb.write_settings(ORIGINAL_SETTINGS);
    assert!(sb.run(&["enable"]).0);
    let first = snapshot_all(&sb);
    let (ok, out) = sb.run(&["enable"]);
    assert!(ok, "{out}");
    assert!(!out.contains("Restart Claude Code"), "{out}");
    assert_eq!(snapshot_all(&sb), first);
    // `setup` still produces the same state and `setup --undo` still reverts it.
    assert!(sb.run(&["setup"]).0);
    assert_eq!(snapshot_all(&sb), first);
    assert!(sb.run(&["setup", "--undo"]).0);
    assert_eq!(
        std::fs::read_to_string(sb.settings()).unwrap(),
        ORIGINAL_SETTINGS
    );
}

#[test]
fn enable_keeps_the_safety_rules_of_setup() {
    // A foreign base URL needs --force.
    let sb = Sandbox::new();
    let foreign = r#"{"env":{"ANTHROPIC_BASE_URL":"https://gateway.example"}}"#;
    sb.write_settings(foreign);
    let (ok, out) = sb.run(&["enable"]);
    assert!(ok, "{out}");
    assert!(out.contains("--force"), "{out}");
    assert_eq!(std::fs::read_to_string(sb.settings()).unwrap(), foreign);
    assert!(sb.run(&["enable", "--force"]).0);
    assert_eq!(
        sb.read_settings()["env"]["ANTHROPIC_BASE_URL"],
        sb.url().as_str()
    );
    assert!(sb.p("claude/settings.json.claude-codex.bak").exists());

    // Invalid JSON is an error and the file is not touched.
    let sb = Sandbox::new();
    sb.write_settings("{ nope");
    let (ok, out) = sb.run(&["enable"]);
    assert!(!ok);
    assert!(out.contains("not valid JSON"), "{out}");
    assert_eq!(std::fs::read_to_string(sb.settings()).unwrap(), "{ nope");
}

#[test]
fn disable_restores_the_value_a_forced_enable_replaced() {
    let sb = Sandbox::new();
    let foreign =
        "{\n  \"env\": {\n    \"ANTHROPIC_BASE_URL\": \"https://gateway.example\"\n  }\n}\n";
    sb.write_settings(foreign);
    assert!(sb.run(&["enable", "--force"]).0);
    let (ok, out) = sb.run(&["disable"]);
    assert!(ok, "{out}");
    assert_eq!(std::fs::read_to_string(sb.settings()).unwrap(), foreign);
}

#[test]
fn disable_never_touches_a_base_url_it_did_not_add() {
    let sb = Sandbox::new();
    let foreign =
        "{\n  \"env\": {\n    \"ANTHROPIC_BASE_URL\": \"https://gateway.example\"\n  }\n}\n";
    sb.write_settings(foreign);
    assert!(sb.run(&["enable"]).0); // not forced: the URL stays foreign
    let (ok, out) = sb.run(&["disable"]);
    assert!(ok, "{out}");
    assert_eq!(std::fs::read_to_string(sb.settings()).unwrap(), foreign);
    assert!(marker(&sb).exists());

    // A URL for this proxy that the user wrote is left too, with a loud warning.
    let sb = Sandbox::new();
    let mine = format!("{{\"env\":{{\"ANTHROPIC_BASE_URL\":\"{}\"}}}}", sb.url());
    sb.write_settings(&mine);
    assert!(sb.run(&["enable"]).0);
    let (ok, out) = sb.run(&["disable"]);
    assert!(ok, "{out}");
    assert_eq!(std::fs::read_to_string(sb.settings()).unwrap(), mine);
    assert!(out.contains("did not add it"), "{out}");
}

#[test]
fn disable_removes_a_settings_file_that_enable_created() {
    let sb = Sandbox::new();
    assert!(sb.run(&["enable"]).0);
    assert!(sb.settings().exists());
    assert!(sb.run(&["disable"]).0);
    assert!(!sb.settings().exists());
    // The record and the unit file remain.
    assert!(sb.state_file().exists());
    assert!(unit(&sb).exists());
}

#[test]
fn disable_keeps_the_binary_credentials_caches_and_record() {
    let sb = Sandbox::new();
    let credentials = plant_credentials(&sb);
    let binary = copy_binary(&sb, "bin");
    std::fs::create_dir_all(sb.p("state/claude-code-proxy")).unwrap();
    std::fs::write(sb.p("state/claude-code-proxy/codex-models.json"), "{}").unwrap();
    assert!(
        sb.cmd_with_binary(&binary, &["enable"])
            .output()
            .unwrap()
            .status
            .success()
    );
    let record = std::fs::read_to_string(sb.state_file()).unwrap();

    let out = sb.cmd_with_binary(&binary, &["disable"]).output().unwrap();
    assert!(out.status.success());
    assert!(binary.exists());
    assert_eq!(snapshot(&sb, "cfg"), credentials[..credentials.len() - 1]);
    assert!(sb.p("codex-auth.json").exists());
    assert_eq!(
        std::fs::read_to_string(sb.p("state/claude-code-proxy/codex-models.json")).unwrap(),
        "{}"
    );
    // The record stays; only the entries that were reverted are forgotten.
    let after: Value =
        serde_json::from_str(&std::fs::read_to_string(sb.state_file()).unwrap()).unwrap();
    let before: Value = serde_json::from_str(&record).unwrap();
    assert!(before["added"].as_object().is_some_and(|a| !a.is_empty()));
    assert!(after["added"].as_object().unwrap().is_empty());
    assert_eq!(after["service_installed"], before["service_installed"]);
}

#[test]
fn disable_is_idempotent_and_works_without_a_prior_setup() {
    let sb = Sandbox::new();
    sb.write_settings(ORIGINAL_SETTINGS);
    let (ok, out) = sb.run(&["disable"]);
    assert!(ok, "{out}");
    assert_eq!(
        std::fs::read_to_string(sb.settings()).unwrap(),
        ORIGINAL_SETTINGS
    );
    assert!(marker(&sb).exists());
    assert!(!sb.state_file().exists(), "no record is invented");
    let first = snapshot_all(&sb);
    assert!(sb.run(&["disable"]).0);
    assert_eq!(snapshot_all(&sb), first);

    // After a real enable/disable, a second disable changes nothing either.
    let sb = Sandbox::new();
    sb.write_settings(ORIGINAL_SETTINGS);
    assert!(sb.run(&["enable"]).0);
    assert!(sb.run(&["disable"]).0);
    let first = snapshot_all(&sb);
    assert!(sb.run(&["disable"]).0);
    assert_eq!(snapshot_all(&sb), first);
}

#[test]
fn dry_run_of_enable_and_disable_has_no_effects() {
    let sb = Sandbox::new();
    sb.write_settings(ORIGINAL_SETTINGS);
    let (ok, out) = sb.run(&["enable", "--dry-run"]);
    assert!(ok && out.contains("would"), "{out}");
    assert!(sb.tree("service").is_empty() && sb.tree("agents").is_empty());
    assert!(sb.tree("state").is_empty());
    assert_eq!(
        std::fs::read_to_string(sb.settings()).unwrap(),
        ORIGINAL_SETTINGS
    );

    assert!(sb.run(&["enable"]).0);
    let enabled = snapshot_all(&sb);
    let (ok, out) = sb.run(&["disable", "--dry-run"]);
    assert!(ok && out.contains("would"), "{out}");
    assert_eq!(snapshot_all(&sb), enabled);
    assert!(!marker(&sb).exists());

    // enable --dry-run on a disabled install keeps the marker.
    assert!(sb.run(&["disable"]).0);
    let disabled = snapshot_all(&sb);
    assert!(sb.run(&["enable", "--dry-run"]).0);
    assert_eq!(snapshot_all(&sb), disabled);
    assert!(marker(&sb).exists());
}

#[test]
fn disable_warns_about_a_manual_server_and_does_not_stop_it() {
    let (sb, listener) = Sandbox::with_listener();
    assert!(sb.run(&["enable"]).0);
    let (ok, out) = sb.run(&["disable"]);
    assert!(ok, "{out}");
    assert!(out.contains("still listening"), "{out}");
    assert!(out.contains("not stopped"), "{out}");
    // Still bound and accepting.
    assert!(std::net::TcpStream::connect(("127.0.0.1", sb.port)).is_ok());
    drop(listener);
}

#[test]
fn enable_disable_and_status_refuse_to_run_when_a_sandbox_variable_is_missing() {
    for command in ["enable", "disable", "status"] {
        let sb = Sandbox::new();
        let mut cmd = sb.cmd(&[command]);
        cmd.env_remove("CCP_SERVICE_DIR");
        let out = cmd.output().unwrap();
        assert!(!out.status.success(), "{command}");
        assert!(
            String::from_utf8_lossy(&out.stderr).contains("CCP_SERVICE_DIR"),
            "{command}"
        );
        assert!(sb.tree("state").is_empty() && sb.tree("agents").is_empty());
    }
}

/// Runs `serve` for a moment with card sync on and returns the card count.
fn cards_after_serve(sb: &Sandbox) -> usize {
    let mut cmd = sb.cmd(&["serve", "--no-monitor", "--port", &sb.port.to_string()]);
    cmd.env("CCP_AGENT_CARDS", "1")
        .env("CCP_MODEL_DISCOVERY", "0")
        .timeout(std::time::Duration::from_secs(3));
    // The timeout kills the server; its exit status is not the point.
    let _ = cmd.output();
    managed_cards(sb)
}

#[test]
fn the_disabled_marker_keeps_serve_from_syncing_cards() {
    // Control: an enabled install syncs at startup.
    let sb = Sandbox::new();
    let control = Sandbox::new();
    assert!(cards_after_serve(&control) > 20);

    // Disabled: the same serve leaves the directory empty.
    assert!(sb.run(&["disable"]).0);
    assert!(marker(&sb).exists());
    assert_eq!(cards_after_serve(&sb), 0);
    assert!(sb.tree("agents").is_empty());

    // Enabled again: it syncs again.
    assert!(sb.run(&["enable"]).0);
    std::fs::remove_dir_all(sb.p("agents")).unwrap();
    assert!(cards_after_serve(&sb) > 20);
}

#[test]
fn every_command_leaves_exactly_its_state() {
    // (unit file, managed cards, base URL in settings, marker) after each command.
    fn state(sb: &Sandbox) -> (bool, bool, bool, bool) {
        let has_url = std::fs::read_to_string(sb.settings())
            .ok()
            .and_then(|t| serde_json::from_str::<Value>(&t).ok())
            .is_some_and(|v| v["env"].get("ANTHROPIC_BASE_URL").is_some());
        (
            unit(sb).exists(),
            managed_cards(sb) > 0,
            has_url,
            marker(sb).exists(),
        )
    }
    let sb = Sandbox::new();
    sb.write_settings("{\"model\":\"opus\"}\n");
    assert_eq!(state(&sb), (false, false, false, false), "fresh");
    assert!(sb.run(&["enable"]).0);
    assert_eq!(state(&sb), (true, true, true, false), "enable");
    assert!(sb.run(&["disable"]).0);
    assert_eq!(state(&sb), (true, false, false, true), "disable");
    assert!(sb.run(&["enable"]).0);
    assert_eq!(state(&sb), (true, true, true, false), "enable again");
    assert!(sb.run(&["disable"]).0);
    assert!(uninstall(&sb, &[]).0);
    assert_eq!(state(&sb), (false, false, false, false), "uninstall");
    assert_eq!(
        std::fs::read_to_string(sb.settings()).unwrap(),
        "{\"model\":\"opus\"}\n"
    );
    // `setup` is the same as `enable`, and `setup --undo` clears the marker.
    assert!(sb.run(&["setup"]).0);
    assert_eq!(state(&sb), (true, true, true, false), "setup");
    assert!(sb.run(&["disable"]).0);
    assert!(sb.run(&["setup", "--undo"]).0);
    assert_eq!(state(&sb), (false, false, false, false), "setup --undo");
}

/// Answers `GET /healthz` with 200 on `port` until the test ends.
fn fake_proxy(port: u16) {
    let listener = std::net::TcpListener::bind(("127.0.0.1", port)).unwrap();
    std::thread::spawn(move || {
        use std::io::{Read, Write};
        for stream in listener.incoming().flatten() {
            let mut stream = stream;
            let mut buf = [0u8; 512];
            let _ = stream.read(&mut buf);
            let _ = stream.write_all(
                b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 11\r\nConnection: close\r\n\r\n{\"ok\":true}",
            );
        }
    });
}

#[test]
fn status_reflects_each_state() {
    let sb = Sandbox::new();
    sb.write_settings("{\"model\":\"opus\"}\n");

    let (ok, out) = sb.run(&["status"]);
    assert!(ok, "{out}");
    assert!(out.contains(env!("CARGO_PKG_VERSION")), "{out}");
    assert!(out.contains("installed: no"), "{out}");
    assert!(out.contains("service manager calls are off"), "{out}");
    assert!(out.contains("listening: no, responds: no"), "{out}");
    assert!(out.contains("ANTHROPIC_BASE_URL is not set"), "{out}");
    assert!(out.contains("0 managed"), "{out}");
    assert!(out.contains("Codex login"), "{out}");
    assert!(out.contains("not found"), "{out}");
    assert!(out.contains("claude-codex enable"), "{out}");

    std::fs::write(sb.p("codex-auth.json"), "{}").unwrap();
    assert!(sb.run(&["enable"]).0);
    fake_proxy(sb.port);
    let (ok, out) = sb.run(&["status"]);
    assert!(ok, "{out}");
    assert!(out.contains("enabled"), "{out}");
    assert!(!out.contains("disabled"), "{out}");
    assert!(out.contains("installed: yes"), "{out}");
    assert!(out.contains("listening: yes, responds: yes"), "{out}");
    assert!(out.contains("(this proxy)"), "{out}");
    let cards = managed_cards(&sb);
    assert!(out.contains(&format!("{cards} managed")), "{out}");

    // Disabled: marker shown, URL gone, no cards, and no changes by status.
    assert!(sb.run(&["disable"]).0);
    let before = snapshot_all(&sb);
    let (ok, out) = sb.run(&["status"]);
    assert!(ok, "{out}");
    assert!(out.contains("disabled (marker"), "{out}");
    assert!(out.contains("0 managed"), "{out}");
    assert!(out.contains("ANTHROPIC_BASE_URL is not set"), "{out}");
    assert_eq!(snapshot_all(&sb), before);

    // A base URL somewhere else is reported as not this proxy.
    let sb = Sandbox::new();
    sb.write_settings(r#"{"env":{"ANTHROPIC_BASE_URL":"https://gateway.example"}}"#);
    let (_, out) = sb.run(&["status"]);
    assert!(out.contains("(not this proxy)"), "{out}");
}
