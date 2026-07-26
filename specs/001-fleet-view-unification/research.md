# Phase 0 Research: Fleet View Unification

**Feature**: 001-fleet-view-unification | **Date**: 2026-07-26

Consolidates three parallel investigations plus direct verification by the lead. Full
evidence, with quoted protocol captures and cited line numbers, lives in `research/`:

- `research/claude-daemon.md` - Claude daemon socket protocol, attach and rename verdicts
- `research/codex-app-server.md` - Codex app-server transport, enumeration, attach, notifications
- `research/duplication-and-opencode.md` - remaining duplication paths, opencode capability inventory

Everything below was verified live on this box against real running processes. Claims that
remain inference are labelled as such.

---

## D-001: Split the planes - native CLI for interaction, daemon for metadata

**Decision**: Separate the **interaction plane** from the **metadata plane**, and bind them at
different depths.

- **Interaction plane: always the native CLI, on every backend.** Attach spawns the backend's
  own client in a PTY. agent-viewer never renders a session's contents itself.
- **Metadata plane: bind as deep as each backend usefully allows.** Codex uses the app-server
  for enumeration, rename and notifications; Claude uses `claude agents --json --all`;
  opencode is under evaluation (see `research/opencode-server.md`).

All three backends turn out to share one topology: **a headless host process owns the session,
and the vendor's own client attaches to it.** That is `claude --bg` plus `claude attach`, the
app-server plus `codex resume --remote unix://`, and `opencode serve` plus
`opencode attach <url>`. agent-viewer is a third-party client of each host, never a host itself
and never a renderer.

**Rationale**: This is a product constraint, stated directly: the deliverable is the unified
view, and going *into* a session must use the real CLI natively. Reimplementing any agent's
conversation UI is out of scope regardless of feasibility.

It also happens to be the lower-risk engineering choice. The metadata plane can bind to
experimental and undocumented interfaces cheaply, because a failure there degrades to a stale
or unfiltered list. The interaction plane cannot, because a failure there breaks the user's
actual working session. Keeping interaction on the vendor's own supported entry point means
protocol churn in a CLI that ships several times a week can never break attach.

**Alternatives considered**:

- *Pump the Claude daemon byte pipe ourselves instead of spawning `claude attach`*: rejected -
  see D-002. Technically proven and not a "renderer", but it trades a supported entry point for
  an undocumented one to save one process.
- *Render Codex conversation events in our own TUI via `thread/resume`*: rejected outright. This
  is the one option that genuinely is reimplementing the client (see D-004).
- *Uniform "stay on CLI everywhere", metadata included*: rejected. Leaves Principle II violated
  on Codex, where rename stays fake despite a real API existing, and forfeits the server-side
  subagent filtering that solves the volume problem (D-005).

---

## D-002: Claude attach stays on the native `claude attach` CLI

**Decision**: Attach to Claude sessions by spawning `claude attach <short_id>` in a PTY. Do
**not** bind the daemon control socket for attach, despite it being proven to work.

**Rationale**: Per D-001, the interaction plane stays on the vendor's supported entry point.
`claude attach` is already proven correct and non-duplicating: verified empirically that it
left agents, roster and jobs counts unchanged, and commit `8452e94` fixed the historical fork.
Binding the socket would buy one fewer process at the cost of depending on an undocumented
protocol for the one path whose failure breaks the user's live session.

The remaining gap on this path is D-003 (interactive rows carry no `id`), which is a defect in
how rows are classified, not a deficiency of `claude attach` itself.

### Rejected alternative, documented because it was proven and may matter later

Direct `control.sock` attach **works**, and the evidence is recorded here so the option can be
revisited if `claude attach` ever regresses. Verified live from a Python process (not `claude`)
against a real running session. The request shape is:

```json
{"proto":1,"op":"attach","short":"<8hex>","auth":"<control.key>",
 "cols":N,"rows":N,"attachId":"...","caps":{...}}
```

The daemon replies with one JSON line and the socket then becomes a raw byte pipe. Captured:

```json
{"ok":true,"op":"attach","decModes":[1000,1002,1003,1006,2004,1004,2031],
 "via":"spare","tempo":"idle","state":"working","cached":false,"stale":false}
```

