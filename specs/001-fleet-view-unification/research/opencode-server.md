# opencode headless server: does it give us the same architecture as Claude and Codex?

Probe run 2026-07-26 against `opencode 1.17.20` on this box. All evidence below is literal
captured output from a throwaway server on port 47821 and a throwaway session created and
deleted by the probe. No pre-existing session, process, or server was touched.

Prior investigation (`duplication-and-opencode.md`) judged several capabilities impossible
based only on the SQLite schema. This probe re-tests those against the HTTP API. **Most of
them were wrong.** The server API closes nearly every gap.

---

## Summary

opencode has the same shape as Codex's app-server: a daemon that hosts sessions, plus a native
TUI that attaches to it as a client. Both halves are proven below.

The HTTP API is much larger than Codex's, and it exposes first-class endpoints for per-session
live status, pending permissions, pending questions, rename, archive, and an SSE event stream.
Four of the five gaps previously judged impossible are POSSIBLE. Only PR association is
genuinely absent.

The one architectural wrinkle that Codex does not have: **no server is running by default, and
there is no discovery mechanism on disk.** The plain `opencode` TUI runs its server in-process
with no listening socket, so there is nothing for agent-viewer to find unless the user has
explicitly run `opencode serve`.

---

## Q1. Does the server host sessions? YES - proven.

Method: create a session via `POST /session`, fire `POST /session/{id}/prompt_async` with a
short-lived `curl` that exits immediately, then poll `GET /session/status` from *new*
connections. At no point after the POST returns is any client connected.

The POST returns instantly, confirming it does not hold the turn open:

```
T0 2026-07-26T11:09:14,791762853+00:00

HTTP=204 time_total=0.011651
T1 (client exited) 2026-07-26T11:09:14,809998490+00:00
```

With zero clients attached, the server kept working - it hit a provider rate limit and drove
its own retry ladder:

```
--- immediately poll status from a NEW connection ---
poll1 11:09:14 -> null
poll2 11:09:16 -> {"type":"retry","attempt":1,"message":"Error from provider (Console): Provider rate limit exceeded","next":1785064157801}
poll3 11:09:18 -> {"type":"retry","attempt":2,"message":"Error from provider (Console): Provider rate limit exceeded","next":1785064161975}
poll5 11:09:22 -> {"type":"retry","attempt":3,"message":"Error from provider (Console): Provider rate limit exceeded","next":1785064170255}
poll8 11:09:28 -> {"type":"retry","attempt":3,"message":"Error from provider (Console): Provider rate limit exceeded","next":1785064170255}
```

And the turn then completed, still with no client connected. Message timestamps:

```
user      created=11:09:14 completed=n/a
assistant created=11:09:15 completed=11:09:31
```

```json
{"id":"msg_f9e1d67f90010A1jLCfraTHtxh","role":"assistant","modelID":"laguna-s-2.1-free",
 "time":{"created":1785064155129,"completed":1785064171493},"text":["HELLO-PROBE"]}
```

**Client disconnected at 11:09:14.81. Assistant message completed at 11:09:31 - 16 seconds
later, with nothing connected.** The work is server-hosted, exactly like Codex's app-server.

Fire-and-forget is a first-class API affordance, not an accident:

> `POST /session/{sessionID}/prompt_async` - "Create and send a new message to a session
> asynchronously, starting the session if needed and returning immediately."

---

## Q2. Does `opencode attach <url> -s <id>` rejoin in place, without duplicating? YES.

Driven headlessly through a pty (winsize set before spawn). Session lists compared before,
during, and after the attach, all via `GET /session?limit=500`:

```
BEFORE: count=250 probe_present=True
DURING: count=250 probe_present=True new_ids=[]
AFTER:  count=250 probe_present=True new_ids=[]
DUPLICATE_CREATED=False
```

The attached TUI rendered the *existing* conversation, not a blank session (ANSI stripped,
tail of the cumulative buffer):

```
  Build · Laguna S 2.1 Free · 16.7s┃    ┃  Reply with exactly: HELLO-PROBE
  ┃  Reply with exactly: SSE-PROBE
  ▣ Build · Laguna S 2.1 Free · 31.6s Free OpenCode Zen 18.9K (7%) HELLO-PROBE SSE-PROBE
  Build · Qwen3.7 Plus OpenCode Go   ~/git/theconnman/claude-settings/jobs/0214d552/tmp/ocprobe
  • OpenCode 1.17.20
```

