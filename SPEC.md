# agent-viewer — build spec

Terminal viewer for OpenAI Codex sessions, in the spirit of Claude Code's `claude agents`.
This spec is the contract. Every fact below was verified on this box (Codex 0.144.1,
Rust 1.95.0) during research; where a fact needs live re-verification during the build it
is called out explicitly. Stop after the naive correct solution — do not add abstractions,
config surfaces, or features not requested.

## Hard requirements (the whole point)

1. **Create new background Codex sessions** — fire-and-forget, like `claude --bg`.
2. **See ALL Codex sessions including background ones, however launched** — no dependency on
   the tool having launched them, no plugin/hook instrumentation required.
3. **Hide / unhide sessions** — like dismissing rows in the `claude agents` view.
4. **Group sessions by project** (working directory).

## Scope

- **v1 (this build): a Rust TUI** (`ratatui`) plus a `-core` library crate. The TUI is the
  primary deliverable because the killer feature (one keystroke attach/resume into a session)
  is inherently terminal. Linux is the fully measured runtime platform. Native release archives
  also target macOS Intel, macOS Apple Silicon, and Windows x64; those platforms enumerate and
  render sessions but do not claim Linux process status, Codex daemon controls, or secure
  managed OpenCode behavior.
- **Out of scope for v1:** a web/Tailscale surface. It is a natural v2 (an `axum` binary
  sharing `-core`, deployed like the `bonus-drain`/`bg-schedule` viewers with token-guarded
  write routes) but do NOT build it now. Leave `-core` cleanly separable so v2 can reuse it.

## Enumeration — the source of truth (requirements 2, 3-read, 4)

Codex maintains a global session registry. Read it and its name index read only.

- Path: `~/.codex/state_*.sqlite`. **Glob and pick the highest version number** (currently
  `state_5.sqlite`); do NOT hardcode `5`.
- Open **read-only** with WAL tolerance: `rusqlite` with `OpenFlags::SQLITE_OPEN_READ_ONLY`
  (use the `bundled` feature). Codex writes concurrently; never write this DB yourself.
- Table `threads`, load-bearing columns (verified via `.schema threads`):
  `id TEXT PK`, `rollout_path TEXT`, `created_at`, `updated_at` / `updated_at_ms`,
  `source TEXT`, `cwd TEXT`, `title TEXT`, `archived INTEGER DEFAULT 0`, `archived_at`,
  `model TEXT` (read only for the model-picker fallback via `distinct_models`, not per row),
  `preview TEXT`. Order by `updated_at_ms ASC`, retaining the existing `id DESC` tie break. Other columns exist in the schema
  (`git_branch`, `git_origin_url`, `first_user_message`, `thread_source`, `agent_nickname`,
  `agent_role`) but the reader does not load them.
- `source` is a **serialized enum, not a flat string**. Observed values: `cli`, `exec`,
  `vscode`, and JSON blobs like `{"subagent":"review"}` or nested `thread_spawn` objects.
  Parse defensively: match `cli`/`exec`/`vscode` prefixes; anything else → treat as subagent.
- `archived=1` rows are the hidden set; their `rollout_path` points into
  `~/.codex/archived_sessions/` (active rows point into `~/.codex/sessions/`).
- Grouping key = `cwd`. Optionally fold `cwd` up to the nearest `.git` root so worktrees
  collapse under one project (mirrors the local `claude-usage` "aggregate worktrees" idea).

`~/.codex/session_index.jsonl` overlays the SQLite title on every list refresh. Read it read
only. For each id, the latest valid entry with a nonempty name wins. Use the SQLite title when
the index is missing, unreadable, malformed, invalid, or has no matching entry. The index never
supplies rows, status, or any field other than the name.

## Shared listing cache

The viewer owns SQLite snapshots keyed by each backend advertised listing scope. A snapshot is
fresh for two seconds. A renewable five second lease coordinates refreshes, and a generation
check is the fast path for readers. Successful empty listings are snapshots too. Cold followers
wait for the lease holder and then read its snapshot rather than independently listing.

This cache is display only. Attach, mutations, and session derived spawn directories always
relist their authoritative source. Default viewer managed OpenCode credentials use one shared
noncredential scope. Explicit configured or environment OpenCode passwords bypass sharing.

## Enumeration and runtime: opencode

The primary OpenCode authority is a secured loopback server. The viewer probes fixed candidates
`127.0.0.1:4097`, then `127.0.0.1:4098`. It never stops or restarts a server. Spawn alone may
start `opencode serve --hostname 127.0.0.1 --port <port>`, always from the user home directory.
The child inherits the normal environment and overrides only `OPENCODE_SERVER_USERNAME` and
`OPENCODE_SERVER_PASSWORD`. Task shells receive neither credential.

Credentials use nonempty environment overrides when supplied, otherwise a generated stable
secret in owner only credential files. SQLite stores only viewer presentation state. `~/.local/share/opencode/opencode.db`
is opened read only as compatibility enumeration when no secure server is available.
That fallback retains parent and run mode companion classification through the stored `permission`
field.

Before any credential bearing request, a fresh stream is connected without writing. The viewer
verifies the listener owner is the pinned process, sends unauthenticated `GET /global/health`
with keep alive, and requires `401` on a reusable connection. It finds the accepted connection
inode for that exact local ephemeral tuple and verifies pid, start time, effective uid, exact
argv, listener inode, and runtime generation. It revalidates after the test hook, then writes
the authorized request with close on that same `TcpStream`; it never reconnects for auth. This
same stream requirement is load bearing because Linux `TCP_DEFER_ACCEPT` delayed acceptance
until the initial health write.

