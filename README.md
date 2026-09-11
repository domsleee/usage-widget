# usage-widget

Always-on-top desktop widget (Rust, egui) for GitHub Copilot, Claude and Codex usage.

<img width="323" height="237" alt="image" src="https://github.com/user-attachments/assets/29bc8928-0440-46c9-98d3-0cd0231311bb" />

Shows plans, renewal countdowns, usage windows and dollar spend.

## Install

Needs Rust. Built for Windows; runs on macOS without `--startup`.

```
cargo install --git https://github.com/pepsi-enjoyer/usage-widget
usage-widget --startup   # start at login; --no-startup undoes
usage-widget
```

Re-run `cargo install` to upgrade.

## Use

- Drag to move; position is remembered.
- Right-click: refresh, usage pages, edit config, quit (no taskbar entry).
- Click a service name to open its usage page.

## Config

`usage-widget config` creates and opens `config.toml` (in `$VISUAL`/`$EDITOR` if set):
`%APPDATA%\usage-widget\` on Windows, `~/Library/Application Support/usage-widget/` on
macOS. Restart the widget after editing.

```toml
refresh_mins = 5    # minutes between refreshes, minimum 1
opacity = 85        # 20-100, Windows only
precision = 1       # decimal places for percentages, 0-3

[copilot]
enabled = true      # false hides a service; same key under each

[claude]
enabled = true
api = true          # false = read Claude Code's cache only
browser_cookies = false  # macOS: read the claude.ai login cookie for the renewal date
estimate = false    # guess the fraction of the next percent from local token logs, shown with ~

[codex]
enabled = true
estimate = false    # as for Claude
```

## Credentials

Reuses the CLIs' stored credentials; never writes them.

| Service | Credential | Endpoints |
| --- | --- | --- |
| Copilot | `gh auth token`, `GITHUB_TOKEN` or `GH_TOKEN` | `api.github.com/copilot_internal/user` |
| Claude | `~/.claude/.credentials.json` (macOS: keychain) | `api.anthropic.com/api/oauth/usage` |
| Codex | `~/.codex/auth.json` | `chatgpt.com/backend-api/wham/usage`, `/subscriptions` |

Claude reads Claude Code's cache first and calls the API at most every 15 minutes,
honouring rate-limit delays; old data shows its age. For live 5h/weekly numbers with no extra requests, add this to your Claude Code
statusline script:

```bash
INPUT=$(cat)
case "$INPUT" in *'"five_hour"'*) printf '%s' "$INPUT" > ~/.claude/usage-widget-statusline.json;; esac
```

Estimates can't see usage outside the local logs (cloud, claude.ai, other devices), and
Claude's needs a few percentage ticks to calibrate. The renewal cookie is read from
Chrome, Arc, Brave or Edge once a day; macOS asks once for keychain access. Expired
token? Run `claude` or `codex` once. Endpoints are undocumented and may change. Dollar
rates: `src/providers.rs`.
