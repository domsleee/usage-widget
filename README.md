# usage-widget

A small always-on-top desktop widget (Rust, egui) that shows how much of your
GitHub Copilot, Claude and Codex allowance you have used: in dollars on business
plans, and as 5-hour and weekly windows on personal subscriptions. It refreshes
every five minutes and can be dragged anywhere on screen.

| Business plans | Personal subscriptions |
| --- | --- |
| <img width="250" alt="Copilot, Claude and Codex business plans, each showing dollars spent against its limit, with a total" src="docs/business-plans.png" /> | <img width="250" alt="Claude and Codex personal plans, each showing 5-hour and weekly usage, with the window that will run out first highlighted and its reset countdown" src="docs/personal-plans.png" /> |

Credit-to-dollar rates live at the top of `src/providers.rs`:
Copilot 50,000 credits = $500, Codex 12,500 credits = $500. Claude reports
dollars directly. When two or more services show dollars, the footer totals them.

## How it gets the numbers

No logins of its own. It reuses credentials the CLIs already store locally:

| Service | Credential | Endpoint |
| --- | --- | --- |
| Copilot | `gh auth token` (or `GITHUB_TOKEN` / `GH_TOKEN`) | `api.github.com/copilot_internal/user` premium request quota |
| Claude | `~/.claude/.credentials.json` written by Claude Code | `api.anthropic.com/api/oauth/usage` |
| Codex | `~/.codex/auth.json` written by Codex CLI | `chatgpt.com/backend-api/wham/usage` |

Only the services you have a paid plan with are shown. One you are not signed in
to on this machine, or are on a free plan with, is left out, so with only a Claude
subscription the widget shows just Claude.

What is shown depends on the plan:

- Business plans (Copilot Business/Enterprise, Claude Team/Enterprise, ChatGPT
  Business/Enterprise/Edu):
  - Copilot: premium request credits used / entitlement, in dollars.
  - Claude: 5-hour and 7-day windows when the plan has them, plus monthly spend against the cap when present.
  - Codex: 5-hour and weekly rate-limit windows when present, plus the workspace spend limit when present.
- Personal subscriptions (Claude Pro/Max, ChatGPT Plus/Pro/Go, Copilot Pro/Pro+): no
  dollar amounts. One line reads `63% 5h / 18% wk`, the 5-hour and weekly windows
  coloured by level. The window that will run out first in the current session is
  shown bright and the other faded; the bar tracks that window, and the countdown
  next to the service name, `(hh:mm)` or `(dd:hh:mm)`, is the time until it resets.
  When the other window becomes the one that runs out first, the bar and labels
  slide across to it. Copilot has no rolling windows, so a personal Copilot plan
  shows its monthly allowance as `% mo`.

  The two percentages are shares of differently sized allowances, so which runs
  out first depends on how much of the week one 5-hour point costs. Neither
  service publishes that, so the widget learns it from how both windows rise
  between refreshes (ignoring refreshes across a reset) and remembers it between
  runs. Until it has seen enough use, the fuller window is picked. A weekly window
  that resets before the 5-hour one cannot be the one that runs out first.

The plan comes from Claude Code's `subscriptionType`, and from the plan named in the
Copilot and Codex usage responses.

Tokens are read from disk on every refresh and never written back or refreshed by the
widget. If Claude Code or the Codex CLI has not run for a while its access token
expires and the widget says so; run `claude` or `codex` once and the next refresh
picks up the new token. The Copilot token comes from `gh auth token`, which does not
expire.

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
  showing the most-used window per service (on a personal plan, the one that will
  run out first). Maximize from the same menu restores it.
  Both views show each service's logo to the left of its name.
  The widget reopens in whichever mode it was left in.
- Size scales the widget from 25% to 200% on top of your display scaling (Ctrl +/-
  also works). Handy when a high-DPI monitor makes it too big. It is remembered.
- Set `USAGE_WIDGET_REFRESH_MINS` to change the refresh interval (default 5).
- Set `USAGE_WIDGET_OPACITY` (20-100) to change the window opacity (default 85).
