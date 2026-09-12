# usage-widget

Always-on-top widget for GitHub Copilot, Claude and Codex usage, plans and renewals.

| Dollar budgets | Plan usage windows |
| --- | --- |
| <img width="270" height="147" alt="Widget showing Copilot, Claude and Codex spend against their budgets, one line each, with a combined total" src="docs/usage-widget-dollars.png" /> | <img width="270" height="173" alt="Widget showing Claude and Codex plans with renewal dates, usage windows side by side and reset times" src="docs/usage-widget.png" /> |

## Install

Needs Rust. Built for Windows; runs on macOS without `--startup`.

```bash
cargo install --git https://github.com/pepsi-enjoyer/usage-widget
usage-widget --startup   # start at login; --no-startup undoes
usage-widget
```

Re-run `cargo install` and restart the widget to upgrade.

## Use

- Drag to move; position is remembered.
- Right-click: refresh, usage pages, edit config, quit (no taskbar entry).
- macOS: lives in the menu bar, with no Dock icon or Cmd+Tab entry. Its menu also hides and shows the widget.
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
# renewal_date = "2026-10-12"  # optional manual next billing date, including Windows
estimate = false    # estimate between percentage ticks from local logs; shown with ~

[codex]
enabled = true
estimate = false    # as for Claude
```

`claude.renewal_date` uses your local calendar date and overrides browser lookup.
Update it after each renewal; the widget does not assume a monthly billing schedule.
An invalid or past date shows a note while usage meters keep working.

### Look up Claude's renewal date with Playwright

When Claude's renewal date is missing, click the tiny circular-arrow icon beside
**Claude**. Once the date is shown, right-click the renewal text and choose
**Look up renewal** instead (right-click the Claude name on spend-only rows).
With Node.js and Chrome
installed, it sets up the helper automatically, opens Chrome for sign-in, and
applies the saved date without restarting the widget. The icon becomes a spinner
while a lookup is running, or turns red on failure; hover for details and click to retry.

To run the helper separately from this checkout:

```powershell
npm ci --prefix scripts
npm --prefix scripts run claude-renewal
```

The helper opens regular Chrome with a separate profile. Complete any verification
and sign in to Claude there on the first run. Playwright attaches only after the
Claude app opens; it does not run during verification or sign-in. The profile
remembers that session for future lookups. The helper checks the workspace
against Claude Code's account, reads the subscription date, and saves
`claude.renewal_date` in your config. Run it again
after each renewal; this is an on-demand lookup, with no extension or background
browser polling. Cancelled subscriptions leave the config unchanged.

The helper preserves existing settings, formatting and comments, and keeps the
first backup as `config.toml.before-renewal`. Unsupported TOML layouts are left
unchanged with an error. The browser profile is
stored in `claude-browser` beside the config. Use `-- --browser msedge` for Edge;
`-- --config <path> --profile <directory>` allows separate test settings and login.

While the helper is open, its dedicated browser exposes a debugging endpoint to
local processes on this computer. It closes immediately after reading the date.
Widget lookups have a timeout and stop their processes when the widget closes;
right-click the renewal text to cancel an active lookup. Progress and failures
appear beside the renewal text. Estimated usage always shows at least two decimal
places so it cannot round up to the next reported percentage.

Installation has its own two-minute timeout. Each browser lookup gets a separate
budget, including five minutes for sign-in and a fresh minute for the workspace
API. Date-only renewals use calendar-day labels, including “today”, without
claiming a billing time. Fractional usage reported by a service is kept as-is;
local estimates apply only to whole-percent readings.

## Credentials

Reuses the CLIs' stored credentials; never writes them.

| Service | Credential |
| --- | --- |
| Copilot | `gh auth token`, `GITHUB_TOKEN` or `GH_TOKEN` |
| Claude | `~/.claude/.credentials.json` (macOS: keychain) |
| Codex | `~/.codex/auth.json` |

Run `claude` or `codex` to refresh expired tokens. API endpoints are undocumented.

### Claude Code status line

For live Claude 5-hour and weekly usage, the widget needs your Claude Code status
line to save its input. Add these lines to the start of your status line script,
then use `$INPUT` wherever the script previously read stdin:

```bash
INPUT=$(cat)
case "$INPUT" in *'"five_hour"'*) printf '%s' "$INPUT" > ~/.claude/usage-widget-statusline.json;; esac
```

This updates usage as you use Claude Code, without extra requests. Without it,
the widget uses cached data and polls at most every 15 minutes, subject to rate limits.

Estimates only cover local logs; Claude needs a few percentage ticks to calibrate.
Optional Claude renewal cookies come from Chrome, Arc, Brave or Edge on macOS,
with a keychain access prompt on first use. Dollar rates: `src/providers.rs`.
