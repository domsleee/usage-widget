# Estimate history

With `estimate = true`, each provider saves refresh inputs and predictions for
future backtesting. There is no backtest command or estimator tuning yet.
History lives separately from the estimator cache:

- Windows: `%LOCALAPPDATA%\usage-widget\history\`
- macOS: `~/Library/Application Support/usage-widget/history/`
- Linux: `$XDG_DATA_HOME/usage-widget/history/`, defaulting under `~/.local/share/`

Daily UTC files such as `codex-2026-09-12.jsonl` are append-only and kept until
manually removed. Disabling `estimate` stops recording but keeps history.
Recording adds no network requests and excludes conversations, prompts, tool
results, credentials and project paths.

Existing scan cursors are kept; previously consumed events are not backfilled.
Checkpoints preserve calibration, but cannot reconstruct past readings or when
older log entries first became available.

## Record format (schema 1)

Each JSON line contains `schema_version`, `estimator_version` (`cumulative-v1`),
`app_version`, `provider`, `received_at` and `batch`. `received_at` is RFC 3339 UTC,
taken after scanning and before evaluation—even for older events discovered now.

`batch.before` holds the starting calibration: window, plan, percentage range,
cumulative costs and previous-window rates, without scanner paths or dedup caches.

| Batch field | Codex | Claude |
| --- | --- | --- |
| Service readings | `pct`, `window`, `plan` (weekly) | `readings`: `[label, pct, window]` tuples, plus `plan` (5h/week) |
| Reading source | `source: "usage_api"` | `source: "statusline"` or `"usage_snapshot"` (the latter may be cached) |
| Reading time (Unix seconds) | `observed_at`: API receipt time; `source_at: null` (not supplied) | `source_at`: selected reading's timestamp |
| Predictions | `estimate`: predicted percentage | `estimates`: predicted percentages in reading order |

A null prediction means use the service reading, e.g. during calibration, at the
limit or for an already-fractional reading. Readings retain numeric precision;
predictions precede UI decimal formatting.

`events` retains consumed local inputs in processing order:

- Codex: `ts` (original timestamp), `session`, `model`, `tokens` (deltas), `totals`
  (running counters), `credits`, `pct`, `window`, `plan`. Session, timestamp and
  counters identify repeats. Missing weekly readings are null; the current
  estimator ignores those events, but history retains them.
- Claude: `source_at` (original timestamp), `ts` (Unix seconds used for evaluation),
  `response_id`, `model`, `tokens`, `cost`. Only the first occurrence of each
  response ID is counted, matching the existing streaming deduplication.

| Token arrays | Field order | Total tokens per event |
| --- | --- | --- |
| Codex `tokens` / `totals` | input including cache, cached input, output | `tokens[0] + tokens[2]` |
| Claude `tokens` | input excluding cache, cache creation, cache read, output | Sum of `tokens` |

Sum event deltas after deduplication, not Codex running totals. Counts cover the
local logs consumed by the widget, not all account usage. Retaining counts and
models alongside costs allows later changes to pricing or model weights.

## Replay and failure handling

Replay in recorded refresh order. Globally sorting event timestamps would leak
late-arriving data into earlier predictions. For Claude, preserve the event split
at `source_at`. Repeated cached readings are not independent calibration samples.

To reproduce the baseline, restore `before`, apply events and readings, and compare
predictions; both estimators test this through JSON round trips. Candidate
algorithms must carry their own state between batches. Bump `estimator_version`
when calibration, pricing or event processing changes, and `schema_version` for
incompatible format changes.

History is flushed before saving scan cursors. Write failures show an
`estimate history:` note and the service reading, leaving inputs retryable.
A crash before cursor saving can repeat inputs; use IDs and checkpoints to detect
retries or gaps. File locks prevent interleaved writes. Interrupted lines are
preserved and newline-separated from later records; readers should report and
skip malformed lines as gaps.

Whole percentages cannot validate exact hidden decimals. Future metrics include
the next observed percentage transition, calibration stability and time at `.99`.
Score fractional service readings, stale snapshots and estimates separately.
