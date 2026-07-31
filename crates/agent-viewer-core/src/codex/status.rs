use super::rollout::TailState;
use crate::backend::Status;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

/// Who holds a rollout's fd: the process id, plus whether that process is the shared
/// `codex app-server` daemon rather than the session's own codex process. The distinction is
/// load-bearing for `stop`: the daemon holds the rollout fd of EVERY thread it hosts, so
/// handing its pid back as the session's pid would let a SIGTERM take down the daemon and
/// every other session in it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RolloutOwner {
    pub pid: u32,
    pub daemon: bool,
}

/// What the platform can prove about ownership of one rollout file descriptor.
///
/// `Closed` is an observed result from a reliable scan. `Unavailable` means the platform
/// cannot perform that scan, so it must not be treated as evidence that the process exited.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FdSignal {
    Held(RolloutOwner),
    Closed,
    Unavailable,
}

/// One sweep's worth of codex process facts: who holds which rollout open, and which threads
/// somebody is currently sitting in.
pub struct CodexProcessScan {
    pub open_rollouts: HashMap<PathBuf, RolloutOwner>,
    /// Thread ids appearing in a codex process's argv. A TUI that joined a daemon-hosted thread
    /// runs `codex resume --remote unix://<sock> <thread-id>`, so its argv names the thread.
    pub attached_threads: HashSet<String>,
    fd_evidence_available: bool,
}

impl CodexProcessScan {
    /// Assemble a scan from already-collected evidence.
    ///
    /// The platform sweeps below build theirs through it, and it is the only way to drive
    /// `resolve_scanned` - the resolver entry point `list` actually calls - without a live codex
    /// process to observe. `fd_evidence_available` is false only where procfs does not exist.
    pub fn from_evidence(
        open_rollouts: HashMap<PathBuf, RolloutOwner>,
        attached_threads: HashSet<String>,
        fd_evidence_available: bool,
    ) -> CodexProcessScan {
        CodexProcessScan {
            open_rollouts,
            attached_threads,
            fd_evidence_available,
        }
    }

    fn fd_signal(&self, rollout_path: &Path) -> FdSignal {
        if !self.fd_evidence_available {
            return FdSignal::Unavailable;
        }
        self.open_rollouts
            .get(rollout_path)
            .copied()
            .map(FdSignal::Held)
            .unwrap_or(FdSignal::Closed)
    }
}