The pin contains pid, start time, listener inode, effective uid, and exact argv. Runtime state
contains a generation, pin, healthy state, and managed ids. Process shared ownership uses only
`flock`; each viewer process serializes its own work locally. An occupied listener that returns `200` to unauthenticated
health is insecure and rejected, never stopped or restarted. Spawn may use `4098` if it is free.

The HTTP client is bounded HTTP/1.1 over `TcpStream`, with strict content framing, bodyless
`204` handling, no redirects, and bounded headers, body, and timeouts.

Global listing is `GET /experimental/session?limit=10000&archived=true`, following
`X-Next-Cursor`. Repeated or malformed cursors, or a full page without a cursor, are errors.
The only managed marker is the exact permission rule
`{"permission":"agent-viewer.background","pattern":"*","action":"allow"}`. Metadata is
not a valid marker.

Only exact marked rows are managed. Only they receive `daemon_hosted`, live status, pending input,
managed capabilities, and server mutations. The managed id cache includes archived marked rows
so archive and unarchive work, but archived marked rows are not status polled. Status, permission,
and question are fetched once per unique active managed directory. A failure affects only that
directory's rows, including external rows, which become `Unknown`. Otherwise external server
enumerated rows use compatibility `Idle` status.

Server mutations apply only to exact managed rows. Managed attach is refused because it would
expose credentials. External rows use `opencode -s <session_id>`. External deletion remains local
`opencode session delete <id>`.

## Enumeration — claude, and the nested `claude -p` companion rule

Rows come from `claude agents --json --all`. A non-zero exit or a missing binary is a quiet
empty backend, never an error. Each row is enriched from `<jobs root>/<short>/state.json` for
summary, transcript path, PR refs, and `updated_at`. For activity, `linkScanPath` is
authoritative when present. Otherwise, resolve an existing canonical projects transcript from
the row `cwd` and `sessionId` under the same config root.

**Companions.** Every live claude process registers itself at `~/.claude/sessions/<pid>.json`,
and the agents list returns all of them. That includes a nested `claude -p`, the Agent SDK's
headless entrypoint, which another session shelled out to from a Bash call: a skill, a hook, an
`/implement` planning pass. It is a real process but not a fleet member anyone started, and
because it has no `jobId` it never gets a name the user chose, so claude derives one from the
cwd as `<dir-basename>-<n>`. Those rows read as mystery sessions in the default view and vanish
on their own when the child exits, since the pid file is removed on exit.

The discriminator is `entrypoint`, matched on the `sdk-` prefix so the whole SDK family
(`sdk-cli`, `sdk-ts`, `sdk-py`) is one rule. Two alternatives were rejected because a genuine
interactive terminal session is indistinguishable from a nested `claude -p` under both:
`kind == "interactive"` is what a real terminal session reports too, and the absence of
`id`/`jobId` is equally true of one. A `--bg` job and an interactive session BOTH report
`entrypoint: "cli"`, which is what makes the `sdk-` prefix the safe cut.

**`entrypoint` is not in the agents output.** Verified live on this box 2026-07-27, claude
2.1.220: `claude agents --json --all` projects only
`cwd, id, kind, name, sessionId, startedAt, state` plus `pid` on live rows. Reading
`entrypoint` off an agents row always yields nothing, so the rule needs a second read of the
per-process registry, keyed by the row's `pid`:

```
$ claude agents --json --all | jq '[.[] | .entrypoint] | unique'
[null]

$ cat ~/.claude/sessions/2054075.json
{"pid":2054075, "cwd":".../agent-viewer/.worktrees/claude-companion-filter",
 "kind":"interactive", "entrypoint":"sdk-cli",
 "name":"claude-companion-filter-61", "nameSource":"derived"}
```

Rows with no `pid` are skipped without touching the disk: a pid is absent exactly for finished
background jobs, which are real fleet members. The sessions root follows the same
`$CLAUDE_CONFIG_DIR` precedence as the jobs root. A missing or unreadable registry file parses
to "real session", the same safe direction as the opencode rule, and the same two escapes keep
this from swallowing anything: the viewer-state overlay clears `companion` for sessions the
viewer itself spawned, and `Ctrl+A` and `Ctrl+F` both surface companion rows.

## Rollout transcripts (reusable readers + status tail)

Path: `~/.codex/sessions/YYYY/MM/DD/rollout-<ts>-<uuid>.jsonl`. Parse with `serde_json`
line-by-line (`BufReader`). For the list, read only the **first line** (`session_meta`, has
`payload.cwd`, `payload.id`, `payload.originator`, `cli_version`) and the **last few lines**
for the terminal marker. The core also exposes full-file transcript readers for the activity
ribbon, which renders complete streamed Codex history including named tool activity.

Activity ribbons cover one hour and aggregate a row with its recursive descendant subtree. Codex
reads `parent_thread_id` from a subagent `thread_spawn` source. Claude treats the root's flat
transcripts under its transcript stem `subagents` directory as descendants, then isolates child
subtrees through sibling `.meta.json` `parentAgentId` links. OpenCode follows `session.parent_id`
with a recursive read only SQLite query. A child row remains isolated to its own subtree. Missing
or malformed child data is best effort and never removes readable root activity. The hierarchy
cache rereads every thirty seconds.

- Message content: `type:"response_item"`, `payload.role`, `payload.content[].text`
  (assistant text is `content[].type == "output_text"`).
- Terminal marker: an `event_msg` with `type:"task_complete"`, preceded by a `token_count`
  event. Its presence near the tail = the session's last turn finished cleanly.

## PR refs — codex reads them out of the transcript

