# Feature Specification: Fleet View Unification

**Feature Branch**: `main` (developed directly on main per user decision; no feature branch)

**Created**: 2026-07-26

**Status**: Draft

**Input**: User description: "We need to back out specs using git speckit out of chat transcripts
and git log and current features. I want to go back to basics with the specs, then potentially
regenerate the code because I'm not thrilled with the current product... the actual requirement
is replicate fleet view [`claude agents`], a single unified fleet view across codex, claude, and
opencode. that's the vision of this project."

## Clarifications

### Session 2026-07-26

- Q: FR-009/SC-006 only say state changes must show "within a short, consistent delay" —
  unquantified. What's the actual target? → A: Real-time/event-driven update is preferred where
  achievable without disproportionate engineering effort; where it is not, a poll-based refresh
  is acceptable as long as a state change is visible within 2 seconds. The exact mechanism
  (subscription vs. poll) remains a `/speckit-plan` decision per the constitution's deferred
  binding-depth ramification; the 2-second bound is now a hard requirement regardless of which
  mechanism is chosen.
- Q: What happens when attach cannot reach the live session (daemon worker gone, socket stale)?
  → A: Show a clear inline error naming the reason and leave the user on the list with the row
  still selected. Never fall back to a path that could spawn a second client/session — a failed
  attach must not risk recreating the duplicate-session problem Principle IV exists to prevent.

## User Scenarios & Testing *(mandatory)*

### User Story 1 - One live list across every backend (Priority: P1)

A user with active work in Claude Code, Codex, and opencode opens agent-viewer and sees every
session that matters — across all three tools — in a single list, grouped by project directory,
in the same visual language as `claude agents` ("Fleet View"): a status glyph, the session name,
a state word, a one-line summary, and a right-aligned age. Sessions belonging to a project the
user no longer cares about, or noise sessions a backend generates internally (e.g. Codex
subagent/companion threads), never appear.

**Why this priority**: This is the entire premise of the rebuild. Without it there is no
product — everything else is refinement of this one list.

**Independent Test**: Launch agent-viewer with real Claude, Codex, and opencode session history
present. Confirm rows are present for sessions from all three backends, grouped by project,
formatted like Fleet View, with zero Codex subagent rows and zero rows for archived/dead-cwd
sessions.

**Acceptance Scenarios**:

1. **Given** at least one active Claude session, one active Codex session, and one active
   opencode session exist in the same project directory, **When** the user opens agent-viewer,
   **Then** all three appear as separate rows under that project's group header.
2. **Given** a Codex thread whose `source` is a subagent/companion marker, **When** the list is
   built, **Then** that thread never appears as a row.
3. **Given** a backend's CLI is not installed or has no session data, **When** the list is built,
   **Then** that backend contributes zero rows and no error is shown.
4. **Given** the user has never used opencode, **When** they open agent-viewer, **Then** Claude
   and Codex rows still render normally.

---

### User Story 2 - Attach lands in the real session, not a copy (Priority: P1)

A user selects a live or resumable session and attaches to it. They land inside the actual
running (or resumable) backend session — the same one they would reach via that backend's own
resume/attach command — with no new session created as a side effect.

**Why this priority**: Named directly by the user as the top complaint with the current build
("I want to attach to the real bg session, so I don't get a duplicate in the claude agents").
A viewer that duplicates the thing it is supposed to be viewing is worse than no viewer.

**Independent Test**: Note the session count reported by each backend's own listing (e.g.
`claude agents --json --all` count, Codex thread count) before attaching. Attach to a session,
interact, detach. Re-query each backend's own listing. Counts are unchanged; no new row appears
anywhere.

**Acceptance Scenarios**:

1. **Given** a running Claude background session, **When** the user attaches from agent-viewer,
   **Then** `claude agents --json --all` reports the same session count before and after.
2. **Given** a finished (resumable) session in any backend, **When** the user attaches, **Then**
   they resume the same conversation history, not a fresh one.
