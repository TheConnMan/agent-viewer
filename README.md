# agent-viewer

A terminal viewer for coding-agent sessions, modeled on Claude Code's `claude agents`
view. These CLIs have no built-in "see all my sessions" console; this fills that gap
across backends. It reads each agent's own local session store, shows every session in a
single live list, and lets you attach to one in an embedded terminal without leaving the
viewer.

The header identifies the viewer as `[av] Agent Viewer` with its version, the full
launch workspace, and live totals for sessions awaiting input, working, and completed
(including errors). The terminal tab title is `Agent Viewer · <launch directory name>`;
when the launch directory cannot be determined, it falls back safely to `Agent Viewer`.

## Backends

Three backends ship, and each advertises which capabilities it supports; the TUI gates
keys accordingly (an unsupported action is a no-op with a footer notice). Backends with
no data or whose CLI is not installed simply list empty — they never error the view.

- **Codex** — full support. Reads the global registry (`~/.codex/state_*.sqlite`, table
  `threads`) plus the rollout transcripts under `~/.codex/sessions/`, resolves live
  running/done/errored status, spawns into the shared `codex app-server` daemon (starting one if
  none is running, and failing the spawn with the daemon's own error rather than quietly
  creating a session nobody could ever join), attaches by joining that daemon
  (`codex resume --remote`), stops a hosted session by interrupting its turn, and
  archives/unarchives.
- **Claude / Claude Code** — enumerate, spawn, attach (`claude attach`), remove (`claude rm`),
  and rename background sessions. Rename writes `name`/`nameSource` into that job's
  `~/.claude/jobs/<short>/state.json`, which is what Claude's own fleet view does and the only
  channel it has (there is no `claude rename` subcommand). It applies to background rows only:
  an interactive row has no job dir, so `Ctrl+R` there is a footer notice. No archive/hide
  (Claude has no hide concept).
- **opencode**: uses a secured local server when one is available, with read only SQLite
  compatibility enumeration otherwise. The fallback retains its process and recency status
  heuristic and companion filtering. The viewer discovers loopback servers on ports `4097` and
  `4098`; only spawn may start one, from the user home directory, and it never stops or restarts
  one. Exact viewer marked sessions receive live status, pending input, rename, archive, stop,
  and delete through that server. Managed attach is refused because it would expose credentials.
  Other server enumerated rows use compatibility idle status
  unless hydration fails for their shared active managed directory, when every row there is unknown.
  External sessions attach locally.

## States

Every session resolves to one of six states, each with its own glyph in the list:

- `✽` working (blinks) — the agent is actively running.
- `◐` needs-input — waiting on you. Best-effort for Codex (inferred from the transcript
  tail), so treat it as a hint rather than a guarantee.
- `∙` idle — live but not doing anything. A Codex session hosted by the app-server daemon reads
  idle rather than done while this viewer still has an embedded terminal open on it, since that
  terminal is a live client sitting in the session. `Ctrl+]` only leaves the view: the terminal
  stays alive for instant re-attach, so the row settles on done once that terminal is closed
  (quitting the viewer, or `Ctrl+X` twice on the row).
- `●` done — finished cleanly.
- `✗` error — exited with an error.
- `?` unknown — the backend cannot say; never shown as a false idle.

Each row is prefixed by its backend's mark in the backend's color — by default the textual
tag `[cc]` Claude (terracotta), `[cx]` Codex (teal), `[oc]` opencode (green) — followed by
the title. Titles share a visible column sized to the widest title, capped at 40 terminal
columns. The state as a word in the state's color (`Working`, `Needs input`, `Idle`, `Done`,
`Error`, `Unknown`) begins the next shared left aligned column, followed by a muted one-line
summary and any Claude pull request badge. Elapsed time alone sits flush right. Claude jobs
with associated pull requests show `#315` for one PR or `2 PRs` for several. The badge is
colored by the PR's live GitHub status: yellow when checks
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
`/tmp` scratch dirs). Press `Ctrl+A` to reveal all of them; the footer shows how many rows are
currently hidden, and `Ctrl+F` searches the hidden rows too. Sessions you spawn from the
viewer are pinned, so they always show even if another source would have marked them a
companion.

