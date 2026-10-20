#!/usr/bin/env bash
# Print release notes for a tag: the matching CHANGELOG.md section, then every
# non-merge commit since the previous GitHub release (at most 20 when there is
# none), then the compare link. Needs a full git history (fetch-depth: 0).
# Authors are printed as plain names, never as @mentions, so a sync of upstream
# commits does not ping anyone.
#
# Usage: release-notes.sh <tag> [<sha>]
# With <sha> the tag does not need to exist yet (the release workflow creates it
# at that exact commit afterwards). The previous release comes from
# `gh release list`, so it needs GH_TOKEN and GITHUB_REPOSITORY (default
# josefl03/claude-codex).
set -euo pipefail

tag="$1"
sha="$(git rev-parse "${2:-$tag}^{commit}")"
repo="${GITHUB_REPOSITORY:-josefl03/claude-codex}"
root="$(cd "$(dirname "$0")/.." && pwd)"

# Newest published release other than this tag whose tag is an ancestor of sha.
prev=""
while read -r candidate; do
  [ "$candidate" = "$tag" ] && continue
  if git rev-parse -q --verify "refs/tags/$candidate" >/dev/null \
    && git merge-base --is-ancestor "$candidate" "$sha" 2>/dev/null; then
    prev="$candidate"
    break
  fi
done < <(gh release list --repo "$repo" --exclude-drafts --limit 50 --json tagName --jq '.[].tagName')

# CHANGELOG.md section "## vX.Y.Z (date)" up to the next "## ".
section="$(awk -v hdr="## $tag " 'index($0, hdr) == 1 { on = 1; next } on && /^## / { exit } on' "$root/CHANGELOG.md")"
if [ -n "${section//[[:space:]]/}" ]; then
  echo "$section" | sed -e '/./,$!d'
  echo
fi

echo "## What's Changed"
if [ -n "$prev" ]; then
  range="$prev..$sha"
  limit=()
else
  range="$sha"
  limit=(-n 20)
fi
git log --no-merges "${limit[@]}" --format='* %s by %an in %h' "$range"
if [ -n "$prev" ]; then
  echo
  echo "**Full Changelog**: https://github.com/$repo/compare/$prev...$tag"
fi
