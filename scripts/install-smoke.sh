#!/usr/bin/env bash
#
# Install and usage smoke test, run by .github/workflows/install-smoke.yml on real
# GitHub-hosted runners. It must stay compatible with bash 3.2 (macOS /bin/bash).
#
#   scripts/install-smoke.sh unix      one-liner install + usage checks (gating)
#   scripts/install-smoke.sh service   real autostart service attempt (may fail on a headless runner)
#   scripts/install-smoke.sh windows   the installer must refuse to run, cleanly
#   scripts/install-smoke.sh report    print the result table (also to $GITHUB_STEP_SUMMARY)
#
# Every check is recorded in $SMOKE_DIR/results.tsv as: status<TAB>name<TAB>note.
# Statuses: ok, fail (a bug or a broken release), warn (runner limitation, with its reason).
# Never uses real credentials: the Codex login is a toy file.

set -uo pipefail

REPO="${SMOKE_REPO:-josefl03/claude-codex}"
INSTALL_URL="https://raw.githubusercontent.com/${REPO}/main/scripts/install.sh"
SMOKE_DIR="${SMOKE_DIR:-${RUNNER_TEMP:-/tmp}/install-smoke}"
RESULTS="$SMOKE_DIR/results.tsv"
mkdir -p "$SMOKE_DIR/logs"
touch "$RESULTS"

PHASE_FAILED=0
COUNTER=0

# ---------------------------------------------------------------- recording

note_of() { # the FAIL line if any, else the last lines of a log; single line, no pipes
	if grep -q '^FAIL: ' "$1" 2>/dev/null; then
		grep '^FAIL: ' "$1" | head -n 1 | tr '|' ' ' | cut -c1-240
		return 0
	fi
	grep -v '^[[:space:]]*$' "$1" 2>/dev/null | tail -n 2 | tr '\n|' '  ' | cut -c1-240
}

# check KIND NAME FUNCTION...   (KIND: hard = failure fails the job; soft = warning only)
check() {
	local kind=$1 name=$2
	shift 2
	COUNTER=$((COUNTER + 1))
	local log
	log="$SMOKE_DIR/logs/$(printf '%02d' "$COUNTER")-$$.log"
	echo "::group::${name}"
	( "$@" ) >"$log" 2>&1
	local rc=$?
	cat "$log"
	echo "::endgroup::"
	if [ "$rc" -eq 0 ]; then
		printf 'ok\t%s\t%s\n' "$name" "$(note_of "$log")" >>"$RESULTS"
	elif [ "$kind" = soft ]; then
		printf 'warn\t%s\t%s\n' "$name" "$(note_of "$log")" >>"$RESULTS"
		echo "::warning::${name} failed (runner limitation or real problem, see log)"
	else
		printf 'fail\t%s\t%s\n' "$name" "$(note_of "$log")" >>"$RESULTS"
		echo "::error::${name} failed"
		PHASE_FAILED=1
	fi
	return 0
}

say() { printf '%s\n' "$*"; }
die() { printf 'FAIL: %s\n' "$*"; exit 1; }

latest_tag() {
	local json
	if [ -n "${GH_TOKEN:-}" ]; then
		json=$(curl -fsS --retry 3 -H "Authorization: Bearer $GH_TOKEN" "https://api.github.com/repos/${REPO}/releases/latest")
	else
		json=$(curl -fsS --retry 3 "https://api.github.com/repos/${REPO}/releases/latest")
	fi
	printf '%s\n' "$json" | sed -n 's/.*"tag_name": *"\([^"]*\)".*/\1/p' | head -n 1
}

# ---------------------------------------------------------------- sandbox

# Fresh root with an isolated HOME and a toy Codex login.
new_root() {
	ROOT=$(mktemp -d "${SMOKE_DIR}/root.XXXXXX")
	mkdir -p "$ROOT/home"
	printf '%s\n' '{"tokens":{"access_token":"toy","refresh_token":"toy","id_token":"toy","account_id":"toy"}}' >"$ROOT/codex-auth.json"
	PORT=$((20000 + RANDOM % 20000))
}

