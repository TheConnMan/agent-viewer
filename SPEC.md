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
  primary deliverable because the killer feature (one-keystroke attach/resume into a session)
  is inherently terminal. This runs on a headless Linux remote-dev box over SSH.
- **Out of scope for v1:** a web/Tailscale surface. It is a natural v2 (an `axum` binary
  sharing `-core`, deployed like the `bonus-drain`/`bg-schedule` viewers with token-guarded
  write routes) but do NOT build it now. Leave `-core` cleanly separable so v2 can reuse it.

## Enumeration — the source of truth (requirements 2, 3-read, 4)

Codex maintains a global session registry. **Read it; do not scrape JSONL for the list view.**

- Path: `~/.codex/state_*.sqlite`. **Glob and pick the highest version number** (currently
  `state_5.sqlite`); do NOT hardcode `5`.
- Open **read-only** with WAL tolerance: `rusqlite` with `OpenFlags::SQLITE_OPEN_READ_ONLY`
  (use the `bundled` feature). Codex writes concurrently; never write this DB yourself.
- Table `threads`, load-bearing columns (verified via `.schema threads`):
  `id TEXT PK`, `rollout_path TEXT`, `created_at`, `updated_at` / `updated_at_ms`,
  `source TEXT`, `cwd TEXT`, `title TEXT`, `archived INTEGER DEFAULT 0`, `archived_at`,
  `model TEXT` (read only for the model-picker fallback via `distinct_models`, not per row),
  `preview TEXT`. Order by `updated_at_ms DESC`. Other columns exist in the schema
  (`git_branch`, `git_origin_url`, `first_user_message`, `thread_source`, `agent_nickname`,
  `agent_role`) but the reader does not load them.
- `source` is a **serialized enum, not a flat string**. Observed values: `cli`, `exec`,
  `vscode`, and JSON blobs like `{"subagent":"review"}` or nested `thread_spawn` objects.
  Parse defensively: match `cli`/`exec`/`vscode` prefixes; anything else → treat as subagent.
- `archived=1` rows are the hidden set; their `rollout_path` points into
  `~/.codex/archived_sessions/` (active rows point into `~/.codex/sessions/`).
- Grouping key = `cwd`. Optionally fold `cwd` up to the nearest `.git` root so worktrees
  collapse under one project (mirrors the local `claude-usage` "aggregate worktrees" idea).

A lighter index exists at `~/.codex/session_index.jsonl` but `threads` is strictly richer —
use the SQLite.

## Enumeration — opencode, and the run-mode companion rule

opencode keeps its own registry at `~/.local/share/opencode/opencode.db`. Same discipline as
the Codex registry: open read-only, never write it, and treat a missing file as a quiet empty
backend rather than an error.

- Table `session`, load-bearing columns: `id`, `parent_id`, `directory`, `title`,
  `time_created`, `time_updated`, `time_archived`, `permission`. Order by `time_updated DESC`.
  The live table carries ~27 drizzle-managed columns; the reader depends only on these eight,
  and the test fixture enforces that by construction.
- `time_archived IS NOT NULL` is the hidden set. Grouping key = `directory`.
- opencode exposes no per-session process signal, so status is a three-tier recency heuristic
  over one `live_opencode_proc()` check per `list()` call, never `NeedsInput` or `Error`.

**Companions.** A session is a companion when `parent_id` is non-NULL (a sub-session of
another session) **or** when it was started as a one-shot `opencode run` rather than by a
human at the TUI. The second half is the opencode analogue of the Codex `exec`/subagent rule
above: a run fired off by a script (an `/implement` review pass, a CI job) is a step inside
somebody else's job, not a fleet member anyone would attach to. `parent_id` alone does not
catch it, because `opencode run` creates a top-level session with no parent.

The discriminator is the `session.permission` column. `opencode run` denies the interactive
`question` tool when it creates the session; the TUI writes no session override at all.
Verified live on this box 2026-07-26, both on opencode 1.17.20, which rules out a version
confound in the stored history (every 1.17.20 row observed had the column set, every 1.17.17
row had it empty):

```
$ opencode run --title "AV probe run mode" "..."     -> permission =
    [{"permission":"question","pattern":"*","action":"deny"},
     {"permission":"plan_enter","pattern":"*","action":"deny"},
     {"permission":"plan_exit","pattern":"*","action":"deny"}]

$ opencode  (TUI, driven through a pty, one message sent)  -> permission = NULL
```

Corroborated in the shipped binary: that array literal is constructed inside the `run`
command handler, immediately after its "You must provide a message or a command" and
"--fork requires --continue or --session" validations.

