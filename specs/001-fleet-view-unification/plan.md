# Implementation Plan: Fleet View Unification

**Branch**: `main` (developed directly on main per user decision) | **Date**: 2026-07-26 | **Spec**: [spec.md](./spec.md)

**Input**: Feature specification from `specs/001-fleet-view-unification/spec.md`

## Summary

Make agent-viewer a faithful reproduction of Claude Code's Fleet View that spans Claude, Codex
and opencode in one list. The primary requirement is parity, not a new interaction model.

The technical approach that follows from Phase 0 is a **split between the interaction plane and
the metadata plane** (D-001):

- **Interaction is always the native CLI**, on every backend. Attach spawns the backend's own
  client in a PTY. agent-viewer never renders a session's contents itself. A failure in this
  plane would break the user's live working session, so it stays on vendor-supported entry
  points only.
- **Metadata binds as deep as each backend usefully allows**, because a failure there degrades
  only to a stale or unfiltered list.

Per backend:

- **Claude**: attach via `claude attach <short_id>`, already proven non-duplicating. Metadata
  from `claude agents --json --all`. Rename is impossible on this backend and becomes an
  advertised no-op. The daemon `control.sock` attach was proven to work and is deliberately
  **not** used (D-002), recorded as a fallback should `claude attach` regress.
- **Codex**: sessions are hosted by the app-server daemon (`thread/start`) and attached with the
  native TUI pointed at that daemon, `codex resume --remote unix://<socketPath> <id>`. Metadata
  from the same daemon - `thread/list` for enumeration with server-side subagent exclusion,
  `thread/name/set` for real rename.
- **opencode**: has the same shape (`opencode serve` plus `opencode attach <url>`); whether to
  bind it to the server API or stay on CLI plus read-only SQLite is being evaluated. Either way
  its capability advertisements are corrected to match what it can actually do.

All three backends share one topology: **a headless host owns the session, and the vendor's own
client attaches to it.** agent-viewer is a third-party client of each host - never a host, never
a renderer. Sessions therefore keep working while detached and survive agent-viewer exiting, on
both primary backends (D-011). Interactive Claude rows become a refusal gate rather than a
supported path (D-012).

This closes the three confirmed defects: attach duplication on interactive Claude rows, fake
rename, and stale state.

## Technical Context

**Language/Version**: Rust 2021 edition, stable toolchain

**Primary Dependencies**: `ratatui` + `crossterm` (TUI crate only), `rusqlite` (read-only
registry access), `serde`/`serde_json` (permissive deserialization - see the schema-drift risk
in D-009), plus a WebSocket client for the Codex control socket and an inotify watcher for
rollout files. Both new dependencies are confined to `-core` and to the metadata plane; the
interaction plane needs no new dependency, since it spawns native CLIs over the existing PTY.

**Storage**: No owned durable store beyond viewer-local UI preferences. Codex
`~/.codex/state_*.sqlite` and opencode's SQLite are read-only inputs. Session identity, names
and status are owned by the backends (Principle II).

**Testing**: `cargo test --workspace`; fixtures in `crates/agent-viewer-core/tests/fixtures/`
and `tests/common/`. Live end-to-end via `tests/e2e_live.rs` (`--ignored`). New protocol work
needs contract tests against captured daemon frames plus a live opt-in test per backend.

**Target Platform**: Linux terminal (this box). The Claude daemon multi-attach behavior
verified in D-002 is Linux-specific; the kick loop is Windows-only.

**Project Type**: Rust workspace - library crate plus TUI binary.

**Performance Goals**: A state change is visible within 2 seconds (FR-009/SC-006), with
event-driven propagation preferred where the mechanism allows. Enumeration must stay responsive
against the measured 4,194-thread Codex registry; `useStateDbOnly: true` skips the JSONL repair
scan specifically to make a 1-2s refresh loop viable.

**Constraints**: Never write the Codex registry. Never start a Codex daemon silently. Never
spawn a second client on attach. Never hardcode a versioned path or socket location. Permissive
deserialization against an `[experimental]` API with known same-version schema drift.

**Scale/Scope**: 4,194 Codex threads (1,279 unarchived, 54% of active ones subagent
companions), 29-30 Claude sessions, 297 opencode sessions. Three backends, one list.

## Constitution Check

*GATE: evaluated after Phase 0 research, before Phase 1 design.*

| Principle | Status | Evidence |
|---|---|---|
| I. Fleet View Parity (NON-NEGOTIABLE) | PASS | Parity surface captured empirically via pyte pty captures, not inferred. Key map, grouping modes, glyphs and model picker all confirmed present in Fleet View. |
| II. Real Machinery, No Shadow State | PASS | D-006 replaces the viewer-local rename override with `thread/name/set` on Codex and an advertised no-op on Claude. The harmful rv-socket rename is removed. |
| III. Unified Session Model with Backend-Native Filtering | PASS | D-005 moves subagent and archived filtering into `thread/list`'s own `sourceKinds`/`archived` parameters instead of viewer-side heuristics over a serialized-enum column. |
| IV. In-Place Attach, No Forked Duplicates | PASS | Native CLI attach on all backends (D-001/D-002), with a `notLoaded` gate on Codex (D-004) making duplication impossible by construction. D-003 fixes the id-less interactive row that still duplicates today. |
| V. Bounded Live State | PASS | D-007 gives a 2s poll backstop with inotify and app-server push where available. |
| VI. UI-Free Core | PASS | WebSocket client, inotify watcher and app-server types all land in `-core`. No `ratatui`/`crossterm` type crosses the boundary. |