# sbx CMD...: run with every location redirected (CCP_SETUP_SANDBOX needs all of them).
sbx() {
	env HOME="$ROOT/home" XDG_STATE_HOME="$ROOT/state" XDG_CONFIG_HOME="$ROOT/xdgconfig" \
		CCP_CONFIG_DIR="$ROOT/cfg" CCP_SERVICE_DIR="$ROOT/service" CCP_AGENTS_DIR="$ROOT/agents" \
		CLAUDE_CONFIG_DIR="$ROOT/claude" CCP_CODEX_AUTH_FILE="$ROOT/codex-auth.json" \
		CCP_AGENT_CARDS=1 CCP_MODEL_DISCOVERY=0 CCP_SETUP_SANDBOX=1 PORT="$PORT" "$@"
}

# ---------------------------------------------------------------- unix checks

tools_info() {
	uname -a
	uname -m
	say "bash on PATH: $(command -v bash) $(bash -c 'echo $BASH_VERSION')"
	say "/bin/bash: $(/bin/bash -c 'echo $BASH_VERSION')"
	local t
	for t in sha256sum shasum sed mktemp date curl tar file paste xattr codesign cargo; do
		say "$t: $(command -v "$t" || echo '(absent)')"
	done
	sed --version >/dev/null 2>&1 && say "sed: GNU" || say "sed: BSD"
	say "anonymous GitHub API budget of this runner IP: $(curl -s https://api.github.com/rate_limit | tr -d ' \n' | sed -n 's/.*"core":{\([^}]*\)}.*/\1/p')"
	if [ "$(uname -s)" = Darwin ]; then
		sw_vers
		command -v sha256sum >/dev/null 2>&1 && say "note: sha256sum present, shasum branch not exercised" || say "sha256sum absent: installer must use the shasum branch"
	fi
}

syntax_bash32() {
	local f
	f=$(mktemp "$SMOKE_DIR/install.XXXXXX")
	curl -fsSL "$INSTALL_URL" -o "$f" || die "cannot fetch install.sh"
	/bin/bash -n "$f" || die "install.sh has a syntax error for /bin/bash $(/bin/bash -c 'echo $BASH_VERSION')"
	say "/bin/bash -n OK ($(/bin/bash -c 'echo $BASH_VERSION'))"
}

# Pass 1: the real one-liner, binary only.
oneliner_nosetup() {
	say "expected latest release: $LATEST"
	curl -fsSL "$INSTALL_URL" | env HOME="$ROOT/home" CLAUDE_CODEX_NO_SETUP=1 bash 2>&1 | tee "$ROOT/pass1.log"
	st=("${PIPESTATUS[@]}")
	[ "${st[0]}" -eq 0 ] && [ "${st[1]}" -eq 0 ] || die "one-liner failed (pipeline statuses: ${st[*]})"
	BIN="$ROOT/home/.local/bin/claude-codex"
	[ -x "$BIN" ] || die "binary missing at $BIN"
}

no_cargo_fallback() {
	local log=$1
	grep -q "Checksum verified" "$log" || die "no 'Checksum verified' line: the prebuilt asset was not used"
	grep -q "Downloading claude-codex-" "$log" || die "no download line"
	if grep -Eq "No prebuilt|Building with cargo|cargo install" "$log"; then
		die "installer fell back to cargo"
	fi
	grep -E "Downloading|Checksum|Installed" "$log"
}

installed_version_matches() {
	local want=${LATEST#v} got
	got=$("$BIN" --version </dev/null) || die "--version failed"
	say "$got (expected ${want})"
	case "$got" in *"$want"*) ;; *) die "installed version is not the latest release";; esac
}

