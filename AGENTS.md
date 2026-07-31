# AGENTS.md - agent-viewer

Project-specific guidance for this repo. The global `~/.claude/AGENTS.md` rules apply except
for the local small change override below. This file adds what is true only of `agent-viewer`
and must not be re-derived every session.

## Small change process

Direct editing is allowed for clear, low risk changes affecting one or two files. This path
needs no implementation plan or subagent. Delegate distinct work streams, substantial
exploration, architectural or security risk, or independent review that materially improves
confidence.

Test first development is required for new behavior and reproducible bug fixes when a
behavioral regression test provides protection. It is not required for documentation, copy,
formatting, straightforward configuration, mechanical refactors covered by existing tests, or
visual tuning better verified interactively. Verification remains proportional: run relevant
tests and clippy where applicable, and never weaken or remove tests to make a change pass.

Any project invariant or specialized live verification rule in this file overrides this small
change path.

`README.md` is the user-facing behavior contract. `SPEC.md` is the architecture contract and
the evidence behind every design decision. Read `SPEC.md` before changing enumeration, status
detection, or the mutation path; those facts were verified live on this box and are load-bearing.

## What this is

A Rust workspace: a terminal viewer for coding-agent sessions (Codex, Claude Code, opencode),
modeled on `claude agents`. Two crates:

- `crates/agent-viewer-core` (lib) - registry readers, rollout parsers, status resolver,
  spawner, PTY attach, PR status, viewer-local SQLite state. No UI. This is the reusable
  layer a future web/`axum` surface (v2) will share, so keep it cleanly separable from the TUI.
- `crates/agent-viewer-tui` (bin) - `ratatui` + `crossterm` over `-core`. Binary name is
  `agent-viewer`.

## Build, run, test

```bash
cargo build --workspace          # build everything
cargo run -p agent-viewer-tui    # launch the TUI (binary: agent-viewer)
cargo test --workspace           # full unit + integration suite
cargo clippy --workspace         # must be clean before any commit
```

