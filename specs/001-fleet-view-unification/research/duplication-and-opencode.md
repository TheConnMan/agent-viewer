# Research: session-duplication audit + opencode capability inventory

Repo: `/home/example/git/example-user/agent-viewer`, branch `main`, HEAD `95f9faa`.
Method: static read of every source file that builds or runs a child process, plus read-only
CLI `--help` probes and read-only SQLite inspection. No repo file other than this one was
modified. No session was created, attached to, stopped, renamed, or deleted.

---

## Part A: duplication / fork risk audit

### A.0 Complete inventory of process-spawn sites

Every `Command::new` / spawn in non-test code, from
`grep -rn "Command::new\|spawn_detached\|PtySession::spawn" crates/*/src/`:

| Site | What it runs | Reached by |
|---|---|---|
| `claude.rs:104` | `claude agents --json --all` | every list refresh |
| `claude.rs:66` | `claude --bg --model M --name N <task>` | composer spawn only |
| `claude.rs:81` | `claude rm <short_id>` | Ctrl+X remove only |
| `claude.rs:204` | `claude attach <id>` OR `claude -r <id>` | attach (Enter / Right / reply) |
| `codex/mod.rs:25` | `codex exec --json -C <dir> --sandbox workspace-write [-m M] <task>` | composer spawn only |
| `codex/mod.rs:38` | `codex debug models` | model discovery, once, cached |
| `codex/cli.rs:19` | `codex archive|unarchive <id>` | hide / unhide / remove only |
| `codex/cli.rs:24` | `codex resume <id>` | attach |
| `codex/cli.rs:56` | `codex app-server` (stdio JSON-RPC) | rename only |
| `opencode.rs:105` | `opencode run --dir D --title T [-m M] <task>` | composer spawn only |
| `opencode.rs:129` | `opencode session delete <id>` | remove only |
| `opencode.rs:138` | `opencode db "<UPDATE ...>"` | rename only |
| `opencode.rs:147` | `opencode -s <id>` | attach |
| `opencode.rs:164` | `opencode models` | model discovery, once, cached |
| `pr_status.rs:191` | `gh pr view <url> --json ...` | PR badge refresh |

There are exactly two entry points into `attach_command` and one into `spawn`
(`grep -rn "attach_selected\|attach_session\|spawn_from_composer"`):

- `keys.rs:139` `KeyCode::Right => attach_selected(backends, ui, terminal)?,`
- `keys.rs:166` `attach_selected(backends, ui, terminal)?;` (Enter, empty composer, non-header row)
- `keys.rs:169` `spawn_from_composer(backends, refresher, ui);` (Enter, NON-empty composer)
- `actions.rs:184` `if !attach_session(backends, ui, terminal, &session)? {` (reply delivery)

Nothing auto-attaches, and nothing spawns on a timer, on selection change, on peek, or on
snapshot apply.

### A.1 Risk table

