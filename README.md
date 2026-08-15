# agent-viewer

A terminal viewer for coding-agent sessions, modeled on Claude Code's `claude agents`
view. These CLIs have no built-in "see all my sessions" console; this fills that gap
across Codex and Claude Code. It reads each agent's own local session store, shows every session in a
single live list, and lets you attach to one in an embedded terminal without leaving the
viewer.

The header identifies the viewer as `[av] Agent Viewer` with its version, the full
launch workspace, and live totals for sessions awaiting input, working, and completed
(including errors). On a comfortably sized terminal it also names the active theme and
draws an animated sprite beside all of that; both drop out on a narrow or short terminal
rather than crowding the totals. The terminal tab title is
`Agent Viewer · <launch directory name>`; when the launch directory cannot be determined,
it falls back safely to `Agent Viewer`.

## Backends

Two backends ship, and each advertises which capabilities it supports; the TUI gates
keys accordingly (an unsupported action is a no-op with a footer notice). Backends with
no data or whose CLI is not installed simply list empty — they never error the view.

- **Codex** — full support. Reads the global registry (`~/.codex/state_*.sqlite`, table
  `threads`) plus the rollout transcripts under `~/.codex/sessions/`, resolves live
  running/done/errored status, spawns into the shared `codex app-server` daemon (starting one if
  none is running, and failing the spawn with the daemon's own error rather than quietly
  creating a session nobody could ever join), attaches by joining that daemon
  (`codex resume --remote`), stops a hosted session by interrupting its turn, and
  archives/unarchives.
- **Claude / Claude Code** — enumerate, spawn, attach (`claude attach`), stop nonterminal
  background sessions (`claude stop`), remove (`claude rm`), and rename background
  sessions. Rename writes `name`/`nameSource` into that job's
  `~/.claude/jobs/<short>/state.json`, which is what Claude's own fleet view does and the only
  channel it has (there is no `claude rename` subcommand). It applies to background rows only:
  an interactive row has no job dir, so `Ctrl+R` there is a footer notice. No archive/hide
  (Claude has no hide concept).

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

Each row is prefixed by its backend's mark, followed by
the title. On a terminal that answers the graphics probe the mark is the backend's own inline
brand logo; the viewer always attempts this at startup, so it is what you get on kitty, iTerm2,
Sixel, and half-block terminals alike. When the probe fails (a terminal with no graphics
support, or no tty at all) the mark falls back to the textual tag in the backend's color,
`[cc]` Claude (terracotta) and `[cx]` Codex (teal). Titles share a visible column sized to the widest title, capped at 40 terminal
columns. The state as a word in the state's color (`Working`, `Needs input`, `Idle`, `Done`,
`Error`, `Unknown`) begins the next shared left aligned column, followed by a muted one-line
summary and any pull request badge. Elapsed time alone sits flush right. Sessions with
associated pull requests show `#315` for one PR or `2 PRs` for several. Claude jobs take those
from the job record; Codex sessions take them from the GitHub pull request links in their own
transcript, so a Codex session badges only PRs it successfully opened, and a fresh viewer fills
the badges in over its first few seconds. The badge is
colored by the PR's live GitHub status: yellow when checks
are pending or failing or a review is requested, green when checks have passed, purple when
merged, grey when a draft or closed, and the flat accent color when the status is unknown or
unresolvable.

Wide rows also show a one hour activity ribbon. A session's ribbon includes meaningful activity
from its own recursive subagent subtree, so a parent remains visibly active while its descendants
work. A child row shows only its own subtree.

Set `AGENT_VIEWER_GLYPH_MARKS=1` to use brand glyph marks instead of the textual tags:
`✳` Claude, `◆` Codex (only if your terminal font renders them). This applies to the fallback
only; when the logo probe succeeds the inline images win over both text modes.

