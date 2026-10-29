---
name: usage-widget
description: Read Claude, Codex and GitHub Copilot usage, remaining quota and reset times from usage-widget. Use when asked how much subscription usage is left or when a limit resets.
---

# Usage

Run `usage-widget usage`. It prints JSON using the widget's existing logins and config without opening a window. From a checkout, `cargo run --quiet -- usage` works too.

For each service under `providers`, report usage and reset times in the user's timezone. A meter with `unit: percent` holds the percentage **used**; remaining is `100 - used`. For `unit: dollars`, report `used / total`. Keep each window (`label`, such as `5h` or `wk`) separate. `personal` is true for personal subscriptions such as Claude Max or ChatGPT Plus.

Times are Unix seconds; `resets_at` is when that window resets. A service with `error` has no reading: `Not Available` means it is not set up on this machine, and `detail` has the full message. Services disabled in the config are left out, and `config_error` explains a config that could not be read. Report the others normally.
