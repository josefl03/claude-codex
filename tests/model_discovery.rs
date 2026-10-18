//! Own model discovery against a local mock of the Codex `/models` endpoint.
//!
//! Hermetic: every process runs with a temporary HOME, state, config, agents
//! and Codex auth location, and the endpoint is a local server injected with
//! `CCP_CODEX_BASE_URL`. Nothing here may touch `~/.claude`, `~/.codex`, the
//! real service manager or the proxy listening on the default port.

use assert_cmd::Command;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode, Uri};
use axum::response::IntoResponse;
use axum::routing::get;
use claude_codex::registry::Registry;
use serde_json::{Value, json};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use tempfile::TempDir;

/// (uri, authorization, chatgpt-account-id) per request.
type Requests = Arc<Mutex<Vec<(String, Option<String>, Option<String>)>>>;

#[derive(Clone)]
struct MockState {
    status: u16,
    body: String,
    requests: Requests,
}

struct Mock {
    base_url: String,
    state: Arc<Mutex<MockState>>,
    requests: Requests,
}

fn catalog(models: &[(&str, &str, bool)]) -> String {
    let models: Vec<Value> = models
        .iter()
        .map(|(slug, visibility, api)| {
            json!({"slug": slug, "display_name": slug, "visibility": visibility,
                   "supported_in_api": api, "priority": 1})
        })
        .collect();
    json!({ "models": models }).to_string()
}

impl Mock {
    fn start(body: String) -> Self {
        let requests = Arc::new(Mutex::new(Vec::new()));
        let state = Arc::new(Mutex::new(MockState {
            status: 200,
            body,
            requests: requests.clone(),
        }));
        let (tx, rx) = std::sync::mpsc::channel();
        let app_state = state.clone();
        std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            runtime.block_on(async move {
                async fn models(
                    State(state): State<Arc<Mutex<MockState>>>,
                    headers: HeaderMap,
                    uri: Uri,
                ) -> impl IntoResponse {
                    let state = state.lock().unwrap().clone();
                    let header = |name: &str| {
                        headers
                            .get(name)
                            .and_then(|v| v.to_str().ok())
                            .map(str::to_string)
                    };
                    state.requests.lock().unwrap().push((
                        uri.to_string(),
                        header("authorization"),
                        header("chatgpt-account-id"),
                    ));
                    (
                        StatusCode::from_u16(state.status).unwrap(),
                        [("content-type", "application/json")],
                        state.body,
                    )
                }
                let app = axum::Router::new()
                    .route("/models", get(models))
                    .with_state(app_state);
                let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
                tx.send(listener.local_addr().unwrap()).unwrap();
                axum::serve(listener, app).await.unwrap();
            });
        });
        let addr = rx.recv().unwrap();
        Self {
            base_url: format!("http://{addr}"),
            state,
            requests,
        }
    }

    fn respond(&self, status: u16, body: impl Into<String>) {
        let mut state = self.state.lock().unwrap();
        state.status = status;
        state.body = body.into();
    }

    fn hits(&self) -> usize {
        self.requests.lock().unwrap().len()
    }
}

struct Sandbox {
    root: TempDir,
}

impl Sandbox {
    fn new() -> Self {
        let sb = Self {
            root: tempfile::tempdir().unwrap(),
        };
        std::fs::create_dir_all(sb.p("codex")).unwrap();
        std::fs::write(
            sb.p("codex/auth.json"),
            json!({"tokens": {"access_token": "test-access",
                "refresh_token": "test-refresh", "account_id": "acct_test"}})
            .to_string(),
        )
        .unwrap();
        sb
    }

    fn p(&self, name: &str) -> PathBuf {
        self.root.path().join(name)
    }

    fn own_cache(&self) -> PathBuf {
        self.p("state/claude-code-proxy/codex-models.json")
    }

    /// The variables every in-process test also sets itself.
    fn envs(&self, mock: &Mock) -> Vec<(&'static str, String)> {
        let s = |name: &str| self.p(name).display().to_string();
        vec![
            ("HOME", s("home")),
            ("XDG_STATE_HOME", s("state")),
            ("XDG_CONFIG_HOME", s("xdgconfig")),
            ("CCP_CONFIG_DIR", s("cfg")),
            ("CCP_SERVICE_DIR", s("service")),
            ("CCP_AGENTS_DIR", s("agents")),
            ("CCP_CODEX_AUTH_FILE", s("codex/auth.json")),
            ("CCP_CODEX_BASE_URL", mock.base_url.clone()),
            ("NO_PROXY", "127.0.0.1,localhost".to_string()),
        ]
    }

