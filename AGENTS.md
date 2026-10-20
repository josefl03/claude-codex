# AGENTS.md

Guidance for Claude Code and other AI tools working on this repository.

## What this is

A small local proxy that lets Claude Code talk to more than one backend at once,
chosen per request by the model name. Claude Code already speaks the Anthropic
Messages API, so the proxy speaks it too and forwards or translates each request:

- `claude-*` models go to Anthropic as a transparent passthrough that reuses
  Claude Code's own subscription login. No API key, no translation.
- `gpt-*` ids go to the Codex backend using the
  ChatGPT subscription that the Codex CLI already logged in.

The headline use is running the opus slot on a Claude subscription and the
sonnet slot on a ChatGPT/Codex subscription in the same session, switching
freely mid-conversation.

This is a fork of `fcakyon/claude-code-with-codex`, itself a fork of
`raine/claude-code-proxy`. The dual Claude plus Codex routing, the
reasoning-across-switch handling, and the Codex-CLI credential model come from
the first fork. This repository (`josefl03/claude-codex`) adds the autostart
service, `claude-codex setup`, the subagent cards, and a model list that follows
the Codex CLI cache while running. The map of modules and flows is in
`ARCHITECTURE.md`; contributor workflow is in `docs/development.md`.

## Architecture

- `src/registry.rs` picks a provider for each request. `claude-*` and the opus
  and sonnet aliases resolve to the Anthropic passthrough; other ids match a
  backend exactly; an unknown id returns a 400 that lists the supported ids.
  Codex ids come from `model_allowlist::allowed_models()`: the bundled list
  plus the listed, API-capable slugs in the Codex CLI's `models_cache.json` and
  in the proxy's own `codex-models.json`, each re-read when its mtime or size
  changes. `Registry::refresh` applies it.
- `src/providers/anthropic/mod.rs` is the passthrough. It relays the original
  body and headers to `api.anthropic.com` and streams the reply back. It holds
  no Anthropic credentials.
- `src/providers/codex/*` translates the Anthropic Messages API to the OpenAI
  Responses API and back.
- `src/providers/translate_shared.rs` holds types and helpers shared by the
  translators, including the reasoning-tag helpers.
- `src/agent_cards.rs` creates, updates and removes `<model>.md` subagent
  cards in `~/.claude/agents/` (`claude-codex agents ...` and a background sync
  started by `serve`).
- `src/model_discovery.rs` fetches the Codex model catalog itself
  (`GET .../codex/models?client_version=`) at `serve` startup and on an interval,
  stores it in `codex-models.json` in the state directory, and runs the loop
  that refreshes models and then syncs the cards.
- `src/service.rs` manages the autostart service (systemd user unit, macOS
  LaunchAgent) behind `claude-codex service ...`.
- `src/linger.rs` owns systemd linger behind the injectable `Loginctl` trait; `setup.rs` records `linger_enabled_by_setup` and `disable`, `setup --undo` and `uninstall` revert it only when recorded.
- `src/setup.rs` implements `claude-codex setup`: service, cards and the merge of
  `ANTHROPIC_BASE_URL` into Claude Code's `settings.json`, with `--undo`.
- `src/lifecycle.rs` implements `enable` (the setup logic plus clearing the
  `disabled` marker), `disable` (marker, `settings.json` revert, cards, service
  stop and disable with the unit file kept) and `status`.
- `src/latest.rs` is the pure algorithm behind the `<family>-latest` and
  `<family>-<tier>-latest` model aliases; `Registry::resolve_latest` applies it
  and `server.rs` resolves the alias before routing.
- `src/uninstall.rs` implements `claude-codex uninstall`, the inverse of `setup` and
  `scripts/install.sh` (service, cards, settings, optional purge, binary).

## Invariants to preserve

- The Anthropic passthrough must stay byte-exact for normal traffic. The only
  rewrite it performs is converting a signature-less `thinking` block into a
  tagged `text` block, and it reserializes only when such a block is present.
  Anything that reserializes every request would evict Anthropic's prompt cache.