arch_matches() {
	local out m
	out=$(file -b "$BIN")
	m=$(uname -m)
	say "uname -m: $m"
	say "file: $out"
	case "$m" in
	x86_64 | amd64) printf '%s' "$out" | grep -Eqi 'x86[-_]64' || die "binary is not x86_64";;
	aarch64 | arm64) printf '%s' "$out" | grep -Eqi 'aarch64|arm64' || die "binary is not arm64";;
	*) die "unexpected uname -m $m";;
	esac
	if [ "$(uname -s)" = Linux ]; then
		printf '%s' "$out" | grep -Eqi 'static' || die "Linux binary is not static"
	fi
}

nosetup_left_nothing() {
	local p bad=0
	for p in "$ROOT/home/.claude" "$ROOT/home/.config/systemd" "$ROOT/home/Library/LaunchAgents"; do
		if [ -e "$p" ]; then say "unexpected: $p exists"; bad=1; fi
	done
	grep -q "Skipping setup" "$ROOT/pass1.log" || die "installer did not report skipping setup"
	[ "$bad" -eq 0 ] || die "NO_SETUP created files besides the binary"
	say "only the binary was installed"
}

uses_version() { "$BIN" version </dev/null; }

uses_status() {
	local out
	out=$(sbx "$BIN" status </dev/null) || die "status exited non-zero"
	printf '%s\n' "$out"
	printf '%s' "$out" | grep -q "Codex login *found" || die "status does not see the toy Codex login"
}

uses_models() {
	local out
	out=$(sbx "$BIN" models </dev/null) || die "models failed"
	printf '%s\n' "$out" | head -n 3 | cut -c1-160
	printf '%s' "$out" | grep -q "gpt-" || die "no gpt models listed"
}

uses_serve() {
	local pid out i ok=0
	sbx "$BIN" serve --no-monitor --port "$PORT" >"$ROOT/serve.log" 2>&1 &
	pid=$!
	for i in 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15 16 17 18 19 20; do
		if out=$(curl -fsS "http://127.0.0.1:$PORT/v1/models" 2>/dev/null); then ok=1; break; fi
		kill -0 "$pid" 2>/dev/null || break
		sleep 1
	done
	if [ "$ok" -ne 1 ]; then
		cat "$ROOT/serve.log"
		kill "$pid" 2>/dev/null
		die "serve did not answer /v1/models on port $PORT"
	fi
	printf '%s' "$out" | cut -c1-200
	echo
	local n
	n=$(printf '%s' "$out" | grep -o '"id":"[^"]*"' | wc -l | tr -d ' ')
	say "models listed: $n"
	kill "$pid" 2>/dev/null
	wait "$pid" 2>/dev/null
	[ "$n" -gt 5 ] || die "too few models listed ($n)"
	printf '%s' "$out" | grep -q '"id":"gpt-' || die "no gpt model in /v1/models"
}

uses_agents_sync() {
	sbx "$BIN" agents sync </dev/null || die "agents sync failed"
	local n
	n=$(ls "$ROOT/agents" 2>/dev/null | wc -l | tr -d ' ')
	say "cards created: $n in $ROOT/agents"
	[ "$n" -gt 0 ] || die "no cards created"
	sbx "$BIN" agents list </dev/null | head -n 3
	sbx "$BIN" agents clean </dev/null || die "agents clean failed"
	n=$(ls "$ROOT/agents" 2>/dev/null | wc -l | tr -d ' ')
	[ "$n" -eq 0 ] || die "agents clean left $n cards"
	say "agents clean removed them"
}

# Pass 2: the real one-liner with a complete setup, file-only service (CCP_SERVICE_DIR).
oneliner_full() {
	curl -fsSL "$INSTALL_URL" | sbx /bin/bash 2>&1 | tee "$ROOT2/pass2.log"
	st=("${PIPESTATUS[@]}")
	[ "${st[0]}" -eq 0 ] && [ "${st[1]}" -eq 0 ] || die "one-liner failed (pipeline statuses: ${st[*]})"
}

