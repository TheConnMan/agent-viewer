# Tasks: Fleet View Unification

**Input**: Design documents from `/specs/001-fleet-view-unification/`

**Prerequisites**: plan.md, spec.md, research.md, data-model.md, contracts/

**Tests**: Test tasks ARE included. The project constitution and repo testing rules require
test-first for new behavior, and the test-writer and implementer must run in separate contexts.

**Organization**: Grouped by user story so each is independently implementable and testable.

## Format: `[ID] [P?] [Story] Description`

- **[P]**: Can run in parallel (different files, no dependencies)
- **[Story]**: Which user story this task belongs to (US1..US5)
- Exact file paths are given in every task

## Path Conventions

Rust workspace, two crates:

- `crates/agent-viewer-core/src/` and `crates/agent-viewer-core/tests/`
- `crates/agent-viewer-tui/src/` and `crates/agent-viewer-tui/tests/`

This is a **modification of an existing codebase**, not a greenfield build. Tasks that reshape an
existing type or module say so explicitly.

---

## Phase 1: Setup (Shared Infrastructure)

**Purpose**: Dependencies and captured protocol fixtures that every later phase needs.

- [ ] T001 Add a WebSocket client dependency and an inotify dependency to `crates/agent-viewer-core/Cargo.toml`, both confined to `-core` per Principle VI; do not add either to `crates/agent-viewer-tui/Cargo.toml`
- [ ] T002 [P] Capture live Codex app-server frames to `crates/agent-viewer-core/tests/fixtures/codex_app_server/` (initialize response, a `thread/list` page with non-null `nextCursor`, a short page, each `ThreadStatus` variant, `thread/name/updated`, and a `Thread` carrying the undeclared `extra` and `historyMode` fields per contracts/codex-app-server.md)
- [ ] T003 [P] Capture `claude agents --json --all` output including at least one interactive (no `id`, no `state`) row and one background row to `crates/agent-viewer-core/tests/fixtures/claude/`
- [ ] T004 [P] Capture opencode `GET /session`, `GET /session/status` (idle, busy, and `retry`), and `/experimental/session?archived=true` responses to `crates/agent-viewer-core/tests/fixtures/opencode/`
- [ ] T005 [P] Add a `contract/` test directory with a shared frame-replay helper in `crates/agent-viewer-core/tests/common/mod.rs` so captured fixtures can be replayed without a live daemon

---

## Phase 2: Foundational (Blocking Prerequisites)

**Purpose**: The unified `Session` type, the truthful capability model, and the `Backend` trait
seam. Every user story reads or writes these.

**CRITICAL**: No user story work can begin until this phase is complete.

- [ ] T006 Reshape `Session` in `crates/agent-viewer-core/src/backend.rs` to the data-model.md field table: `short_id: Option<String>` (never `""`), new `origin`, `git_branch`, `companion`, `hidden`, `rollout_path`
- [ ] T007 Add `SessionOrigin { Background, Interactive, Exec }` to `crates/agent-viewer-core/src/backend.rs`
- [ ] T008 Replace the status enum in `crates/agent-viewer-core/src/backend.rs` with `Status { Working, NeedsInput { reason }, Idle, Done, Error, Unknown }` per data-model.md
- [ ] T009 Add `Capabilities` plus both `capabilities()` and `capabilities_for(&Session)` to the `Backend` trait in `crates/agent-viewer-core/src/backend.rs`, so per-row conditions (attach, stop) are expressible
- [ ] T010 Add `AttachRefusal` carrying a human-readable reason to `crates/agent-viewer-core/src/error.rs`, and change the trait's attach signature to `attach_command(&self, s: &Session) -> Result<Command, AttachRefusal>` in `crates/agent-viewer-core/src/backend.rs`
- [ ] T011 Add the `subscribe(&self, sink: StatusSink) -> Result<Subscription>` liveness seam to `crates/agent-viewer-core/src/backend.rs` with a default no-op implementation, so backends can adopt it incrementally behind the poll backstop
- [ ] T012 Reduce `crates/agent-viewer-core/src/state.rs` to presentation-only viewer state (grouping mode, sort, collapsed groups, retention window) and add an optional `opencode.server_url`; delete any name or archived override storage, which is the shadow state Principle II forbids
- [ ] T013 [P] Update `crates/agent-viewer-core/tests/state_tests.rs` to assert the viewer store rejects or no longer exposes session-identifying fields (name, archived, status)
- [ ] T014 Update every existing call site so the workspace compiles against the reshaped types: `crates/agent-viewer-core/src/claude.rs`, `codex/mod.rs`, `codex/registry.rs`, `codex/status.rs`, `opencode.rs`, `group.rs`, `spawn.rs`, and the TUI's `app.rs`, `ui.rs`, `ops.rs`