/// IMPURE scanner (live-e2e-verified only, no unit tests): enumerate processes via
/// sysinfo whose name starts with "codex", read /proc/<pid>/fd/* via
/// std::fs::read_link, collect target paths -> owning codex process. Strip a trailing
/// " (deleted)" suffix. Unreadable /proc entries are skipped silently. The same pass collects
/// the attached thread ids from argv, because this runs every refresh tick over thousands of
/// threads and a second sweep (or a per-row daemon query) would land on the render path.
#[cfg(target_os = "linux")]
pub fn scan_codex_processes() -> CodexProcessScan {
    let mut sys = sysinfo::System::new();
    // The command line must be requested explicitly: `refresh_processes` refreshes memory,
    // cpu, disk usage, exe and tasks but NOT `cmd`, so `process.cmd()` comes back EMPTY under
    // it and every process reads as "not the daemon" (caught by the live e2e). Ask for the one
    // field this scan actually reads beyond the name.
    sys.refresh_processes_specifics(
        sysinfo::ProcessesToUpdate::All,
        true,
        sysinfo::ProcessRefreshKind::nothing().with_cmd(sysinfo::UpdateKind::OnlyIfNotSet),
    );
    let mut scan = CodexProcessScan::from_evidence(HashMap::new(), HashSet::new(), true);
    // The two host signals are a UNION, not a fallback chain. Neither is complete on its own:
    // the socket table misses an app-server on the `stdio://` transport (it holds no socket at
    // all), and argv misses a host whose command line this crate cannot parse. Either saying
    // "host" is enough, because the mistake in that direction is recoverable and the other one
    // SIGTERMs a process hosting other people's threads.
    let listening = std::fs::read_to_string("/proc/net/unix")
        .map(|table| listening_socket_inodes(&table))
        .unwrap_or_default();
    for (pid, process) in sys.processes() {
        if !process.name().to_string_lossy().starts_with("codex") {
            continue;
        }
        let args: Vec<_> = process
            .cmd()
            .iter()
            .map(|arg| arg.to_string_lossy())
            .collect();
        for arg in &args {
            if looks_like_thread_id(arg) {
                scan.attached_threads.insert(arg.to_string());
            }
        }
        let fd_dir = format!("/proc/{}/fd", pid.as_u32());
        let Ok(entries) = std::fs::read_dir(&fd_dir) else {
            continue;
        };
        // A set, not a list: the shared daemon holds the rollout fd of EVERY thread it hosts,
        // so a linear `contains` over the fds already seen made this pass quadratic in the fd
        // count on exactly the process that has the most of them.
        let mut held: HashSet<PathBuf> = HashSet::new();
        let mut holds_a_listening_socket = false;
        for entry in entries.flatten() {
            let Ok(target) = std::fs::read_link(entry.path()) else {
                continue;
            };
            let display = target.to_string_lossy();
            if let Some(inode) = socket_inode(&display) {
                holds_a_listening_socket |= listening.contains(&inode);
                continue;
            }
            let stripped = display.strip_suffix(" (deleted)").unwrap_or(&display);
            held.insert(PathBuf::from(stripped));
        }
        let owner = RolloutOwner {
            pid: pid.as_u32(),
            daemon: holds_a_listening_socket || is_daemon_process(&args),
        };
        for path in held {
            record_owner(&mut scan.open_rollouts, path, owner);
        }
    }
    scan
}

/// Platforms without Linux procfs cannot distinguish a closed rollout from one whose owning
/// process is still live. Return explicit unavailable evidence and make no process claims.
#[cfg(not(target_os = "linux"))]
pub fn scan_codex_processes() -> CodexProcessScan {
    CodexProcessScan::from_evidence(HashMap::new(), HashSet::new(), false)
}

/// PURE: the inode of an fd whose /proc link target is `socket:[N]`, else None.
fn socket_inode(link_target: &str) -> Option<u64> {
    link_target
        .strip_prefix("socket:[")?
        .strip_suffix(']')?
        .parse()
        .ok()
}

/// PURE: inodes of every unix socket that has called `listen()`, from /proc/net/unix.
///
/// Columns are `Num RefCount Protocol Flags Type St Inode Path`. The listening marker is
/// `SO_ACCEPTCON` (0x10000) in FLAGS, not the state: `St` comes from `sock->state`, where 01
/// is SS_UNCONNECTED (which a plain unbound socket also reports) and 03 is SS_CONNECTED.
/// Confirmed against the live table on 2026-07-27 - the daemon's accepting socket is
/// `Flags=00010000 St=01` while a client on the same path is `Flags=00000000 St=03`.
///
/// Intersected against a codex process's own fds this says "this process is a server", which
/// is what an app-server hosting other people's threads IS. A `codex exec` session runs its
/// app-server in process and listens on nothing; an interactive session holds a CONNECTED
/// socket to the daemon, which is deliberately not matched.
///
/// Deliberately NOT filtered to the control-socket path. A second app-server on a custom
/// `--listen` endpoint hosts threads exactly as the managed daemon does, and a path filter
/// would classify it as an ordinary session, exposing its pid to a SIGTERM that would kill
/// every thread inside it.
fn listening_socket_inodes(proc_net_unix: &str) -> HashSet<u64> {
    const SO_ACCEPTCON: u64 = 0x10000;
    proc_net_unix
        .lines()
        .filter_map(|line| {
            let mut fields = line.split_whitespace().skip(3);
            let flags = u64::from_str_radix(fields.next()?, 16).ok()?;
            let inode = fields.nth(2)?.parse().ok()?;
            (flags & SO_ACCEPTCON != 0).then_some(inode)
        })
        .collect()
}