The default list groups alphabetic project directories and orders each project's sessions oldest first by creation time. `Ctrl+S` regroups by state in this fixed order: needs input, working, idle, done, with error folding into done and unknown folding into idle. Each section's sessions are oldest first by creation time. A session whose whole title is
exactly `hold` (in any letter case) is not drawn as a row, is not counted in its group's
header, and does not appear in the `Ctrl+K` palette; it is a presentation filter only, and
touches no backend data. `SPEC.md` carries the exact rule.
The list is uncapped and scrolls with the selection to fill the terminal height. A blank line
separates each group/section, and rows sit flush-left under their group header.

Working rows shimmer their glyph, NeedsInput rows stay static with a warning-colored `◐`, and
a session you just spawned blooms once when its row first appears.

## Companion filtering

A single session can surface from more than one source (e.g. a registry row and a
rollout file). The secondary copies are marked **companions** and hidden by default so
the list shows one row per session. One-shot runs started by a script rather than by you
are companions too, because they are steps inside somebody else's job and not fleet members
you would ever attach to: Codex `exec` and bare or malformed subagent rows (the review passes an
`/implement` run fires off, for example). A valid nested `subagent.thread_spawn` row with a
nonempty `parent_thread_id` remains visible by default so its activity can be followed, but it
is still marked as a subagent for safety. Process-hosted ThreadSpawn rows do not advertise stop,
while daemon-hosted ThreadSpawn rows retain the daemon's safe turn interrupt. Archived sessions are hidden
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
line breaks preserved. `Tab` cycles the target agent among only the providers whose CLIs are
installed on `PATH`. When `agent-router` is installed, `auto` joins the cycle and becomes the
starting selection;
`Shift+Tab` cycles that agent's model; and `Enter` explicitly submits the draft and spawns it
detached with that model. A Codex spawn goes into the shared
`codex app-server` daemon so the new session can be joined live later; the viewer starts that
daemon if none is running and never stops one. The models are discovered from each agent's
CLI or catalog (Codex: `codex debug models`; Claude: the models in your `~/.claude.json`),
default first. Discovery runs in the background and is cached
for a day, so the picker is populated from the first keystroke rather than waiting on a probe
that takes seconds; a catalog that has never been discovered shows just the agent's default
until its first probe lands. `Shift+Tab` cycles the Claude/Codex lists. The
target directory is the selected row's project root (by-project view) or its exact cwd
(by-state view). Bare letters, numbers, and slash always type into the composer, including when
it is empty; once you have typed anything, every printable key (and space) is task text, and
`Esc` clears it. After a spawn, the list selects the new row in the first selectable snapshot
that contains it and keeps that selection. It uses the exact returned identifier first, then the
exact returned job name, otherwise bounded cwd and invocation-interval matching while excluding
rows that existed before submission.

### agent-router

Spawning delegates to the sibling [agent-router](https://github.com/TheConnMan/agent-router)
project whenever its CLI is on your `PATH` — **every** spawn, not only the `auto` one. The
router names the job and records the decision, so a job started from the viewer is tracked the
same way as one dispatched from anywhere else.

With the router installed the composer STARTS on a third `auto` entry (one `Tab` reaches the
concrete agents, and the entry sits last in the cycle). It has a single `auto` model, because
the router chooses the provider, model, and reasoning effort itself, scaled to the task's
classified complexity: on `Enter` the viewer runs
`agent-router run --json --dir <target> --provider auto -- "<task>"`, and the router classifies
the task, weighs the weekly usage headroom of each subscription, and dispatches the job. The
footer then shows the decision (for example
`auto: codex gpt-5.6-luna effort low job 0199… (codex weekly 3%, claude 47%)`).

Picking a concrete agent chooses the provider, not a way around the router: the run becomes
`agent-router run --json --dir <target> --provider claude --model 'opus[1m]' -- "<task>"`, the
router honours that override without classifying anything, and the job still gets its derived
name and decision-log row. The footer reads as an ordinary spawn
(`spawned on claude opus[1m] job Fix The Parser (codex weekly 3%, claude 47%)`). Codex's
`default` model sends no `--model` at all, leaving that choice to the router as before.

Either way the new session appears and is selected through the dispatching agent's normal
listing. Without the binary installed the `auto` entry never appears, the composer starts on
Claude, and spawns call the agent's own CLI directly exactly as they used to. A router that
fails (non-zero exit, timeout, unreadable output, or a pinned provider it did not honour) is a
footer error with nothing spawned — never a fallback to a guessed provider, and never a silent
retry straight at the agent.

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
codex prompts under `~/.codex/prompts`),
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
  (Claude and Codex).
