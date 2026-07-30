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
- **Claude / Claude Code** — enumerate, spawn, attach (`claude attach`), stop working or
  needs input background sessions (`claude stop`), remove (`claude rm`), and rename background
  sessions. Rename writes `name`/`nameSource` into that job's
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
summary and any pull request badge. Elapsed time alone sits flush right. Sessions with
associated pull requests show `#315` for one PR or `2 PRs` for several. Claude jobs take those
from the job record; Codex sessions take them from the GitHub pull request links in their own
transcript, so a Codex session badges the PRs it opened or reviewed, and a fresh viewer fills
the badges in over its first few seconds. The badge is
colored by the PR's live GitHub status: yellow when checks
are pending or failing or a review is requested, green when checks have passed, purple when
merged, grey when a draft or closed, and the flat accent color when the status is unknown or
unresolvable.

Wide rows also show a one hour activity ribbon. A session's ribbon includes meaningful activity
from its own recursive subagent subtree, so a parent remains visibly active while its descendants
work. A child row shows only its own subtree.

Set `AGENT_VIEWER_GLYPH_MARKS=1` to use brand glyph marks instead of the textual tags:
`✳` Claude, `◆` Codex, `■` opencode (only if your terminal font renders them).

The default list groups alphabetic project directories and orders each project's sessions oldest first by creation time. `Ctrl+S` regroups by state in this fixed order: needs input, working, idle, done, with error folding into done and unknown folding into idle. Each section's sessions are oldest first by creation time. After sanitization, the exact whole title `hold` is matched without regard to ASCII letter case as a TUI presentation convention. Thus `hold`, `Hold`, and `HOLD` session rows are omitted in either grouping, while whitespace and substring variants remain visible. Project headers count rendered non hold sessions, so a project with only matching sessions remains with count zero. State section counts include only rendered rows. `Ctrl+K` quickswitcher session entries use the same visible row model, so matching sessions are omitted while ordinary sessions and independent quickswitcher actions remain. This does not change backend data or mutation behavior.
The list is uncapped and scrolls with the selection to fill the terminal height. A blank line
separates each group/section, and rows sit flush-left under their group header.

Working rows shimmer their glyph, NeedsInput rows stay static with a warning-colored `◐`, and
a session you just spawned blooms once when its row first appears.

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
line breaks preserved. `Tab` cycles the target agent among only the providers whose CLIs are
installed on `PATH`. When `agent-router` is installed, `auto` joins the cycle and becomes the
starting selection;
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
that contains it and keeps that selection. It uses the exact returned identifier first, then the
exact returned job name, otherwise bounded cwd and invocation-interval matching while excluding
rows that existed before submission.

### Auto (agent-router)

Spawning delegates to the sibling [agent-router](https://github.com/TheConnMan/agent-router)
project when its CLI is on your `PATH`: the composer then STARTS on a fourth `auto` entry
(one `Tab` reaches the concrete agents, and the entry sits after opencode in the cycle). It
has a single `auto` model, because the router chooses the provider, model, and reasoning
effort itself, scaled to the task's classified complexity: on `Enter` the viewer runs
`agent-router run --json --dir <target> --provider auto -- "<task>"`, and the router classifies the task, weighs
the weekly usage headroom of each subscription, and dispatches the job. The footer then shows
the decision (for example `auto: codex gpt-5.6-luna effort low job 0199… (codex weekly 3%, claude 47%)`),
and the new session appears and is selected through the winning agent's normal listing. Without
the binary installed the entry never appears at all and the composer starts on Claude, and a
router that fails (missing, non-zero exit, timeout, unreadable output) is a footer error with
nothing spawned, never a fallback to a guessed provider.

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
- `Ctrl+X` — stop the selected session; press again within 2s to queue its removal after stop
  succeeds. If stop fails, removal is discarded and confirmation is cleared, so the next press
  retries stop. Claude stops a working or needs input background session with `claude stop`,
  retaining it for later attach.
  A Codex session hosted by the app-server daemon is stopped by interrupting its current turn,
  never by signalling a process: the daemon runs every session it hosts, so a signal would take
  all of them down with it.
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
  External opencode attaches with capture off for selection; press `Ctrl+T` to opt into its native
  wheel forwarding. Detaching restores list mouse controls. A footer notice names the mode and
  the way back.
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
New attached PTYs use the active viewer theme's text and background as their terminal defaults;
explicit indexed and RGB child colors are preserved. The built in terminal match theme instead
uses the captured host foreground and background. Because attached PTYs are reused, changing
the theme then reentering a retained session refreshes its default foreground and background
without restarting the child.
Codex and Claude attached transcripts scroll immediately: Codex scrolls the viewer's retained
transcript, while Claude receives the wheel in its attached terminal. Their capture behavior and
external opencode selection behavior follow the `Ctrl+T` controls above.

When a Linux viewer is displayed remotely through Windows Terminal or another terminal with
OSC 52 support, `Ctrl+Y` sends the exact visible PTY viewport to that client terminal. The
request includes only `screen.contents()`, so scrolling first sends the visible historical
viewport rather than text outside it. The chord leaves mouse capture unchanged and is not
forwarded to the child. A complete output write means only `copy request sent to terminal`,
because OSC 52 has no acknowledgement; an output failure means the terminal clipboard state is
unknown. Terminal policy may still reject the request. Use `Ctrl+T` to disable capture and select
text in the host terminal when OSC 52 is unavailable.

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
If a child exits while you are attached, its final screen stays visible. `Ctrl+Y` remains
available to send that retained visible screen, and `Ctrl+T` remains available for host
selection. Any other key returns you to the list.

Viewer-local presentation state is kept in a SQLite database at
`~/.local/state/agent-viewer/viewer.db`, separate from every backend's own store. OpenCode's own
SQLite database is read only compatibility enumeration, not job authority. OpenCode credentials
are stored only in owner only credential files.

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

## Run

```
cargo run -p agent-viewer-tui
```

The binary is named `agent-viewer`. It expects a `~/.codex/state_*.sqlite` on the box
(the Codex backend's source of truth); the Claude and opencode backends appear
automatically when their CLIs and data exist and silently list empty otherwise.

Linux retains the full measured runtime behavior. macOS and Windows can enumerate and render
sessions, but do not claim Linux process status, Codex daemon controls, or secure managed
opencode behavior. On those platforms, a rollout is `Done` only when its transcript proves
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
