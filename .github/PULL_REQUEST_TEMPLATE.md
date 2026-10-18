## What and why

<!-- What does this change, and why? Link the issue if there is one. -->

## Checklist

- [ ] `cargo fmt --all`, `cargo clippy --all-targets -- -D warnings` and `cargo test -- --test-threads=1` pass (or `just check`)
- [ ] Tests added or updated, and they do not touch the real home directory, real credentials or live backends
- [ ] `CHANGELOG.md` has a line under `Unreleased` for user-visible changes
- [ ] `ARCHITECTURE.md` / docs updated if modules, flows or commands changed
- [ ] No secrets or personal paths in the diff