Claude records its PRs in `jobs/<short>/state.json` (`children[]` where `kind == "pr"`). Codex
records them nowhere: the registry has no PR column, and `threads.git_branch` is captured when
the thread starts, so it is stale the moment the agent branches (measured: the thread that
opened `curie-eng/curie/pull/1089` still reports `task/fix-interactive-clippy`). Branch lookups
are therefore not a usable source, and the rollout transcript is — the same thread's JSONL
carries all three PR URLs it touched.

So `codex::pr_scan` scans rollouts for `github.com/<owner>/<repo>/pull/<n>`. That means a
session badges every PR it *mentions*, not only one it created; a review session pointing at the
PR under review is the intended reading. Cost rules, all load-bearing at this box's scale (4,963
threads, 1.8 GB of rollouts, of which 1.3 GB is `archived_sessions`):

- **Per-file offset.** A rollout is read once, then only where it grew, and not at all while its
  length is unchanged. Without it, `list` would re-read gigabytes every second.
- **Complete lines only.** Rollouts are appended live, so the trailing partial line is left for
  the next tick. Parsing it would mint a truncated number (`pull/10` for an in-flight `1089`),
  and refs are sticky, so that badge would never heal.
- **A shared per-tick byte budget** (`SCAN_BUDGET_BYTES`), spent live-newest-first and archived
  last. Measured cold on this box: every visible codex row that has a PR is badged within ~14
  ticks, the newest within one or two, and the archive trickles in behind it. There is no
  on-disk cache, so this repeats once per launch.
- **`MAX_REFS_PER_SESSION`**, keeping the most recent. Each ref costs a live `gh` fetch in the
  TUI's status cache, and one real batch-review rollout mentions 115 distinct PRs.

## Status detection — TWO signals, both required

The file signal alone is insufficient: during research 66/383 rollouts lacked `task_complete`
yet **zero** `codex` processes were running, i.e. those are crashed/abandoned, not live.

Resolve per thread on Linux:
1. **running** — a live `codex`/`codex exec` PID holds this thread's `rollout_path` open.
   Enumerate PIDs (`sysinfo`), then read `/proc/<pid>/fd/*` (readable for same-user procs;
   use `procfs` or `std::fs::read_link`) and match an open path to `threads.rollout_path`.
2. **done** — not running AND `task_complete` in the tail.
3. **errored/abandoned** — not running AND no `task_complete` (ends mid-turn).
4. **hidden** — `archived=1` (orthogonal; applies on top of 1-3).

On macOS and Windows, `/proc` file descriptor evidence is unavailable. A `task_complete` tail
proves `Done`; every other tail is `Unknown`, never inferred as live, idle, or errored. Actions
that need unavailable runtime evidence are capability gated and retain the existing footer
notice behavior.

> LIVE VERIFICATION REQUIRED: the PID→rollout `/proc/fd` correlation was NOT tested against a
> running Codex process (none were running during research). The build MUST verify it
> end-to-end: spawn a real `codex exec`, confirm it shows **running**, confirm it flips to
> **done** on completion. This is a done-when criterion, not optional.

## Mutations — delegate to Codex subcommands, never write the DB

