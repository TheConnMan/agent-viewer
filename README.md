# codex-agent-viewer

A terminal viewer for OpenAI Codex sessions, modeled on Claude Code's `claude agents`
view. Codex has no built-in "see all my sessions" console; this fills that gap.

It reads Codex's own global session registry (`~/.codex/state_*.sqlite`, table `threads`)
plus the rollout transcripts under `~/.codex/sessions/`, and lets you:

1. **Create** new background Codex sessions (fire-and-forget, like `claude --bg`).
2. **See all** Codex sessions, including background (`codex exec`) ones, however launched.
3. **Hide / unhide** sessions (like dismissing rows in `claude agents`).
4. **Group** sessions by project (working directory).

Built in Rust. See `SPEC.md` for the full architecture and the evidence behind it.

## Status

Under construction (bootstrapped from a research-backed spec). Not yet functional.