3. **Given** the user detaches and later re-attaches to the same session, **When** they do so,
   **Then** they see the same in-progress state they left (no lost output, no restart).

---

### User Story 3 - Rename is real, not a local fiction (Priority: P2)

A user renames a session from agent-viewer. The new name is authoritative: the backend's own
view of that session (e.g. `claude agents`) shows the same name. If the backend rejects the
rename, agent-viewer says so rather than quietly keeping a different name locally.

**Why this priority**: A second named complaint. A rename that only agent-viewer believes is
worse than no rename feature, because it silently produces disagreeing sources of truth.

**Independent Test**: Rename a session in agent-viewer. Query the backend's own listing for that
session's name. Names match. If the backend's rename channel is unavailable, agent-viewer
surfaces that the rename did not take effect natively, rather than showing success.

**Acceptance Scenarios**:

1. **Given** a renamable session, **When** the user renames it in agent-viewer, **Then** the
   backend's own tooling reports the new name for that session.
2. **Given** a backend rejects a rename attempt, **When** this happens, **Then** agent-viewer
   does not present the rename as having succeeded.

---

### User Story 4 - Switch between project view and state view (Priority: P2)

A user toggles between grouping sessions by project directory and grouping them by state
(Needs input / Working / Ready for review / Completed), using the same key and producing the
same section structure as Fleet View — now spanning all three backends instead of one.

**Why this priority**: This is a defining, well-liked Fleet View behavior the user explicitly
wants preserved, not reinvented, across the unified session set.

**Independent Test**: With a mix of working, needs-input, and completed sessions across
backends, toggle the view. Confirm sessions regroup into the same four sections Fleet View uses,
each session appearing in exactly one section consistent with its current state.

**Acceptance Scenarios**:

1. **Given** the project-grouped view is active, **When** the user presses the state-toggle key,
   **Then** the list regroups into Needs input / Working / Ready for review / Completed
   sections, populated from all three backends.
2. **Given** a Codex session has an open, unmerged PR associated with its branch, **When** state
   view is active, **Then** it appears in "Ready for review" the same way a Claude session with
   an open PR does.
3. **Given** an opencode session has no PR/branch data, **When** state view is active, **Then**
   it is grouped by its working state (never placed in "Ready for review").

---

### User Story 5 - The list stays live and stays small (Priority: P3)

A user watching agent-viewer sees a session's status change (e.g. Working → Needs input) without
a noticeable lag behind what the backend itself would report, and the list never grows
unmanageable even though one backend (Codex) may hold thousands of historical sessions. Finished
sessions age out of the default view after a bounded window; sessions still needing attention
never do, no matter how old.

**Why this priority**: Makes the unified list usable rather than merely correct. Lower priority
than P1/P2 because a correct-but-occasionally-stale or occasionally-long list is still usable;
an incorrect or duplicating one (P1/P2) is not.

**Independent Test**: Leave a session running and watch it complete; confirm the row updates
without requiring the user to take an action to refresh it, and within 2 seconds. Separately,
confirm that a needs-input session older than the retention window still appears,
while a completed session older than the window does not (until "show all" is invoked).

**Acceptance Scenarios**:

1. **Given** a working session finishes, **When** its backend reflects that, **Then**
   agent-viewer's row updates without manual refresh, within 2 seconds (sooner where a
   real-time/event-driven update path is achievable without disproportionate engineering effort).
2. **Given** a needs-input session that has been waiting far longer than the retention window,
   **When** the default list is shown, **Then** it still appears.
3. **Given** a completed session older than the retention window, **When** the default list is
   shown, **Then** it is hidden unless the user asks to see everything.

---

### Edge Cases

- A backend CLI is installed but its data store is temporarily unreadable (e.g. mid-write) —
  the list must degrade to "no rows from that backend this refresh," never crash or block the
  other backends' rows.