All verified present in `codex --help` (0.144.1):
- **Create (req 1):** background a detached `codex exec`. Shape:
  ```
  setsid codex exec --json -C <project-dir> \
    --dangerously-bypass-approvals-and-sandbox "<task>" \
    > ~/.codex/bg-logs/<name>.log 2>&1 &
  ```
  In Rust: `std::process::Command`, detached (new session via `nix::unistd::setsid` or
  `libc`, `stdin(Stdio::null())`), do not wait. The new `threads` row appears within ~1s and
  enumeration picks it up.

  **Spawn sandbox posture — resolved, do not "harden" this back.** The open question above
  ("prefer the least-privileged flag that actually runs unattended") was once answered with
  `--sandbox workspace-write`, and that answer was wrong. Evidence, verified live against
  codex-cli 0.145.0 on this box:
  - Under `workspace-write`, codex mounts `.git` **read-only** and leaves `network_access`
    off. A probe run confirmed it directly: writing a file in the workspace exits 0, writing
    `.git/PROBE_HEAD` exits 1. The same probe under
    `--dangerously-bypass-approvals-and-sandbox` exits 0.
  - A real viewer-spawned session ended with: *"No source files were changed. The sandbox
    mounts `.git` read only, so the required branch and worktree could not be created.
    `git fetch origin main` failed with `cannot open '.git/FETCH_HEAD': Read-only file
    system`."* It burned a full turn on read-only triage and shipped nothing.
  - Every task this viewer spawns is a git-shaped task (branch, worktree, commit) on the
    user's own box, per the repo's branch-before-editing rule. A sandbox that blocks `.git`
    turns those runs into silent no-ops, which is strictly worse than no sandbox: the session
    still consumes tokens and still reports "done".
  - `codex exec --help` (0.145.0) exposes only `writable_roots`, `network_access`,
    `exclude_tmpdir_env_var`, and `exclude_slash_tmp` under `sandbox_workspace_write`. There
    is no "make `.git` writable" switch. `--add-dir <repo>/.git` does unlock it, but only for
    one repo's metadata dir, and it breaks for worktrees (whose `.git` is a *file* pointing
    into the parent repo) — so it is not a general fix.

  Viewer-spawned sessions therefore run `--dangerously-bypass-approvals-and-sandbox`: no
  sandbox, no approval prompts. This matches how the sessions are actually used (the user's
  own machine, the user's own repos, unattended) and is asserted by
  `codex_spawn_command_runs_unsandboxed` in `crates/agent-viewer-core/src/codex/mod.rs`.
  **Superseded for viewer-spawned sessions (kept for the record).** A `codex exec` spawn runs
  its app-server IN PROCESS, so the thread it creates can never be joined from another
  process. Spawn now goes through the shared `codex app-server` daemon (`thread/start` plus
  `turn/start`) and the `codex exec` shape above is the fallback for when no daemon can be
  reached. The sandbox posture is unchanged and carried onto the new path: `thread/start` pins
  `sandbox: danger-full-access` and `approvalPolicy: never` for exactly the evidence above.
  See "Codex attach/resume" below.
- **Hide (req 3):** `codex archive <id>`; **unhide:** `codex unarchive <id>`;
  hard-delete (optional, guard behind a confirm): `codex delete <id>`.
- **Attach/resume (superseded, kept for the record):** `codex resume <id>` (or
  `codex exec resume <id>`), exec'd into the user's terminal from the TUI. This is still the
  command for a session with no live turn, but it is NOT how a live session is joined; see
  "Codex attach/resume" below for what replaced it and why.

## Codex attach/resume - what is implemented, and the measurements behind it

**`codex resume <id>` on a running session does not join it.** The new process gets its own
ThreadManager, replays the rollout, sees that it ends mid-turn, and appends a synthesized
`TurnAborted{reason: Interrupted}`, which the TUI renders as "Conversation interrupted - tell
the model what to do differently". Measured live against codex-cli 0.145.0 on this box: a
foreign resume of a live thread reported `status: idle` with turn statuses `["interrupted"]`
while the real session kept running untouched, both processes then holding the same rollout
path. That is a destructive read, so agent-viewer refuses it (see below) rather than showing a
false interruption.

**A real join exists only inside the ONE app-server process that hosts the thread.** There,
`thread/resume` returns the history and atomically subscribes the caller to live updates: two
clients on one app-server received byte-identical live streams with no interrupt marker. The
TUI reaches that with `codex resume --remote unix://<socketPath> <id>`.

**Codex attaches use inline terminal mode.** The viewer adds `--no-alt-screen` to each Codex
resume command and retains a bounded 2,000 row PTY scrollback. Codex discards mouse reports,
so the viewer consumes each Codex wheel event as local viewport movement of three rows. Inline
mode is required because Codex's alternate screen leaves the terminal emulator with no history
for the viewer to scroll.

**A daemon-hosted turn survives its client disconnecting.** A turn started with `thread/start`
plus `turn/start` kept running after the initiating client closed the socket: the rollout grew
3778 bytes post-disconnect, thread status stayed "active" and the turn "inProgress". So a spawn
is exactly connect, start thread, start turn, disconnect. `turn/start` answers immediately with
the new turn (`status: inProgress`), so waiting for that response confirms acceptance without
waiting for the work.

**THE GUARD: the daemon holds the rollout fd of every thread it hosts** (verified by reading
`/proc/<daemon pid>/fd`). The `/proc/fd` scan therefore reports the DAEMON's pid as the owning
pid of every daemon-hosted row, and a SIGTERM there would kill the daemon and every session
inside it. The scanner now returns a `RolloutOwner { pid, daemon }`, the resolver withholds a
daemon pid (returning `None` plus `daemon_hosted: true`), and `stop` routes a daemon-hosted row
to `turn/interrupt` over the socket, never to a signal. What such a row's fd does NOT do any
more is decide its status: see the rule table below.

**A host is identified by the LISTENING socket it holds, in UNION with the argv test.**
`/proc/net/unix` carries every unix socket's inode and flags, and `SO_ACCEPTCON` (0x10000) in
FLAGS is the listening marker. Not the state: `St` comes from `sock->state`, where 01 is
SS_UNCONNECTED, which a socket that never called `listen()` also reports. Confirmed against
the live table on 2026-07-27 - the daemon's accepting socket is `Flags=00010000 St=01` (inode
4441325, pid 2950586) while a client on the same path is `Flags=00000000 St=03`.

Intersected against a codex process's own fds (the `/proc/<pid>/fd` sweep that collects rollout
fds already sees the `socket:[<inode>]` links, so it costs nothing extra) this says "this
process is a server", which is what an app-server hosting other people's threads IS. A
`codex exec` session runs its app-server in process and listens on nothing; an interactive
session holds a CONNECTED socket, which is deliberately not matched.

Deliberately NOT filtered to the control-socket path: a second app-server on a custom
`--listen` endpoint hosts threads exactly as the managed daemon does.

**Neither signal is complete, so they are a union rather than a fallback chain.** The socket
table cannot see an app-server on the `stdio://` transport, which holds no socket at all (the
editor-extension shape). argv cannot be parsed reliably, which three rounds of review
established: a fixed index breaks `codex -c k=v app-server`, scanning for prompt-taking
subcommands breaks a profile named `exec`, and a uniform option-arity parse breaks a boolean
flag mixed with a value-taking one. argv does not say which options take values and this crate
does not own the CLI, so that guessing had no fixed point.

Either signal saying "host" is therefore enough, and the argv side stays deliberately crude and
over-eager: `app-server` anywhere means host, minus the `app-server daemon` probes. The
asymmetry licenses it. A false negative hands a host's pid to SIGTERM and kills every session
inside it; a false positive only routes stop through `turn/interrupt`, which fails visibly and
kills nothing. The known residual is a session whose task prompt is exactly the word
`app-server`, which reads as a host and loses its own pid. That is the accepted side of the
trade.

