# Contributing

Thanks for helping. This project is a fork of [fcakyon/claude-code-with-codex](https://github.com/fcakyon/claude-code-with-codex), which is a fork of [raine/claude-code-proxy](https://github.com/raine/claude-code-proxy). Changes to the Codex translator are often better sent upstream first so they do not diverge.

## Before you start

- Read [ARCHITECTURE.md](ARCHITECTURE.md) and [AGENTS.md](AGENTS.md) (invariants that must not break).
- For anything bigger than a small fix, open an issue first so we can agree on the approach.
- Follow the [Code of Conduct](CODE_OF_CONDUCT.md). Vulnerabilities go through [SECURITY.md](SECURITY.md), not public issues.

## Workflow

1. Fork, then create a branch from `main`.
2. Make the change, matching the style of the module you edit. Avoid style-only churn.
3. Run the checks (details in [docs/development.md](docs/development.md)):
   ```sh
   cargo fmt --all
   cargo clippy --all-targets -- -D warnings
   cargo test -- --test-threads=1
   ```
   or `just check`.
4. Add or update tests. They must be hermetic: no real credentials, no live backends, and **never the real home directory**. Anything that touches the service, agent cards or Claude Code settings must redirect every path to a temporary directory (see the safety rule in the development guide).
5. Update docs when behavior changes: add a line under `Unreleased` in [CHANGELOG.md](CHANGELOG.md), and update `ARCHITECTURE.md` if modules or flows changed.
6. Open a pull request using the template. Keep it focused; one concern per PR.

## Commit messages

Short imperative subject, with a prefix such as `feat:`, `fix:`, `docs:`, `build:` or `test:`. Commit subjects become release notes.

## Licensing

By contributing you agree that your contribution is licensed under the [MIT License](LICENSE).
