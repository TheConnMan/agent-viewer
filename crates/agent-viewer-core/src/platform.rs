use std::ffi::OsStr;
use std::path::PathBuf;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Platform {
    Linux,
    Macos,
    Windows,
}

pub const fn current_platform() -> Platform {
    #[cfg(target_os = "linux")]
    {
        Platform::Linux
    }
    #[cfg(target_os = "macos")]
    {
        Platform::Macos
    }
    #[cfg(target_os = "windows")]
    {
        Platform::Windows
    }
}

pub fn home_from(
    home: Option<&OsStr>,
    user_profile: Option<&OsStr>,
    home_drive: Option<&OsStr>,
    home_path: Option<&OsStr>,
) -> PathBuf {
    if let Some(home) = home.filter(|value| !value.is_empty()) {
        return PathBuf::from(home);
    }
    if let Some(user_profile) = user_profile.filter(|value| !value.is_empty()) {
        return PathBuf::from(user_profile);
    }
    match (
        home_drive.filter(|value| !value.is_empty()),
        home_path.filter(|value| !value.is_empty()),
    ) {
        (Some(drive), Some(path)) => {
            let mut combined = drive.to_os_string();
            combined.push(path);
            PathBuf::from(combined)
        }
        _ => PathBuf::new(),
    }
}