The predicate previously also treated "holds more than one rollout fd" as proof of the daemon.
That is measurably false and was removed: a plain
`codex exec --json -C ... --dangerously-bypass-approvals-and-sandbox` (pid 2910115, live on
2026-07-27) held TWO rollouts, its own plus a subagent thread it spawned (registry `source`
`{"subagent":{"thread_spawn":...}}`). Scoring that as the daemon would route its attach to a
`--remote` join on a daemon that does not host it, fabricating the exact interrupt this whole
path exists to prevent, and would make its stop a silent no-op.

**Interactive `codex` sessions ARE daemon-hosted, and therefore joinable.** Measured live on
2026-07-27 with a pty harness (TIOCSWINSZ, raw mode, no `start_new_session`): a plain `codex`
launched in a terminal with the daemon already up ran a real turn, and the ONLY holder of the
new session's rollout fd was the listening daemon (pid 2950586), not the TUI process. So an
ordinary terminal session can be joined with `resume --remote`, and the unjoinable rows are the
`codex exec` ones (bg jobs, plugin dispatches), whose app-server is in process. Two earlier
probes suggested the opposite; both were harness failures (no raw mode, `start_new_session`, so
the prompt never reached the app) and are not evidence.

**A viewer-started daemon is pinned to a cwd it cannot outlive** (`$HOME`, falling back to `/`).
The daemon is long-lived and shared box-wide but inherits its starter's cwd, and agent-viewer
routinely runs from a `.worktrees/` checkout that is deleted when its branch merges. Measured
live on 2026-07-27: the daemon's `/proc/<pid>/cwd` pointed at a deleted worktree, `daemon
version` still answered `"status":"running"`, and every `thread/start` failed with `failed to
load configuration: No such file or directory (os error 2)`. Spawns then silently degraded to
`codex exec` for hours, and every other codex client on the box was equally broken. Recovering
required restarting the daemon from a real directory, which the viewer must never do itself.

**No silent exec fallback.** The spawn path used to fall through to `codex exec` on any daemon
failure, which is why the poisoned daemon above stayed invisible: a degraded spawn that produced
an unjoinable session looked identical in the UI to a good one. Every viewer-spawned session
must be joinable, so a spawn that cannot be is now a footer error naming the daemon's own
failure. `AGENT_VIEWER_CODEX_EXEC_SPAWN=1` restores the exec path as an explicit opt-in, and it
outranks the daemon so an operator who asks for exec is not silently given something else.

**Daemon-created threads are recorded with `source = vscode`,** because the daemon runs with
the default `--session-source vscode`. Viewer-spawned rows therefore enumerate as
interactive-origin. That is accepted, not worked around.

**Consequence, and the second status rule set it forces.** The daemon keeps the rollout fd open
long after the turn completes (observed still held 20+ minutes later), so for a daemon-hosted row
the open fd carries NO liveness information. Applying the fd rules there stranded every finished
background task at Idle forever, which makes working and finished indistinguishable for exactly
the sessions the viewer spawns. `status_from` therefore takes `daemon` and `attached` alongside
`open` and the tail, and a daemon-hosted row is resolved from the TAIL:

| daemon-hosted row, tail | status |
| --- | --- |
| MidTurn, or unreadable/empty (spawn race) | Working |
| AwaitingApproval | NeedsInput |
| Complete, a client attached | Idle |
| Complete | Done |

Rows the daemon does not host keep the fd rules unchanged, and `attached` never touches them.

**"A client is attached" is read from argv in the same process sweep.** A TUI that joined a
daemon-hosted thread runs `codex resume --remote unix://<sock> <thread-id>`, so the thread id
appears in a codex process's argv. `scan_codex_processes` returns both the open-rollout map and
that set of thread ids from ONE sysinfo pass, because this runs every refresh tick over thousands
of threads: no second sweep, and no per-row daemon query on the render path. Detection is
shape-based (a 36-char UUID token), and a false positive costs one tick of Idle instead of Done.

The precise meaning of "attached" is therefore "a `codex resume` process for this thread still
exists", which is NOT the same as "the user is looking at it". `Ctrl+]` detaches the view but
the viewer deliberately keeps the PTY child alive so re-attaching is instant, so the row stays
Idle until that child actually goes away (quitting the viewer, `Ctrl+X` twice, or the child
exiting on its own). Documented rather than fixed: the alternative is asking the daemon per row
on the render path.

**Known gap, narrowed but not closed: stale-daemon attach targeting.** `attach_route` points a
daemon-hosted row at whatever daemon `daemon version` currently answers, without checking that
it is the same process that holds the row's fd. If a daemon were restarted (the viewer never
restarts one) the endpoint could name a daemon that does not host that thread. The socket-inode
signal above now makes the fix cheap, since the scan already knows which pid holds the listening
socket: carrying that pid on the row and comparing it to the answering daemon is all that is
left. Still out of scope, and now a small change rather than a design one.

Two facts make the tail trustworthy here: the daemon writes the same `task_complete` event_msg
the exec path does (verified on a daemon-created rollout), and the Working-to-Done flip is
observable end to end on the daemon path (`codex_spawn_running_then_done` and
`embedded_attach_live` both pass against a daemon spawn).

Implemented routing (all three seams are pure functions in `codex/mod.rs`, so they are unit
tested without a daemon):
- **spawn:** `ensure_daemon()`, then `thread/start` + `turn/start` on it; the thread id comes
  back from `result.thread.id` and flows as `SpawnResult.session_id` through `Mutation::Spawn`
  into `SpawnSelection`. It is not stored in the viewer database `SpawnRecord`, which remains
  PID based; a daemon spawn still has no killable PID. The TUI selects the first selectable
  snapshot containing that id and preserves the selection. For a backend without an id, row
  discovery excludes the complete session set captured before submission, including hidden,
  filtered, and collapsed rows. There is NO silent fallback: any daemon failure, and the
  absence of a daemon, is a visible failed spawn carrying the daemon's own error. `codex exec`
  is reachable only via `AGENT_VIEWER_CODEX_EXEC_SPAWN=1`. See "No silent exec fallback".