Both prior turns (`HELLO-PROBE`, `SSE-PROBE`) and their responses replayed into the native TUI.
This is the Codex `codex resume --remote` equivalent: interaction plane stays the vendor's own
client, metadata plane is ours. `--fork` was not passed and must never be passed.

---

## Q3. What does the HTTP API expose?

The OpenAPI spec is served at **`GET /doc`** (`200 application/json`, 467 KB). `/openapi.json`,
`/swagger`, `/docs`, `/spec` all return `200 text/html` - that is the web-UI SPA fallback, not a
spec. `/doc` is the one.

The surface is ~190 operations. There are two parallel API families: a legacy/current one at the
root (`/session`, `/event`, `/permission`) and a newer one under `/api` (`/api/session`,
`/api/session/active`, `/api/permission/request`, `/api/session/{id}/event`). Both are live.
Fleet-view-relevant subset:

| Endpoint | Purpose |
|---|---|
| `GET /session` | List sessions. Params: `directory`, `workspace`, `scope=project`, `path`, `roots`, `start`, `search`, `limit` |
| `GET /experimental/session` | Same, plus **`archived` (true/false)** and `cursor` |
| `GET /api/session` | Newer list, cursor pagination, `order`, `project`, `subpath` |
| `GET /api/session/active` | Active sessions only |
| `GET /session/status` | **Map of sessionID -> SessionStatus** (`idle` / `busy` / `retry`) |
| `GET /session/{id}` | Single session |
| `PATCH /session/{id}` | **Update `title`, `metadata`, `permission`, `time.archived`** |
| `DELETE /session/{id}` | Delete |
| `POST /session` | Create |
| `GET /session/{id}/message` | Full message history |
| `POST /session/{id}/prompt_async` | Fire-and-forget prompt |
| `POST /session/{id}/abort` | Abort in-flight turn |
| `GET /session/{id}/children`, `/todo`, `/diff` | Subagents, todos, diff |
| `GET /permission` | **List pending permissions** |
| `POST /permission/{requestID}/reply` | Answer a pending permission |
| `GET /api/permission/request` | Newer pending-permission list |
| `GET /api/session/{id}/permission` | Per-session pending permissions |
| `GET /question` | **List pending questions** (distinct from permissions) |
| `POST /question/{requestID}/reply` / `/reject` | Answer or reject |
| `GET /event` | **SSE event stream** (`text/event-stream`) |
| `GET /api/session/{id}/event` | Per-session SSE stream |
| `GET /vcs`, `/vcs/status`, `/vcs/diff` | Branch, dirty files, diff |
| `GET /experimental/worktree` | Worktrees for a directory |
| `GET /api/model`, `GET /provider` | Model / provider lists |
| `GET /global/health`, `GET /api/health` | Health |

`SessionStatus` is a three-member union:

```json
{"anyOf":[
  {"properties":{"type":{"enum":["idle"]}}},
  {"properties":{"type":{"enum":["retry"]},"attempt":{"type":"integer"},
                 "message":{"type":"string"},"next":{"type":"integer"},
                 "action":{"reason","provider","title","message","label","link"}}},
  {"properties":{"type":{"enum":["busy"]}}}
]}
```

`Session.time` carries `archived` as a writable field:

```json
{"type":"object","properties":{
  "created":{"type":"integer"},"updated":{"type":"integer"},
  "compacting":{"type":"integer"},"archived":{"type":"number"}},
 "required":["created","updated"]}
```

`PATCH /session/{sessionID}` request body - the load-bearing one:

```json
{"type":"object","properties":{
  "title":{"type":"string"},
  "metadata":{"type":"object"},
  "permission":{"$ref":"#/components/schemas/PermissionRuleset"},
  "time":{"type":"object","properties":{"archived":{"type":"number"}}}},
 "additionalProperties":false}
```

---

## Q4. Does it close the gaps?