| # | Risk path | file:line | Literal code | Trigger conditions | Severity |
|---|---|---|---|---|---|
| 1 | Claude native attach | `crates/agent-viewer-core/src/claude.rs:211-213` | `Some(short_id) if !short_id.is_empty() => {`<br>`    cmd.arg("attach").arg(short_id);`<br>`}` | Any claude row whose agents-JSON entry carried an `id` key. 29 of 30 live entries on this box. | RULED-OUT |
| 2 | Claude `-r` fallback on an id-less row | `crates/agent-viewer-core/src/claude.rs:214-219` | `_ => {`<br>`    cmd.arg("-r").arg(&session.id);`<br>`    if session.cwd.is_dir() {`<br>`        cmd.current_dir(&session.cwd);`<br>`    }`<br>`}` | Fires whenever `short_id` is `Some("")`. **This is not rare**: `claude agents --json` documents itself as printing "active sessions (interactive **and** background)", and interactive entries carry no `id`. Live on this box right now: 1 of 30 entries, `kind:"interactive"`, keys `pid/cwd/kind/startedAt/sessionId/name` and no `id`. Attaching to that row launches a **second interactive Claude Code process** on the same conversation, not an attach. | THEORETICAL (high-likelihood trigger, unproven outcome) |
| 3 | Reply auto-inject into a `-r` fallback PTY that was already open | `crates/agent-viewer-tui/src/pending_reply.rs:173-179` | `let run_view_ready = if !state.require_run_view {`<br>`    true`<br>`} else if state.fresh_attach {`<br>`    transient_seen && !marker_present`<br>`} else {`<br>`    !marker_present`<br>`};` | Needs all of: a claude row with an EMPTY `short_id`, status `NeedsInput`, and a PTY for that key already in `ui.attached` from a previous attach. Then `fresh_attach=false`, `marker_present` is permanently false (the `"Waking session"`/`"Attaching"` markers are `claude attach` transients that a `-r` process never prints), so the gate passes on the first frame and after a 600 ms settle the payload `"<text>\r"` is written blind into whatever that `-r` process is showing, including a resume picker or a fresh-session composer. That is the old fork bug reachable by a different route. | THEORETICAL (needs an id-less row that is also `blocked`; none exists on this box) |
| 4 | Reply auto-inject on a FRESH `-r` fallback attach | `crates/agent-viewer-tui/src/pending_reply.rs:176` | `transient_seen && !marker_present` | Same preconditions as #3 but with no pre-existing PTY. `transient_seen` can never latch because a `-r` process never prints the attach transient, so the gate never opens, the 50 s timeout fires and `ui.set_notice("reply not delivered; type it in the session")` runs. **This is the safe branch**, called out because it is the reason #3 and #4 differ. | RULED-OUT |
| 5 | Reply injected into the wrong session | `crates/agent-viewer-tui/src/pending_reply.rs:118` and `:218-219` | `let focused = ui.focused.as_ref() == Some(&state.key);`<br>`if let Some(pty) = ui.attached.get_mut(&state.key) {`<br>`    let _ = pty.write_input(&state.payload);` | Impossible. The payload is written only to `ui.attached[state.key]`, and only when `ui.focused == state.key`. Any key the user presses while attached clears the arm first (`keys.rs:226` `ui.pending_reply = None;`). A session that stops being `NeedsInput` aborts (`pending_reply.rs:127`). | RULED-OUT |
| 6 | Composer Enter spawns a session | `crates/agent-viewer-tui/src/keys.rs:163-170` | `} else if ui.composer.is_empty() {`<br>`    if !toggle_group_if_header(ui) {`<br>`        attach_selected(backends, ui, terminal)?;`<br>`    }`<br>`} else {`<br>`    spawn_from_composer(backends, refresher, ui);`<br>`}` | A stray single letter cannot spawn: with an empty composer the hotkeys `q/a/h/u/?/space` act and every other printable only types (`keys.rs:199` `_ => ui.composer.push_char(c),`). Spawning requires a printable followed by Enter. There is no confirmation modal, so one typo plus Enter does create a real background agent, but that is the documented spawn path, not a view or attach side effect. | THEORETICAL (user-error, by design) |
| 7 | Codex attach | `crates/agent-viewer-core/src/codex/cli.rs:23-27` | `pub fn resume_command(id: &str) -> std::process::Command {`<br>`    let mut cmd = std::process::Command::new("codex");`<br>`    cmd.arg("resume").arg(id);`<br>`    cmd`<br>`}` | `codex resume --help` on this box exposes `[SESSION_ID]`, `--last`, `--all`, `-c`, `--enable`. `grep -i "fork\|new session\|branch"` over its full help returns nothing, so codex has no fork-on-resume mode and the viewer always passes an explicit id (never the bare picker form). | RULED-OUT |
| 8 | Opencode attach | `crates/agent-viewer-core/src/opencode.rs:147-151` | `let mut cmd = std::process::Command::new("opencode");`<br>`cmd.arg("-s").arg(&session.id);`<br>`if session.cwd.is_dir() {`<br>`    cmd.current_dir(&session.cwd);`<br>`}` | opencode DOES have a fork mode: `opencode --help` lists `--fork  fork the session when continuing (use with --continue or --session)`. The viewer never passes it. Residual: a stale `-s <id>` whose session row was deleted may drop opencode into a new session; unverified because testing it would start an opencode session. | RULED-OUT for the fork flag; THEORETICAL-LOW for the stale-id case |
| 9 | Any child process spawned by merely refreshing or viewing | `crates/agent-viewer-core/src/claude.rs:104-108` | `let output = std::process::Command::new(&self.binary)`<br>`    .arg("agents")`<br>`    .arg("--json")`<br>`    .arg("--all")`<br>`    .output();` | The only per-tick shell-out. Parent already verified it leaves no residue. The other refresh-adjacent spawns are `gh pr view` (`pr_status.rs:191`, read-only GitHub query) and the once-per-process, `OnceLock`-cached `codex debug models` / `opencode models` probes (`codex/mod.rs:38`, `opencode.rs:164`), both wrapped in a 3 s `run_with_timeout`. Peek reads files and SQLite only; `grep Command::new` finds nothing in `peek.rs`, `peek_cache.rs`, `app.rs`, `main.rs`, `state.rs`, `ui.rs`. | RULED-OUT |
| 10 | Leftover `CLAUDE_AGENTS_SELECT` driver from the pre-`8452e94` path | `crates/agent-viewer-core/src/pty.rs:253-254` | `let mut cmd = std::process::Command::new("claude");`<br>`cmd.arg("agents").env("CLAUDE_AGENTS_SELECT", "work0001");` | `grep -rn CLAUDE_AGENTS_SELECT crates/*/src/` returns only this `#[cfg(test)]` unit test (`spec_from_command_copies_env_pairs`), a doc comment at `pty.rs:18`, and a historical note at `claude.rs:207`. No production code sets the variable and no PTY auto-Enter driver remains anywhere in the tree. | RULED-OUT |
| 11 | Attach mutates `~/.claude.json` as a side effect | `crates/agent-viewer-tui/src/actions.rs:367-373` | `let claude_fallback = session.backend == BackendKind::Claude`<br>`    && session.short_id.as_deref().unwrap_or_default().is_empty();`<br>`if claude_fallback {`<br>`    let home = std::env::var("HOME").unwrap_or_default();`<br>`    let config = std::path::PathBuf::from(&home).join(".claude.json");`<br>`    let _ = ensure_trusted(&config, &session.cwd);`<br>`}` | Fires on exactly the same id-less rows as #2. It writes `projects.<cwd>.hasTrustDialogAccepted = true` into the user's global claude config (atomically, preserving other keys, `claude.rs:256-327`). Not a duplication, but it is a persistent global-config write triggered by a plain attach keypress. | RULED-OUT for duplication; flagged as a view-triggered write |
| 12 | Attach teardown SIGKILLs a process group | `crates/agent-viewer-core/src/pty.rs:213-217` | `unsafe {`<br>`    let pgid = libc::getpgid(pid);`<br>`    if pgid > 0 {`<br>`        libc::kill(-pgid, libc::SIGKILL);`<br>`    }`<br>`}` | Runs on `q`, Ctrl+C, and `PtySession::drop`. The killed group is the attach client's own (portable-pty setsids it), not the daemon-managed bg worker, so the underlying session survives. Guarded against pid reuse by the `self.exited` latch at `pty.rs:204`. | RULED-OUT |