## Inline spawn composer

The list view carries a persistent composer in a rounded box between the list and the
footer. Its metadata row shows the backend, the selected model when it is not the default,
and the target folder. The task input is below it, uses the full width, and wraps as it grows.
Just start typing to describe a task. A bracketed multiline paste remains one draft with its
line breaks preserved. `Tab` cycles the target agent among Claude, Codex, and opencode;
`Shift+Tab` cycles that agent's model; and `Enter` explicitly submits the draft and spawns it
detached with that model. A Codex spawn goes into the shared
`codex app-server` daemon so the new session can be joined live later; the viewer starts that
daemon if none is running and never stops one. The models are discovered from each agent's
CLI or catalog (Codex: `codex debug models`; Claude: the models in your `~/.claude.json`;
opencode: `opencode models`), default first. Discovery runs in the background and is cached
for a day, so the picker is populated from the first keystroke rather than waiting on a probe
that takes seconds; a catalog that has never been discovered shows just the agent's default
until its first probe lands. `Shift+Tab` cycles the Claude/Codex lists;
opencode has too many models to cycle, so it stays on its default there (use `/model`). The
target directory is the selected row's project root (by-project view) or its exact cwd
(by-state view). Bare letters, numbers, and slash always type into the composer, including when
it is empty; once you have typed anything, every printable key (and space) is task text, and
`Esc` clears it. After a spawn, the list selects the new row in the first selectable snapshot
that contains it and keeps that selection. When a backend does not return a new identifier, rows
that existed before submission are excluded while finding the new one.

An opencode spawn may start a secured loopback server when neither verified candidate is usable.
It starts from the user home directory and the viewer never stops or restarts it. The viewer uses
an environment password override when present, otherwise a stable generated secret in owner only
credential files. SQLite stores only viewer presentation state. Task shells receive neither
`OPENCODE_SERVER_USERNAME` nor `OPENCODE_SERVER_PASSWORD`. An existing listener that allows
unauthenticated health requests is rejected; it is left untouched and the viewer may use port
`4098` instead. Process shared ownership uses only `flock`; each viewer process serializes its
own work locally.

Type `/model` (optionally `/model <filter>`) to open a filterable picker of every available
model for the target agent, floating above the box. `↑`/`↓` move the highlight, `Tab` or
`Enter` picks the model (and clears the composer), `Esc` closes it.

**Spawned sessions run unsandboxed.** Codex jobs are started with
`--dangerously-bypass-approvals-and-sandbox` on the exec path, and with the same posture
(`sandbox: danger-full-access`, `approvalPolicy: never`) on the daemon path, so they can write
files, use the network, and
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

## Inline rename

`Ctrl+R` turns the selected row itself into an empty edit field. Type the new name, press
`Enter` to commit, or press `Esc` to cancel. `Enter` on a blank field also cancels. The edit
field is not a modal. A Codex rename is retained immediately in its session index, even when
its SQLite title still shows the prompt.

## Keys

- `↑`/`↓` — move selection.
- `→` — attach the selected session in an embedded terminal.
- `Enter` — spawn the composed task, or (empty composer) attach the selected session.
- `Tab` / `Shift+Tab` — cycle the composer's target agent / that agent's discovered models
  (Claude and Codex; opencode has too many, so use `/model` there).
- `/model` — open a filterable picker of every available model for the target agent
  (`↑`/`↓` highlight, `Tab`/`Enter` pick, `Esc` close).
- `Space` — collapse or expand a group when a group header is selected; does nothing on a session row.
- `Ctrl+E` — reserved. Reply is deliberately out of scope for this rebuild (see below);
  pressing it always reports a footer notice that reply is not supported.
