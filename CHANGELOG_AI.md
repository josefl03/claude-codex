# AI changelog

Past work done with AI assistance, newest first. Entries are not edited retroactively. The present state is described in [ARCHITECTURE.md](ARCHITECTURE.md).

## 2026-10-20: prebuilt binaries and automatic releases

Why: no release existed, so the one-liner always fell back to `cargo install`. The inherited `release.yml` only ran on `v*` tag pushes, built glibc binaries (the upstream once shipped one that demanded GLIBC 2.39) and had never run in this repository.

Changed:

- `.github/workflows/release.yml`: now `workflow_dispatch` and `workflow_call` (inputs `tag`, `sha`). Jobs `prepare` (tag shape, tag equals `Cargo.toml` version, no existing release or tag), `checks`, `build` (matrix of `x86_64`/`aarch64-unknown-linux-musl` on `ubuntu-latest`/`ubuntu-24.04-arm` with `musl-tools`, and the two Apple targets; static check with `file`/`ldd`; archive test; `actions/attest-build-provenance@v3`) and `release` (`SHA256SUMS`, asset-name check, notes, `gh release create --target <sha>`). Windows targets removed. Per-tag concurrency group; permissions `contents: read` by default, `contents: write` only in `release`, `id-token`/`attestations: write` only in `build`.
- `.github/workflows/auto-release.yml` (new): on a push to `main` touching `Cargo.toml`, starts `release.yml` with `gh workflow run` when `v<version>` has no release. A tag pushed with `GITHUB_TOKEN` would not trigger anything, hence dispatch (`actions: write` only in that job).
- `.github/workflows/ci.yml`: job `versions` (Cargo.toml vs Cargo.lock, asset names).
- `scripts/check-release-assets.sh` (new) and `scripts/release-notes.sh` (rewritten: the GitHub compare API 404s because this repository is not a GitHub fork of the upstream; it now uses `CHANGELOG.md` and local git history).
- Docs: `docs/releasing.md` (new), README install section, AGENTS.md, ARCHITECTURE.md, `docs/development.md`, release skill, CHANGELOG.md.

Found while testing in GitHub: the first run failed because a trailing comment on the `version` line leaked into the tag (the sed pattern now stops at the closing quote). `Cargo.toml` gained a comment line above `version`.

Verified: release v0.6.0 was created by the automatic path (push to `main`, `Auto release`, `Release`) at commit f65878c; the Linux amd64 asset checked against its `.sha256`, `./claude-codex version` prints 0.6.0, `file` says static-pie, and the real one-liner downloaded the release (no cargo fallback) in a sandbox. Not verified: macOS and Linux arm64 binaries were built and ran `--version` in CI but were not run elsewhere; `gh attestation verify` could not be run locally (gh 2.46), the attestation exists in the API.

## 2026-10-20: automatic linger, single curl-bash installer, v0.6.0

Why: the service was a systemd user unit but only started at login; `sudo loginctl enable-linger` was a manual step, and it turned out that plain `loginctl enable-linger` works for the user without sudo in a local session. The installer also needed to be one command that leaves everything configured.

Added:

- `src/linger.rs`: `Loginctl` trait (`is_enabled`, `enable`, `disable`), `SystemLoginctl`, `system()` (None off Linux and under `CCP_SERVICE_DIR`), `ensure`/`release` and a test-only `FakeLoginctl`. Never calls sudo.
- `setup::linger_step` (after the service step, also in dry run), `setup::release_linger` used by `disable`, `setup --undo` and `uninstall` (new step "linger"); `SetupState.linger_enabled_by_setup` (serde default, so older `setup.json` still loads). Failure to enable is a "Still to do" item with `loginctl enable-linger` and the `sudo` form as alternative text. `status` shows `on (starts at boot; enabled by setup)`, `... not enabled by setup`, or `off`. `service::install_quiet` skips the hint of `service install`, which no longer says sudo.
- Tests: 9 unit tests in `setup.rs` and 7 in `linger.rs` with the fake (already on, enabled, failure, dry run, release only when recorded, failure keeps the record, no backend, old JSON), 3 integration tests in `tests/setup.rs` (sandbox never touches or mentions linger; old record; record present in sandbox is kept and `loginctl` not run), 1 status unit test.
- `scripts/install.sh` rewritten as the only installer (see ARCHITECTURE.md, "Installer and releases"). Tested in a sandbox with `CLAUDE_CODEX_LOCAL_BINARY`, a fake `curl` serving a release (good and bad checksum), a fake `cargo` for the fallback (404 release), no `cargo` at all (error with two options), non-writable install dir, `CLAUDE_CODEX_NO_SETUP=1`, login present and absent. `shellcheck` (shellcheck-py, installed `pip install shellcheck-py` in a temporary venv): clean.
- Version 0.6.0 (Cargo.toml, Cargo.lock, CHANGELOG.md). `release.yml` got a step that checks the asset names. README rewritten, `docs/examples.md` added with real output (status, models, a card, two `claude -p` runs against the live proxy).

