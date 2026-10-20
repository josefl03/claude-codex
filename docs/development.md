# Development guide

Read [ARCHITECTURE.md](../ARCHITECTURE.md) first for the module map and [AGENTS.md](../AGENTS.md) for the invariants that must not break.

## Build and check

```sh
cargo build
cargo test -- --test-threads=1
just check        # fmt check, clippy -D warnings, build, tests (via checkle)
```

- `just check` needs `just` and `checkle` (`cargo install just checkle --locked`) plus clippy and rustfmt. CI runs `just check-ci`, which also fails if the run leaves uncommitted changes.
- Run tests with `--test-threads=1`. Some config tests mutate process-wide environment variables and race under the default parallel runner.
- `just install-dev` symlinks the debug build into `~/.cargo/bin`.
- Optional: `just install-hooks` installs the pre-commit shim (`hooks/pre-commit` runs `checkle pre-commit`).

## Safety rule for tests: never touch the real HOME

`service install`, `setup`, `agents sync` and `agents clean` write to the user's real systemd units, `~/.claude/settings.json` and `~/.claude/agents/`. Tests and manual experiments must redirect every location.

| Variable | Redirects |
|---|---|
| `HOME`, `XDG_STATE_HOME`, `XDG_CONFIG_HOME` | home, state (`setup.json`, logs), config base |
| `CCP_CONFIG_DIR` | proxy config directory (also holds `service.env`) |
| `CCP_SERVICE_DIR` | where the unit/plist is written; also disables every `systemctl`/`launchctl`/`loginctl` call |
| `CCP_AGENTS_DIR` | agent cards directory |
| `CLAUDE_CONFIG_DIR` | Claude Code config directory (`settings.json`) |
| `CCP_CODEX_AUTH_FILE` | Codex CLI credential file (and, next to it, `models_cache.json`) |
| `CCP_CODEX_BASE_URL` | Codex backend base URL; point it at a local mock to test model discovery |
| `CCP_MODEL_DISCOVERY=0` | stops `serve` fetching the model list |
| `CCP_AGENT_CARDS=0` | disables the background card sync in `serve` |
| `CCP_AGENT_CARD_PROVIDERS` | providers that get cards (default `codex`) |
| `CCP_LATEST_ALIASES=0` | disables the `-latest` model aliases (listing, routing, cards) |
| `CCP_SETUP_SANDBOX=1` | makes `setup` refuse to run unless all eight redirect variables above (`HOME`, `XDG_STATE_HOME`, `CCP_CONFIG_DIR`, `CCP_SERVICE_DIR`, `CCP_AGENTS_DIR`, `CLAUDE_CONFIG_DIR`, `CCP_CODEX_AUTH_FILE`, `CCP_AGENT_CARDS`) are set and non-empty |

Rules:

- Integration tests for `setup` go through the single `Sandbox::cmd` helper in `tests/setup.rs`, which calls `env_clear()` and sets all of the above. Add new setup tests through it; do not build a `Command` elsewhere.
- Tests for `service` and `agents` set `CCP_SERVICE_DIR` / `CCP_AGENTS_DIR` to a `tempfile` directory (see `tests/cli.rs`).
- When you run the binary by hand while developing, export `CCP_SETUP_SANDBOX=1` plus the variables, or use `--dry-run`. Never run `service install` or `setup` unsandboxed on a machine whose configuration you care about unless you intend to.
- Unit tests of pure functions (`render_systemd_unit`, `plan`, `merge_env`) need no filesystem.
- Linger: unit-test `linger_step`/`release_linger` with `linger::fake::FakeLoginctl`; integration tests run with `CCP_SERVICE_DIR`, so the real `loginctl` is never called. Do not run `enable`, `disable`, `setup` or `uninstall` unsandboxed on a machine with a live service.
- `scripts/install.sh` is tested by running it with `CLAUDE_CODEX_LOCAL_BINARY`, a temporary `HOME` and every redirect variable above (plus `CCP_SETUP_SANDBOX=1`); `bash -n` and `shellcheck` should pass.
- `enable`, `disable` and `status` go through the same guard and tests: add their cases to `tests/setup.rs` via `Sandbox::cmd`. `disable` and `status` also read `<XDG_STATE_HOME>/claude-code-proxy/disabled`, so it must stay inside the sandbox.
- Keep tests hermetic: toy credentials and mock upstreams, never the real `~/.codex/auth.json` or a live backend. `cli_models()` returns an empty list under `cfg!(test)` so unit tests do not depend on a developer's account.