- `/model` — open a filterable picker of every available model for the target agent
  (`↑`/`↓` highlight, `Tab`/`Enter` pick, `Esc` close).
- `Space` / `Enter` — collapse or expand a group when a group header is selected (the collapse
  is persisted); on a session row `Space` does nothing and `Enter` attaches.
- `/theme` — open the theme picker: `↑`/`↓` preview a theme against the whole screen, `Enter`
  commits and persists it, `Esc` reverts to the one you started on.
- `Ctrl+K` — command palette. It carries every action in this list (each with its chord, and
  unavailable ones shown as such), plus the header sprites, every visible session to jump to,
  every discovered model for each backend, and the slash commands for the composer's backend.
- `Ctrl+B` — toggle the tail pane, the last 12 turns of the selected session beside the list.
  It needs at least 100 columns; below that opening it is a footer notice naming the width.
- `Ctrl+W` — video wall (see below). `Ctrl+O` zooms the focused tile to the full attach view.
- `Ctrl+G` — cycle the header sprite; the choice is announced and persisted.
- `Ctrl+E` — reserved. Reply is deliberately out of scope for this rebuild (see below); on a
  selected session it reports a footer notice that reply is not supported, and with nothing
  selected it does nothing at all.
- `Ctrl+N` — open the triage inbox on every session waiting for input, longest wait first
  (see below). Nothing waiting is a footer notice, not a modal.
- `Ctrl+R` — rename the selected session inline (the row becomes an edit field).
  Capability-gated per row, not per backend: a row the backend cannot rename (an interactive
  Claude row, which has no job dir) is a no-op with a footer notice.
- `Ctrl+X` — stop the selected session; press again within 2s to queue its removal after stop
  succeeds. If stop fails, removal is discarded and confirmation is cleared, so the next press
  retries stop. Claude stops a nonterminal background session with `claude stop`,
  retaining it for later attach.
  A Codex session hosted by the app-server daemon is stopped by interrupting its current turn,
  never by signalling a process: the daemon runs every session it hosts, so a signal would take
  all of them down with it. A process-hosted Codex subagent row advertises no stop and reports
  `codex does not support stop`, because the only pid on that row is its parent's and signalling it
  would take the parent and every sibling down with it. A daemon-hosted ThreadSpawn subagent keeps
  the daemon's safe turn interrupt.
- `Ctrl+S` — toggle grouping by project / by state.
- `Ctrl+A` — show all (companions + archived + deleted-dir rows).
- `Ctrl+D` / `Ctrl+U` — archive / unarchive (Codex only).
- `Ctrl+F` — filter by title or directory (searches hidden/archived sessions too).
- `Ctrl+Y` — while attached, send the exact visible PTY viewport to the client terminal as an
  OSC 52 clipboard request. This includes a visible scrolled historical viewport. It does not
  change mouse capture or reach the child. A completed write reports only
  `copy request sent to terminal`; an output failure reports that the terminal clipboard state
  is unknown. This requires Windows Terminal or another terminal that supports OSC 52. Use
  `Ctrl+T` and host terminal selection as the fallback.
- `Ctrl+T` — toggle mouse reporting. The list starts with capture on for left click activation,
  hover row selection, and wheel scrolling. Every successful Codex or Claude attach or reattach
  starts with capture on, so scrolling works immediately. While attached, press `Ctrl+T` to
  switch capture off for host terminal text selection, then press it again to restore scrolling.
  Detaching restores list mouse controls. A footer notice names the mode and
  the way back.
