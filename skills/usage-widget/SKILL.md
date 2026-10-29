---
name: usage-widget
description: Read Claude, Codex and GitHub Copilot plans, usage, remaining quota, reset times and renewal dates from the usage-widget CLI. Use when asked how much usage is left, when a limit resets, or when a plan renews.
---

# Usage widget

Run `usage-widget usage`. It prints JSON using the widget's existing logins and config without opening a window. Requires a version of usage-widget with the `usage` command; from its checkout, use `cargo run --quiet -- usage` if needed.

For each service, report the plan, usage and reset times in the user's timezone. Meters with `unit: percent` contain the percentage **used**; remaining is `100 - used`. For `unit: dollars`, report `used / total`. Keep each window separate. A meter labelled `unlimited` has no percentage quota. Missing or zero totals do not imply 0% used.

Times are Unix seconds. `meters[].resets_at` is a quota reset; `cycle` separately identifies renewal, reset or cancellation. Relay `note` when readings are cached, old or rate limited. `fetched_at` is the command time, not necessarily when cached usage was measured. Report service errors individually and keep successful readings.
