# Examples

Real output from the author's machine on 2026-10-20 (Linux, Claude Code 2.1.304, `claude-codex` 0.6.0). The home directory is shortened to `~`. Everything marked "run" was executed; prompts marked "not run" are only suggestions.

## Status

```sh
claude-codex status
```

```text
claude-codex  0.6.0
state         enabled
service       installed: yes, active: yes, enabled: yes
unit file     ~/.config/systemd/user/claude-codex.service
linger        on (starts at boot; not enabled by setup)
proxy         port 18765, listening: yes, responds: yes
Claude Code   ANTHROPIC_BASE_URL=http://localhost:18765 (this proxy) in ~/.claude/settings.json
agent cards   21 managed in ~/.claude/agents
Codex login   found
```

The linger line says who turned linger on. Here the user did it by hand, so `disable` and `uninstall` leave it alone. When `enable` turns it on, the line reads `on (starts at boot; enabled by setup)` and `disable` turns it off again.

## Models and aliases

```sh
claude-codex models
```

```text
codex: gpt-5.2, gpt-5.2-fast, gpt-5.3-codex, gpt-5.3-codex-fast, gpt-5.3-codex-spark, gpt-5.3-codex-spark-fast, gpt-5.4, gpt-5.4-fast, gpt-5.4-mini, gpt-5.4-mini-fast, gpt-5.5, gpt-5.5-fast, gpt-5.6-luna, gpt-5.6-luna-fast, gpt-5.6-sol, gpt-5.6-sol-fast, gpt-5.6-terra, gpt-5.6-terra-fast, gpt-6-astra, gpt-6-astra-fast, gpt-6-luna, gpt-6-luna-fast, gpt-6-sol, gpt-6-sol-fast, gpt-6.1-sol, gpt-6.1-sol-fast
latest aliases (resolved by the proxy on every request):
  gpt-astra-latest        ->  gpt-6-astra
  gpt-codex-latest        ->  gpt-5.3-codex
  gpt-codex-spark-latest  ->  gpt-5.3-codex-spark
  gpt-latest              ->  gpt-6.1-sol
  gpt-luna-latest         ->  gpt-6-luna
  gpt-mini-latest         ->  gpt-5.4-mini
  gpt-sol-latest          ->  gpt-6.1-sol
  gpt-terra-latest        ->  gpt-5.6-terra
```

The command also printed lines for Kimi, Grok and Cursor models; they are left out here. The list comes from the Codex backend and changes when OpenAI adds or removes a model.

## A card

```sh
cat ~/.claude/agents/gpt-luna-latest.md
```

```markdown
---
name: gpt-luna-latest
description: "Always the newest luna model of the gpt family, currently gpt-6-luna (provider: codex), through the claude-codex proxy. Delegate a self-contained task to it to get that model's answer."
model: gpt-luna-latest
---
<!-- managed-by: claude-codex; do not edit -->

You are a subagent running on `gpt-luna-latest`, which the claude-codex proxy resolves on every request to the newest luna model of the gpt family (currently `gpt-6-luna`, codex). Complete the task you are given and reply with the result only.
```

A card for a fixed model (`gpt-6-luna.md`) has the same shape with `model: gpt-6-luna`. When the alias target changes, the `description` is refreshed and the `model:` line stays.

## Three subagents in parallel (run)

From an empty directory, with `claude -p --model haiku --effort low --max-budget-usd 0.5 --no-session-persistence --permission-mode dontAsk`:

```text
Reply in English. Start three gpt-luna-latest subagents in parallel, one per question, each answering in one sentence: (1) what is git bisect for, (2) what is git reflog for, (3) what is git worktree for. Then print the three answers as a numbered list.
```

Output, 10 seconds in total:

```text
Three gpt-luna-latest subagents launched in parallel, one per question. Their answers will come back as completion notifications; I'll print the numbered list once all three have arrived.

Bisect done. Waiting on reflog and worktree agents.

1. `git bisect` uses binary search across a commit range to find the commit that introduced a bug.
2. `git reflog` records recent movements of Git references, letting you find and recover commits or branch states that may no longer be reachable.
3. `git worktree` lets you check out multiple branches of the same repository in separate directories, so you can work on them independently without switching branches in one directory.
```

stderr also carried `[claude-code:unrecognized_model] {"model":"gpt-luna-latest",...}`. Claude Code does not know the id, and the request went through anyway.

## One subagent (run)

Same setup, one subagent:

```text
Use the gpt-luna-latest subagent to list three everyday uses of git bisect, one line each. Show me its answer, then add one line saying which subagent answered.
```

The answer came back from `gpt-luna-latest` in Spanish. That is not a proxy behavior: the author's own Claude Code instructions ask for Spanish. Add "Reply in English" to the prompt if you want to control the language.

## More prompts (not run)

```text
Use the gpt-sol-latest subagent to review this diff and list anything risky.
```

```text
Ask the gpt-sol-latest subagent for a second opinion on this migration plan. Tell me where it disagrees with you.
```

```text
Start one gpt-luna-latest subagent for each package under packages/ to list its public exports, then merge the lists.
```

## Switching off (not run)

```sh
claude-codex disable
```

This stops the service, removes the managed cards, takes `ANTHROPIC_BASE_URL` out of `settings.json`, and turns linger off if setup had turned it on. Restart Claude Code. `claude-codex enable` reverses it.
