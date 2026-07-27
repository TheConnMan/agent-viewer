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

/// One sweep's worth of codex process facts: who holds which rollout open, and which threads
/// somebody is currently sitting in.
pub struct CodexProcessScan {
    pub open_rollouts: HashMap<PathBuf, RolloutOwner>,
    /// Thread ids appearing in a codex process's argv. A TUI that joined a daemon-hosted thread
    /// runs `codex resume --remote unix://<sock> <thread-id>`, so its argv names the thread.
    pub attached_threads: HashSet<String>,
}

/// IMPURE scanner (live-e2e-verified only, no unit tests): enumerate processes via
/// sysinfo whose name starts with "codex", read /proc/<pid>/fd/* via
/// std::fs::read_link, collect target paths -> owning codex process. Strip a trailing
/// " (deleted)" suffix. Unreadable /proc entries are skipped silently. The same pass collects
/// the attached thread ids from argv, because this runs every refresh tick over thousands of
/// threads and a second sweep (or a per-row daemon query) would land on the render path.
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
    let mut scan = CodexProcessScan {
        open_rollouts: HashMap::new(),
        attached_threads: HashSet::new(),
    };
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
        // Collect this process's fds first: whether it is the daemon depends on how many
        // DISTINCT rollouts it holds, which is only known once they are all read.
        let fd_dir = format!("/proc/{}/fd", pid.as_u32());
        let Ok(entries) = std::fs::read_dir(&fd_dir) else {
            continue;
        };
        let mut held: Vec<PathBuf> = Vec::new();
        for entry in entries.flatten() {
            let Ok(target) = std::fs::read_link(entry.path()) else {
                continue;
            };
            let display = target.to_string_lossy();
            let stripped = display.strip_suffix(" (deleted)").unwrap_or(&display);
            let path = PathBuf::from(stripped);
            if !held.contains(&path) {
                held.push(path);
            }
        }
        let rollouts = held.iter().filter(|path| looks_like_rollout(path)).count();
        let owner = RolloutOwner {
            pid: pid.as_u32(),
            daemon: is_daemon_process(&args, rollouts),
        };
        for path in held {
            record_owner(&mut scan.open_rollouts, path, owner);
        }
    }
    scan
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

