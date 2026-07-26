# Quickstart: Verifying Fleet View Unification

**Feature**: 001-fleet-view-unification | **Date**: 2026-07-26

Every claim in this feature is verifiable locally on this box. This is the manual verification
path; it is not a substitute for the automated suite, which must pass first.

```bash
cargo clippy --workspace              # must be clean
~/.cargo/bin/cargo test --workspace --no-fail-fast > /tmp/test.log 2>&1
```

Use the direct binary, not plain `cargo` - the `rtk` wrapper compresses output to one-line
summaries, and `--no-fail-fast` is required or the run stops at the first failing binary. Note
the known flaky test (`pty_tests::pty_kill_returns_when_grandchild_holds_slave`, 2s deadline);
confirm any failure standalone before treating it as a break.

---

## 1. The list is unified and filtered (FR-001, FR-003, FR-012)

```bash
cargo run -p agent-viewer-tui
```

Expect: one list, sessions from every available backend, each visually attributable to its
backend at a glance.

Cross-check the counts against the backends' own truth:

```bash
claude agents --json --all | jq '[.[] | select(.kind == "background")] | length'
codex app-server daemon version | jq -r .status     # must be "running"
```

- **No interactive Claude rows.** Fleet View does not list them (D-012). If one appears, that is
  a parity defect.
- **No Codex subagent/companion rows.** This is server-side via `sourceKinds` (D-005), not a
  viewer heuristic. The measured baseline was 4,194 total Codex threads with 54% of active ones
  being companions, so a list that looks Codex-dominated means the filter is not applied.
- **`exec` rows must still appear.** The default `sourceKinds` drops them; if `codex exec` runs
  are missing, the parameter was not passed explicitly.

## 2. Attach joins the real session and never duplicates (FR-004, FR-005, SC-007)

Take a census, attach, detach, take it again. The counts must be identical.

```bash
claude agents --json --all | jq length                  # before
# ... attach to a Claude row in the TUI, then detach ...
claude agents --json --all | jq length                  # after: unchanged
```

For Codex, the equivalent census is the daemon's own loaded-thread list:

```bash
codex app-server daemon version | jq -r .socketPath
# with a WebSocket client: thread/loaded/list before, during and after attach
```

Expect exactly one entry for the attached thread while the native TUI is up, and the thread
still loaded after the TUI exits.

**Verify it is the native client**, not a reimplementation: the attached view must be
indistinguishable from running `claude attach <short_id>` or
`codex resume --remote unix://<socketPath> <id>` by hand (D-001).

## 3. Sessions keep working while detached (D-011)

The point of daemon hosting. Start work, detach, confirm progress continued.

1. Start a session from the TUI and give it a task that takes a little while.
2. Detach. Quit agent-viewer entirely.
3. Relaunch. The row must show progress made while nothing was attached.

If the session died on detach, agent-viewer is owning the process rather than letting the
backend's host own it - a contract violation.

## 4. Rename is real (FR-006, SC-005)

Rename a **Codex** row in the TUI, then confirm the backend agrees:

```bash
codex app-server daemon version   # then query thread/list and compare `name`
```

The new name must survive a full restart of agent-viewer. If it does not, it was viewer-local
shadow state - the original defect.

On **Claude**, rename must be a no-op with a footer notice (D-006). Verify it does **not**
connect to the daemon rendezvous socket; that connection evicts the daemon's supervisor
connection for zero benefit.

## 5. State is fresh (FR-009, SC-006)

Trigger a state change out of band and watch the row without touching the keyboard:

```bash
# in another terminal, start work in a session agent-viewer is displaying
```

The row must reflect the change within 2 seconds, with no manual refresh. Check all three
transitions that matter: idle to working, working to needs-input, working to done.

## 6. Retention (FR-010, SC-004)

Unfinished sessions never age out; finished ones age out after the configured window. Confirm a
long-idle needs-input session is still listed - the observed Fleet View baseline had a 16-day-old
needs-input session still shown while completed ones had rolled off.

## 7. Key map parity (FR-011)

Fleet View's footer is context-sensitive, so check each state, not just the default screen:

| Context | Expected footer |
|---|---|
| No row selected | `enter to collapse \| ctrl+x to delete all \| ? for shortcuts` |
| Row selected | `enter to open \| space to reply \| ctrl+x to delete \| ? for shortcuts` |
| Composer has text | `enter to create \| esc to clear` |
| `?` hint bar | `ctrl+s to switch views \| @ to mention \| ? to close \| ctrl+j for newline \| esc to quit` |

`ctrl+s` toggles project versus state grouping (FR-007). Note `?` typed into a **non-empty**
composer is literal text, not help - send `esc` first.

Peek and reply are deliberately out of scope (FR-013). Their absence is a recorded divergence,
not a missing feature.

## 8. Capabilities never lie (D-010)

For every backend, every advertised action must succeed when pressed. An action the backend
cannot perform must be advertised as unsupported and produce a footer notice - never an error,
and never a silent failure. Check the conditional ones specifically, since these are per-row:
Claude `attach`, Codex `attach`, opencode `stop`.

---

## Driving the TUI headlessly

A fresh pty has a 0x0 winsize and renders nothing; setting `COLUMNS`/`LINES` does not help. Set
the size on the pty fd before spawning:

```python
fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", rows, cols, 0, 0))
```

Read frames from the master and grep the **cumulative** stripped buffer - ratatui redraws are
cell diffs, so a per-interval read misses text drawn in an earlier frame.

Two traps that have already cost time:

- Writing keys to another process's pty master via `/proc/<pid>/fd/<n>` silently fails; that
  symlink resolves to `/dev/ptmx` and opening it allocates a **new** pty pair. The driver must
  own the master fd.
- A `claude` spawned from inside a Claude session inherits `CLAUDE_CODE_CHILD_SESSION=1` and
  never registers with the agents daemon. Unset it, or the experiment measures nothing.
