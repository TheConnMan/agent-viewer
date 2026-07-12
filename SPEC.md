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

Note the experimental `codex app-server` JSON-RPC daemon (`thread/subscribe`, `thread/list`,
`command/exec/terminate`) exists and is the eventual "clean" backend, but v1 does NOT use it:
it is labeled experimental, its control socket is already contended on this box, and it reads
the same SQLite + files we read directly. Leave a comment marking it as the v2 upgrade path.

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
