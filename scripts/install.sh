#!/usr/bin/env bash
#
# claude-codex installer, the only one. Safe to pipe into bash:
#
#   curl -fsSL https://raw.githubusercontent.com/josefl03/claude-codex/main/scripts/install.sh | bash
#
# What it does, without prompts and without sudo:
#   1. installs the binary into ~/.local/bin (prebuilt release with checksum; if no
#      release or no asset exists for your platform and `cargo` is available, builds
#      with `cargo install --git`);
#   2. runs `claude-codex enable` (autostart service, linger, subagent cards,
#      ANTHROPIC_BASE_URL in Claude Code's settings.json);
#   3. checks the Codex login and Claude Code, and prints a status summary with what
#      is still left for you. It never runs `codex login` for you.
#
# Environment variables:
#   CLAUDE_CODEX_REPO         GitHub repository, owner/name (default: josefl03/claude-codex)
#   CLAUDE_CODEX_VERSION      Pin a release tag, for example v0.6.0 (default: latest)
#   CLAUDE_CODEX_INSTALL_DIR  Install directory (default: ~/.local/bin). Must be writable:
#                             the installer never uses sudo
#   CLAUDE_CODEX_NO_SETUP     Set to 1 to install the binary only, without `enable`
#   CLAUDE_CODEX_LOCAL_BINARY Testing: install this local binary instead of downloading
#
# Undo: `claude-codex disable` switches it off, `claude-codex uninstall` removes it.

set -euo pipefail

BIN_NAME="claude-codex"
REPO="${CLAUDE_CODEX_REPO:-josefl03/claude-codex}"

TMP_DIR=""
BIN_SRC=""
INSTALLED_BIN=""
PLATFORM=""
RELEASE_VERSION=""
ENABLE_STATE="not run"
PENDING=()

if [ -t 1 ]; then
	C_RED=$'\033[0;31m'; C_GREEN=$'\033[0;32m'; C_YELLOW=$'\033[1;33m'; C_BLUE=$'\033[0;34m'; C_OFF=$'\033[0m'
else
	C_RED=""; C_GREEN=""; C_YELLOW=""; C_BLUE=""; C_OFF=""
fi

log_info()    { printf '%s==>%s %s\n' "$C_BLUE" "$C_OFF" "$1"; }
log_success() { printf '%s==>%s %s\n' "$C_GREEN" "$C_OFF" "$1"; }
log_warning() { printf '%s==>%s %s\n' "$C_YELLOW" "$C_OFF" "$1"; }
log_error()   { printf '%sError:%s %s\n' "$C_RED" "$C_OFF" "$1" >&2; }

have() { command -v "$1" >/dev/null 2>&1; }

cleanup() {
	if [ -n "${TMP_DIR:-}" ]; then
		rm -rf "$TMP_DIR"
	fi
}

# download URL DEST: quiet, with retries. Non-zero when the file is not available.
download() {
	local url=$1 dest=$2
	if have curl; then
		curl -fsL --retry 3 --retry-connrefused --connect-timeout 10 --max-time 180 -o "$dest" "$url" </dev/null
	elif have wget; then
		wget --tries=3 --timeout=60 -q -O "$dest" "$url" </dev/null
	else
		log_error "Neither curl nor wget found. Install one of them and rerun."
		exit 1
	fi
}

detect_platform() {
	local os arch
	case "$(uname -s)" in
	Darwin) os="darwin" ;;
	Linux)  os="linux" ;;
	MINGW* | MSYS* | CYGWIN* | Windows_NT)
		log_error "Windows is not supported natively: ${BIN_NAME} has no Windows build and no Windows autostart service (this shell reports $(uname -s))."
		echo "Use WSL 2 instead: run \`wsl --install\` in PowerShell (https://learn.microsoft.com/windows/wsl/install)," >&2
		echo "then run this installer again inside the Linux shell. Nothing was installed." >&2
		exit 1
		;;
	*)
		log_error "Unsupported operating system: $(uname -s). ${BIN_NAME} supports Linux and macOS (Windows through WSL 2)."
		exit 1
		;;
	esac
	case "$(uname -m)" in
	x86_64 | amd64)  arch="amd64" ;;
	aarch64 | arm64) arch="arm64" ;;
	*)
		log_error "Unsupported architecture: $(uname -m). Prebuilt binaries exist for amd64 and arm64."
		echo "Build from source instead: cargo install --git https://github.com/${REPO} --locked" >&2
		exit 1
		;;
	esac
	PLATFORM="${os}-${arch}"
}