| Capability | Verdict | Evidence |
|---|---|---|
| Per-session live status | **POSSIBLE** | `GET /session/status` returns a per-ID map; observed `busy` and `retry{attempt,message,next}` for one specific session while others were absent (= idle). Also pushed live over SSE as `session.status`. |
| needs-input / pending permission | **POSSIBLE** | `GET /permission` and `GET /api/permission/request` exist and returned `[]` / `{"data":[]}` (nothing pending during the probe). `GET /question` likewise. SSE emits `EventPermissionAsked`, `EventPermissionV2Asked`, `EventQuestionAsked`, `EventQuestionV2Asked`. Reply endpoints exist. |
| Archive / hide | **POSSIBLE** | `PATCH {"time":{"archived":<epoch_ms>}}` persisted, and `GET /experimental/session?archived=true` returned 298 including our session while `archived=false` and the default returned 297 excluding it. |
| Rename | **POSSIBLE** | `PATCH {"title":"RENAMED-BY-PROBE"}` returned and read back the new title. No raw SQL needed. |
| PR association | **NOT POSSIBLE** | Zero matches for `pull_request` / `pullRequest` / any path containing `pull` in the 467 KB spec. Best available is `GET /vcs` -> `{"branch":"main","default_branch":"main"}` plus `GET /vcs/status`. agent-viewer would still resolve PRs itself from branch + directory, exactly as today. |

Supporting captures:

Rename, request and read-back:

```
=== RENAME: PATCH title ===
{"id":"ses_061e2ccabffesZ3Vg5SI74ORDf","title":"RENAMED-BY-PROBE","time":{"created":1785064141652,"updated":1785064181316}}
--- read back via GET /session/{id} ---
{"id":"ses_061e2ccabffesZ3Vg5SI74ORDf","title":"RENAMED-BY-PROBE","time":{"created":1785064141652,"updated":1785064181316}}
```

Archive, write then filter:

```
=== ARCHIVE: PATCH time.archived ===
{"id":"ses_061e2ccabffesZ3Vg5SI74ORDf","title":"RENAMED-BY-PROBE","time":{"created":1785064141652,"updated":1785064181316,"archived":1785064232000}}

=== /experimental/session?archived=true ===
{"n":298}
our_sid_in_archived_true=1
=== /experimental/session?archived=false ===
count=297 our_sid_present=0
=== default (no param) ===
count=297 our_sid_present=0
```

Two caveats on archive:

1. `GET /session` (the plain list) does **not** filter on archived - our archived session still
   appeared there (`present_in_list=1`). Only `/experimental/session` honours the flag. A
   fleet view either uses `/experimental/session` or filters `time.archived` client-side.
2. Unarchive is `archived: 0`, not `null` - the schema types it as `number`, and
   `{"time":{}}` is a no-op that leaves the previous value:

```
curl -X PATCH ... -d '{"time":{"archived":0}}'  ->  {"created":1785064141652,"updated":1785064181316,"archived":0}
curl -X PATCH ... -d '{"time":{}}'              ->  {"created":1785064141652,"updated":1785064181316,"archived":0}
```

The prior "0 of 297 rows archived" observation was correct as a fact about the data and wrong as
a conclusion about capability: nothing had ever written the column because no CLI does, but the
server writes it happily.

---

## Q5. Is there a real-time event stream? YES - SSE.

`GET /event` is declared `text/event-stream` returning the `Event` union. A 22-second capture
during a rename and a prompt produced:

```
      9 "type":"session.status"
      5 "type":"busy"
      4 "type":"session.updated"
      4 "type":"retry"
      3 "type":"message.updated"
      2 "type":"server.heartbeat"
      1 "type":"text"
      1 "type":"session.diff"
      1 "type":"server.connected"
      1 "type":"message.part.updated"
```

Literal frames - note the per-session status push, which is exactly what a fleet view needs:

```
data: {"id":"evt_f9e1f15f1001nxYl5Axx98Px3O","type":"server.connected","properties":{}}

data: {"id":"evt_f9e1f1dc3001AqL8yGJ3wHBWsu","type":"session.updated","properties":{"sessionID":"ses_061e2ccabffesZ3Vg5SI74ORDf","info":{"id":"ses_061e2ccabffesZ3Vg5SI74ORDf","slug":"mighty-otter",...

data: {"id":"evt_f9e1f21c7001Rt2y46852bmOo0","type":"session.status","properties":{"sessionID":"ses_061e2ccabffesZ3Vg5SI74ORDf","status":{"type":"busy"}}}

data: {"id":"evt_f9e1f2291001apZfGajQeJDb45","type":"session.status","properties":{"sessionID":"ses_061e2ccabffesZ3Vg5SI74ORDf","status":{"type":"retry","attempt":1,"message":"Error from provider (Console): Provider rate limit exceeded","next":1785064270432}}}

data: {"id":"evt_f9e1f2b06001u9RqLJeCWlDeP1","type":"session.status","properties":{"sessionID":"ses_061e2ccabffesZ3Vg5SI74ORDf","status":{"type":"retry","attempt":2,"message":"Error from provider (Console): Provider rate limit exceeded","next":1785064274598}}}
```

