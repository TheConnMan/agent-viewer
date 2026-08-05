# agent-viewer — build spec

Terminal viewer for OpenAI Codex and Claude Code sessions, in the spirit of Claude Code's
`claude agents`.
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
  render sessions but do not claim Linux process status or Codex daemon controls.
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
  `git_branch TEXT`, `preview TEXT`. Order by `updated_at_ms ASC`, retaining the existing
  `id DESC` tie break. The remaining schema columns (`git_origin_url`, `first_user_message`,
  `thread_source`, `agent_nickname`, `agent_role`) are not loaded.
- **`parent_thread_id` is derived, not selected.** No such column exists. The reader parses the
  raw `source` text as JSON and reads `subagent.thread_spawn.parent_thread_id`, trimmed and
  dropped when empty (`spawn_parent_thread_id`). That derived value is what links a subagent row
  to its parent for the activity ribbon's recursive subtree.
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
relist their authoritative source.

## Removed backends

opencode support was removed before 1.0 by owner decision: the viewer ships as Codex plus
Claude Code, and a cleaner integration can be built later. The managed-server design
(credential pinning, the same-stream authorization rule, and the measured Linux
`TCP_DEFER_ACCEPT` evidence behind it) lives in git history at commit `30ed871`, the last one
carrying the "Enumeration and runtime: opencode" section.

## Enumeration — claude, and the nested `claude -p` companion rule

Rows come from `claude agents --json --all`. A non-zero exit or a missing binary is a quiet
empty backend, never an error. Each row is enriched from `<jobs root>/<short>/state.json` for
summary, transcript path, PR refs, and `updated_at`. For activity, `linkScanPath` is
authoritative when present. Otherwise, resolve an existing canonical projects transcript from
the row `cwd` and `sessionId` under the same config root.

**`state` is a self-report and it goes stale; `tempo` is the liveness field.** The agents row's
`state` is written when a turn ends and records what the session believed it was doing, not
whether it is running. A session that ended a turn saying "6 bg threads spawned; awaiting PRs"
is recorded `working` and stays `working` indefinitely, because nothing rewrites the label until
another turn runs. Taking it verbatim put a finished overnight campaign in the working group for
over two hours with no process behind it. The row's `pid` does not rescue this: the stale row
still published one, and it pointed at a `claude bg-spare` PTY host, not a live turn.

`state.json` carries `tempo`, which the agents output does not project, and that is the field
that tracks execution. So `state == "working" && tempo == "idle"` demotes the row to `Idle`
during the same state.json read that fills summary and PR refs. `Idle` is still a LIVE state,
so the row keeps stop and attach and only leaves the working group.

Measured on this box 2026-07-31 across all 21 live jobs, `state`/`tempo` pairs: 12 done+idle,
3 stopped+idle, 2 done+active, 1 blocked+active, 1 working+active, 2 working+idle. The single
working+active job had rewritten its state.json 90 seconds earlier; both working+idle jobs had
been quiet for over an hour.

**The stale label does not time out or self-heal.** The specimen job (`8fe26ef1`) held
`working` from 11:40:20Z, when its last turn ended, until 13:55:52Z. What cleared it was the
user sending that session a new message, which ran a turn and rewrote the label to `done` at
13:56:25Z. 2h15m stale, ended by a human poke. That is the whole problem: the label corrects
itself exactly when someone is already interacting with the session and no longer needs the
viewer to tell them whether it is alive.

A bare `inFlight.tasks` count was evaluated and
**rejected** as part of the rule: it counts subagent tasks, so the genuinely running job read
`tasks: 0` while a finished one read `tasks: 3`, which is backwards as a liveness signal.
An absent `tempo` is absent evidence and never demotes.

**A running shell suppresses the demotion.** `tempo` tracks model turns, not the child
processes a turn started, so a session parked on a long `Bash` call (a suite, a build, an
`until ... done` watcher) runs no turn for as long as it waits and reads working+idle exactly
like the stale pair. Measured 2026-08-01: of five live jobs, the one working+idle job held
`inFlight: {tasks: 8, kinds: ["local_bash"]}` with eight real shell processes behind it (one
of them an `until ! pgrep -f pytest; do sleep 30; done` watcher, pid 3088578), and the viewer
showed it Idle while it was the busiest session on the box.

The signal is `inFlight.kinds` containing `local_bash` **with a non-zero `tasks`**. The KIND
is what makes `inFlight` usable here where the raw count was not: `local_agent` entries stay
excluded, so the rejected subagent evidence above is unaffected. Both halves are required
because the sibling `fan` array was observed holding an `agent` entry against `tasks: 0` on a
job that had moved on, so a kind name with no count behind it is a stale label, not evidence.
`fan[].kind == "shell"` is therefore NOT the signal, even though it lists the same shells.

