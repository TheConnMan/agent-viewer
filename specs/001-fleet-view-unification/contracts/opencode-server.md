# Contract: opencode server (optional, opportunistic)

Bound only when `opencode.server_url` is configured and reachable (D-013). Unset or unreachable
means the backend behaves exactly as today: CLI plus read-only SQLite.

## Lifecycle

- agent-viewer **MUST NOT** start a server. `opencode serve` is strictly manual, and starting one
  is a user-visible side effect.
- agent-viewer **MUST NOT** guess a port. There is no pidfile, port file or registry; the plain
  `opencode` TUI opens no listening socket at all (it embeds its server in-process).
- Reachability MUST be probed, and failure MUST degrade silently to the no-server tier rather
  than erroring. Capabilities are recomputed from the live tier.
- The server is unsecured by default (`OPENCODE_SERVER_PASSWORD` unset). Basic auth credentials
  are supported by `opencode attach` via `-p`/`-u` and MUST be passed through when configured.

## Endpoints used

Deliberately four, not the ~190 the spec declares. The API is visibly mid-migration - parallel
`/session` and `/api/session` families, `experimental/` paths, `V2`/`Next` event names - so the
binding stays narrow.

| Call | Use |
|---|---|
| `GET /session` | enumeration |
| `GET /session/status` | per-session `idle` / `busy` / `retry{attempt,message,next}` |
| `PATCH /session/{id}` with `{"title":"..."}` | rename |
| `PATCH /session/{id}` with `{"time":{"archived":<ms>}}` | archive |

Two encoded caveats:

1. **`GET /session` does not filter archived sessions.** Only `/experimental/session` does
   (`?archived=true` returned 298; `false` or default returned 297). Archived filtering must
   therefore be applied by the caller on the stable endpoint.
2. **Unarchive is `archived: 0`.** Sending `{"time":{}}` is a silent no-op.

The OpenAPI document is at `GET /doc` (467 KB). `/openapi.json`, `/swagger` and `/docs` are SPA
fallbacks, not the spec.

## Attach

```
opencode attach <url> -s <session_id>
```

Native client, no renderer on our side (D-001). Verified: session count was 250 before, during
and after, with `new_ids=[]`, and the TUI replayed existing conversation history.

`--fork` MUST never be passed.

## Events

SSE at `GET /event`. Consumed: `session.status` (per-session payloads), `session.updated`,
`message.updated`. `server.heartbeat` is the liveness signal. The declared catalogue also
includes `EventPermissionAsked`, `EventQuestionAsked` and `EventSessionIdle`.

As everywhere else, the 2s poll backstop (FR-009) is what guarantees the bound; SSE is an
optimization (D-007).

## Not available

PR association. There are zero `pull` matches in the API spec; `GET /vcs` returns only
`{"branch":"main","default_branch":"main"}`. `pr_refs` MUST be advertised unsupported on this
backend in both tiers.