- `?` — key help.
- `Ctrl+C` — quit.

Renames, stops, removes, and archives run on a background worker so a slow backend call
never freezes the list; a `…` notice shows while the action is in flight, and pressing the
same action again on the same row while it is still running reports `still <verb>` rather
than queueing a second call.

## Embedded attach

`→` (or `Enter` with an empty composer) opens the selected session inside the viewer as a
full-screen embedded terminal. Codex joins the `codex app-server` daemon that hosts the session
(`codex resume --remote unix://<socket>`) when it hosts one, and otherwise runs a plain
`codex resume`; Claude runs `claude attach`, which resumes the same thread for both a running background
job and a finished one (waking it in place); a row with no background-job id falls back to
`claude -r`. `←` returns to the list when the
input line is empty (otherwise it moves the child's cursor), and `Ctrl+]` always returns.
A session is connected exactly while it is on screen: leaving it closes that connection, so
there is never a session still connected in the background. Opening it again reconnects, which
takes a second or two. Conversation state lives in each backend's own store, so nothing is lost
by closing. Resolving an attach can take those seconds, so if you have moved on by the time it
lands the viewer drops it rather than yanking you into a session you left: the footer reads
`attach cancelled: <title> is no longer in focus` and nothing was spawned.
New attached PTYs use the active viewer theme's text and background as their terminal defaults;
explicit indexed and RGB child colors are preserved. The built in terminal match theme instead
uses the captured host foreground and background. Because a session reconnects when you open it,
a theme change applies to the next session you open.
Codex and Claude attached transcripts scroll immediately: Codex scrolls the viewer's retained
transcript, while Claude receives the wheel in its attached terminal. Their capture behavior
follows the `Ctrl+T` controls above.

### Video wall

`Ctrl+W` replaces the session list with a grid of everything that is running, and gives the
keyboard to one tile so you can answer a session without leaving the grid. It connects each
session for you through the same path a manual attach uses, so there is nothing to set up
first; a tile shows `connecting…` until its session is live, or the reason it could not be
joined. `Ctrl+W` again closes the wall and every connection it opened.

A session earns a tile while it is working or awaiting input, and keeps it for fifteen minutes
after it stops. A run that just finished is exactly when the wall is most useful — the reply
you want to type is the one that follows the result you watched arrive — so the tile stays put
instead of vanishing at that moment.

The grid is 1x1 for one session, 2x1 for two, 2x2 for three or four, 3x2 for five or six, and
3x3 up to nine. Nine is the cap, because every tile is a live child process; beyond that the
footer reads `showing 9 of N` rather than dropping the rest silently. All eligible sessions,
whether still running or recently stopped, are ordered by creation time ascending; equal times
break by backend and then ID. The earliest nine take the slots, and status changes do not move a
tile. Each tile carries its
state glyph, backend mark, title, project, and elapsed time, dropping the project then the
elapsed as tiles get narrower.

Every tile outlines itself in the colour of what its session is doing, so the wall sorts itself
at a glance: a session waiting on you flashes its border in the theme's attention colour (pink
in most themes, and the same colour as the `◐` glyph everywhere else in the viewer), a session
that has finished goes solid green, one that broke goes red, and one that is still working
keeps the plain border — most of a busy wall is working, and a wall that shouts everywhere
shouts nowhere. Focus is a separate channel: the tile with the keyboard takes a thick, bold
border plus the caret in its header, so pointing at a tile never hides what that tile is doing.
Only the border of a blocked tile moves; tiles retain their creation-time position even when a
session changes status, so you keep your place in whatever you were reading. A theme with animation
off holds the attention colour solid instead of flashing.