Decisions and limits:

- Linger is never disabled unless recorded; with `CCP_SERVICE_DIR` set and a record present, `disable` keeps the record and says linger was left as it is.
- No release of `josefl03/claude-codex` exists yet (the API returned 404), so today the one-liner takes the `cargo install --git` path. Creating the `v0.6.0` tag is left to the maintainer.
- Not verified: the real `loginctl enable-linger`/`disable-linger` calls from the new code (tests use a fake and the real machine's linger must stay on), macOS, and the release workflow itself (it only runs on a tag).

## 2026-10-18: `enable`/`disable`/`status` and `-latest` model aliases

Why: the only ways to stop using the proxy were `setup --undo` (reverts the record) or `uninstall` (removes everything); there was no cheap off switch that keeps the install. And choosing "the newest sol" meant knowing the version and restarting Claude Code whenever a model came out.

Added:

- `src/lifecycle.rs`: `claude-codex enable [--dry-run] [--force]` (runs `setup::run_setup_as` with the title "Enable"), `disable [--dry-run]` and `status`. `disable` order: `disabled` marker in the state directory, `settings.json` revert (`unmerge_env`; backup bytes via `uninstall::restored_text`; created file removed), managed cards removed, service stopped and disabled with the unit file kept (new `service::ServiceCommand::Disable`), reverted entries cleared from `setup.json`, a listener left on the port reported and not killed. A BASE_URL for this proxy that the user wrote is left with a warning. `service::inspect` gives `status` a read-only view of the service. `agent_cards::sync_logged` returns early while the marker exists; `agents sync` prints a note. `setup`, `setup --undo` and `uninstall` clear the marker; `setup --undo` now also restores the original bytes when it can. `scripts/install.sh` runs `enable`.
- `src/latest.rs`: the pure parse/compare/pick algorithm; `Registry::{latest_aliases, resolve_latest, no_latest_candidates_message}`; resolution in `server.rs` after `normalize_incoming_model` for Messages and `count_tokens` (log `latest alias resolved` with alias and model; 400 when no candidate); aliases in `/v1/models`; `agent_cards::alias_cards` and `CardModel.latest`; `claude-codex models` prints the table. Only the `gpt` family (the Codex models) has aliases; ids that do not parse and `fast` copies never take part; variants (`mini`, `codex`, `spark`, ...) have only their own alias. `CCP_LATEST_ALIASES=0` disables everything.
- Tests: 14 in `tests/setup.rs` (enable/disable/enable cycle byte for byte, foreign and replaced URLs, created settings file, binary/credentials/caches kept, idempotence, dry run, manual listener, serve honoring the marker, state table per command, status), 3 in `tests/server.rs` (alias reaches the provider as the real model on both routes, 400, listing and switch-off), 3 in `tests/model_discovery.rs` (`gpt-7-sol` takes over without restart, card model line stable, aliases off retire cards), unit tests in `latest.rs`, `registry.rs`, `agent_cards.rs`, `lifecycle.rs`.

Changed: README (enable/disable/status and `-latest` in few lines, 1500 words or fewer), `docs/usage.md` (state table, alias rules and table), `ARCHITECTURE.md`, `AGENTS.md` (invariants), `docs/development.md`, `CHANGELOG.md`. `known_model_reaches_codex_provider` no longer uses the developer's real Codex login.

Decisions and limits:

- Cards default to the Codex provider (`CCP_AGENT_CARD_PROVIDERS`, `agent_cards::eligible_models` takes the provider list); managed cards of other providers are retired by the normal diff on the next sync. Test: `cards_default_to_codex_only_and_the_variable_adds_providers`.
- Ties on equal versions go to the alphabetically first id; `6` and `6.0` are one version.
- The OpenAI-compatible routes (`/v1/responses`, `/v1/chat/completions`) do not resolve aliases; only the Anthropic Messages path used by Claude Code does.
- Not covered by tests: the real `systemctl`/`launchctl` calls of `disable` and `service disable` (tests run with `CCP_SERVICE_DIR`, which skips them) and macOS.
- Suggestion, not done: pointing `ANTHROPIC_DEFAULT_SONNET_MODEL` at `gpt-sol-latest` (or opus at `gpt-latest`) would make that slot follow the newest tier without editing anything; the existing alias lists were left as they are.

## 2026-10-18: `uninstall`, loopback URL aliases, agent cards without the `ccp-` prefix

Why: `setup` had no inverse for a whole install (service, cards, settings, binary); a localhost alias of the proxy address was reported as a conflict or half handled; and the `ccp-` file prefix made every card name ugly in Claude Code's agent list.

Added:

- `src/uninstall.rs` and `claude-codex uninstall [--dry-run] [--yes] [--purge] [--purge-credentials] [--keep-binary]`. Six independent steps (service, cards, settings, state dir, credentials, binary), each reported with a check mark, dash or cross; any failure gives exit code 1. Without `--yes` it asks on a terminal and refuses when stdin is not one. `settings.json` is restored from `setup.json`; when the result equals the pre-setup backup the backup's bytes are written back, and a file setup created is removed (`settings_created` is new in `setup.json`). A manual `serve` is only reported. The binary is deleted only from `~/.local/bin`, `~/.cargo/bin`, `/usr/local/bin` (not in sandbox mode) or `CLAUDE_CODEX_INSTALL_DIR`, never from a `target/` path.
- `setup::same_base_url`: `localhost`, `127.0.0.1` and `[::1]` with the same effective port (default 80) are one address; extra path, query, userinfo, other scheme or other port is not. Report text is now "already configured".
- `tests/setup.rs`: the sandbox now sets `PORT` to a free port (nothing probes the default one) and has `cmd_with_binary` for copies of the binary; 20 new tests for uninstall and aliases (byte-for-byte restore, idempotence, dry run, no TTY, binary rules, purge levels with a simulated `~/.codex`, failing step, manual listener).

Changed:

- Agent cards are `<model>.md` with `name: <model>`. `is_managed` needs the marker as the first body line after the frontmatter; `scan` reads every plain `.md` (no symlinks, size cap). New `Action::Reserved` and `RESERVED_NAMES`; `apply` re-checks the marker before overwriting. Old `ccp-*.md` managed cards (including the Spanish marker) are removed by the normal diff on the next sync. Skips are logged once per file.
- README (Uninstall section, still under 1500 words), `docs/usage.md`, `ARCHITECTURE.md`, `AGENTS.md`, `CHANGELOG.md`, `scripts/install.sh` messages.

Decisions and limits:

- `--purge-credentials` deletes only exact known files in the proxy config directory, then removes directories only if empty. macOS Keychain entries are not removed.
- A binary reached through a symlink (for example `just install-dev`) resolves into `target/` and is kept.
- The agent card marker rule is stricter than before (first body line), so a user file that merely quotes the marker is never treated as ours.

## 2026-10-18: own Codex model discovery, English card marker

Why: the model list only followed the Codex CLI's `models_cache.json`, so a new model appeared only after the user ran the Codex CLI. The proxy now asks the Codex backend itself, which makes "new models show up on their own" true without any other tool. The card marker was also still in Spanish in a public repository.

Added:

- `src/model_discovery.rs`: `GET <codex api>/models?client_version=<x>` through `CodexHttpClient::get_models` (same OAuth bearer, `ChatGPT-Account-Id`, originator and user agent as Responses requests; one forced token refresh on a 401). `parse_catalog` keeps slim entries and rejects a body with no listed, API-capable model. The result is written atomically, only when changed, to `codex-models.json` in the state directory. Never written to `~/.codex`.
- Background loop (`spawn_background_tasks`): refreshes at startup and every `CCP_MODEL_REFRESH_INTERVAL_SECS` (default 900, `0` = startup only with a few retries), retries failures after 30 s doubling to 15 min, diffs the effective list each pass, syncs cards immediately on a change and logs `new model discovered: X, card created` / `model no longer available: X, card removed`. `CCP_MODEL_DISCOVERY=0` disables the fetch; `CCP_CODEX_CLIENT_VERSION` overrides the version sent.
- `claude-codex models --refresh`; `agents sync` refreshes first (skipped with `--dry-run`).
- `model_allowlist.rs`: effective list is bundled, Codex CLI cache and own cache; both file caches are keyed by mtime and size and keep the last good parse.
- `tests/model_discovery.rs`: a local mock of `/models`, all paths redirected.

Changed:

- Agent card marker is now `<!-- managed-by: claude-codex; do not edit -->`. The previous `no editar` text is still recognized as managed (`LEGACY_MANAGED_MARKER`), so sync rewrites old cards. Unit test `legacy_marker_is_still_managed_and_sync_rewrites_it`.
- README, `ARCHITECTURE.md`, `docs/usage.md`, `docs/development.md`, `AGENTS.md` and `CHANGELOG.md` updated.

Decisions and limits:

- Kimi and Grok: no listing endpoint verified and fixed translator allowlists; Cursor would need gRPC/protobuf. They stay static.
- Opus/Sonnet/Haiku/Fable aliases keep fixed preference lists; a new model gets a card and is routable but is not chosen as an alias target.
- Claude Code reads its agent list at startup, so a new card needs a Claude Code restart.
- Any failure keeps the last good list, so a bad response can never delete cards.

## 2026-10-18: autostart service, subagent cards, one-shot setup, public repo files

Why: the fork's value was dual-subscription routing, but installing it meant building, writing a service by hand, editing `settings.json` and knowing the model ids. The goal of this change was one command to install and start it, plus a way to use any non-Claude model as a Claude Code subagent. The repository is also being published as `josefl03/claude-codex`, so it needed public-facing documentation.

Added:

- `claude-codex service install|uninstall|status|restart` (`src/service.rs`): systemd user unit on Linux, LaunchAgent on macOS, both running `serve --no-monitor`. `CCP_SERVICE_DIR` gives a file-only mode for tests.
- `claude-codex agents sync|list|clean` and a background sync inside `serve` (`src/agent_cards.rs`): one `ccp-<model>.md` card per non-Claude model in `~/.claude/agents/`. Only files with the `managed-by: claude-codex` marker are touched. Controlled by `CCP_AGENT_CARDS`, `CCP_AGENTS_DIR`, `CCP_AGENT_SYNC_INTERVAL_SECS`.
- `claude-codex setup [--dry-run|--undo|--force|--no-service|--no-agents|--no-claude-settings]` (`src/setup.rs`): installs the service, syncs cards and merges `ANTHROPIC_BASE_URL` into Claude Code's `settings.json`. Records changes in `setup.json` so `--undo` reverts only them. Never overwrites a different `ANTHROPIC_BASE_URL` without `--force`; never writes credentials. `CCP_SETUP_SANDBOX` makes it refuse to run unless every path is redirected.
- Live model list: `Registry::refresh` and an mtime/size-keyed cache of the Codex CLI's `models_cache.json` (`model_allowlist.rs`), so new Codex models are routable and get cards without restarting. `ALLOWED_MODELS` became the function `allowed_models()`.
- `scripts/install.sh` runs `setup` after installing, honors `CLAUDE_CODEX_NO_SETUP=1`, and takes the release repository from `CLAUDE_CODEX_REPO` (default `josefl03/claude-codex`).
- Tests in `tests/setup.rs` and `tests/cli.rs`, all sandboxed.
- Documentation and repository files: new README, `ARCHITECTURE.md`, `docs/development.md`, `docs/usage.md`, `CONTRIBUTING.md`, `SECURITY.md`, `CODE_OF_CONDUCT.md`, issue and pull request templates, updated `AGENTS.md`, `LICENSE` (added a copyright line, original kept), `Cargo.toml` repository metadata.

Decisions:

- `cargo install` cannot run post-install steps, hence a separate idempotent `setup` command rather than relying on the installer script.
- Cards carry no `tools:` line, so subagents inherit Claude Code's tools.
- The README quickstart uses `cargo install --git` because the repository has no releases yet; the `curl | bash` route is documented as available after the first release.
- No claims of terms-of-service compliance are made anywhere; the README has a disclaimer instead.
