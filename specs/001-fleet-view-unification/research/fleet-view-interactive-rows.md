# Fleet View and interactive Claude sessions

Empirical study, 2026-07-26, on `remote-dev`. Claude Code `v2.1.220`.

Question: what does Claude Code's interactive Fleet View (`claude agents`, no `--json`) do
with **interactive** sessions, given that `claude agents --json` self-documents as printing
"active sessions (interactive and background)"?

## Method

Two throwaway interactive sessions were spawned under `pty` + `pyte` in scratch dirs
(`.../tmp/ivtest3`, `.../tmp/ivtest4`), and `claude agents` was run in a third pty. All
screen text below is post-`pyte`-render, ANSI stripped.

Two environment gotchas that cost real time and are worth recording:

1. Writing keys to another process's pty master via `/proc/<pid>/fd/<n>` does **not** work.
   That symlink resolves to `/dev/ptmx`, and opening it allocates a *brand new* pty pair.
   The driver process must own the master fd and do the writes itself.
2. A `claude` spawned from inside another Claude session inherits `CLAUDE_CODE_CHILD_SESSION=1`
   and silently runs with transcript persistence off:

   ```
     ⚠ Transcript saving is off — inherited CLAUDE_CODE_CHILD_SESSION marker · restart with CLAUDE_CODE_FORCE_SESSION_PERSISTENCE=1 …
   ```

   A session in that state **never registers with the agents daemon at all** and is invisible
   to `claude agents --json`. Unset `CLAUDE_CODE_CHILD_SESSION` (and set
   `CLAUDE_CODE_FORCE_SESSION_PERSISTENCE=1`) or the experiment silently measures nothing.

---

## Q1. Does Fleet View list interactive sessions at all?

**No. Fleet View's TUI lists background sessions only.** Three independent confirmations.

### Evidence A: the JSON does carry interactive rows

With two throwaway interactive sessions alive, `claude agents --json` returns them, and they
are shaped differently from background rows:

```json
{"pid":203939,"cwd":"/home/example/git/example-user/claude-settings/jobs/0214d552/tmp/ivtest3","kind":"interactive","startedAt":1785063978527,"sessionId":"599bdf0b-6cae-4348-b75c-62af08319e2c","name":"ivtest3-51","status":"idle"}
{"pid":211453,"cwd":"/home/example/git/example-user/claude-settings/jobs/0214d552/tmp/ivtest4","kind":"interactive","startedAt":1785064081697,"sessionId":"06bbea79-f3dc-4816-a590-f1200fc08fc3","name":"ivtest4-38","status":"idle"}
```

versus a background row from the same call:

```json
{"pid":178698,"id":"7fb47b1b","cwd":"/home/example/git/example-user/claude-settings/jobs/0214d552/tmp/ivtest","kind":"background","startedAt":1785063825559,"sessionId":"7fb47b1b-5f51-4233-979e-5571b4ba84fd","name":"7fb47b1b","status":"idle","state":"done"}
```

Differences, confirmed on every interactive row observed:

| key | background | interactive |
| --- | --- | --- |
| `id` (8-hex short id) | present, `= sessionId[0..8]` | **absent** |
| `kind` | `"background"` | `"interactive"` |
| `state` | present (`done`/`working`/...) | **absent** |
| `status` | present | present (`"idle"`) |
| `pid` | present only while a process is live | present |
| `name` | user/agent-assigned title (`"Website Redesign"`) | auto `<dirname>-<n>` (`"ivtest3-51"`) |

Kind census with both throwaways alive:

```
background: 30
interactive: 2
```

### Evidence B: the Fleet View header count equals the background count exactly

`claude agents` header, captured with 30 background + **1** interactive session alive:

```
 ▐▛███▜▌   Claude Code v2.1.220
▝▜█████▛▘  Opus 5 (1M context) · ~/git/example-user/claude-settings/jobs/0214d552/tmp/ivtest3
  ▘▘ ▝▝    5 awaiting input · 1 working · 24 completed
```

`5 + 1 + 24 = 30`. Then a **second** interactive session was started (`background: 30`,
`interactive: 2`) and Fleet View was relaunched. The header is byte-identical:

```
 ▐▛███▜▌   Claude Code v2.1.220
▝▜█████▛▘  Opus 5 (1M context) · ~/git/example-user/claude-settings/jobs/0214d552/tmp/ivtest3
  ▘▘ ▝▝    5 awaiting input · 1 working · 24 completed
```

Adding an interactive session changes nothing. The Fleet View total tracks `background` only.

Grepping the **cumulative** pty byte stream (not a single frame) of that Fleet View run for
the two interactive session names returns zero hits:

```
--- grep for interactive names in cumulative raw ---
0        # "ivtest3-51"
0        # "ivtest4-38"
(0 = absent)
```

Note: Fleet View is also the session Claude Code is itself run *from* in these captures
(cwd `.../ivtest3`), and it still does not list itself.

### Evidence C: `--cwd` scoped to a dir containing only an interactive session is empty

`claude agents --cwd /home/example/.claude/jobs/0214d552/tmp/ivtest3`, run while the
interactive session with exactly that cwd was alive and present in `--json`:

```
 ▐▛███▜▌   Claude Code v2.1.220
▝▜█████▛▘  Opus 5 (1M context) · ~/git/example-user/claude-settings/jobs/0214d552/tmp/ivtest3
  ▘▘ ▝▝    0 awaiting input · 0 working · 0 completed

Needs input
 Sessions that have a question or need your decision land here

Working
 Sessions Claude is actively working on — they keep running even if you close the terminal

Completed
 Finished sessions wait here for you to review
```

The full empty state, with an interactive session in that exact directory.

### Corroborating self-documentation

`claude agents --help`:

```
Usage: claude agents [options]

Manage background agents
```

```
  --all                                 With --json: also include completed
                                        background sessions
  --cwd <path>                          Show only background sessions started
                                        under <path>
```

```
  --json                                Print active sessions (interactive and
                                        background) as a JSON array and exit
                                        (for scripting; does not require a TTY)
```

The command is scoped to "background agents"; only `--json` advertises interactive rows. The
interactive shape is a **scripting-only** surface, not something the TUI surfaces.

---

## Q2. How are interactive rows visually distinguished?

**Not applicable — they are never rendered.** No glyph, colour, label, section or dimming
exists for them, because Fleet View has no interactive row.

For reference, the row vocabulary Fleet View actually uses (all background):

```
Ready for review
 ✻ CUR-1667 UI Runner Usage Events      PR #1032: browser-use cost tracking ready; awaiting kubexec approval + merge …  #1032   3d

Needs input
 ✻ RS-456 Stale Schema Run Mode         say the word and I'll run it                                                     #394   1d

Working
 ✢ Specs                                reverse-engineering agent-viewer specs; protocol analysis complete, migration…          1h

Completed
 ∙ AgentOS PR Review Daily              Report: `.projects/pr-review-daily/reports/2026-07-26.md`; 3 PRs blocking (#9…          2m
 ∙ Hold                                 stopped                                                                                57s
```

Sections are `Ready for review` / `Needs input` / `Working` / `Completed`; glyphs are `✻`
(attention/complete-with-summary), `✢` (working), `∙` (idle/done/stopped). There is no fourth
section and no fourth glyph.

`ctrl+s` toggles between a cwd-grouped layout (headers like `~/git/example-org/example-app`) and the
status-grouped layout above. Neither layout introduced an interactive row.

---

## Q3. Is an interactive row selectable / actionable? What happens on Enter?

**Not applicable — there is no interactive row to select, so Fleet View offers no Enter
behaviour for one.** By construction, Fleet View never has to decide how to attach to an
interactive session, because it never presents one.

This is the load-bearing answer for the defect: strict parity means agent-viewer has no
Fleet View behaviour to copy for interactive rows, because Fleet View's chosen behaviour is
*omission*.

---

## Q4. What do the other hotkeys do on an interactive row?

**Not applicable, same reason.** The Fleet View footer advertises only list-level and
composer-level keys:

```
──────────────────────────────────────────────────────────────────────────────────────────────
❯ describe a task for a new session
──────────────────────────────────────────────────────────────────────────────────────────────
  enter to collapse · ctrl+x to delete all · ? for shortcuts
```

and the `?` overlay (captured from the same agents component rendered at Claude Code startup):

```
  ctrl+s to switch views    @ to mention    ? to close
  ctrl+j for newline        esc to quit
```

No key was pressed against a live row: every row in the fleet belongs to a real session on
this box, and the task constraints forbid acting on them. See "what I could not determine".

---

## Q5. Anything else that meaningfully differs

1. **`claude agents --json` (no `--all`) already includes interactive rows.** With 2 throwaways
   alive the active list was 8 entries: 6 active background + 2 interactive. So a consumer that
   reads the non-`--all` JSON — which agent-viewer does — receives interactive rows today,
   unfiltered.

2. **`name` is auto-generated for interactive rows** (`ivtest3-51`, `ivtest4-38`: basename of
   cwd plus a numeric suffix). It is not a user-meaningful title and will never match the
   ticket/task naming that background rows carry. A fleet list that mixed them would look
   inconsistent even before any action semantics.

3. **Interactive rows carry no `state`**, only `status: "idle"`. `parse_agents_json` in
   `crates/agent-viewer-core/src/claude.rs` maps a missing `state` to `Status::Idle`, so an
   interactive row currently renders as a permanently idle session regardless of whether the
   human is actively typing in it.

4. **Fleet View is also Claude Code's startup screen** when background sessions exist. Bare
   `claude` in a fresh dir rendered the same agents panel, with the same composer, and typing
   text + Enter there **dispatches a new background session** rather than starting a chat.
   (Observed accidentally: a background session `7fb47b1b` was created that way in a scratch
   dir and cleaned up.) That composer never targets an interactive session either.

5. **Fleet View does not list the session it is running inside.** The capture above was taken
   from cwd `.../ivtest3` while the interactive session for `.../ivtest3` was alive, and no
   self-row appeared. Fleet View has no concept of "the session you are in".

---

## What I could not determine

- **Whether any hidden toggle reveals interactive rows.** `?` opened the shortcuts overlay in
  the startup-embedded panel but did not register in the standalone `claude agents` pty at
  45x130; only `ctrl+s` (view switch) was exercised there. A key I did not press could in
  principle reveal a hidden section, though the header counts (Evidence B) argue strongly
  against any such section existing, since the totals themselves exclude interactive sessions.
- **What Enter/kill/rename would do if an interactive row somehow were selected.** Untestable
  without a listed row, and pressing keys against the real background rows on this box was
  out of scope by constraint. Recorded as unknown rather than guessed.
- **Behaviour when an interactive session is mid-turn (`status` other than `idle`).** Both
  throwaways were captured at `status: "idle"`; a `working` interactive row was not observed,
  so it is unproven that a busy interactive session stays hidden (though the count evidence
  suggests the filter is on `kind`, not `status`).

---

## Recommendation for agent-viewer

Fleet View's answer to "what do you do with an interactive session" is **do not show it**.
Under strict parity, agent-viewer should do the same: **filter Claude rows to
`kind == "background"` and drop `kind == "interactive"` entirely** in the Claude backend list.

That is a stronger fix than making the attach command smarter, because it removes the failure
mode at the source:

- The `claude.rs:355` `unwrap_or_default()` on a missing `id` stops being reachable for
  interactive rows, so the `attach_command` fallthrough to `claude -r <full-uuid>` (which
  spawns a *second* client instead of attaching) can no longer fire.
- Every other interactive-row wart disappears with it: the meaningless auto `name`, the
  missing `state` rendering as permanently `Idle`, and the fact that stop/rename/remove have
  no defined meaning against a live human-attended terminal.
- It matches the CLI's own framing (`Manage background agents`; `--cwd` and `--all` are both
  documented as background-only). The interactive rows in `--json` read as a scripting
  affordance, not a fleet-list member.

If a future product decision wants interactive sessions visible, that is a deliberate
*divergence* from Fleet View and needs its own design (a distinct section, and an attach story
that does not mean "launch a duplicate client"). It is not the parity fix.
