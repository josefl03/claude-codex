#!/usr/bin/env bash
#
# claude-codex installation script
# Usage: curl -fsSL https://raw.githubusercontent.com/josefl03/claude-codex/main/scripts/install.sh | bash
#
# Binaries are downloaded from the GitHub releases of ${CLAUDE_CODEX_REPO}. Until the first
# release of that repository exists, install with:
#   cargo install --git https://github.com/josefl03/claude-codex --locked && claude-codex setup
#
# Environment variables:
#   CLAUDE_CODEX_REPO         - GitHub repository to download releases from, as owner/name
#                               (default: josefl03/claude-codex)
#   CLAUDE_CODEX_VERSION      - Pin a specific version (e.g., v0.1.0)
#   CLAUDE_CODEX_INSTALL_DIR  - Override install directory (default: /usr/local/bin or ~/.local/bin)
#   CLAUDE_CODEX_NO_SETUP     - Set to 1 to skip the automatic `claude-codex enable` after installing
#   CLAUDE_CODEX_LOCAL_BINARY - Testing override: install this local binary instead of
#                               downloading a release (no network, no checksum)
#
# After installing, the script runs `claude-codex enable` (autostart service, subagent
# cards, Claude Code settings). If that fails the install still succeeds; rerun it with
# `claude-codex enable`. Switch it off with `claude-codex disable`, or remove everything
# (service, cards, settings, binary) with `claude-codex uninstall`.
#
# Examples:
#   CLAUDE_CODEX_VERSION=v0.1.0 bash install.sh
#   CLAUDE_CODEX_INSTALL_DIR=/opt/bin bash install.sh
#   CLAUDE_CODEX_NO_SETUP=1 bash install.sh
#   CLAUDE_CODEX_LOCAL_BINARY=target/debug/claude-codex CLAUDE_CODEX_INSTALL_DIR=/tmp/bin bash install.sh
#

set -euo pipefail

TMP_DIR=""
INSTALL_DIR=""
RELEASE_VERSION=""

BIN_NAME="claude-codex"
REPO="${CLAUDE_CODEX_REPO:-josefl03/claude-codex}"

RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
BLUE='\033[0;34m'
NC='\033[0m'

log_info()    { echo -e "${BLUE}==>${NC} $1"; }
log_success() { echo -e "${GREEN}==>${NC} $1"; }
log_warning() { echo -e "${YELLOW}==>${NC} $1"; }
log_error()   { echo -e "${RED}Error:${NC} $1" >&2; }

detect_platform() {
	local os arch

	case "$(uname -s)" in
	Darwin) os="darwin" ;;
	Linux)  os="linux" ;;
	*)
		log_error "Unsupported operating system: $(uname -s)"
		echo ""
		echo "${BIN_NAME} supports macOS and Linux."
		echo "For other platforms, build from source with Rust:"
		echo "  cargo install --git https://github.com/${REPO} --locked"
		echo ""
		exit 1
		;;
	esac

	case "$(uname -m)" in
	x86_64 | amd64)   arch="amd64" ;;
	aarch64 | arm64)  arch="arm64" ;;
	*)
		log_error "Unsupported architecture: $(uname -m)"
		echo ""
		echo "${BIN_NAME} prebuilt binaries are available for amd64 and arm64."
		echo "For other architectures, build from source with Rust:"
		echo "  cargo install --git https://github.com/${REPO} --locked"
		echo ""
		exit 1
		;;
	esac

	echo "${os}-${arch}"
}