### A.2 Verdict

**No remaining code path duplicates a Claude background agent on attach.** The
`claude attach <short_id>` branch is taken for every row that has a short id, and
`claude attach --help` on this box reads:

```
Usage: claude attach <id>
  Open the background session in this terminal. <- returns to agent view, Ctrl+Z drops back
  to your shell. The session keeps running either way.
```

That is an in-place open, and 29 of the 30 live agents-JSON entries carry an `id`, so it is
the branch that runs in practice.

**The one live, non-hypothetical gap is risk #2, and it is not a background agent at all.**
`claude agents --json` includes `kind:"interactive"` rows (its own `--help` says "Print active
sessions (interactive and background)"), those rows carry no `id`, and `parse_agents_json`
turns a missing `id` into an empty string rather than `None`:

```rust
// crates/agent-viewer-core/src/claude.rs:355-357
let short_id = crate::json_str(&entry, "id")
    .unwrap_or_default()
    .to_string();
```

```rust
// crates/agent-viewer-core/src/claude.rs:376
short_id: Some(short_id),
```

so `attach_command` reaches the `_` arm and runs `claude -r <full-uuid>` pinned to the
session cwd. The live example on this box, captured from `claude agents --json --all`:

```json
{
 "pid": 117244,
 "cwd": "/home/example/git/example-user/agent-viewer",
 "kind": "interactive",
 "startedAt": 1785062538170,
 "sessionId": "74bd56ca-d881-43d9-be30-de4a34189750",
 "name": "agent-viewer-8a"
}
```

Two facts bound how bad that is. In favor: `claude --help` documents
`--fork-session  When resuming, create a new session ID instead of reusing the original`,
and the viewer never passes it, so a plain `-r` reuses the original session ID. Against:
`-r, --resume [value]  Resume a conversation by session ID, or open interactive picker with
optional search term` means an id that does not resolve in that cwd degrades into a picker,
and pressing Enter on an id-less row starts a whole second interactive Claude Code client
against a conversation that another terminal is concurrently writing.

**Two out-of-scope hazards found on the same id-less rows**, noted because they share the
trigger and are more destructive than the duplication:

- `crates/agent-viewer-tui/src/mutations.rs:50-52` runs the kill before the backend rejects
  the remove:
  ```rust
  if let Some(pid) = s.pid {
      let _ = agent_viewer_core::spawn::terminate(pid, s.backend.name());
  }
  ```
  while `crates/agent-viewer-core/src/claude.rs:194-197` only then declines:
  ```rust
  let short_id = session.short_id.as_deref().unwrap_or_default();
  if short_id.is_empty() {
      return Err(Error::Unsupported(self.kind().name()));
  }
  ```
  An interactive row carries the user's own live Claude Code `pid`, so the second stage of
  Ctrl+X SIGTERMs that process group before the remove no-ops. The only thing standing
  between that and killing the user's own session is the comm-prefix guard at
  `crates/agent-viewer-core/src/spawn.rs:120`
  (`if !comm.trim_start().starts_with(expected_comm_prefix) {`) plus `BackendKind::Claude`
  resolving to the literal `"claude"` (`backend.rs:13`).
- `crates/agent-viewer-core/src/claude.rs:117` calls
  `job_state_path(session.short_id.as_deref().unwrap_or_default())`, which for an empty id
  resolves to `~/.claude/jobs/state.json` (the empty component collapses). Harmless today
  because that file does not exist, but it is a path built from a sentinel.

Given all of this, the parent's hypothesis (i) is the likelier explanation for the user's
memory of duplicates, but hypothesis (ii) has a real candidate: the interactive rows.

---

## Part B: opencode capability inventory

Backend source: `crates/agent-viewer-core/src/opencode.rs`. Live CLI on this box:
`/home/example/.opencode/bin/opencode`, version `1.17.20`. Store:
`~/.local/share/opencode/opencode.db`, 32.5 MB, 297 rows in `session`.

| Capability | Supported | How, with evidence |
|---|---|---|
| Enumerate | YES | Direct read-only SQLite. `opencode.rs:60-63`: `"SELECT id, parent_id, directory, title, time_created, time_updated, time_archived \ FROM session ORDER BY time_updated DESC"`. Opened via `crate::open_readonly(&self.db_path)?` (`opencode.rs:56`). Missing DB is a quiet empty backend (`opencode.rs:51-53`). Live count 297. Subagents are detected from `parent_id` (`opencode.rs:78-82`, `companion: parent_id.is_some()`). |
| Live status | COARSE ONLY | One process check for the WHOLE backend, not per session: `opencode.rs:264-270`, `sys.processes().values().any(\|p\| p.name().to_string_lossy().starts_with("opencode"))`, with the comment at `:263` "All opencode sessions share one process, so this is a single best-effort signal." Combined with recency in the pure heuristic `opencode_status` (`opencode.rs:280-292`): Working if live and `age <= 60_000`, Idle if live and `age <= 1_800_000`, Done otherwise. Consequence: every session in the DB flips to Working the moment any opencode process exists and its `time_updated` is fresh. |
| Needs-input detection | **NOT POSSIBLE** | `opencode.rs:277-279`: "Never NeedsInput/Failed/Stopped (no signal exists ...)". Re-verified against the live schema, and the two plausible sources are both dead ends: the `permission` TABLE is project-scoped policy (`CREATE TABLE permission (id, project_id, action, resource, time_created, time_updated)` with a UNIQUE index on `(project_id, action, resource)`) and has **0 rows**; the `session.permission` COLUMN is a static per-session policy array, sampled as `[{"permission":"question","pattern":"*","action":"deny"},{"permission":"plan_enter","pattern":"*","action":"deny"},{"permission":"plan_exit","pattern":"*","action":"deny"}]`, present on 215 of 297 rows and unrelated to any pending ask. **Gate every needs-input UI off for opencode.** |
| Summary / last message | YES, but not on the row | `read_opencode_last_message` (`opencode.rs:186-241`) walks `"SELECT m.id, m.data, p.data FROM message m JOIN part p ON p.message_id = m.id WHERE m.session_id = ?1 ORDER BY m.time_created DESC, m.id DESC, p.time_created ASC"` and returns the most recent message with non-blank text, keeping only `{"type":"text"}` parts (`parsed_text_part`, `:254-260`). `list()` itself sets `summary: String::new()` (`opencode.rs:83`), so the list row is blank until the peek layer calls the helper. |
| Spawn | YES | `opencode.rs:105-118`: `opencode run --dir <dir> --title <truncated 40 chars> [-m <model>] <task>`, detached via `spawn_detached` into a viewer-owned log (`opencode.rs:116`, "Viewer-owned log dir; we do NOT write under ~/.local/share/opencode/."). Returns a real pid: `Ok(Some(pid))`. |
| Attach in place | YES | `opencode.rs:147-151`: `opencode -s <id>`, cwd pinned only when `session.cwd.is_dir()`. `opencode --help` confirms `-s, --session  session id to continue`. The separate `opencode attach <url>` subcommand attaches to a running opencode SERVER, not a session, and is unused. |
| Rename | YES, via raw SQL | `opencode.rs:135-142` shells `opencode db "<sql>"` with `rename_sql` (`:296-303`) building `UPDATE session SET title='<t>' WHERE id='<id>'` with single quotes doubled. `opencode db --help` confirms `opencode db [query]  open an interactive sqlite3 shell or run a query`. This is a CLI-mediated DB write rather than a semantic rename subcommand; no such subcommand exists (`opencode session --help` lists only `list` and `delete`). |
| Archive / hide | **NOT POSSIBLE** | `capabilities()` sets `hide: false` (`opencode.rs:41`). The schema HAS `time_archived` and `list()` reads it into `hidden` (`opencode.rs:77`, `hidden: time_archived.is_some()`), but no CLI path sets it and there is no `Backend::hide` impl for opencode. Live: `select count(*) from session where time_archived is not null` returns **0**, so in practice every opencode row is visible. Gate the hide/unhide keys off. |
| Delete / remove | YES | `opencode.rs:127-134`: `opencode session delete <id>` via `run_checked`. Confirmed present in `opencode session --help`. |
| Stop | PARTIAL | `capabilities()` sets `stop: true` (`opencode.rs:42`) but `stop()` is pid-gated: `opencode.rs:120-126`, `match session.pid { Some(pid) => crate::spawn::terminate(pid, "opencode"), None => Err(Error::Unsupported(...)) }`, and `list()` always sets `pid: None` (`opencode.rs:86-87`, "Overlay fills the pid for viewer-spawned opencode sessions."). So stop works ONLY for sessions this viewer spawned in this install's lifetime; every pre-existing session reports unsupported at press time despite advertising the capability. |
| Reply | **NOT POSSIBLE** | `capabilities()` sets `reply: false` (`opencode.rs:46`), and `open_reply` refuses with `"{} does not support reply"` (`actions.rs:113-115`). Follows from the missing needs-input signal. |
| Model list | YES | `opencode.rs:163-170` runs `opencode models` under a 3 s `run_with_timeout`; `parse_opencode_models` (`:174-181`) takes each non-blank trimmed line as a `provider/model` id. `available_models` prepends `"default"` and dedups (`:93-102`). `opencode models [provider]  list all available models` confirmed in `opencode --help`. |
| PR association | **NOT POSSIBLE** | `list()` hardcodes `pr_refs: Vec::new()` (`opencode.rs:88`). No PR column exists in the `session` schema, and `share_url` and `path` are NULL on every recent row sampled. The CLI has `opencode pr <number>  fetch and checkout a GitHub PR branch, then run opencode`, which is a spawn helper, not a lookup, so there is no back-reference from a session to a PR. Gate PR badges off for opencode. |

### B.1 Session columns the backend does not currently read

Available in the live schema but unused by `list()`: `slug` (human ids such as `happy-squid`,
`curious-squid`), `agent` (`build` on every sampled row), `model` (a JSON blob such as
`{"id":"grok-4.5","providerID":"opencode-go","variant":"default"}`), `cost` (real, e.g.
`0.928936`), `tokens_input` / `tokens_output` / `tokens_reasoning` / `tokens_cache_read` /
`tokens_cache_write`, `summary_additions` / `summary_deletions` / `summary_files` /
`summary_diffs`, `time_compacting`, `revert`, `share_url`, `workspace_id`, `path`,
`project_id`, `version`, `metadata` (NULL on all sampled rows). The `project` and `workspace`
tables carry `worktree`, `vcs`, `name`, `branch`, and `directory`.

### B.2 Richer surfaces the CLI exposes but the backend does not use

Stated as fact, not as a recommendation: `opencode serve` (headless server),
`opencode attach <url>`, `opencode acp` (Agent Client Protocol), `opencode export [sessionID]`,
and `opencode stats`.

---

## What I could not determine

1. Whether `claude -r <uuid>` against a **currently live** interactive session produces a
   second entry in `claude agents --json`, silently forks despite the absence of
   `--fork-session`, or refuses. Testing requires starting a Claude session, which the task
   forbids. This is the single unresolved question behind risk #2.
2. What `claude -r <uuid>` does when the uuid does not resolve in the pinned cwd. The help
   text says it degrades to "interactive picker with optional search term", but whether an
   empty picker exits or drops into a new-session composer is unverified.
3. Whether any `kind:"background"` agents-JSON entry ever lacks an `id` key. All 29 background
   entries on this box had one; only the single `kind:"interactive"` entry did not. Risks #3
   and #11 depend on this, and risk #3 additionally needs such a row to be `blocked`.
4. Whether `opencode -s <stale-or-deleted-id>` errors or starts a fresh session. Verifying it
   would start an opencode session.
5. Whether `codex resume <id>` on an ARCHIVED thread unarchives it, errors, or creates a new
   thread. The viewer's remove for codex is `codex archive` (`codex/mod.rs:213-215`), so an
   archive-then-attach sequence is reachable, but exercising it would mutate real state.
6. Whether the user's remembered duplicates predate `8452e94` (2026-07-13). The claude
   transcript directory for this repo holds a single session jsonl plus a `memory/` dir, which
   is neither evidence for nor against historical forking.