- `Ctrl+R` — rename the selected session inline (the row becomes an edit field).
  Capability-gated per row, not per backend: a row the backend cannot rename (an interactive
  Claude row, which has no job dir) is a no-op with a footer notice.
- `Ctrl+X` — stop the selected session; press again within 2s to remove it. A Codex session
  hosted by the app-server daemon is stopped by interrupting its current turn, never by
  signalling a process: the daemon runs every session it hosts, so a signal would take all of
  them down with it.
- `Ctrl+S` — toggle grouping by project / by state.
- `Ctrl+A` — show all (companions + archived + deleted-dir rows).
- `Ctrl+D` / `Ctrl+U` — archive / unarchive (Codex only).
- `Ctrl+F` — filter by title or directory (searches hidden/archived sessions too).
- `Ctrl+T` — toggle mouse reporting. Off hands the mouse back to your terminal so you can
  drag-select and copy text; on restores left click row activation, hover row selection, and
  wheel scrolling. A footer notice names the mode you just switched to.
- `?` — key help.
- `Ctrl+C` — quit.

Renames, stops, removes, and archives run on a background worker so a slow backend call
never freezes the list; a `…` notice shows while the action is in flight.

## Embedded attach

`→` (or `Enter` with an empty composer) opens the selected session inside the viewer as a
full-screen embedded terminal. Codex joins the `codex app-server` daemon that hosts the session
(`codex resume --remote unix://<socket>`) when it hosts one, and otherwise runs a plain
`codex resume`; an exact viewer marked opencode session is refused because attach would expose
credentials, while an external opencode session runs `opencode -s`;
Claude runs `claude attach`, which resumes the same thread for both a running background
job and a finished one (waking it in place); a row with no background-job id falls back to
`claude -r`. `←` returns to the list when the
input line is empty (otherwise it moves the child's cursor), and `Ctrl+]` always detaches.
The attached PTY stays alive in the background so re-attaching is instant.
Mouse reporting starts off for every attached backend, so Codex, Claude, and opencode
transcripts are selectable immediately. Returning to the list restores its mouse behavior;
`Ctrl+T` toggles either mode manually.

**A Codex session that cannot be joined is refused instead of forked.**
Attaching to a mid-turn session the daemon does not host would not join it: the new
`codex resume` process replays the transcript, finds it ends mid-turn, and writes a synthesized
interruption into the running session's transcript, which then renders as "Conversation
interrupted". The viewer refuses that attach and explains why in the footer. This applies only
to `codex exec` sessions (background jobs and plugin dispatches), whose app server runs inside
the session's own process and cannot be joined by anything, including the ChatGPT app.
Sessions you start in a terminal, and sessions the viewer spawns, are hosted by the shared
daemon and are joined live.

Reply is deliberately out of scope for this rebuild. `Ctrl+E` is reserved in the key list but
always reports a footer notice that reply is not supported. Revisiting it is a future,
separately specified decision.

This viewer binds `Space` to group collapse and expand, not to reply, which is a deliberate
divergence from Fleet View, which binds `space` to reply — noted here so it is not mistaken
for an oversight; see the constitution's Additional Constraints.

Quitting the viewer (`Ctrl+C`) kills the attach PTYs it owns, but that does not lose any work:
the conversations live in each backend's own store and re-attach by session ID next time.
If a child exits while you are attached, its final screen stays visible and any key
except `Ctrl+T` returns you to the list; `Ctrl+T` toggles the mouse instead.

Viewer-local presentation state is kept in a SQLite database at
`~/.local/state/agent-viewer/viewer.db`, separate from every backend's own store. OpenCode's own
SQLite database is read only compatibility enumeration, not job authority. OpenCode credentials
are stored only in owner only credential files.

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

The live end-to-end tests are `#[ignore]` by default because they spawn real Codex sessions
(through the app-server daemon and through `codex exec`) and need Codex auth plus network:

```
cargo test -p agent-viewer-core --test e2e_live -- --ignored --nocapture
```