- Reasoning stays portable across a model switch. A `thinking` block written by
  one backend cannot be replayed to the other in native form (Anthropic rejects
  a signature-less `thinking` block; the Responses API has no `thinking`
  container). Both translators convert it to text wrapped in the shared
  `REASONING_OPEN` and `REASONING_CLOSE` tags via `wrap_reasoning`. Keep this
  deterministic so the rewritten prefix is byte-stable turn to turn.
- Codex credentials come only from the Codex CLI's `~/.codex/auth.json`. The
  proxy has no Codex login of its own. Token refresh writes back to that file so
  the Codex CLI keeps working (OpenAI rotates the refresh token on use).

- Never write into `~/.codex`. Model discovery stores its result only in the
  proxy's own state directory; the single permitted write to the Codex CLI's
  directory is the token write-back to `auth.json`. A failed discovery (network,
  401/403, 429, bad JSON, empty catalog) must keep the last good list and leave
  the cards untouched.
- Only files carrying the `managed-by: claude-codex` marker (the current
  `do not edit` text or the older `no editar` one, as the first body line after
  the frontmatter) are rewritten or deleted in the agents directory. There is no
  file-name prefix: the marker is the only proof of ownership. Never touch any
  other file there; a name taken by an unmarked file, and the reserved names
  (`general-purpose`, `explore`, `plan`, `statusline-setup`, `claude-code-guide`,
  `claude-*`, model aliases), are skipped with a warning. Writes and removals
  re-check the marker.
- `setup` never overwrites an `ANTHROPIC_BASE_URL` that points somewhere else
  unless `--force` is given, never writes `ANTHROPIC_API_KEY` or
  `ANTHROPIC_AUTH_TOKEN`, and `--undo` only reverts what `setup.json` recorded,
  and only where the value is still the one setup wrote. It treats `localhost`,
  `127.0.0.1` and `[::1]` with the same effective port as the same address.
- `disable` must always take out of `settings.json` the keys that `enable`/`setup`
  added (and restore a value `--force` replaced), whatever else fails: leaving
  `ANTHROPIC_BASE_URL` pointing at a stopped proxy breaks Claude Code. It never
  deletes the binary, credentials, caches or `setup.json`, never kills a manual
  `serve`, and while `<state dir>/disabled` exists `serve` must not sync cards.
  `enable`, `disable` and `status` use `setup::Locations::resolve`, so the sandbox
  guard covers them. Each command leaves the state in the table in
  `docs/usage.md`; `tests/setup.rs` checks it.
- The `-latest` aliases do not alter the existing ones: `opus`, `sonnet`, `haiku`
  and `fable` and their `OPUS_TARGETS`-style lists are untouched. An alias is
  resolved only in the Messages and `count_tokens` path, before
  `provider_for_model`; a real model id equal to an alias wins; the card's
  `model:` line is the alias and never the target. `CCP_LATEST_ALIASES=0` removes
  the aliases everywhere.
- `uninstall` is tolerant (a failed step never stops the others) and conservative:
  no `ANTHROPIC_BASE_URL` change without a setup record; state is deleted only with
  `--purge`, the proxy's own provider logins only with `--purge-credentials`, by
  exact path; `~/.codex` never; no `serve` process is ever killed; the binary only
  from a known install directory and never from `target/`. Its only recursive
  delete is the state directory named `claude-code-proxy`.
- Never invoke `sudo`, anywhere (code, `scripts/install.sh`, tests). Linger is enabled with plain `loginctl enable-linger`; if that fails, `setup` still succeeds and only prints the manual command (the `sudo` form is text for the user, never executed). Linger is turned off again (`disable`, `setup --undo`, `uninstall`) only when `setup.json` says setup turned it on; a linger the user enabled is never touched. All `loginctl` calls go through `linger::Loginctl`, `linger::system()` is `None` under `CCP_SERVICE_DIR` and off Linux, and tests use `FakeLoginctl`, never the real command.
- `scripts/install.sh` is the only installer: everything inside functions with `main "$@"` last, no prompts, no sudo, child commands read `/dev/null`, asset names `claude-codex-<os>-<arch>.tar.gz` and `.sha256` matching `release.yml` (never rename an asset without changing `install.sh`; `scripts/check-release-assets.sh` checks both), and it never runs `codex login`.
- Code and tests that write outside the proxy's own directories must honor an env
  override per location and must never touch the real home directory. See the
  safety rule in `docs/development.md`.