Hover or click a tile to focus it, or move with `Shift+↑↓←→`. The mouse wheel scrolls the tile
under the pointer back through what it has already printed, without moving the focus — that is
the viewer's own viewport over the session's retained output, so it works the same whatever the
backend does with mouse reports. `Ctrl+O` zooms the focused tile to the full attach view,
reusing the connection the wall already holds.

`Ctrl+X` retires the focused tile without leaving the grid, using the same two stages as the
list: the first press stops the session, and a second press within two seconds removes it. A
session that has already finished is not stopped again, so its first press only arms the
removal and the footer reads `[press Ctrl+X again to remove]` until the window closes. Removing
a session drops it from every view, and its tile and connection go with it. `Ctrl+K` opens the
palette over the wall, aimed at the focused tile, for the actions the grid has no chord of its
own for — archive, rename, stop or remove, or jumping to another session.

Starting a new task goes through that palette too: `Ctrl+K`, then `New session`, floats the
spawn composer over the grid. It is the list's composer unchanged — `Tab` switches agent,
`⇧Tab` cycles its model, and slash commands complete as they do below the list — so describe
the task and press `Enter` to start it, and the keyboard goes straight back to the tiles. `Esc`
backs out and keeps what you had typed, so glancing at a tile mid-sentence never costs the
draft. `Ctrl+C` backs out the same way rather than quitting the viewer, and `Ctrl+W` and
`Ctrl+]` still leave for the list — every way out of the box keeps the draft. Picking a slash
command or a model from the palette opens the same box, since both write into the composer and
the grid has nowhere else to show it.

Everything else you type —
plain arrows, `Enter`, `Esc`, `Ctrl+C`, and every other chord — goes to the focused session:
`Esc` interrupts it and `Ctrl+C` clears its input line, exactly as they would if you had
attached to it. That holds while the grid has the keyboard; the compose overlay above takes it
back for as long as it is up. `Ctrl+W` and `Ctrl+]` are the unconditional ways back to the list. The composer
is not drawn while the wall is up, since the tiles have the keyboard, except while it is holding
that keyboard itself as the overlay above.

**A Codex session that cannot be joined is refused instead of forked.**
Attaching to a mid-turn session the daemon does not host would not join it: the new
`codex resume` process replays the transcript, finds it ends mid-turn, and writes a synthesized
interruption into the running session's transcript, which then renders as "Conversation
interrupted". The viewer refuses that attach and explains why in the footer. This applies only
to `codex exec` sessions (background jobs and plugin dispatches), whose app server runs inside
the session's own process and cannot be joined by anything, including the ChatGPT app.
Sessions you start in a terminal, and sessions the viewer spawns, are hosted by the shared
daemon and are joined live.

## Triage inbox

`Ctrl+N` (or "Triage sessions waiting for input" in the `Ctrl+K` palette) opens a modal over
the list that walks every session waiting for input, one at a time, longest wait first, with
a `3 of 7` progress counter and the next few items named underneath.

The panel in the middle is the session itself, live — the same attach the list's `Enter`
gives you, drawn into the panel instead of the whole screen. So you see the agent's own
interface: its real question, its own numbered options, whatever it drew. Every key you press
goes to it, and your answer is delivered by the agent's own input handling.

Three chords are reserved for the queue: `Ctrl+N` next, `Ctrl+P` previous, `Ctrl+]` leave.
Everything else — `Enter`, `Esc`, the arrows, digits, paste — belongs to the session, because
those are how you answer a prompt. Running off the end closes the modal; it never wraps.

Attaching is the existing attach, so triage inherits each backend's semantics exactly (Claude
resumes the same thread; Codex goes through the app-server daemon) and invents no second way
to reach a session. Sessions are attached one at a time as you reach them, never prefetched
for the whole queue, and leaving the modal detaches without stopping anything. The queue is
snapshotted when the modal opens, so a background refresh cannot reorder it mid-answer, and
nothing in the modal touches the composer or the list selection. A visit lasts exactly as long
as the item is on screen: moving to the next or previous item closes the one you left, and so
does closing the queue, so a walk through seven sessions never leaves seven children running
behind it.

