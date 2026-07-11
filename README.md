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

Each row is prefixed by its backend's mark in the backend's color — by default the textual
tag `[cc]` Claude (terracotta), `[cx]` Codex (teal), `[oc]` opencode (green) — followed by
the state as a word in the state's color (`Working`, `Needs input`, `Idle`, `Done`, `Failed`,
`Stopped`) and a muted one-line summary. The status word and time sit right-aligned; Claude
jobs with associated pull requests show a badge just left of the time — `#315` for one PR,
`2 PRs` for several.

Set `AGENT_VIEWER_GLYPH_MARKS=1` to use brand glyph marks instead of the textual tags:
`✳` Claude, `◆` Codex, `■` opencode (only if your terminal font renders them).

The default list groups by project directory; `Ctrl+S` regroups by state (needs-input,
working, idle, done, with failed and stopped folding into done). Every row renders — the
list is uncapped and scrolls with the selection to fill the terminal height. A blank line
separates each group/section, and rows sit flush-left under their group header.

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

The list view carries a persistent composer in a rounded box between the list and the
footer: `[cc] claude opus[1m] ~/git/foo ❯ …`. Just start typing to describe a task; `Tab`
cycles the target agent (Claude → Codex → opencode), `Shift+Tab` cycles that agent's model
(Claude: `opus[1m]` → `sonnet` → `fable`; Codex: `default` → `gpt-5.3-codex` → `gpt-5.2-codex`;
opencode: a single default), and `Enter` spawns it detached with that model. The target
directory is the selected row's project root (by-project view) or its exact cwd (by-state
view). While the composer is empty, the single-letter command keys below still fire; once you
have typed anything, every printable key (and space) is task text, and `Esc` clears it.

Typing a slash command shows a completion popup above the box: the available commands for the
selected agent (Claude skills under `~/.claude/skills` plus the target's project skills;
opencode commands under `~/.config/opencode/command`; codex prompts under `~/.codex/prompts`),
prefix-filtered live. While it is open, `↑`/`↓` move the highlight, `Tab` inserts the
highlighted command, `Esc` dismisses the popup, and `Enter` spawns the text as-is (so
`/implement RS-123` runs that slash command as the task prompt).

## Inline peek and rename

`Space` expands the selected row in place. The peek shows the session's last message
word-wrapped to the panel width with its newlines preserved, and a short recent-tail of the
prior items collapsed to one line each above it. A blocked (needs-input) row leads with a
prominent header: `Awaiting approval: <command/patch>` for Codex or `Awaiting input:
<question>` for Claude. opencode previews its own real last message from its SQLite store,
falling back to status and cwd only when it has none. `Space` again or moving the cursor
collapses it. `Ctrl+R` turns the selected row itself into an edit field prefilled with the
title — type to edit, `Enter` commits, `Esc` cancels. Neither is a modal.

## Keys

- `↑`/`↓` — move selection.
- `→` — attach the selected session in an embedded terminal.
- `Enter` — spawn the composed task, or (empty composer) attach the selected session.
- `Tab` / `Shift+Tab` — cycle the composer's target agent / that agent's model.
- `Space` — expand the selected row in place to peek its last message / metadata.
- `r` — reply to the selected needs-input session (an input opens in the composer area;
  `Enter` sends, `Esc` cancels). Capability-gated: unsupported backends (opencode) and
  non-blocked rows are a no-op with a footer notice.
- `Ctrl+R` — rename the selected session inline (the row becomes an edit field).
- `Ctrl+X` — stop the selected session; press again within 2s to remove it.
- `Ctrl+S` — toggle grouping by project / by state.
- `a` — show all (companions + archived + deleted-dir rows).
- `h` / `u` — archive / unarchive (Codex only).
- `Ctrl+F` — filter by title or directory (searches hidden/archived sessions too).
- `?` — key help.
- `q` — quit.

Renames, stops, removes, and archives run on a background worker so a slow backend call
never freezes the list; a `…` notice shows while the action is in flight.

## Embedded attach

`→` (or `Enter` with an empty composer) opens the selected session inside the viewer as a
full-screen embedded terminal. Codex runs `codex resume`; opencode runs `opencode -s`;
Claude opens its live agents view for a running background job — the viewer then presses
Enter for you once the view is ready so you land directly in that job's run, not on the
agents list — or `claude -r` to resume a finished one. `←` returns to the list when the
input line is empty (otherwise it moves the child's cursor), and `Ctrl+]` always detaches.
The attached PTY stays alive in the background so re-attaching is instant.

Replies (`r`) ride the same embedded attach. A Claude reply lands the typed text plus Enter
into the run once the viewer has navigated into it (not while still on the agents list). A
Codex approval reply maps yes / no to the approval keystroke; any free-text reply where only
yes or no is valid instead attaches you with focus so you can finish it by hand. Delivery is
best-effort: the auto-inject rides on detecting when the child is ready to accept input, and
if that detection does not land, the reply is not sent blindly. In that case you are left
attached in the session to type it yourself, and a footer notice says so.

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