    fn cmd(&self, mock: &Mock, args: &[&str]) -> Command {
        let mut cmd = Command::cargo_bin("claude-codex").unwrap();
        cmd.env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap_or_default());
        for (key, value) in self.envs(mock) {
            cmd.env(key, value);
        }
        cmd.args(args);
        cmd
    }

    fn run(&self, mock: &Mock, args: &[&str]) -> (bool, String) {
        let out = self.cmd(mock, args).output().unwrap();
        (
            out.status.success(),
            format!(
                "{}{}",
                String::from_utf8_lossy(&out.stdout),
                String::from_utf8_lossy(&out.stderr)
            ),
        )
    }

    fn cards(&self) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(self.p("agents"))
            .map(|rd| {
                rd.flatten()
                    .map(|e| e.file_name().to_string_lossy().into_owned())
                    .collect()
            })
            .unwrap_or_default();
        names.sort();
        names
    }
}

struct EnvGuard(Vec<(&'static str, Option<std::ffi::OsString>)>);

impl EnvGuard {
    fn set(vars: Vec<(&'static str, String)>) -> Self {
        let saved = vars
            .iter()
            .map(|(k, _)| (*k, std::env::var_os(k)))
            .collect();
        for (key, value) in vars {
            unsafe { std::env::set_var(key, value) };
        }
        Self(saved)
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        for (key, value) in self.0.drain(..) {
            unsafe {
                match value {
                    Some(v) => std::env::set_var(key, v),
                    None => std::env::remove_var(key),
                }
            }
        }
    }
}

#[test]
fn refresh_adds_a_new_model_authenticates_like_codex_and_never_touches_codex_files() {
    let sb = Sandbox::new();
    let mock = Mock::start(catalog(&[("gpt-9-new", "list", true)]));
    let auth_before = std::fs::read(sb.p("codex/auth.json")).unwrap();

    let (ok, out) = sb.run(&mock, &["models", "--refresh"]);
    assert!(ok, "{out}");
    assert!(out.contains("new model discovered: gpt-9-new"), "{out}");
    assert!(out.contains("gpt-9-new, gpt-9-new-fast"), "{out}");
    // Bundled models are still there: the cache only adds.
    assert!(out.contains("gpt-6-luna"), "{out}");

    let requests = mock.requests.lock().unwrap().clone();
    assert_eq!(requests.len(), 1);
    assert!(
        requests[0].0.starts_with("/models?client_version="),
        "{requests:?}"
    );
    assert_eq!(requests[0].1.as_deref(), Some("Bearer test-access"));
    assert_eq!(requests[0].2.as_deref(), Some("acct_test"));

    let cache: Value = serde_json::from_slice(&std::fs::read(sb.own_cache()).unwrap()).unwrap();
    assert_eq!(cache["models"][0]["slug"], "gpt-9-new");
    // The Codex CLI's files are only read.
    assert!(!sb.p("codex/models_cache.json").exists());
    assert_eq!(std::fs::read(sb.p("codex/auth.json")).unwrap(), auth_before);

    // A later plain `models` (no network) still sees it.
    let hits = mock.hits();
    let (ok, out) = sb.run(&mock, &["models"]);
    assert!(ok && out.contains("gpt-9-new"), "{out}");
    assert_eq!(mock.hits(), hits);
}

#[test]
fn hidden_and_non_api_models_from_the_endpoint_are_ignored() {
    let sb = Sandbox::new();
    let mock = Mock::start(catalog(&[
        ("gpt-9-new", "list", true),
        ("gpt-9-hidden", "hide", true),
        ("gpt-9-noapi", "list", false),
    ]));
    let (ok, out) = sb.run(&mock, &["models", "--refresh"]);
    assert!(ok, "{out}");
    assert!(out.contains("gpt-9-new"));
    assert!(
        !out.contains("gpt-9-hidden") && !out.contains("gpt-9-noapi"),
        "{out}"
    );
}

#[test]
fn agents_sync_refreshes_first_creates_the_card_and_removes_it_when_the_model_goes() {
    let sb = Sandbox::new();
    let mock = Mock::start(catalog(&[("gpt-9-new", "list", true)]));
    let (ok, out) = sb.run(&mock, &["agents", "sync"]);
    assert!(ok, "{out}");
    assert!(sb.cards().contains(&"gpt-9-new.md".to_string()), "{out}");
    let card = std::fs::read_to_string(sb.p("agents/gpt-9-new.md")).unwrap();
    assert!(card.contains("model: gpt-9-new"));

    // The endpoint stops listing it: the next sync retires the card.
    mock.respond(200, catalog(&[("gpt-9-other", "list", true)]));
    let (ok, out) = sb.run(&mock, &["agents", "sync"]);
    assert!(ok, "{out}");
    assert!(
        out.contains("model no longer available: gpt-9-new"),
        "{out}"
    );
    let cards = sb.cards();
    assert!(!cards.contains(&"gpt-9-new.md".to_string()), "{cards:?}");
    assert!(cards.contains(&"gpt-9-other.md".to_string()), "{cards:?}");
    // Bundled models keep theirs.
    assert!(cards.contains(&"gpt-6-luna.md".to_string()), "{cards:?}");
}

#[test]
fn agents_sync_dry_run_does_not_hit_the_endpoint_or_write_the_cache() {
    let sb = Sandbox::new();
    let mock = Mock::start(catalog(&[("gpt-9-new", "list", true)]));
    let (ok, out) = sb.run(&mock, &["agents", "sync", "--dry-run"]);
    assert!(ok, "{out}");
    assert_eq!(mock.hits(), 0);
    assert!(!sb.own_cache().exists());
    assert!(sb.cards().is_empty());
}

#[test]
fn failures_keep_the_last_good_list_and_the_cards() {
    let failures: Vec<(u16, String)> = vec![
        (500, "boom".into()),
        (401, r#"{"error":"unauthorized"}"#.into()),
        (429, "slow down".into()),
        (200, "<html>not json</html>".into()),
        (200, r#"{"data":[1,2,3]}"#.into()),
        (200, catalog(&[])),
        (200, catalog(&[("gpt-9-hidden", "hide", true)])),
    ];
    let sb = Sandbox::new();
    let mock = Mock::start(catalog(&[("gpt-9-new", "list", true)]));
    let (ok, out) = sb.run(&mock, &["agents", "sync"]);
    assert!(ok, "{out}");
    let cache_before = std::fs::read(sb.own_cache()).unwrap();
    let cards_before = sb.cards();
    assert!(cards_before.contains(&"gpt-9-new.md".to_string()));

    for (status, body) in failures {
        mock.respond(status, body.clone());
        let (ok, out) = sb.run(&mock, &["models", "--refresh"]);
        assert!(!ok, "{status} {body}: refresh should report failure\n{out}");
        assert!(out.contains("keeping the last known list"), "{out}");
        // The list is still served in full...
        assert!(out.contains("gpt-9-new"), "{status}: {out}");
        assert!(out.contains("gpt-6-luna"), "{status}: {out}");
        // ...and the cache and the cards did not move.
        assert_eq!(
            std::fs::read(sb.own_cache()).unwrap(),
            cache_before,
            "{status}"
        );
        let (ok, out) = sb.run(&mock, &["agents", "sync"]);
        assert!(ok, "{out}");
        assert_eq!(sb.cards(), cards_before, "{status}: {out}");
    }
}

#[test]
fn unreachable_endpoint_and_missing_login_are_tolerated() {
    let sb = Sandbox::new();
    let mock = Mock::start(catalog(&[("gpt-9-new", "list", true)]));
    assert!(sb.run(&mock, &["models", "--refresh"]).0);

    // Nothing listens on port 9 of localhost.
    let dead = Mock {
        base_url: "http://127.0.0.1:9".into(),
        state: mock.state.clone(),
        requests: mock.requests.clone(),
    };
    let (ok, out) = sb.run(&dead, &["models", "--refresh"]);
    assert!(!ok && out.contains("network error"), "{out}");
    assert!(out.contains("gpt-9-new"), "{out}");

    std::fs::remove_file(sb.p("codex/auth.json")).unwrap();
    let before = mock.hits();
    let (ok, out) = sb.run(&mock, &["models", "--refresh"]);
    assert!(!ok && out.contains("not authorized"), "{out}");
    assert!(out.contains("gpt-9-new"), "{out}");
    assert_eq!(mock.hits(), before, "no token, no request");
}

#[test]
fn the_list_is_the_union_of_bundled_codex_cli_cache_and_own_cache() {
    let sb = Sandbox::new();
    std::fs::write(
        sb.p("codex/models_cache.json"),
        catalog(&[("gpt-cli-only", "list", true)]),
    )
    .unwrap();
    let mock = Mock::start(catalog(&[("gpt-own-only", "list", true)]));
    let (ok, out) = sb.run(&mock, &["models", "--refresh"]);
    assert!(ok, "{out}");
    for model in ["gpt-cli-only", "gpt-own-only", "gpt-6-luna"] {
        assert!(out.contains(model), "{model} missing\n{out}");
    }
    // The Codex CLI's cache was not rewritten.
    let cli_cache = std::fs::read_to_string(sb.p("codex/models_cache.json")).unwrap();
    assert!(!cli_cache.contains("gpt-own-only"));
}

#[tokio::test]
async fn a_running_registry_routes_lists_and_aliases_a_new_model_without_restart() {
    use axum::body::Body;
    use axum::http::Request;
    use tower::util::ServiceExt;

    let sb = Sandbox::new();
    let mock = Mock::start(catalog(&[("gpt-9-sol", "list", true)]));
    let _env = EnvGuard::set(sb.envs(&mock));

    let registry = Arc::new(Registry::with_default_alias());
    assert!(
        registry.provider_for_model("gpt-9-sol", None).is_none(),
        "{:?} {:?}",
        registry.supported_models_for("codex"),
        sb.own_cache()
    );

    let report = claude_codex::model_discovery::refresh_codex()
        .await
        .unwrap();
    assert_eq!(report.added, vec!["gpt-9-sol"]);
    assert!(report.cache_written);

    let provider = registry.provider_for_model("gpt-9-sol", None).unwrap();
    assert_eq!(provider.name(), "codex");
    assert!(
        registry
            .provider_for_model("gpt-9-sol-fast", None)
            .is_some()
    );

    let response = claude_codex::server::app(registry.clone())
        .oneshot(
            Request::builder()
                .uri("/v1/models")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let listing: Value = serde_json::from_slice(&bytes).unwrap();
    let ids: Vec<&str> = listing["data"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|m| m["id"].as_str())
        .collect();
    assert!(
        ids.contains(&"gpt-9-sol") && ids.contains(&"gpt-9-sol-fast"),
        "{ids:?}"
    );

    // Same endpoint, model gone: it disappears from routing too.
    mock.respond(200, catalog(&[("gpt-9-terra", "list", true)]));
    let report = claude_codex::model_discovery::refresh_codex()
        .await
        .unwrap();
    assert_eq!(report.removed, vec!["gpt-9-sol"]);
    // Any listing (`/v1/models`, the monitor, a miss in routing) refreshes the
    // table; a retired model then stops routing.
    registry.refresh();
    assert!(registry.provider_for_model("gpt-9-sol", None).is_none());
    assert!(registry.provider_for_model("gpt-9-terra", None).is_some());
}

#[tokio::test]
async fn aliases_resolve_to_discovered_targets() {
    use claude_codex::providers::codex::translate::model_allowlist::resolve_model;
    let sb = Sandbox::new();
    // The account lost gpt-6-sol but still has gpt-6.1-sol, the next opus target.
    let mock = Mock::start(catalog(&[
        ("gpt-6.1-sol", "list", true),
        ("gpt-6-luna", "list", true),
    ]));
    let _env = EnvGuard::set(sb.envs(&mock));
    claude_codex::model_discovery::refresh_codex()
        .await
        .unwrap();
    assert_eq!(resolve_model("opus"), "gpt-6.1-sol");
    assert_eq!(resolve_model("haiku"), "gpt-6-luna");
}

#[tokio::test]
async fn startup_pass_refreshes_before_syncing_cards_and_stops_when_interval_is_zero() {
    let sb = Sandbox::new();
    let mock = Mock::start(catalog(&[("gpt-9-new", "list", true)]));
    let mut vars = sb.envs(&mock);
    vars.extend([
        ("CCP_MODEL_REFRESH_INTERVAL_SECS", "0".to_string()),
        ("CCP_AGENT_SYNC_INTERVAL_SECS", "0".to_string()),
    ]);
    let _env = EnvGuard::set(vars);
    let task = claude_codex::model_discovery::spawn_background_tasks().expect("enabled");
    for _ in 0..200 {
        if task.is_finished() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    assert!(
        task.is_finished(),
        "interval 0 must end the loop after one pass"
    );
    assert_eq!(mock.hits(), 1);
    // The card exists on the first pass only because the refresh ran first.
    assert!(
        sb.cards().contains(&"gpt-9-new.md".to_string()),
        "{:?}",
        sb.cards()
    );
}

#[tokio::test]
async fn discovery_can_be_switched_off() {
    let sb = Sandbox::new();
    let mock = Mock::start(catalog(&[("gpt-9-new", "list", true)]));
    let mut vars = sb.envs(&mock);
    vars.extend([
        ("CCP_MODEL_DISCOVERY", "0".to_string()),
        ("CCP_MODEL_REFRESH_INTERVAL_SECS", "0".to_string()),
        ("CCP_AGENT_SYNC_INTERVAL_SECS", "0".to_string()),
    ]);
    let _env = EnvGuard::set(vars);
    let task = claude_codex::model_discovery::spawn_background_tasks().expect("cards still on");
    for _ in 0..200 {
        if task.is_finished() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    assert!(task.is_finished());
    assert_eq!(mock.hits(), 0);
    assert!(!sb.cards().contains(&"gpt-9-new.md".to_string()));
    assert!(sb.cards().contains(&"gpt-6-luna.md".to_string()));
}

// ---------- `-latest` aliases follow the discovered models ----------

fn card_lines(sb: &Sandbox, file: &str) -> Vec<String> {
    std::fs::read_to_string(sb.p("agents").join(file))
        .unwrap()
        .lines()
        .map(str::to_string)
        .collect()
}

#[tokio::test]
async fn a_new_model_takes_over_the_latest_alias_without_a_restart() {
    use claude_codex::latest::Resolution;
    let sb = Sandbox::new();
    let mock = Mock::start(catalog(&[("gpt-6.1-sol", "list", true)]));
    let _env = EnvGuard::set(sb.envs(&mock));
    let registry = Registry::with_default_alias();
    let target = |registry: &Registry, alias: &str| match registry.resolve_latest(alias) {
        Resolution::Resolved(a) => a.target,
        other => panic!("{alias}: {other:?}"),
    };
    assert_eq!(target(&registry, "gpt-sol-latest"), "gpt-6.1-sol");
    assert_eq!(target(&registry, "gpt-latest"), "gpt-6.1-sol");

    claude_codex::model_discovery::refresh_codex()
        .await
        .unwrap();
    mock.respond(
        200,
        catalog(&[("gpt-6.1-sol", "list", true), ("gpt-7-sol", "list", true)]),
    );
    claude_codex::model_discovery::refresh_codex()
        .await
        .unwrap();

    // Same Registry object: no restart, and only the sol/family aliases move.
    assert_eq!(target(&registry, "gpt-sol-latest"), "gpt-7-sol");
    assert_eq!(target(&registry, "gpt-latest"), "gpt-7-sol");
    assert_eq!(target(&registry, "gpt-luna-latest"), "gpt-6-luna");
    // The new model's alias target also routes.
    assert_eq!(
        registry
            .provider_for_model("gpt-7-sol", None)
            .unwrap()
            .name(),
        "codex"
    );
}

#[test]
fn alias_cards_keep_their_model_line_and_only_the_text_follows_the_target() {
    let sb = Sandbox::new();
    let mock = Mock::start(catalog(&[("gpt-6.1-sol", "list", true)]));
    let (ok, out) = sb.run(&mock, &["agents", "sync"]);
    assert!(ok, "{out}");
    let before = card_lines(&sb, "gpt-sol-latest.md");
    assert!(
        before.contains(&"name: gpt-sol-latest".to_string()),
        "{before:?}"
    );
    assert!(
        before.contains(&"model: gpt-sol-latest".to_string()),
        "{before:?}"
    );
    assert!(
        before.iter().any(|l| l.starts_with("description:")
            && l.contains("Always the newest sol model of the gpt family, currently gpt-6.1-sol")),
        "{before:?}"
    );
    for alias in [
        "gpt-latest",
        "gpt-luna-latest",
        "gpt-terra-latest",
        "gpt-astra-latest",
    ] {
        assert!(sb.cards().contains(&format!("{alias}.md")), "{alias}");
    }
    // Nothing to do the second time.
    let (_, out) = sb.run(&mock, &["agents", "sync"]);
    assert!(out.contains("already up to date"), "{out}");

    // A newer sol appears: the card is rewritten, but its name and model stay.
    mock.respond(
        200,
        catalog(&[("gpt-6.1-sol", "list", true), ("gpt-7-sol", "list", true)]),
    );
    let (ok, out) = sb.run(&mock, &["agents", "sync"]);
    assert!(ok, "{out}");
    assert!(out.contains("update: gpt-sol-latest.md"), "{out}");
    assert!(out.contains("create: gpt-7-sol.md"), "{out}");
    let after = card_lines(&sb, "gpt-sol-latest.md");
    assert_eq!(after[1], before[1], "name line");
    assert!(after.contains(&"model: gpt-sol-latest".to_string()));
    assert!(
        after.iter().any(|l| l.contains("currently gpt-7-sol")),
        "{after:?}"
    );
}

#[test]
fn switching_the_aliases_off_retires_their_cards_and_the_listing() {
    let sb = Sandbox::new();
    let mock = Mock::start(catalog(&[("gpt-6.1-sol", "list", true)]));
    assert!(sb.run(&mock, &["agents", "sync"]).0);
    assert!(sb.cards().contains(&"gpt-sol-latest.md".to_string()));
    // An unmanaged card must survive.
    std::fs::write(sb.p("agents/mine.md"), "mine").unwrap();

    let mut cmd = sb.cmd(&mock, &["agents", "sync"]);
    cmd.env("CCP_LATEST_ALIASES", "0");
    let out = cmd.output().unwrap();
    assert!(out.status.success());
    let cards = sb.cards();
    assert!(
        cards.iter().all(|c| !c.ends_with("-latest.md")),
        "{cards:?}"
    );
    assert!(cards.contains(&"gpt-6-luna.md".to_string()));
    assert!(cards.contains(&"mine.md".to_string()));

    let mut cmd = sb.cmd(&mock, &["models"]);
    cmd.env("CCP_LATEST_ALIASES", "0");
    let text = String::from_utf8(cmd.output().unwrap().stdout).unwrap();
    assert!(!text.contains("latest aliases"), "{text}");
    let (_, text) = sb.run(&mock, &["models"]);
    assert!(
        text.lines()
            .any(|l| l.trim_start().starts_with("gpt-sol-latest") && l.ends_with("->  gpt-6.1-sol")),
        "{text}"
    );
}

// ---------- which providers get cards ----------

#[test]
fn cards_default_to_codex_only_and_the_variable_adds_providers() {
    let sb = Sandbox::new();
    let mock = Mock::start(catalog(&[("gpt-6.1-sol", "list", true)]));
    let others = ["k3.md", "grok-4-7.md", "cursor.md", "composer-2-5.md"];

    // Default: Codex models and the gpt aliases only.
    let (ok, out) = sb.run(&mock, &["agents", "sync"]);
    assert!(ok, "{out}");
    let cards = sb.cards();
    assert!(cards.contains(&"gpt-6-luna.md".to_string()), "{cards:?}");
    assert!(
        cards.contains(&"gpt-sol-latest.md".to_string()),
        "{cards:?}"
    );
    for other in others {
        assert!(!cards.contains(&other.to_string()), "{other} {cards:?}");
    }

    // An unmarked file named like another provider's card is skipped, then kept.
    std::fs::write(sb.p("agents/k2-6.md"), "mine").unwrap();
    std::fs::write(sb.p("agents/mine.md"), "also mine").unwrap();
    let run_with = |value: &str| {
        let mut cmd = sb.cmd(&mock, &["agents", "sync"]);
        cmd.env("CCP_AGENT_CARD_PROVIDERS", value);
        let out = cmd.output().unwrap();
        assert!(out.status.success());
    };
    run_with("codex,kimi,grok,cursor");
    let cards = sb.cards();
    for other in others {
        assert!(cards.contains(&other.to_string()), "{other} {cards:?}");
    }
    assert!(cards.contains(&"gpt-6-luna.md".to_string()));
    assert_eq!(
        std::fs::read_to_string(sb.p("agents/k2-6.md")).unwrap(),
        "mine"
    );

    // Back to the default: the managed cards of the other providers go on the
    // next sync; unmarked files and the Codex cards stay.
    let (ok, out) = sb.run(&mock, &["agents", "sync"]);
    assert!(ok, "{out}");
    let cards = sb.cards();
    for other in others {
        assert!(!cards.contains(&other.to_string()), "{other} {cards:?}");
    }
    assert!(cards.contains(&"gpt-6-luna.md".to_string()));
    assert!(cards.contains(&"gpt-sol-latest.md".to_string()));
    assert_eq!(
        std::fs::read_to_string(sb.p("agents/k2-6.md")).unwrap(),
        "mine"
    );
    assert_eq!(
        std::fs::read_to_string(sb.p("agents/mine.md")).unwrap(),
        "also mine"
    );

    // `agents list` shows only managed cards.
    let (_, listing) = sb.run(&mock, &["agents", "list"]);
    assert!(
        !listing.contains("mine.md") && !listing.contains("k3.md"),
        "{listing}"
    );
}