The TUI expects a `~/.codex/state_*.sqlite` on the box (the Codex backend's source of truth).
The Claude and opencode backends appear automatically when their CLIs and data exist and list
empty otherwise, so a missing backend is never an error.

## Cargo is wrapped on this box - bypass it when output is the evidence

Plain `cargo` from the shell is wrapped by `rtk`, which compresses output to one-line summaries
(`cargo test: 2 passed (1 suite, 22.55s)`) and only sometimes tees the full log to
`~/.local/share/rtk/tee/`. When the test *output itself* is the deliverable (a completion-report
proof, `--nocapture` prints, a failing assertion you need to read), bypass the wrapper:

```bash
~/.cargo/bin/cargo test --workspace --no-fail-fast > /tmp/test.log 2>&1
```

`--no-fail-fast` matters: plain `cargo test` stops after the first failing test binary, so a
workspace-wide tally needs it. For a report where the transition prints are the proof, always
capture to a file via the direct binary; the wrapper will otherwise swallow them.

## Test suite specifics

- Unit and integration tests live in `crates/*/tests/`. Fixtures (sample `session_meta` first
  lines, `task_complete` tails, archived-vs-active rows, serialized-`source` variants, backend
  schemas) live in `crates/agent-viewer-core/tests/fixtures/` and `tests/common/`. Reuse them;
  do not invent new fixture shapes when one exists.
- Mock only the filesystem inputs (fixture files). Do NOT mock rusqlite, the parsers, or the
  status resolver - those are the things under test. A test must fail if you stub the resolver
  body (the repo test-quality gate).

### Known flaky test - re-run standalone before treating as a break

`agent-viewer-core`'s `pty_tests::pty_kill_returns_when_grandchild_holds_slave` has a hard 2s
deadline and can miss it under the CPU contention of a full parallel `--workspace` run, then
pass on the next run and pass every time in isolation (~1.35s). It is scheduler jitter, not a
regression. If it fails in a workspace run, confirm with:

```bash
~/.cargo/bin/cargo test -p agent-viewer-core --test pty_tests
```

### Live end-to-end test - opt-in, needs Codex auth + network

The live e2e is `#[ignore]` by default because it spawns a real `codex exec`:

```bash
~/.cargo/bin/cargo test -p agent-viewer-core --test e2e_live -- --ignored --nocapture
```

This is the load-bearing proof that the PID to `rollout_path` `/proc/fd` correlation works: a
real session must show `running`, then flip to `done` on completion. Any change to status
detection or the spawner must be validated with this test end-to-end, not asserted in prose.

### Headless-driving the TUI needs a winsize

If you drive the `ratatui` TUI through a Python `pty.openpty()` harness, a fresh pty has a 0x0
winsize and nothing renders (setting COLUMNS/LINES env does not help). Set the size on the pty
fd before spawning:

```python
fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", rows, cols, 0, 0))
```

Then read frames from the master, strip ANSI, and grep the *cumulative* buffer - ratatui
redraws are cell-diffs, so a per-interval read can miss text drawn in an earlier frame.

## Verify locally - this project is fully local-verifiable

Everything this tool does runs against local state on this box, so there is no excuse to ship a
change unverified. Prefer real local verification over prose claims of correctness, matched to
the blast radius of the change:

- Logic in `-core` (parsers, resolvers, grouping, source parsing): unit tests with fixtures.
- Status detection / spawner: the live e2e above (real `codex exec`, watch `running` to `done`).
- TUI rendering, keys, reply, layout: run `cargo run -p agent-viewer-tui` against the real
  session store, or the pty harness for a scripted headless check.
- Mutations (archive/unarchive/rename/stop): drive them in the running TUI and confirm the row
  moves between visible/hidden or updates, since these run on a background worker.

Run `cargo clippy --workspace` and the relevant tests before every commit; every commit must
build clean with clippy and tests passing.

**Always close a change with the command Brian runs to see it.** Anything with a visible
surface (a sprite, a key, a layout, a row, a popup) ends the report with the literal line to
paste, plus the keys to press:

```bash
cd <repo-or-worktree> && cargo run --release -p agent-viewer-tui
```

He judges this tool by running it, not by reading a diff, so a report that omits the line only
costs a round trip. Name the worktree path when the work has not merged to `main` yet.

**`--release` is not optional in that line.** The binary on his PATH is a release build, so
handing him a debug one makes the build profile the only thing that actually changed, and it
reads as "your commit made everything slow". Measured 2026-07-31 on an idle list view: debug
burns 52% of a core against 9% for release, and the gap widens on the wall, where every tile
is its own vt100 parser. Build the release binary before handing over the line, so his first
run is not a two-minute compile.

## Project invariants - do not violate

- **Read the Codex registry read-only.** Open `~/.codex/state_*.sqlite` with
  `SQLITE_OPEN_READ_ONLY`. Codex writes it concurrently; never write it yourself.
- **Never hardcode the state file version.** Glob `state_*.sqlite` and pick the highest version
  number. Same for any versioned Codex path.
- **Mutations delegate to CLI subcommands or the app-server, never the DB.**
  Archive/unarchive/resume go through `codex ...` (and the Claude/opencode equivalents), never a
  direct DB write. Two documented exceptions:
  - Codex spawn and stop speak JSON-RPC to the `codex app-server` daemon (`thread/start` plus
    `turn/start`, and `turn/interrupt`), because a `codex exec` spawn hosts its app-server in
    process and produces a thread nobody can ever join, and because the daemon holds the
    rollout fd of every thread it hosts so a signal is not a safe stop. agent-viewer MAY start
    a daemon and NEVER stops or restarts one, and a daemon it starts is pinned to `$HOME`, never
    the viewer's cwd - a daemon that inherits a `.worktrees/` cwd keeps answering "running" long
    after that directory is deleted and then fails every spawn, box-wide. Read `SPEC.md`
    "Codex attach/resume" before touching spawn, attach, or stop - the fabricated-interrupt
    behavior, the daemon-cwd poisoning, and the argv-only daemon test were all measured live.
  - Claude rename writes `name`/`nameSource` into `~/.claude/jobs/<short>/state.json`, because
    Claude ships no `rename` subcommand and that file is its own store of record for the name.
    Read `SPEC.md` "Claude mutations" before touching it - the channel it does NOT use was
    verified the expensive way.
- **`source` is a serialized enum, not a flat string.** Parse `cli`/`exec`/`vscode` prefixes;
  treat anything else (JSON blobs, subagent objects) as a subagent. Keep this defensive.
- **Capabilities are backend-advertised.** An unsupported action is a no-op with a footer
  notice, not an error. Do not assume a capability exists across backends; gate on it.
- **Keep `-core` UI-free.** No `ratatui`/`crossterm` types leak into the library crate; that is
  what lets the v2 web surface reuse it.

## Worktrees are mandatory

All branch-creating work happens in a git worktree, never in the main checkout on `main`.
This repo keeps worktrees under `.worktrees/` (gitignored):

```bash
git worktree add .worktrees/<short-desc> -b task/<short-desc> HEAD
# ... work, test, commit inside .worktrees/<short-desc> ...
git worktree remove .worktrees/<short-desc>
```

Before branching fresh, audit `git worktree list` and continue any in-flight branch for the
same work rather than starting a duplicate.

**Persistent artifacts live ABOVE the worktree, never inside it.** `git worktree remove` deletes
the entire directory with no safety net, so anything that must outlive the branch (plans, notes,
`.projects/`, untracked local config) must sit in the repo root or the parent, not in
`.worktrees/<name>/`. `.projects/` and `.worktrees/` are gitignored at the repo root for exactly
this reason. Never create `.projects/` inside a worktree.

## Git and PRs

The remote is `git@github.com:TheConnMan/agent-viewer.git`. Never push without explicit
approval. Follow the global commit format (`Ref` linkage only when the branch carries a ticket
ID, no AI mentions, no Co-Authored-By AI lines). Docs-only or internal changes merge to `main`
locally; feature work destined for review opens a PR only after the diff is shown and approved.

**A mechanical bugfix merges itself.** When the work is a bug with one obviously correct fix and
no design or UX decision in it, do the whole loop without asking: worktree, fix, clippy and tests
green, commit, `git merge --no-ff` to `main`, remove the worktree. Say what landed afterward. The
approval gate above still holds for anything that decides how something looks or behaves, adds a
surface, or changes an invariant in this file. Merging locally is never pushing; pushing still
needs Brian.

Run `cargo clippy --workspace` and the tests on `main` after every merge, not only on the branch:
a textually clean merge still hits semantic conflicts (a `Draw` literal gaining a field on one
branch while another branch adds a new call site) that only the compiler catches.