fetch_release() {
	local platform=$1
	local tmp_dir=$2
	local version="${CLAUDE_CODEX_VERSION:-}"

	if [ -z "$version" ]; then
		log_info "Fetching latest release..."
		local latest_url="https://api.github.com/repos/${REPO}/releases/latest"
		local release_json

		if command -v curl &>/dev/null; then
			release_json=$(curl -fsSL --retry 3 --retry-connrefused --connect-timeout 10 --max-time 30 "$latest_url")
		elif command -v wget &>/dev/null; then
			release_json=$(wget --tries=3 --timeout=30 -qO- "$latest_url")
		else
			log_error "Neither curl nor wget found. Please install one of them."
			exit 1
		fi

		version=$(echo "$release_json" | grep '"tag_name"' | sed -E 's/.*"tag_name": "([^"]+)".*/\1/' || true)

		if [ -z "$version" ]; then
			log_error "Failed to fetch latest version"
			echo ""
			echo "This might be due to network issues or GitHub API rate limits."
			echo "You can specify a version manually:"
			echo "  CLAUDE_CODEX_VERSION=v0.1.0 bash install.sh"
			echo ""
			exit 1
		fi
	fi

	log_info "Installing version: $version"

	local archive_name="${BIN_NAME}-${platform}.tar.gz"
	local download_url="https://github.com/${REPO}/releases/download/${version}/${archive_name}"

	log_info "Downloading $archive_name..."

	cd "$tmp_dir"
	if command -v curl &>/dev/null; then
		if ! curl -fsSL --retry 3 --retry-connrefused --connect-timeout 10 --max-time 120 -o "$archive_name" "$download_url"; then
			log_error "Download failed"
			echo ""
			echo "The release may not have a prebuilt binary for your platform."
			echo ""
			cd - >/dev/null || cd "$HOME"
			exit 1
		fi
	elif command -v wget &>/dev/null; then
		if ! wget --tries=3 --timeout=120 -q -O "$archive_name" "$download_url"; then
			log_error "Download failed"
			cd - >/dev/null || cd "$HOME"
			exit 1
		fi
	fi

	log_info "Verifying checksum..."
	local checksum_file="${BIN_NAME}-${platform}.sha256"
	local checksum_url="https://github.com/${REPO}/releases/download/${version}/${checksum_file}"

	if command -v curl &>/dev/null; then
		if ! curl -fsSL --retry 3 --retry-connrefused --connect-timeout 10 --max-time 30 -o "$checksum_file" "$checksum_url"; then
			log_error "Failed to download checksum file"
			cd - >/dev/null || cd "$HOME"
			exit 1
		fi
	elif command -v wget &>/dev/null; then
		if ! wget --tries=3 --timeout=30 -q -O "$checksum_file" "$checksum_url"; then
			log_error "Failed to download checksum file"
			cd - >/dev/null || cd "$HOME"
			exit 1
		fi
	fi

	if command -v sha256sum &>/dev/null; then
		if ! sha256sum -c "$checksum_file" &>/dev/null; then
			log_error "Checksum verification failed"
			cd - >/dev/null || cd "$HOME"
			exit 1
		fi
	elif command -v shasum &>/dev/null; then
		if ! shasum -a 256 -c "$checksum_file" &>/dev/null; then
			log_error "Checksum verification failed"
			cd - >/dev/null || cd "$HOME"
			exit 1
		fi
	else
		log_warning "Neither sha256sum nor shasum found, skipping checksum verification"
	fi

	log_success "Checksum verified"

	log_info "Extracting archive..."
	if ! tar -xzf "$archive_name"; then
		log_error "Failed to extract archive"
		exit 1
	fi

	RELEASE_VERSION="$version"
}

