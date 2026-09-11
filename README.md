# usage-widget

Always-on-top widget for GitHub Copilot, Claude and Codex usage, plans and renewals.

<img width="323" height="237" alt="image" src="https://github.com/user-attachments/assets/29bc8928-0440-46c9-98d3-0cd0231311bb" />

## Install

Needs Rust. Built for Windows; runs on macOS without `--startup`.

```
cargo install --git https://github.com/pepsi-enjoyer/usage-widget
usage-widget --startup   # start at login; --no-startup undoes
usage-widget
```

Re-run `cargo install` and restart the widget to upgrade.

## Claude Code setup

**Live Claude 5-hour and weekly usage requires a Claude Code status line that saves
its input for the widget.** Add this at the start of your status line script:

```bash
INPUT=$(cat)
case "$INPUT" in *'"five_hour"'*) printf '%s' "$INPUT" > ~/.claude/usage-widget-statusline.json;; esac
```

Use `$INPUT` for the rest of the script; stdin has already been read. The file updates
while you use Claude Code, with no extra API requests.

Without this setup, the widget falls back to cached usage and API requests at most
every 15 minutes; rate limits can leave the numbers stale.

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
opacity = 85        # window opacity, 20-100
precision = 1       # decimal places for percentages, 0-3

[copilot]
enabled = true      # false hides a service; same key under each

[claude]
enabled = true
api = true          # false = read Claude Code's cache only
browser_cookies = false  # macOS: read the claude.ai login cookie for the renewal date
estimate = false    # estimate between percentage ticks from local logs; shown with ~

[codex]
enabled = true
estimate = false    # as for Claude
```

## Credentials

Reuses the CLIs' stored credentials; never writes them.

| Service | Credential |
| --- | --- |
| Copilot | `gh auth token`, `GITHUB_TOKEN` or `GH_TOKEN` |
| Claude | `~/.claude/.credentials.json` (macOS: keychain) |
| Codex | `~/.codex/auth.json` |

Run `claude` or `codex` to refresh expired tokens. API endpoints are undocumented.

Estimates only cover local logs; Claude needs a few percentage ticks to calibrate.
Optional Claude renewal cookies come from Chrome, Arc, Brave or Edge on macOS,
with a keychain access prompt on first use. Dollar rates: `src/providers.rs`.
