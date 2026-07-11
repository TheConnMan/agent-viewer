use crate::error::Result;

/// Run `codex archive <id>`; capture output; non-zero exit -> Err(Error::Command(stderr)).
/// Never touch the DB directly.
pub fn archive(id: &str) -> Result<()> {
    let _ = id;
    todo!()
}

/// Run `codex unarchive <id>`; capture output; non-zero exit -> Err(Error::Command(stderr)).
pub fn unarchive(id: &str) -> Result<()> {
    let _ = id;
    todo!()
}

/// Build (do not run) `codex resume <id>` with inherited stdio.
pub fn resume_command(id: &str) -> std::process::Command {
    let _ = id;
    todo!()
}
