# Phase 1 Data Model: Fleet View Unification

**Feature**: 001-fleet-view-unification | **Date**: 2026-07-26

Derived from the Key Entities in `spec.md` and the Phase 0 decisions in `research.md`. All
types live in `agent-viewer-core` and carry no UI-toolkit types (Principle VI).

---

## Session

The unified row. One per backend session, regardless of which backend produced it.

| Field | Type | Source | Notes |
|---|---|---|---|
| `backend` | `BackendKind` | intrinsic | Claude / Codex / Opencode. Drives the glyph (FR-012). |
| `id` | `String` | backend | Full stable identity. Codex thread id, Claude session uuid, opencode session id. |
| `short_id` | `Option<String>` | backend | **`None` when absent, never `""`.** See D-003; the current `unwrap_or_default()` is the duplication bug. |
| `origin` | `SessionOrigin` | backend | NEW. Background vs interactive vs exec. Gates attach (D-003). |
| `title` | `String` | backend | The backend's own name. Never a viewer-local override (Principle II). |
| `cwd` | `PathBuf` | backend | Drives project grouping. |
| `git_branch` | `Option<String>` | backend | Codex supplies `gitInfo.branch` directly; feeds PR-by-branch (FR-008). |
| `status` | `Status` | resolver | See below. |
| `created_at_ms` / `updated_at_ms` | `i64` | backend | `updated_at_ms` drives retention (FR-010). |
| `hidden` | `bool` | backend | Archived. Backend-native, not viewer-local (FR-003). |
| `companion` | `bool` | backend | Subagent/companion. On Codex this is now server-side (D-005). |
| `summary` | `String` | backend | Last-message preview. Codex `preview`. |
| `pid` | `Option<u32>` | correlation | `/proc/fd` correlation. Retained; drives Codex CLI liveness (D-007). |
| `rollout_path` | `Option<PathBuf>` | backend | Watched by inotify for event-driven status (D-007). |
| `pr_refs` | `Vec<PrRef>` | resolver | Resolved by branch (FR-008). |

### SessionOrigin

```
Background   // daemon/job-managed; carries a short_id; attachable
Interactive  // a human's own terminal session; no short_id
Exec         // one-shot non-interactive run (codex exec)
```

**Why this is new**: today interactive and background Claude rows are indistinguishable
downstream because a missing `id` silently becomes `""`. Making origin explicit is what lets
attach refuse correctly instead of falling through to a duplicating code path.

**Interactive sessions are filtered out of the list entirely** (D-012), because Fleet View's own
TUI does not list them - proven live, three ways. `list()` MUST drop Claude rows whose `kind` is
not `"background"`. This removes the `claude -r <uuid>` duplicate path at the source, and also
removes a second defect: interactive rows lack `state` as well as `id`, so today they render as
permanently `Idle`.

`origin` is retained on the type because Codex distinguishes `exec` from ordinary threads and
that distinction is load-bearing for D-005's `sourceKinds`.

### Session durability (D-011)

Every backend hosts its own sessions in a headless process that outlives agent-viewer, so a
session keeps working while detached on both primary backends. agent-viewer holds no session
lifetime itself and MUST NOT keep a session alive by owning its PTY.

### Status

```
Working                        // a turn is in flight
NeedsInput { reason }          // blocked on the user
Idle                           // loaded, awaiting input, nothing in flight
Done                           // finished
Error                          // systemError
Unknown                        // backend cannot say
```

Codex maps natively from `ThreadStatus` (D-004): `active{waitingOnUserInput|waitingOnApproval}`
to `NeedsInput`, `active{}` to `Working`, `idle` to `Idle`, `systemError` to `Error`,
`notLoaded` to a value derived from PID correlation plus rollout tail rather than assumed
`Done` - a `notLoaded` thread may still be running as an external CLI process.

Claude maps from `state` in `agents --json`: `working`/`blocked`/`idle`.

opencode cannot report per-session status (D-010); it must advertise status as unsupported
rather than flipping every row to Working together.

---

## Backend capability advertisement

Capabilities are backend-advertised, and an unsupported action is a no-op with a footer notice
rather than an error. D-010 makes truthfulness a hard requirement: **a capability that is
advertised and then fails at press time is worse than one advertised as unsupported**, because
the footer notice is the designed affordance for the latter.

opencode has two tiers, selected by whether `opencode.server_url` is configured and reachable
(D-013). The capability set MUST be computed from the live tier, not assumed.

| Capability | Claude | Codex | Opencode (no server) | Opencode (server) |
|---|---|---|---|---|
| `attach` | yes, `Background` origin only | yes, via `--remote`; refuses only when an external process holds the thread | yes | yes, `opencode attach <url> -s <id>` |
| `rename` | **no** (D-006: no daemon op exists) | yes (`thread/name/set`) | raw SQL via `opencode db` | yes, `PATCH {"title":...}` |
| `archive` | yes | yes | **no** (0 of 297 rows; no CLI writes it) | yes, `PATCH {"time":{"archived":<ms>}}` |
| `needs_input` | yes | yes (native `activeFlags`) | **no** (permission table has 0 rows) | yes, `GET /permission`, `GET /question` |
| `stop` | yes | yes | **conditional** - pid-gated, and `list()` sets `pid: None`, so only sessions spawned by this process instance | same |
| `delete` | yes | yes | yes | yes |
| `pr_refs` | yes | yes (`gitInfo`) | **no** | **no** (only `GET /vcs`, branch names) |
| `live_status` | yes | yes | **no** (process-presence only, backend-wide) | yes, `GET /session/status` per id |

Capabilities that are conditional per row (attach, stop) must be evaluated **per session**, not
per backend, or the advertisement lies again in a new way.

---

## Grouping and retention

**Grouping** (FR-007) toggles between project (`cwd`) and state (`status`). Both are pure
functions over `Vec<Session>` and stay in `-core`.

**Retention** (FR-010): unfinished sessions never age out; finished sessions age out after a
configurable window. This matches observed Fleet View behavior - a 16-day-old needs-input
session was still shown while completed ones had rolled off.

```
retain(s) = !is_finished(s.status) || (now - s.updated_at_ms) < window
```

---

## Viewer-local state

Deliberately minimal, and deliberately **not** session-identifying. Principle II forbids shadow
state, so this holds only presentation preferences:

- current grouping mode
- current sort
- collapsed/expanded group set
- retention window

Names, archived flags and status are **never** stored here. That is exactly the shadow state
that produced the "rename is fake" defect.