`server.heartbeat` fires periodically, so a dropped stream is detectable.

The declared event catalogue is large. Fleet-relevant members, quoted from the spec's schema keys:

```
EventSessionCreated  EventSessionDeleted  EventSessionIdle  EventSessionError
EventSessionCompacted  EventSessionDiff
EventPermissionAsked  EventPermissionReplied  EventPermissionV2Asked  EventPermissionV2Replied
EventQuestionAsked  EventQuestionReplied  EventQuestionRejected
EventQuestionV2Asked  EventQuestionV2Replied  EventQuestionV2Rejected
EventMessageUpdated  EventMessageRemoved  EventMessagePartUpdated  EventMessagePartDelta
EventProjectUpdated  EventProjectDirectoriesUpdated
EventServerConnected  EventServerInstanceDisposed  EventGlobalDisposed
EventSessionNextPrompted  EventSessionNextStepStarted  EventSessionNextStepEnded
EventSessionNextStepFailed  EventSessionNextToolCalled  EventSessionNextToolFailed
EventSessionNextRetried  EventSessionNextMoved  EventSessionNextAgentSwitched
EventSessionNextModelSwitched  EventSessionNextCompactionStarted/Delta/Ended
```

There is also `GET /api/session/{id}/event` for a single-session stream, and
`GET /global/event` for cross-instance events.

---

## Q6. Who owns the server lifecycle?

**Nothing was running before the probe.** `ps aux | grep -i opencode` returned empty at start.

**`opencode serve` is strictly manual.** The plain `opencode` TUI does not start a listening
server and does not reuse one. Driven in a pty for 14 seconds in the same directory as my
running server:

```
=== LISTENERS BEFORE plain TUI ===
LISTEN 0  512  127.0.0.1:47821  0.0.0.0:*  users:(("opencode",pid=213479,fd=17))
=== LISTENERS DURING plain TUI (tui pid 239559) ===
LISTEN 0  512  127.0.0.1:47821  0.0.0.0:*  users:(("opencode",pid=213479,fd=17))
=== child processes of tui ===
(none)
=== LISTENERS AFTER ===
LISTEN 0  512  127.0.0.1:47821  0.0.0.0:*  users:(("opencode",pid=213479,fd=17))
```

No new TCP listener, no unix listener, and no child process. The TUI embeds its server
in-process with no socket at all. A second run confirmed the TUI holds no listening socket.

**There is no on-disk discovery mechanism.** Everything opencode wrote during the probe window,
excluding my own probe dir:

```
/home/theconnman/.cache/opencode/models.json
/home/theconnman/.local/share/opencode/opencode.db{,-wal,-shm}
/home/theconnman/.local/share/opencode/snapshot/global/...
/home/theconnman/.local/share/opencode/log/opencode.log
```

No pidfile, no port file, no socket path, no server registry. `opencode debug paths` /
`debug info` exist but describe data/config/cache/state dirs, not running servers.

Discovery options, in descending order of reliability:

1. **User-configured port.** agent-viewer asks for a URL (this is what `opencode attach <url>`
   itself does - the url is a required positional).
2. **mDNS.** `opencode serve --mdns` advertises under `opencode.local` (`--mdns-domain`
   configurable). Opt-in on the server side, so not reliable for arbitrary users.
3. **Process/socket scan.** Find an `opencode` process with a listening TCP socket and probe
   `GET /global/health`. Workable, unverified in this probe.

Also note the server is **unsecured by default**:

```
Warning: OPENCODE_SERVER_PASSWORD is not set; server is unsecured.
```

`opencode attach` has `-p/--password` and `-u/--username` for basic auth, so a hardened server
needs credentials that agent-viewer would have to carry too.

---

## Recommendation

**Bind opencode's metadata plane to the server API, but only opportunistically - keep the
CLI + read-only SQLite path as the fallback, and never start a server.**

What binding buys:

- Per-session live status (`busy` / `retry` / `idle`), replacing
  `crates/agent-viewer-core/src/opencode.rs:264-270`, which today flips every session to
  "Working" whenever any `opencode*` process exists. This is the single biggest correctness
  win and it is currently just wrong.