followed by 9,777 bytes of real ANSI. Multiple attachers coexist on Linux (the kick loop is
Windows-only), so this does not displace a human already sitting in `claude attach`. Resize is
a separate short connection carrying `op:"resize"` and the same `attachId`.

The reply hands back `decModes`, `state`, `tempo`, `stale` and `cached` for free. Those fields
remain interesting for the **metadata** plane even under this decision, since reading them does
not require attaching - but they are not needed for FR-009, which `claude agents --json --all`
already satisfies. Treated as a possible later optimization, not part of this feature.

Risk is bounded by a real version gate: `proto` is validated and a mismatch returns
`{"code":"EPROTO","serverProto":1,"serverVersion":"2.1.220"}` - loud, not silent. That plus the
`claude attach` fallback is what makes binding to an undocumented socket tolerable against a
CLI that ships several times a week.

**Alternatives considered**:

- *Direct `ptySock` framing* (5-byte header: uint32BE length + uint8 kind; 0=DATA, 1=JSON
  ctrl): fully decoded and verified, but rejected twice over - it violates D-001 and, unlike
  `control.sock`, has no version negotiation and an unauthenticated `kill` verb.

---

## D-003: The duplication bug is real, but not where it was thought to be

**Decision**: Treat Claude duplication as an **open defect scoped to interactive rows**, not as
a resolved issue and not as a general attach defect.

**Rationale**: Commit `8452e94` (2026-07-13) fixed the original fork. The old path drove
`claude agents` with `CLAUDE_AGENTS_SELECT` plus a PTY auto-Enter driver; when auto-Enter
missed the target row it landed on the composer and spawned a genuinely new session. That code
is gone - `grep -rn CLAUDE_AGENTS_SELECT crates/*/src/` finds only a `#[cfg(test)]` test and
two comments. Verified empirically: `claude attach <short_id>` left agents, roster and jobs
counts unchanged.

**But the fix only covers rows that carry an `id`.** `claude agents --json` self-documents as
printing "active sessions (interactive **and** background)". Interactive rows carry no `id`
key, and `claude.rs:355` collapses that into `""` rather than `None`:

```rust
let short_id = crate::json_str(&entry, "id").unwrap_or_default().to_string();
```

so `attach_command` falls through to the `-r` branch at `claude.rs:214-219`. Pressing Enter on
an interactive row launches a **second interactive Claude Code client**. The comment above that
branch calls this "a rare jobs entry missing its `id` key"; it is in fact every interactive row.
Mitigating: `--fork-session` is documented as required to mint a new session id and the viewer
never passes it, so this is a second client on one thread rather than a true fork. Aggravating:
`-r` degrades to an interactive picker when the id does not resolve in the pinned cwd.

This is precisely the "use it alongside real Claude Agents sessions" case named as core to the
product, so it is in scope.

**Two adjacent defects found in the same audit**:

1. `mutations.rs:50-52` runs `terminate(pid, "claude")` **before** `claude.rs:194-197` declines
   remove as unsupported. An interactive row carries the user's own live Claude pid, so stage
   two of Ctrl+X SIGTERMs that process group. Only the comm-prefix guard at `spawn.rs:120`
   prevents it. Must be fixed regardless of the rebuild decision.
2. `pending_reply.rs:173-179` could reproduce the fork bug by another route on a reused PTY
   over an id-less row. Moot given peek/reply is being removed, recorded for completeness.

**Ruled out with quoted evidence**: codex `resume` (no fork flag exists in its help at all),
opencode `-s` (`--fork` exists in the CLI and is never passed), and every refresh/view path
(the only per-tick shell-out is `claude agents --json --all`; `gh pr view` and the two model
probes are read-only and `OnceLock`-cached).

---

## D-004: Codex sessions are daemon-hosted, and the native TUI attaches to them

**Decision**: Host Codex sessions in the app-server daemon, and attach by spawning the **native
Codex TUI pointed at that daemon**:

```
codex resume --remote unix://<socketPath> <thread_id>
```

This satisfies all three requirements simultaneously - daemon-hosted so it keeps working while
detached, native so agent-viewer renders nothing itself, and non-duplicating.

**Rationale**: Verified end-to-end by the lead. `codex --help` documents `--remote <ADDR>` as
"Connect the TUI to a remote app server endpoint", accepting `unix://PATH`. The probe:

