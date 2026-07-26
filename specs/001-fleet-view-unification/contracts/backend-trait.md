# Contract: Backend trait

The single seam every backend implements in `agent-viewer-core`. No `ratatui`/`crossterm` types
appear here (Principle VI).

## Enumeration

```
fn list(&self) -> Result<Vec<Session>>
```

- MUST return backend-native filtering already applied (FR-003): subagent/companion threads and
  archived sessions excluded by the backend's own mechanism, not by viewer-side heuristics.
- MUST populate `short_id` as `None` when the backend supplies no short id. Returning `""` is a
  contract violation - it is the D-003 duplication bug.
- The Claude implementation MUST drop rows whose `kind` is not `"background"` (D-012). Fleet
  View does not list interactive sessions, and such rows carry neither `id` nor `state`.
- MUST NOT block longer than the refresh budget; enumeration participates in the 2s bound
  (FR-009).

## Capability advertisement

```
fn capabilities(&self) -> Capabilities              // backend-wide
fn capabilities_for(&self, s: &Session) -> Capabilities  // per row
```

- An advertised capability that fails at press time is a **contract violation**, not a runtime
  error (D-010). If a capability is conditional, it MUST be reported per row.
- Known per-row conditions: `attach` on Claude requires `origin == Background`; `attach` on
  Codex requires `status == NotLoaded`; `stop` on opencode requires a known pid.
- An unsupported action is a no-op with a footer notice. It MUST NOT surface as an error.

## Attach

```
fn attach_command(&self, s: &Session) -> Result<Command, AttachRefusal>
```

- MUST return a command that spawns the backend's **own native client** (D-001). Implementations
  MUST NOT return a command that renders session contents in agent-viewer.
- MUST return `Err(AttachRefusal)` rather than a command whenever attaching could produce a
  second client or session for the same thread (Principle IV, FR-005a). Refusal carries a
  human-readable reason for the inline error.
- MUST NOT pass any fork flag. Specifically: never `--fork-session` (Claude), never `--fork`
  (opencode).

## Mutations

```
fn rename(&self, s: &Session, name: &str) -> Result<()>
fn archive(&self, s: &Session, hidden: bool) -> Result<()>
fn stop(&self, s: &Session) -> Result<()>
fn delete(&self, s: &Session) -> Result<()>
```

- MUST go through the backend's native channel. A viewer-local override is a contract violation
  (Principle II) - this is the "rename is fake" defect.
- `rename` on Claude MUST be unimplemented and advertised as such. It MUST NOT connect to the
  daemon rendezvous socket, which evicts the daemon's supervisor connection for zero benefit
  (D-006).
- A mutation whose CLI reports partial success MUST surface that output rather than reading a
  zero exit code as success. Specifically `claude rm` prints `kept <id> - worktree <reason>`
  when it declines to delete, and matches ids by **prefix**, exiting 1 on ambiguity.

## Liveness

```
fn subscribe(&self, sink: StatusSink) -> Result<Subscription>
```

- Implementations SHOULD push status changes as events where the backend allows (D-007).
- Implementations that cannot MUST still be correct under the poll backstop; the 2s bound
  (FR-009) is guaranteed by the poll, not by the subscription.
- A backend that cannot report per-session status MUST advertise `live_status: false` rather
  than reporting a backend-wide signal per row (D-010, opencode).