- needs-input parity via `GET /permission` + `GET /question`, plus push notification of it via
  `EventPermissionAsked` / `EventQuestionAsked`. This is what makes opencode rows actionable in
  a fleet view instead of decorative.
- Archive and rename as supported mutations rather than raw SQL, satisfying the
  "mutations never write the DB" invariant that a `opencode db` rename would have violated.
- SSE removes polling entirely for the bound path.

What it risks:

- **The server usually is not running.** This is the real cost and it is qualitatively different
  from Codex. Codex's app-server and Claude's `--bg` daemon are part of how those tools already
  run; opencode's is not. Binding means either (a) most users see the degraded SQLite path
  anyway, or (b) agent-viewer tells users to run `opencode serve`, which changes their workflow.
  Starting one silently is ruled out by the brief and would be wrong regardless - the TUI does
  not use one, so a spawned server would host a *disjoint* set of sessions from whatever the
  user is actually running.
- **Discovery is unsolved.** No pidfile, no well-known port. Config or a process scan.
- **API churn.** Two overlapping families (`/session` vs `/api/session`), a large
  `experimental/` prefix, and `archived` filtering living only on `/experimental/session`.
  `v2`/`Next` event names throughout suggest an in-flight migration. This is a moving target in
  a way `codex app-server`'s narrower surface is not.
- **Auth.** Unsecured by default, basic auth when hardened; another config surface.

**Proportionality judgement: partially worth it, and the split is clean.**

A full binding - SSE, permissions, archive, rename, message history - is *not* proportionate for
the third and least-used backend, given the server is usually absent so the work is dark most of
the time, and the API is visibly mid-migration.

What *is* proportionate is the narrow slice that fixes a live defect: an optional
`opencode.server_url` config, and when it is set, use `GET /session` + `GET /session/status` for
listing and status, and `PATCH /session/{id}` for rename and archive. That is four endpoints, no
SSE, no streaming, no auth beyond an optional password, and it converts opencode from
"every row says Working" to correct per-session status plus two mutations that were previously
impossible. When the URL is unset, behaviour is exactly what ships today.

The important structural finding regardless of scope: **opencode does fit the settled two-plane
architecture.** `opencode attach <url> -s <id>` is a proven in-place rejoin with no duplicate,
so the interaction plane stays the vendor's own TUI, and the server can carry the metadata
plane. The architecture is not the blocker; the missing daemon is.

---

## What I could not determine

- **Live pending-permission payload shape.** `GET /permission` and `GET /api/permission/request`
  returned empty (`[]`, `{"data":[]}`) because the probe turn triggered no tool that requires
  approval. The endpoints and the `EventPermissionAsked` / `EventQuestionAsked` events exist and
  are typed in the spec, but I never captured a populated response. The verdict is POSSIBLE on
  endpoint existence, not on a confirmed live payload.
- **Whether a server started by `opencode serve` and a plain `opencode` TUI in the same directory
  see the same session set.** They share `~/.local/share/opencode/opencode.db`, so they almost
  certainly do, but I did not verify that a TUI-created session appears in the server's
  `GET /session` without a restart.
- **Whether a process/socket scan is a reliable discovery mechanism.** I confirmed no on-disk
  registry exists and that the serve process holds an identifiable listening socket, but I did
  not build or test the scan.
- **mDNS discovery.** `--mdns` was never exercised; I ran on an explicit fixed port.
- **Basic-auth behaviour.** `OPENCODE_SERVER_PASSWORD` was unset and the server ran unsecured.
- **`/sync/*` and `/experimental/workspace/*`.** Endpoints named `sync/steal`,
  `sync/start`, `workspace/warp`, `experimental/control-plane/move-session` suggest
  multi-machine session migration. Not investigated; possibly relevant to a future fleet view.
- **Why the second `prompt_async` (model `opencode/grok-code`) produced no assistant message**
  after an `abort`. Irrelevant to the questions asked, but the abort-then-reprompt path left a
  dangling user message.

---

## Cleanup performed

- Probe session `ses_061e2ccabffesZ3Vg5SI74ORDf` deleted:
  `delete_http=200`, then `remaining_matches=0 total=249`.
- Probe server (pid 213479, port 47821) killed. `ps aux | grep -i opencode` returns empty and
  nothing listens on 47821.
- No pre-existing opencode session, process, or server was read-modified, signalled, or touched.
  All writes went to the one session the probe created. The opencode DB was never written
  directly.