**Checkpoint**: The workspace builds, `cargo clippy --workspace` is clean, and the trait seam is
in place. User stories can now begin.

---

## Phase 3: User Story 1 - One live list across every backend (Priority: P1) MVP

**Goal**: One list, all three backends, grouped by project, in Fleet View's row format, with zero
subagent rows, zero archived rows, and zero dead-cwd rows.

**Independent Test**: Launch agent-viewer against real Claude, Codex, and opencode history.
Rows from all three backends appear under project group headers in Fleet View row format, with
zero Codex subagent rows and zero archived or dead-cwd rows. A backend with no data contributes
zero rows and shows no error.

### Tests for User Story 1

> Write these first and confirm they fail before implementing.

- [ ] T015 [P] [US1] Contract test in `crates/agent-viewer-core/tests/contract/codex_app_server_tests.rs`: replayed `thread/list` fixtures deserialize permissively (undeclared `extra` and `historyMode` do not fail), and pagination follows `nextCursor` only, so an 11-row page with a non-null cursor is NOT treated as end-of-list
- [ ] T016 [P] [US1] Test in `crates/agent-viewer-core/tests/claude_tests.rs`: rows whose `kind` is not `"background"` are dropped by `list()`, and every returned row has `short_id: Some(_)`, never `Some("")`
- [ ] T017 [P] [US1] Contract test in `crates/agent-viewer-core/tests/contract/opencode_server_tests.rs`: with a server URL configured, sessions come from `GET /session` and archived rows are filtered caller-side (the stable endpoint does not filter them); with no URL configured, enumeration falls back to the existing read-only path
- [ ] T018 [P] [US1] Test in `crates/agent-viewer-core/tests/registry_tests.rs`: when the app-server is absent or its handshake fails, Codex enumeration falls back to read-only `state_*.sqlite` and still returns rows
- [ ] T019 [P] [US1] Test in `crates/agent-viewer-core/tests/group_tests.rs`: sessions from three different backends sharing one `cwd` land in one project group, and a session whose `cwd` no longer exists is excluded

### Implementation for User Story 1

- [ ] T020 [US1] Create `crates/agent-viewer-core/src/codex/app_server.rs`: socket discovery via `codex app-server daemon version` -> `socketPath` (never hardcoded), the `status == "running"` availability gate that never starts a daemon, the WebSocket upgrade handshake, and JSON-RPC framing without a `jsonrpc` field
- [ ] T021 [US1] Implement `initialize` in `crates/agent-viewer-core/src/codex/app_server.rs`, sending `clientInfo` once per connection and setting `capabilities.optOutNotificationMethods` to mute the per-token delta firehose
- [ ] T022 [US1] Implement `thread/list` enumeration in `crates/agent-viewer-core/src/codex/app_server.rs` with explicit `sourceKinds: ["cli","exec"]`, `useStateDbOnly: true`, `archived: false`, and strict `nextCursor` pagination; map `id`, `name`, `preview`, `cwd`, `gitInfo.branch`, `gitInfo.originUrl`, `updatedAt`, `status`, `source` onto `Session`
- [ ] T023 [US1] Wire `crates/agent-viewer-core/src/codex/mod.rs` to prefer `app_server.rs` enumeration and fall back to `registry.rs` read-only SQLite when the daemon is unavailable; never write the registry
- [ ] T024 [US1] Delete the viewer-side subagent heuristic from `crates/agent-viewer-core/src/codex/source.rs` for the app-server path, since `sourceKinds` now excludes companions server-side; retain the parser for the SQLite fallback path only
- [ ] T025 [US1] Fix the duplication root cause in `crates/agent-viewer-core/src/claude.rs`: replace `unwrap_or_default()` on the `id` field with an `Option`, set `origin` from `kind`, and drop non-background rows from `list()`
- [ ] T026 [US1] Add the opportunistic opencode server binding in `crates/agent-viewer-core/src/opencode.rs`: probe the configured `opencode.server_url`, degrade silently to the no-server tier on failure, enumerate via `GET /session`, and apply archived filtering caller-side; never start a server and never guess a port
- [ ] T027 [US1] Pass basic-auth credentials through to the opencode server client in `crates/agent-viewer-core/src/opencode.rs` when configured, matching `opencode attach`'s `-u`/`-p`
- [ ] T028 [US1] Make each backend's enumeration failure isolated in `crates/agent-viewer-core/src/backend.rs`: an unreadable or mid-write store yields zero rows from that backend for that refresh and never blocks or fails the others
- [ ] T029 [US1] Render the Fleet View row format in `crates/agent-viewer-tui/src/ui.rs`: status glyph, name, state word, summary, right-aligned age, with no state vocabulary Fleet View does not use
- [ ] T030 [US1] Add the per-backend identity glyph in `crates/agent-viewer-tui/src/logos.rs` and `ui.rs` without changing row shape for a single-backend user
- [ ] T031 [US1] Verify list-open and refresh latency against the real 4,194-thread Codex registry and record the measurement in `specs/001-fleet-view-unification/quickstart.md`

