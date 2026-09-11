# usage-widget

Always-on-top desktop widget (Rust, egui) for GitHub Copilot, Claude and Codex usage.

<img width="323" height="237" alt="image" src="https://github.com/user-attachments/assets/29bc8928-0440-46c9-98d3-0cd0231311bb" />

Per service: plan, countdown to renewal or quota reset, rolling windows (5h, week,
per-model caps) with reset countdowns (hover either for the local time), and spend in
dollars.

## Install

Needs Rust. Built for Windows; runs on macOS without `--startup`.

```
cargo install --git https://github.com/pepsi-enjoyer/usage-widget
usage-widget --startup   # start at login; --no-startup undoes
usage-widget
```

Re-run `cargo install` to upgrade. `install.ps1` does the same from a clone.

## Use

- Drag to move; position is remembered.
- Right-click: refresh, usage pages, edit config, quit (no taskbar entry).
- Click a service name to open its usage page.

## Config

`usage-widget config` creates and opens `config.toml` (in `$VISUAL`/`$EDITOR` if set).
It lives in `%APPDATA%\usage-widget\` on Windows and
`~/Library/Application Support/usage-widget/` on macOS. Restart the widget after editing.

```toml
refresh_mins = 5    # minutes between refreshes, minimum 1
opacity = 85        # window opacity 20-100, Windows only
precision = 1       # decimal places for percentages, 0-3

[copilot]
enabled = true      # false hides the service; same for [claude] and [codex]

[claude]
enabled = true
api = true          # false = only read Claude Code's cache, never call Anthropic
browser_cookies = false  # macOS: read your claude.ai login from Chrome/Arc/Brave/Edge for the renewal day
estimate = false    # estimate the part of the next percent from local token use

[codex]
enabled = true
estimate = false    # estimate the part of the next percent from local token use
```

## Credentials

Reuses what the CLIs already store; never writes them.

| Service | Credential | Endpoints |
| --- | --- | --- |
| Copilot | `gh auth token`, `GITHUB_TOKEN` or `GH_TOKEN` | `api.github.com/copilot_internal/user` |
| Claude | `~/.claude/.credentials.json` (macOS: keychain) | `api.anthropic.com/api/oauth/usage` |
| Codex | `~/.codex/auth.json` | `chatgpt.com/backend-api/wham/usage`, `/subscriptions` |

Claude's usage endpoint allows each token only a few calls before a 429 of up to an
hour, so the widget reads the copy Claude Code caches in `~/.claude.json` and calls
the API only when that is over 15 minutes old, at most every 15 minutes, honouring
`Retry-After`. Old data is labelled with its age.

For live Claude 5h/weekly numbers, have your Claude Code statusline script save its
input (it carries `rate_limits` from every response, so no extra requests):

```bash
INPUT=$(cat)
case "$INPUT" in *'"five_hour"'*) printf '%s' "$INPUT" > ~/.claude/usage-widget-statusline.json;; esac
```

Codex and Claude report whole percents. With `estimate = true` their windows get an
estimated fraction, marked `~`: each refresh the widget reads what the local logs gained
(Codex CLI's `~/.codex/sessions`, Claude Code's `~/.claude/projects`), prices those
tokens (Codex's credit rate card, Anthropic's relative model prices), and divides the
cost since the last whole-percent tick by what a point has cost in that window. Codex
logs carry the percent with every response; for Claude the widget learns it from its own
readings, so its estimate needs a few ticks to warm up. Use the logs never see (Codex
cloud, claude.ai, other devices) makes the decimals a guess.

Copilot shows its monthly quota reset. Claude's renewal date is only served to claude.ai
browser sessions, so it needs `claude.browser_cookies = true`: the widget then decrypts
the claude.ai session cookie from Chrome, Arc, Brave or Edge (macOS asks once for
keychain access) and asks claude.ai once a day. Token expired? Run `claude` or `codex` once. Endpoints are
undocumented and may change. Dollar rates live in `src/providers.rs`.