Reply is deliberately out of scope for this rebuild. `Ctrl+E` is reserved in the key list but
only reports a footer notice that reply is not supported. Revisiting it is a future,
separately specified decision. The triage inbox does not depend on it: it delivers an answer
by typing into the attached session, which is the agent's own input path, not a reply API.

This viewer binds `Space` to group collapse and expand, not to reply, which is a deliberate
divergence from Fleet View, which binds `space` to reply. Peek and reply were removed from this
rebuild because they confused the interaction model, so the rebinding is a considered choice and
not an oversight; revisiting either is a future, separately specified decision.

Quitting the viewer (`Ctrl+C`) kills the attach PTYs it owns, but that does not lose any work:
the conversations live in each backend's own store and re-attach by session ID next time.
If a child exits while you are attached, its final screen stays visible. `Ctrl+Y` remains
available to send that retained visible screen, and `Ctrl+T` remains available for host
selection. Any other key returns you to the list.

## Themes

Eleven themes ship: `analog amber` (the default), `terminal match`, `paper light`, `mono 16`,
`aubergine`, `hoth`, `catppuccin mocha`, `tokyo night`, `gruvbox dark`, `rose pine`, and `nord`.
Type `/theme` in the composer to open the picker; `↑`/`↓` preview each one against the whole
screen, `Enter` commits, `Esc` reverts. The choice is persisted, so the next launch opens on it.
Marks and motion travel with the theme: `terminal match` builds itself from your terminal's own
palette, and a theme with animation off holds colors solid instead of blinking or shimmering.

Your own themes go in `~/.config/agent-viewer/themes` as `*.theme` files, one `key=#rrggbb` per
line, `#` comments allowed. They join the picker alongside the built-ins. The active user theme
is reloaded whenever its file's timestamp changes, so you can edit one with the viewer running
and watch each save land. A malformed line is skipped with a footer notice rather than failing
the file.

## Sprites

The header carries one of six animated sprites: `lighthouse` (the default), `constellation`,
`turbine`, `sailboat`, `airplane`, and `hot air balloon`. `Ctrl+G` cycles to the next one and
persists it, and the `Ctrl+K` palette picks one directly. `AV_SPRITE=<name>` opens on that
sprite for one run without overwriting the saved choice; an unknown name falls back rather than
erroring.

## Configuration

There is no config file. The viewer reads:

- `AGENT_VIEWER_GLYPH_MARKS=1` uses the brand glyphs `✳`/`◆` instead of the `[cc]`/`[cx]` text
  tags when the inline logo marks are unavailable.
- `AV_SPRITE=<name>` opens on that header sprite for one run.
- `AGENT_VIEWER_CODEX_EXEC_SPAWN=1` spawns Codex sessions with `codex exec` instead of the
  app-server daemon. Those sessions can never be attached to, which is why it is opt-in.
- `CODEX_HOME` is where the Codex registry and rollouts live. Defaults to `~/.codex`.
- `CLAUDE_CONFIG_DIR` is where Claude's `jobs/` and `sessions/` live. Defaults to `~/.claude`.
- `~/.config/agent-viewer/themes/*.theme` holds your own themes, as described above.
- `~/.local/state/agent-viewer/viewer.db` is the only database the viewer writes, holding
  presentation state only: the theme, the sprite, the age ramp, which groups are collapsed, the
  cached model catalogs, the shared listing cache, and the record of sessions this viewer
  spawned (which is what keeps your own spawns from being filtered away as companions). Deleting
  it resets all of that to defaults and costs nothing else: no conversation, session, or backend
  state lives here.

## Troubleshooting

- **The list is empty.** Each backend fails quietly and independently. A missing
  `~/.codex/state_*.sqlite` shows `codex: no state_*.sqlite found under <path>` in the footer
  with an empty Codex list; if no backend can be listed at all, the viewer exits with
  `agent-viewer: no backend could be listed` rather than showing a blank screen. A CLI that is
  not on `PATH` simply drops out of the composer's `Tab` cycle.