`claude agents --json` publishes `status` on live rows, but it read `busy` for all five live
jobs including the truly quiet ones, so it does not discriminate and is not part of the rule.

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
to "real session", the same safe direction as the codex rule, and the same two escapes keep
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
subtrees through sibling `.meta.json` `parentAgentId` links. A child row remains isolated to
its own subtree. Missing
or malformed child data is best effort and never removes readable root activity. The hierarchy
cache rereads every thirty seconds.

- Message content: `type:"response_item"`, `payload.role`, `payload.content[].text`
  (assistant text is `content[].type == "output_text"`).
- Terminal marker: an `event_msg` whose `payload.type` is `task_complete` **or**
  `turn_aborted`. Both end a turn and neither is followed by a `task_started`, so scoring only
  `task_complete` left every interrupted session reading as mid-turn forever: a turn stopped by
  `turn/interrupt` (or `Esc` in a TUI) writes `turn_aborted` and nothing after it.
- Approval marker: any `event_msg` whose `payload.type` **ends with** `_approval_request`
  (`exec_approval_request`, `apply_patch_approval_request`). The suffix match rather than a
  fixed list is deliberate, so a future approval variant classifies correctly without a code
  change. An approval that is the most recent event after the last `task_started`, with no
  terminal marker behind it, is `TailState::AwaitingApproval`.
- The status classifier reads a 64 KiB tail window. The tail *pane* reads a wider one; see
  "Bounded transcript tails".

## PR refs — codex reads them out of the transcript

Claude records its PRs in `jobs/<short>/state.json` (`children[]` where `kind == "pr"`). Codex
records them nowhere: the registry has no PR column, and `threads.git_branch` is captured when
the thread starts, so it is stale the moment the agent branches (measured: the thread that
opened `example-org/example-repo/pull/1089` still reports `task/fix-interactive-clippy`). Branch lookups
are therefore not a usable source. The rollout transcript is the source only when it proves that
the same thread successfully opened the PR.

`codex::pr_scan` records a pending `response_item` custom tool call only when its name is `exec`,
its input contains `gh pr create`, and it has a `call_id`. It badges a PR only after a later
`custom_tool_call_output` with that same `call_id` contains a nested command result with
`exit_code == 0` and a `github.com/<owner>/<repo>/pull/<n>` URL. An unpaired or failed command
does not badge a PR. URLs in messages, unrelated tool output, or other calls are incidental and
are excluded. Cost rules, all load bearing at this box's scale (4,963 threads, 1.8 GB of
rollouts, of which 1.3 GB is `archived_sessions`):

- **Per file offset and pairing state.** A rollout is read once, then only where it grew, and
  not at all while its length is unchanged. Pending calls survive incremental reads so a command
  and its result can arrive on different ticks. Without this, `list` would re-read gigabytes
  every second.
- **Complete lines only.** Rollouts are appended live, so the trailing partial line is left for
  the next tick. Parsing it would mint a truncated number (`pull/10` for an in-flight `1089`),
  and refs are sticky, so that badge would never heal.
- **A shared per-tick byte budget** (`SCAN_BUDGET_BYTES`), spent live-newest-first and archived
  last. Measured cold on this box: every visible codex row that has a PR is badged within ~14
  ticks, the newest within one or two, and the archive trickles in behind it. There is no
  on-disk cache, so this repeats once per launch.
- **`MAX_REFS_PER_SESSION`**, keeping the most recent successful refs. Each ref costs a live
  `gh` fetch in the TUI's status cache, and one real batch creation rollout successfully opened
  115 distinct PRs.

## Status detection — TWO signals, both required

The file signal alone is insufficient: during research 66/383 rollouts lacked `task_complete`
yet **zero** `codex` processes were running, i.e. those are crashed/abandoned, not live.

Resolve per thread on Linux. The fd signal is three-valued, not a boolean: `Held(owner)` when a
live `codex`/`codex exec` PID holds this thread's `rollout_path` open (enumerate PIDs with
`sysinfo`, read `/proc/<pid>/fd/*`, match an open path to `threads.rollout_path`), `Closed` when
the scan ran and nobody holds it, and `Unavailable` when the scan could not run at all. Crossed
with the tail, `status_from` produces six outcomes for a row the daemon does not host:

