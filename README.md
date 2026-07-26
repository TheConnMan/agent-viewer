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
- **Claude / Claude Code** — enumerate, spawn, attach (`claude attach`), remove (`claude rm`),
  and rename background sessions. Rename writes `name`/`nameSource` into that job's
  `~/.claude/jobs/<short>/state.json`, which is what Claude's own fleet view does and the only
  channel it has (there is no `claude rename` subcommand). It applies to background rows only:
  an interactive row has no job dir, so `Ctrl+R` there is a footer notice. No archive/hide
  (Claude has no hide concept).
- **opencode** — enumerate, spawn, and attach from `~/.local/share/opencode/opencode.db`.
  No archive, no rename.

## States

Every session resolves to one of six states, each with its own glyph in the list:

- `✽` working (blinks) — the agent is actively running.
- `◐` needs-input — waiting on you. Best-effort for Codex (inferred from the transcript
  tail), so treat it as a hint rather than a guarantee.
- `∙` idle — live but not doing anything.
- `●` done — finished cleanly.
- `✗` error — exited with an error.
- `?` unknown — the backend cannot say; never shown as a false idle.

Each row is prefixed by its backend's mark in the backend's color — by default the textual
tag `[cc]` Claude (terracotta), `[cx]` Codex (teal), `[oc]` opencode (green) — followed by
the state as a word in the state's color (`Working`, `Needs input`, `Idle`, `Done`, `Error`,
`Unknown`) and a muted one-line summary. The status word and time sit right-aligned; Claude
jobs with associated pull requests show a badge just left of the time — `#315` for one PR,
`2 PRs` for several. The badge is colored by the PR's live GitHub status: yellow when checks
are pending or failing or a review is requested, green when checks have passed, purple when
merged, grey when a draft or closed, and the flat accent color when the status is unknown or
unresolvable.

Set `AGENT_VIEWER_GLYPH_MARKS=1` to use brand glyph marks instead of the textual tags:
`✳` Claude, `◆` Codex, `■` opencode (only if your terminal font renders them).

The default list groups by project directory; `Ctrl+S` regroups by state (needs-input,
working, idle, done, with error folding into done and unknown folding into idle). Every row renders — the
list is uncapped and scrolls with the selection to fill the terminal height. A blank line
separates each group/section, and rows sit flush-left under their group header.

Working rows shimmer their glyph, needs-input rows breathe between muted and bright, and a
session you just spawned blooms once when its row first appears.

## Companion filtering

A single session can surface from more than one source (e.g. a registry row and a
rollout file). The secondary copies are marked **companions** and hidden by default so
the list shows one row per session. One-shot runs started by a script rather than by you
are companions too, because they are steps inside somebody else's job and not fleet members
you would ever attach to: Codex `exec` and subagent threads, and `opencode run` sessions
(the review passes an `/implement` run fires off, for example). Archived sessions are hidden
too, and so are sessions whose working directory no longer exists on disk (e.g. deleted
`/tmp` scratch dirs). Press `a` to reveal all of them; the footer shows how many rows are
currently hidden, and `Ctrl+F` searches the hidden rows too. Sessions you spawn from the
viewer are pinned, so they always show even if another source would have marked them a
companion.

## Inline spawn composer

The list view carries a persistent composer in a rounded box between the list and the
footer: `[cc] claude opus[1m] ~/git/foo ❯ …`. Just start typing to describe a task; `Tab`
cycles the target agent (Claude → Codex → opencode), `Shift+Tab` cycles that agent's model,
and `Enter` spawns it detached with that model. The models are discovered from each agent's
CLI or catalog (Codex: `codex debug models`; Claude: the models in your `~/.claude.json`;
opencode: `opencode models`), default first. Discovery runs in the background and is cached
for a day, so the picker is populated from the first keystroke rather than waiting on a probe
that takes seconds; a catalog that has never been discovered shows just the agent's default
until its first probe lands. `Shift+Tab` cycles the Claude/Codex lists;
opencode has too many models to cycle, so it stays on its default there (use `/model`). The
target directory is the selected row's project root (by-project view) or its exact cwd
(by-state view). While the composer is empty, the single-letter command keys below still fire;
once you have typed anything, every printable key (and space) is task text, and `Esc` clears it.

