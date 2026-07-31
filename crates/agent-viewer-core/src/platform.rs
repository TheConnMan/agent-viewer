use std::ffi::OsStr;
use std::path::PathBuf;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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
    platform: Platform,
    home: Option<&OsStr>,
    user_profile: Option<&OsStr>,
    home_drive: Option<&OsStr>,
    home_path: Option<&OsStr>,
) -> PathBuf {
    let home = home.filter(|value| !value.is_empty());
    let user_profile = user_profile.filter(|value| !value.is_empty());
    let native_windows_home = match (
        home_drive.filter(|value| !value.is_empty()),
        home_path.filter(|value| !value.is_empty()),
    ) {
        (Some(drive), Some(path)) => {
            let mut combined = drive.to_os_string();
            combined.push(path);
            Some(PathBuf::from(combined))
        }
        _ => None,
    };

    match platform {
        Platform::Windows => user_profile
            .map(PathBuf::from)
            .or(native_windows_home)
            .or_else(|| home.map(PathBuf::from))
            .unwrap_or_default(),
        Platform::Linux | Platform::Macos => home
            .map(PathBuf::from)
            .or_else(|| user_profile.map(PathBuf::from))
            .or(native_windows_home)
            .unwrap_or_default(),
    }
}