1. Started a thread via `thread/start` on the daemon and ran a turn via `turn/start`, both from
   a raw WebSocket client - **no TUI involved**.
2. **Closed the client connection entirely.** `thread/loaded/list` still returned
   `["019f9e18-de65-7d43-9ed5-d30bcfe66bcf"]`. The daemon kept hosting the thread. This is the
   "close Codex Desktop and come back" behavior, and it is a property of the **daemon**, not of
   Desktop.
3. Launched `codex resume --remote unix://<sock> <id>` in a pty. The native TUI rendered the
   conversation that the WebSocket client had started:

   ```
   › Reply with exactly: HELLO-PROBE

   • HELLO-PROBE
   ```

4. `thread/loaded/list` returned `COUNT: 1` while the TUI was attached, and the thread remained
   loaded after the TUI exited. **No duplicate at any point.**

Independent corroboration that resume does not fork: `fork` is a **separate top-level
subcommand** (`codex fork`), distinct from `codex resume`.

`codex remote-control {start,stop,pair}` exists as the supported way to manage a daemon with
remote control enabled, confirming this is an intended client topology rather than an
accident.

### The `notLoaded` gate is retained, demoted to a fallback selector

`ThreadStatus` is a four-way union (`v2/ThreadStatusChangedNotification.json`):

```
notLoaded | idle | systemError | active { activeFlags: waitingOnApproval | waitingOnUserInput }
```

`activeFlags` maps natively to needs-input. `notLoaded` remains meaningful because **threads
launched outside the daemon are invisible to it**: a real `codex` TUI launched in a pty produced
4,056 bytes of live UI while `thread/loaded/list` stayed `[]` and no new thread appeared in
`thread/list`. Such threads exist as durable data but have no daemon-side host.

So `status` selects the attach command rather than gating it:

| Thread status | Attach command | Why |
|---|---|---|
| `idle` / `active` / `systemError` (daemon-hosted) | `codex resume --remote unix://<sock> <id>` | Rejoins the live thread in place; proven no duplicate |
| `notLoaded` | `codex resume --remote unix://<sock> <id>` | Loads it **into the daemon**, so it becomes daemon-hosted from then on |

Using `--remote` in both cases is the simpler rule and the better outcome: it converges every
session onto the daemon, so a session attached once thereafter survives detaching. Plain
`codex resume` (no `--remote`) is the fallback used only when the daemon is unavailable.

**The one case that still refuses**: a thread that is `notLoaded` **and** has a live external
CLI process holding it (detected by the existing `/proc/fd` PID correlation). Loading that into
the daemon while a separate process runs it would be the duplicate this feature exists to
remove. Refuse with an inline error per FR-005a. Under the stated workflow - every session
launched from agent-viewer - this should not arise, so it is a safety gate rather than a
supported path.

**Alternatives considered**:

- *`thread/resume` + render conversation events in agent-viewer's own UI*: rejected. This is the
  one option that genuinely reimplements the Codex client, which D-001 forbids on product
  grounds. `--remote` obtains the same daemon-hosted semantics with the vendor's own renderer.
- *Plain `codex resume` in a PTY, gated on `notLoaded`* (the earlier proposal): rejected. It
  works and never duplicates, but the session dies on detach because agent-viewer owns the
  process. `--remote` keeps the daemon as the host and loses nothing.
- *Attach unconditionally without checking for an external process holder*: rejected. That is
  the remaining duplication path.

---

## D-005: Codex enumeration moves to `thread/list`

**Decision**: Drive the Codex list from app-server `thread/list` with an explicit `sourceKinds`
filter and `useStateDbOnly: true`. Stop reading `state_*.sqlite` directly for enumeration.

**Rationale**: `thread/list` carries everything the row needs - `name`, `preview`, `cwd`,
`gitInfo.branch/originUrl`, `updatedAt`, `status`, `source`, `path` - plus cursor paging, an
`archived` filter, a `cwd` filter, `searchTerm`, and sort keys. `useStateDbOnly: true` skips
the JSONL repair scan, which is what makes a 1-2s refresh loop viable.

Critically, **subagent exclusion is server-side and free**. The measured problem was 4,194
Codex threads against Claude's 29, of which 54% of active threads were subagent companions.
The default `sourceKinds` already drops them: a live 100-row all-kinds page contained 42
`subAgent: review` + 3 `thread_spawn` + 3 `guardian`, and the default page contained zero. This
resolves the volume problem in the protocol rather than in viewer-side heuristics.

