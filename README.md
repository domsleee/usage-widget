# usage-widget

A small always-on-top desktop widget (Rust, egui) that shows how much of your
GitHub Copilot, Claude and Codex allowance you have used, in dollars. It
refreshes every five minutes and can be dragged anywhere on screen.

<img width="259" height="154" alt="image" src="https://github.com/user-attachments/assets/ac0edb40-1ca3-4d8f-a7e0-10053e63bff0" />


Credit-to-dollar rates live at the top of `src/providers.rs`:
Copilot 50,000 credits = $500, Codex 12,500 credits = $500. Claude reports
dollars directly. The footer total sums all three.

## How it gets the numbers

No logins of its own. It reuses credentials the CLIs already store locally:

| Service | Credential | Endpoint |
| --- | --- | --- |
| Copilot | `gh auth token` (or `GITHUB_TOKEN` / `GH_TOKEN`) | `api.github.com/copilot_internal/user` premium request quota |
| Claude | `~/.claude/.credentials.json` written by Claude Code | `api.anthropic.com/api/oauth/usage` |
| Codex | `~/.codex/auth.json` written by Codex CLI | `chatgpt.com/backend-api/wham/usage` |

What is shown depends on the plan:

- Copilot: premium request credits used / entitlement.
- Claude: 5-hour and 7-day windows when the plan has them, plus monthly spend against the cap when present.
- Codex: 5-hour and weekly rate-limit windows when present, plus the workspace spend limit when present.

Tokens are read from disk on every refresh and never written back. If Claude Code or
Codex have not run for a while their access token expires and the widget says so.
Running `claude` or `codex` once refreshes it.

These endpoints are the same ones the CLIs and web settings pages use. They are not
formally documented and may change.

## Build and run

```
cargo build --release
target\release\usage-widget.exe
```

`install.ps1` builds the release binary and drops a shortcut into your Startup folder
so the widget appears at login.

## Using it

- Drag anywhere on the widget to move it. Position is remembered between runs.
- Right-click for refresh, links to each service's usage page, and Quit.
- Click a service name to open its usage page.
- Set `USAGE_WIDGET_REFRESH_MINS` to change the refresh interval (default 5).
- Set `USAGE_WIDGET_OPACITY` (20-100) to change the window opacity (default 85).