# Sets RELEASE_VERSION. Non-zero when there is no release to install.
# The latest tag comes from the redirect of github.com/<repo>/releases/latest, not from
# api.github.com: the API allows 60 anonymous requests per hour per IP address, and the
# shared IP of a CI runner or an office exhausts that. The API is only the fallback.
resolve_version() {
	local tmp=$1 version="${CLAUDE_CODEX_VERSION:-}" url
	if [ -z "$version" ]; then
		log_info "Looking for the latest release..."
		if have curl; then
			url=$(curl -fsL --retry 3 --retry-connrefused --connect-timeout 10 --max-time 60 \
				-o /dev/null -w '%{url_effective}' "https://github.com/${REPO}/releases/latest" </dev/null 2>/dev/null) || url=""
			case "$url" in
			*/releases/tag/*) version="${url##*/releases/tag/}" ;;
			esac
		fi
		if [ -z "$version" ]; then
			if ! download "https://api.github.com/repos/${REPO}/releases/latest" "$tmp/latest.json"; then
				return 1
			fi
			version=$(sed -n 's/.*"tag_name": *"\([^"]*\)".*/\1/p' "$tmp/latest.json" | head -n 1)
		fi
		[ -n "$version" ] || return 1
	fi
	case "$version" in
	v*) ;;
	*) version="v${version}" ;;
	esac
	RELEASE_VERSION="$version"
}

verify_checksum() {
	local dir=$1 file=$2
	if have sha256sum; then
		(cd "$dir" && sha256sum -c "$file" >/dev/null 2>&1)
	elif have shasum; then
		(cd "$dir" && shasum -a 256 -c "$file" >/dev/null 2>&1)
	else
		log_warning "Neither sha256sum nor shasum found: skipping checksum verification"
	fi
}

# Download, verify and unpack a release into $1. Sets BIN_SRC.
# Returns 1 when the release or the asset for this platform does not exist.
fetch_release() {
	local tmp=$1
	resolve_version "$tmp" || return 1
	local archive="${BIN_NAME}-${PLATFORM}.tar.gz"
	local sum="${BIN_NAME}-${PLATFORM}.sha256"
	local base="https://github.com/${REPO}/releases/download/${RELEASE_VERSION}"

	log_info "Downloading ${archive} (${RELEASE_VERSION})..."
	download "${base}/${archive}" "$tmp/${archive}" || return 1
	download "${base}/${sum}" "$tmp/${sum}" || return 1
	if ! verify_checksum "$tmp" "$sum"; then
		log_error "Checksum verification failed for ${archive}. Nothing was installed."
		exit 1
	fi
	log_success "Checksum verified"
	mkdir -p "$tmp/unpacked"
	if ! tar -xzf "$tmp/${archive}" -C "$tmp/unpacked" || [ ! -f "$tmp/unpacked/${BIN_NAME}" ]; then
		log_error "The archive does not contain ${BIN_NAME}"
		exit 1
	fi
	BIN_SRC="$tmp/unpacked/${BIN_NAME}"
}

build_from_source() {
	local tmp=$1
	log_info "Building with cargo (this takes a few minutes)..."
	local args=(install --git "https://github.com/${REPO}" --locked --root "$tmp/cargo-root")
	if [ -n "${CLAUDE_CODEX_VERSION:-}" ]; then
		args+=(--tag "$RELEASE_VERSION")
	fi
	if ! cargo "${args[@]}" </dev/null; then
		log_error "cargo install failed"
		exit 1
	fi
	BIN_SRC="$tmp/cargo-root/bin/${BIN_NAME}"
	RELEASE_VERSION="source"
}