`gitInfo.branch` also feeds PR-by-branch resolution directly.

**Two gotchas that must be encoded**:

1. The default `sourceKinds` also excludes `exec`, so it must be passed explicitly.
2. A page can be far shorter than `limit` while still carrying a `nextCursor` (11 rows observed
   for `limit: 200`). A short page is **not** the end of the list.

**Alternatives considered**:

- *Keep reading `state_*.sqlite` read-only*: rejected for enumeration. It works, but subagent
  exclusion and archived filtering become viewer-side guesswork against a serialized-enum
  `source` column, which is exactly the defensive parsing the current design is stuck with.
  Retained as the fallback when the daemon is not running.

---

## D-006: Rename is real on Codex, impossible on Claude

**Decision**: Implement real rename on Codex via `thread/name/set`. On Claude, rename is a
**no-op with a footer notice** under the backend-advertised capability model. Remove the
current rename code entirely.

**Rationale**: On Codex, `name` is a distinct persisted field from `preview` - a live row shows
`"name":"Codex Companion Task: say hi"` alongside `"preview":"say hi"` - and there is a
dedicated `thread/name/updated` broadcast notification. The response is an empty ack, so
success is read from absence-of-error plus the notification.

On Claude, no rename op exists on any daemon socket. Confirmed live: a correctly-authed rename
request is rejected at schema validation with
`{"ok":false,"error":"malformed request: Invalid input","code":"EUNKNOWN"}`. The complete
control-socket op union is ping / nudge / yield / lease / leases / await-ack / dispatch / list /
has / kill / reply / subscribe / attach / resize / ensure-spare / permission-response /
respawn-stale / shutdown. `rename_session` exists only as an SDK **stdio bridge**
control_request, unreachable from an external process.

**The current Claude rename code must be removed even if nothing else changes.** The rv server
does `jEe?.destroy(), jEe=r` on every new connection - it keeps exactly one. So every rename
keypress in agent-viewer **evicts the daemon's supervisor connection** from that worker, for
zero benefit. That is actively harmful, not merely a silent no-op.

**Alternatives considered**:

- *Keep the viewer-local rename override on Claude*: rejected. This is the "shadow state"
  Principle II forbids, and it is the confirmed source of the "rename is fake" complaint.

---

## D-007: Real-time is available, but not from one source

**Decision**: Drive freshness from three sources, each event-driven where the mechanism allows,
with a 2-second poll as the backstop that guarantees FR-009 regardless.

| Source | Mechanism | Covers |
|---|---|---|
| Codex app-server | push notifications post-`initialize` | daemon-hosted threads, name changes |
| Codex CLI sessions | inotify on rollout files + `/proc/fd` correlation | every CLI session |
| Claude | `claude agents --json --all` poll | Claude rows |

**Rationale**: On Codex there is no `thread/subscribe`; loading via `resume`/`read`/`start`
subscribes and `thread/unsubscribe` releases. Notifications nonetheless flow immediately after
`initialize` with no subscription at all - proved by the daemon pushing
`remoteControl/status/changed` unprompted. `capabilities.optOutNotificationMethods` mutes the
delta firehose.

**The limit that shapes this decision**: the daemon reports status only for threads it hosts.
Under D-004 every CLI session is `notLoaded` from the daemon's view, permanently. So
`thread/status/changed` does **not** deliver live status for the sessions this tool primarily
watches. Those must keep coming from the rollout file plus PID correlation - but that can be
inotify-driven rather than polled, which satisfies FR-009's "event-driven where achievable
without disproportionate engineering effort" without heroics.

`claude agents --json --all` was measured to create zero daemon residue across repeated
invocations, so polling it is safe.

**Alternatives considered**:

- *Pure 2s polling everywhere*: acceptable per the clarified FR-009, kept as the backstop, but
  rejected as the primary because inotify and app-server push are both cheap here.
- *Pure event-driven with no poll*: rejected. Neither mechanism covers opencode, and a missed
  event would leave a row permanently stale with no self-healing.

---

## D-008: Contention was a misread in the current SPEC

**Decision**: Treat concurrent app-server clients as safe. Restrict caution to semantic
conflict, not transport conflict.

