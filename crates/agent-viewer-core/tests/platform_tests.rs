use agent_viewer_core::platform::{Platform, home_from};
use std::ffi::OsStr;
use std::path::PathBuf;

#[test]
fn linux_home_precedes_windows_fallbacks() {
    assert_eq!(
        home_from(
            Platform::Linux,
            Some(OsStr::new("/home/Brian Work")),
            Some(OsStr::new(r"C:\Users\ignored")),
            Some(OsStr::new("D:")),
            Some(OsStr::new(r"\ignored")),
        ),
        PathBuf::from("/home/Brian Work")
    );
}

#[test]
fn windows_userprofile_precedes_posix_home() {
    assert_eq!(
        home_from(
            Platform::Windows,
            Some(OsStr::new("/home/brian")),
            Some(OsStr::new(r"C:\Users\Brían Work")),
            Some(OsStr::new("D:")),
            Some(OsStr::new(r"\ignored")),
        ),
        PathBuf::from(r"C:\Users\Brían Work")
    );
}

#[test]
fn windows_home_drive_and_path_are_last_fallback() {
    assert_eq!(
        home_from(
            Platform::Windows,
            None,
            Some(OsStr::new("")),
            Some(OsStr::new("D:")),
            Some(OsStr::new(r"\Users\Brian Work")),
        ),
        PathBuf::from(r"D:\Users\Brian Work")
    );
    assert_eq!(
        home_from(Platform::Windows, None, None, Some(OsStr::new("D:")), None,),
        PathBuf::new()
    );
}
