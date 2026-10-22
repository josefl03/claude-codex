# Releasing

Publishing a version is automatic: raise the version, push to `main`, and the
release workflow builds, tests, publishes and tags it. Nothing needs a token
beyond the default `GITHUB_TOKEN`.

## Publish a version

1. Set `version` in `Cargo.toml` and run `cargo build` so `Cargo.lock` follows
   (CI fails when the two disagree).
2. Add `## vX.Y.Z (YYYY-MM-DD)` on top of `CHANGELOG.md`. Its text becomes the top
   of the release notes.
3. Run `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings` and
   `cargo test --locked -- --test-threads=1`.
4. Commit and `git push origin main`.

`auto-release.yml` runs on every push to `main` that touches `Cargo.toml`. If the
tag `vX.Y.Z` (from the version in the file) has no release yet, it starts
`release.yml` for that exact commit with `gh workflow run`. A push that does not
change the version finds the tag and does nothing.

Why a dispatch and not a tag push: events created with `GITHUB_TOKEN` (a tag, a
release) never start other workflows, but `workflow_dispatch` started with it does.
So `release.yml` creates the tag itself, at the commit it built, with
`gh release create vX.Y.Z --target <sha>`.

## What `release.yml` does

Triggers: `workflow_dispatch` and `workflow_call`, both with `tag` (required) and
`sha` (optional, default: the branch head).

1. `prepare`: the tag must be `vX.Y.Z`, equal to the `Cargo.toml` version at that
   commit, with no existing release or tag. It also runs
   `scripts/check-release-assets.sh`.
2. `checks`: `cargo test --locked -- --test-threads=1`.
3. `build` (four runners in parallel): build, check `--version` and `version`, check
   Linux binaries are static (`file` and `ldd`), package, test the archive, attest.
4. `release`: gather the artifacts, write `SHA256SUMS`, run
   `scripts/check-release-assets.sh dist`, generate the notes with
   `scripts/release-notes.sh` and create the release.

A `concurrency` group per tag and the "release already exists" check keep a
release from being created twice.

## Assets

| Platform | Runner | Target | Asset |
|---|---|---|---|
| Linux x86_64 | `ubuntu-latest` | `x86_64-unknown-linux-musl` | `claude-codex-linux-amd64.tar.gz` |
| Linux arm64 | `ubuntu-24.04-arm` | `aarch64-unknown-linux-musl` | `claude-codex-linux-arm64.tar.gz` |
| macOS Apple silicon | `macos-14` | `aarch64-apple-darwin` | `claude-codex-darwin-arm64.tar.gz` |
| macOS Intel | `macos-15-intel` | `x86_64-apple-darwin` | `claude-codex-darwin-amd64.tar.gz` |

Each archive holds the single file `claude-codex`, and has a `claude-codex-<platform>.sha256`
next to it. `SHA256SUMS` joins the four checksums. Linux is built against musl so
the binary is static and does not depend on the glibc of the build host (an earlier
build needed glibc 2.39 and did not start on RHEL 9 or Debian stable). There is no
Windows binary: `install.sh` supports Linux and macOS only and, on Git Bash, MSYS or
Cygwin, stops with a pointer to WSL 2.

**Never rename an asset without changing `scripts/install.sh` in the same commit.**
The installer downloads `claude-codex-<os>-<arch>.tar.gz` and `.sha256`.
`scripts/check-release-assets.sh` fails when `install.sh` stops building those
names, and (with a directory argument) when an expected file is missing. CI and the
release workflow both run it.

## Install smoke

`install-smoke.yml` proves that the published release installs and works on real
GitHub-hosted runners (free for public repositories). It does not build anything.
Runs: `workflow_dispatch`, a weekly cron (Monday 05:23 UTC) and, as the `smoke` job at the
end of `release.yml`, after every release it publishes. A platform that breaks turns
that job, and so the release run, red. Run it by hand with
`gh workflow run install-smoke.yml -R josefl03/claude-codex --ref main`.

| Job | Runner | What it checks |
|---|---|---|
| Linux amd64 (reference) | `ubuntu-latest` | everything below |
| Linux arm64 | `ubuntu-24.04-arm` | same |
| macOS arm64 | `macos-15` | same, plus a pass with only Apple tools on `PATH` (no `sha256sum`, so the `shasum` branch; BSD `sed`, `paste`; `/bin/bash` 3.2) |
| macOS Intel | `macos-15-intel` | same as macOS arm64 |
| Windows | `windows-latest` (Git Bash) | the installer refuses with "not supported" and a `wsl --install` hint, downloads nothing and leaves no files; a source check that service commands bail off Linux/macOS (not executed: there is no Windows build) |

On Unix runners `scripts/install-smoke.sh unix` runs the real one-liner from `main`
twice, in separate isolated homes with a toy Codex login (`CCP_CODEX_AUTH_FILE`): once with
`CLAUDE_CODEX_NO_SETUP=1`, once with a full setup. It checks the prebuilt asset was used
(no cargo fallback), the version is the latest release, `file` matches `uname -m` (and
static on Linux), `version`, `status`, `models`, `serve --no-monitor` with `GET /v1/models`,
`agents sync`/`list`/`clean`, then `enable`, `disable`, `enable`, `uninstall --yes` with
`CCP_SERVICE_DIR` (unit or plist generated, no service manager involved).
`scripts/install-smoke.sh service` is a separate step with `continue-on-error: true`: the
real `service install` against systemd `--user` or launchd, a request to port 18765 and
`service uninstall`. On a headless runner it can fail for runner reasons (no user bus, no GUI
session); the step then shows as failed-but-ignored and the table marks it with a warning and
the reason, never as a pass. Each job ends with a table in the job summary; logs are kept as an
artifact for 7 days.

The installer resolves the latest tag from the `github.com/<repo>/releases/latest` redirect and
only falls back to `api.github.com`: runners share IP addresses and the anonymous API allows
60 requests per hour per address. The first smoke run fell back to `cargo` on macOS arm64 right after the API lookup failed (probable cause: the exhausted budget; a later run on the same pool showed 32 of 60 used).

If the smoke is red after a release, read the failing job's table first. A step marked with a
cross is a bug in the release or the installer; fix it, and for a change that ends up in the
binary publish a new version (the installer itself is served from `main`, so a fix to
`scripts/install.sh` needs no release).

## Verify a release

```sh
dir=$(mktemp -d) && cd "$dir"
gh release download vX.Y.Z --repo josefl03/claude-codex --pattern 'claude-codex-linux-amd64.*'
sha256sum -c claude-codex-linux-amd64.sha256        # shasum -a 256 -c on macOS
tar -xzf claude-codex-linux-amd64.tar.gz
./claude-codex version
file claude-codex                                    # static-pie linked
gh attestation verify claude-codex-linux-amd64.tar.gz --repo josefl03/claude-codex
```

`gh attestation` needs GitHub CLI 2.49 or newer. The attestation is the build
provenance signed by `actions/attest-build-provenance`; it ties the archive to
this repository, the workflow and the commit.

## Run it by hand

Actions tab, "Release", "Run workflow", or:

```sh
gh workflow run release.yml --ref main -f tag=vX.Y.Z -f sha=<full commit sha>
```

The tag must match `Cargo.toml` at that commit. To redo a broken release, delete
the release and its tag first (`gh release delete vX.Y.Z --cleanup-tag`), then run
it again.

## crates.io

Not part of the workflow. A crates.io version can never be reused, so run
`cargo publish --locked` by hand, only after the release succeeded.