**Checkpoint**: US1 is independently functional. One list, three backends, correct filtering.

---

## Phase 4: User Story 2 - Attach lands in the real session (Priority: P1)

**Goal**: Attach joins the actual backend session through that backend's own native client, never
creating a second session, and refuses cleanly when it cannot.

**Independent Test**: Record each backend's own session count. Attach, interact, detach,
re-attach. Counts are unchanged and the same conversation history is present. Force an
unreachable session and confirm an inline error naming the reason, the row still selected, and no
new session anywhere.

### Tests for User Story 2

- [ ] T032 [P] [US2] Test in `crates/agent-viewer-core/tests/attach_contract_tests.rs`: no backend's attach command ever contains a fork flag (`--fork-session`, `--fork`)
- [ ] T033 [P] [US2] Test in `crates/agent-viewer-core/tests/claude_tests.rs`: attach is refused with a reason for any row whose `origin` is not `Background`, and returns `claude attach <short_id>` otherwise
- [ ] T034 [P] [US2] Test in `crates/agent-viewer-core/tests/codex_attach_tests.rs`: attach returns `codex resume --remote unix://<discovered socketPath> <thread_id>` with the socket path resolved at call time, and refuses only for `notLoaded` plus a live external process holding the thread
- [ ] T035 [P] [US2] Test in `crates/agent-viewer-tui/tests/app_tests.rs`: an `AttachRefusal` renders as an inline error naming the reason, leaves the selection on the same row, and triggers no spawn
- [ ] T036 [P] [US2] Test in `crates/agent-viewer-tui/tests/mutations_tests.rs`: a mutation the backend advertises as unsupported performs no process action at all, specifically no `terminate` before the refusal

### Implementation for User Story 2

- [ ] T037 [US2] Implement `attach_command` for Claude in `crates/agent-viewer-core/src/claude.rs`: `claude attach <short_id>` gated on `origin == Background`, returning `AttachRefusal` otherwise; remove the `claude -r <uuid>` fallback path entirely
- [ ] T038 [US2] Implement `attach_command` for Codex in `crates/agent-viewer-core/src/codex/mod.rs` using `codex resume --remote unix://<socketPath> <thread_id>`, refusing only in the `notLoaded` plus live-external-process case
- [ ] T039 [US2] Implement `attach_command` for opencode in `crates/agent-viewer-core/src/opencode.rs` as `opencode attach <url> -s <session_id>` in the server tier, and the existing native resume path in the no-server tier
- [ ] T040 [US2] Implement `capabilities_for` per backend so attach is advertised per row rather than per backend, in `crates/agent-viewer-core/src/claude.rs`, `codex/mod.rs`, and `opencode.rs`
- [ ] T041 [US2] Handle `AttachRefusal` in `crates/agent-viewer-tui/src/attach.rs` and `actions.rs`: show the inline error, keep the row selected, and attempt no fallback path
- [ ] T042 [US2] Fix the live defect in `crates/agent-viewer-tui/src/mutations.rs:50-52` where `terminate(pid, "claude")` runs before the backend declines the action as unsupported; check capability first so an unsupported remove cannot SIGTERM a live session's process group
- [ ] T043 [US2] Extend `crates/agent-viewer-core/tests/e2e_live.rs` with an opt-in per-backend attach test asserting the backend's own session count is unchanged across attach and detach

**Checkpoint**: US1 and US2 both work independently. The duplication defect is closed at the
source and by construction.

---

## Phase 5: User Story 3 - Rename is real, not a local fiction (Priority: P2)

**Goal**: Rename goes through the backend's native channel or is honestly advertised as
unsupported. No viewer-local name ever exists.