**Rationale**: `SPEC.md:100-103` rejected the app-server partly on the grounds that "the
control socket is already contended". That is a misreading of the log line, which is a **bind**
failure from a second would-be *server*, not client contention. `lsof` shows a single LISTEN
holder; three simultaneous client connections all initialized and served different requests
concurrently. The lead independently reproduced this with a stdlib WebSocket client while other
clients were connected.

The real residual risk is semantic - two clients steering one live thread - which D-004 already
forecloses by never calling `thread/resume`.

---

## D-009: Codex transport is WebSocket over the Unix socket

**Decision**: Connect directly to the socket path reported by `codex app-server daemon version`
and speak RFC6455-framed JSON-RPC (no `jsonrpc` field). Do not use `codex app-server proxy`.

**Rationale**: The control socket is not a raw JSONL endpoint. Raw JSONL gets an immediate EOF.
`codex app-server proxy` is broken against it in 0.144.4 - strace shows a literal byte relay
with no HTTP upgrade. The working handshake is an HTTP `Upgrade: websocket` on the UDS,
reverse-engineered by stracing `codex app-server daemon version` and then independently
reimplemented by the lead in stdlib Python, which returned real `initialize`, `thread/list` and
`thread/loaded/list` responses.

Discovery, not hardcoding: `codex app-server daemon version` prints `socketPath`, and
`"status":"running"` is the availability gate. This matches the existing discover-don't-hardcode
discipline used for `state_*.sqlite`.

**Daemon lifecycle**: verified independent of Codex Desktop. The listener is pid 23921 with
`PPid 1` (detached), started before Desktop, and Desktop is not running. `codex app-server
daemon start` is a documented subcommand. So abandoning Desktop does not cost the daemon, and
agent-viewer can ensure it. Starting a daemon is a user-visible side effect, so when
`status` is not `running` the viewer falls back to read-only SQLite (D-005) rather than
starting one silently.

**Risk to carry forward**: the API is marked `[experimental]`, versioning is shallow (`v1/`
holds only `initialize`, `v2/` holds all 228 thread/turn files, with no protocol-version
negotiation), and there is already same-version schema drift - the live 0.144.4 daemon emits
`extra` and `historyMode` on `Thread` which the schema generated by that same binary does not
declare. Deserialization must be permissive. `Thread.path` is explicitly marked `[UNSTABLE]`.

---

## D-010: opencode capabilities are narrower than currently advertised

**Decision**: Gate opencode down to what it can actually do, and correct two capability bits
that currently lie.

**Working**: enumerate (read-only SQLite, 297 rows, subagents via `parent_id`), last-message,
spawn, attach-in-place, rename (raw SQL via `opencode db`, since no rename subcommand exists),
delete, model list.

**Not possible - must be advertised as unsupported**: needs-input, archive/hide, reply, PR
association. Re-verified against live data rather than trusting the code comment: the
`permission` table is project-scoped policy with **0 rows**, and `session.permission` is a
static per-session deny array (`[{"permission":"question","pattern":"*","action":"deny"},...]`),
not a pending ask. Archive is equally dead - `time_archived` exists and is read, but no CLI
writes it and **0 of 297** rows are archived.

**Two bits that currently misreport**:

1. Live status is **backend-wide, not per-session**: `opencode.rs:264-270` checks whether any
   `opencode*` process exists, so every session flips to Working together. Either derive
   per-session status or advertise status as unsupported.
2. Stop advertises `true` but is pid-gated, and `list()` always sets `pid: None`. It therefore
   works only for sessions this viewer spawned in this process's lifetime and fails at press
   time for everything else, despite the capability bit being set.

**Rationale**: Principle "capabilities are backend-advertised" only holds if the advertisement
is truthful. A capability that is advertised and then fails at press time is worse than one
advertised as unsupported, because the footer notice is the designed affordance for the latter.

---

## D-011: Both primary backends have real detached background work

**Decision**: Treat Claude and Codex as equivalent in session durability. Both host sessions in
a daemon that outlives agent-viewer; both are attached with the vendor's own client.

| Mode | Claude | Codex |
|---|---|---|
| Session host | Claude daemon (`claude --bg`) | app-server daemon (`thread/start`) |
| Background progress while detached | yes | yes |
| Attach | `claude attach <short_id>` | `codex resume --remote unix://<sock> <id>` |
| Survives agent-viewer exit | yes | yes |

