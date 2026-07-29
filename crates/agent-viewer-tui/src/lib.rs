pub mod app;
pub mod attach;
pub mod composer;
pub mod logos;
pub mod model_cache;
pub mod mouse;
pub mod mutations;
pub mod pr_cache;
pub mod shared_listing;
pub mod terminal_title;
pub mod ui;

/// Work selected from command line arguments before interactive startup begins.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StartupAction {
    Run,
    PrintVersion,
}

/// Select startup behavior without probing the terminal, filesystem, or backends.
pub fn startup_action<I, S>(args: I) -> StartupAction
where
    I: IntoIterator<Item = S>,
    S: AsRef<std::ffi::OsStr>,
{
    if args.into_iter().any(|arg| {
        let arg = arg.as_ref();
        arg == std::ffi::OsStr::new("--version") || arg == std::ffi::OsStr::new("-V")
    }) {
        StartupAction::PrintVersion
    } else {
        StartupAction::Run
    }
}