**Independent Test**: Rename in agent-viewer, then query the backend's own listing. Names match.
On a backend without a rename channel, the key is a no-op with a footer notice, never a success
message and never a locally-changed name.

### Tests for User Story 3

- [ ] T044 [P] [US3] Test in `crates/agent-viewer-core/tests/rename_tests.rs`: Claude advertises `rename: false` and its `rename()` is unimplemented; assert specifically that no connection is opened to the daemon rendezvous socket
- [ ] T045 [P] [US3] Contract test in `crates/agent-viewer-core/tests/contract/codex_app_server_tests.rs`: `thread/name/set` success is read as absence-of-error plus the `thread/name/updated` notification, never as a returned name in the empty ack
- [ ] T046 [P] [US3] Test in `crates/agent-viewer-core/tests/opencode_tests.rs`: rename issues `PATCH /session/{id}` with `{"title":...}` in the server tier and is advertised unsupported in the no-server tier
- [ ] T047 [P] [US3] Test in `crates/agent-viewer-core/tests/claude_tests.rs`: a `claude rm` run that prints `kept <id> - worktree <reason>` is surfaced as a partial failure, not read as success from its zero exit code

### Implementation for User Story 3

- [ ] T048 [US3] Remove the rendezvous-socket rename from `crates/agent-viewer-core/src/claude.rs` and advertise `rename: false`; this closes the live defect where every rename keypress evicts the daemon's supervisor connection for zero benefit
- [ ] T049 [US3] Implement `thread/name/set` in `crates/agent-viewer-core/src/codex/app_server.rs` and expose it as `rename()` in `codex/mod.rs`
- [ ] T050 [US3] Implement rename and archive in `crates/agent-viewer-core/src/opencode.rs` via `PATCH /session/{id}` (`{"title":...}` and `{"time":{"archived":<ms>}}`), encoding that unarchive is `archived: 0` and that `{"time":{}}` is a silent no-op
- [ ] T051 [US3] Surface partial-success CLI output as failure in `crates/agent-viewer-core/src/claude.rs`, and handle `claude rm`'s prefix id matching and exit-1-on-ambiguity behavior
- [ ] T052 [US3] Route unsupported mutations to a footer notice in `crates/agent-viewer-tui/src/mutations.rs` and `ui.rs`, never an error dialog and never an optimistic local update

**Checkpoint**: Names agree between agent-viewer and every backend, or the action is honestly
refused.

---

## Phase 6: User Story 4 - Switch between project view and state view (Priority: P2)

**Goal**: Fleet View's grouping toggle, with its own key and its four section names, spanning all
three backends.

**Independent Test**: With a mix of working, needs-input, and completed sessions across backends,
press Fleet View's toggle key. Sessions regroup into Needs input / Working / Ready for review /
Completed, each appearing in exactly one section matching its state.

### Tests for User Story 4

- [ ] T053 [P] [US4] Test in `crates/agent-viewer-core/tests/group_tests.rs`: state grouping produces exactly Fleet View's four sections and places every session in exactly one
- [ ] T054 [P] [US4] Test in `crates/agent-viewer-core/tests/pr_status_tests.rs`: a Codex session whose `gitInfo.branch` resolves to an open PR lands in Ready for review, and a session whose PR is merged or closed does not
- [ ] T055 [P] [US4] Test in `crates/agent-viewer-core/tests/opencode_tests.rs`: an opencode session is never placed in Ready for review, since `pr_refs` is unsupported on both tiers

### Implementation for User Story 4

- [ ] T056 [US4] Implement state grouping alongside project grouping in `crates/agent-viewer-core/src/group.rs` as a pure function over `Vec<Session>`
- [ ] T057 [US4] Resolve PRs by branch for Codex in `crates/agent-viewer-core/src/pr_status.rs` using `gitInfo.branch` and `gitInfo.originUrl`, and exclude a session from Ready for review once its PR is merged or closed
- [ ] T058 [US4] Advertise `pr_refs: false` for opencode in both tiers in `crates/agent-viewer-core/src/opencode.rs`
- [ ] T059 [US4] Bind the grouping toggle to Fleet View's own key in `crates/agent-viewer-tui/src/keys.rs` and render the four section headers in `ui.rs`
- [ ] T060 [US4] Persist the active grouping mode and collapsed group set in `crates/agent-viewer-core/src/state.rs` as presentation-only state

**Checkpoint**: Both views work across all three backends.

---