obtain_binary() {
	local tmp=$1
	if [ -n "${CLAUDE_CODEX_LOCAL_BINARY:-}" ]; then
		if [ ! -f "$CLAUDE_CODEX_LOCAL_BINARY" ]; then
			log_error "CLAUDE_CODEX_LOCAL_BINARY does not exist: $CLAUDE_CODEX_LOCAL_BINARY"
			exit 1
		fi
		log_info "Using local binary: $CLAUDE_CODEX_LOCAL_BINARY"
		BIN_SRC="$CLAUDE_CODEX_LOCAL_BINARY"
		RELEASE_VERSION="local"
		return 0
	fi
	if fetch_release "$tmp"; then
		return 0
	fi
	log_warning "No prebuilt ${PLATFORM} binary available from ${REPO} (no release yet, or none for this platform)."
	if have cargo; then
		build_from_source "$tmp"
		return 0
	fi
	log_error "Could not get a binary."
	cat >&2 <<EOF

Two ways forward:
  1. Check https://github.com/${REPO}/releases for a release with a ${PLATFORM} asset,
     then rerun this script (pin one with CLAUDE_CODEX_VERSION=vX.Y.Z).
  2. Install Rust from https://rustup.rs and rerun this script. It will build with:
       cargo install --git https://github.com/${REPO} --locked
EOF
	exit 1
}

path_display() {
	case "$1" in
	"$HOME"/*) printf '%s/%s' "\$HOME" "${1#"$HOME"/}" ;;
	*) printf '%s' "$1" ;;
	esac
}

install_binary() {
	local dir="${CLAUDE_CODEX_INSTALL_DIR:-$HOME/.local/bin}"
	if ! mkdir -p "$dir" 2>/dev/null || [ ! -w "$dir" ]; then
		log_error "$dir is not writable. This installer never uses sudo."
		echo "Choose a directory you own: CLAUDE_CODEX_INSTALL_DIR=\$HOME/bin bash install.sh" >&2
		exit 1
	fi
	if [ -x "$dir/${BIN_NAME}" ]; then
		local existing
		existing=$("$dir/${BIN_NAME}" --version 2>/dev/null </dev/null || echo "unknown version")
		log_info "Replacing existing installation: $existing"
	fi
	local tmp_binary="$dir/${BIN_NAME}.tmp.$$"
	cp "$BIN_SRC" "$tmp_binary"
	chmod +x "$tmp_binary"
	mv -f "$tmp_binary" "$dir/${BIN_NAME}"

	if [ "$(uname -s)" = "Darwin" ]; then
		if have xattr; then
			xattr -d com.apple.quarantine "$dir/${BIN_NAME}" 2>/dev/null || true
		fi
		if have codesign; then
			codesign --remove-signature "$dir/${BIN_NAME}" 2>/dev/null || true
			codesign --sign - --force "$dir/${BIN_NAME}" 2>/dev/null || true
		fi
	fi

	INSTALLED_BIN="$dir/${BIN_NAME}"
	if ! "$INSTALLED_BIN" --version >/dev/null 2>&1 </dev/null; then
		log_error "${INSTALLED_BIN} was installed but does not run"
		exit 1
	fi
	log_success "Installed $("$INSTALLED_BIN" --version </dev/null) to $INSTALLED_BIN"

	case ":$PATH:" in
	*":$dir:"*) ;;
	*)
		log_warning "$dir is not in your PATH"
		PENDING+=("Add the install directory to your PATH (put this line in ~/.bashrc or ~/.zshrc): export PATH=\"$(path_display "$dir"):\$PATH\"")
		;;
	esac
}

run_enable() {
	if [ "${CLAUDE_CODEX_NO_SETUP:-}" = "1" ]; then
		log_info "Skipping setup (CLAUDE_CODEX_NO_SETUP=1)"
		ENABLE_STATE="skipped (CLAUDE_CODEX_NO_SETUP=1)"
		PENDING+=("Run: $INSTALLED_BIN enable")
		return 0
	fi
	log_info "Running ${BIN_NAME} enable..."
	if "$INSTALLED_BIN" enable </dev/null; then
		ENABLE_STATE="done"
	else
		ENABLE_STATE="incomplete"
		log_warning "${BIN_NAME} is installed, but enable did not complete"
		PENDING+=("Fix the problem reported above and retry: $INSTALLED_BIN enable")
	fi
}

# The text after "<label>  " in the `status` output.
status_field() {
	printf '%s\n' "$STATUS" | sed -n "s/^$1  *//p" | head -n 1
}

