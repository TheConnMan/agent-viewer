# codex-agent-viewer

A terminal viewer for coding-agent sessions, modeled on Claude Code's `claude agents`
view. These CLIs have no built-in "see all my sessions" console; this fills that gap.

It reads each agent's own local session store and lets you:

1. **Create** new background sessions (fire-and-forget, like `claude --bg`).
2. **See all** sessions, including background ones, however launched.
3. **Hide / unhide** sessions (like dismissing rows in `claude agents`).
4. **Group** sessions by project (working directory).

Three backends ship in v1, and each backend advertises which of those capabilities
it supports; the TUI gates keys accordingly (an unsupported action is a no-op with a
notice):

- **Codex** — full support. Reads the global registry (`~/.codex/state_*.sqlite`, table
  `threads`) plus the rollout transcripts under `~/.codex/sessions/`, resolves live
  running/done/errored status, spawns detached `codex exec` jobs, and archives/unarchives
  and resumes via `codex` subcommands.
- **Claude / Claude Code** — enumerate, spawn, and attach. No hide (Claude has no archive
  concept), and no local transcript tail.
- **opencode** — enumerate, spawn, and attach from `~/.local/share/opencode/opencode.db`.
  No hide, no transcript tail.

Backends that have no data or whose CLI is not installed simply list empty; they never
error the view. Built in Rust. See `SPEC.md` for the full architecture and the evidence
behind it.

## Build

```
cargo build --workspace
```

## Run

```
cargo run -p codex-viewer-tui
```

Requires a `~/.codex/state_*.sqlite` on the box (the Codex backend's source of truth).
The Claude and opencode backends appear automatically when their CLIs and data exist and
silently list empty otherwise.

## Keys

- `Enter` — attach / resume the selected session.
- `n` — new session. A modal prompts for directory and task; `Left`/`Right` pick the
  backend, `Tab` moves between fields, `Enter` spawns detached, `Esc` cancels.
- `h` — hide (archive) the selected session.
- `u` — unhide (unarchive) the selected session.
- `a` — toggle showing hidden sessions.
- `space` — collapse / expand the selected project group.
- `/` — filter.
- `j` / `k` or the arrow keys — navigate.
- `q` — quit.

## Test

```
cargo test --workspace
```

The live end-to-end tests are `#[ignore]` by default because they spawn a real
`codex exec` and need Codex auth plus network:

```
cargo test -p codex-viewer-core --test e2e_live -- --ignored --nocapture
```

## Status

v1 is the TUI described above. A v2 web surface (an `axum` binary over the same `-core`
library, deployed behind Tailscale) is a natural follow-on but is out of scope for now.