Type `/model` (optionally `/model <filter>`) to open a filterable picker of every available
model for the target agent, floating above the box. `↑`/`↓` move the highlight, `Tab` or
`Enter` picks the model (and clears the composer), `Esc` closes it.

**Spawned sessions run unsandboxed.** Codex jobs are started with
`--dangerously-bypass-approvals-and-sandbox`, so they can write files, use the network, and
drive git (fetch, branch, worktree, commit) without prompting. This is deliberate: codex's
`workspace-write` sandbox mounts `.git` read-only and blocks the network, which made every
git-shaped task a silent no-op — the session would burn a turn and report back that it could
not create a branch. Treat a task you type here as something you are running with your own
shell privileges, because that is what it is.

Typing any other slash command shows a completion popup above the box: the available commands
for the selected agent (Claude skills under `~/.claude/skills` plus the target's project skills;
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
collapses it. `Ctrl+R` turns the selected row itself into an empty edit field — type the new
name, `Enter` commits, `Esc` cancels, and `Enter` on a blank field cancels too. Neither is a
modal.

## Keys

- `↑`/`↓` — move selection.
- `→` — attach the selected session in an embedded terminal.
- `Enter` — spawn the composed task, or (empty composer) attach the selected session.
- `Tab` / `Shift+Tab` — cycle the composer's target agent / that agent's discovered models
  (Claude and Codex; opencode has too many, so use `/model` there).
- `/model` — open a filterable picker of every available model for the target agent
  (`↑`/`↓` highlight, `Tab`/`Enter` pick, `Esc` close).
- `Space` — expand the selected row in place to peek its last message / metadata.
- `Ctrl+E` — reserved. Reply is deliberately out of scope for this rebuild (see below);
  pressing it always reports a footer notice that reply is not supported.
- `Ctrl+R` — rename the selected session inline (the row becomes an edit field).
  Capability-gated per row, not per backend: a row the backend cannot rename (an interactive
  Claude row, which has no job dir) is a no-op with a footer notice.
- `Ctrl+X` — stop the selected session; press again within 2s to remove it.
- `Ctrl+S` — toggle grouping by project / by state.
- `a` — show all (companions + archived + deleted-dir rows).
- `h` / `u` — archive / unarchive (Codex only).
- `Ctrl+F` — filter by title or directory (searches hidden/archived sessions too).
- `Ctrl+T` — turn mouse reporting off / back on. Off hands the mouse back to your terminal so
  you can drag-select and copy text out of the viewer; on restores click/hover row selection
  and wheel scrolling. Works everywhere, including inside an attach, so the transcript is
  copyable. A footer notice names the mode you just switched to.
- `?` — key help.
- `q` — quit.

Renames, stops, removes, and archives run on a background worker so a slow backend call
never freezes the list; a `…` notice shows while the action is in flight.

## Embedded attach

`→` (or `Enter` with an empty composer) opens the selected session inside the viewer as a
full-screen embedded terminal. Codex runs `codex resume`; opencode runs `opencode -s`;
Claude runs `claude attach`, which resumes the same thread for both a running background
job and a finished one (waking it in place); a row with no background-job id falls back to
`claude -r`. `←` returns to the list when the
input line is empty (otherwise it moves the child's cursor), and `Ctrl+]` always detaches.
The attached PTY stays alive in the background so re-attaching is instant.

Peek and reply are deliberately out of scope for this rebuild (a divergence from Fleet View,
which binds `space` to reply — noted here so it is not mistaken for an oversight; see the
constitution's Additional Constraints). `Ctrl+E` is reserved in the key list but always
reports a footer notice that reply is not supported. Revisiting either is a future,
separately-specified decision.

Quitting the viewer (`q`) kills the attach PTYs it owns, but that does not lose any work:
the conversations live in each backend's own store and re-attach by session ID next time.
If a child exits while you are attached, its final screen stays visible and any key
returns you to the list.

Viewer-local state (archive flags, stop markers, spawn records) is kept in a
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
