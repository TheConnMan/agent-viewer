use std::io::{Read, Seek, SeekFrom};
use std::process::{Command, ExitStatus, Output, Stdio};
use std::thread;
use std::time::{Duration, Instant};

const PROCESS_TIMEOUT: Duration = Duration::from_secs(5);

fn run_bounded(command: &mut Command) -> Output {
    let mut stdout = tempfile::tempfile().expect("create stdout capture");
    let mut stderr = tempfile::tempfile().expect("create stderr capture");
    command
        .stdin(Stdio::null())
        .stdout(Stdio::from(
            stdout.try_clone().expect("clone stdout capture"),
        ))
        .stderr(Stdio::from(
            stderr.try_clone().expect("clone stderr capture"),
        ));

    let mut child = command.spawn().expect("spawn agent-viewer");
    let deadline = Instant::now() + PROCESS_TIMEOUT;
    let status = loop {
        if let Some(status) = child.try_wait().expect("poll agent-viewer") {
            break status;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let cleanup_deadline = Instant::now() + Duration::from_secs(1);
            while Instant::now() < cleanup_deadline {
                match child.try_wait() {
                    Ok(Some(_)) | Err(_) => break,
                    Ok(None) => thread::sleep(Duration::from_millis(10)),
                }
            }
            panic!("agent-viewer did not exit within {PROCESS_TIMEOUT:?}");
        }
        thread::sleep(Duration::from_millis(10));
    };

    Output {
        status,
        stdout: read_capture(&mut stdout),
        stderr: read_capture(&mut stderr),
    }
}

fn read_capture(capture: &mut std::fs::File) -> Vec<u8> {
    capture
        .seek(SeekFrom::Start(0))
        .expect("rewind output capture");
    let mut bytes = Vec::new();
    capture
        .read_to_end(&mut bytes)
        .expect("read output capture");
    bytes
}

fn assert_version_flag(flag: &str) {
    let home = tempfile::tempdir().expect("create isolated home");
    let output = run_bounded(
        Command::new(env!("CARGO_BIN_EXE_agent-viewer"))
            .arg(flag)
            .env("HOME", home.path())
            .env("USERPROFILE", home.path()),
    );

    assert_success(output.status, flag);
    assert_eq!(
        output.stdout,
        format!("agent-viewer {}\n", env!("CARGO_PKG_VERSION")).as_bytes(),
        "{flag} must print only the package version"
    );
    assert_eq!(
        output.stderr, b"",
        "{flag} must not emit startup diagnostics"
    );
    assert!(
        home.path()
            .read_dir()
            .expect("read isolated home")
            .next()
            .is_none(),
        "{flag} must exit before backend or viewer state initialization"
    );
}

fn assert_success(status: ExitStatus, flag: &str) {
    assert!(status.success(), "{flag} exited with {status}");
}

#[test]
fn long_version_flag_exits_before_terminal_and_backend_startup() {
    assert_version_flag("--version");
}

#[test]
fn short_version_flag_exits_before_terminal_and_backend_startup() {
    assert_version_flag("-V");
}