full_summary_parsed() {
	local log=$ROOT/pass2.log
	grep -Eq "enable +done" "$log" || die "installer summary does not say 'enable done'"
	grep -Eq "Codex login +found \(account toy" "$log" || die "Codex login detail not parsed (sed/paste differences?)"
	grep -Eq "agent cards +[0-9]+ managed" "$log" || die "agent cards line missing from the summary"
	grep -E "^  (binary|enable|service|agent cards|Claude Code|Codex login)" "$log"
}

full_artifacts() {
	local unit
	case "$(uname -s)" in
	Darwin)
		unit="$ROOT/service/com.claude-codex.proxy.plist"
		[ -f "$unit" ] || die "plist not generated at $unit"
		grep -q "com.claude-codex.proxy" "$unit" || die "plist lacks the label"
		grep -q "ProgramArguments" "$unit" || die "plist lacks ProgramArguments"
		command -v plutil >/dev/null 2>&1 && plutil -lint "$unit"
		;;
	*)
		unit="$ROOT/service/claude-codex.service"
		[ -f "$unit" ] || die "unit not generated at $unit"
		grep -q "^ExecStart=" "$unit" || die "unit lacks ExecStart"
		;;
	esac
	say "service definition: $unit"
	grep -E "ExecStart|ProgramArguments|Label" -A1 "$unit" | head -n 8
	grep -q ANTHROPIC_BASE_URL "$ROOT/claude/settings.json" || die "settings.json lacks ANTHROPIC_BASE_URL"
	say "settings.json has ANTHROPIC_BASE_URL"
	local n
	n=$(ls "$ROOT/agents" 2>/dev/null | wc -l | tr -d ' ')
	[ "$n" -gt 0 ] || die "enable created no cards"
	say "cards: $n"
}

lifecycle_disable_enable() {
	sbx "$BIN" disable </dev/null || die "disable failed"
	if grep -q ANTHROPIC_BASE_URL "$ROOT/claude/settings.json" 2>/dev/null; then die "disable left ANTHROPIC_BASE_URL"; fi
	[ -f "$(ls "$ROOT"/service/* 2>/dev/null | head -n 1)" ] || die "disable removed the unit file (it must keep it)"
	say "disabled: settings reverted, unit kept"
	sbx "$BIN" status </dev/null | head -n 4
	sbx "$BIN" enable </dev/null || die "second enable failed"
	grep -q ANTHROPIC_BASE_URL "$ROOT/claude/settings.json" || die "re-enable did not restore settings"
	say "re-enabled"
}

