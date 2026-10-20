#!/usr/bin/env bash
# Check that the asset names scripts/install.sh downloads are the ones the release
# workflow publishes. The installer and release.yml MUST agree, so both read the
# same four platforms from here.
#
# Usage: check-release-assets.sh [<dir>]
#   no <dir>: only check that install.sh still uses the expected name templates
#   <dir>:    also check that every asset exists in <dir>
set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
installer="$root/scripts/install.sh"
dir="${1:-}"

# The installer builds names as ${BIN_NAME}-${PLATFORM}.tar.gz / .sha256 with
# PLATFORM=<os>-<arch>; os is darwin|linux and arch is amd64|arm64.
grep -qF 'archive="${BIN_NAME}-${PLATFORM}.tar.gz"' "$installer" \
  || { echo "install.sh no longer builds the archive name from \${BIN_NAME}-\${PLATFORM}.tar.gz" >&2; exit 1; }
grep -qF 'sum="${BIN_NAME}-${PLATFORM}.sha256"' "$installer" \
  || { echo "install.sh no longer builds the checksum name from \${BIN_NAME}-\${PLATFORM}.sha256" >&2; exit 1; }
for token in 'os="darwin"' 'os="linux"' 'arch="amd64"' 'arch="arm64"'; do
  grep -qF "$token" "$installer" || { echo "install.sh lost $token" >&2; exit 1; }
done

if [ -n "$dir" ]; then
  for os in linux darwin; do
    for arch in amd64 arm64; do
      for f in "claude-codex-$os-$arch.tar.gz" "claude-codex-$os-$arch.sha256"; do
        [ -f "$dir/$f" ] || { echo "missing release asset: $f" >&2; exit 1; }
      done
    done
  done
  [ -f "$dir/SHA256SUMS" ] || { echo "missing release asset: SHA256SUMS" >&2; exit 1; }
fi
echo "release asset names OK"