- Two sessions from different backends share the same project directory and similar names — the
  backend identity marker must make them visually distinguishable at a glance.
- A user issues the "delete all" action with nothing selected — it must act only on the sessions
  currently visible in the list (respecting active filters/retention), never on backend history
  outside what is shown.
- A session is mid-rename when the backend process for it exits — the rename attempt must fail
  cleanly rather than appear to hang or silently succeed.
- Codex's thread volume (thousands of rows, most historical) must never make the list
  perceptibly slower to open or refresh than a Claude-only session list would be today.
- A session with an associated PR that has since been merged or closed must not linger in
  "Ready for review."
- An interactive (non-background) session for any backend must still appear in the list with
  accurate state, even where the backend's richer detail (summary, transcript) is only available
  for background-style sessions.
- Attach cannot reach the live session (e.g. a stale daemon worker or dead socket) — the user
  sees a clear inline error naming the reason, remains on the list with the row still selected,
  and no fallback path is attempted that could spawn a second client/session.

## Requirements *(mandatory)*

### Functional Requirements

- **FR-001**: The system MUST display, in one list, sessions from the Claude, Codex, and
  opencode backends, grouped by project directory by default. *(Principle III)*
- **FR-002**: The system MUST visually match Fleet View's row format (status glyph, name, state
  word, summary, right-aligned age) and MUST NOT introduce a state vocabulary Fleet View does
  not use, except where required to distinguish backend identity. *(Principle I)*
- **FR-003**: The system MUST exclude Codex subagent/companion threads, archived sessions, and
  sessions whose working directory no longer exists from the default list. *(Principle III)*
- **FR-004**: The system MUST let a user attach to a session such that they join the actual
  running or resumable backend session, verified by the backend's own session count being
  unchanged by the attach action. *(Principle IV)*
- **FR-005**: The system MUST NOT spawn a second client process on attach that the backend then
  treats as a distinct, separately-listed session. *(Principle IV)*
- **FR-005a**: When attach cannot reach the live session (e.g. a stale daemon worker or dead
  socket), the system MUST show a clear inline error naming the reason and MUST leave the user
  on the list with the row still selected. It MUST NOT fall back to any path that could spawn a
  second client/session to force the attach through. *(Principle IV; Clarification
  2026-07-26)*
- **FR-006**: The system MUST perform rename through the backend's native rename channel and
  MUST report failure (not silent local success) when that channel rejects or is unavailable.
  *(Principle II)*
- **FR-007**: The system MUST support toggling between project-grouped and state-grouped views,
  using Fleet View's own key binding and section names (Needs input / Working / Ready for
  review / Completed). *(Principle I)*
- **FR-008**: The system MUST populate "Ready for review" from both Claude sessions (native PR
  association) and Codex sessions (PR resolved via the session's git branch), and MUST exclude a
  session from that section once its PR is merged or closed. *(Additional Constraints)*
- **FR-009**: The system MUST reflect a session's state change without requiring a manual
  refresh, within 2 seconds. A real-time/event-driven update path SHOULD be used where it is
  achievable without disproportionate engineering effort; where it is not, poll-based refresh
  meeting the 2-second bound is acceptable. Which mechanism is used is a `/speckit-plan` decision
  (see Additional Constraints — binding depth); the 2-second bound applies regardless of choice.
  *(Principle V; Clarification 2026-07-26)*
- **FR-010**: The system MUST always show sessions in a live or needs-input state regardless of
  age, and MUST hide finished (done/failed/stopped) sessions older than a configurable retention
  window from the default view. *(Principle V)*
- **FR-011**: The system MUST adopt Fleet View's key map for every action Fleet View also has
  (open, delete-selected, delete-all-when-unselected, switch view, quit, mention, newline), with
  no reassignment of a key Fleet View already binds to something else. *(Principle I)*
- **FR-012**: The system MUST distinguish which backend a session belongs to at a glance, without
  altering Fleet View's row layout in a way that changes for single-backend users. *(Principle
  III, informed by user: distinguishing marker is a per-backend glyph)*
