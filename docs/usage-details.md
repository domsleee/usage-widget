# Settings, renewals and estimates

## Config

`usage-widget config` creates and opens `config.toml` (in `$VISUAL`/`$EDITOR` if set):
`%APPDATA%\usage-widget\` on Windows, `~/Library/Application Support/usage-widget/` on
macOS. Restart the widget after editing.

```toml
refresh_mins = 5    # minutes between refreshes, minimum 1
opacity = 85        # window opacity, 20-100
precision = 0       # decimal places for percentages, 0-3

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

Date-only renewals use calendar-day labels, including “today”, without claiming
a billing time.

## Credentials

Reuses the CLIs' stored credentials; never writes them.

| Service | Credential |
| --- | --- |
| Copilot | `gh auth token`, `GITHUB_TOKEN` or `GH_TOKEN` |
| Claude | `~/.claude/.credentials.json` (macOS: keychain) |
| Codex | `~/.codex/auth.json` |

API endpoints are undocumented.

### Claude Code status line

For live Claude 5-hour and weekly usage, the widget needs your Claude Code status
line to save its input. Add these lines to the start of your status line script,
then use `$INPUT` wherever the script previously read stdin:

```bash
INPUT=$(cat)
case "$INPUT" in *'"five_hour"'*) printf '%s' "$INPUT" > ~/.claude/usage-widget-statusline.json;; esac
```

This updates usage as you use Claude Code, without extra requests. Without it,
the widget uses cached data and polls at most every 3 minutes, subject to rate limits.

Estimates only cover local logs; Claude needs a few percentage ticks to calibrate.
Estimated usage always shows at least two decimal places so it cannot round up to
the next reported percentage. Fractional usage reported by a service is kept as-is;
local estimates apply only to whole-percent readings.
With `estimate = true`, daily JSONL history retains readings, token counts,
calibration and predictions until manually removed. It excludes conversations and
credentials. See [estimate history](estimate-history.md) for paths, token
totals and replay rules.
Optional Claude renewal cookies come from Chrome, Arc, Brave or Edge on macOS,
with a keychain access prompt on first use. If a token expires, run `claude` or
`codex` once; the widget picks up the new token on its next refresh.