/// PURE: is this codex process an app-server hosting other people's threads? Two independent
/// signals, either of which is enough, because this single predicate is what keeps `stop` from
/// SIGTERMing the daemon and every session inside it:
///   1. the argv shape `codex app-server --listen unix://...` (verified live from
///      /proc/<pid>/cmdline); both args are required so the short-lived
///      `codex app-server daemon version` probe is not mistaken for the daemon.
///   2. more than one distinct rollout held open at once. A per-session codex process has
///      exactly one rollout; only a host of several threads has more (the live daemon held 3).
///      This is the belt and braces for signal 1: a future codex release that drops `--listen`
///      from the daemon's argv would otherwise silently turn Ctrl+X into "kill every session".
fn is_daemon_process(args: &[std::borrow::Cow<'_, str>], rollouts_held: usize) -> bool {
    is_app_server(args) || rollouts_held > 1
}

/// PURE: the argv half of `is_daemon_process`.
fn is_app_server(args: &[std::borrow::Cow<'_, str>]) -> bool {
    args.iter().any(|arg| arg == "app-server") && args.iter().any(|arg| arg == "--listen")
}

/// PURE: does this open fd point at a codex rollout transcript
/// (`.../rollout-<ts>-<uuid>.jsonl`)? Only rollouts count toward the multi-thread signal
/// above; a process holding two log files is not a daemon.
fn looks_like_rollout(path: &Path) -> bool {
    path.file_name()
        .map(|name| name.to_string_lossy())
        .is_some_and(|name| name.starts_with("rollout-") && name.ends_with(".jsonl"))
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
/// `Unknown` is NOT produced here: the codex resolver always has an open/closed signal and a
/// tail to reason from, so it never needs the "backend cannot say" escape hatch. `Unknown` is
/// reserved for a resolver that genuinely has no signal to reason from. Never panics.
fn status_from(open: bool, daemon: bool, attached: bool, tail: Option<TailState>) -> Status {
    if daemon {
        return match tail {
            Some(TailState::AwaitingApproval) => Status::NeedsInput {
                reason: Some("awaiting approval".into()),
            },
            Some(TailState::Complete) if attached => Status::Idle,
            Some(TailState::Complete) => Status::Done,
            _ => Status::Working,
        };
    }
    match (open, tail) {
        (true, Some(TailState::AwaitingApproval)) => Status::NeedsInput {
            reason: Some("awaiting approval".into()),
        },
        (true, Some(TailState::Complete)) => Status::Idle,
        // MidTurn or an unreadable/empty file (spawn race) -> Working while held open.
        (true, _) => Status::Working,
        (false, Some(TailState::Complete)) => Status::Done,
        // Closed and not cleanly complete (MidTurn, awaiting, or unreadable) -> Error.
        (false, _) => Status::Error,
    }
}

/// PURE six-state resolution given the open map and whether a client is attached to this
/// thread (only consulted for a daemon-held rollout). Canonicalization exactly as v1.
/// Never panics.
pub fn resolve_status(
    rollout_path: &Path,
    open_paths: &HashMap<PathBuf, RolloutOwner>,
    attached: bool,
) -> Status {
    let canonical =
        std::fs::canonicalize(rollout_path).unwrap_or_else(|_| rollout_path.to_path_buf());
    let owner = open_paths.get(&canonical).copied();
    status_from(
        owner.is_some(),
        owner.is_some_and(|owner| owner.daemon),
        attached,
        super::rollout::tail_state(rollout_path).ok(),
    )
}

/// Caching wrapper for the refresh loop. The cache holds the TAIL STATE (a pure function
/// of the file) keyed by (mtime, len); the final status is recomputed every tick from
/// (open?, cached tail) so a session that goes open -> closed with no file change still
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
    /// app-server daemon. The owner is looked up in the same canonical-path cache the resolver
    /// already maintains, so callers do not need to canonicalize a second time. `attached` says
    /// whether a client is sitting in this thread and is only consulted for a daemon-held
    /// rollout (see `status_from`).
    ///
    /// A daemon-held rollout returns `(status, None, true)`: the daemon's pid is deliberately
    /// withheld, because it is the pid of every OTHER thread the daemon hosts too and `stop`
    /// would SIGTERM all of them.
    pub fn resolve(
        &mut self,
        rollout_path: &Path,
        open_paths: &HashMap<PathBuf, RolloutOwner>,
        attached: bool,
    ) -> (Status, Option<u32>, bool) {
        // canonicalize once per distinct rollout_path, then reuse across ticks. Cache ONLY
        // successful canonicalizations: an Err (path not present yet) uses the raw path for
        // this tick without caching, so a later tick retries once the file appears.
        let canonical = match self.canonical.get(rollout_path) {
            Some(c) => c.clone(),
            None => match std::fs::canonicalize(rollout_path) {
                Ok(c) => {
                    self.canonical.insert(rollout_path.to_path_buf(), c.clone());
                    c
                }
                Err(_) => rollout_path.to_path_buf(),
            },
        };
        let owner = open_paths.get(&canonical).copied();
        let open = owner.is_some();
        let daemon = owner.is_some_and(|owner| owner.daemon);
        let pid = owner.filter(|owner| !owner.daemon).map(|owner| owner.pid);
        // The (mtime, len) cache holds the TAIL STATE only — pure in the file. Reuse it
        // whether or not the session is open (the tail cannot change without the key
        // changing); status is always recomputed from (open?, daemon?, attached?, tail) below.
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
        (status_from(open, daemon, attached, tail), pid, daemon)
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
        RolloutOwner, is_daemon_process, looks_like_rollout, looks_like_thread_id, record_owner,
    };
    use std::borrow::Cow;
    use std::collections::HashMap;
    use std::path::{Path, PathBuf};

    fn argv(args: &[&str]) -> Vec<Cow<'static, str>> {
        args.iter().map(|a| Cow::Owned(a.to_string())).collect()
    }

    /// The whole safety guard. If this predicate answers false for the daemon, `stop` hands the
    /// daemon's pid to SIGTERM and takes down every session it hosts, so it is pinned against
    /// the REAL captured argv of the running daemon and against every codex command line that
    /// must NOT match.
    #[test]
    fn the_daemon_is_recognized_by_its_argv_and_nothing_else_is() {
        // Live capture from /proc/<pid>/cmdline of the running app-server on this box.
        let daemon = argv(&[
            "/home/user/.codex/packages/standalone/current/codex",
            "app-server",
            "--listen",
            "unix://",
        ]);
        assert!(is_daemon_process(&daemon, 1));
        assert!(is_daemon_process(&daemon, 0), "argv alone is enough");

        let not_daemons = [
            argv(&["codex", "exec", "--json", "-C", "/tmp", "do a thing"]),
            argv(&["codex", "resume", "019fa125-fa8e-7133-9aab-820742609be3"]),
            argv(&[
                "codex",
                "resume",
                "--remote",
                "unix:///home/user/.codex/app-server-control/app-server-control.sock",
                "019fa125-fa8e-7133-9aab-820742609be3",
            ]),
            // The short-lived availability probe: `app-server` without `--listen`.
            argv(&["codex", "app-server", "daemon", "version"]),
            argv(&["codex", "app-server", "daemon", "start"]),
            // sysinfo returns an empty argv for a process it could not read.
            argv(&[]),
        ];
        for args in &not_daemons {
            assert!(
                !is_daemon_process(args, 1),
                "must not read as the daemon: {args:?}"
            );
        }

        // Belt and braces: whatever the argv says, a process holding more than one rollout is
        // hosting other people's threads (the live daemon held 3).
        for args in &not_daemons {
            assert!(
                is_daemon_process(args, 2),
                "two rollouts at once is a host: {args:?}"
            );
        }
    }

    #[test]
    fn only_rollout_transcripts_count_toward_the_multi_thread_signal() {
        assert!(looks_like_rollout(Path::new(
            "/home/user/.codex/sessions/2026/07/27/rollout-2026-07-27T01-41-01-019fa13b-87fd-7b21-88c7-0a53d9735a14.jsonl"
        )));
        assert!(looks_like_rollout(Path::new(
            "/home/user/.codex/archived_sessions/rollout-2026-07-27T01-17-28-019fa125-fa8e-7133-9aab-820742609be3.jsonl"
        )));
        for other in [
            "/home/user/.codex/bg-logs/1785113777.log",
            "/home/user/.codex/sessions/2026/07/27/rollout-2026-07-27.txt",
            "/dev/pts/3",
            "/home/user/.codex/history.jsonl",
        ] {
            assert!(!looks_like_rollout(Path::new(other)), "{other}");
        }
    }

    /// Two codex processes can hold one rollout at once, and the sweep visits them in HashMap
    /// order. Whichever order they arrive in, the daemon must own the entry: the losing case
    /// would report a signalable pid for a daemon-hosted row and SIGTERM a codex TUI's process
    /// group on Ctrl+X.
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