- **FR-013**: The "delete all" action, when nothing is selected, MUST operate only on the
  sessions currently visible in the list (post-filter, post-retention), never on backend history
  outside the current view. *(Edge case, Principle V)*
- **FR-014**: `agent-viewer-core` MUST remain free of UI-toolkit types; all logic in FR-001
  through FR-013 MUST be implemented in `-core` with the TUI as a thin renderer over it.
  *(Principle VI)*
- **FR-015**: Peek and reply are explicitly out of scope for this feature and MUST NOT be
  implemented as part of it. *(Additional Constraints — deliberate divergence from Fleet View's
  `space`-to-reply binding)*

### Key Entities

- **Session**: A unified representation of one coding-agent conversation, regardless of backend.
  Carries: backend identity, a stable id, title/name, project directory, current state (working /
  needs-input / ready-for-review / done / failed / stopped), a short summary, created/updated
  timestamps, and (when resolvable) associated pull request references.
- **Backend**: One of Claude, Codex, or opencode. Advertises which actions it supports (attach,
  rename, spawn, delete, PR resolution) so the UI can gate unsupported actions per backend
  without erroring.
- **Project Group**: The default grouping unit — a working directory (or its nearest repo root)
  that one or more sessions, across any backend, share.
- **Pull Request Reference**: An external PR associated with a session via native backend data
  (Claude) or resolved via git branch + origin (Codex), carrying enough state (open/merged/
  closed/checks) to place or exclude the session from "Ready for review."

## Success Criteria *(mandatory)*

### Measurable Outcomes

- **SC-001**: A user can see every currently working or needs-input session across all three
  backends in one list without switching tools, 100% of the time such sessions exist.
- **SC-002**: Attaching to a session never changes the session count reported by that session's
  own backend (0 duplicate sessions created per attach, measured across repeated attach/detach
  cycles).
- **SC-003**: A renamed session's name agrees between agent-viewer and the backend's own listing
  100% of the time rename succeeds; 100% of rejected renames are reported as failures, never as
  silent local success.
- **SC-004**: The default list remains readable (bounded to live/needs-input sessions plus a
  recent, bounded window of finished ones) even when a backend holds thousands of historical
  sessions — verified against a real backend store exceeding 1,000 sessions.
- **SC-005**: A user already fluent in Fleet View's keys can operate every shared action in
  agent-viewer with zero newly-learned bindings.
- **SC-006**: A session's displayed state change is visible without manual refresh within 2
  seconds (measured worst-case), and sooner wherever a real-time update path is used.
- **SC-007**: A failed attach never results in a duplicate session appearing anywhere (0
  duplicates across repeated failed-attach attempts), and always leaves the user on the list
  with a stated reason for the failure.

## Assumptions

- "Fleet View" refers to `claude agents`'s interactive session-management screen, captured live
  against Claude Code v2.1.220 on 2026-07-26; if a future Claude Code release changes that
  screen's behavior, this spec's Fleet View references should be re-verified rather than assumed
  current.
- Claude, Codex, and opencode CLIs are installed and authenticated on the host running
  agent-viewer; a backend that is absent or has no data contributes zero rows and never errors
  (existing project invariant, retained).
- The exact retention window default (e.g. 7 days) is a planning-time parameter, not fixed by
  this spec; it must be configurable.
- The exact visual form of the per-backend identity marker (color, glyph, or both) is a
  planning-time detail; the requirement is only that backend identity is distinguishable without
  changing Fleet View's row shape for the common case.
- Cross-backend "Ready for review" PR resolution for Codex depends on the session's git branch
  and remote being resolvable to a real PR via `gh`; sessions without a resolvable branch/PR
  simply do not appear in that section, which is not an error state.
- Peek, reply, and a web/browser surface are out of scope for this feature; each is a separate,
  future feature if pursued.
