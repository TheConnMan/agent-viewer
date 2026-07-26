# Claude agent fixtures

All captures came from Claude Code 2.1.220 on 2026-07-26.

| Fixture | Source | Evidence |
| --- | --- | --- |
| `agents_all.json` | LIVE CAPTURE from `claude agents --json --all`. | Thirty two rows: thirty one background rows and one real interactive row. |
| `agents_background_only.json` | LIVE CAPTURE from `claude agents --json --all`, taken while no interactive session was alive. | The expected post filter result contains thirty one background rows. |
| `agents_interactive_row.json` | LIVE CAPTURE, lifted from `agents_all.json`. | The interactive row shape without fabricated keys. |

Per the 2026-07-26 clarification and `research.md` D-012,
interactive rows are excluded from the unified list to match Fleet View.
Therefore `agents_all.json` tests exclusion, not rendering.

The observed interactive key set is
`[cwd, kind, name, pid, sessionId, startedAt]`. It has no
`id` key and no `state` key. The base background key set
is `[cwd, id, kind, name, sessionId, startedAt, state]`, with
`pid` and `status` present on some rows. Observed
`state` values are `blocked`, `done`,
`failed`, `stopped`, and `working`.

Names and project paths use one deterministic neutral mapping across both
captures. IDs, session IDs, timestamps, process IDs, key presence, value types,
and row order remain verbatim.