Match **semantically** — a `question` entry with `action: "deny"` anywhere in the array — not
by string equality. The stored key order is not the source order, and the github-action path
writes the `question` deny without the `plan_enter`/`plan_exit` pair. Anything that is not a
JSON array of objects parses to "interactive", the safe direction: a shown row one keypress
from hidden beats a hidden row the user cannot find.

Two existing behaviors keep this from swallowing anything: sessions the viewer itself spawned
are pinned by the viewer-state overlay, which clears `companion`, and `Ctrl+F` searches hidden
rows, so a run-mode session is always reachable by name.

Selecting `permission` makes the reader depend on a column older opencode schemas lack. That
is deliberate and matches how the Codex reader depends on its columns directly; a
`backend.list()` error is already contained to a footer notice over the previous snapshot, so
the failure mode on an old schema is a visible notice, not a crash.

## Rollout transcripts (for detail view + status tail)

Path: `~/.codex/sessions/YYYY/MM/DD/rollout-<ts>-<uuid>.jsonl`. Parse with `serde_json`
line-by-line (`BufReader`). For the list, read only the **first line** (`session_meta`, has
`payload.cwd`, `payload.id`, `payload.originator`, `cli_version`) and the **last few lines**
for the terminal marker. Parse the full file lazily only when a session is opened.

- Message content: `type:"response_item"`, `payload.role`, `payload.content[].text`
  (assistant text is `content[].type == "output_text"`).
- Terminal marker: an `event_msg` with `type:"task_complete"`, preceded by a `token_count`
  event. Its presence near the tail = the session's last turn finished cleanly.

## Status detection — TWO signals, both required

The file signal alone is insufficient: during research 66/383 rollouts lacked `task_complete`
yet **zero** `codex` processes were running, i.e. those are crashed/abandoned, not live.

Resolve per thread:
1. **running** — a live `codex`/`codex exec` PID holds this thread's `rollout_path` open.
   Enumerate PIDs (`sysinfo`), then read `/proc/<pid>/fd/*` (readable for same-user procs;
   use `procfs` or `std::fs::read_link`) and match an open path to `threads.rollout_path`.
2. **done** — not running AND `task_complete` in the tail.
3. **errored/abandoned** — not running AND no `task_complete` (ends mid-turn).
4. **hidden** — `archived=1` (orthogonal; applies on top of 1-3).

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
  enumeration picks it up. (Confirm the exact sandbox flag default is acceptable; prefer the
  least-privileged flag that actually runs unattended on this box.)
- **Hide (req 3):** `codex archive <id>`; **unhide:** `codex unarchive <id>`;
  hard-delete (optional, guard behind a confirm): `codex delete <id>`.
- **Attach/resume:** `codex resume <id>` (or `codex exec resume <id>`), exec'd into the
  user's terminal from the TUI.

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
calling `thread/resume`.

**Current design: agent-viewer binds to the app-server for Codex metadata.** Enumeration comes
from `thread/list` with an explicit `sourceKinds` filter and `useStateDbOnly: true` (D-005),
rename from `thread/name/set`, and attach from `codex resume --remote unix://<socketPath> <id>`
(D-004). The transport is RFC6455-framed JSON-RPC over the Unix socket (D-009), discovered
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
shallow versioning, so deserialize permissively and fall back to read-only `state_*.sqlite`
enumeration when `status` is not `running` or the handshake fails. agent-viewer never starts,
stops, or restarts a daemon.

## Crate layout

- `agent-viewer-core` (lib): registry reader (rusqlite), rollout parser (serde_json),
  status resolver (sysinfo/procfs + `/proc/fd`), spawner (Command+setsid), and thin wrappers
  around `codex archive`/`unarchive`/`resume`. No UI. Unit-tested.
- `agent-viewer-tui` (bin): `ratatui` + `crossterm` over `-core`.

Cargo workspace. Live-refresh the registry every ~1-2s; `notify`-watch the focused session's
rollout for live tail.

## TUI behavior

- Left pane: sessions grouped by project, status glyphs (borrow opencode-monitor's vocabulary:
  spinner=running, green=done, gray=hidden, red=errored). Right pane: focused session tail.
- Keys: `Enter` attach/resume; `n` new session (prompt dir + task, spawn detached);
  `h` hide (archive); `u` unhide; `a` toggle show-hidden; `space` collapse group;
  `/` filter; `j/k` + arrows navigate; `q` quit. Keep the set small and obvious.

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
3. Pressing `n` spawns a background `codex exec` that appears in the list within ~2s.
4. That spawned session shows **running** (proving the `/proc/fd` correlation works live), then
   **done** after it finishes — demonstrated with actual output, not asserted in prose.
5. `h`/`u` archive/unarchive a session and it moves between visible and hidden views.
6. README documents how to build and run.

Report at completion with: the exact commands run and their output for done-when items 1, 3, and 4
(the live status transition is the load-bearing proof).