check_codex_login() {
	CODEX_LOGIN="missing"
	CODEX_DETAIL=""
	local out
	if out=$("$INSTALLED_BIN" codex auth status 2>&1 </dev/null); then
		CODEX_LOGIN="found"
		CODEX_DETAIL=$(printf '%s\n' "$out" | sed -n -e 's/^Account: /account /p' -e 's/^Expires: \(.*\)$/access token expires \1/p' | paste -sd ';' - | sed 's/;/; /g')
		return 0
	fi
	if have codex; then
		PENDING+=("Sign in to ChatGPT with the Codex CLI: codex login")
	else
		PENDING+=("Install the Codex CLI (npm install -g @openai/codex, or brew install --cask codex), then sign in: codex login")
	fi
}

check_claude_code() {
	if have claude; then
		CLAUDE_FOUND="found ($(command -v claude))"
	else
		CLAUDE_FOUND="not found in PATH"
		PENDING+=("Install Claude Code (https://code.claude.com/docs/en/setup) and sign in with a Claude plan")
	fi
}

print_summary() {
	STATUS=""
	if [ "$ENABLE_STATE" != "skipped (CLAUDE_CODEX_NO_SETUP=1)" ]; then
		STATUS=$("$INSTALLED_BIN" status 2>&1 </dev/null || true)
	fi
	check_codex_login
	check_claude_code

	echo ""
	echo "Summary"
	printf '  %-13s %s\n' "binary" "$INSTALLED_BIN ($("$INSTALLED_BIN" --version </dev/null))"
	printf '  %-13s %s\n' "enable" "$ENABLE_STATE"
	if [ -n "$STATUS" ]; then
		printf '  %-13s %s\n' "service" "$(status_field service)"
		local linger
		linger=$(status_field linger)
		if [ -n "$linger" ]; then
			printf '  %-13s %s\n' "linger" "$linger"
		fi
		printf '  %-13s %s\n' "agent cards" "$(status_field 'agent cards')"
		printf '  %-13s %s\n' "Claude Code" "$(status_field 'Claude Code')"
	fi
	printf '  %-13s %s\n' "claude CLI" "$CLAUDE_FOUND"
	if [ "$CODEX_LOGIN" = "found" ]; then
		printf '  %-13s %s\n' "Codex login" "found${CODEX_DETAIL:+ ($CODEX_DETAIL)}"
	else
		printf '  %-13s %s\n' "Codex login" "not found"
	fi

	if [ "$ENABLE_STATE" = "done" ]; then
		PENDING+=("Restart Claude Code so it picks up the new settings")
	fi
	echo ""
	if [ "${#PENDING[@]}" -gt 0 ]; then
		echo "Still to do:"
		local item
		for item in "${PENDING[@]}"; do
			echo "  - $item"
		done
	else
		log_success "Everything is set up."
	fi
	echo ""
	echo "Switch off: ${BIN_NAME} disable    Remove: ${BIN_NAME} uninstall    Docs: https://github.com/${REPO}"
}

main() {
	: "${HOME:?HOME is not set}"

	echo ""
	echo "${BIN_NAME} installer"
	echo ""

	# Before the temp directory exists: an unsupported system leaves nothing behind.
	detect_platform
	TMP_DIR=$(mktemp -d)
	trap cleanup EXIT
	log_info "Platform: $PLATFORM"
	obtain_binary "$TMP_DIR"
	install_binary
	run_enable
	print_summary
}

main "$@"
