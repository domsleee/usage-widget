# Settings

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

[codex]
enabled = true
```
