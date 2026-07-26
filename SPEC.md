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
 "managedCodexPath":"/home/theconnman/.codex/packages/standalone/current/codex",
 "managedCodexVersion":"0.144.4",
 "socketPath":"/home/theconnman/.codex/app-server-control/app-server-control.sock",
 "cliVersion":"0.144.4","appServerVersion":"0.144.4"}
```

The other half of the original caution still stands: the API is marked `[experimental]` with
shallow versioning, so deserialize permissively and fall back to read-only `state_*.sqlite`
enumeration when `status` is not `running` or the handshake fails. agent-viewer never starts,
stops, or restarts a daemon.

## Claude mutations — CLI subcommands, plus one deliberate state-file write

Spawn is `claude --bg`, attach is `claude attach <short>`, remove is `claude rm <short>`. Rename
is the single exception to "delegate to a CLI subcommand": Claude ships no `rename` subcommand,
so agent-viewer does what Claude's own fleet view does and writes the job's state file.

**Mechanism.** Read `~/.claude/jobs/<short>/state.json`, set `name` and `nameSource: "user"`,
write it back atomically (temp file in the same dir, then rename over the target). Read-modify-
write, never a blind overwrite: that file also carries `respawnFlags`, `intent`, and the
transcript path, and dropping them would break the job's respawn contract.

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
