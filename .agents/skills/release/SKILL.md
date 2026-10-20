---
name: release
description: Cut a claude-codex release to GitHub and crates.io. Use when asked to "release", "cut a version", "bump and publish", "tag a release", or "fix release notes".
---

# Release

Pick the bump from the user's words (patch, minor, major). Never skip a step.

1. Set `version` in `Cargo.toml`, then `cargo build` so `Cargo.lock` follows.
2. Add `## vX.Y.Z (YYYY-MM-DD)` on top of `CHANGELOG.md`: one short bullet per
   user-visible change. Name the upstream version for an upstream merge.
3. Check, all must pass:
   - `cargo fmt --check`
   - `cargo +<latest stable> clippy --all-targets -- -D warnings`. CI runs the
     latest stable, which can be newer than the local default.
   - `cargo test --locked -- --test-threads=1`
4. Commit (`build: release vX.Y.Z`) and `git push origin main`. Wait for the
   `CI` workflow on that commit to succeed.
5. The push starts `Auto release`, which starts `Release` for that commit:
   four platform archives plus sha256 files and `SHA256SUMS`, tag and GitHub
   Release created by the workflow itself (do not push a tag by hand). Watch it
   with `gh run list --workflow release.yml`. Details: `docs/releasing.md`.
6. `cargo publish --locked`, last, only after step 5 succeeded. A crates.io
   version can never be reused.

## Release notes

The body is the CHANGELOG.md section of the version, then one line per non-merge commit
since the previous release. Authors are plain names, never @mentions, so syncs
do not ping anyone. To regenerate an existing release:

```bash
gh release edit vX.Y.Z --notes-file <(scripts/release-notes.sh vX.Y.Z)   # needs a full clone with tags
```