- **A Codex session refuses to attach.** `codex exec` sessions (background jobs and plugin
  dispatches) host their app server inside their own process, so nothing can join them. Joining
  one mid-turn would write a fake interruption into its transcript, so the viewer refuses and
  says so in the footer. Sessions started in a terminal, and sessions this viewer spawns, are
  hosted by the shared daemon and attach normally.
- **Rows show `[cc]`/`[cx]` instead of logos.** The startup graphics probe needs a real terminal
  that answers it. On one that does not, the text tags are the fallback; set
  `AGENT_VIEWER_GLYPH_MARKS=1` for the glyphs instead.
- **`Ctrl+]` seems to do nothing.** Terminals send it as the raw byte `0x1D`, which some parsers
  report as `Ctrl+5`. Both encodings are accepted everywhere `Ctrl+]` is documented, so if it
  still does not detach, the chord is being swallowed before the viewer sees it.
- **`Ctrl+B` will not open.** The tail pane needs 100 columns; below that it reports the width
  it has instead of opening.

Built in Rust. See `SPEC.md` for the full architecture and the evidence behind it.

## Build

```
cargo build --workspace
```

## Releases

Pushing a tag that matches `v*` builds native release binaries and publishes a GitHub Release.
Each release has these archives and a sibling SHA256 file for each one:

- `agent-viewer-x86_64-unknown-linux-gnu.tar.gz`
- `agent-viewer-x86_64-apple-darwin.tar.gz`
- `agent-viewer-aarch64-apple-darwin.tar.gz`
- `agent-viewer-x86_64-pc-windows-msvc.zip`

Verify an archive before unpacking it with `sha256sum --check <archive>.sha256` on Linux,
`shasum -a 256 --check <archive>.sha256` on macOS, or
`$expected=((Get-Content <archive>.sha256 -Raw).Trim() -split '\s+')[0]; if ($expected -ine (Get-FileHash <archive> -Algorithm SHA256).Hash) { throw 'SHA256 mismatch' }`
in PowerShell on Windows. Unpack a `.tar.gz` with
`tar -xzf <archive>` and the Windows `.zip` with `Expand-Archive <archive>`.

## Install

Download the archive for your platform from the
[releases page](https://github.com/TheConnMan/agent-viewer/releases), verify it against its
sibling SHA256 file as described above, unpack it, and put the `agent-viewer` binary on your
`PATH`. On linux-x86_64:

```
tar -xzf agent-viewer-x86_64-unknown-linux-gnu.tar.gz
mv agent-viewer ~/.local/bin/
```

To build from source instead:

```
cargo install --git https://github.com/TheConnMan/agent-viewer agent-viewer-tui
```

A source build uses the repository's vendored vt100 patch, so it pulls the whole repo rather
than the published crate.

## Run

```
cargo run -p agent-viewer-tui
```

The binary is named `agent-viewer`. It expects a `~/.codex/state_*.sqlite` on the box
(the Codex backend's source of truth); the Claude backend appears
automatically when its CLI and data exist and silently lists empty otherwise.

Linux retains the full measured runtime behavior. macOS and Windows can enumerate and render
sessions, but do not claim Linux process status or Codex daemon controls. On those
platforms, a rollout is `Done` only when its transcript proves
completion; every other process dependent state is `Unknown`. Unsupported actions remain
no ops with the existing footer notice. `agent-viewer --version` and `agent-viewer -V` print
the version before terminal, filesystem, or backend startup, making them safe release smoke
paths.

## Test

```
cargo test --workspace
```

The live end-to-end tests are `#[ignore]` by default because they spawn real Codex sessions
(through the app-server daemon and through `codex exec`) and need Codex auth plus network:

```
cargo test -p agent-viewer-core --test e2e_live -- --ignored --nocapture
```