/// PURE: record who holds `path`, with the DAEMON always winning the entry.
///
/// Two codex processes can hold the same rollout at once (SPEC.md records the case: a foreign
/// `codex resume` of a live thread ends with both processes holding it), and `sys.processes()`
/// iterates a HashMap, so without a precedence rule the winner is random per tick. On a tick
/// where the non-daemon won, `daemon_hosted` would flip false and `pid` would become the other
/// client's pid, so Ctrl+X would SIGTERM the process group of somebody's own codex TUI. A
/// daemon-held rollout must never be downgradable to a signalable pid.
fn record_owner(open: &mut HashMap<PathBuf, RolloutOwner>, path: PathBuf, owner: RolloutOwner) {
    match open.entry(path) {
        std::collections::hash_map::Entry::Occupied(mut held) => {
            if owner.daemon && !held.get().daemon {
                held.insert(owner);
            }
        }
        std::collections::hash_map::Entry::Vacant(slot) => {
            slot.insert(owner);
        }
    }
}

/// IMPURE: just the open-rollout map from one sweep, for callers that do not need the attached
/// set (no second sweep; `scan_codex_processes` does the work).
pub fn open_rollout_paths() -> HashMap<PathBuf, RolloutOwner> {
    scan_codex_processes().open_rollouts
}

