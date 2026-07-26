# opencode server fixtures

Live captures came from opencode 1.17.20 on 2026-07-26. The schema fixture came
from the server OpenAPI document at `GET /doc`.

| Fixture | Source and endpoint | Evidence |
| --- | --- | --- |
| `session_list.json` | LIVE CAPTURE 2026-07-26 from `GET /session`. | The stable session list shape with eight rows. |
| `session_list_with_archived_row.json` | SCHEMA DERIVED from opencode 1.17.20 `GET /doc`, built from `session_list.json` because no live row carried `time.archived`. | The positive case for caller side archived filtering on the stable `GET /session` endpoint. |
| `session_status_empty.json` | LIVE CAPTURE 2026-07-26 from `GET /session/status`. | No session was active, so the live status map was empty. |
| `session_status_idle_busy_retry.json` | SCHEMA DERIVED from opencode 1.17.20 `GET /doc`. | All three `SessionStatus` variants, including every required retry and action field. |
| `experimental_session_archived_true.json` | LIVE CAPTURE 2026-07-26 from `GET /experimental/session?archived=true`, trimmed to six representative rows after the full response was measured. | The archived query returned the same content as the false query. |
| `experimental_session_archived_false.json` | LIVE CAPTURE 2026-07-26 from `GET /experimental/session?archived=false`, trimmed to six representative rows after the full response was measured. | This file is byte identical to the true fixture. |
| `vcs.json` | LIVE CAPTURE 2026-07-26 from `GET /vcs`. | The endpoint exposes branch names only and no pull request association surface. |

The archived measurement contradicts
`contracts/opencode-server.md`, which claims 298 rows for
`archived=true` and 297 rows for `archived=false`. On
opencode 1.17.20, both full responses contained 297 rows, they were byte
identical, and zero rows carried `time.archived`. The two trimmed
fixtures remain byte identical after redaction so that behavior is directly
testable. Zero of the 297 live rows carried `time.archived`, so the positive
archived row fixture is schema derived rather than captured.

Titles and project paths use a deterministic neutral mapping. IDs, event
identifiers, timestamps, token counts, costs, key presence, key order, value
types, nulls, and row order remain verbatim.
