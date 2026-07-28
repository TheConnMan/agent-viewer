use agent_viewer_core::platform::home_from;
use std::ffi::OsStr;
use std::path::PathBuf;

#[test]
fn home_fallback_order_is_home_then_userprofile_then_drive_and_path() {
    assert_eq!(
        home_from(
            Some(OsStr::new("/home/Brian Work")),
            Some(OsStr::new(r"C:\Users\ignored")),
            Some(OsStr::new("D:")),
            Some(OsStr::new(r"\ignored")),
        ),
        PathBuf::from("/home/Brian Work")
    );
    assert_eq!(
        home_from(
            Some(OsStr::new("")),
            Some(OsStr::new(r"C:\Users\Brían Work")),
            Some(OsStr::new("D:")),
            Some(OsStr::new(r"\ignored")),
        ),
        PathBuf::from(r"C:\Users\Brían Work")
    );
    assert_eq!(
        home_from(
            None,
            Some(OsStr::new("")),
            Some(OsStr::new("D:")),
            Some(OsStr::new(r"\Users\Brian Work")),
        ),
        PathBuf::from(r"D:\Users\Brian Work")
    );
    assert_eq!(
        home_from(None, None, Some(OsStr::new("D:")), None),
        PathBuf::new()
    );
}