- **attach:** daemon-hosted plus a reachable daemon -> `codex resume --remote <endpoint> <id>`;
  a foreign row that is mid-turn with a live pid -> refused with the reason in the footer;
  everything else -> plain `codex resume <id>`. A daemon-hosted row whose daemon is gone also
  takes the plain resume: there is no live turn left to protect.
- **stop:** daemon-hosted -> `turn/interrupt` (the in-progress turn id is resolved first with
  `thread/read` + `includeTurns`, since `TurnInterruptParams` requires both ids; no live turn is
  a no-op success); else a pid -> SIGTERM as before; else unsupported.

**Attached terminal palette.** The TUI captures the host terminal foreground and background
once. Concrete RGB themes choose their text and background first; the terminal match theme falls
back to the captured host colors. The chosen palette is mutable on `PtySession` and refreshes
when the user reenters a retained session after changing theme. OSC 10, OSC 11, and default cell
rendering use that same current palette. Explicit indexed and RGB child colors remain untouched.
When no palette is available, OSC queries receive no reply and default cells use Reset rendering.
Never fabricate unavailable host colors.

Transport: the control socket is a Unix socket that upgrades to RFC6455 WebSocket at `/rpc`
(handshake URL `ws://localhost/rpc`), driven by blocking `tungstenite` over
`std::os::unix::net::UnixStream` since this crate is synchronous. A client offering
`permessage-deflate` is rejected ("Missing, duplicated or incorrect header
sec-websocket-extensions") and dropped, so no compression may be advertised.

`codex app-server daemon version` is the availability gate and the socket discovery
(`"status":"running"` plus a non-empty `socketPath`); it exits non-zero with "failed to connect
to ..." when nothing is listening. agent-viewer MAY start a daemon (`codex app-server daemon
start`, idempotent, 0.5s on this box, and it prints `"status":"started"` so availability is
confirmed by re-probing) and NEVER stops or restarts one - other clients and every other hosted
thread live in that process. `attach` and `stop` probe only, since a daemon that is not up
cannot be hosting anything.

**Superseded (original v1 note, kept for the record).** This spec previously read: "Note the
experimental `codex app-server` JSON-RPC daemon (`thread/subscribe`, `thread/list`,
`command/exec/terminate`) exists and is the eventual 'clean' backend, but v1 does NOT use it:
it is labeled experimental, its control socket is already contended on this box, and it reads
the same SQLite + files we read directly. Leave a comment marking it as the v2 upgrade path."

**Correction.** The contention half of that rejection was wrong. Research decision D-008
(`specs/001-fleet-view-unification/research.md`) traced the claim to a misread log line: the
failure is a **bind** failure from a second would-be *server*, not client contention. `lsof`
shows a single LISTEN holder, and three simultaneous client connections all initialized and
served different requests concurrently, reproduced independently with a stdlib WebSocket client
while other clients were already connected. Concurrent app-server clients are safe. The residual
risk is semantic (two clients steering one live thread), and D-004 forecloses that by never
calling `thread/resume` from the viewer's own client: the join happens in the `codex resume
--remote` TUI the viewer execs into, which is the user's single steering client.

**Current design: agent-viewer binds to the app-server for Codex mutation and attach.**
Enumeration reads `state_*.sqlite` with the read only session index overlay above. Rename remains
`thread/name/set`, which persists an explicit Codex name to `session_index.jsonl`, and attach uses
`codex resume --remote unix://<socketPath> <id>` (D-004).
The transport is RFC6455-framed JSON-RPC over the Unix socket (D-009), discovered
rather than hardcoded: `codex app-server daemon version` prints `socketPath`, and
`"status":"running"` is the availability gate. Verified on this box on 2026-07-26 (output
line-wrapped for readability):

```
$ codex app-server daemon version
{"status":"running",
 "managedCodexPath":"/home/theconnman/.codex/packages/standalone/current/codex",
 "managedCodexVersion":"0.144.4",
 "socketPath":"/home/theconnman/.codex/app-server-control/app-server-control.sock",
 "cliVersion":"0.144.4","appServerVersion":"0.144.4"}
```

The other half of the original caution still stands: the API is marked `[experimental]` with
shallow versioning. Enumeration is always the read only SQLite registry with the session index
overlay, so it remains available when `status` is not `running` or the handshake fails.

**Correction (daemon lifecycle).** This previously read "agent-viewer never starts, stops, or
restarts a daemon". It now MAY start one, because a spawn that lands on no daemon produces a
session nobody can ever join (`codex exec` hosts its app-server in process). It still NEVER
stops and NEVER restarts one: other clients, and every other thread the daemon hosts, live in
that process. Only spawn may start; attach and stop probe. See "Codex attach/resume".

## Model discovery: probe off-thread, cache on disk

Every backend advertises its spawnable models through `available_models()` (default first).
Two of the three discover them by shelling out, and those shell-outs are slow enough to shape
the design. Measured on this box, three consecutive runs: `opencode models` takes 3.72s /
3.81s / 3.82s and prints 378 ids (12,991 bytes); `codex debug models` is comparable; Claude's
list is a `~/.claude.json` read and effectively free.

- **The probe never runs on the render thread.** The composer's key path reads memory only.
  `ModelCache` (TUI) spawns discovery on a worker thread, results drain non-blocking via
  `poll()`, and a backend is probed at most once per viewer session, including when that probe
  found nothing. A probe deadline lost to a slow CLI is silent: the picker degrades to the
  single built-in default with no error, which is exactly the bug this replaced (the old
  3s deadline was under `opencode models`' real 3.8s, so opencode never had a picker).
- **`MODEL_PROBE_TIMEOUT` is 15s.** Generous on purpose: it only bounds a worker thread, and
  losing the race costs a whole catalog.
- **Catalogs persist in the viewer DB** (`model_cache` table: backend, newline-joined ids,
  `fetched_at_ms`), seeded into the cache at startup so the picker is populated from the first
  keystroke on every run after the first. The TTL is a day and lives in the TUI, not the DB;
  a stale list still serves the picker while its refresh runs behind it.
- **A failed probe is not cached.** `available_models()` always seeds the backend's default
  first, so a one-entry result means discovery failed; it is dropped rather than written,
  which would otherwise pin an empty picker for the whole TTL.
- **`run_with_timeout` drains stdout on a reader thread.** Reading after the wait deadlocks
  against the 64KB pipe buffer for any catalog bigger than that, which presents identically to
  a timeout (empty picker, no error).

## Claude mutations — CLI subcommands, plus one deliberate state-file write

Spawn is `claude --bg`, attach is `claude attach <short>`, remove is `claude rm <short>`. Rename
is the single exception to "delegate to a CLI subcommand": Claude ships no `rename` subcommand,
so agent-viewer does what Claude's own fleet view does and writes the job's state file.

**Mechanism.** Read `<jobs root>/<short>/state.json`, set `name`, `nameSource: "user"`, and
`updatedAt` (Claude's writer stamps all three), write it back atomically (temp file in the same
dir, then rename over the target). Read-modify-write, never a blind overwrite: that file also
carries `respawnFlags`, `intent`, and the transcript path, and dropping them would break the
job's respawn contract. The jobs root is `$CLAUDE_CONFIG_DIR/jobs` when that is set, else
`~/.claude/jobs`, matching what `claude agents` lists.

**The temp file is created 0600, not chmod'd afterwards.** Claude writes state.json 0600 while
the jobs dir is group/other traversable, so a temp left at the umask default would publish that
job's intent, output, respawn flags, and transcript path — and widening only after the write
still leaves a window another local user can read. Create restricted, write, then match the
target's own mode, then rename.

**Evidence (2026-07-26, claude 2.1.220).** Fleet View's `Ctrl+R` has exactly two branches. For
daemon-backed rows it calls the state writer above; its failure notice in the bundle is "the job
may have been removed or *its state file is unwritable*", and the writer sets exactly
`name`/`nameSource`/`updatedAt`. For interactive rows it writes one line of
`{"type":"control","action":"rename","name":...}` to the session's `messagingSocketPath` from
`~/.claude/sessions/<pid>.json` — dead in this build, because the field that would hold that
path is declared and never assigned, so no session has one and Fleet View's own gate
(`backend !== "daemon" && !sock` returns early) makes rename background-only. Confirmed live:
writing `name` into a finished job's state.json made `claude agents --json --all` report the new
name on the next listing.

**What the rendezvous socket is not.** The per-session rendezvous socket authenticates its FIRST
frame as `attacher-caps` and answers anything else with `{"type":"reply-rejected"}`; merely
opening it evicts the daemon's supervisor connection for that live session. The
`{"subtype":"rename_session"}` frame agent-viewer once sent there belongs to a third, unrelated
protocol — the SDK/bridge control-request schema carried on the subprocess-stdin transport and
the remote claude.ai bridge. It was never going to be understood by that socket.

**Consequences.** `rename` is advertised backend-wide but gated per row on the short id
(`capabilities_for`), since an interactive row has no job dir. Racing a live worker's own state
write is accepted: the worker re-reads the file immediately before each write, so it merges the
new name rather than reverting it, which is the same race Claude's own fleet view runs. No
viewer-local name override is involved, so the list can never disagree with `claude agents`.

## Auto spawn — `agent-router`, and why it is not a backend

The composer's fourth selector entry, `auto`, delegates the provider choice to the
`agent-router` CLI: `agent-router run --json --dir <target> --provider auto -- "<task>"`, parsed into a
`RouterOutcome` by `core/router.rs`.

- **Auto is deliberately NOT a `Backend`.** It enumerates nothing, owns no sessions, advertises
  no capabilities, and never appears in `all_backends()`. It exists only in the spawn flow, so
  the routed job shows up through the winning backend's normal listing path and is selected by
  the existing `SpawnSelection` mechanism (the router's exact returned id when available,
  otherwise its exact returned job name, then bounded cwd and invocation-interval matching while
  excluding that provider's preexisting ids).
- **The entry is capability-gated on the binary**, resolved once at startup with a PATH lookup
  (`router::available()`), matching the backends-appear-when-present posture: no router means no
  entry, which is not an error state.
- **No model is passed.** The router owns model and reasoning-effort selection, so the picker
  offers a single `auto` entry and the CLI is invoked without `--model`.
- **Every router failure is a footer error with nothing spawned** — missing binary, non-zero
  exit (carrying its stderr), timeout, or unreadable JSON, including a provider name the viewer
  does not know. There is deliberately no fallback to a hardcoded provider: a decision the
  viewer cannot read must not become a silent spawn somewhere.
- It runs on the mutation worker like every other spawn, since a routed dispatch pays for a
  classifier call plus the winning backend's own spawn.

## Crate layout

- `agent-viewer-core` (lib): registry reader (rusqlite), rollout parser (serde_json),
  status resolver (sysinfo/procfs + `/proc/fd`), spawner (Command+setsid), and thin wrappers
  around `codex archive`/`unarchive`/`resume`. No UI. Unit-tested.
- `agent-viewer-tui` (bin): `ratatui` + `crossterm` over `-core`.

Cargo workspace. Live-refresh the registry every ~1-2s.

## Release artifacts

A pushed `v*` tag builds native binaries on GitHub hosted runners and publishes a GitHub Release.
The release contains these four archives, each with a sibling `.sha256` file:

- `agent-viewer-x86_64-unknown-linux-gnu.tar.gz`
- `agent-viewer-x86_64-apple-darwin.tar.gz`
- `agent-viewer-aarch64-apple-darwin.tar.gz`
- `agent-viewer-x86_64-pc-windows-msvc.zip`

The archive contains `agent-viewer` on Unix and `agent-viewer.exe` on Windows. Build jobs run
`agent-viewer --version` or `agent-viewer.exe --version` before packaging. `--version` and `-V`
must print the package version and exit before terminal, filesystem, or backend startup, so they
remain safe cross platform smoke paths.

## TUI behavior

- Single list: sessions grouped by project or state, with status glyphs based on
  opencode-monitor's vocabulary (spinner=running, green=done, gray=hidden, red=errored).
  Project groups remain alphabetic and their members order by `created_at_ms` ascending.
  State sections remain fixed and their members order by `created_at_ms` ascending.
  After sanitization, the exact whole title `hold` is matched without regard to ASCII letter
  case and omitted only when the TUI emits a session row. Thus `hold`, `Hold`, and `HOLD` are
  omitted, while whitespace and substring variants remain visible. Project headers count
  rendered non hold sessions, so a project containing only matching sessions remains as a header
  with count zero. State section counts include only rendered rows, so a section containing
  only matching sessions is absent. `Ctrl+K` quickswitcher session entries use the same visible
  row model, so matching sessions are omitted while ordinary sessions and independent
  quickswitcher actions remain. Enumeration, backend data, and mutation behavior do not change.
- Keys: `Enter` attach/resume when the composer is empty, or spawn a composed task; bare letters
  and numbers type into the composer, including when empty, and `/` composes; `Ctrl+D` hide
  (archive); `Ctrl+U` unhide; `Ctrl+A` toggle show-hidden; `space` toggles a selected group
  header; arrows navigate; `Ctrl+F` filters; `Ctrl+C` quit. Keep the set small and obvious.

### Mouse capture must be escapable (`Ctrl+T`)

The viewer enables mouse capture at startup for list click and hover row selection. Every
successful Codex or Claude attach or reattach requests capture on, so those transcripts scroll
immediately. While attached, `Ctrl+T` requests capture off for host terminal text selection.
Press it again to restore session scrolling. Codex wheel reports move the viewer's local PTY
viewport by three rows, using its bounded 2,000 row scrollback. Claude attached terminals receive
native wheel forwarding. External opencode attaches with capture off for host selection and
requires `Ctrl+T` to opt into native wheel forwarding. Detaching requests capture on to restore
list mouse controls. External opencode behavior is described as its supported attach path, not as
a live verification claim.

The cost is that **capture swallows the terminal's own drag-select**, so text cannot be copied
out of the viewer. This spec previously waved that away with "the terminal's native text
selection still works with Shift held in most terminals" — "most" is the bug. That override is
a per-terminal convention, not a protocol guarantee, and where it is absent the content on
screen is simply uncopyable.

`Ctrl+T` toggles mouse reporting at runtime, sending the real
`DisableMouseCapture`/`EnableMouseCapture` sequences and flipping `Ui::mouse_capture`. Rules:
- The chord is claimed in **every** mode, attach included. The attached transcript is the
  surface users most want to copy out of, so the child does not receive `Ctrl+T` (the same
  deliberate theft as `Ctrl+]` for detach).
- `handle_mouse` early-returns while capture is off, so a report still in flight — or one from
  a terminal that ignored the disable sequence — cannot steer the selection.
- Each toggle sets a footer notice naming scrolling or selection and the way back, because the
  state is otherwise invisible.
- List capture starts **on**. Every successful Codex or Claude attach or reattach requests
  capture **on**. External opencode attach requests capture **off** until `Ctrl+T` opts into
  scrolling. Attached `Ctrl+T` toggles between selection and scrolling. Every detach requests
  capture **on**.

## Testing

- `-core` unit tests with fixtures: a sample `session_meta` first line, a `task_complete`
  tail, an archived vs active row, the serialized-`source` variants. Test the status resolver
  and the `source` parser against these. Mock only the filesystem inputs (fixture files); do
  NOT mock rusqlite or the parser — those are the things under test. Follow the repo's
  test-quality gate (a test must fail if you replace the resolver body with a stub).
- Integration: an end-to-end test/script that spawns a real short `codex exec`, asserts it is
  enumerated and shows running→done, then archives it and asserts it moves to hidden.

## Done when

1. `cargo build --workspace` and `cargo clippy --workspace` are clean; `cargo test --workspace` passes.
2. The TUI launches on this box and lists real sessions from `state_*.sqlite`, grouped by project,
   with correct hidden/visible split.
3. Composing a task and pressing `Enter` spawns a background `codex exec` that appears in the
   list within ~2s.
4. That spawned session shows **running** (proving the `/proc/fd` correlation works live), then
   **done** after it finishes — demonstrated with actual output, not asserted in prose.
5. `Ctrl+D`/`Ctrl+U` archive/unarchive a session and it moves between visible and hidden views.
6. README documents how to build and run.

Report at completion with: the exact commands run and their output for done-when items 1, 3, and 4
(the live status transition is the load-bearing proof).
