# usage-widget

A small always-on-top desktop widget (Rust, egui) that shows how much of your
GitHub Copilot, Claude and Codex allowance you have used, in dollars. It
refreshes every five minutes and can be dragged anywhere on screen.

<img width="323" height="237" alt="image" src="https://github.com/user-attachments/assets/29bc8928-0440-46c9-98d3-0cd0231311bb" />


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

Tokens are read from disk on every refresh and never written back by the widget. If
Claude Code has not run for a while its access token expires; the widget then runs a
one-line headless prompt (`claude -p` on Haiku, at most once every 15 minutes) so
Claude Code refreshes the token itself, and retries. An expired Codex token is only
reported; running `codex` once refreshes it.

Logos are from [Simple Icons](https://simpleicons.org) (CC0); the marks belong to their owners.

These endpoints are the same ones the CLIs and web settings pages use. They are not
formally documented and may change.

## Install

Needs a Rust toolchain (https://rustup.rs). Windows only.

```
cargo install --git https://github.com/pepsi-enjoyer/usage-widget
usage-widget --startup
usage-widget
```

`--startup` registers the exe in your per-user Run key so it launches at login;
`--no-startup` removes it. The widget stays out of the taskbar and Alt-Tab, so quit
it from its right-click menu.

To upgrade, quit the widget first (Windows will not let a running exe be replaced),
then run the `cargo install` line again and start `usage-widget`. There is no need to
re-run `--startup`. Only one copy runs at a time, so launching it while it is already
running does nothing.

From a clone, `install.ps1` does the same three steps using the local checkout.

## Using it

- Drag anywhere on the widget to move it. Position is remembered between runs, and
  if the saved spot is off screen (say, a monitor is gone) it is moved back on.
- Right-click for refresh, minimize, size, links to each service's usage page, and Quit.
- Minimize shrinks it to one line (`COP 87%  CLD 83%  CDX 99%` with each service's logo),
  showing the most-used window per service. Maximize from the same menu restores it.
  The widget reopens in whichever mode it was left in.
- Size scales the widget from 50% to 150% on top of your display scaling (Ctrl +/-
  also works). Handy when a high-DPI monitor makes it too big. It is remembered.
- Set `USAGE_WIDGET_REFRESH_MINS` to change the refresh interval (default 5).
- Set `USAGE_WIDGET_OPACITY` (20-100) to change the window opacity (default 85).
