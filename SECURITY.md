# Security policy

## Supported versions

Only the latest release (and the `main` branch) receives security fixes.

## Reporting a vulnerability

Please do not open a public issue for a vulnerability. Use GitHub's private reporting instead: open the repository's **Security** tab and choose **Report a vulnerability** (https://github.com/josefl03/claude-codex/security/advisories/new).

Include what you found, how to reproduce it, the version (`claude-codex --version`) and your operating system. Do not include real credentials or tokens; redact them.

You can expect an acknowledgement within a few days. This is a volunteer project, so there is no fixed fix timeline, but confirmed issues are handled with priority and credited in the advisory unless you prefer otherwise.

## Scope and known design limits

- **The listener has no inbound authentication.** Anyone who can reach the port can use your Claude and ChatGPT sessions through the proxy. It binds to `127.0.0.1` by default. Binding it to another address (`CCP_BIND_ADDRESS`) is your decision and your responsibility. Reports that amount to "it accepts requests from whoever can connect" are a known limitation, not a vulnerability, unless the default loopback binding can be bypassed.
- The proxy reads the Codex CLI's credential file (`~/.codex/auth.json`) and writes refreshed tokens back to it. It does not store Claude credentials.
- `CCP_TRAFFIC_LOG` writes captured traffic (with redaction) to the state directory. Treat those files as sensitive.

Issues in the upstream projects ([raine/claude-code-proxy](https://github.com/raine/claude-code-proxy), [fcakyon/claude-code-with-codex](https://github.com/fcakyon/claude-code-with-codex)) should also be reported to them if the problem is in code this fork inherited unchanged.