## Phase 7: User Story 5 - The list stays live and stays small (Priority: P3)

**Goal**: A state change is visible within 2 seconds without manual refresh, and the default list
stays bounded despite thousands of historical Codex threads.

**Independent Test**: Watch a running session complete and confirm the row updates unprompted
within 2 seconds. Confirm a needs-input session older than the retention window still appears
while a completed one does not, until show-all is invoked.

### Tests for User Story 5

- [ ] T061 [P] [US5] Test in `crates/agent-viewer-core/tests/status_tests.rs`: retention keeps every unfinished session regardless of age and hides finished sessions past the configured window
- [ ] T062 [P] [US5] Test in `crates/agent-viewer-core/tests/status_tests.rs`: a `notLoaded` Codex thread is never mapped to `Done`; its status comes from PID correlation plus the rollout tail
- [ ] T063 [P] [US5] Contract test in `crates/agent-viewer-core/tests/contract/codex_app_server_tests.rs`: each `ThreadStatus` variant maps as specified, with `active.activeFlags` going to `NeedsInput` natively and no inference
- [ ] T064 [P] [US5] Test in `crates/agent-viewer-core/tests/opencode_tests.rs`: `live_status` is advertised false in the no-server tier rather than flipping every row to Working from a backend-wide process signal, and true in the server tier from per-id `GET /session/status`

### Implementation for User Story 5

- [ ] T065 [US5] Make `crates/agent-viewer-core/src/codex/status.rs` inotify-driven on the rollout file, retaining `/proc/fd` PID correlation for threads the daemon does not host
- [ ] T066 [US5] Consume `thread/status/changed`, `thread/name/updated`, `turn/started`, and `turn/completed` notifications in `crates/agent-viewer-core/src/codex/app_server.rs`, encoding that the daemon reports status only for threads it hosts
- [ ] T067 [US5] Consume the opencode SSE stream at `GET /event` in `crates/agent-viewer-core/src/opencode.rs` (`session.status`, `session.updated`, `message.updated`, with `server.heartbeat` as the liveness signal), server tier only
- [ ] T068 [US5] Implement the 2-second poll backstop in `crates/agent-viewer-core/src/backend.rs` so the FR-009 bound holds regardless of which backends deliver events
- [ ] T069 [US5] Implement retention in `crates/agent-viewer-core/src/group.rs` as `retain(s) = !is_finished(s.status) || (now - s.updated_at_ms) < window`, with the window configurable and a show-all override
- [ ] T070 [US5] Fix the opencode status defect in `crates/agent-viewer-core/src/opencode.rs` where any running `opencode*` process currently flips every session to Working; use per-id status in the server tier and `Unknown` in the no-server tier
- [ ] T071 [US5] Wire the status sink into the TUI event loop in `crates/agent-viewer-tui/src/app.rs` so pushed changes repaint without a keypress

**Checkpoint**: All five user stories are independently functional.

---

## Phase 8: Polish and Cross-Cutting Concerns

- [ ] T072 [P] Delete `crates/agent-viewer-tui/src/peek.rs`, `peek_cache.rs`, and `pending_reply.rs` and their module declarations in `lib.rs`, per FR-015 (peek and reply are out of scope for this feature)
- [ ] T073 Scope delete-all to the currently visible, post-filter, post-retention rows in `crates/agent-viewer-tui/src/actions.rs` and `mutations.rs`, per FR-013
- [ ] T074 [P] Audit the key map in `crates/agent-viewer-tui/src/keys.rs` against the captured Fleet View bindings (open, delete-selected, delete-all-when-unselected, switch view, quit, mention, newline) and remove any reassignment of a key Fleet View already binds, per FR-011
- [ ] T075 [P] Verify `-core` carries no `ratatui` or `crossterm` types by checking `crates/agent-viewer-core/Cargo.toml` has neither as a dependency, per FR-014 and Principle VI
- [ ] T076 [P] Update `SPEC.md` to correct the `SPEC.md:100-103` app-server contention claim, which was a misread of a bind failure from a second would-be server (D-008), and to record the app-server binding
- [ ] T077 [P] Update `README.md` for the changed attach, rename, and grouping behavior
- [ ] T078 Run every section of `specs/001-fleet-view-unification/quickstart.md` against real local state and record the results
- [ ] T079 Run `~/.cargo/bin/cargo test --workspace --no-fail-fast` and `cargo clippy --workspace`, and confirm the live e2e passes with `~/.cargo/bin/cargo test -p agent-viewer-core --test e2e_live -- --ignored --nocapture`