| fd signal | tail | status |
| --- | --- | --- |
| Held | AwaitingApproval | NeedsInput (`awaiting approval`) |
| Held | Complete | Idle |
| Held | MidTurn, or unreadable | Working |
| Closed | Complete | Done |
| Closed | anything else | Error |
| Unavailable | anything | Unknown (Done if the tail proves Complete) |

`Unavailable` is the non-Linux and failed-scan case, and it is why the resolver never infers a
false Idle: absent process evidence resolves to Unknown, never to a live state. A daemon-hosted
row takes a different rule set entirely, because its fd carries no liveness information; see
"Codex attach/resume". `archived=1` is the hidden set and is orthogonal, applying on top of any
of these.

**Stop is gated on the same scan, and subagent rows are withheld.** `stop_route` is a pure
function: a daemon-hosted row routes to `turn/interrupt`, a subagent row returns `Unsupported`,
and everything else signals its pid or, with no pid, is `Unsupported` too. Subagent-ness is the
`subagent` field the listing records from the parsed `source` enum, never `companion`. It was
`companion && origin != Exec` and that was wrong: `companion` is presentational and
`mark_dead_dirs` sets it on every session whose cwd has been deleted, so an ordinary cli or
vscode session in a removed worktree was reclassified as a subagent and lost its stop action.
The subagent case is the same hazard as the daemon one, one level
down: a `codex exec` parent holds the rollout fd of every subagent thread it spawned (measured
live 2026-07-27, pid 2910115 held two), so the fd scan stamps the PARENT's pid onto the
subagent rows. Signalling from there SIGTERMs the parent's whole process group to stop one
child, and there is no separate process to signal instead, so the row advertises no stop at all
and the footer reads `codex does not support stop`.

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
- **Hide (req 3):** `codex archive <id>`; **unhide:** `codex unarchive <id>`.
- **Remove is archive, deliberately.** The optional `codex delete <id>` hard-delete this spec
  once contemplated was not built: `CodexBackend::remove` calls `cli::archive`, the same
  `codex archive <id>` that `Ctrl+D` runs. So the second `Ctrl+X` press on a Codex row hides it
  rather than destroying its transcript, and `Ctrl+U` brings it back. Claude's `remove` is a
  real delete (`claude rm <short>`, which also removes the job's worktree), so the two backends
  genuinely differ here and the row's capabilities, not a shared assumption, decide what a
  remove costs.
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
exists", which is NOT the same as "the user is looking at it". These now line up much more
closely than they used to: the viewer closes a session's PTY when it leaves the screen, so the
child goes away with the view rather than lingering. The gap that remains is only the teardown
window, plus a child that outlives its kill.

This replaced a deliberate retain-on-detach: the PTY used to be kept alive so re-attaching was
instant, which left rows reading Idle long after the user had stopped looking at them. The cost
of closing is a one-to-three second reconnect when a session is opened again; the benefit is
that "connected" means "on screen", which is both what the status resolver assumes and what the
user expects. The video wall depends on this too: it opens a connection per tile, and a wall
that leaked those on close would strand up to nine agent processes per visit.

One consequence worth naming, since the wall now tiles sessions for fifteen minutes after they
stop: connecting to a Complete Codex thread makes `attached` true for it, which is exactly the
condition that reads Done as Idle. A tiled Done row therefore shows Idle while the wall holds
it, which is not a lie — the resume process really does exist — and it cannot pin the row open
forever, because the recency window is measured against `updated_at_ms`, which the connection
does not touch. At minute fifteen the tile drops and the connection closes with it.

That last clause is load-bearing and was not free: a tile leaving the wall has to be pruned
explicitly (`prune_wall_tiles`, run each frame before the join pass). Without it the expired
session's key stays in `wall.requested` and its child stays in `attached` until the whole wall
closes — invisible, and steadily pushing the live-process count past `MAX_TILES` as expired
slots are refilled. Dropping the key from `requested` also invalidates any join still in
flight, since `install_wall_join` bails on `!wall.owns(&key)` before it spawns.

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

**Correction.** The contention half of that rejection was wrong. Live investigation traced the
claim to a misread log line: the
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
 "managedCodexPath":"/home/example/.codex/packages/standalone/current/codex",
 "managedCodexVersion":"0.144.4",
 "socketPath":"/home/example/.codex/app-server-control/app-server-control.sock",
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
Codex discovers them by shelling out, and that shell-out is slow enough to shape the design.
Measured on this box, `codex debug models` takes seconds on a cold run; Claude's list is a
`~/.claude.json` read and effectively free.

- **The probe never runs on the render thread.** The composer's key path reads memory only.
  `ModelCache` (TUI) spawns discovery on a worker thread, results drain non-blocking via
  `poll()`, and a backend is probed at most once per viewer session, including when that probe
  found nothing. A probe deadline lost to a slow CLI is silent: the picker degrades to the
  single built-in default with no error, which is exactly the bug this replaced (the old
  3s deadline was under a real cold probe's 3.8s, so that backend never had a picker).
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

Spawn is `claude --bg`, attach is `claude attach <short>`, stop is `claude stop <short>` for
nonterminal background rows, and remove is `claude rm <short>`. Stop retains the
conversation for later attach. A second press queues removal after stop succeeds. If stop fails,
removal is discarded and confirmation is cleared, so the next press retries stop. Rename is the
single exception to "delegate to a CLI subcommand": Claude ships no `rename` subcommand, so
agent-viewer does what Claude's own fleet view does and writes the job's state file.

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
(`capabilities_for`), since an interactive row has no job dir. `stop` is likewise capability
gated to nonterminal background rows. Racing a live worker's own state write is
accepted: the worker re-reads the file immediately before each write, so it merges the new name
rather than reverting it, which is the same race Claude's own fleet view runs. No viewer-local
name override is involved, so the list can never disagree with `claude agents`.

## Auto spawn — `agent-router`, and why it is not a backend

The composer's fourth selector entry, `auto`, delegates the provider choice to the
`agent-router` CLI: `agent-router run --json --dir <target> --provider auto -- "<task>"`, parsed into a
`RouterOutcome` by `core/router.rs`.

Concrete composer providers are discovered once from `PATH` at startup. The Tab cycle, model
palette, model cache seed, and model probes include only providers with an installed executable.

- **Auto is deliberately NOT a `Backend`.** It enumerates nothing, owns no sessions, advertises
  no capabilities, and never appears in `all_backends()`. It exists only in the spawn flow, so
  the routed job shows up through the winning backend's normal listing path and is selected by
  the existing `SpawnSelection` mechanism (the router's exact returned id when available,
  otherwise its exact returned job name, then bounded cwd and invocation-interval matching while
  excluding that provider's preexisting ids).
- **The entry is capability-gated on the binary**, resolved once at startup with a PATH lookup
  (`router::available()`), matching the backends-appear-when-present posture: no router means no
  entry, which is not an error state.
- **When the router is present, Auto is the composer's STARTING selection**
  (`Composer::default_to_auto`, called once at startup after the availability probe): routed
  spawns are the default posture, and one `Tab` reaches the concrete backends. Without the
  router the composer starts on the first installed concrete backend.
- **No model is passed.** The router owns model and reasoning-effort selection, so the picker
  offers a single `auto` entry and the CLI is invoked without `--model`.
- **Every router failure is a footer error with nothing spawned** — missing binary, non-zero
  exit (carrying its stderr), timeout, or unreadable JSON, including a provider name the viewer
  does not know. There is deliberately no fallback to a hardcoded provider: a decision the
  viewer cannot read must not become a silent spawn somewhere.
- It runs on the mutation worker like every other spawn, since a routed dispatch pays for a
  classifier call plus the winning backend's own spawn.

## Crate layout

Cargo workspace, two crates plus one vendored dependency. Live-refresh the registry every ~1-2s.

`agent-viewer-core` (lib), no UI, unit-tested:

- `backend.rs` holds the `Backend` trait, `BackendKind`, `Session`, `Status`, `Capabilities`,
  and the listing-cache scope: the whole cross-backend contract.
- `codex/` is the Codex backend, split by concern. `registry.rs` is the read-only
  `state_*.sqlite` reader; `rollout.rs` does JSONL tail parsing, `TailState`, pending approvals,
  and activity timestamps; `source.rs` parses the serialized `source` enum; `status.rs` owns the
  procfs sweep, `FdSignal`, `RolloutOwner`, and the six-state resolver; `pr_scan.rs` scans PR
  refs out of rollouts; `cli.rs` wraps the `codex archive`/`unarchive`/`resume` shell-outs;
  `app_server.rs` is the JSON-RPC client for the `codex app-server` daemon (discovery, the pure
  request builders, and the blocking WebSocket exchange that starts and interrupts threads); and
  `mod.rs` carries the `Backend` impl plus the three pure routing seams (`spawn_route`,
  `attach_route`, `stop_route`).
- `claude.rs` is the Claude backend: `claude agents --json --all`, `state.json` enrichment, the
  trust bootstrap, and spawn/attach/stop/remove/rename.
- `router.rs` is the `agent-router` shell-out behind the composer's Auto entry. Deliberately not
  a `Backend`: it enumerates nothing and owns no sessions.
- `pr_status.rs` parses a GitHub PR href, fetches its live state via `gh`, and maps that to a
  badge color.
- `pty.rs` is the embedded-attach engine: a real PTY plus child plus vt100 parser, with the
  bounded scrollback and the palette replies.
- `spawn.rs` owns detached spawn, the reaper, and the shell-out wrappers (`run_checked`,
  `run_with_timeout`).
- `state.rs` is the viewer-owned SQLite (the only database this tool writes) plus the pure
  overlay and spawn-matching functions.
- `platform.rs` carries the `Platform` enum and the cross-platform home resolution that gates
  every Linux-only claim.
- `group.rs` folds a cwd to its project root, `lib.rs` holds `mark_dead_dirs`, `open_readonly`,
  `home_dir`, and the bounded tail reader, and `error.rs` holds `Error` and `AttachRefusal`.

`agent-viewer-tui` (bin): `ratatui` + `crossterm` over `-core`. `main.rs` owns the run loop and
the refresh tick; `app.rs` the row model and grouping; `keys.rs` the per-mode key routing;
`actions.rs` what those keys do; `composer.rs`, `attach.rs`, and `mouse.rs` the input encodings;
`ops.rs` and `mutations.rs` the background mutation worker; `model_cache.rs`, `pr_cache.rs`, and
`shared_listing.rs` the off-thread caches; `logos.rs` the inline brand marks; `terminal_title.rs`
the tab title. Rendering lives under `ui/`: `list.rs`, `header.rs`, `composer.rs`, `overlay.rs`,
`palette.rs`, `wall.rs`, `triage.rs`, `tail.rs`, `theme.rs`, `sprite.rs`, `age.rs`.

`vendor/vt100` is a patched fork of the published crate; see "Vendored vt100".

## Vendored vt100

`vendor/vt100` is vt100 0.16.2 with exactly one behavioral change, wired in by
`[patch.crates-io] vt100 = { path = "vendor/vt100" }`. In `Grid::scroll_up`, the guard deciding
whether a scrolled-off row enters the scrollback was `!self.scroll_region_active()`; it is now
`self.scroll_top == 0`. The now-unused helper is deleted, and nothing else in the crate differs
from the published source.

The bug it fixes: Codex sets a **top-anchored** scroll region (`scroll_top == 0`,
`scroll_bottom` short of the last row) to pin its own status area. Upstream treats any region at
all as "not the main screen" and drops every row that scrolls out of it, so the viewer's PTY
scrollback stayed empty for the entire life of a Codex attach and the wheel had nothing to
scroll back through. Anchoring the test at the top instead keeps history for a region that
starts at row 0, which is the case where the rows leaving the region really are leaving the
screen, and still discards it for a region that starts lower down, where they are not.

Both candidate upstreams carry the bug: the published vt100 0.16.2 and the `vt100-ctt` 0.17.1
fork both still read `!self.scroll_region_active()`. Coverage is the four viewport tests in
`crates/agent-viewer-core/tests/pty_tests.rs`
(`pty_viewport_follows_live_output_and_preserves_a_scrolled_view`,
`pty_codex_top_anchored_scroll_region_preserves_global_history`,
`pty_scroll_region_below_top_does_not_create_global_history`, and
`pty_viewport_retains_exactly_two_thousand_history_rows`); the third is the one that proves the
patch did not simply disable the check. The exit condition is an upstream release that fixes
restricted-region scrolling, at which point the `[patch.crates-io]` entry and the whole
`vendor/` tree can be dropped. Until then a source build pulls the repo rather than the
published crate, which is why `cargo install` targets the git URL.

## Runtime mechanics

### Viewer state, and why the listing payload lives in its own table

`state.rs` owns the only SQLite this tool writes: `spawned` (the spawn records that keep a
viewer-started session from being filtered away as a companion), `collapsed_groups` (which also
carries the persisted theme under a `theme:` key), `settings` (sprite, age ramp), `model_cache`,
and the two listing-cache tables.

Those are two tables on purpose. The listing payload used to sit in `backend_listing_cache`
beside the lease columns, and that is not free: SQLite rewrites a row's entire record, overflow
pages included, whenever the record's size changes, so every lease take, renewal, release, and
invalidation dragged the whole snapshot back through the write-ahead log. Measured on this box
with a 24 MB snapshot, one metadata-only UPDATE cost **48 MB of WAL, against 4 KB** once the
payload moved into `backend_listing_snapshot`. The legacy column is cleared rather than dropped,
so an older viewer binary running against the same file sees a cache miss and refreshes instead
of erroring on a column that vanished.

### Bounded transcript tails

`TRANSCRIPT_TAIL_BYTES` is 512 KiB: the most a tail-pane read may touch at the end of a JSONL
transcript. Transcripts grow without bound while a session works (18.6 MB measured on this box)
and the tail pane refreshes every 2s tick, so reading whole files re-parsed megabytes per tick
to display twelve events. The window is deliberately far wider than the 64 KiB the status
classifier works over, because a single transcript line can be an `apply_patch` call carrying a
whole diff: a window that landed inside one such line would find no complete line at all and
show an empty pane.

### Registry rollover

`find_state_db` is re-globbed on **every** listing tick, not resolved once at startup. It is the
only thing that notices a Codex upgrade laying down a new `state_N+1.sqlite`: the previously
opened file stays open, stays readable, and keeps answering with its frozen row set, so a
registry resolved once showed no new session until the viewer was restarted. The cost is one
`readdir` of `~/.codex` per tick, against a scan that already reads thousands of rollout tails.
The open connection is still cached and reused for as long as the winning path is unchanged.

### Detached-child reaping

`setsid` detaches the SESSION, not the parent-child relationship. This process stays the child's
parent, so dropping the `Child` never collects its exit status and the kernel holds a zombie for
the whole life of the viewer (two accumulated in 22 minutes of ordinary use). Every detached
spawn therefore hands its `Child` to a thread that blocks in `wait()`. The thread costs one
stack and lives exactly as long as the child.

### PR badge fetch

A badge's live color comes from `gh pr view <url> --json state,mergedAt,isDraft,reviewDecision,
reviewRequests,statusCheckRollup`, run under a 10s deadline (`PR_STATUS_TIMEOUT`); any failure,
including a missing `gh`, degrades to the flat default color rather than erroring. `pr_status.rs`
itself is a pure fetch-and-map with no cache. The cache is one layer up in the TUI
(`PrStatusCache`, modeled on the mutation runner): each resolvable PR key gets a fetch on a
worker thread, keys already in flight are never re-spawned, results drain non-blocking, the
render path reads cached colors purely, and an entry re-fetches once past its 30s TTL. Nothing
here is persisted, so a fresh viewer fills its badges in over its first few seconds.

### Dead-cwd marking

`mark_dead_dirs` flags any session whose cwd is a non-empty path that no longer exists on disk
as a companion, so the default view hides deleted-directory noise (a `/tmp` scratch dir, a
removed worktree). It only ever SETS `companion`: an already-flagged session, and one with a
live or empty cwd, are left untouched, so it can never un-hide something another rule hid.
`Ctrl+A` still surfaces these rows.

### Claude trust bootstrap

A `claude` launched into a project it has not seen stalls on the trust prompt, which inside an
embedded PTY reads as an attach that simply hangs. So on the one attach path that starts a fresh
process in the session's own directory, the Claude fallback (`claude -r`, taken by a row with no
background job id), the viewer first merges `projects.<cwd>.hasTrustDialogAccepted = true` into
`~/.claude.json`. This is Claude's own sanctioned field: its error text instructs setting exactly
that key for non-interactive use. The write is a read-modify-write preserving every other key at
every level, atomic (temp file in the same directory, then rename), and skipped entirely when the
cwd or any ancestor is already accepted. A config that is not a JSON object is an error, never an
overwrite, and the whole call is best-effort: a failure to pre-accept never blocks the attach.

## Release artifacts

A pushed `v*` tag builds native binaries on GitHub hosted runners and publishes a GitHub Release.
`README.md` lists the exact archive names and the verification commands; that list is not
repeated here.

The requirement this spec owns is the smoke path. Every build job runs `agent-viewer --version`
(`agent-viewer.exe --version` on Windows) before packaging, so `--version` and `-V` must print
the package version and exit **before** terminal, filesystem, or backend startup. A version flag
that touched any of those would fail on a runner with no tty and no `~/.codex`, and would take
the release with it.

## TUI behavior

- Single list: sessions grouped by project or state, with status glyphs (spinner=running,
  green=done, gray=hidden, red=errored).
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
  (archive); `Ctrl+U` unhide; `Ctrl+A` toggle show-hidden; `Space` or `Enter` toggles a selected
  group header; arrows navigate; `Ctrl+F` filters; `Ctrl+C` quit. Keep the set small and obvious.
  The full shipped set, including the surfaces below, is the in-app `?` help and `README.md`.

### Mouse capture must be escapable (`Ctrl+T`)

The viewer enables mouse capture at startup for list click and hover row selection. Every
successful Codex or Claude attach or reattach requests capture on, so those transcripts scroll
immediately. While attached, `Ctrl+T` requests capture off for host terminal text selection.
Press it again to restore session scrolling. Codex wheel reports move the viewer's local PTY
viewport by three rows, using its bounded 2,000 row scrollback. Claude attached terminals receive
native wheel forwarding. Detaching requests capture on to restore list mouse controls.

The cost is that **capture swallows the terminal's own drag-select**. This spec previously
waved that away with "the terminal's native text selection still works with Shift held in most
terminals". That override is a per-terminal convention, not a protocol guarantee. `Ctrl+T`
therefore remains the universal fallback for host terminal selection.

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
  capture **on**. Attached `Ctrl+T` toggles between selection and scrolling. Every detach
  requests capture **on**.

If a mouse mode transition write fails, the viewer writes the complete prior mouse mode as a
rollback. A successful rollback restores `Ui::mouse_capture` to the prior applied value and
reports that it was restored. If rollback also fails, the UI retains its last known value but
reports that the terminal mouse state is unknown. Both paths clear stale mouse press state.
Guidance mentions `Ctrl+Y` only while attached; list and modal guidance mention `Ctrl+T`.

### Remote attached viewport copy (`Ctrl+Y`)

The confirmed deployment boundary is a Linux `agent-viewer` process displayed through Windows
Terminal over SSH. A process local clipboard API writes on the Linux host, which is the wrong
machine. Such APIs are prohibited for this deployment. While attached, `Ctrl+Y` instead writes
one Microsoft compatible OSC 52 request to the outer terminal:

```text
ESC ] 52 ; ; BASE64(UTF8(screen.contents())) BEL
```

The empty selector addresses the default clipboard, standard base64 encodes the exact UTF8
bytes, and BEL terminates the frame. The request uses `screen.contents()` without trimming,
truncation, or an arbitrary payload cap. It therefore represents exactly the current visible
vt100 viewport. If the user has scrolled, it represents the visible historical viewport, not
the live bottom or content outside the viewport.

Attached PTY sizing reserves exactly two chrome rows. On initial attach, retained reattach, and
resize, the PTY width matches the terminal width and its height is the terminal height minus
two, with a minimum of one row. An exited retained PTY keeps its final screen, and `Ctrl+Y`
remains available for that visible content.

`Ctrl+Y` is claimed only in attached mode before ordinary child forwarding. It never reaches
the child, changes mouse capture, changes scrolling, or alters `Ctrl+C` child interrupt
delivery. Mouse wheel behavior and the `Ctrl+T` host selection fallback remain unchanged.

OSC 52 has no acknowledgement. A complete frame write and flush reports only
`copy request sent to terminal`; it never claims that the clipboard changed. On any write or
flush failure, the viewer makes a best effort to write `ESC \` to terminate a possibly partial
OSC sequence and reports that the terminal clipboard state is unknown. A partial frame may
already have reached the client, so failure never reports copied or sent.

Windows Terminal supports this remote clipboard path, but terminal policy or implementation
limits may reject the request or limit its payload. The viewer cannot detect that rejection.
Only clipboard readback on the client proves mutation.

OSC 52 clipboard reads, terminal multiplexer passthrough wrapping, forwarding OSC 52 emitted by
the child, and copying content beyond the visible viewport are out of scope.

### Video wall (`Ctrl+W`)

The list is replaced by a grid of live PTY tiles, and the focused tile takes the keyboard. This
is a real attach, not transcript rendering: the wall connects each session through the same
attach path a manual attach uses, and closes every one of those connections when it closes. A
session earns a tile while it is working or awaiting input and keeps it for `RECENT_MS`
(fifteen minutes) after it stops, because that moment is exactly when the wall is most useful:
the reply you want to type is the one that follows the result you watched arrive. `MAX_TILES` is
9, and it is a process budget rather than a rendering one, since each tile is a live child being
resized and re-parsed; above it the footer carries the overflow count. All eligible sessions are
ordered by creation time ascending, with backend then ID as deterministic ties; the earliest take
the slots. Status changes never alter a tile's position. `Ctrl+O` zooms the focused tile to the
full attach view, reusing the connection the wall already holds rather than spawning a second child.

An expired tile has to be pruned explicitly (`prune_wall_tiles`, run each frame before the join
pass). Without it the expired session's key stays in `wall.requested` and its child stays in
`attached` until the whole wall closes: invisible, and steadily pushing the live-process count
past `MAX_TILES` as freed slots are refilled. Dropping the key also invalidates any join still
in flight, since `install_wall_join` bails on `!wall.owns(&key)` before it spawns.

### Triage inbox (`Ctrl+N`)

A modal walking every session that is awaiting input, longest wait first, with the session
itself attached live into the middle panel. Three chords are reserved (`Ctrl+N` next, `Ctrl+P`
previous, `Ctrl+]` leave) and everything else goes to the session, because typing into the
agent's own input path is how a prompt gets answered; there is no second reply mechanism. The
queue is snapshotted when the modal opens so a background refresh cannot reorder it mid-answer.

Sessions are attached one at a time as they are reached, never prefetched, and a visit lasts
exactly as long as the item is on screen: `release_triage_attachment` closes the child both when
you move off an item and when you close the queue. Keeping every visited child alive would
accumulate invisible processes across a walk. The one exception is a key the video wall owns,
which the wall is responsible for and triage must not tear down underneath it.

### Tail pane (`Ctrl+B`) and command palette (`Ctrl+K`)

The tail pane shows the selected session's last `TAIL_EVENTS` (12) turns beside the list, read
through the bounded 512 KiB display tail above. It requires `TAIL_MIN_TOTAL_WIDTH` (100)
columns; below that, opening it is refused with a notice naming the width available, because the
pane and a usable list cannot both fit. Closing an already-open pane is never refused.

The palette is the discoverability surface for everything without a chord of its own, and the
only way to reach some actions from the wall. It carries the action list (each with its chord,
and unavailable actions shown as unavailable rather than hidden), the header sprites, every
visible session as a jump target, every discovered model per backend, and the slash commands for
the composer's current backend. Session entries use the same visible-row model as the list, so
the `hold` rule applies there too.

### Themes, sprites, and the age ramp

Eleven built-in themes, selected through the composer's `/theme` command, which previews a
candidate against the whole screen on `↑`/`↓`, commits on `Enter`, and reverts on `Esc`. The
choice persists in the viewer DB. User themes are `*.theme` files under
`~/.config/agent-viewer/themes`, one `key=#rrggbb` per line with `#` comments; a malformed line
is skipped with a notice rather than failing the file. The active theme is reloaded whenever its
file's mtime changes, checked on the ordinary refresh cycle, so editing a theme with the viewer
running lands on save. Marks and motion belong to the theme: a theme may select glyph marks and
may switch animation off, and `terminal match` builds itself from the captured host palette
rather than concrete RGB.

The header sprite is one of six, cycled with `Ctrl+G` and persisted; `AV_SPRITE` overrides the
startup choice for one run without overwriting what is saved. The age ramp is an optional mode
that fades a finished row toward the theme's `faint` token over a 24 hour horizon, so today's
work pops and Tuesday's recedes. Both endpoints come from the theme and there are no literal
colors in it, which also gives it a free truecolor gate: if either endpoint is not `Color::Rgb`
(the `terminal` theme builds from named ANSI colors) there is nowhere to interpolate to, so the
start color comes back untouched instead of degrading to noise.

### Brand logo marks

The row mark is an inline image, not a character, whenever the terminal can render one. The
backend SVGs are embedded at build time and rasterized once at startup at 64px, oversampled so
the half-blocks fallback is not chunky. List rows use fixed two-cell protocols; kitty, iTerm2,
and Sixel composers use separate three-cell protocols whose artwork is offset by half a cell,
which half-blocks cannot represent, so that fallback reuses the two-cell protocols.

**The probe must run before ratatui takes the alternate screen.** `Picker::from_query_stdio`
performs real terminal I/O and temporarily toggles raw mode on stdin. On a non-tty or an
unsupported terminal it errors, and every failure leaves the textual marks in place:
`[cc]`/`[cx]` by default, or the `✳`/`◆` glyphs under `AGENT_VIEWER_GLYPH_MARKS=1`. Logo mode
outranks both and blanks the mark slot for the image overlay.

### Terminal title

The tab title is `Agent Viewer · <launch directory basename>`. The basename is sanitized by
dropping control characters, and a name that is missing, unreadable, or empty after that
sanitization falls back to a bare `Agent Viewer` rather than emitting a half-formed title. The
write itself is best-effort: a terminal that does not support `SetTitle` is not an error and is
never surfaced.

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
