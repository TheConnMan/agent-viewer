# agent-viewer

A terminal viewer for coding-agent sessions, modeled on Claude Code's `claude agents`
view. These CLIs have no built-in "see all my sessions" console; this fills that gap
across backends. It reads each agent's own local session store, shows every session in a
single live list, and lets you attach to one in an embedded terminal without leaving the
viewer.

## Backends

Three backends ship, and each advertises which capabilities it supports; the TUI gates
keys accordingly (an unsupported action is a no-op with a footer notice). Backends with
no data or whose CLI is not installed simply list empty — they never error the view.

- **Codex** — full support. Reads the global registry (`~/.codex/state_*.sqlite`, table
  `threads`) plus the rollout transcripts under `~/.codex/sessions/`, resolves live
  running/done/errored status, spawns detached `codex exec` jobs, attaches via
  `codex resume`, and archives/unarchives.
- **Claude / Claude Code** — enumerate, spawn, attach (`claude -r`), and rename. No
  archive (Claude has no hide concept); rename falls back to a local override when the
  daemon rename is unavailable.
- **opencode** — enumerate, spawn, and attach from `~/.local/share/opencode/opencode.db`.
  No archive, no rename.

## States

Every session resolves to one of six states, each with its own glyph in the list:

- `✽` working (blinks) — the agent is actively running.
- `◐` needs-input — waiting on you. Best-effort for Codex (inferred from the transcript
  tail), so treat it as a hint rather than a guarantee.
- `∙` idle — live but not doing anything.
- `●` done — finished cleanly.
- `✗` failed — exited with an error.
- `○` stopped — stopped from the viewer.

The default list groups by project directory; `Ctrl+S` regroups by state (needs-input,
working, idle, done, with failed and stopped folding into done). Every row renders — the
list is uncapped and scrolls with the selection to fill the terminal height.

Working rows shimmer their glyph, needs-input rows breathe between muted and bright, and a
session you just spawned blooms once when its row first appears.

## Companion filtering

A single session can surface from more than one source (e.g. a registry row and a
rollout file). The secondary copies are marked **companions** and hidden by default so
the list shows one row per session. Archived sessions are hidden too, and so are sessions
whose working directory no longer exists on disk (e.g. deleted `/tmp` scratch dirs). Press
`a` to reveal all of them; the footer shows how many rows are currently hidden. Sessions
you spawn from the viewer are pinned, so they always show even if another source would
have marked them a companion.

## Inline spawn composer

The list view carries a persistent one-line composer above the footer:
`[cc] ~/git/foo ❯ …`. Just start typing to describe a task; `Tab` cycles the target agent
(claude code `[cc]` → codex `[cx]` → opencode `[oc]`), and `Enter` spawns it detached. The
target directory is the selected row's project root (by-project view) or its exact cwd
(by-state view). While the composer is empty, the single-letter command keys below still
fire; once you have typed anything, every printable key (and space) is task text, and `Esc`
clears it.

## Keys

- `↑`/`↓` — move selection.
- `→` — attach the selected session in an embedded terminal.
- `Enter` — spawn the composed task, or (empty composer) attach the selected session.
- `Tab` — cycle the composer's target agent.
- `Space` — peek the transcript tail (Codex/Claude) or session metadata (opencode).
- `Ctrl+R` — rename the selected session.
- `Ctrl+X` — stop the selected session; press again within 2s to remove it.
- `Ctrl+S` — toggle grouping by project / by state.
- `a` — show all (companions + archived + deleted-dir rows).
- `h` / `u` — archive / unarchive (Codex only).
- `/` — filter by title or directory.
- `?` — key help.
- `q` — quit.

Renames, stops, removes, and archives run on a background worker so a slow backend call
never freezes the list; a `…` notice shows while the action is in flight.

## Embedded attach

`→` (or `Enter` with an empty composer) opens the selected session inside the viewer as a
full-screen embedded terminal. Codex runs `codex resume`; opencode runs `opencode -s`;
Claude opens its live agents view for a running background job (preselecting it) or
`claude -r` to resume a finished one. `←` returns to the list when the input line is empty
(otherwise it moves the child's cursor), and `Ctrl+]` always detaches. The attached PTY
stays alive in the background so re-attaching is instant.

Quitting the viewer (`q`) kills the attach PTYs it owns, but that does not lose any work:
the conversations live in each backend's own store and re-attach by session ID next time.
If a child exits while you are attached, its final screen stays visible and any key
returns you to the list.

Viewer-local state (renames, archive flags, stop markers, spawn records) is kept in a
SQLite database at `~/.local/state/agent-viewer/viewer.db`, separate from every backend's
own store.

Built in Rust. See `SPEC.md` for the full architecture and the evidence behind it.

## Build

```
cargo build --workspace
```

## Run

```
cargo run -p agent-viewer-tui
```

The binary is named `agent-viewer`. It expects a `~/.codex/state_*.sqlite` on the box
(the Codex backend's source of truth); the Claude and opencode backends appear
automatically when their CLIs and data exist and silently list empty otherwise.

## Test

```
cargo test --workspace
```

The live end-to-end tests are `#[ignore]` by default because they spawn a real
`codex exec` and need Codex auth plus network:

```
cargo test -p agent-viewer-core --test e2e_live -- --ignored --nocapture
```