install_from_release() {
	local platform=$1
	TMP_DIR=$(mktemp -d)
	trap 'rm -rf "${TMP_DIR:-}"' EXIT
	local tmp_dir="$TMP_DIR"
	local version

	if [ -n "${CLAUDE_CODEX_LOCAL_BINARY:-}" ]; then
		if [ ! -f "$CLAUDE_CODEX_LOCAL_BINARY" ]; then
			log_error "CLAUDE_CODEX_LOCAL_BINARY does not exist: $CLAUDE_CODEX_LOCAL_BINARY"
			exit 1
		fi
		log_info "Installing ${BIN_NAME} from local binary: $CLAUDE_CODEX_LOCAL_BINARY"
		cp "$CLAUDE_CODEX_LOCAL_BINARY" "$tmp_dir/${BIN_NAME}"
		version="local"
		cd "$tmp_dir"
	else
		log_info "Installing ${BIN_NAME} from GitHub releases..."
		fetch_release "$platform" "$tmp_dir"
		version="$RELEASE_VERSION"
	fi

	local install_dir="${CLAUDE_CODEX_INSTALL_DIR:-}"
	if [ -z "$install_dir" ]; then
		if [[ -w /usr/local/bin ]]; then
			install_dir="/usr/local/bin"
		else
			install_dir="$HOME/.local/bin"
			mkdir -p "$install_dir"
		fi
	fi

	mkdir -p "$install_dir" 2>/dev/null || true

	if [ -f "$install_dir/${BIN_NAME}" ]; then
		local existing_version
		existing_version=$("$install_dir/${BIN_NAME}" --version 2>/dev/null | grep -oE 'v?[0-9]+\.[0-9]+\.[0-9]+' || echo "unknown")
		log_info "Existing installation found: $existing_version"
		log_info "Upgrading to: $version"
	fi

	log_info "Installing to $install_dir..."
	local tmp_binary="$install_dir/${BIN_NAME}.tmp.$$"

	if [[ -w "$install_dir" ]]; then
		cp "${BIN_NAME}" "$tmp_binary"
		chmod +x "$tmp_binary"
		mv -f "$tmp_binary" "$install_dir/${BIN_NAME}"
	else
		if ! sudo cp "${BIN_NAME}" "$tmp_binary"; then
			log_error "Failed to install to $install_dir (sudo required)"
			exit 1
		fi
		sudo chmod +x "$tmp_binary"
		sudo mv -f "$tmp_binary" "$install_dir/${BIN_NAME}"
	fi

	if [[ "$(uname -s)" == "Darwin" ]]; then
		if command -v xattr &>/dev/null; then
			xattr -d com.apple.quarantine "$install_dir/${BIN_NAME}" 2>/dev/null || true
		fi
		if command -v codesign &>/dev/null; then
			codesign --remove-signature "$install_dir/${BIN_NAME}" 2>/dev/null || true
			codesign --sign - --force "$install_dir/${BIN_NAME}" 2>/dev/null || true
		fi
	fi

	log_success "${BIN_NAME} installed to $install_dir/${BIN_NAME}"

	if [[ ":$PATH:" != *":$install_dir:"* ]]; then
		log_warning "$install_dir is not in your PATH"
		echo ""
		echo "Add this to your shell profile (~/.bashrc, ~/.zshrc, etc.):"
		echo "  export PATH=\"\$PATH:$install_dir\""
		echo ""
	fi

	cd - >/dev/null || cd "$HOME"

	INSTALL_DIR="$install_dir"
}

verify_installation() {
	local install_dir="$1"

	if [ ! -x "$install_dir/${BIN_NAME}" ]; then
		log_error "${BIN_NAME} binary not found or not executable at $install_dir/${BIN_NAME}"
		exit 1
	fi

	if ! "$install_dir/${BIN_NAME}" --version &>/dev/null; then
		log_error "${BIN_NAME} binary exists but failed to run"
		exit 1
	fi

	log_success "${BIN_NAME} is installed and ready!"
	echo ""
	"$install_dir/${BIN_NAME}" --version
	echo ""
}

print_manual_steps() {
	echo "Finish the setup yourself:"
	echo "  ${BIN_NAME} enable              # service, subagent cards, Claude Code settings"
	echo "  codex login                     # sign in to ChatGPT with the Codex CLI"
	echo "  ${BIN_NAME} codex auth status   # confirm the proxy sees that login"
	echo ""
	echo "Documentation: https://github.com/${REPO}"
	echo ""
}

run_setup() {
	local install_dir="$1"

	if [ "${CLAUDE_CODEX_NO_SETUP:-}" = "1" ]; then
		log_info "Skipping setup (CLAUDE_CODEX_NO_SETUP=1)"
		print_manual_steps
		return 0
	fi

	log_info "Running ${BIN_NAME} enable..."
	if "$install_dir/${BIN_NAME}" enable; then
		log_success "Setup finished"
		echo ""
		echo "Switch it off any time with: ${BIN_NAME} disable (and back on with ${BIN_NAME} enable)"
		echo "Remove everything with: ${BIN_NAME} uninstall"
		echo "Documentation: https://github.com/${REPO}"
		echo ""
	else
		log_warning "${BIN_NAME} is installed, but setup did not complete"
		echo ""
		echo "Fix the problem reported above and retry with:"
		echo "  $install_dir/${BIN_NAME} enable"
		echo ""
	fi
}

main() {
	echo ""
	echo "${BIN_NAME} installer"
	echo ""

	log_info "Detecting platform..."
	local platform
	platform=$(detect_platform)
	log_info "Platform: $platform"

	install_from_release "$platform"

	verify_installation "$INSTALL_DIR"

	run_setup "$INSTALL_DIR"
}

main "$@"
