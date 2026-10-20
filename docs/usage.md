# Usage guide

The [README](../README.md) has the quickstart. This page has the details.

## Switching models

- Inside Claude Code: `/model gpt-6-astra[1m]` for Codex, `/model claude-opus-5-5` for Claude. The `[1m]` suffix enables Claude Code's larger-context mode; the proxy strips it before routing.
- For one new session: set `ANTHROPIC_MODEL` when launching Claude Code.
- List what the proxy exposes and the current `-latest` aliases: `claude-codex models`.
- New Codex models: the proxy discovers them itself (see [Model discovery](#model-discovery)). It also still reads the Codex CLI's `models_cache.json`, so either source is enough, and neither needs a proxy restart.
- When a Claude alias is sent to Codex, the proxy picks the first preferred model that your Codex catalog lists.

## Mixing both plans in one session

The default alias target is Anthropic, so `opus`, `sonnet` and `haiku` use your Claude plan. Remap a slot to a Codex id in Claude Code's environment to move it to your ChatGPT plan:

```sh
export ANTHROPIC_DEFAULT_OPUS_MODEL=claude-opus-5-5     # stays on Claude
export ANTHROPIC_DEFAULT_SONNET_MODEL=gpt-5.6-terra     # runs on Codex
export ANTHROPIC_DEFAULT_HAIKU_MODEL=claude-haiku-5-5   # stays on Claude
```

Do not set `ANTHROPIC_AUTH_TOKEN` or `ANTHROPIC_API_KEY`; Claude Code forwards its subscription login for `claude-*` models only when no explicit token is present.

## Install

```sh
curl -fsSL https://raw.githubusercontent.com/josefl03/claude-codex/main/scripts/install.sh | bash
```

`scripts/install.sh` is the only installer. It never uses sudo and never prompts. It installs into `~/.local/bin` (override with `CLAUDE_CODEX_INSTALL_DIR`; the directory must be writable), taking the latest release for `linux` or `darwin` on `amd64` or `arm64` and checking its `.sha256`. With no release or no asset for your platform it runs `cargo install --git https://github.com/josefl03/claude-codex --locked` when `cargo` exists, and otherwise stops with both options spelled out. Then it runs `claude-codex enable` (a failure is reported with the retry command, the install stays), checks the Codex login with `claude-codex codex auth status` (it never runs `codex login`; if the login or the Codex CLI is missing it prints `codex login` and the install commands `npm install -g @openai/codex` or `brew install --cask codex`), checks that `claude` is in `PATH`, and prints a summary and a "Still to do" list. If the install directory is not in `PATH` it prints the exact `export PATH=...` line.

Variables: `CLAUDE_CODEX_VERSION` (a tag such as `v0.6.0`), `CLAUDE_CODEX_REPO`, `CLAUDE_CODEX_INSTALL_DIR`, `CLAUDE_CODEX_NO_SETUP=1` (binary only), `CLAUDE_CODEX_LOCAL_BINARY` (testing). `just install` is for developers and runs `cargo install` on a checkout.

## Enable, disable and status

```sh
claude-codex enable [--dry-run] [--force]   # switch the proxy on (idempotent)
claude-codex disable [--dry-run]            # switch it off, keep the install
claude-codex status                         # one screen, changes nothing
```

**`enable`** makes sure that: the autostart service is installed (its unit rewritten with the path of the running binary), enabled and started; on Linux, systemd linger is turned on (unless it already is) so the service also starts at boot; the agent cards are synced and `serve` syncs them again; `env.ANTHROPIC_BASE_URL` (and `env.CLAUDE_CODE_ENABLE_GATEWAY_MODEL_DISCOVERY=1`) are in Claude Code's `settings.json`; and the `disabled` marker is gone. It is `setup` under another name and obeys the same safety rules: a different `ANTHROPIC_BASE_URL` stays unless you pass `--force`, a backup is made once, and invalid JSON is an error that leaves the file untouched. After `disable`, a previously `--force`d replacement needs `--force` again.

**`disable`** switches the proxy off without removing it, in this order, continuing past a failed step (the exit code is then non-zero):

1. writes the `disabled` marker (`<state dir>/disabled`) so a `serve` started by hand does not sync cards;
2. removes from `settings.json` the entries setup added, and puts back a value that `--force` replaced. This is not optional: leaving `ANTHROPIC_BASE_URL` pointing at a stopped proxy would break Claude Code. When the result equals the pre-setup backup, its exact bytes are written back; a `settings.json` that setup created and nothing else lives in is removed. A `ANTHROPIC_BASE_URL` for this proxy that the user wrote (setup never recorded it) is left, with a warning;
3. removes the managed agent cards (marker rule as always);
4. stops and disables the service (`systemctl --user disable --now`; `launchctl bootout` on macOS) and keeps the unit or plist file;
5. turns linger off if `setup.json` says setup turned it on (`loginctl disable-linger`), and clears that record; a linger you enabled yourself is left alone, and the output says which case applied;
6. forgets the reverted entries in `setup.json` but keeps the file.

It never deletes the binary, provider logins, caches or `setup.json`, and it does not stop a `serve` you started by hand: if something still listens on the port it says so. At the end it reminds you to restart Claude Code; run `enable` and restart Claude Code to switch back.

**`status`** prints: enabled or disabled (the marker), the service (installed, active, enabled), linger on Linux (on or off, and whether setup turned it on), the port and whether `/healthz` answers, whether `settings.json` has `ANTHROPIC_BASE_URL` and whether it is this proxy, the number of managed cards, the version and whether a Codex login exists. With `CCP_SERVICE_DIR` set the service manager is not queried and `status` says so.

State left by each command (checked by `tests/setup.rs`):

| Command | Binary | Service unit file | Service enabled and running | Linger (Linux) | Managed cards | `settings.json` entries | `setup.json` | `disabled` marker | Logins and caches |
|---|---|---|---|---|---|---|---|---|---|
| `install.sh`, `enable`, `setup` | kept or installed | written | yes | on (recorded as `linger_enabled_by_setup` only if setup turned it on) | synced | present | updated | removed | untouched |
| `disable` | kept | kept | no | off only if setup turned it on, else as it was | removed | removed (previous value restored) | kept, reverted entries cleared | written | untouched |
| `setup --undo` | kept | removed if setup installed it | no | off only if setup turned it on, else as it was | removed | removed | removed | removed | untouched |
| `uninstall` | removed | removed | no | off only if setup turned it on, else as it was | removed | removed | removed | removed | untouched; `--purge` deletes state, `--purge-credentials` also the proxy logins |

`~/.codex` is untouched by every command.

## `setup`

```sh
claude-codex setup               # service, subagent cards, Claude Code settings
claude-codex setup --dry-run     # preview
claude-codex setup --undo        # revert only what setup recorded
claude-codex setup --force       # replace a different ANTHROPIC_BASE_URL
```

Skip parts with `--no-service`, `--no-agents`, `--no-claude-settings`. Setup edits `settings.json` under `$CLAUDE_CONFIG_DIR` (default `~/.claude`): it adds `env.ANTHROPIC_BASE_URL` and `env.CLAUDE_CODE_ENABLE_GATEWAY_MODEL_DISCOVERY`, keeps everything else, writes `settings.json.claude-codex.bak` the first time, and never writes credentials. A different existing `ANTHROPIC_BASE_URL` is left alone unless you pass `--force`. `http://localhost:P`, `http://127.0.0.1:P` and `http://[::1]:P` (with or without a trailing slash) count as the same address when `P` is the port the proxy uses; setup then changes nothing, reports the key as already configured, and `--undo` will not remove it because setup did not add it. `setup` is what `enable` runs. Its record is `setup.json` in the state directory (`~/.local/state/claude-code-proxy/` on Linux).

To configure by hand instead, put this in `~/.claude/settings.json` and run `claude-codex serve`:

```json
{ "env": { "ANTHROPIC_BASE_URL": "http://127.0.0.1:18765" } }
```

## Uninstall

```sh
claude-codex uninstall               # asks for confirmation on a terminal
claude-codex uninstall --yes         # no prompt (required when stdin is not a terminal)
claude-codex uninstall --dry-run     # show what would be done, change nothing
```

It undoes `setup` and `scripts/install.sh` in this order. Each step is independent: one that fails does not stop the others, the summary marks each with a check mark, a dash (skipped) or a cross, and the exit code is non-zero if any failed. Running it twice is safe.

1. **Service.** Stops and disables the systemd user unit or LaunchAgent and deletes its file (same code as `service uninstall`). A manual `claude-codex serve` is never stopped; if something still listens on the configured port, uninstall says so.
   Linger (Linux): it runs `loginctl disable-linger` only when `setup.json` says setup turned linger on; a linger you enabled yourself is not touched, and the step says which case applied. If it fails, the setup record is kept so a rerun can retry.
2. **Subagent cards.** Same as `agents clean`: only files carrying the `managed-by: claude-codex` marker are removed, including cards written by earlier versions as `ccp-<model>.md`.
3. **Claude Code settings.** Uses `setup.json` (state directory) to remove only the keys setup added and restore a value it replaced with `--force`, and only where the value is still the one setup wrote. Other keys and their order are kept. When the result is the same document as the pre-setup backup, the backup's exact bytes are written back. If setup created `settings.json` and nothing else lives in it, the file is removed. `settings.json.claude-codex.bak` is left in place and reported. Restart Claude Code afterwards.
   Without a setup record (setup never ran, or it was already undone), `settings.json` is not touched and uninstall tells you to remove `ANTHROPIC_BASE_URL` yourself if you want to.
4. **`--purge`** also deletes the proxy's state directory (`~/.local/state/claude-code-proxy/` on Linux: model cache, logs, setup record). **`--purge-credentials`** implies `--purge` and also deletes the proxy's own provider logins and config in its config directory (`~/.config/claude-code-proxy/`): `<provider>/auth.json`, provider device-id files, `config.json` and `service.env`, then the directories if they are empty. Anything else you keep there stays. `~/.codex` belongs to the Codex CLI and is never touched, and neither is the file named by `CCP_CODEX_AUTH_FILE`. macOS Keychain entries are not removed.
5. **Binary.** Deleted only when it is named `claude-codex`, sits in `~/.local/bin`, `~/.cargo/bin`, `/usr/local/bin` or `$CLAUDE_CODEX_INSTALL_DIR`, and is not inside a `target/` directory. Otherwise it is kept and the reason is shown. `--keep-binary` always keeps it.

`uninstall` also removes the `disabled` marker. `CCP_SERVICE_DIR`, `CCP_AGENTS_DIR`, `CLAUDE_CONFIG_DIR`, `CCP_CONFIG_DIR` and `XDG_STATE_HOME` are honored as in `setup`, and `CCP_SETUP_SANDBOX=1` applies the same guard (in sandbox mode `/usr/local/bin` is not a candidate for deletion).

## Autostart service

```sh
claude-codex service install     # write the unit, enable and start it
claude-codex service disable     # stop and disable it, keep the unit file
claude-codex service status      # installed, active, linger, port
claude-codex service restart
claude-codex service uninstall
```

Linux installs a systemd user unit (`~/.config/systemd/user/claude-codex.service`); macOS installs a LaunchAgent (`~/Library/LaunchAgents/com.claude-codex.proxy.plist`). Both run `serve --no-monitor`. Other systems are not supported. On Linux a user service starts at login unless linger is on for the user. `setup` and `enable` turn it on with plain `loginctl enable-linger` (no sudo: polkit allows it for your own account in a local session) and record that in `setup.json` as `linger_enabled_by_setup`; if linger is already on they do nothing and record nothing. If it cannot be enabled (a remote session without polkit access, no `loginctl`), setup still succeeds and lists the manual command under "Still to do": `loginctl enable-linger`, or `sudo loginctl enable-linger $USER` as a last resort. macOS has no linger: the LaunchAgent starts at login. `service install` alone only prints a hint.

To pass settings such as `PORT` or other `CCP_*` variables, write `KEY=VALUE` lines to `service.env` in the config directory. On macOS the file is read when you run `service install`, so rerun it after editing.

## Model discovery

While `serve` runs, the proxy calls `GET https://chatgpt.com/backend-api/codex/models?client_version=<x>` with the Codex CLI's login at startup and then every `CCP_MODEL_REFRESH_INTERVAL_SECS` seconds. The result goes to `codex-models.json` in the proxy's state directory (`~/.local/state/claude-code-proxy/` on Linux); nothing is written to `~/.codex`.

- The usable Codex list is the bundled models, plus the Codex CLI cache, plus `codex-models.json`. Only models listed in the picker and callable through the API are kept.
- Any failure (no login, 401/403, 429, other HTTP status, network error, unexpected JSON) keeps the last good list. Failed refreshes are retried after 30 seconds, doubling up to 15 minutes, never later than the normal interval.
- A refresh that changes the list syncs the agent cards right away, and the log shows `new model discovered: X, card created` or `model no longer available: X, card removed`.
- `claude-codex models --refresh` forces a refresh and prints the changes (exit code 1 if it failed). `claude-codex agents sync` refreshes first, except with `--dry-run`.
- Not covered: the `opus`, `sonnet`, `haiku` and `fable` aliases keep fixed preference lists, so a new model such as `gpt-7-sol` gets a card and is routable by name but is not chosen as an alias target.
- Claude Code reads its agent list at startup: restart it to use a card created while it was running.

| Variable | Effect |
|---|---|
| `CCP_MODEL_DISCOVERY` | `0` (also `false`, `off`, `no`) disables the fetch; the Codex CLI cache is still read. Default on |
| `CCP_MODEL_REFRESH_INTERVAL_SECS` | Seconds between refreshes (default 900). `0` refreshes only at startup, retrying a failed attempt a few times |
| `CCP_CODEX_CLIENT_VERSION` | Overrides the `client_version` sent (default: the newest of a built-in version and the versions found in the Codex CLI's `models_cache.json` and `version.json`) |

## Latest aliases

`<family>-latest` and `<family>-<tier>-latest` are model ids the proxy resolves on every request (Messages and `count_tokens`, before routing). They are listed by `/v1/models` (display name `<alias> (alias of <model>, <provider>)`), printed by `claude-codex models`, and get a card each (`gpt-sol-latest.md` with `name` and `model` equal to the alias). The card text says `Always the newest <tier> model of the <family> family, currently <model>` and is rewritten when the target moves; the `model:` line never changes, so Claude Code needs no restart. The log has a `latest alias resolved` line with `alias` and `model`.

Algorithm:

1. An id is parsed as `<family>-<version>[-<tier>]`. The only family is `gpt` (the Codex models). The version is dot-separated integers compared as tuples with trailing zeros ignored: `6.1 > 6 > 5.6`, `10 > 9`. The tier is one or more lowercase words (`sol`, `luna`, `terra`, `astra`, `codex-spark`).
2. Ids that do not fit take no part: other families, ids with digits in the tier such as `gpt-4-0613`, anything ending in or containing the word `latest`. Ids containing the word `fast` (priority copies) never take part.
3. A tier containing `mini`, `nano`, `codex`, `spark`, `image`, `realtime`, `audio`, `transcribe`, `tts`, `search`, `preview`, `lite`, `oss` or `embedding` marks a variant. Variants are left out of `<family>-latest` and answer only to their own alias, for example `gpt-mini-latest`.
4. `<family>-<tier>-latest` is the highest version with exactly that tier. `<family>-latest` is the highest version among non-variant ids. On equal versions the alphabetically first id wins.
5. An alias with no candidate returns HTTP 400 `No model matches the alias "X". Available latest aliases: ...`. A real model with the same id as an alias wins and the alias is dropped (none collides today). When a family loses all candidates of a tier, its alias disappears and the next sync removes its card.

With the account list from 2026-10-18 (`claude-codex models`):

| Alias | Resolves to |
|---|---|
| `gpt-latest`, `gpt-sol-latest` | `gpt-6.1-sol` |
| `gpt-luna-latest` | `gpt-6-luna` |
| `gpt-terra-latest` | `gpt-5.6-terra` |
| `gpt-astra-latest` | `gpt-6-astra` |
| `gpt-mini-latest` | `gpt-5.4-mini` |
| `gpt-codex-latest`, `gpt-codex-spark-latest` | `gpt-5.3-codex`, `gpt-5.3-codex-spark` |

`CCP_LATEST_ALIASES=0` (also `false`, `off`, `no`) turns the aliases off: they are not listed, requests for them fail as unknown models, and the next card sync removes their cards. The `opus`, `sonnet`, `haiku` and `fable` aliases are not affected. The OpenAI-compatible routes (`/v1/responses`, `/v1/chat/completions`) do not resolve these aliases.

## Subagent cards

See the README for the overview. Details:

- A card is `<model>.md` in `~/.claude/agents/` (the model id lowercased, dots and symbols turned into dashes, for example `gpt-6-luna.md`) with `name`, `description` and `model` frontmatter and a `<!-- managed-by: claude-codex; do not edit -->` marker line right after the frontmatter. That marker, not the file name, is what makes a file ours: every `.md` in the directory is read to find the managed ones, and a file without the marker is never rewritten or deleted. The older Spanish marker (`...; no editar -->`) is still recognized.
- Migration: earlier versions named cards `ccp-<model>.md`. Those files carry the marker, so the first sync removes them and creates the new names.
- Only Codex models get cards by default. `CCP_AGENT_CARD_PROVIDERS` (comma-separated, default `codex`) chooses the providers; at the next sync, managed cards of a provider that is not listed are removed, and unmarked files are never touched. Models get a card unless they are Claude ids or aliases, or a `-fast` variant whose base model is also listed. The `-latest` aliases get cards too (see [Latest aliases](#latest-aliases)).
- While [`disable`](#enable-disable-and-status) is in effect (`disabled` marker), the background sync does nothing; `agents sync` still works and says so.
- The background sync runs at `serve` startup, every `CCP_AGENT_SYNC_INTERVAL_SECS` seconds (default 300), and right after a model refresh that changed the list.
- If you own a file with the same name but no marker, that model is skipped (reported as a conflict, logged once as `agent card skipped`).
- These names are never generated, whatever the model: `general-purpose`, `explore`, `plan`, `statusline-setup`, `claude-code-guide`, anything starting with `claude-`, and the `opus`/`sonnet`/`haiku`/`fable` aliases. A model that sanitizes to one of them is skipped with the same log.

## Environment variables

| Variable | Effect |
|---|---|
| `PORT` | Listen port (default `18765`) |
| `CCP_BIND_ADDRESS` | Bind address (default `127.0.0.1`) |
| `CCP_ALIAS_PROVIDER` | Where `opus`/`sonnet`/`haiku` aliases go: `anthropic` (default) or `codex` |
| `CCP_CONFIG_DIR` | Config directory (default `~/.config/claude-code-proxy`) |
| `CCP_LOG_VERBOSE`, `CCP_LOG_STDERR` | Setting either (any value) turns on verbose logs or stderr logs |
| `CCP_ANTHROPIC_BASE_URL` | Upstream for the Claude route (default `https://api.anthropic.com`) |
| `CCP_CODEX_AUTH_FILE`, `CODEX_HOME` | Where to read the Codex CLI login |
| `CCP_AGENT_CARDS` | `0` disables the background card sync in `serve` |
| `CCP_AGENT_SYNC_INTERVAL_SECS` | Seconds between card syncs (default 300; `0` only at startup) |
| `CCP_AGENTS_DIR` | Cards directory (default `~/.claude/agents`) |
| `CCP_AGENT_CARD_PROVIDERS` | Providers that get cards, comma-separated (default `codex`) |
| `CCP_LATEST_ALIASES` | `0` disables the `-latest` model aliases |
| `CCP_MODEL_DISCOVERY`, `CCP_MODEL_REFRESH_INTERVAL_SECS` | See [Model discovery](#model-discovery) |
| `CCP_SERVICE_DIR` | Write the unit or plist here and skip `systemctl`/`launchctl` |
| `CLAUDE_CONFIG_DIR` | Claude Code's config directory, used by `enable`, `disable`, `setup` and `uninstall` |

Port, bind address and alias provider can also go in `config.json`; service settings in `<config dir>/service.env`.

Do not set `ANTHROPIC_AUTH_TOKEN` or `ANTHROPIC_API_KEY`: they replace your Claude login and the Claude route returns 401.

Beyond the table above, `src/config.rs` reads Codex tuning (`CCP_CODEX_*`), `CCP_TRAFFIC_LOG` (set to `1`, `true` or `yes` to capture redacted traffic under the state directory) and `CCP_USER_AGENT`. Most have a `config.json` equivalent.