/// PURE: is this codex process an app-server hosting other people's threads, as far as its
/// COMMAND LINE can say? The deciding signal is the listening control socket
/// (`scan_codex_processes`); this is the backstop for when the socket table names no daemon,
/// and `is_app_server` documents why it is deliberately crude.
///
/// The predicate USED to also treat "holds more than one rollout fd" as proof of the daemon.
/// That is measurably false and was removed: a plain
/// `codex exec --json -C ... --dangerously-bypass-approvals-and-sandbox` (pid 2910115, live on
/// 2026-07-27) held TWO rollouts, its own plus a subagent thread it spawned (registry source
/// `{"subagent":{"thread_spawn":...}}`). Misclassifying an exec session as the daemon routes
/// attach to a `--remote` join on a daemon that does not host that thread, which fabricates
/// the exact interrupt this whole path exists to prevent, and turns stop into a silent no-op.
fn is_daemon_process(args: &[std::borrow::Cow<'_, str>]) -> bool {
    is_app_server(args)
}

/// PURE: does this argv look like an app-server, as the BACKSTOP behind the socket check?
///
/// Deliberately crude, and deliberately biased. Three rounds of review were spent trying to
/// locate the subcommand precisely in argv, and each rule traded one hypothetical for another
/// (a fixed index breaks `codex -c k=v app-server`; scanning for prompt-taking subcommands
/// breaks a profile named `exec`; a uniform option-arity parse breaks a boolean flag mixed
/// with a value-taking one). That is unwinnable, because argv does not say which options take
/// values and this crate does not own the CLI to find out.
///
/// So argv stopped being the deciding signal. `scan_codex_processes` identifies the daemon by
/// the LISTENING control socket it holds, which is a kernel fact and needs no CLI knowledge,
/// and this remains only for the case where /proc/net/unix cannot be read. As a backstop it
/// takes the safe bias: `app-server` anywhere means host, minus the `app-server daemon`
/// probes, which host nothing. A false positive here only routes stop through
/// `turn/interrupt`, which fails visibly; the false negative it protects against hands a
/// host's pid to SIGTERM and kills every session inside it.
fn is_app_server(args: &[std::borrow::Cow<'_, str>]) -> bool {
    // Skip argv[0]: a codex installed under a path containing "app-server" is not a daemon.
    let Some(at) = args.iter().skip(1).position(|arg| arg == "app-server") else {
        return false;
    };
    args.get(at + 2).is_none_or(|next| next != "daemon")
}

/// PURE: does this argv token have the shape of a codex thread id (a 36-char UUID)? Shape-based
/// on purpose - the sweep cannot know the registry's ids, and it is the cheap half of the
/// attached-client signal. A false positive costs one tick of Idle instead of Done on a row
/// whose id a codex process happened to name.
fn looks_like_thread_id(arg: &str) -> bool {
    let bytes = arg.as_bytes();
    bytes.len() == 36
        && bytes.iter().enumerate().all(|(i, byte)| match i {
            8 | 13 | 18 | 23 => *byte == b'-',
            _ => byte.is_ascii_hexdigit(),
        })
}

/// PURE combination of who holds the rollout open, whether a client is sitting in the thread,
/// and the last-turn tail (`None` = the file was unreadable/empty).
///
/// Two rule sets, because the open fd means different things in each.
///
/// A session's OWN codex process holds the fd only while it lives, so there the fd IS the
/// liveness signal (section 5.3):
///   open + tail AwaitingApproval          -> NeedsInput
///   open + tail MidTurn OR unreadable     -> Working   (spawn race: empty file)
///   open + tail Complete                  -> Idle      (live session between turns)
///   closed + Complete                     -> Done
///   closed + anything else                -> Error
///
/// The app-server DAEMON holds the rollout of every thread it hosts indefinitely (observed
/// still held 20+ minutes after the turn finished), so for those rows the fd carries no liveness
/// at all and an fd-driven rule would strand every finished background task at Idle forever -
/// telling working from finished is the whole point of the view. The tail becomes the signal,
/// with `attached` as the one refinement that keeps a session a human is sitting in from being
/// reported as finished:
///   MidTurn OR unreadable                 -> Working   (spawn race preserved)
///   AwaitingApproval                      -> NeedsInput
///   Complete + a client attached          -> Idle
///   Complete                              -> Done
///
/// When fd inspection is unavailable, only a complete tail proves `Done`. Every other tail is
/// `Unknown`; it must not be collapsed into the observed-closed `Error` rule or promoted to a
/// live status. Never panics.
fn status_from(fd: FdSignal, attached: bool, tail: Option<TailState>) -> Status {
    if matches!(fd, FdSignal::Held(RolloutOwner { daemon: true, .. })) {
        return match tail {
            Some(TailState::AwaitingApproval) => Status::NeedsInput {
                reason: Some("awaiting approval".into()),
            },
            Some(TailState::Complete) if attached => Status::Idle,
            Some(TailState::Complete) => Status::Done,
            _ => Status::Working,
        };
    }
    match (fd, tail) {
        (FdSignal::Held(_), Some(TailState::AwaitingApproval)) => Status::NeedsInput {
            reason: Some("awaiting approval".into()),
        },
        (FdSignal::Held(_), Some(TailState::Complete)) => Status::Idle,
        // MidTurn or an unreadable/empty file (spawn race) -> Working while held open.
        (FdSignal::Held(_), _) => Status::Working,
        (FdSignal::Closed, Some(TailState::Complete)) => Status::Done,
        // Closed and not cleanly complete (MidTurn, awaiting, or unreadable) -> Error.
        (FdSignal::Closed, _) => Status::Error,
        // A complete tail proves the turn finished even when process inspection is
        // unavailable. Every other tail remains unknown rather than fabricating liveness,
        // approval state, or an observed crash.
        (FdSignal::Unavailable, Some(TailState::Complete)) => Status::Done,
        (FdSignal::Unavailable, _) => Status::Unknown,
    }
}

/// PURE six-state resolution given explicit process evidence and whether a client is attached
/// to this thread. The attached signal is consulted only for a daemon-held rollout.
pub fn resolve_status(rollout_path: &Path, fd: FdSignal, attached: bool) -> Status {
    status_from(fd, attached, super::rollout::tail_state(rollout_path).ok())
}

/// Caching wrapper for the refresh loop. The cache holds the TAIL STATE (a pure function
/// of the file) keyed by (mtime, len); the final status is recomputed every tick from
/// (fd signal, cached tail) so a session that goes held -> closed with no file change still
/// re-resolves (Idle -> Done). Recompute the tail when the key changes or on first sight.
pub struct StatusResolver {
    cache: HashMap<PathBuf, ((SystemTime, u64), Option<TailState>)>,
    canonical: HashMap<PathBuf, PathBuf>,
}

impl StatusResolver {
    pub fn new() -> StatusResolver {
        StatusResolver {
            cache: HashMap::new(),
            canonical: HashMap::new(),
        }
    }

    /// Resolve the status, the owning pid (if any), and whether the fd holder is the shared
    /// app-server daemon. `attached` says whether a client is sitting in this thread and is
    /// only consulted for a daemon-held rollout.
    ///
    /// A daemon-held rollout returns `(status, None, true)`: the daemon's pid is deliberately
    /// withheld, because it is the pid of every OTHER thread the daemon hosts too and `stop`
    /// would SIGTERM all of them.
    pub fn resolve(
        &mut self,
        rollout_path: &Path,
        fd: FdSignal,
        attached: bool,
    ) -> (Status, Option<u32>, bool) {
        let owner = match fd {
            FdSignal::Held(owner) => Some(owner),
            FdSignal::Closed | FdSignal::Unavailable => None,
        };
        let daemon = owner.is_some_and(|owner| owner.daemon);
        let pid = owner.filter(|owner| !owner.daemon).map(|owner| owner.pid);
        // The (mtime, len) cache holds the TAIL STATE only — pure in the file. Reuse it
        // for every fd signal (the tail cannot change without the key changing); status is
        // always recomputed from process evidence, attached state, and the tail below.
        // This is the hot path this struct exists for (prior intent, commit 49a7c1b; 2,883
        // threads).
        let key = std::fs::metadata(rollout_path).ok().map(|meta| {
            (
                meta.modified().unwrap_or(SystemTime::UNIX_EPOCH),
                meta.len(),
            )
        });
        let tail = match (key, self.cache.get(rollout_path)) {
            (Some(key), Some((cached_key, cached_tail))) if *cached_key == key => *cached_tail,
            _ => {
                let tail = super::rollout::tail_state(rollout_path).ok();
                if let Some(key) = key {
                    self.cache.insert(rollout_path.to_path_buf(), (key, tail));
                }
                tail
            }
        };
        (status_from(fd, attached, tail), pid, daemon)
    }

    /// Resolve one row from a process scan while preserving the canonical path cache used by
    /// the Linux hot path. Portable scans yield `Unavailable` without touching procfs.
    pub fn resolve_scanned(
        &mut self,
        rollout_path: &Path,
        scan: &CodexProcessScan,
        attached: bool,
    ) -> (Status, Option<u32>, bool) {
        // Cache only successful canonicalizations. A registry row can appear before its
        // rollout file, so a failed canonicalization must be retried on a later tick.
        let canonical = match self.canonical.get(rollout_path) {
            Some(canonical) => canonical.clone(),
            None => match std::fs::canonicalize(rollout_path) {
                Ok(canonical) => {
                    self.canonical
                        .insert(rollout_path.to_path_buf(), canonical.clone());
                    canonical
                }
                Err(_) => rollout_path.to_path_buf(),
            },
        };
        self.resolve(rollout_path, scan.fd_signal(&canonical), attached)
    }
}

impl Default for StatusResolver {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::{
        RolloutOwner, is_daemon_process, listening_socket_inodes, looks_like_thread_id,
        record_owner, socket_inode,
    };
    use std::borrow::Cow;
    use std::collections::HashMap;
    use std::path::PathBuf;

    fn argv(args: &[&str]) -> Vec<Cow<'static, str>> {
        args.iter().map(|a| Cow::Owned(a.to_string())).collect()
    }

    /// The whole safety guard. If this predicate answers false for the daemon, `stop` hands the
    /// daemon's pid to SIGTERM and takes down every session it hosts, so it is pinned against
    /// the REAL captured argv of the running daemon and against every codex command line that
    /// must NOT match.
    #[test]
    fn the_argv_backstop_is_biased_toward_calling_a_host_a_host() {
        // Live capture from /proc/<pid>/cmdline of the running app-server on this box.
        assert!(is_daemon_process(&argv(&[
            "/home/user/.codex/packages/standalone/current/codex",
            "app-server",
            "--listen",
            "unix://",
        ])));
        // Every argv shape three rounds of review produced as a counterexample. The backstop
        // answers "host" to all of them ON PURPOSE: it cannot locate a subcommand reliably
        // (argv does not say which options take values), and the recoverable mistake is the
        // false positive, which only makes stop fail visibly. The false negative it is
        // guarding against SIGTERMs a process hosting other people's threads.
        for hosts in [
            argv(&["codex", "app-server"]),
            argv(&["codex", "-c", "model=gpt-5", "app-server", "--listen"]),
            argv(&["codex", "--profile", "exec", "app-server", "--listen"]),
            argv(&["codex", "--search", "-c", "k=v", "app-server", "--listen"]),
        ] {
            assert!(is_daemon_process(&hosts), "must read as a host: {hosts:?}");
        }

        let not_daemons = [
            argv(&["codex", "exec", "--json", "-C", "/tmp", "do a thing"]),
            // The one that broke this predicate originally: a plain exec session that spawned
            // a subagent thread held TWO rollout fds (pid 2910115, live on 2026-07-27), which
            // the old multi-rollout heuristic scored as the daemon.
            argv(&[
                "codex",
                "exec",
                "--json",
                "-C",
                "/home/user/git/agent-viewer",
                "--dangerously-bypass-approvals-and-sandbox",
                "clicking on things should do what enter does",
            ]),
            argv(&["codex", "resume", "019fa125-fa8e-7133-9aab-820742609be3"]),
            argv(&[
                "codex",
                "resume",
                "--remote",
                "unix:///home/user/.codex/app-server-control/app-server-control.sock",
                "019fa125-fa8e-7133-9aab-820742609be3",
            ]),
            // The short-lived availability probes, which carry the `daemon` subcommand and
            // host nothing. These are the reason the rule excludes `daemon`.
            argv(&["codex", "app-server", "daemon", "version"]),
            argv(&["codex", "app-server", "daemon", "start"]),
            // argv[0] alone is the binary path, never evidence of anything.
            argv(&["/opt/app-server/codex", "exec", "do a thing"]),
            // sysinfo returns an empty argv for a process it could not read.
            argv(&[]),
        ];
        for args in &not_daemons {
            assert!(
                !is_daemon_process(args),
                "must not read as the daemon: {args:?}"
            );
        }
    }

    /// THE decisive signal, and why the argv backstop above is allowed to be crude: an
    /// app-server is a SERVER, so it holds a listening socket. That is a kernel fact, so no
    /// command line a user invents can produce a false negative, and a process merely
    /// CONNECTED to a socket (any client, the interactive TUI and this viewer included) must
    /// not be mistaken for one.
    #[test]
    fn a_listening_socket_identifies_a_host_and_a_connected_one_does_not() {
        // Real /proc/net/unix shape: Num RefCount Protocol Flags Type St Inode Path.
        // Live shape captured from this box on 2026-07-27: the accepting socket carries
        // SO_ACCEPTCON in FLAGS and reports St=01 (SS_UNCONNECTED, NOT a listening state), the
        // client on the same path carries no flags and St=03.
        let table = "\
Num       RefCount Protocol Flags    Type St Inode Path
ffff0001: 00000002 00000000 00010000 0001 01 111111 /home/user/.codex/app-server-control/app-server-control.sock
ffff0002: 00000003 00000000 00000000 0001 03 222222 /home/user/.codex/app-server-control/app-server-control.sock
ffff0003: 00000002 00000000 00010000 0001 01 333333 /tmp/second-app-server.sock
ffff0004: 00000003 00000000 00000000 0001 03 444444
ffff0005: 00000002 00000000 00000000 0001 01 555555 /tmp/bound-but-never-listening.sock
";
        let found = listening_socket_inodes(table);

        assert!(found.contains(&111111), "the listening end IS a host");
        assert!(
            !found.contains(&222222),
            "a CONNECTED client on the same path is not a host"
        );
        // The finding that killed the control-dir filter: a second app-server on a custom
        // --listen endpoint hosts threads exactly as the managed daemon does, and filtering by
        // path would classify it as an ordinary session, exposing its pid to a SIGTERM that
        // kills every thread inside it.
        assert!(
            found.contains(&333333),
            "a host on a custom listen endpoint is still a host"
        );
        assert!(!found.contains(&444444), "an unbound client socket is not");
        // St alone would have matched this: 01 is SS_UNCONNECTED, which a socket that never
        // called listen() also reports. Only SO_ACCEPTCON in FLAGS means listening.
        assert!(
            !found.contains(&555555),
            "a bound socket that never called listen() is not a host"
        );
    }

    /// The two signals are a UNION and each covers the other's blind spot: an app-server on
    /// the `stdio://` transport holds no socket for the table to see, and argv cannot be
    /// parsed reliably for a host whose options this crate does not know. Either one saying
    /// "host" is enough, which is why the argv side is allowed to be crude and over-eager: its
    /// mistake costs a stop that fails visibly, and the mistake it prevents kills a process
    /// hosting other people's threads.
    #[test]
    fn the_argv_signal_still_answers_for_a_host_with_no_socket_to_see() {
        // The stdio-transport app-server, the case that killed the gated design.
        assert!(is_daemon_process(&argv(&["codex", "app-server"])));
    }

    #[test]
    fn only_socket_fds_yield_an_inode() {
        assert_eq!(socket_inode("socket:[111111]"), Some(111111));
        for other in [
            "/home/user/.codex/sessions/2026/07/27/rollout-x.jsonl",
            "socket:[]",
            "socket:[abc]",
            "anon_inode:[eventpoll]",
            "/dev/pts/3",
        ] {
            assert_eq!(socket_inode(other), None, "{other}");
        }
    }

    #[test]
    fn the_daemon_wins_the_entry_in_either_scan_order() {
        let rollout = PathBuf::from("/home/user/.codex/sessions/rollout-x.jsonl");
        let daemon = RolloutOwner {
            pid: 9999,
            daemon: true,
        };
        let client = RolloutOwner {
            pid: 4242,
            daemon: false,
        };
        for (first, second) in [(daemon, client), (client, daemon)] {
            let mut open: HashMap<PathBuf, RolloutOwner> = HashMap::new();
            record_owner(&mut open, rollout.clone(), first);
            record_owner(&mut open, rollout.clone(), second);
            assert_eq!(
                open.get(&rollout),
                Some(&daemon),
                "daemon must win: {first:?} then {second:?}"
            );
        }
        // A lone non-daemon holder is still recorded (that is the ordinary killable case).
        let mut open: HashMap<PathBuf, RolloutOwner> = HashMap::new();
        record_owner(&mut open, rollout.clone(), client);
        assert_eq!(open.get(&rollout), Some(&client));
    }

    #[test]
    fn looks_like_thread_id_matches_a_real_thread_id_and_nothing_else() {
        // A real id from this box's registry, which is what `codex resume --remote <sock> <id>`
        // puts in the joined TUI's argv.
        assert!(looks_like_thread_id("019fa125-fa8e-7133-9aab-820742609be3"));
        for arg in [
            "resume",
            "--remote",
            "unix:///home/user/.codex/app-server-control/app-server-control.sock",
            // Right length, wrong shape.
            "019fa125_fa8e_7133_9aab_820742609be3x",
            "019fa125-fa8e-7133-9aab-820742609bez",
            // A prefix and a suffix of a real id.
            "019fa125-fa8e-7133-9aab-820742609be",
            "019fa125-fa8e-7133-9aab-820742609be31",
            "",
        ] {
            assert!(!looks_like_thread_id(arg), "must not match: {arg:?}");
        }
    }
}