---

## Dependencies and Execution Order

### Phase Dependencies

- **Setup (Phase 1)**: No dependencies
- **Foundational (Phase 2)**: Depends on Setup; BLOCKS every user story
- **US1 (Phase 3)**: Depends on Foundational. No dependency on other stories
- **US2 (Phase 4)**: Depends on Foundational. Shares `app_server.rs` transport with US1 (T020, T021), so start US2 after T021 lands
- **US3 (Phase 5)**: Depends on Foundational, plus T021 for the Codex rename call
- **US4 (Phase 6)**: Depends on Foundational, plus T022 for `gitInfo.branch` on Codex rows
- **US5 (Phase 7)**: Depends on Foundational, plus T021 for notifications and T026 for the opencode server tier
- **Polish (Phase 8)**: Depends on all desired stories

### The one genuine cross-story dependency

The Codex WebSocket transport (T020, T021) is shared by US1, US3, and US5. It sits in US1 because
enumeration is what makes it load-bearing for the MVP. If US3 or US5 is built first, T020 and
T021 move with it.

### Within Each User Story

- Tests are written first and MUST fail before implementation
- Core types before backend implementations
- Backend implementations before TUI wiring
- Story complete and independently verified before moving to the next priority

### Parallel Opportunities

- T002, T003, T004, T005 (fixture capture, four different directories)
- All test tasks within a story: T015-T019, T032-T036, T044-T047, T053-T055, T061-T064
- Per-backend implementation within a story, since each backend is a separate file: T025 (claude.rs), T026 (opencode.rs), and T022-T023 (codex/) can run together
- T072, T074, T075, T076, T077 in Polish

Backend files are the natural parallel seam. Never assign two agents the same file: `claude.rs`,
`opencode.rs`, and `codex/` are three streams, but `backend.rs` is a single shared stream and
must be serialized.

---

## Parallel Example: User Story 1

```bash
# Tests first, all five in parallel (different files):
Task: "Contract test for thread/list pagination in crates/agent-viewer-core/tests/contract/codex_app_server_tests.rs"
Task: "Interactive-row drop test in crates/agent-viewer-core/tests/claude_tests.rs"
Task: "opencode server tier test in crates/agent-viewer-core/tests/contract/opencode_server_tests.rs"
Task: "SQLite fallback test in crates/agent-viewer-core/tests/registry_tests.rs"
Task: "Cross-backend project grouping test in crates/agent-viewer-core/tests/group_tests.rs"

# Then the three backend streams in parallel (different files):
Task: "Codex app-server enumeration in crates/agent-viewer-core/src/codex/"
Task: "Claude id-less row fix in crates/agent-viewer-core/src/claude.rs"
Task: "opencode opportunistic server binding in crates/agent-viewer-core/src/opencode.rs"
```

---

## Implementation Strategy

### MVP scope: Phases 1, 2, and 3 (US1)

US1 alone is the premise of the rebuild: one list across three backends, correctly filtered. It
is demonstrable on its own and is where the Codex app-server binding proves itself.

However, **US2 is also P1 and closes the user's top complaint**. The honest MVP for this
particular feature is US1 plus US2, because a list you cannot safely attach from reproduces the
defect that motivated the rebuild. Ship US1 to validate enumeration, then US2 immediately.

### Incremental Delivery

1. Setup and Foundational: the trait seam and the reshaped `Session`
2. US1: one correct list (validate against real local state)
3. US2: safe attach (validate with backend session counts before and after)
4. US3: real rename (validate against each backend's own listing)
5. US4: state view and Ready for review
6. US5: liveness and retention
7. Polish: peek and reply removal, delete-all scoping, key map audit, docs

### Two live defects to pull forward

Independent of the rebuild-versus-regenerate decision in `/speckit-converge`, these are bugs in
the current build and can land immediately:

- **T048**: the Claude rename path evicts the daemon's supervisor connection on every keypress
- **T042**: `mutations.rs:50-52` SIGTERMs a session's process group before the remove is declined
  as unsupported

---

## Notes

- [P] tasks touch different files and have no incomplete dependencies
- Every test must survive the repo's three deletion checks: delete the test, delete the
  implementation, rename internals
- Mock only filesystem inputs and captured protocol frames; never mock rusqlite, the parsers, or
  the status resolver
- Every commit builds clean with `cargo clippy --workspace` and passing tests
- The test-writer and the implementer must run in separate contexts