**Deferred question resolved**: the constitution deferred binding depth to this phase. D-001
resolves it by splitting the planes - interaction stays on native CLIs everywhere, metadata
binds as deep as each backend usefully allows. This is a product constraint first (agent-viewer
must never render a session itself) and a risk-management choice second (a metadata failure
degrades the list; an interaction failure breaks the user's live session).

**No violations require justification.** The Complexity Tracking table below is empty.

## Project Structure

### Documentation (this feature)

```text
specs/001-fleet-view-unification/
├── plan.md                       # This file
├── spec.md                       # Feature specification
├── research.md                   # Phase 0 output - consolidated decisions D-001..D-010
├── research/                     # Phase 0 evidence
│   ├── claude-daemon.md
│   ├── codex-app-server.md
│   └── duplication-and-opencode.md
├── data-model.md                 # Phase 1 output
├── quickstart.md                 # Phase 1 output
├── contracts/                    # Phase 1 output
├── checklists/
│   └── requirements.md
└── tasks.md                      # Phase 2 output (/speckit-tasks)
```

### Source Code (repository root)

```text
crates/agent-viewer-core/
├── src/
│   ├── lib.rs
│   ├── backend.rs                # Backend trait + capability advertisement
│   ├── error.rs
│   ├── group.rs                  # project/state grouping
│   ├── spawn.rs
│   ├── pty.rs                    # PTY attach transport
│   ├── pr_status.rs
│   ├── state.rs                  # viewer-local UI preferences only
│   ├── claude.rs                 # backend impl; `agents --json --all` parsing (id-less rows: D-003)
│   ├── codex/
│   │   ├── mod.rs
│   │   ├── app_server.rs         # NEW: WebSocket JSON-RPC client, discovery, notifications
│   │   ├── cli.rs
│   │   ├── registry.rs           # read-only SQLite fallback when daemon absent
│   │   ├── rollout.rs
│   │   ├── source.rs
│   │   └── status.rs             # inotify-driven; /proc/fd correlation retained
│   └── opencode.rs               # capability advertisements corrected (D-010)
└── tests/
    ├── contract/                 # NEW: captured-frame contract tests per protocol
    ├── e2e_live.rs               # opt-in live tests, extended per backend
    └── ...                       # existing unit/integration suites

crates/agent-viewer-tui/
└── src/
    ├── main.rs, app.rs, ui.rs, keys.rs, actions.rs, composer.rs,
    ├── mouse.rs, mutations.rs, ops.rs, attach.rs, logos.rs, pr_cache.rs
    └── (peek.rs, peek_cache.rs, pending_reply.rs REMOVED - FR-013)
```

**Structure Decision**: Keep the existing two-crate workspace unchanged in shape. Because D-001
keeps interaction on the native CLIs, the structural delta is small: (a) add
`codex/app_server.rs` alongside the existing `codex/` module, (b) make `codex/status.rs`
inotify-driven, and (c) delete the three peek/reply TUI modules per FR-013. `claude.rs` stays a
single file - with the daemon protocol out of scope it does not grow. New transports live in
`-core` so the future web surface inherits them (Principle VI). No new crate is warranted;
nothing here is independently consumable.

## Complexity Tracking

> No Constitution Check violations. Table intentionally empty.

| Violation | Why Needed | Simpler Alternative Rejected Because |
|-----------|------------|-------------------------------------|
| (none) | | |

## Risks

| Risk | Mitigation |
|---|---|
| Claude daemon protocol is undocumented and the CLI ships several times a week | Avoided entirely: D-001 keeps attach on `claude attach`. The socket path stays documented in research as a fallback only |
| Codex app-server is `[experimental]` with shallow versioning and known same-version schema drift (`extra`, `historyMode` undeclared) | Permissive deserialization; fall back to read-only `state_*.sqlite` enumeration when the daemon is absent or the handshake fails |
| Daemon not running when Desktop is abandoned | Verified independent: listener is `PPid 1` and survives Desktop; `codex app-server daemon start` exists. Availability gate is `status == "running"`; never start one silently |
| `Thread.path` marked `[UNSTABLE]` | Do not depend on it for identity; thread id is the key |
| A short `thread/list` page misread as end-of-list | Paginate strictly on `nextCursor`, never on page length (11 rows observed for `limit: 200`) |
| Default `sourceKinds` silently drops `exec` threads | Pass `sourceKinds` explicitly rather than relying on the default |
