# Codex app server fixtures

These fixtures describe the metadata protocol without requiring a daemon. Live
captures came from daemon version 0.144.4 on 2026-07-26. Schema fixtures came
from `codex app-server generate-json-schema`.

| Fixture | Source and request | Evidence |
| --- | --- | --- |
| `daemon_version.json` | LIVE CAPTURE 2026-07-26 from `codex app-server daemon version`. | Socket discovery is by `socketPath` and the availability gate is `status == "running"`, so the path is never hardcoded. |
| `initialize_response.json` | LIVE CAPTURE 2026-07-26, daemon 0.144.4. Params: `{"clientInfo":{"name":"agent-viewer","version":"0.1.0"}}`. | The initialize result shape and platform fields. |
| `thread_list_page_with_cursor.json` | LIVE CAPTURE 2026-07-26, daemon 0.144.4. Params: `{"limit":200,"cursor":null,"archived":false,"sourceKinds":["cli","exec"],"useStateDbOnly":true}`. The request asked `limit: 200` and the daemon returned 100 rows with a nonnull `nextCursor`. The fixture retains 6 of those 100 rows, with `nextCursor` verbatim. | A page shorter than the requested limit can still have a nonnull `nextCursor`. |
| `thread_list_short_page_with_cursor.json` | LIVE CAPTURE 2026-07-26, daemon 0.144.4. Params: `{"limit":3,"cursor":null,"archived":false,"sourceKinds":["cli","exec"],"useStateDbOnly":true}`. | A three row page has a nonnull `nextCursor`. |
| `thread_list_final_page.json` | LIVE CAPTURE 2026-07-26, daemon 0.144.4. This was a first page (`cursor: null`) narrowed by a `cwd` filter to a single project directory. Params: `{"limit":50,"cursor":null,"archived":false,"sourceKinds":["cli","exec","vscode"],"useStateDbOnly":true,"cwd":"/home/user/git/example/proj-alpha/.worktrees/change-004"}`. It returned 1 row and `nextCursor: null`. | The one row page with `nextCursor: null` is the only true end of list fixture. |
| `thread_list_archived_page.json` | LIVE CAPTURE 2026-07-26, daemon 0.144.4. Params include `{"limit":3,"cursor":null,"archived":true,"sourceKinds":["cli","exec"],"useStateDbOnly":true}`. | Archived enumeration uses the same thread shape. |
| `thread_list_default_source_kinds.json` | LIVE CAPTURE 2026-07-26, daemon 0.144.4. Params: `{"limit":3,"cursor":null,"archived":false,"useStateDbOnly":true}`. `sourceKinds` was omitted. | The default includes `vscode` rows and omits `exec` rows. |
| `thread_list_subagent_page.json` | LIVE CAPTURE 2026-07-26, daemon 0.144.4. Params include `{"limit":5,"cursor":null,"archived":false,"sourceKinds":["subAgent","subAgentReview","subAgentCompact","subAgentThreadSpawn","subAgentOther"],"useStateDbOnly":true}`. | `source` is a serialized enum such as `{"subAgent":"review"}`, not always a string. |
| `thread_with_extra_and_history_mode.json` | LIVE CAPTURE 2026-07-26, daemon 0.144.4. Lifted unchanged in structure from `thread_list_page_with_cursor.json`. | The undeclared `extra` key is present with a null value and `historyMode` is `legacy`. |
| `thread_loaded_list_empty.json` | LIVE CAPTURE 2026-07-26, daemon 0.144.4. Method `thread/loaded/list` with empty params. | The daemon hosted no threads on this box. |
| `status_changed_not_loaded.json` | LIVE CAPTURE 2026-07-26, daemon 0.144.4, framed with the generated notification method. | The only live status variant observed. All 581 enumerated threads reported `notLoaded`. |
| `status_changed_idle.json` | SCHEMA DERIVED from `codex app-server generate-json-schema`. | The `idle` status wire shape. |
| `status_changed_system_error.json` | SCHEMA DERIVED from `codex app-server generate-json-schema`. | The `systemError` status wire shape. |
| `status_changed_active_no_flags.json` | SCHEMA DERIVED from `codex app-server generate-json-schema`. | The `active` shape with no flags, which `data-model.md` maps to `Working` rather than `NeedsInput`. |
| `status_changed_active_waiting_on_approval.json` | SCHEMA DERIVED from `codex app-server generate-json-schema`. | The `active` shape with `waitingOnApproval`. |
| `status_changed_active_waiting_on_user_input.json` | SCHEMA DERIVED from `codex app-server generate-json-schema`. | The `active` shape with `waitingOnUserInput`. |
| `name_updated_notification.json` | SCHEMA DERIVED from `codex app-server generate-json-schema`. A live rename was intentionally not performed. | `threadId` is required and `threadName` can be null. |

The `idle`, `systemError`, and `active` variants
were not observable live because the daemon hosted zero threads on this box.
The live `thread/loaded/list` result was empty, and every one of 581
enumerated threads reported `notLoaded`. Those variants are schema
derived for that reason.

All identifying text and repository details are replaced by a deterministic
neutral mapping. Absolute paths retain their structural segments, including
`.worktrees`. Nulls, empty strings, key presence, key order, value
types, enum values, and array lengths are preserved. IDs, cursors, timestamps,
process IDs, token counts, and costs remain verbatim.