- The listener has no inbound authentication and binds to `127.0.0.1` by default.
  Do not change the default bind address.

## Naming and distribution

The crate, the installed command, and the library target are all `claude-codex`
(`claude_codex` for the library, derived automatically from the package name).
The crates.io package is `claude-codex`. The GitHub repository is
`josefl03/claude-codex`; the upstream forks keep their own names.

Some strings deliberately keep the old `claude-code-proxy` name because they are
compatibility contracts, not the user-facing name. Do not rename them in a
future cleanup:

- The on-disk config and data directory and the macOS Keychain service, in
  `paths.rs` and the provider auth modules. Renaming these orphans saved
  logins. `paths.rs` already has
  `legacy_config_dir` as the migration hook if this is ever changed on purpose.
- The Codex `ORIGINATOR` and `User-Agent` in `providers/codex`. These go to the
  ChatGPT backend, so keep them stable to avoid changing what the server sees.

Install paths: `cargo install --git https://github.com/josefl03/claude-codex --locked`
followed by `claude-codex setup`; `cargo install claude-codex` once published to
crates.io; and prebuilt binaries from `.github/workflows/release.yml` (static musl on Linux, started by
`.github/workflows/auto-release.yml` when a push to `main` changes the `Cargo.toml` version, or by hand
with `workflow_dispatch`), installed by `scripts/install.sh`, the one-liner (repository configurable
through `CLAUDE_CODEX_REPO`; falls back to `cargo install --git` when no release asset exists).
The workflows use the default `GITHUB_TOKEN` and need no secrets or Homebrew tap.

How a release is published and verified: [docs/releasing.md](docs/releasing.md). Release steps for an
agent live in the `release` skill at `.agents/skills/release/SKILL.md`.

## Build and test

- `cargo build`
- `cargo test -- --test-threads=1`. Run tests single-threaded. A few config
  tests mutate process-wide environment variables and race under the default
  parallel runner. This is pre-existing and unrelated to product behavior.
- `just check` runs format, clippy, and tests together where the toolchain has
  clippy installed.
- Keep tests hermetic: toy credentials and mock upstreams, never the real
  `~/.codex/auth.json` or a live backend. Tests of `service`, `agents` and
  `setup` redirect every path (`CCP_SERVICE_DIR`, `CCP_AGENTS_DIR`,
  `CLAUDE_CONFIG_DIR`, `CCP_CONFIG_DIR`, `CCP_AGENT_CARDS=0`,
  `CCP_SETUP_SANDBOX`); model-discovery tests also point `CCP_CODEX_BASE_URL` at
  a local mock. See `docs/development.md`.
- Keep `ARCHITECTURE.md` current (rewrite it) and add an entry at the top of
  `CHANGELOG_AI.md` after changes.

## Gotchas

- In the dual setup, `ANTHROPIC_AUTH_TOKEN` and `ANTHROPIC_API_KEY` must be
  unset. Claude Code forwards its subscription login for `claude-*` only when no
  explicit token is present. Setting either one sends that value instead and the
  Anthropic route returns 401.
- Claude Code's web search is a client-side `WebSearch` function tool. The
  hosted `web_search_20250305` tool only appears inside an isolated, history-free
  inner call that Claude Code makes to run the search, so its reconstructed
  `server_tool_use` and `web_search_tool_result` blocks never enter the outer
  transcript. The passthrough logs `hosted_web_search_in_history` if that ever
  changes, which would mean this assumption needs rechecking.

## Known limitation

Switching backends in the middle of an active tool call (for example pressing
Esc during a codex tool use, then switching to a Claude model and continuing)
can fail. Anthropic requires a leading signed `thinking` block in that position
and no valid signature can be produced for reasoning that came from another
backend. This is rare and not worked around.

## Style

Match the surrounding code. `reqwest` is built without gzip or brotli so bodies
are never auto-decompressed, and with rustls so no OS keychain is touched. Keep
new code in the same shape as the module it lives in. Stay close to upstream and
avoid style-only divergence, so upstream merges stay cheap.