## Testing model discovery

`tests/model_discovery.rs` runs the real binary and library against a local mock of `GET /models`. Each process gets a temporary `HOME`, `XDG_STATE_HOME`, `XDG_CONFIG_HOME`, `CCP_CONFIG_DIR`, `CCP_SERVICE_DIR`, `CCP_AGENTS_DIR` and `CCP_CODEX_AUTH_FILE`, plus `CCP_CODEX_BASE_URL=http://127.0.0.1:<mock port>`, so nothing touches `~/.codex`, `~/.claude`, the service manager or a proxy on the default port. The mock records the request URI, `Authorization` and `ChatGPT-Account-Id`, and its status and body can be changed between calls to simulate a new model, a removed one, a 401, a 429 or garbage.

Run it with:

```sh
cargo test --test model_discovery -- --test-threads=1
```

When you add a case: build the sandbox the way the existing tests do, change the mock's body, run `models --refresh` or `agents sync` (or call `refresh_codex_with` directly), then assert on the `codex-models.json` in the temporary state directory (`<XDG_STATE_HOME>/claude-code-proxy/`) and on the cards. Check the failure paths too: a failed refresh must leave the cache file and the cards unchanged. `own_models()` returns an empty list under `cfg!(test)` so unit tests never read the developer's state directory.

## Syncing with upstream

The remotes are `origin` (this fork) and `upstream` (`raine/claude-code-proxy`). Upstream is where Codex translation work lands.

```sh
git fetch upstream
git log --oneline HEAD..upstream/main      # what is new
git merge upstream/main                    # merge, do not rebase published history
```

To keep merges cheap:

- Match upstream style; avoid style-only changes in files upstream owns (`server.rs`, `registry.rs`, `providers/*`, `tui.rs`).
- Keep fork-only logic in the fork-only modules (`service.rs`, `setup.rs`, `agent_cards.rs`, `model_discovery.rs`, `providers/anthropic`) and small, clearly delimited hooks elsewhere.
- Expect conflicts in `registry.rs` (the model table is behind a `RwLock` here), `model_allowlist.rs` (mtime-keyed caches for the Codex CLI file and the proxy's own `codex-models.json`, `allowed_models()` function instead of a static) and `main.rs` (extra subcommands and the card-sync task). Resolve toward keeping both behaviors.
- The code inherited from upstream contains other backends that this fork does not support and does not test.
- After merging: `just check`, then add a `CHANGELOG.md` line naming the upstream version, and a `CHANGELOG_AI.md` entry.
- Do not rename the compatibility strings listed in AGENTS.md ("Naming and distribution").

## Adding or changing a model

- Codex: add the slug to `BUNDLED_MODELS` in `src/providers/codex/translate/model_allowlist.rs`. Models the account's catalog lists (found by `src/model_discovery.rs`, or read from the Codex CLI's `models_cache.json`) appear without code changes. Alias targets (`OPUS_TARGETS` and siblings in that file) decide where Claude aliases go when `CCP_ALIAS_PROVIDER=codex`.
- New Claude id: add it to `ANTHROPIC_STYLE_ALIASES` in `src/registry.rs`; it also stops it from getting an agent card.
- Run `claude-codex models` and `claude-codex agents sync --dry-run` (with `CCP_AGENTS_DIR` pointing to a temp directory) to see the effect.

## Adding a subcommand

1. Put the logic in a library module `src/<name>.rs` and export it from `src/lib.rs`. Keep content generation pure and I/O in thin functions so it is unit-testable.
2. Derive `clap::Subcommand` / `Args` there, add the variant to `Commands` in `src/main.rs`, and dispatch in `main`.
3. If it writes outside the proxy's own directories, honor an env override for each location, add the new variable to the sandbox table above and, for `setup`-like commands, to `SANDBOX_REQUIRED`.
4. Add an integration test using `assert_cmd` with `env_clear()` and temp directories.

## Documentation upkeep

- `ARCHITECTURE.md` describes the present and is rewritten whole.
- `CHANGELOG_AI.md` describes the past; add an entry at the top, never edit old ones.
- `CHANGELOG.md` gets a bullet per user-visible change under `Unreleased` until a release (see `.agents/skills/release/SKILL.md` and [releasing.md](releasing.md)). Pushing a new version to `main` publishes the release automatically; do not change release asset names without changing `scripts/install.sh`.
- Docs and code comments are in English; the repository is public.
