<!--
Sync Impact Report
Version change: none (template) → 1.0.0
Modified principles: n/a (initial ratification)
Added sections: I. Fleet View Parity, II. Real Machinery No Shadow State,
  III. Unified Session Model with Backend-Native Filtering, IV. In-Place Attach
  No Forked Duplicates, V. Bounded Live State, VI. UI-Free Core;
  Additional Constraints; Development Workflow; Governance
Removed sections: none (first ratification from template)
Templates requiring updates:
  ✅ .specify/templates/plan-template.md (generic "Constitution Check" gate,
     no principle-specific text to sync)
  ✅ .specify/templates/spec-template.md (no constitution references)
  ✅ .specify/templates/tasks-template.md (no constitution references)
Follow-up TODOs: none
-->

# agent-viewer Constitution

## Core Principles

### I. Fleet View Parity (NON-NEGOTIABLE)
agent-viewer's outcome is `claude agents` ("Fleet View"), generalized to every
supported backend. Where Fleet View defines a behavior — layout, grouping,
state vocabulary, footer hints, key bindings — agent-viewer MUST match it
exactly. A feature Fleet View does not have MUST be justified by the fact of
being multi-backend (e.g., a backend marker); it MUST NOT be added because it
seems generally useful. Any deviation from Fleet View behavior MUST be
recorded explicitly as a deliberate divergence, not left implicit.

**Rationale**: The prior build invented its own interaction model in the
mistaken belief that it was building "a new UI." That divergence, not any
missing feature, is the dissatisfaction driving this rebuild. Fidelity to a
UX the user already trusts is the product; anything else is scope creep.

### II. Real Machinery, No Shadow State
A mutation (rename, spawn, attach, stop, remove) MUST operate on the actual
backend session and MUST NOT silently fall back to viewer-local state that
disagrees with what the backend itself reports. If a native operation is
rejected or unsupported, the failure MUST be surfaced, not masked by a local
override that makes the UI lie about backend state.

**Rationale**: The current Claude rename path sends a frame the daemon
rejects, then silently writes a local override — so agent-viewer shows one
name while `claude agents` shows another for the same session. That silent
divergence is a named, pinned complaint, not a hypothetical risk.

### III. Unified Session Model with Backend-Native Filtering
Claude, Codex, and opencode sessions render in one list using one shared
session model. Backend-native concepts that do not represent a user-facing
session (Codex subagent/companion threads, archived rows, dead-cwd rows) MUST
be filtered out before display, not merely deprioritized. A backend with
disproportionate history (Codex: thousands of threads vs. Claude's tens) MUST
NOT be allowed to swamp or slow the unified list.

**Rationale**: Verified live: Codex carries 4,194 threads (1,279 active) vs.
Claude's 29, and 54% of active Codex threads are subagent companions. Without
filtering and bounding, "unified" becomes "unusable."

### IV. In-Place Attach, No Forked Duplicates
Attaching to a session MUST join the real, already-running backend session.
It MUST NOT start a second client process that the backend then treats as a
distinct entry (e.g., a new row in `claude agents`, a new job directory, a new
worktree). One session, one row, everywhere it is observed.

**Rationale**: This is the user's lead complaint: "claude forking... I want to
attach to the real bg session, so I don't get a duplicate in the claude
agents." Verified live: `claude attach` runs the CLI as a full second Node
process; the daemon separately publishes a `ptySock`/`ptyAuth` pair per
worker that a direct attach could speak without forking a client.

### V. Bounded Live State
The list MUST reflect backend state promptly — status changes must not lag
visibly behind what the backend's own view (e.g., `claude agents`) shows.
Session retention follows one rule across backends: sessions in a live or
needs-input state are always shown regardless of age; finished sessions
(done/failed/stopped) age out of the default view after a configurable
window. The rule is enforced per backend, not as a single global time cutoff
that could hide an old but still-blocked session.

**Rationale**: Verified live via pty capture: Fleet View shows a
needs-input session 16 days old while completed sessions roll off — polling
staleness and unbounded history were both pinned as real problems the
previous build had.

### VI. UI-Free Core
`agent-viewer-core` MUST contain no `ratatui`/`crossterm` (or any other
UI-toolkit) types. All backend reading, session-model unification, filtering,
retention, and mutation logic lives in `-core`; the TUI is a thin renderer
over it.

**Rationale**: Keeps a future non-TUI surface possible without a rewrite, and
keeps backend logic unit-testable without a terminal.

## Additional Constraints

- Three backends are in scope: Claude, Codex, opencode. Each backend
  advertises its own capabilities; an unsupported action on a given backend
  is a no-op with a footer notice, never a crash or a silent lie.
- Peek and reply are explicitly removed from this rebuild (they confused the
  interaction model). This is a deliberate divergence from Fleet View, which
  binds `space` to reply — noted here so it is not mistaken for an oversight.
  Revisiting either is a future, separately-specified decision.
- Cross-backend PR association ("Ready for review") is in scope for Claude
  (native) and Codex (resolved via the registry's `git_branch`/
  `git_origin_url`); opencode has no branch data and is excluded from that
  section until it does.
- The exact depth of binding to backend internals (documented CLI surfaces
  vs. daemon sockets vs. app-server protocol) is a technical ramification of
  Principles I, II, and IV — not decided here. It is resolved during
  `/speckit-plan`, evidenced by what each principle actually requires.
- A prior web-surface attempt (`agent-viewer-web`, axum + SSE) is out of
  scope for this rebuild and has been removed from the branch history.

## Development Workflow

- This project is built Spec-Kit-first: `/speckit-constitution` (this
  document) → `/speckit-specify` → `/speckit-clarify` → `/speckit-plan` →
  `/speckit-tasks` → `/speckit-converge` (assess the existing ~14k-line
  codebase against the spec before committing to rebuild-vs-regenerate) →
  `/speckit-implement`.
- All other engineering process (worktree usage, commit format, test
  discipline, review gates, verification commands) is governed by this
  repo's own `CLAUDE.md`/`AGENTS.md` and is not restated here.

## Governance

This constitution supersedes ad hoc practice for this rebuild. Amendments
require: a stated reason, a version bump per the rules below, and an update
to any dependent Spec Kit template found to reference a changed principle.

Versioning policy (semantic):
- MAJOR: a principle is removed or redefined in a backward-incompatible way.
- MINOR: a principle or a materially new constraint is added.
- PATCH: wording, clarification, or non-semantic fixes.

All specs, plans, and tasks produced under this constitution MUST be
traceable to one or more of the six principles above; a requirement that
maps to none of them is out of scope until the constitution is amended.

**Version**: 1.0.0 | **Ratified**: 2026-07-26 | **Last Amended**: 2026-07-26