lifecycle_uninstall() {
	sbx "$BIN" uninstall --yes </dev/null || die "uninstall failed"
	[ ! -e "$ROOT/home/.local/bin/claude-codex" ] || die "uninstall left the binary"
	if ls "$ROOT"/service/* >/dev/null 2>&1; then die "uninstall left the service definition"; fi
	if grep -q ANTHROPIC_BASE_URL "$ROOT/claude/settings.json" 2>/dev/null; then die "uninstall left ANTHROPIC_BASE_URL"; fi
	say "uninstalled: binary, service definition and settings entry are gone"
}

phase_unix() {
	LATEST=$(latest_tag)
	[ -n "$LATEST" ] || { printf 'fail\tlatest release lookup\tno tag_name from the API\n' >>"$RESULTS"; return 1; }
	check hard "Tools and bash version" tools_info
	check hard "install.sh parses with /bin/bash" syntax_bash32

	new_root
	echo "ROOT=$ROOT" >"$SMOKE_DIR/state.env"
	check hard "One-liner, binary only (CLAUDE_CODEX_NO_SETUP=1)" oneliner_nosetup
	if [ -x "$ROOT/home/.local/bin/claude-codex" ]; then
		BIN="$ROOT/home/.local/bin/claude-codex"
		echo "BIN=$BIN" >>"$SMOKE_DIR/state.env"
		check hard "Prebuilt release used (no cargo fallback)" no_cargo_fallback "$ROOT/pass1.log"
		check hard "Installed version is the latest release" installed_version_matches
		check hard "Binary architecture matches the machine" arch_matches
		check hard "NO_SETUP leaves only the binary" nosetup_left_nothing
		check hard "claude-codex version" uses_version
		check hard "claude-codex status" uses_status
		check hard "claude-codex models" uses_models
		check hard "serve + GET /v1/models" uses_serve
		check hard "agents sync / list / clean" uses_agents_sync
	fi

	# Pass 2 in its own root so nothing from pass 1 hides a problem.
	ROOT_FIRST=$ROOT
	new_root
	ROOT2=$ROOT
	echo "ROOT2=$ROOT2" >>"$SMOKE_DIR/state.env"
	check hard "One-liner, full setup with /bin/bash (service file-only)" oneliner_full
	if [ -x "$ROOT/home/.local/bin/claude-codex" ]; then
		BIN="$ROOT/home/.local/bin/claude-codex"
		check hard "Prebuilt release used in the full-setup pass" no_cargo_fallback "$ROOT/pass2.log"
		check hard "Summary parsed (sed/paste on this OS)" full_summary_parsed
		check hard "Service definition, settings.json and cards created" full_artifacts
		check hard "status after enable" uses_status
		check hard "disable keeps the unit, enable restores" lifecycle_disable_enable
		check hard "uninstall removes everything" lifecycle_uninstall
	fi
	return "$PHASE_FAILED"
}

# ---------------------------------------------------------------- real service

service_diag() {
	say "user: $(id -un) uid $(id -u)"
	say "XDG_RUNTIME_DIR=${XDG_RUNTIME_DIR:-(unset)} DBUS_SESSION_BUS_ADDRESS=${DBUS_SESSION_BUS_ADDRESS:-(unset)}"
	case "$(uname -s)" in
	Linux)
		say "PID 1: $(ps -p 1 -o comm= 2>/dev/null)"
		systemctl --version 2>&1 | head -n 1
		systemctl --user is-system-running 2>&1 | head -n 2
		ls -ld "/run/user/$(id -u)" 2>&1
		;;
	Darwin)
		say "console user: $(stat -f %Su /dev/console 2>&1)"
		launchctl print "gui/$(id -u)" 2>&1 | head -n 3
		;;
	esac
}

wait_models() {
	local i
	for i in 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15; do
		if curl -fsS "http://127.0.0.1:18765/v1/models" 2>/dev/null | grep -q '"id"'; then
			say "service answered /v1/models after ${i}s"
			return 0
		fi
		sleep 1
	done
	die "nothing answers on 127.0.0.1:18765"
}

real_service_install() {
	"$BIN" service install </dev/null || die "service install failed"
	"$BIN" service status </dev/null
}

real_service_uninstall() {
	"$BIN" service uninstall </dev/null || die "service uninstall failed"
}

linux_prepare_user_bus() {
	local uid
	uid=$(id -u)
	sudo -n loginctl enable-linger "$(id -un)" || die "cannot enable linger"
	sudo -n systemctl start "user@${uid}.service" || die "cannot start user@${uid}.service"
	sleep 2
	ls -ld "/run/user/$uid" && say "user bus socket: $(ls /run/user/$uid/bus 2>&1)"
}

phase_service() {
	# shellcheck disable=SC1090
	. "$SMOKE_DIR/state.env" 2>/dev/null || { printf 'fail\tservice phase\tno state from the unix phase\n' >>"$RESULTS"; return 1; }
	[ -x "${BIN:-}" ] || { printf 'warn\tReal service\tno installed binary to test\n' >>"$RESULTS"; return 1; }
	# The runner is ephemeral, so the real HOME is used here (a service manager reads it, not an isolated one).
	export CCP_CODEX_AUTH_FILE="$ROOT/codex-auth.json"
	check soft "Runner environment for services" service_diag
	check soft "Real service install, as a user would run it" real_service_install
	if grep -q '^warn.*Real service install, as a user' "$RESULTS" && [ "$(uname -s)" = Linux ]; then
		check soft "Linux: give the runner a systemd user session (sudo loginctl enable-linger, start user@UID)" linux_prepare_user_bus
		export XDG_RUNTIME_DIR="/run/user/$(id -u)"
		export DBUS_SESSION_BUS_ADDRESS="unix:path=$XDG_RUNTIME_DIR/bus"
		check soft "Real service install, with a user session" real_service_install
	fi
	check soft "Real service answers /v1/models" wait_models
	check soft "Real service uninstall" real_service_uninstall
	grep -q '^warn' "$RESULTS" && return 1
	return 0
}

# ---------------------------------------------------------------- windows

list_tmp() { ls -A "${TMPDIR:-/tmp}" 2>/dev/null | grep -E '^tmp\.|^claude-codex' | sort; }

windows_refuses() {
	local home out rc before after
	home=$(mktemp -d "$SMOKE_DIR/winhome.XXXXXX")
	say "uname -s: $(uname -s)"
	before=$(list_tmp)
	out=$(curl -fsSL "$INSTALL_URL" | env HOME="$home" bash 2>&1)
	rc=$?
	after=$(list_tmp)
	printf '%s\n' "$out"
	say "installer exit code: $rc"
	[ "$rc" -ne 0 ] || die "installer succeeded on Windows"
	printf '%s' "$out" | grep -qi "not supported" || die "message does not say 'not supported'"
	printf '%s' "$out" | grep -q "WSL" || die "message does not point to WSL"
	printf '%s' "$out" | grep -q "wsl --install" || die "message has no actionable command"
	if printf '%s' "$out" | grep -qE "Downloading|Installed "; then die "installer started downloading before refusing"; fi
	[ "$before" = "$after" ] || die "temp files left behind: $after"
	if [ -n "$(find "$home" -mindepth 1 -print -quit)" ]; then die "files created in HOME: $(ls -A "$home")"; fi
	say "refused with exit $rc, no download, no temp dir, HOME untouched"
}

windows_service_static() {
	local f=src/service.rs
	[ -f "$f" ] || die "run from the repository root (checkout missing)"
	grep -q 'Platform::Other' "$f" || die "no Platform::Other"
	grep -q 'service autostart is not supported on' "$f" || die "unsupported message missing"
	local n
	n=$(grep -c 'if platform == Platform::Other' "$f")
	say "source check (not executed on Windows: there is no Windows build): $n entry points bail with 'service autostart is not supported on windows (only Linux/systemd and macOS/launchd)' when the OS is neither Linux nor macOS"
	[ "$n" -ge 2 ] || die "expected the guard in run_service_command and install_quiet"
}

phase_windows() {
	check hard "Installer refuses on Windows with an actionable message, leaving nothing" windows_refuses
	check hard "Service commands report 'not supported' off Linux/macOS (source check)" windows_service_static
	return "$PHASE_FAILED"
}

# ---------------------------------------------------------------- report

phase_report() {
	local out="${GITHUB_STEP_SUMMARY:-/dev/stdout}" status name note icon
	{
		echo "### ${SMOKE_LABEL:-install smoke}: $(uname -s) $(uname -m)"
		echo ""
		echo "| Step | Result | Detail |"
		echo "|---|---|---|"
		while IFS="$(printf '\t')" read -r status name note; do
			case "$status" in
			ok) icon="✅" ;;
			warn) icon="⚠️" ;;
			*) icon="❌" ;;
			esac
			echo "| ${name} | ${icon} | ${note} |"
		done <"$RESULTS"
		echo ""
		echo "⚠️ marks a runner limitation or an unconfirmed service attempt: the reason is in the detail and in the step log."
	} | tee -a "$out"
	grep -q '^fail' "$RESULTS" && return 1
	return 0
}

case "${1:-}" in
unix) phase_unix ;;
service) phase_service ;;
windows) phase_windows ;;
report) phase_report ;;
*) echo "usage: $0 unix|service|windows|report" >&2; exit 2 ;;
esac
