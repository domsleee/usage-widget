---
name: subscription-usage
description: Read Claude, Codex and GitHub Copilot subscription usage, remaining quota, reset times and per-model estimation token prices from usage-widget. Use when asked how much subscription usage is left, when a limit resets, or how the estimator prices tokens and cache usage.
---

# Subscription usage

Run `usage-widget usage`. It prints JSON using the widget's existing logins and config without opening a window. Requires a version of usage-widget with the `usage` command; from its checkout, use `cargo run --quiet -- usage` if needed.

For each service, report the plan, usage and reset times in the user's timezone. Meters with `unit: percent` contain the percentage **used**; remaining is `100 - used`. For `unit: dollars`, report `used / total`. Keep each window separate. A meter labelled `unlimited` has no percentage quota. Missing or zero totals do not imply 0% used.

Times are Unix seconds. `meters[].resets_at` is a quota reset; `cycle` separately identifies renewal, reset or cancellation. Relay `note` when readings are cached, old or rate limited. `fetched_at` is the command time, not necessarily when cached usage was measured. Report service errors individually and keep successful readings. Local estimates are disabled by this command.

For token prices by model, run `usage-widget prices` (or `cargo run --quiet -- prices` from the checkout). Rates are per million tokens and come directly from the estimator's pricing functions. Report normal input, cache reads, output, and Claude cache writes separately. Codex rates are in credits; Claude rates are relative cost weights for calibration, not actual subscription charges. Unlisted Codex models use `default_model`; Claude model names containing `haiku` or `sonnet` use that family, and all others use `default_family`. Effort does not select a different rate in this estimator.

When calculating estimated cost, Codex's input token count includes cache reads: charge `(input - cached)` at the normal input rate and `cached` at the cache-read rate, plus output. Claude's input count excludes cache: add normal input, cache writes, cache reads and output at their separate rates. Divide each weighted sum by one million.