**Correction**: an earlier version of this decision claimed Codex had no background equivalent
and that a foreground Codex session necessarily died on detach. That was wrong. It was based on
the true observation that a PTY-spawned `codex` is a child process, but missed `--remote`, which
makes the daemon the host and the TUI a mere client. D-004 records the end-to-end proof: a turn
started from a raw WebSocket client survived the client disconnecting, and the native TUI
rejoined it afterwards with no duplicate.

**Rationale**: This removes the asymmetry that would otherwise have forced different mental
models per backend in the same list. A row is a row: it belongs to a daemon, it keeps working
whether or not anyone is looking at it, and pressing Enter runs the vendor's own client against
it.

`codex exec` remains available and enumerated (the `exec` source kind, which D-005 requires be
passed explicitly), but it is now a *choice* for one-shot non-interactive work rather than the
only way to get detached progress on Codex.

**Alternatives considered**:

- *Have agent-viewer keep PTYs alive itself (setsid plus a PTY multiplexer) so sessions survive
  viewer restarts*: rejected. Both backends already ship a daemon that does this properly;
  building a third would duplicate them and own the hardest part of their design.

---

## D-012: Interactive rows are not listed at all

**Decision**: Filter Claude sessions to `kind == "background"`. Interactive sessions do not
appear in the unified list.

**Rationale**: This is parity, not a judgement call - **Fleet View's own TUI does not list
interactive sessions.** Only `--json` emits them. Three independent proofs, captured live:

1. With 30 background and 1 interactive session alive, the Fleet View header read
   `5 awaiting input · 1 working · 24 completed` - exactly the 30 background sessions. Starting
   a *second* interactive session left the header byte-identical, and grepping the cumulative
   pty stream for both interactive session names returned 0 hits.
2. `claude agents --cwd <dir containing only an interactive session>` rendered the full empty
   state (`0 awaiting input · 0 working · 0 completed`) while that session was present in
   `--json`.
3. Self-documentation agrees: `claude agents --help` reads "Manage background agents"; `--all`
   and `--cwd` are both worded "background sessions"; only `--json` mentions interactive.

Because Fleet View's chosen behavior is **omission**, filtering is the faithful reproduction and
it removes the `claude -r <uuid>` duplicate-client path (D-003) at the source rather than gating
it. Showing interactive rows would be a deliberate divergence requiring its own design.

**Second wart in the same parsing path**: interactive rows lack `state` as well as `id`, so the
current code renders every one of them as permanently `Idle`. Filtering removes both defects at
once.

Questions about how Fleet View glyphs, Enter, or hotkeys behave on an interactive row are
**not applicable** - no such row is ever rendered.

**Alternatives considered**:

- *Refuse attach on interactive rows but still display them*: rejected. It diverges from Fleet
  View, and the row would carry no usable state anyway.

### Environment gotchas recorded for future probes

- Writing keys to another process's pty master via `/proc/<pid>/fd/<n>` silently fails; that
  symlink resolves to `/dev/ptmx` and opening it allocates a *new* pty pair. The driver must own
  the master fd.
- A `claude` spawned from inside a Claude session inherits `CLAUDE_CODE_CHILD_SESSION=1`, runs
  with transcript persistence off, and **never registers with the agents daemon**. Without
  unsetting it, an experiment silently measures nothing.
- Typing into the Fleet View panel composer **dispatches a new session** rather than chatting.

1. **APC out-of-band messages on the Claude byte stream.** `EKICKED:` / `ESTALLED:` arrive
   inline in the pipe and need parsing or stripping. Format not fully determined.
2. **Whether the Claude client must emit the `decModes` enable sequences itself** or the daemon
   replays them. The reply advertises them; who applies them is unconfirmed.
3. **`claude rm` worktree deletion was not observed end-to-end.** It deletes the worktree and
   branch only when the kill is confirmed, the tree is unclaimed, not git-locked, not dirty
   (`--force` overrides) and has no unpushed commits; otherwise it prints
   `kept <id> - worktree <reason>`. The id is matched by **prefix** and it exits 1 on ambiguity,
   so a hotkey must surface the "kept" output rather than read non-zero as success. Conditions
   were read from the bundle, not observed.
4. **Per-session opencode status** - whether any live signal exists beyond process presence.
