mod common;

use agent_viewer_core::Status;
use agent_viewer_core::backend::{Backend, BackendKind, Capabilities, Session, SessionOrigin};
use agent_viewer_core::opencode::{
    OpencodeBackend, OpencodeRuntime, OpencodeRuntimeTestConfig, is_run_mode_permission,
    opencode_status, parse_opencode_models, read_opencode_last_message,
};
use base64::Engine;
use serde_json::{Value, json};
use std::collections::{BTreeMap, VecDeque};
use std::io::{self, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

#[derive(Clone, Debug)]
struct RecordedRequest {
    connection_id: usize,
    method: String,
    target: String,
    headers: BTreeMap<String, String>,
    body: Vec<u8>,
}

impl RecordedRequest {
    fn json_body(&self) -> Value {
        serde_json::from_slice(&self.body).expect("request body is JSON")
    }
}

#[derive(Clone)]
struct ScriptedResponse {
    status: u16,
    content_type: &'static str,
    body: String,
    delay: Duration,
    headers: Vec<(String, String)>,
    framing: ResponseFraming,
    close_connection: bool,
}

#[derive(Clone, Copy)]
enum ResponseFraming {
    ContentLength,
    Chunked,
    None,
}

impl ScriptedResponse {
    fn json(status: u16, body: Value) -> Self {
        Self {
            status,
            content_type: "application/json",
            body: body.to_string(),
            delay: Duration::ZERO,
            headers: Vec::new(),
            framing: ResponseFraming::ContentLength,
            close_connection: true,
        }
    }

    fn raw(status: u16, content_type: &'static str, body: impl Into<String>) -> Self {
        Self {
            status,
            content_type,
            body: body.into(),
            delay: Duration::ZERO,
            headers: Vec::new(),
            framing: ResponseFraming::ContentLength,
            close_connection: true,
        }
    }

    fn empty(status: u16) -> Self {
        Self::raw(status, "application/json", "")
    }

    fn header(mut self, name: &str, value: &str) -> Self {
        self.headers.push((name.to_string(), value.to_string()));
        self
    }

    fn chunked(mut self) -> Self {
        self.framing = ResponseFraming::Chunked;
        self
    }

    fn without_framing(mut self) -> Self {
        self.framing = ResponseFraming::None;
        self
    }

    fn keep_alive(mut self) -> Self {
        self.close_connection = false;
        self
    }
}

struct ScriptedServer {
    addr: SocketAddr,
    requests: Arc<Mutex<Vec<RecordedRequest>>>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
    process: Option<Arc<ProcFixture>>,
}

impl ScriptedServer {
    fn spawn(responses: Vec<ScriptedResponse>) -> Self {
        Self::spawn_on("127.0.0.1:0".parse().unwrap(), responses).expect("bind scripted server")
    }

    fn spawn_on(addr: SocketAddr, responses: Vec<ScriptedResponse>) -> io::Result<Self> {
        Self::spawn_on_with_process(addr, responses, None)
    }

    fn spawn_verified(responses: Vec<ScriptedResponse>) -> Self {
        Self::spawn_on_with_process(
            "127.0.0.1:0".parse().unwrap(),
            responses,
            Some((new_proc_root(), 51001)),
        )
        .expect("bind verified scripted server")
    }

    fn spawn_verified_deferred(responses: Vec<ScriptedResponse>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind deferred scripted server");
        enable_deferred_accept(&listener).expect("enable deferred accept");
        listener.set_nonblocking(true).unwrap();
        let addr = listener.local_addr().unwrap();
        let process = Some(Arc::new(ProcFixture::new_in(
            new_proc_root(),
            51002,
            &listener,
            addr,
        )));
        let process_for_thread = process.clone();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let requests_for_thread = Arc::clone(&requests);
        let stop = Arc::new(AtomicBool::new(false));
        let stop_for_thread = Arc::clone(&stop);
        let mut responses: VecDeque<_> = responses.into();
        let thread = std::thread::spawn(move || {
            let mut connection_id = 0;
            while !stop_for_thread.load(Ordering::SeqCst) {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        connection_id += 1;
                        if let Some(process) = &process_for_thread {
                            process.record_connection(&stream);
                        }
                        while let Ok(request) = read_http_request(&mut stream, connection_id) {
                            requests_for_thread.lock().unwrap().push(request);
                            let response = responses.pop_front().unwrap_or_else(|| {
                                ScriptedResponse::raw(500, "text/plain", "unexpected request")
                            });
                            if !response.delay.is_zero() {
                                std::thread::sleep(response.delay);
                            }
                            let close = response.close_connection;
                            let _ = write_http_response(&mut stream, &response);
                            if close {
                                break;
                            }
                        }
                    }
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(2));
                    }
                    Err(_) => break,
                }
            }
        });
        Self {
            addr,
            requests,
            stop,
            thread: Some(thread),
            process,
        }
    }

    fn spawn_on_verified_in(
        addr: SocketAddr,
        responses: Vec<ScriptedResponse>,
        proc_root: Arc<tempfile::TempDir>,
        pid: u32,
    ) -> io::Result<Self> {
        Self::spawn_on_with_process(addr, responses, Some((proc_root, pid)))
    }

    fn spawn_on_verified_routed_in(
        addr: SocketAddr,
        proc_root: Arc<tempfile::TempDir>,
        pid: u32,
        route: impl Fn(&RecordedRequest) -> ScriptedResponse + Send + Sync + 'static,
    ) -> io::Result<Self> {
        let listener = TcpListener::bind(addr)?;
        listener.set_nonblocking(true)?;
        let addr = listener.local_addr()?;
        let process = Some(Arc::new(ProcFixture::new_in(
            proc_root, pid, &listener, addr,
        )));
        let process_for_thread = process.clone();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let requests_for_thread = Arc::clone(&requests);
        let stop = Arc::new(AtomicBool::new(false));
        let stop_for_thread = Arc::clone(&stop);
        let thread = std::thread::spawn(move || {
            let mut connection_id = 0;
            while !stop_for_thread.load(Ordering::SeqCst) {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        connection_id += 1;
                        if let Some(process) = &process_for_thread {
                            process.record_connection(&stream);
                        }
                        while let Ok(request) = read_http_request(&mut stream, connection_id) {
                            let response = route(&request);
                            requests_for_thread.lock().unwrap().push(request);
                            if !response.delay.is_zero() {
                                std::thread::sleep(response.delay);
                            }
                            let close = response.close_connection;
                            let _ = write_http_response(&mut stream, &response);
                            if close {
                                break;
                            }
                        }
                    }
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(2));
                    }
                    Err(_) => break,
                }
            }
        });
        Ok(Self {
            addr,
            requests,
            stop,
            thread: Some(thread),
            process,
        })
    }

    fn spawn_verified_concurrent_routed(
        route: impl Fn(&RecordedRequest) -> ScriptedResponse + Send + Sync + 'static,
    ) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind concurrent scripted server");
        listener.set_nonblocking(true).unwrap();
        let addr = listener.local_addr().unwrap();
        let process = Some(Arc::new(ProcFixture::new_in(
            new_proc_root(),
            51003,
            &listener,
            addr,
        )));
        let process_for_thread = process.clone();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let requests_for_thread = Arc::clone(&requests);
        let stop = Arc::new(AtomicBool::new(false));
        let stop_for_thread = Arc::clone(&stop);
        let route = Arc::new(route);
        let thread = std::thread::spawn(move || {
            let mut connection_id = 0;
            while !stop_for_thread.load(Ordering::SeqCst) {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        connection_id += 1;
                        if let Some(process) = &process_for_thread {
                            process.record_connection_at(&stream, connection_id);
                        }
                        let requests = Arc::clone(&requests_for_thread);
                        let route = Arc::clone(&route);
                        let current_connection = connection_id;
                        std::thread::spawn(move || {
                            while let Ok(request) =
                                read_http_request(&mut stream, current_connection)
                            {
                                let response = route(&request);
                                requests.lock().unwrap().push(request);
                                if !response.delay.is_zero() {
                                    std::thread::sleep(response.delay);
                                }
                                let close = response.close_connection;
                                let _ = write_http_response(&mut stream, &response);
                                if close {
                                    break;
                                }
                            }
                        });
                    }
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(2));
                    }
                    Err(_) => break,
                }
            }
        });
        Self {
            addr,
            requests,
            stop,
            thread: Some(thread),
            process,
        }
    }

    fn spawn_on_with_process(
        addr: SocketAddr,
        responses: Vec<ScriptedResponse>,
        process: Option<(Arc<tempfile::TempDir>, u32)>,
    ) -> io::Result<Self> {
        let listener = TcpListener::bind(addr)?;
        listener.set_nonblocking(true)?;
        let addr = listener.local_addr()?;
        let process =
            process.map(|(root, pid)| Arc::new(ProcFixture::new_in(root, pid, &listener, addr)));
        let process_for_thread = process.clone();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let requests_for_thread = Arc::clone(&requests);
        let stop = Arc::new(AtomicBool::new(false));
        let stop_for_thread = Arc::clone(&stop);
        let mut responses: VecDeque<_> = responses.into();
        let thread = std::thread::spawn(move || {
            let mut connection_id = 0;
            while !stop_for_thread.load(Ordering::SeqCst) {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        connection_id += 1;
                        if let Some(process) = &process_for_thread {
                            process.record_connection(&stream);
                        }
                        while let Ok(request) = read_http_request(&mut stream, connection_id) {
                            requests_for_thread.lock().unwrap().push(request);
                            let response = responses.pop_front().unwrap_or_else(|| {
                                ScriptedResponse::raw(500, "text/plain", "unexpected request")
                            });
                            if !response.delay.is_zero() {
                                std::thread::sleep(response.delay);
                            }
                            let close = response.close_connection;
                            let _ = write_http_response(&mut stream, &response);
                            if close {
                                break;
                            }
                        }
                    }
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(2));
                    }
                    Err(_) => break,
                }
            }
        });
        Ok(Self {
            addr,
            requests,
            stop,
            thread: Some(thread),
            process,
        })
    }

    fn proc_root(&self) -> PathBuf {
        self.process
            .as_ref()
            .expect("verified server process fixture")
            .root()
    }

    fn process(&self) -> Arc<ProcFixture> {
        Arc::clone(
            self.process
                .as_ref()
                .expect("verified server process fixture"),
        )
    }

    fn requests(&self) -> Vec<RecordedRequest> {
        self.requests.lock().unwrap().clone()
    }

    fn wait_for_requests(&self, count: usize) -> Vec<RecordedRequest> {
        let start = Instant::now();
        while start.elapsed() < Duration::from_secs(2) {
            let requests = self.requests();
            if requests.len() >= count {
                return requests;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        self.requests()
    }
}

struct ProcFixture {
    root: Arc<tempfile::TempDir>,
    pid: u32,
    addr: SocketAddr,
    listener_inode: u64,
}

impl ProcFixture {
    fn new_in(
        root: Arc<tempfile::TempDir>,
        pid: u32,
        listener: &TcpListener,
        addr: SocketAddr,
    ) -> Self {
        let fixture = Self {
            root,
            pid,
            addr,
            listener_inode: socket_inode(listener.as_raw_fd()),
        };
        fixture.write_identity(1000, unsafe { libc::geteuid() }, None);
        fixture
    }

    fn root(&self) -> PathBuf {
        self.root.path().to_path_buf()
    }

    fn write_identity(&self, start_time: u64, uid: u32, argv: Option<Vec<String>>) {
        let process = self.root.path().join(self.pid.to_string());
        let fd = process.join("fd");
        std::fs::create_dir_all(&fd).unwrap();
        let _ = std::fs::remove_file(fd.join("100"));
        std::os::unix::fs::symlink(format!("socket:[{}]", self.listener_inode), fd.join("100"))
            .unwrap();
        std::fs::write(
            process.join("stat"),
            format!(
                "{} (opencode) S {} {start_time}\n",
                self.pid,
                vec!["0"; 18].join(" ")
            ),
        )
        .unwrap();
        std::fs::write(
            process.join("status"),
            format!("Name:\topencode\nUid:\t{uid}\t{uid}\t{uid}\t{uid}\n"),
        )
        .unwrap();
        let argv = argv.unwrap_or_else(|| {
            vec![
                "opencode".to_string(),
                "serve".to_string(),
                "--hostname".to_string(),
                "127.0.0.1".to_string(),
                "--port".to_string(),
                self.addr.port().to_string(),
            ]
        });
        let mut cmdline = argv.join("\0").into_bytes();
        cmdline.push(0);
        std::fs::write(process.join("cmdline"), cmdline).unwrap();
    }

    fn record_connection(&self, stream: &TcpStream) {
        self.record_connection_at(stream, 0);
    }

    fn record_connection_at(&self, stream: &TcpStream, slot: usize) {
        let fd = self
            .root
            .path()
            .join(self.pid.to_string())
            .join("fd")
            .join((101 + slot).to_string());
        let _ = std::fs::remove_file(&fd);
        std::os::unix::fs::symlink(format!("socket:[{}]", socket_inode(stream.as_raw_fd())), fd)
            .unwrap();
    }

    fn remove_process(&self) {
        std::fs::remove_dir_all(self.root.path().join(self.pid.to_string())).unwrap();
    }

    fn handoff_listener_inode(&self) {
        let fd = self
            .root
            .path()
            .join(self.pid.to_string())
            .join("fd")
            .join("100");
        std::fs::remove_file(&fd).unwrap();
        std::os::unix::fs::symlink("socket:[1]", fd).unwrap();
    }
}

fn new_proc_root() -> Arc<tempfile::TempDir> {
    let root = Arc::new(tempfile::TempDir::new().expect("temporary proc root"));
    let net = root.path().join("net");
    std::fs::create_dir_all(&net).unwrap();
    std::os::unix::fs::symlink("/proc/net/tcp", net.join("tcp")).unwrap();
    root
}

fn socket_inode(fd: i32) -> u64 {
    std::fs::metadata(format!("/proc/self/fd/{fd}"))
        .expect("socket metadata")
        .ino()
}

fn enable_deferred_accept(listener: &TcpListener) -> io::Result<()> {
    let seconds: libc::c_int = 1;
    let result = unsafe {
        libc::setsockopt(
            listener.as_raw_fd(),
            libc::IPPROTO_TCP,
            libc::TCP_DEFER_ACCEPT,
            (&seconds as *const libc::c_int).cast(),
            std::mem::size_of_val(&seconds) as libc::socklen_t,
        )
    };
    if result == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

impl Drop for ScriptedServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Ok(mut stream) = TcpStream::connect(self.addr) {
            let _ = stream.write_all(b"GET /shutdown HTTP/1.1\r\nHost: localhost\r\n\r\n");
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn read_http_request(stream: &mut TcpStream, connection_id: usize) -> io::Result<RecordedRequest> {
    stream.set_read_timeout(Some(Duration::from_secs(1)))?;
    let mut bytes = Vec::new();
    let mut chunk = [0_u8; 1024];
    let header_end = loop {
        let read = stream.read(&mut chunk)?;
        if read == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "request ended before headers",
            ));
        }
        bytes.extend_from_slice(&chunk[..read]);
        if let Some(position) = bytes.windows(4).position(|window| window == b"\r\n\r\n") {
            break position + 4;
        }
    };
    let header_text = String::from_utf8_lossy(&bytes[..header_end]);
    let mut lines = header_text.split("\r\n");
    let request_line = lines.next().unwrap_or_default();
    let mut request_parts = request_line.split_whitespace();
    let method = request_parts.next().unwrap_or_default().to_string();
    let target = request_parts.next().unwrap_or_default().to_string();
    let mut headers = BTreeMap::new();
    for line in lines.filter(|line| !line.is_empty()) {
        if let Some((name, value)) = line.split_once(':') {
            headers.insert(name.trim().to_ascii_lowercase(), value.trim().to_string());
        }
    }
    let content_length = headers
        .get("content-length")
        .and_then(|length| length.parse::<usize>().ok())
        .unwrap_or(0);
    while bytes.len() < header_end + content_length {
        let read = stream.read(&mut chunk)?;
        if read == 0 {
            break;
        }
        bytes.extend_from_slice(&chunk[..read]);
    }
    Ok(RecordedRequest {
        connection_id,
        method,
        target,
        headers,
        body: bytes[header_end..bytes.len().min(header_end + content_length)].to_vec(),
    })
}

fn write_http_response(stream: &mut TcpStream, response: &ScriptedResponse) -> io::Result<()> {
    let reason = match response.status {
        200 => "OK",
        204 => "No Content",
        400 => "Bad Request",
        401 => "Unauthorized",
        404 => "Not Found",
        409 => "Conflict",
        500 => "Internal Server Error",
        _ => "Response",
    };
    write!(
        stream,
        "HTTP/1.1 {} {}\r\nContent-Type: {}\r\n",
        response.status, reason, response.content_type
    )?;
    for (name, value) in &response.headers {
        write!(stream, "{name}: {value}\r\n")?;
    }
    match response.framing {
        ResponseFraming::ContentLength => {
            write!(stream, "Content-Length: {}\r\n", response.body.len())?;
        }
        ResponseFraming::Chunked => {
            write!(stream, "Transfer-Encoding: chunked\r\n")?;
        }
        ResponseFraming::None => {}
    }
    let connection = if response.close_connection {
        "close"
    } else {
        "keep-alive"
    };
    write!(stream, "Connection: {connection}\r\n\r\n")?;
    match response.framing {
        ResponseFraming::Chunked => {
            write!(
                stream,
                "{:X}\r\n{}\r\n0\r\n\r\n",
                response.body.len(),
                response.body
            )?;
        }
        _ => write!(stream, "{}", response.body)?,
    }
    stream.flush()
}

fn healthy_response() -> ScriptedResponse {
    ScriptedResponse::json(200, json!({"healthy": true, "version": "1.17.20"}))
}

fn unused_addr() -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").expect("reserve free address");
    let addr = listener.local_addr().unwrap();
    drop(listener);
    addr
}

fn unused_addrs() -> [SocketAddr; 2] {
    let primary = TcpListener::bind("127.0.0.1:0").expect("reserve primary address");
    let backup = TcpListener::bind("127.0.0.1:0").expect("reserve backup address");
    let addresses = [primary.local_addr().unwrap(), backup.local_addr().unwrap()];
    drop(primary);
    drop(backup);
    addresses
}

fn secure_runtime_for(
    server: &ScriptedServer,
    candidates: [SocketAddr; 2],
    timeout: Duration,
    launcher: impl Fn(Command) -> io::Result<u32> + Send + Sync + 'static,
) -> OpencodeRuntime {
    OpencodeRuntime::for_test_secure(OpencodeRuntimeTestConfig {
        candidates,
        startup_timeout: timeout,
        durable_cwd: PathBuf::from("/"),
        launcher: Arc::new(launcher),
        viewer_db_path: server.proc_root().join("viewer.db"),
        proc_root: server.proc_root(),
        password_override: Some("test-secret".to_string()),
        before_authorized_write: None,
    })
}

fn secure_runtime_with_hook(
    server: &ScriptedServer,
    hook: Arc<dyn Fn() + Send + Sync>,
) -> OpencodeRuntime {
    OpencodeRuntime::for_test_secure(OpencodeRuntimeTestConfig {
        candidates: [server.addr, unused_addr()],
        startup_timeout: Duration::from_millis(250),
        durable_cwd: PathBuf::from("/"),
        launcher: Arc::new(|_| Err(io::Error::other("launcher was not expected"))),
        viewer_db_path: server.proc_root().join("viewer.db"),
        proc_root: server.proc_root(),
        password_override: Some("test-secret".to_string()),
        before_authorized_write: Some(hook),
    })
}

fn unauthorized_health() -> ScriptedResponse {
    ScriptedResponse::json(401, json!({"error": "Unauthorized"}))
}

fn secure_responses(actual: Vec<ScriptedResponse>) -> Vec<ScriptedResponse> {
    actual
        .into_iter()
        .flat_map(|response| [unauthorized_health().keep_alive(), response])
        .collect()
}

fn assert_test_secret_pairs(requests: &[RecordedRequest]) {
    assert_eq!(requests.len() % 2, 0);
    let expected = format!(
        "Basic {}",
        base64::engine::general_purpose::STANDARD.encode(b"agent-viewer:test-secret")
    );
    for pair in requests.chunks_exact(2) {
        assert_eq!(pair[0].connection_id, pair[1].connection_id);
        assert_eq!(pair[0].method, "GET");
        assert_eq!(pair[0].target, "/global/health");
        assert!(!pair[0].headers.contains_key("authorization"));
        assert!(pair[0].body.is_empty());
        assert_eq!(pair[1].headers.get("authorization"), Some(&expected));
    }
}

fn authenticated_requests(requests: &[RecordedRequest]) -> Vec<&RecordedRequest> {
    requests
        .iter()
        .filter(|request| request.headers.contains_key("authorization"))
        .collect()
}

fn managed_permission() -> Value {
    json!([
        {"permission": "agent-viewer.background", "pattern": "*", "action": "allow"}
    ])
}

fn backend_with_runtime(runtime: OpencodeRuntime) -> OpencodeBackend {
    OpencodeBackend::with_runtime(runtime)
}

fn backend_with_db_and_runtime(db_path: PathBuf, runtime: OpencodeRuntime) -> OpencodeBackend {
    OpencodeBackend::with_db_and_runtime(db_path, runtime)
}

fn command_args(command: &Command) -> Vec<String> {
    command
        .get_args()
        .map(|arg| arg.to_string_lossy().into_owned())
        .collect()
}

fn encoded_component(value: &str) -> String {
    let mut encoded = String::new();
    for byte in value.as_bytes() {
        match *byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                encoded.push(*byte as char);
            }
            byte => encoded.push_str(&format!("%{byte:02X}")),
        }
    }
    encoded
}

#[test]
fn opencode_spawn_readiness_deadline_is_one_bounded_budget() {
    let [primary, backup] = unused_addrs();
    let launches = Arc::new(AtomicUsize::new(0));
    let launches_for_callback = Arc::clone(&launches);
    let fixture = tempfile::TempDir::new().unwrap();
    let runtime = OpencodeRuntime::for_test_secure(OpencodeRuntimeTestConfig {
        candidates: [primary, backup],
        startup_timeout: Duration::from_millis(120),
        durable_cwd: PathBuf::from("/"),
        launcher: Arc::new(move |_| {
            launches_for_callback.fetch_add(1, Ordering::SeqCst);
            Ok(42)
        }),
        viewer_db_path: fixture.path().join("viewer.db"),
        proc_root: fixture.path().join("proc"),
        password_override: Some("test-secret".to_string()),
        before_authorized_write: None,
    });
    let backend = backend_with_runtime(runtime);

    let start = Instant::now();
    let error = backend
        .spawn(PathBuf::from("/tmp").as_path(), "hello", None, None)
        .expect_err("a launcher that never becomes healthy must fail");
    let elapsed = start.elapsed();
    assert!(
        elapsed < Duration::from_secs(1),
        "readiness exceeded its overall deadline: {elapsed:?}"
    );
    assert!(
        error.to_string().contains(&primary.port().to_string()),
        "{}",
        error
    );
    assert!(launches.load(Ordering::SeqCst) >= 1);
}

// --- Preserved v1 listing shape (order / labels / hidden) ---

#[test]
fn opencode_lists_rows_hidden_and_update_order_ascending() {
    let schema = common::read_fixture("opencode_session_schema.sql");
    let inserts = [
        "INSERT INTO session (id, parent_id, directory, title, time_created, time_updated, time_archived) \
         VALUES ('ses_parent',NULL,'/home/user/oc-proj','Parent',1000,3000,NULL)",
        "INSERT INTO session (id, parent_id, directory, title, time_created, time_updated, time_archived) \
         VALUES ('ses_child','ses_parent','/home/user/oc-proj','Child',1100,2000,NULL)",
        "INSERT INTO session (id, parent_id, directory, title, time_created, time_updated, time_archived) \
         VALUES ('ses_arch',NULL,'/home/user/oc-proj','Archived',900,1000,5000)",
    ];
    let (_dir, path) = common::temp_db(&schema, &inserts);

    let mut backend = OpencodeBackend::with_db(path);
    let sessions = backend.list().expect("list opencode sessions");
    assert_eq!(sessions.len(), 3);

    assert_eq!(
        sessions
            .iter()
            .map(|session| session.id.as_str())
            .collect::<Vec<_>>(),
        vec!["ses_arch", "ses_child", "ses_parent"]
    );

    let parent = sessions
        .iter()
        .find(|session| session.id == "ses_parent")
        .unwrap();
    assert_eq!(parent.backend, BackendKind::Opencode);
    assert_eq!(parent.cwd, PathBuf::from("/home/user/oc-proj"));
    assert_eq!(parent.title, "Parent");
    assert_eq!(parent.created_at_ms, 1000);
    assert_eq!(parent.updated_at_ms, 3000);
    assert_eq!(parent.origin, SessionOrigin::Interactive);
    assert!(!parent.hidden);
    assert_eq!(parent.short_id, None); // opencode sessions carry no claude short id

    let child = sessions
        .iter()
        .find(|session| session.id == "ses_child")
        .unwrap();
    assert!(child.companion); // parent_id non-NULL
    assert!(!child.hidden);
    let archived = sessions
        .iter()
        .find(|session| session.id == "ses_arch")
        .unwrap();
    assert!(archived.hidden); // time_archived IS NOT NULL
}

#[test]
fn opencode_missing_db_lists_empty() {
    let dir = tempfile::TempDir::new().unwrap();
    let mut backend = OpencodeBackend::with_db(dir.path().join("nope.db"));
    let sessions = backend.list().expect("missing DB must be Ok(empty)");
    assert!(sessions.is_empty());
}

#[test]
fn opencode_server_list_maps_status_input_companions_archive_and_hosting() {
    let mut sessions = common::fixture_json("opencode/session_list_with_archived_row.json");
    let rows = sessions.as_array_mut().unwrap();
    let archived_id = rows[0]["id"].as_str().unwrap().to_string();
    let busy_id = rows[1]["id"].as_str().unwrap().to_string();
    let run_mode_id = rows[2]["id"].as_str().unwrap().to_string();
    let permission_id = rows[3]["id"].as_str().unwrap().to_string();
    let question_id = rows[4]["id"].as_str().unwrap().to_string();
    let external_ids = rows[5..]
        .iter()
        .map(|row| row["id"].as_str().unwrap().to_string())
        .collect::<Vec<_>>();
    rows[0]["permission"] = Value::Null;
    let parent_id = rows[0]["id"].clone();
    rows[1]["parentID"] = parent_id;
    for index in [1, 3, 4] {
        rows[index]["permission"] = managed_permission();
    }
    for row in &mut rows[5..] {
        row["permission"] = Value::Null;
    }
    for (index, row) in rows.iter_mut().enumerate() {
        row["time"]["created"] = json!((index as i64 + 1) * 1000);
        row["time"]["updated"] = json!([8000, 2000, 2000, 7000, 6000, 5000, 4000, 3000][index]);
    }
    let directory = rows[1]["directory"].as_str().unwrap().to_string();
    rows.swap(1, 2);
    let mut status = common::fixture_json("opencode/session_status_idle_busy_retry.json");
    status
        .as_object_mut()
        .unwrap()
        .insert(external_ids[0].clone(), json!({"type": "busy"}));
    let primary = ScriptedServer::spawn_verified(secure_responses(vec![
        healthy_response(),
        ScriptedResponse::json(200, sessions),
        ScriptedResponse::json(200, status),
        ScriptedResponse::json(
            200,
            json!([{
                "id": "per_1",
                "sessionID": permission_id,
                "permission": "bash",
                "patterns": ["git status", "git diff"]
            }]),
        ),
        ScriptedResponse::json(
            200,
            json!([{
                "id": "que_1",
                "sessionID": question_id,
                "questions": [
                    {"question": "Which deployment target?", "header": "Target"},
                    {"question": "Ignored second question?"}
                ]
            }]),
        ),
    ]));
    let launcher_calls = Arc::new(AtomicUsize::new(0));
    let launcher_calls_for_runtime = Arc::clone(&launcher_calls);
    let runtime = secure_runtime_for(
        &primary,
        [primary.addr, unused_addr()],
        Duration::from_millis(250),
        move |_| {
            launcher_calls_for_runtime.fetch_add(1, Ordering::SeqCst);
            Err(io::Error::other("launcher was not expected"))
        },
    );
    let mut backend = backend_with_runtime(runtime);

    let listed = backend.list().expect("server list");
    assert_eq!(listed.len(), 8);
    assert_eq!(
        listed
            .iter()
            .map(|session| session.updated_at_ms)
            .collect::<Vec<_>>(),
        {
            let mut values = listed
                .iter()
                .map(|session| session.updated_at_ms)
                .collect::<Vec<_>>();
            values.sort();
            values
        },
        "server rows remain oldest first"
    );
    assert_eq!(
        listed
            .iter()
            .filter(|session| session.updated_at_ms == 2000)
            .map(|session| session.id.as_str())
            .collect::<Vec<_>>(),
        vec![run_mode_id.as_str(), busy_id.as_str()],
        "equal update values retain input order"
    );
    let row = |id: &str| listed.iter().find(|session| session.id == id).unwrap();
    let archived = row(&archived_id);
    assert_eq!(archived.status, Status::Idle);
    assert!(archived.hidden);
    assert!(!archived.daemon_hosted);

    let busy = row(&busy_id);
    assert_eq!(busy.status, Status::Working);
    assert!(busy.companion, "parentID independently marks a companion");
    assert!(busy.daemon_hosted);
    assert_eq!(busy.pid, None);

    let run_mode = row(&run_mode_id);
    assert_eq!(
        run_mode.status,
        Status::Idle,
        "external run mode history ignores scoped retry status"
    );
    assert!(
        run_mode.companion,
        "the captured denied question permission independently marks a run row"
    );
    assert!(!run_mode.daemon_hosted);

    let permission = row(&permission_id);
    let Status::NeedsInput {
        reason: Some(reason),
    } = &permission.status
    else {
        panic!("pending permission did not become NeedsInput");
    };
    assert!(reason.contains("bash"), "{reason}");
    assert!(reason.contains("git status"), "{reason}");
    assert!(permission.daemon_hosted);
    assert_eq!(permission.pid, None);

    let question = row(&question_id);
    assert_eq!(
        question.status,
        Status::NeedsInput {
            reason: Some("Which deployment target?".to_string())
        }
    );
    assert!(question.daemon_hosted);
    assert_eq!(question.pid, None);

    for external_id in &external_ids {
        let external = row(external_id);
        assert_eq!(
            external.status,
            Status::Idle,
            "unmarked external history ignores scoped server status"
        );
        assert!(!external.daemon_hosted);
    }
    assert_eq!(launcher_calls.load(Ordering::SeqCst), 0);
    let requests = primary.wait_for_requests(10);
    assert_test_secret_pairs(&requests);
    assert_eq!(
        requests
            .iter()
            .filter(|request| request.headers.contains_key("authorization"))
            .map(|request| request.target.clone())
            .collect::<Vec<_>>(),
        vec![
            "/global/health".to_string(),
            "/experimental/session?limit=10000&archived=true".to_string(),
            format!(
                "/session/status?directory={}",
                encoded_component(&directory)
            ),
            format!("/permission?directory={}", encoded_component(&directory)),
            format!("/question?directory={}", encoded_component(&directory)),
        ],
        "listing scopes one directory snapshot triplet to the managed directory"
    );
}

#[test]
fn opencode_server_directory_snapshots_are_once_per_unique_managed_directory() {
    let first_directory = "/tmp/project one";
    let second_directory = "/tmp/雪?project/two";
    let sessions = json!([
        {
            "id": "ses_first_busy",
            "parentID": null,
            "directory": first_directory,
            "title": "first busy",
            "permission": managed_permission(),
            "time": {"created": 10, "updated": 50}
        },
        {
            "id": "ses_first_idle",
            "parentID": null,
            "directory": first_directory,
            "title": "first idle",
            "permission": managed_permission(),
            "time": {"created": 10, "updated": 40}
        },
        {
            "id": "ses_second_retry",
            "parentID": null,
            "directory": second_directory,
            "title": "second retry",
            "permission": managed_permission(),
            "time": {"created": 10, "updated": 30}
        },
        {
            "id": "ses_external_history",
            "parentID": null,
            "directory": "/historical/do not probe",
            "title": "external",
            "permission": null,
            "time": {"created": 10, "updated": 20}
        },
        {
            "id": "ses_archived_marked",
            "parentID": null,
            "directory": "/archived/do not probe",
            "title": "archived",
            "permission": managed_permission(),
            "time": {"created": 10, "updated": 10, "archived": 11}
        }
    ]);
    let server = ScriptedServer::spawn_verified(secure_responses(vec![
        healthy_response(),
        ScriptedResponse::json(200, sessions),
        ScriptedResponse::json(
            200,
            json!({
                "ses_first_busy": {"type": "busy"},
                "ses_first_idle": {"type": "idle"}
            }),
        ),
        ScriptedResponse::json(200, json!([])),
        ScriptedResponse::json(200, json!([])),
        ScriptedResponse::json(
            200,
            json!({
                "ses_second_retry": {
                    "type": "retry",
                    "attempt": 2,
                    "message": "temporary provider failure",
                    "next": 100
                }
            }),
        ),
        ScriptedResponse::json(200, json!([])),
        ScriptedResponse::json(200, json!([])),
    ]));
    let launcher_calls = Arc::new(AtomicUsize::new(0));
    let launcher_calls_for_runtime = Arc::clone(&launcher_calls);
    let runtime = secure_runtime_for(
        &server,
        [server.addr, unused_addr()],
        Duration::from_millis(250),
        move |_| {
            launcher_calls_for_runtime.fetch_add(1, Ordering::SeqCst);
            Err(io::Error::other("launcher was not expected"))
        },
    );
    let mut backend = backend_with_runtime(runtime);

    let listed = backend.list().expect("directory scoped server list");
    let status = |id: &str| {
        listed
            .iter()
            .find(|session| session.id == id)
            .unwrap()
            .status
            .clone()
    };
    assert_eq!(status("ses_first_busy"), Status::Working);
    assert_eq!(status("ses_first_idle"), Status::Idle);
    assert_eq!(status("ses_second_retry"), Status::Working);
    assert_eq!(status("ses_external_history"), Status::Idle);

    let first = encoded_component(first_directory);
    let second = encoded_component(second_directory);
    let requests = server.wait_for_requests(16);
    assert_test_secret_pairs(&requests);
    assert_eq!(
        requests
            .iter()
            .filter(|request| request.headers.contains_key("authorization"))
            .map(|request| request.target.clone())
            .collect::<Vec<_>>(),
        vec![
            "/global/health".to_string(),
            "/experimental/session?limit=10000&archived=true".to_string(),
            format!("/session/status?directory={first}"),
            format!("/permission?directory={first}"),
            format!("/question?directory={first}"),
            format!("/session/status?directory={second}"),
            format!("/permission?directory={second}"),
            format!("/question?directory={second}"),
        ],
        "two managed rows in one directory share one directory snapshot triplet"
    );
    assert_eq!(
        server.requests().len(),
        16,
        "directory snapshot calls scale with unique managed directories, not rows"
    );
    assert_eq!(launcher_calls.load(Ordering::SeqCst), 0);
}

#[test]
fn opencode_healthy_server_api_failure_is_not_masked_by_sqlite() {
    let schema = common::read_fixture("opencode_session_schema.sql");
    let inserts = [
        "INSERT INTO session (id, parent_id, directory, title, time_created, time_updated, time_archived) \
         VALUES ('ses_sqlite',NULL,'/tmp','must not leak through',1000,3000,NULL)",
    ];
    let (_dir, path) = common::temp_db(&schema, &inserts);
    let server = ScriptedServer::spawn_verified(secure_responses(vec![
        healthy_response(),
        ScriptedResponse::raw(500, "text/plain", "session route failed"),
    ]));
    let launcher_calls = Arc::new(AtomicUsize::new(0));
    let launcher_calls_for_runtime = Arc::clone(&launcher_calls);
    let runtime = secure_runtime_for(
        &server,
        [server.addr, unused_addr()],
        Duration::from_millis(250),
        move |_| {
            launcher_calls_for_runtime.fetch_add(1, Ordering::SeqCst);
            Err(io::Error::other("launcher was not expected"))
        },
    );
    let mut backend = backend_with_db_and_runtime(path, runtime);

    let error = backend
        .list()
        .expect_err("healthy server route failure must remain visible");
    let message = error.to_string();
    assert!(message.contains("/session"), "{message}");
    assert!(message.contains("500"), "{message}");
    assert!(!message.contains("must not leak through"), "{message}");
    assert_eq!(launcher_calls.load(Ordering::SeqCst), 0);
}

#[test]
fn opencode_unavailable_runtime_uses_read_only_sqlite_without_launching() {
    let schema = common::read_fixture("opencode_session_schema.sql");
    let inserts = [
        "INSERT INTO session (id, parent_id, directory, title, time_created, time_updated, time_archived) \
         VALUES ('ses_external',NULL,'/tmp','External compatibility row',1000,3000,NULL)",
    ];
    let (_dir, path) = common::temp_db(&schema, &inserts);
    let mut backend = OpencodeBackend::with_db(path);

    let listed = backend.list().expect("SQLite compatibility listing");
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].id, "ses_external");
    assert!(!listed[0].daemon_hosted);
}

#[test]
fn opencode_missing_servers_use_sqlite_when_viewer_secret_storage_is_unavailable() {
    let schema = common::read_fixture("opencode_session_schema.sql");
    let inserts = [
        "INSERT INTO session (id, parent_id, directory, title, time_created, time_updated, time_archived) \
         VALUES ('ses_offline',NULL,'/tmp','Offline compatibility row',1000,3000,NULL)",
    ];
    let (_dir, path) = common::temp_db(&schema, &inserts);
    let proc_root = new_proc_root();
    let launches = Arc::new(AtomicUsize::new(0));
    let launches_for_runtime = Arc::clone(&launches);
    let runtime = OpencodeRuntime::for_test_secure(OpencodeRuntimeTestConfig {
        candidates: unused_addrs(),
        startup_timeout: Duration::from_millis(80),
        durable_cwd: PathBuf::from("/"),
        launcher: Arc::new(move |_| {
            launches_for_runtime.fetch_add(1, Ordering::SeqCst);
            Err(io::Error::other("listing must not launch"))
        }),
        viewer_db_path: proc_root.path().to_path_buf(),
        proc_root: proc_root.path().to_path_buf(),
        password_override: None,
        before_authorized_write: None,
    });
    let mut backend = backend_with_db_and_runtime(path, runtime);

    let listed = backend
        .list()
        .expect("missing servers use the SQLite compatibility listing");

    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].id, "ses_offline");
    assert!(!listed[0].daemon_hosted);
    assert_eq!(launches.load(Ordering::SeqCst), 0);
}

#[test]
fn opencode_discovery_continues_after_primary_rejects_viewer_credentials() {
    let proc_root = new_proc_root();
    let primary = ScriptedServer::spawn_on_verified_in(
        unused_addr(),
        vec![unauthorized_health().keep_alive(), unauthorized_health()],
        Arc::clone(&proc_root),
        54001,
    )
    .expect("bind primary server");
    let backup = ScriptedServer::spawn_on_verified_in(
        unused_addr(),
        secure_responses(vec![
            healthy_response(),
            ScriptedResponse::json(200, json!([])),
            healthy_response(),
            ScriptedResponse::raw(500, "text/plain", "unexpected mutation"),
        ]),
        Arc::clone(&proc_root),
        54002,
    )
    .expect("bind backup server");
    let launches = Arc::new(AtomicUsize::new(0));
    let launches_for_runtime = Arc::clone(&launches);
    let runtime = OpencodeRuntime::for_test_secure(OpencodeRuntimeTestConfig {
        candidates: [primary.addr, backup.addr],
        startup_timeout: Duration::from_millis(250),
        durable_cwd: PathBuf::from("/"),
        launcher: Arc::new(move |_| {
            launches_for_runtime.fetch_add(1, Ordering::SeqCst);
            Err(io::Error::other("discovery must not launch"))
        }),
        viewer_db_path: proc_root.path().join("viewer.db"),
        proc_root: proc_root.path().to_path_buf(),
        password_override: Some("test-secret".to_string()),
        before_authorized_write: None,
    });
    let mut backend = backend_with_runtime(runtime);

    assert!(
        backend
            .list()
            .expect("authenticated backup provides the listing")
            .is_empty()
    );
    assert_eq!(launches.load(Ordering::SeqCst), 0);
    assert_eq!(primary.wait_for_requests(2).len(), 2);
    assert_eq!(backup.wait_for_requests(4).len(), 4);
    assert_test_secret_pairs(&primary.requests());
    assert_test_secret_pairs(&backup.requests());
}

#[test]
fn opencode_capabilities_follow_the_last_probed_tier_without_network_on_read() {
    let server = ScriptedServer::spawn_verified(secure_responses(vec![
        healthy_response(),
        ScriptedResponse::json(
            200,
            json!([{
                "id": "ses_cap",
                "parentID": null,
                "directory": "/tmp",
                "title": "managed",
                "permission": managed_permission(),
                "time": {"created": 1, "updated": 2, "archived": 3}
            }]),
        ),
    ]));
    let healthy_launcher_calls = Arc::new(AtomicUsize::new(0));
    let healthy_launcher_calls_for_runtime = Arc::clone(&healthy_launcher_calls);
    let healthy_runtime = secure_runtime_for(
        &server,
        [server.addr, unused_addr()],
        Duration::from_millis(250),
        move |_| {
            healthy_launcher_calls_for_runtime.fetch_add(1, Ordering::SeqCst);
            Err(io::Error::other("launcher was not expected"))
        },
    );
    let mut healthy = backend_with_runtime(healthy_runtime);
    healthy.list().expect("establish healthy tier");
    let request_count = server.requests().len();
    let healthy_caps = healthy.capabilities();
    assert_eq!(
        healthy_caps,
        Capabilities {
            spawn: true,
            attach: true,
            rename: true,
            archive: true,
            delete: true,
            stop: true,
            needs_input: true,
            pr_refs: false,
            live_status: true,
        }
    );
    let mut server_row = session_with_pid(None);
    server_row.daemon_hosted = true;
    assert!(
        !healthy.capabilities_for(&server_row).attach,
        "managed server rows must not advertise credentialed attach"
    );
    assert!(
        healthy.capabilities_for(&session_with_pid(None)).attach,
        "external rows remain locally attachable"
    );
    assert_eq!(
        server.requests().len(),
        request_count,
        "capability reads must not perform HTTP"
    );

    let mut fallback =
        OpencodeBackend::with_db(tempfile::TempDir::new().unwrap().path().join("missing.db"));
    fallback.list().expect("establish compatibility tier");
    assert_eq!(
        fallback.capabilities(),
        Capabilities {
            spawn: true,
            attach: true,
            rename: false,
            archive: false,
            delete: true,
            stop: false,
            needs_input: false,
            pr_refs: false,
            live_status: false,
        }
    );
    assert_eq!(healthy_launcher_calls.load(Ordering::SeqCst), 0);
}

// --- v2: three-tier heuristic (test 17) ---

#[test]
fn opencode_status_three_tiers() {
    let now = 1_000_000_000_000_i64;
    // live + fresh -> Working (age <= 60_000, boundary inclusive).
    assert_eq!(opencode_status(true, now - 59_000, now), Status::Working);
    assert_eq!(opencode_status(true, now - 60_000, now), Status::Working);
    // live + <= 30 min -> Idle (boundary inclusive at 1_800_000).
    assert_eq!(opencode_status(true, now - 61_000, now), Status::Idle);
    assert_eq!(opencode_status(true, now - 1_800_000, now), Status::Idle);
    // live but older than 30 min -> Done.
    assert_eq!(opencode_status(true, now - 1_800_001, now), Status::Done);
    // no live process -> Done regardless of recency.
    assert_eq!(opencode_status(false, now - 5_000, now), Status::Done);
}

// --- v2: companion flag from parent_id (test 18) ---

#[test]
fn opencode_lists_companion_flag() {
    let schema = common::read_fixture("opencode_session_schema.sql");
    let inserts = [
        "INSERT INTO session (id, parent_id, directory, title, time_created, time_updated, time_archived) \
         VALUES ('ses_root',NULL,'/home/user/oc-proj','Root',1000,3000,NULL)",
        "INSERT INTO session (id, parent_id, directory, title, time_created, time_updated, time_archived) \
         VALUES ('ses_kid','ses_root','/home/user/oc-proj','Kid',1100,2000,NULL)",
    ];
    let (_dir, path) = common::temp_db(&schema, &inserts);

    let mut backend = OpencodeBackend::with_db(path);
    let sessions = backend.list().expect("list opencode sessions");

    let root = sessions.iter().find(|s| s.id == "ses_root").unwrap();
    let kid = sessions.iter().find(|s| s.id == "ses_kid").unwrap();
    assert!(!root.companion); // parent_id IS NULL
    assert!(kid.companion); // parent_id set -> companion
}

// --- v2: last-message reader (peek) ---

#[test]
fn opencode_last_message_returns_newest_text_concatenated() {
    let schema = common::read_fixture("opencode_message_schema.sql");
    let inserts = [
        "INSERT INTO session (id, parent_id, directory, title, time_created, time_updated, time_archived) \
         VALUES ('ses_1',NULL,'/home/user/oc-proj','Proj',1000,3000,NULL)",
        // Older assistant message with a single text part.
        "INSERT INTO message (id, session_id, time_created, time_updated, data) \
         VALUES ('msg_old','ses_1',1000,1000,'{\"role\":\"assistant\"}')",
        "INSERT INTO part (id, message_id, session_id, time_created, time_updated, data) \
         VALUES ('prt_old','msg_old','ses_1',1000,1000,'{\"type\":\"text\",\"text\":\"older reply\"}')",
        // Newer assistant message: a tool part THEN a text part (tool must be skipped).
        "INSERT INTO message (id, session_id, time_created, time_updated, data) \
         VALUES ('msg_new','ses_1',2000,2000,'{\"role\":\"assistant\"}')",
        "INSERT INTO part (id, message_id, session_id, time_created, time_updated, data) \
         VALUES ('prt_new_tool','msg_new','ses_1',2001,2001,'{\"type\":\"tool\",\"tool\":\"bash\"}')",
        "INSERT INTO part (id, message_id, session_id, time_created, time_updated, data) \
         VALUES ('prt_new_text','msg_new','ses_1',2002,2002,'{\"type\":\"text\",\"text\":\"newer reply text\"}')",
    ];
    let (_dir, path) = common::temp_db(&schema, &inserts);

    let item = read_opencode_last_message(&path, "ses_1")
        .expect("read ok")
        .expect("a text message exists");
    assert_eq!(item.role, "assistant");
    // The NEWER message's text only, tool part skipped.
    assert_eq!(item.text, "newer reply text");
}

#[test]
fn opencode_last_message_skips_whitespace_only_newest() {
    let schema = common::read_fixture("opencode_message_schema.sql");
    let inserts = [
        "INSERT INTO session (id, parent_id, directory, title, time_created, time_updated, time_archived) \
         VALUES ('ses_1',NULL,'/home/user/oc-proj','Proj',1000,3000,NULL)",
        // Older assistant message with real text.
        "INSERT INTO message (id, session_id, time_created, time_updated, data) \
         VALUES ('msg_old','ses_1',1000,1000,'{\"role\":\"assistant\"}')",
        "INSERT INTO part (id, message_id, session_id, time_created, time_updated, data) \
         VALUES ('prt_old','msg_old','ses_1',1000,1000,'{\"type\":\"text\",\"text\":\"real prior message\"}')",
        // Newest message whose only text part is whitespace (newline-only around a tool
        // transition) -> it must be skipped so the real prior message surfaces.
        "INSERT INTO message (id, session_id, time_created, time_updated, data) \
         VALUES ('msg_new','ses_1',2000,2000,'{\"role\":\"assistant\"}')",
        "INSERT INTO part (id, message_id, session_id, time_created, time_updated, data) \
         VALUES ('prt_new','msg_new','ses_1',2000,2000,'{\"type\":\"text\",\"text\":\"\\n\\n\"}')",
    ];
    let (_dir, path) = common::temp_db(&schema, &inserts);

    let item = read_opencode_last_message(&path, "ses_1")
        .expect("read ok")
        .expect("a text message exists");
    assert_eq!(item.text, "real prior message");
}

#[test]
fn opencode_last_message_none_when_no_text_message() {
    let schema = common::read_fixture("opencode_message_schema.sql");
    let inserts = [
        "INSERT INTO session (id, parent_id, directory, title, time_created, time_updated, time_archived) \
         VALUES ('ses_1',NULL,'/home/user/oc-proj','Proj',1000,3000,NULL)",
        // A message whose only part is a tool part -> no text -> Ok(None).
        "INSERT INTO message (id, session_id, time_created, time_updated, data) \
         VALUES ('msg_1','ses_1',1000,1000,'{\"role\":\"assistant\"}')",
        "INSERT INTO part (id, message_id, session_id, time_created, time_updated, data) \
         VALUES ('prt_1','msg_1','ses_1',1000,1000,'{\"type\":\"tool\",\"tool\":\"bash\"}')",
    ];
    let (_dir, path) = common::temp_db(&schema, &inserts);
    // Also nothing for an unknown session id.
    assert!(
        read_opencode_last_message(&path, "ses_1")
            .expect("read ok")
            .is_none()
    );
    assert!(
        read_opencode_last_message(&path, "nope")
            .expect("read ok")
            .is_none()
    );
}

#[test]
fn opencode_last_message_missing_db_is_none() {
    let dir = tempfile::TempDir::new().unwrap();
    let missing = dir.path().join("nope.db");
    assert!(
        read_opencode_last_message(&missing, "ses_1")
            .expect("read ok")
            .is_none()
    );
}

fn opencode_activity_session(id: &str) -> Session {
    Session {
        backend: BackendKind::Opencode,
        id: id.to_string(),
        short_id: None,
        origin: SessionOrigin::Interactive,
        title: "Activity session".to_string(),
        cwd: PathBuf::from("/home/user/project"),
        git_branch: None,
        status: Status::Done,
        created_at_ms: 0,
        updated_at_ms: 0,
        hidden: false,
        companion: false,
        summary: String::new(),
        pid: None,
        rollout_path: None,
        pr_refs: Vec::new(),
        daemon_hosted: false,
    }
}

#[test]
fn opencode_turn_activity_normalizes_filters_and_missing_is_empty() {
    let schema = common::read_fixture("opencode_message_schema.sql");
    let now = agent_viewer_core::spawn::now_ms();
    let user = now - 2_000;
    let assistant = now - 1_000;
    let old = now - 60_000;
    let inserts = [
        format!(
            "INSERT INTO session (id, parent_id, directory, title, time_created, time_updated, time_archived) \
             VALUES ('ses_activity',NULL,'/home/user/project','Activity',{now},{now},NULL)"
        ),
        format!(
            "INSERT INTO message (id, session_id, time_created, time_updated, data) \
             VALUES ('msg_old','ses_activity',{old},{old},'{{\"role\":\"user\"}}')"
        ),
        format!(
            "INSERT INTO message (id, session_id, time_created, time_updated, data) \
             VALUES ('msg_user','ses_activity',{user},{user},'{{\"role\":\"user\"}}')"
        ),
        format!(
            "INSERT INTO message (id, session_id, time_created, time_updated, data) \
             VALUES ('msg_assistant','ses_activity',{assistant},{assistant},'{{\"role\":\"assistant\"}}')"
        ),
        format!(
            "INSERT INTO message (id, session_id, time_created, time_updated, data) \
             VALUES ('msg_tool','ses_activity',{now},{now},'{{\"role\":\"tool\"}}')"
        ),
        format!(
            "INSERT INTO message (id, session_id, time_created, time_updated, data) \
             VALUES ('msg_bad','ses_activity','not-a-timestamp',{now},'{{\"role\":\"assistant\"}}')"
        ),
    ];
    let insert_refs = inserts.iter().map(String::as_str).collect::<Vec<_>>();
    let (_dir, path) = common::temp_db(&schema, &insert_refs);
    let backend = OpencodeBackend::with_db(path);
    assert_eq!(
        backend
            .turn_activity(
                &opencode_activity_session("ses_activity"),
                Duration::from_secs(10)
            )
            .expect("activity"),
        vec![user, assistant]
    );
    assert!(
        backend
            .turn_activity(
                &opencode_activity_session("missing"),
                Duration::from_secs(10)
            )
            .expect("missing session")
            .is_empty()
    );

    let missing_backend = OpencodeBackend::with_db(PathBuf::from("/missing/opencode/activity.db"));
    assert!(
        missing_backend
            .turn_activity(
                &opencode_activity_session("ses_activity"),
                Duration::from_secs(10)
            )
            .expect("missing database")
            .is_empty()
    );
}

#[test]
fn opencode_turn_activity_includes_only_requested_subtree() {
    let schema = common::read_fixture("opencode_message_schema.sql");
    let now = agent_viewer_core::spawn::now_ms();
    let session_rows = [
        format!(
            "INSERT INTO session (id, parent_id, directory, title, time_created, time_updated, time_archived) \
             VALUES ('root',NULL,'/project','Root',{now},{now},NULL)"
        ),
        format!(
            "INSERT INTO session (id, parent_id, directory, title, time_created, time_updated, time_archived) \
             VALUES ('child_a','root','/project','Child A',{now},{now},NULL)"
        ),
        format!(
            "INSERT INTO session (id, parent_id, directory, title, time_created, time_updated, time_archived) \
             VALUES ('child_b','root','/project','Child B',{now},{now},NULL)"
        ),
        format!(
            "INSERT INTO session (id, parent_id, directory, title, time_created, time_updated, time_archived) \
             VALUES ('grandchild','child_a','/project','Grandchild',{now},{now},NULL)"
        ),
        format!(
            "INSERT INTO session (id, parent_id, directory, title, time_created, time_updated, time_archived) \
             VALUES ('missing_activity','root','/project','Missing Activity',{now},{now},NULL)"
        ),
        format!(
            "INSERT INTO session (id, parent_id, directory, title, time_created, time_updated, time_archived) \
             VALUES ('unrelated',NULL,'/project','Unrelated',{now},{now},NULL)"
        ),
        format!(
            "INSERT INTO session (id, parent_id, directory, title, time_created, time_updated, time_archived) \
             VALUES ('cycle_a','cycle_b','/project','Cycle A',{now},{now},NULL)"
        ),
        format!(
            "INSERT INTO session (id, parent_id, directory, title, time_created, time_updated, time_archived) \
             VALUES ('cycle_b','cycle_a','/project','Cycle B',{now},{now},NULL)"
        ),
    ];
    let message_rows = [
        format!(
            "INSERT INTO message (id, session_id, time_created, time_updated, data) \
             VALUES ('root_message','root',{},{} ,'{{\"role\":\"user\"}}')",
            now - 6_000,
            now - 6_000
        ),
        format!(
            "INSERT INTO message (id, session_id, time_created, time_updated, data) \
             VALUES ('child_a_message','child_a',{},{} ,'{{\"role\":\"assistant\"}}')",
            now - 5_000,
            now - 5_000
        ),
        format!(
            "INSERT INTO message (id, session_id, time_created, time_updated, data) \
             VALUES ('child_b_message','child_b',{},{} ,'{{\"role\":\"user\"}}')",
            now - 4_000,
            now - 4_000
        ),
        format!(
            "INSERT INTO message (id, session_id, time_created, time_updated, data) \
             VALUES ('grandchild_message','grandchild',{},{} ,'{{\"role\":\"assistant\"}}')",
            now - 3_000,
            now - 3_000
        ),
        format!(
            "INSERT INTO message (id, session_id, time_created, time_updated, data) \
             VALUES ('unrelated_message','unrelated',{},{} ,'{{\"role\":\"assistant\"}}')",
            now - 2_000,
            now - 2_000
        ),
        format!(
            "INSERT INTO message (id, session_id, time_created, time_updated, data) \
             VALUES ('malformed_message','child_a',{},{} ,'{{not json')",
            now - 1_000,
            now - 1_000
        ),
        format!(
            "INSERT INTO message (id, session_id, time_created, time_updated, data) \
             VALUES ('cycle_a_message','cycle_a',{},{} ,'{{\"role\":\"user\"}}')",
            now - 8_000,
            now - 8_000
        ),
        format!(
            "INSERT INTO message (id, session_id, time_created, time_updated, data) \
             VALUES ('cycle_b_message','cycle_b',{},{} ,'{{\"role\":\"assistant\"}}')",
            now - 7_000,
            now - 7_000
        ),
    ];
    let inserts = session_rows
        .iter()
        .chain(message_rows.iter())
        .map(String::as_str)
        .collect::<Vec<_>>();
    let (_dir, path) = common::temp_db(&schema, &inserts);
    let backend = OpencodeBackend::with_db(path);

    assert_eq!(
        backend
            .turn_activity(&opencode_activity_session("root"), Duration::from_secs(60))
            .expect("root subtree activity"),
        vec![now - 6_000, now - 5_000, now - 4_000, now - 3_000]
    );
    assert_eq!(
        backend
            .turn_activity(
                &opencode_activity_session("child_a"),
                Duration::from_secs(60)
            )
            .expect("child subtree activity"),
        vec![now - 5_000, now - 3_000]
    );
    assert_eq!(
        backend
            .turn_activity(
                &opencode_activity_session("cycle_a"),
                Duration::from_secs(60)
            )
            .expect("cycle activity"),
        vec![now - 8_000, now - 7_000]
    );
}

// --- v2: `opencode models` stdout parse ---

#[test]
fn parse_opencode_models_trims_and_drops_blanks() {
    let stdout = "\
anthropic/claude-opus-4-8
  openai/gpt-5.6

github-copilot/gpt-5

";
    let got = parse_opencode_models(stdout);
    assert_eq!(
        got,
        vec![
            "anthropic/claude-opus-4-8",
            "openai/gpt-5.6",
            "github-copilot/gpt-5",
        ]
    );
}

// --- per-row stop capability ---

fn session_with_pid(pid: Option<u32>) -> Session {
    Session {
        backend: BackendKind::Opencode,
        id: "ses_cap".to_string(),
        short_id: None,
        origin: SessionOrigin::Interactive,
        title: "probe".to_string(),
        cwd: PathBuf::from("/tmp"),
        git_branch: None,
        status: Status::Idle,
        created_at_ms: 0,
        updated_at_ms: 0,
        hidden: false,
        companion: false,
        summary: String::new(),
        pid,
        rollout_path: None,
        pr_refs: Vec::new(),
        daemon_hosted: false,
    }
}

fn server_session(id: &str, cwd: PathBuf) -> Session {
    let mut session = session_with_pid(None);
    session.id = id.to_string();
    session.cwd = cwd;
    session.daemon_hosted = true;
    session
}

#[test]
fn opencode_spawn_create_then_prompt_returns_exact_id_and_model_shapes() {
    let server = ScriptedServer::spawn_verified(secure_responses(vec![
        healthy_response(),
        ScriptedResponse::json(200, json!({"id": "ses_exact_identity"})),
        ScriptedResponse::empty(204),
    ]));
    let launcher_calls = Arc::new(AtomicUsize::new(0));
    let launcher_calls_for_runtime = Arc::clone(&launcher_calls);
    let runtime = secure_runtime_for(
        &server,
        [server.addr, unused_addr()],
        Duration::from_millis(250),
        move |_| {
            launcher_calls_for_runtime.fetch_add(1, Ordering::SeqCst);
            Err(io::Error::other("launcher was not expected"))
        },
    );
    let backend = backend_with_runtime(runtime);
    let cwd = PathBuf::from("/tmp/space and 雪/?x=1&next=../");
    let task = "A title with Unicode 雪 and enough text to exceed the forty character title limit";

    let spawned = backend
        .spawn(&cwd, task, Some("openai/gpt-5.6-sol"), None)
        .expect("server spawn");
    assert_eq!(spawned.pid, None);
    assert_eq!(spawned.session_id.as_deref(), Some("ses_exact_identity"));
    let all_requests = server.wait_for_requests(6);
    assert_test_secret_pairs(&all_requests);
    let requests = authenticated_requests(&all_requests);
    assert_eq!(
        requests
            .iter()
            .map(|request| request.method.as_str())
            .collect::<Vec<_>>(),
        vec!["GET", "POST", "POST"]
    );
    let directory = encoded_component(cwd.to_str().unwrap());
    assert_eq!(
        requests[1].target,
        format!("/session?directory={directory}")
    );
    assert_eq!(
        requests[2].target,
        format!("/session/ses_exact_identity/prompt_async?directory={directory}")
    );
    assert_eq!(
        requests[1].json_body(),
        json!({
            "title": task.chars().take(40).collect::<String>(),
            "permission": managed_permission(),
            "model": {"providerID": "openai", "id": "gpt-5.6-sol"}
        })
    );
    assert_eq!(
        requests[2].json_body(),
        json!({
            "parts": [{"type": "text", "text": task}],
            "model": {"providerID": "openai", "modelID": "gpt-5.6-sol"}
        })
    );
    assert!(
        requests[1]
            .headers
            .get("content-type")
            .is_some_and(|value| value.starts_with("application/json"))
    );
    assert_eq!(launcher_calls.load(Ordering::SeqCst), 0);
}

#[test]
fn opencode_spawn_omits_default_model_and_rejects_malformed_model_before_mutation() {
    let server = ScriptedServer::spawn_verified(secure_responses(vec![
        healthy_response(),
        ScriptedResponse::json(200, json!({"id": "ses_default"})),
        ScriptedResponse::empty(204),
        healthy_response(),
    ]));
    let launcher_calls = Arc::new(AtomicUsize::new(0));
    let launcher_calls_for_runtime = Arc::clone(&launcher_calls);
    let runtime = secure_runtime_for(
        &server,
        [server.addr, unused_addr()],
        Duration::from_millis(250),
        move |_| {
            launcher_calls_for_runtime.fetch_add(1, Ordering::SeqCst);
            Err(io::Error::other("launcher was not expected"))
        },
    );
    let backend = backend_with_runtime(runtime);

    backend
        .spawn(
            PathBuf::from("/tmp").as_path(),
            "default prompt",
            None,
            None,
        )
        .expect("default spawn");
    let all_requests = server.wait_for_requests(6);
    assert_test_secret_pairs(&all_requests);
    let requests = authenticated_requests(&all_requests);
    assert!(requests[1].json_body().get("model").is_none());
    assert!(requests[2].json_body().get("model").is_none());

    let error = backend
        .spawn(
            PathBuf::from("/tmp").as_path(),
            "must not submit",
            Some("missing-provider-separator"),
            None,
        )
        .expect_err("malformed model must fail");
    assert!(error.to_string().contains("provider"), "{error}");
    let all_after = server.wait_for_requests(8);
    assert_test_secret_pairs(&all_after);
    let after = authenticated_requests(&all_after);
    assert_eq!(
        after
            .iter()
            .filter(|request| request.method == "POST")
            .count(),
        2,
        "malformed model must not create or prompt a session"
    );
}

#[test]
fn opencode_prompt_failure_reports_created_id_and_never_submits_twice() {
    let server = ScriptedServer::spawn_verified(secure_responses(vec![
        healthy_response(),
        ScriptedResponse::json(200, json!({"id": "ses_orphan_visible"})),
        ScriptedResponse::raw(200, "text/plain", "provider rejected prompt"),
    ]));
    let launcher_calls = Arc::new(AtomicUsize::new(0));
    let launcher_calls_for_runtime = Arc::clone(&launcher_calls);
    let runtime = secure_runtime_for(
        &server,
        [server.addr, unused_addr()],
        Duration::from_millis(250),
        move |_| {
            launcher_calls_for_runtime.fetch_add(1, Ordering::SeqCst);
            Err(io::Error::other("launcher was not expected"))
        },
    );
    let backend = backend_with_runtime(runtime);

    let error = backend
        .spawn(
            PathBuf::from("/tmp").as_path(),
            "one submission",
            None,
            None,
        )
        .expect_err("prompt failure must be visible");
    let message = error.to_string();
    assert!(message.contains("ses_orphan_visible"), "{message}");
    assert!(
        message.contains(
            "POST /session/ses_orphan_visible/prompt_async?directory=%2Ftmp returned HTTP 200"
        ),
        "{message}"
    );
    assert!(!message.contains("provider rejected prompt"), "{message}");
    let all_requests = server.wait_for_requests(6);
    assert_test_secret_pairs(&all_requests);
    let requests = authenticated_requests(&all_requests);
    assert_eq!(
        requests
            .iter()
            .filter(|request| request.target.contains("prompt_async"))
            .count(),
        1
    );
    assert_eq!(launcher_calls.load(Ordering::SeqCst), 0);
}

#[test]
fn opencode_managed_attach_is_refused_while_external_attach_stays_local() {
    let server = ScriptedServer::spawn_verified(secure_responses(vec![
        healthy_response(),
        ScriptedResponse::json(
            200,
            json!([{
                "id": "ses_attach_exact",
                "parentID": null,
                "directory": "/tmp",
                "title": "managed",
                "permission": managed_permission(),
                "time": {"created": 1, "updated": 2, "archived": 3}
            }]),
        ),
        healthy_response(),
        healthy_response(),
    ]));
    let launcher_calls = Arc::new(AtomicUsize::new(0));
    let launcher_calls_for_runtime = Arc::clone(&launcher_calls);
    let runtime = secure_runtime_for(
        &server,
        [server.addr, unused_addr()],
        Duration::from_millis(250),
        move |_| {
            launcher_calls_for_runtime.fetch_add(1, Ordering::SeqCst);
            Err(io::Error::other("launcher was not expected"))
        },
    );
    let mut backend = backend_with_runtime(runtime);
    backend.list().expect("establish exact managed marker");
    let existing_dir = tempfile::TempDir::new().unwrap();
    let session = server_session("ses_attach_exact", existing_dir.path().to_path_buf());

    let refusal = backend
        .attach_command(&session)
        .expect_err("managed session attach is refused before a credentialed command exists");
    assert!(refusal.to_string().contains("managed"), "{refusal}");

    let external = session_with_pid(None);
    let command = backend
        .attach_command(&external)
        .expect("external session remains locally attachable");
    assert_eq!(command.get_program(), "opencode");
    assert_eq!(
        command_args(&command),
        vec!["-s".to_string(), "ses_cap".to_string()]
    );
    assert_eq!(
        command.get_current_dir(),
        Some(PathBuf::from("/tmp").as_path())
    );
    let env = command
        .get_envs()
        .map(|(key, value)| (key.to_string_lossy().into_owned(), value))
        .collect::<BTreeMap<_, _>>();
    assert!(!env.contains_key("OPENCODE_SERVER_USERNAME"));
    assert!(!env.contains_key("OPENCODE_SERVER_PASSWORD"));
    assert_eq!(launcher_calls.load(Ordering::SeqCst), 0);
}

#[test]
fn opencode_stop_aborts_only_the_percent_encoded_session() {
    let id = "ses/雪?../../target#fragment";
    let server = ScriptedServer::spawn_verified(secure_responses(vec![
        healthy_response(),
        ScriptedResponse::json(
            200,
            json!([{
                "id": id,
                "parentID": null,
                "directory": "/tmp",
                "title": "managed",
                "permission": managed_permission(),
                "time": {"created": 1, "updated": 2, "archived": 3}
            }]),
        ),
        healthy_response(),
        ScriptedResponse::json(200, json!(false)),
    ]));
    let launcher_calls = Arc::new(AtomicUsize::new(0));
    let launcher_calls_for_runtime = Arc::clone(&launcher_calls);
    let runtime = secure_runtime_for(
        &server,
        [server.addr, unused_addr()],
        Duration::from_millis(250),
        move |_| {
            launcher_calls_for_runtime.fetch_add(1, Ordering::SeqCst);
            Err(io::Error::other("launcher was not expected"))
        },
    );
    let mut backend = backend_with_runtime(runtime);
    backend.list().expect("establish exact managed marker");
    let mut session = server_session(id, PathBuf::from("/tmp"));
    session.pid = Some(u32::MAX);

    backend
        .stop(&session)
        .expect("idle abort false is still an accepted response");
    let all_requests = server.wait_for_requests(8);
    assert_test_secret_pairs(&all_requests);
    let requests = authenticated_requests(&all_requests);
    let abort = requests.last().unwrap();
    assert_eq!(abort.method, "POST");
    assert_eq!(
        abort.target,
        format!("/session/{}/abort", encoded_component(id))
    );
    assert!(abort.body.is_empty());
    assert_eq!(launcher_calls.load(Ordering::SeqCst), 0);
}

#[test]
fn opencode_server_session_mutations_use_exact_paths_and_json() {
    let id = "ses/\"雪?../../target#fragment";
    let encoded = encoded_component(id);
    let server = ScriptedServer::spawn_verified(secure_responses(vec![
        healthy_response(),
        ScriptedResponse::json(
            200,
            json!([{
                "id": id,
                "parentID": null,
                "directory": "/tmp",
                "title": "managed",
                "permission": managed_permission(),
                "time": {"created": 1, "updated": 2, "archived": 3}
            }]),
        ),
        healthy_response(),
        ScriptedResponse::json(200, json!({"id": id})),
        healthy_response(),
        ScriptedResponse::json(200, json!({"id": id})),
        healthy_response(),
        ScriptedResponse::json(200, json!({"id": id})),
        healthy_response(),
        ScriptedResponse::json(200, json!(true)),
    ]));
    let launcher_calls = Arc::new(AtomicUsize::new(0));
    let launcher_calls_for_runtime = Arc::clone(&launcher_calls);
    let runtime = secure_runtime_for(
        &server,
        [server.addr, unused_addr()],
        Duration::from_millis(250),
        move |_| {
            launcher_calls_for_runtime.fetch_add(1, Ordering::SeqCst);
            Err(io::Error::other("launcher was not expected"))
        },
    );
    let mut backend = backend_with_runtime(runtime);
    backend.list().expect("establish exact managed marker");
    let session = server_session(id, PathBuf::from("/tmp"));
    let name = "quoted \"name\" with 雪 / ? and ../";

    backend.rename(&session, name).expect("rename");
    let before_archive = agent_viewer_core::spawn::now_ms();
    backend.hide(id).expect("archive");
    let after_archive = agent_viewer_core::spawn::now_ms();
    backend.unhide(id).expect("unarchive");
    backend.remove(&session).expect("delete");

    let requests = server.wait_for_requests(20);
    assert_test_secret_pairs(&requests);
    let mutations = authenticated_requests(&requests)
        .iter()
        .filter(|request| request.method != "GET")
        .copied()
        .collect::<Vec<_>>();
    assert_eq!(mutations.len(), 4);
    assert_eq!(mutations[0].method, "PATCH");
    assert_eq!(mutations[0].target, format!("/session/{encoded}"));
    assert_eq!(mutations[0].json_body(), json!({"title": name}));
    assert_eq!(mutations[1].method, "PATCH");
    assert_eq!(mutations[1].target, format!("/session/{encoded}"));
    let archive_body = mutations[1].json_body();
    let archived = archive_body
        .pointer("/time/archived")
        .and_then(Value::as_i64)
        .expect("archive timestamp");
    assert!(
        (before_archive..=after_archive).contains(&archived),
        "{archive_body}"
    );
    assert_eq!(
        archive_body.as_object().unwrap().keys().collect::<Vec<_>>(),
        vec!["time"]
    );
    assert_eq!(mutations[2].json_body(), json!({"time": {"archived": 0}}));
    assert_eq!(mutations[3].method, "DELETE");
    assert_eq!(mutations[3].target, format!("/session/{encoded}"));
    assert!(mutations[3].body.is_empty());
    assert_eq!(launcher_calls.load(Ordering::SeqCst), 0);
}

// --- run-mode companions: `opencode run` sessions are one-shots, not fleet members ---

#[test]
fn opencode_run_mode_permission_marks_companion() {
    let schema = common::read_fixture("opencode_session_schema.sql");
    // The exact blob `opencode run` writes (verified live on this box, opencode 1.17.20:
    // a `run` session stores this triple, a TUI session stores NULL).
    let run_perm = "[{\"permission\":\"question\",\"pattern\":\"*\",\"action\":\"deny\"},\
                    {\"permission\":\"plan_enter\",\"pattern\":\"*\",\"action\":\"deny\"},\
                    {\"permission\":\"plan_exit\",\"pattern\":\"*\",\"action\":\"deny\"}]";
    let inserts = [
        // TUI session: no parent, no permission override -> a real fleet row.
        "INSERT INTO session (id, parent_id, directory, title, time_created, time_updated, time_archived, permission) \
         VALUES ('ses_tui',NULL,'/home/user/oc-proj','Interactive',1000,5000,NULL,NULL)"
            .to_string(),
        // `opencode run` one-shot (an /implement review pass): no parent, so parent_id
        // alone would have shown it.
        format!(
            "INSERT INTO session (id, parent_id, directory, title, time_created, time_updated, time_archived, permission) \
             VALUES ('ses_run',NULL,'/home/user/oc-proj','CUR-1667 billing bug fix review',1000,4000,NULL,'{run_perm}')"
        ),
        // A permission override that is NOT the run marker must stay visible.
        "INSERT INTO session (id, parent_id, directory, title, time_created, time_updated, time_archived, permission) \
         VALUES ('ses_other',NULL,'/home/user/oc-proj','Custom perms',1000,3000,NULL,'[{\"permission\":\"read\",\"pattern\":\"*\",\"action\":\"allow\"}]')"
            .to_string(),
        // Empty string (not NULL) is the same as no override.
        "INSERT INTO session (id, parent_id, directory, title, time_created, time_updated, time_archived, permission) \
         VALUES ('ses_empty',NULL,'/home/user/oc-proj','Empty perms',1000,2000,NULL,'')"
            .to_string(),
    ];
    let refs: Vec<&str> = inserts.iter().map(String::as_str).collect();
    let (_dir, path) = common::temp_db(&schema, &refs);

    let mut backend = OpencodeBackend::with_db(path);
    let sessions = backend.list().expect("list opencode sessions");
    let by = |id: &str| {
        sessions
            .iter()
            .find(|s| s.id == id)
            .unwrap_or_else(|| panic!("{id} missing"))
            .companion
    };

    assert!(!by("ses_tui"), "interactive TUI session must stay visible");
    assert!(by("ses_run"), "`opencode run` one-shot must be a companion");
    assert!(
        !by("ses_other"),
        "unrelated permission override is not a run marker"
    );
    assert!(!by("ses_empty"), "empty permission is not a run marker");
}

#[test]
fn opencode_run_mode_permission_marker_shapes() {
    // Absent / empty -> interactive.
    assert!(!is_run_mode_permission(None));
    assert!(!is_run_mode_permission(Some("")));
    assert!(!is_run_mode_permission(Some("   ")));
    // Not JSON, or JSON of the wrong shape -> interactive (never panics).
    assert!(!is_run_mode_permission(Some("not json at all")));
    assert!(!is_run_mode_permission(Some("{}")));
    assert!(!is_run_mode_permission(Some("[]")));
    assert!(!is_run_mode_permission(Some(
        "[{\"permission\":\"question\"}]"
    )));
    // A `question` entry that is allowed, not denied -> interactive.
    assert!(!is_run_mode_permission(Some(
        "[{\"permission\":\"question\",\"pattern\":\"*\",\"action\":\"allow\"}]"
    )));
    // The stored key order is not the source order, so order must not matter.
    assert!(is_run_mode_permission(Some(
        "[{\"action\":\"deny\",\"permission\":\"question\",\"pattern\":\"*\"}]"
    )));
    // The github-action variant writes the `question` deny alone, without the plan pair.
    assert!(is_run_mode_permission(Some(
        "[{\"permission\":\"question\",\"pattern\":\"*\",\"action\":\"deny\"}]"
    )));
    // Extra unrelated entries around the marker must not hide it.
    assert!(is_run_mode_permission(Some(
        "[{\"permission\":\"read\",\"pattern\":\"*\",\"action\":\"allow\"},\
          {\"permission\":\"question\",\"pattern\":\"*\",\"action\":\"deny\"}]"
    )));
}

#[test]
fn secure_transport_verifies_before_auth_and_accepts_bodyless_204() {
    let server = ScriptedServer::spawn_verified(secure_responses(vec![
        healthy_response(),
        ScriptedResponse::json(200, json!({"id": "ses_secure"})),
        ScriptedResponse::empty(204).without_framing(),
    ]));
    let runtime = secure_runtime_for(
        &server,
        [server.addr, unused_addr()],
        Duration::from_millis(300),
        |_| Err(io::Error::other("launcher was not expected")),
    );
    let backend = backend_with_runtime(runtime);

    let spawned = backend
        .spawn(PathBuf::from("/tmp").as_path(), "secure prompt", None, None)
        .expect("secure spawn");
    assert_eq!(spawned.session_id.as_deref(), Some("ses_secure"));

    let requests = server.wait_for_requests(6);
    assert_eq!(requests.len(), 6);
    assert_test_secret_pairs(&requests);
    assert_eq!(
        requests[3].json_body().get("permission"),
        Some(&managed_permission()),
        "spawn writes the exact durable permission marker"
    );
    assert!(
        requests[3].json_body().get("metadata").is_none(),
        "metadata is not a supported create field"
    );
}

#[test]
fn secure_health_uses_startup_budget_for_delayed_authenticated_response() {
    let mut delayed_health = healthy_response();
    delayed_health.delay = Duration::from_millis(300);
    let server = ScriptedServer::spawn_verified(secure_responses(vec![
        delayed_health,
        ScriptedResponse::json(200, json!({"id": "ses_slow_health"})),
        ScriptedResponse::empty(204).without_framing(),
    ]));
    let runtime = secure_runtime_for(
        &server,
        [server.addr, unused_addr()],
        Duration::from_secs(1),
        |_| Err(io::Error::other("launcher was not expected")),
    );
    let backend = backend_with_runtime(runtime);

    let spawned = backend
        .spawn(
            PathBuf::from("/tmp/slow_health").as_path(),
            "slow health",
            None,
            None,
        )
        .expect("300 ms authenticated health fits one second startup budget");
    assert_eq!(spawned.session_id.as_deref(), Some("ses_slow_health"));

    let requests = server.wait_for_requests(6);
    assert_eq!(requests.len(), 6);
    assert_test_secret_pairs(&requests);
}

#[test]
fn deferred_accept_preflights_then_authenticates_on_the_same_connection() {
    let server = ScriptedServer::spawn_verified_deferred(vec![
        unauthorized_health().keep_alive(),
        healthy_response(),
        unauthorized_health().keep_alive(),
        ScriptedResponse::json(200, json!({"id": "ses_deferred"})),
        unauthorized_health().keep_alive(),
        ScriptedResponse::empty(204).without_framing(),
    ]);
    let runtime = secure_runtime_for(
        &server,
        [server.addr, unused_addr()],
        Duration::from_millis(500),
        |_| Err(io::Error::other("launcher was not expected")),
    );
    let backend = backend_with_runtime(runtime);

    let spawned = backend
        .spawn(
            PathBuf::from("/tmp/deferred").as_path(),
            "deferred body",
            None,
            None,
        )
        .expect("deferred accept spawn");
    assert_eq!(spawned.session_id.as_deref(), Some("ses_deferred"));

    let requests = server.wait_for_requests(6);
    assert_eq!(requests.len(), 6);
    let expected = format!(
        "Basic {}",
        base64::engine::general_purpose::STANDARD.encode(b"agent-viewer:test-secret")
    );
    for pair in requests.chunks_exact(2) {
        assert_eq!(pair[0].connection_id, pair[1].connection_id);
        assert_eq!(pair[0].method, "GET");
        assert_eq!(pair[0].target, "/global/health");
        assert!(!pair[0].headers.contains_key("authorization"));
        assert!(pair[0].body.is_empty());
        assert_eq!(pair[1].headers.get("authorization"), Some(&expected));
    }
    assert_eq!(
        requests
            .iter()
            .map(|request| request.connection_id)
            .collect::<Vec<_>>(),
        vec![1, 1, 2, 2, 3, 3]
    );
    assert_eq!(requests[2].target, "/global/health");
    assert!(requests[3].target.starts_with("/session?directory="));
    assert_eq!(
        requests[3].json_body().get("permission"),
        Some(&managed_permission())
    );
    assert_eq!(requests[4].target, "/global/health");
    assert!(
        requests[5]
            .target
            .starts_with("/session/ses_deferred/prompt_async?directory=")
    );
    assert_eq!(
        requests[5].json_body(),
        json!({"parts": [{"type": "text", "text": "deferred body"}]})
    );
}

#[test]
fn secure_http_accepts_chunked_and_rejects_unframed_body_redirects_and_large_headers() {
    let chunked = ScriptedServer::spawn_verified(secure_responses(vec![
        healthy_response(),
        ScriptedResponse::json(200, json!([])).chunked(),
    ]));
    let mut backend = backend_with_runtime(secure_runtime_for(
        &chunked,
        [chunked.addr, unused_addr()],
        Duration::from_millis(300),
        |_| Err(io::Error::other("launcher was not expected")),
    ));
    assert!(backend.list().expect("chunked global list").is_empty());
    assert_test_secret_pairs(&chunked.wait_for_requests(4));

    for (response, expected) in [
        (
            ScriptedResponse::json(200, json!([])).without_framing(),
            "framing",
        ),
        (
            ScriptedResponse::json(200, json!([])).header("X-Oversized", &"x".repeat(70_000)),
            "header",
        ),
        (
            ScriptedResponse::raw(200, "application/json", "x".repeat(20_000_000)),
            "body",
        ),
    ] {
        let server =
            ScriptedServer::spawn_verified(secure_responses(vec![healthy_response(), response]));
        let mut backend = backend_with_runtime(secure_runtime_for(
            &server,
            [server.addr, unused_addr()],
            Duration::from_millis(300),
            |_| Err(io::Error::other("launcher was not expected")),
        ));
        let error = backend.list().expect_err("unsafe HTTP response must fail");
        assert_test_secret_pairs(&server.wait_for_requests(4));
        let message = error.to_string().to_ascii_lowercase();
        assert!(message.contains(expected), "{message}");
    }

    let redirect = ScriptedServer::spawn_verified(secure_responses(vec![
        healthy_response(),
        ScriptedResponse::empty(302).header("Location", "http://127.0.0.1:9/stolen"),
    ]));
    let mut backend = backend_with_runtime(secure_runtime_for(
        &redirect,
        [redirect.addr, unused_addr()],
        Duration::from_millis(300),
        |_| Err(io::Error::other("launcher was not expected")),
    ));
    let message = backend
        .list()
        .expect_err("redirect response must fail")
        .to_string();
    assert_test_secret_pairs(&redirect.wait_for_requests(4));
    assert!(
        message.contains("GET /experimental/session?limit=10000&archived=true returned HTTP 302"),
        "{message}"
    );
    assert!(!message.contains("Location"), "{message}");
    assert!(!message.contains("stolen"), "{message}");
    assert!(!message.contains("127.0.0.1:9"), "{message}");
}

#[test]
fn process_identity_mismatches_send_no_http_bytes() {
    type ProcMutation = Box<dyn Fn(&ProcFixture)>;
    let cases: Vec<ProcMutation> = vec![
        Box::new(|process| process.write_identity(1000, unsafe { libc::geteuid() } + 1, None)),
        Box::new(|process| {
            process.write_identity(
                1000,
                unsafe { libc::geteuid() },
                Some(vec!["opencode".into(), "serve".into()]),
            )
        }),
        Box::new(|process| process.handoff_listener_inode()),
        Box::new(|process| process.remove_process()),
    ];

    for mutate in cases {
        let server = ScriptedServer::spawn_verified(vec![unauthorized_health()]);
        mutate(&server.process());
        let mut backend = backend_with_runtime(secure_runtime_for(
            &server,
            [server.addr, unused_addr()],
            Duration::from_millis(80),
            |_| Err(io::Error::other("refuse replacement start")),
        ));

        let _ = backend.list();

        assert!(
            server.requests().is_empty(),
            "wrong uid, argv, inode, or unreadable process state must send no HTTP bytes"
        );
    }
}

#[test]
fn replacement_between_connect_and_authorized_write_sends_no_credential_or_task() {
    let server = ScriptedServer::spawn_verified(vec![unauthorized_health().keep_alive()]);
    let process = server.process();
    let runtime = secure_runtime_with_hook(
        &server,
        Arc::new(move || {
            process.write_identity(2000, unsafe { libc::geteuid() }, None);
        }),
    );
    let mut backend = backend_with_runtime(runtime);

    let _ = backend.list();

    let requests = server.wait_for_requests(1);
    assert_eq!(requests.len(), 1);
    assert!(!requests[0].headers.contains_key("authorization"));
}

#[test]
fn startup_credentials_are_env_only_and_errors_are_sanitized() {
    let root = tempfile::TempDir::new().unwrap();
    std::fs::create_dir_all(root.path().join("net")).unwrap();
    std::os::unix::fs::symlink("/proc/net/tcp", root.path().join("net/tcp")).unwrap();
    let launched = Arc::new(Mutex::new(Vec::<Command>::new()));
    let launched_for_callback = Arc::clone(&launched);
    let candidates = unused_addrs();
    let runtime = OpencodeRuntime::for_test_secure(OpencodeRuntimeTestConfig {
        candidates,
        startup_timeout: Duration::from_millis(500),
        durable_cwd: PathBuf::from("/"),
        launcher: Arc::new(move |command| {
            launched_for_callback.lock().unwrap().push(command);
            Err(io::Error::other(
                "failed with test-secret and Basic YWdlbnQtdmlld2VyOnRlc3Qtc2VjcmV0",
            ))
        }),
        viewer_db_path: root.path().join("viewer.db"),
        proc_root: root.path().to_path_buf(),
        password_override: Some("test-secret".to_string()),
        before_authorized_write: None,
    });
    let backend = backend_with_runtime(runtime);

    let error = backend
        .spawn(PathBuf::from("/tmp").as_path(), "must not leak", None, None)
        .expect_err("launcher failure");
    let message = error.to_string();
    assert!(!message.contains("test-secret"), "{message}");
    assert!(!message.contains("YWdlbnQtdmlld2Vy"), "{message}");
    assert!(
        message.contains(&candidates[0].port().to_string()),
        "{message}"
    );
    assert!(
        message.contains(&candidates[1].port().to_string()),
        "{message}"
    );

    let launched = launched.lock().unwrap();
    assert_eq!(launched.len(), 2);
    for (index, command) in launched.iter().enumerate() {
        let args = command_args(command);
        assert_eq!(
            args,
            vec![
                "serve".to_string(),
                "--hostname".to_string(),
                "127.0.0.1".to_string(),
                "--port".to_string(),
                candidates[index].port().to_string(),
            ]
        );
        assert!(!args.iter().any(|arg| arg.contains("test-secret")));
        let env = command
            .get_envs()
            .map(|(key, value)| {
                (
                    key.to_string_lossy().into_owned(),
                    value.map(|value| value.to_string_lossy().into_owned()),
                )
            })
            .collect::<BTreeMap<_, _>>();
        assert_eq!(
            env.get("OPENCODE_SERVER_USERNAME"),
            Some(&Some("agent-viewer".to_string()))
        );
        assert_eq!(
            env.get("OPENCODE_SERVER_PASSWORD"),
            Some(&Some("test-secret".to_string()))
        );
    }
}

#[test]
fn viewer_server_launch_marks_its_process_and_installs_a_shell_credential_scrubber() {
    let root = tempfile::TempDir::new().unwrap();
    std::fs::create_dir_all(root.path().join("net")).unwrap();
    std::os::unix::fs::symlink("/proc/net/tcp", root.path().join("net/tcp")).unwrap();
    let existing_ancestor = root.path().join("existing");
    std::fs::create_dir(&existing_ancestor).unwrap();
    std::fs::set_permissions(&existing_ancestor, std::fs::Permissions::from_mode(0o751)).unwrap();
    let state_dir = existing_ancestor.join("missing/state");
    let launched = Arc::new(Mutex::new(Vec::<Command>::new()));
    let launched_for_callback = Arc::clone(&launched);
    let candidates = unused_addrs();
    let runtime = OpencodeRuntime::for_test_secure(OpencodeRuntimeTestConfig {
        candidates,
        startup_timeout: Duration::from_millis(500),
        durable_cwd: PathBuf::from("/"),
        launcher: Arc::new(move |command| {
            launched_for_callback.lock().unwrap().push(command);
            Err(io::Error::other(
                "launch intentionally stops after command capture",
            ))
        }),
        viewer_db_path: state_dir.join("viewer.db"),
        proc_root: root.path().to_path_buf(),
        password_override: Some("test-secret".to_string()),
        before_authorized_write: None,
    });

    let _ = backend_with_runtime(runtime).spawn(
        PathBuf::from("/tmp").as_path(),
        "must not reach a server",
        None,
        None,
    );

    assert_eq!(
        std::fs::metadata(&existing_ancestor)
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o751,
        "existing server credential state ancestors remain unchanged"
    );
    assert_eq!(
        std::fs::metadata(existing_ancestor.join("missing"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o700
    );
    assert_eq!(
        std::fs::metadata(&state_dir).unwrap().permissions().mode() & 0o777,
        0o700
    );
    for private_file in [
        state_dir.join("opencode_server.lock"),
        state_dir.join("shell.env_OPENCODE_SERVER_USERNAME_OPENCODE_SERVER_PASSWORD.js"),
    ] {
        assert_eq!(
            std::fs::metadata(private_file)
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }

    let launched = launched.lock().unwrap();
    assert_eq!(launched.len(), 2);
    for command in launched.iter() {
        let env = command
            .get_envs()
            .map(|(key, value)| {
                (
                    key.to_string_lossy().into_owned(),
                    value.map(|value| value.to_string_lossy().into_owned()),
                )
            })
            .collect::<BTreeMap<_, _>>();
        assert_eq!(
            env.get("AGENT_VIEWER_OPENCODE_SERVER"),
            Some(&Some("1".to_string()))
        );

        let config = env
            .get("OPENCODE_CONFIG_CONTENT")
            .and_then(Option::as_deref)
            .expect("viewer server launch supplies private plugin config");
        let config: Value = serde_json::from_str(config).expect("plugin config is JSON");
        assert!(
            config
                .get("plugin")
                .and_then(Value::as_array)
                .is_some_and(|plugins| !plugins.is_empty()),
            "viewer server config includes a private plugin"
        );
        let config = config.to_string();
        assert!(config.contains("shell.env"), "{config}");
        assert!(config.contains("OPENCODE_SERVER_USERNAME"), "{config}");
        assert!(config.contains("OPENCODE_SERVER_PASSWORD"), "{config}");
        assert!(!config.contains("test-secret"), "{config}");
    }
}

#[test]
fn global_pagination_exact_marker_archived_cache_and_directory_isolation() {
    let exact = managed_permission();
    let near = json!([
        {"permission": "agent-viewer.background", "pattern": "*", "action": "deny"}
    ]);
    let run_mode_compatibility = json!([
        {"permission": "question", "pattern": "*", "action": "deny"},
        {"permission": "plan_enter", "pattern": "*", "action": "deny"},
        {"permission": "plan_exit", "pattern": "*", "action": "deny"}
    ]);
    let server = ScriptedServer::spawn_verified(secure_responses(vec![
        healthy_response(),
        ScriptedResponse::json(
            200,
            json!([
                {
                    "id": "ses_good",
                    "parentID": null,
                    "directory": "/good",
                    "title": "good",
                    "permission": exact,
                    "time": {"created": 1, "updated": 5}
                },
                {
                    "id": "ses_external",
                    "parentID": null,
                    "directory": "/external",
                    "title": "near marker",
                    "permission": near,
                    "time": {"created": 1, "updated": 4}
                },
                {
                    "id": "ses_run_compatibility",
                    "parentID": null,
                    "directory": "/run_compatibility",
                    "title": "run mode compatibility",
                    "permission": run_mode_compatibility,
                    "time": {"created": 1, "updated": 4}
                }
            ]),
        )
        .header("x-next-cursor", "page two"),
        ScriptedResponse::json(
            200,
            json!([
                {
                    "id": "ses_bad",
                    "parentID": null,
                    "directory": "/bad",
                    "title": "bad directory",
                    "permission": managed_permission(),
                    "time": {"created": 1, "updated": 3}
                },
                {
                    "id": "ses_archived",
                    "parentID": null,
                    "directory": "/archived",
                    "title": "archived",
                    "permission": managed_permission(),
                    "time": {"created": 1, "updated": 2, "archived": 9}
                }
            ]),
        ),
        ScriptedResponse::raw(500, "text/plain", "bad status"),
        ScriptedResponse::json(200, json!([])),
        ScriptedResponse::json(200, json!([])),
        ScriptedResponse::json(200, json!({"ses_good": {"type": "busy"}})),
        ScriptedResponse::json(200, json!([])),
        ScriptedResponse::json(200, json!([])),
        healthy_response(),
        ScriptedResponse::json(200, json!({"id": "ses_archived"})),
        healthy_response(),
        ScriptedResponse::json(200, json!({"id": "ses_archived"})),
        healthy_response(),
        ScriptedResponse::json(200, json!({"id": "ses_good"})),
    ]));
    let runtime = secure_runtime_for(
        &server,
        [server.addr, unused_addr()],
        Duration::from_millis(400),
        |_| Err(io::Error::other("launcher was not expected")),
    );
    let mut backend = backend_with_runtime(runtime);

    let listed = backend.list().expect("isolated global list");
    let row = |id: &str| listed.iter().find(|session| session.id == id).unwrap();
    assert_eq!(row("ses_good").status, Status::Working);
    assert_eq!(row("ses_bad").status, Status::Unknown);
    assert_eq!(row("ses_external").status, Status::Idle);
    assert_eq!(row("ses_run_compatibility").status, Status::Idle);
    assert!(row("ses_good").daemon_hosted);
    assert!(row("ses_bad").daemon_hosted);
    assert!(row("ses_archived").daemon_hosted);
    assert!(!row("ses_external").daemon_hosted);
    assert!(!row("ses_run_compatibility").daemon_hosted);
    assert!(row("ses_archived").hidden);

    let requests = server.wait_for_requests(18);
    assert_test_secret_pairs(&requests);
    let targets = requests
        .chunks_exact(2)
        .map(|pair| pair[1].target.as_str())
        .collect::<Vec<_>>();
    assert_eq!(
        targets[1], "/experimental/session?limit=10000&archived=true",
        "listing starts at the global experimental route"
    );
    assert_eq!(
        targets[2],
        "/experimental/session?limit=10000&archived=true&cursor=page%20two"
    );
    assert!(targets.contains(&"/session/status?directory=%2Fbad"));
    assert!(targets.contains(&"/session/status?directory=%2Fgood"));
    assert!(
        targets.iter().all(|target| !target.contains("%2Farchived")),
        "archived managed ids remain cached without active directory snapshot probes"
    );
    assert!(
        targets.iter().all(|target| !target.contains("%2Fexternal")),
        "near marker rows remain external"
    );
    assert!(
        targets
            .iter()
            .all(|target| !target.contains("%2Frun_compatibility")),
        "run mode compatibility rows remain external"
    );

    let archived_row = row("ses_archived").clone();
    let good_row = row("ses_good").clone();
    backend
        .hide("ses_archived")
        .expect("cached archived id remains archive authorized");
    backend
        .unhide("ses_archived")
        .expect("cached archived id remains unarchive authorized");
    backend
        .rename(&good_row, "managed renamed")
        .expect("cached managed row remains rename authorized");
    let all_requests = server.wait_for_requests(30);
    assert_test_secret_pairs(&all_requests);
    let mutation_requests = all_requests
        .into_iter()
        .filter(|request| {
            request.method == "PATCH"
                && (request.target == "/session/ses_archived"
                    || request.target == "/session/ses_good")
        })
        .collect::<Vec<_>>();
    assert_eq!(mutation_requests.len(), 3);
    assert_eq!(mutation_requests[0].target, "/session/ses_archived");
    assert!(
        mutation_requests[0]
            .json_body()
            .pointer("/time/archived")
            .and_then(Value::as_i64)
            .is_some_and(|timestamp| timestamp > 0)
    );
    assert_eq!(
        mutation_requests[1].json_body(),
        json!({"time": {"archived": 0}})
    );
    assert_eq!(mutation_requests[2].target, "/session/ses_good");
    assert_eq!(
        mutation_requests[2].json_body(),
        json!({"title": "managed renamed"})
    );
    assert!(archived_row.daemon_hosted);

    let before = server.requests().len();
    assert!(
        backend.hide("ses_external").is_err(),
        "id only archive is gated by the exact managed id cache"
    );
    assert!(
        backend.unhide("ses_external").is_err(),
        "id only unarchive is gated by the exact managed id cache"
    );
    let mut forged = row("ses_external").clone();
    forged.daemon_hosted = true;
    assert!(backend.stop(&forged).is_err());
    assert!(backend.rename(&forged, "forged").is_err());
    assert!(backend.remove(&forged).is_err());
    assert_eq!(server.requests().len(), before);
}

#[test]
fn global_pagination_requests_archived_sessions_on_every_page() {
    let server = ScriptedServer::spawn_verified(secure_responses(vec![
        healthy_response(),
        ScriptedResponse::json(200, json!([])).header("x-next-cursor", "page two"),
        ScriptedResponse::json(200, json!([])),
    ]));
    let runtime = secure_runtime_for(
        &server,
        [server.addr, unused_addr()],
        Duration::from_millis(400),
        |_| Err(io::Error::other("launcher was not expected")),
    );
    let mut backend = backend_with_runtime(runtime);

    assert!(backend.list().expect("global archived list").is_empty());

    let requests = server.wait_for_requests(6);
    assert_test_secret_pairs(&requests);
    let targets = requests
        .chunks_exact(2)
        .map(|pair| pair[1].target.as_str())
        .collect::<Vec<_>>();
    assert_eq!(
        targets[1],
        "/experimental/session?limit=10000&archived=true"
    );
    assert_eq!(
        targets[2],
        "/experimental/session?limit=10000&archived=true&cursor=page%20two"
    );
}

#[test]
fn pagination_rejects_repeated_or_malformed_cursor_and_full_page_without_cursor() {
    for responses in [
        vec![
            ScriptedResponse::json(200, json!([])).header("x-next-cursor", "repeat"),
            ScriptedResponse::json(200, json!([])).header("x-next-cursor", "repeat"),
        ],
        vec![ScriptedResponse::json(200, json!([])).header("x-next-cursor", "bad cursor\tvalue")],
        vec![ScriptedResponse::json(
            200,
            Value::Array(
                (0..10_000)
                    .map(|index| {
                        json!({
                            "id": format!("external_{index}"),
                            "parentID": null,
                            "directory": "/external",
                            "title": "external",
                            "permission": null,
                            "time": {"created": 1, "updated": 1}
                        })
                    })
                    .collect(),
            ),
        )],
    ] {
        let mut actual = vec![healthy_response()];
        actual.extend(responses);
        let server = ScriptedServer::spawn_verified(secure_responses(actual));
        let mut backend = backend_with_runtime(secure_runtime_for(
            &server,
            [server.addr, unused_addr()],
            Duration::from_secs(2),
            |_| Err(io::Error::other("launcher was not expected")),
        ));

        let error = backend.list().expect_err("invalid pagination must fail");
        assert_test_secret_pairs(&server.requests());
        assert!(
            error.to_string().to_ascii_lowercase().contains("cursor"),
            "{error}"
        );
    }
}

#[test]
fn same_pinned_identity_with_failed_health_stays_pinned_and_does_not_start() {
    let server = ScriptedServer::spawn_verified(secure_responses(vec![
        healthy_response(),
        ScriptedResponse::json(200, json!([])),
        ScriptedResponse::raw(500, "text/plain", "temporarily unhealthy"),
    ]));
    let launches = Arc::new(AtomicUsize::new(0));
    let launches_for_callback = Arc::clone(&launches);
    let runtime = secure_runtime_for(
        &server,
        [server.addr, unused_addr()],
        Duration::from_millis(250),
        move |_| {
            launches_for_callback.fetch_add(1, Ordering::SeqCst);
            Err(io::Error::other("must not start"))
        },
    );
    let mut backend = backend_with_runtime(runtime);
    backend.list().expect("establish pin");

    let _ = backend.list();

    assert_eq!(launches.load(Ordering::SeqCst), 0);
    let requests = server.wait_for_requests(6);
    assert_eq!(requests.len(), 6);
    assert_test_secret_pairs(&requests);
}

#[test]
fn same_pinned_identity_recovers_after_one_transient_health_failure() {
    let server = ScriptedServer::spawn_verified(secure_responses(vec![
        healthy_response(),
        ScriptedResponse::json(200, json!([])),
        ScriptedResponse::raw(500, "text/plain", "temporarily unhealthy"),
        healthy_response(),
        ScriptedResponse::json(200, json!([])),
    ]));
    let launches = Arc::new(AtomicUsize::new(0));
    let launches_for_callback = Arc::clone(&launches);
    let runtime = secure_runtime_for(
        &server,
        [server.addr, unused_addr()],
        Duration::from_millis(250),
        move |_| {
            launches_for_callback.fetch_add(1, Ordering::SeqCst);
            Err(io::Error::other("must not start"))
        },
    );
    let mut backend = backend_with_runtime(runtime);
    backend.list().expect("establish pin");

    backend
        .list()
        .expect_err("one authenticated health request fails");
    assert!(
        backend
            .list()
            .expect("later probe retries the same pinned identity")
            .is_empty()
    );

    assert_eq!(launches.load(Ordering::SeqCst), 0);
    let requests = server.wait_for_requests(10);
    assert_eq!(requests.len(), 10);
    assert_test_secret_pairs(&requests);
}

#[test]
fn changed_pinned_identity_is_cleared_without_sending_replacement_credentials() {
    let server = ScriptedServer::spawn_verified(secure_responses(vec![
        healthy_response(),
        ScriptedResponse::json(200, json!([])),
    ]));
    let runtime = secure_runtime_for(
        &server,
        [server.addr, unused_addr()],
        Duration::from_millis(100),
        |_| Err(io::Error::other("no verified replacement")),
    );
    let mut backend = backend_with_runtime(runtime);
    backend.list().expect("establish pin");
    server
        .process()
        .write_identity(2000, unsafe { libc::geteuid() }, None);

    let _ = backend.list();

    assert_eq!(
        server.requests().len(),
        4,
        "a changed pin is rejected from process state before opening another HTTP request"
    );
    assert_test_secret_pairs(&server.requests());
}

#[test]
fn capabilities_do_not_wait_for_blocked_health_or_startup() {
    let mut delayed = unauthorized_health();
    delayed.delay = Duration::from_millis(350);
    delayed = delayed.keep_alive();
    let server = ScriptedServer::spawn_verified(vec![delayed]);
    let runtime = secure_runtime_for(
        &server,
        [server.addr, unused_addr()],
        Duration::from_millis(450),
        |_| Err(io::Error::other("startup ends after blocked health")),
    );
    let mut listing = backend_with_runtime(runtime.clone());
    let worker = std::thread::spawn(move || listing.list());
    assert_eq!(server.wait_for_requests(1).len(), 1);

    let started = Instant::now();
    let capabilities = backend_with_runtime(runtime).capabilities();
    assert!(
        started.elapsed() < Duration::from_millis(50),
        "capabilities blocked on runtime startup or HTTP"
    );
    assert!(!capabilities.live_status);
    let _ = worker.join().expect("listing thread");
    let requests = server.requests();
    assert_eq!(requests.len(), 1);
    assert!(!requests[0].headers.contains_key("authorization"));
}

#[test]
fn stale_health_completion_cannot_publish_over_a_changed_process_generation() {
    let mut delayed_health = healthy_response();
    delayed_health.delay = Duration::from_millis(180);
    let server =
        ScriptedServer::spawn_verified(vec![unauthorized_health().keep_alive(), delayed_health]);
    let runtime = secure_runtime_for(
        &server,
        [server.addr, unused_addr()],
        Duration::from_millis(300),
        |_| Err(io::Error::other("no replacement server")),
    );
    let mut listing = backend_with_runtime(runtime.clone());
    let worker = std::thread::spawn(move || listing.list());
    assert_eq!(server.wait_for_requests(2).len(), 2);
    server
        .process()
        .write_identity(2000, unsafe { libc::geteuid() }, None);

    let _ = worker.join().expect("listing worker");
    assert_test_secret_pairs(&server.requests());

    assert!(
        !backend_with_runtime(runtime.clone())
            .capabilities()
            .live_status,
        "the delayed health result belongs to the old process generation"
    );
    let mut second = backend_with_runtime(runtime);
    let before_later_probe = server.requests().len();
    let _ = second.list();
    let requests = server.requests();
    for request in &requests[before_later_probe..] {
        assert!(
            !request.headers.contains_key("authorization"),
            "the changed process receives no later credential"
        );
        assert!(
            request.body.is_empty(),
            "the changed process receives no later request body"
        );
    }
}

#[test]
fn independently_created_runtimes_converge_on_one_secure_server_and_secret() {
    let proc_root = new_proc_root();
    let db_path = proc_root.path().join("viewer.db");
    let primary = unused_addr();
    let backup = ScriptedServer::spawn(vec![]);
    let launch_gate = Arc::new((Mutex::new((0_usize, false)), std::sync::Condvar::new()));
    let launch_attempts = Arc::new(AtomicUsize::new(0));
    let successful_launches = Arc::new(AtomicUsize::new(0));
    let launched_server = Arc::new(Mutex::new(None::<ScriptedServer>));
    let launched_pid = 62001;
    let make_runtime = || {
        let proc_root_for_launcher = Arc::clone(&proc_root);
        let gate = Arc::clone(&launch_gate);
        let attempts = Arc::clone(&launch_attempts);
        let successes = Arc::clone(&successful_launches);
        let server_slot = Arc::clone(&launched_server);
        OpencodeRuntime::for_test_secure(OpencodeRuntimeTestConfig {
            candidates: [primary, backup.addr],
            startup_timeout: Duration::from_secs(2),
            durable_cwd: PathBuf::from("/"),
            launcher: Arc::new(move |_| {
                attempts.fetch_add(1, Ordering::SeqCst);
                let (lock, ready) = &*gate;
                let mut state = lock.lock().unwrap();
                state.0 += 1;
                ready.notify_all();
                let (state, timeout) = ready
                    .wait_timeout_while(state, Duration::from_secs(1), |state| !state.1)
                    .unwrap();
                assert!(!timeout.timed_out(), "first launch is released by the test");
                drop(state);
                let created = Arc::new(AtomicUsize::new(0));
                let created_for_route = Arc::clone(&created);
                match ScriptedServer::spawn_on_verified_routed_in(
                    primary,
                    Arc::clone(&proc_root_for_launcher),
                    launched_pid,
                    move |request| {
                        let authorized = request.headers.contains_key("authorization");
                        match (request.method.as_str(), request.target.as_str(), authorized) {
                            ("GET", "/global/health", false) => unauthorized_health().keep_alive(),
                            ("GET", "/global/health", true) => healthy_response(),
                            ("POST", target, true) if target.starts_with("/session?directory=") => {
                                let index = created_for_route.fetch_add(1, Ordering::SeqCst);
                                match index {
                                    0 => ScriptedResponse::json(200, json!({"id": "ses_race_one"})),
                                    1 => ScriptedResponse::json(200, json!({"id": "ses_race_two"})),
                                    _ => {
                                        ScriptedResponse::raw(500, "text/plain", "duplicate create")
                                    }
                                }
                            }
                            ("POST", target, true)
                                if target.contains("/prompt_async?directory=") =>
                            {
                                ScriptedResponse::empty(204).without_framing()
                            }
                            (_, _, false) => unauthorized_health().keep_alive(),
                            _ => ScriptedResponse::raw(500, "text/plain", "unexpected request"),
                        }
                    },
                ) {
                    Ok(server) => {
                        successes.fetch_add(1, Ordering::SeqCst);
                        *server_slot.lock().unwrap() = Some(server);
                        Ok(launched_pid)
                    }
                    Err(error) if error.kind() == io::ErrorKind::AddrInUse => {
                        let started = Instant::now();
                        while started.elapsed() < Duration::from_secs(1) {
                            if server_slot.lock().unwrap().is_some() {
                                break;
                            }
                            std::thread::sleep(Duration::from_millis(2));
                        }
                        Err(error)
                    }
                    Err(error) => Err(error),
                }
            }),
            viewer_db_path: db_path.clone(),
            proc_root: proc_root.path().to_path_buf(),
            password_override: None,
            before_authorized_write: None,
        })
    };
    let first = make_runtime();
    let second = make_runtime();
    let first_worker = std::thread::spawn(move || {
        backend_with_runtime(first).spawn(
            PathBuf::from("/race_one").as_path(),
            "run sleep 20",
            None,
            None,
        )
    });
    let second_worker = std::thread::spawn(move || {
        backend_with_runtime(second).spawn(
            PathBuf::from("/race_two").as_path(),
            "run sleep 20",
            None,
            None,
        )
    });
    let (lock, ready) = &*launch_gate;
    let state = lock.lock().unwrap();
    let (mut state, timeout) = ready
        .wait_timeout_while(state, Duration::from_millis(800), |state| state.0 < 1)
        .unwrap();
    assert!(
        !timeout.timed_out(),
        "the lock holder reaches the launch callback before the test releases it"
    );
    assert_eq!(
        state.0, 1,
        "only the lock holder reaches the launch callback"
    );
    let (next_state, timeout) = ready
        .wait_timeout_while(state, Duration::from_millis(200), |state| state.0 < 2)
        .unwrap();
    state = next_state;
    assert!(timeout.timed_out(), "the second caller must wait locally");
    assert_eq!(
        state.0, 1,
        "only the lock holder reaches the launch callback"
    );
    state.1 = true;
    ready.notify_all();
    drop(state);

    let first_spawn = first_worker.join().unwrap().expect("first exact spawn");
    let second_spawn = second_worker.join().unwrap().expect("second exact spawn");
    let spawn_ids = [
        first_spawn.session_id.expect("first session id"),
        second_spawn.session_id.expect("second session id"),
    ]
    .into_iter()
    .collect::<std::collections::BTreeSet<_>>();

    assert_eq!(launch_attempts.load(Ordering::SeqCst), 1);
    assert_eq!(successful_launches.load(Ordering::SeqCst), 1);
    assert_eq!(
        spawn_ids,
        ["ses_race_one".to_string(), "ses_race_two".to_string()]
            .into_iter()
            .collect()
    );
    let server_guard = launched_server.lock().unwrap();
    let server = server_guard.as_ref().expect("one verified launch winner");
    let requests = server.wait_for_requests(12);
    for pair in requests.chunks_exact(2) {
        assert_eq!(pair[0].connection_id, pair[1].connection_id);
        assert_eq!(pair[0].method, "GET");
        assert_eq!(pair[0].target, "/global/health");
        assert!(!pair[0].headers.contains_key("authorization"));
        assert!(pair[1].headers.contains_key("authorization"));
    }
    assert_eq!(
        requests
            .iter()
            .filter(|request| request.method == "POST"
                && request.target.starts_with("/session?directory="))
            .count(),
        2
    );
    assert_eq!(
        requests
            .iter()
            .filter(|request| request.target.contains("/prompt_async?directory="))
            .count(),
        2
    );
    let authorized = requests
        .iter()
        .filter_map(|request| request.headers.get("authorization"))
        .collect::<Vec<_>>();
    assert_eq!(authorized.len(), 6);
    assert!(authorized.iter().all(|value| *value == authorized[0]));
}

#[test]
fn detached_launch_loser_adopts_the_exact_verified_server_that_won_the_bind() {
    let proc_root = new_proc_root();
    let primary = unused_addr();
    let backup = ScriptedServer::spawn(vec![]);
    let launch_gate = Arc::new((Mutex::new((0_usize, false)), Condvar::new()));
    let launch_attempts = Arc::new(AtomicUsize::new(0));
    let successful_launches = Arc::new(AtomicUsize::new(0));
    let launched_server = Arc::new(Mutex::new(None::<ScriptedServer>));
    let make_runtime = |viewer_db_path: PathBuf| {
        let proc_root_for_launcher = Arc::clone(&proc_root);
        let gate = Arc::clone(&launch_gate);
        let attempts = Arc::clone(&launch_attempts);
        let successes = Arc::clone(&successful_launches);
        let server_slot = Arc::clone(&launched_server);
        OpencodeRuntime::for_test_secure(OpencodeRuntimeTestConfig {
            candidates: [primary, backup.addr],
            startup_timeout: Duration::from_secs(2),
            durable_cwd: PathBuf::from("/"),
            launcher: Arc::new(move |_| {
                let launch_index = attempts.fetch_add(1, Ordering::SeqCst);
                let launched_pid = 62101 + launch_index as u32;
                let (lock, ready) = &*gate;
                let mut state = lock.lock().unwrap();
                state.0 += 1;
                ready.notify_all();
                let (state, _) = ready
                    .wait_timeout_while(state, Duration::from_secs(1), |state| !state.1)
                    .unwrap();
                drop(state);
                let created = Arc::new(AtomicUsize::new(0));
                let created_for_route = Arc::clone(&created);
                match ScriptedServer::spawn_on_verified_routed_in(
                    primary,
                    Arc::clone(&proc_root_for_launcher),
                    launched_pid,
                    move |request| {
                        let authorized = request.headers.contains_key("authorization");
                        match (request.method.as_str(), request.target.as_str(), authorized) {
                            ("GET", "/global/health", false) => unauthorized_health().keep_alive(),
                            ("GET", "/global/health", true) => healthy_response(),
                            ("POST", target, true) if target.starts_with("/session?directory=") => {
                                let index = created_for_route.fetch_add(1, Ordering::SeqCst);
                                ScriptedResponse::json(
                                    200,
                                    json!({"id": format!("ses_detached_{index}")}),
                                )
                            }
                            ("POST", target, true)
                                if target.contains("/prompt_async?directory=") =>
                            {
                                ScriptedResponse::empty(204).without_framing()
                            }
                            (_, _, false) => unauthorized_health().keep_alive(),
                            _ => ScriptedResponse::raw(500, "text/plain", "unexpected request"),
                        }
                    },
                ) {
                    Ok(server) => {
                        successes.fetch_add(1, Ordering::SeqCst);
                        *server_slot.lock().unwrap() = Some(server);
                    }
                    Err(error) if error.kind() == io::ErrorKind::AddrInUse => {
                        let started = Instant::now();
                        while started.elapsed() < Duration::from_millis(250) {
                            if server_slot.lock().unwrap().is_some() {
                                break;
                            }
                            std::thread::sleep(Duration::from_millis(2));
                        }
                    }
                    Err(error) => return Err(error),
                }
                Ok(launched_pid)
            }),
            viewer_db_path,
            proc_root: proc_root.path().to_path_buf(),
            password_override: Some("test-secret".to_string()),
            before_authorized_write: None,
        })
    };
    let first = make_runtime(proc_root.path().join("first/viewer.db"));
    let second = make_runtime(proc_root.path().join("second/viewer.db"));
    let first_worker = std::thread::spawn(move || {
        backend_with_runtime(first).spawn(
            PathBuf::from("/detached_one").as_path(),
            "run sleep 20",
            None,
            None,
        )
    });
    let second_worker = std::thread::spawn(move || {
        backend_with_runtime(second).spawn(
            PathBuf::from("/detached_two").as_path(),
            "run sleep 20",
            None,
            None,
        )
    });
    let (lock, ready) = &*launch_gate;
    let state = lock.lock().unwrap();
    let (mut state, _) = ready
        .wait_timeout_while(state, Duration::from_millis(800), |state| state.0 < 2)
        .unwrap();
    assert_eq!(state.0, 2, "both detached launches reached the bind race");
    state.1 = true;
    ready.notify_all();
    drop(state);

    let first_spawn = first_worker.join().unwrap().expect("first exact spawn");
    let second_spawn = second_worker.join().unwrap().expect("second exact spawn");
    let spawn_ids = [
        first_spawn.session_id.expect("first session id"),
        second_spawn.session_id.expect("second session id"),
    ]
    .into_iter()
    .collect::<std::collections::BTreeSet<_>>();

    assert_eq!(launch_attempts.load(Ordering::SeqCst), 2);
    assert_eq!(successful_launches.load(Ordering::SeqCst), 1);
    assert_eq!(
        spawn_ids,
        ["ses_detached_0".to_string(), "ses_detached_1".to_string()]
            .into_iter()
            .collect()
    );
    let server_guard = launched_server.lock().unwrap();
    let server = server_guard.as_ref().expect("one verified launch winner");
    let requests = server.wait_for_requests(12);
    assert_eq!(
        requests
            .iter()
            .filter(|request| request.method == "POST"
                && request.target.starts_with("/session?directory="))
            .count(),
        2
    );
    assert_eq!(
        requests
            .iter()
            .filter(|request| request.target.contains("/prompt_async?directory="))
            .count(),
        2
    );
}

#[test]
fn cloned_runtime_concurrent_discovery_publishes_one_identical_pin() {
    let server = ScriptedServer::spawn_verified_concurrent_routed(|request| {
        let authorized = request.headers.contains_key("authorization");
        match (request.method.as_str(), request.target.as_str(), authorized) {
            ("GET", "/global/health", false) => unauthorized_health().keep_alive(),
            ("GET", "/global/health", true) => healthy_response(),
            ("GET", "/experimental/session?limit=10000&archived=true", true) => {
                ScriptedResponse::json(200, json!([]))
            }
            ("POST", target, true) if target.starts_with("/session?directory=") => {
                ScriptedResponse::json(200, json!({"id": "ses_shared_pin"}))
            }
            ("POST", target, true) if target.contains("/prompt_async?directory=") => {
                ScriptedResponse::empty(204).without_framing()
            }
            (_, _, false) => unauthorized_health().keep_alive(),
            _ => ScriptedResponse::raw(500, "text/plain", "unexpected request"),
        }
    });
    let authorization_gate = Arc::new((Mutex::new(0_usize), Condvar::new()));
    let gate_for_hook = Arc::clone(&authorization_gate);
    let hook = Arc::new(move || {
        let (lock, ready) = &*gate_for_hook;
        let mut arrivals = lock.lock().unwrap();
        if *arrivals < 2 {
            *arrivals += 1;
            ready.notify_all();
            let (next, timeout) = ready
                .wait_timeout_while(arrivals, Duration::from_secs(1), |count| *count < 2)
                .unwrap();
            arrivals = next;
            assert!(!timeout.timed_out(), "both discoveries reach authorization");
        }
        drop(arrivals);
    });
    let launches = Arc::new(AtomicUsize::new(0));
    let launches_for_callback = Arc::clone(&launches);
    let runtime = OpencodeRuntime::for_test_secure(OpencodeRuntimeTestConfig {
        candidates: [server.addr, unused_addr()],
        startup_timeout: Duration::from_millis(500),
        durable_cwd: PathBuf::from("/"),
        launcher: Arc::new(move |_| {
            launches_for_callback.fetch_add(1, Ordering::SeqCst);
            Err(io::Error::other("launcher was not expected"))
        }),
        viewer_db_path: server.proc_root().join("viewer.db"),
        proc_root: server.proc_root(),
        password_override: Some("test-secret".to_string()),
        before_authorized_write: Some(hook),
    });

    let listing_runtime = runtime.clone();
    let listing = std::thread::spawn(move || backend_with_runtime(listing_runtime).list());
    let spawning = std::thread::spawn(move || {
        backend_with_runtime(runtime).spawn(
            PathBuf::from("/shared_pin").as_path(),
            "shared pin",
            None,
            None,
        )
    });

    assert!(
        listing
            .join()
            .expect("listing thread")
            .expect("concurrent listing")
            .is_empty()
    );
    assert_eq!(
        spawning
            .join()
            .expect("spawning thread")
            .expect("concurrent spawn")
            .session_id
            .as_deref(),
        Some("ses_shared_pin")
    );
    assert_eq!(launches.load(Ordering::SeqCst), 0);

    let requests = server.wait_for_requests(10);
    assert_eq!(requests.len(), 10);
    let mut requests_by_connection = BTreeMap::<usize, Vec<&RecordedRequest>>::new();
    for request in &requests {
        requests_by_connection
            .entry(request.connection_id)
            .or_default()
            .push(request);
    }
    assert_eq!(requests_by_connection.len(), 5);
    let expected = format!(
        "Basic {}",
        base64::engine::general_purpose::STANDARD.encode(b"agent-viewer:test-secret")
    );
    for connection_requests in requests_by_connection.values() {
        let [preflight, authorized] = connection_requests.as_slice() else {
            panic!("each connection carries one preflight and one authorized request");
        };
        assert_eq!(preflight.method, "GET");
        assert_eq!(preflight.target, "/global/health");
        assert!(!preflight.headers.contains_key("authorization"));
        assert!(preflight.body.is_empty());
        assert_eq!(authorized.headers.get("authorization"), Some(&expected));
    }
}

#[test]
fn readiness_rejects_a_server_not_owned_by_the_exact_launched_pid() {
    let proc_root = new_proc_root();
    let primary = unused_addr();
    let launched_server = Arc::new(Mutex::new(None));
    let server_for_launcher = Arc::clone(&launched_server);
    let root_for_launcher = Arc::clone(&proc_root);
    let runtime = OpencodeRuntime::for_test_secure(OpencodeRuntimeTestConfig {
        candidates: [primary, unused_addr()],
        startup_timeout: Duration::from_millis(250),
        durable_cwd: PathBuf::from("/"),
        launcher: Arc::new(move |_| {
            let server = ScriptedServer::spawn_on_verified_in(
                primary,
                vec![unauthorized_health()],
                Arc::clone(&root_for_launcher),
                63002,
            )?;
            *server_for_launcher.lock().unwrap() = Some(server);
            Ok(63001)
        }),
        viewer_db_path: proc_root.path().join("viewer.db"),
        proc_root: proc_root.path().to_path_buf(),
        password_override: Some("test-secret".to_string()),
        before_authorized_write: None,
    });
    let backend = backend_with_runtime(runtime);

    backend
        .spawn(
            PathBuf::from("/wrong_pid").as_path(),
            "must not submit",
            None,
            None,
        )
        .expect_err("readiness rejects the wrong launched pid");

    let guard = launched_server.lock().unwrap();
    let server = guard.as_ref().expect("fake server started");
    assert!(
        server
            .requests()
            .iter()
            .all(|request| !request.headers.contains_key("authorization")
                && request.method != "POST"),
        "a server not owned by the launched pid receives neither credentials nor work"
    );
}

#[test]
fn occupied_insecure_primary_is_untouched_while_verified_backup_is_reused() {
    let primary = ScriptedServer::spawn(vec![ScriptedResponse::json(
        200,
        json!({"healthy": true, "version": "insecure"}),
    )]);
    let backup = ScriptedServer::spawn_verified(secure_responses(vec![
        healthy_response(),
        ScriptedResponse::json(200, json!([])),
    ]));
    let runtime = secure_runtime_for(
        &backup,
        [primary.addr, backup.addr],
        Duration::from_millis(300),
        |_| Err(io::Error::other("launcher was not expected")),
    );
    let mut backend = backend_with_runtime(runtime);

    assert!(backend.list().expect("secure backup list").is_empty());

    assert!(
        primary.requests().is_empty(),
        "the unrelated primary receives neither a health request nor a credential"
    );
    let requests = backup.wait_for_requests(4);
    assert_eq!(requests.len(), 4);
    assert_test_secret_pairs(&requests);
}

#[test]
fn secret_storage_failure_refuses_startup_before_launcher() {
    let root = tempfile::TempDir::new().unwrap();
    std::fs::create_dir_all(root.path().join("net")).unwrap();
    std::os::unix::fs::symlink("/proc/net/tcp", root.path().join("net/tcp")).unwrap();
    let launches = Arc::new(AtomicUsize::new(0));
    let launches_for_callback = Arc::clone(&launches);
    let runtime = OpencodeRuntime::for_test_secure(OpencodeRuntimeTestConfig {
        candidates: unused_addrs(),
        startup_timeout: Duration::from_millis(80),
        durable_cwd: PathBuf::from("/"),
        launcher: Arc::new(move |_| {
            launches_for_callback.fetch_add(1, Ordering::SeqCst);
            Ok(4242)
        }),
        viewer_db_path: root.path().to_path_buf(),
        proc_root: root.path().to_path_buf(),
        password_override: None,
        before_authorized_write: None,
    });
    let backend = backend_with_runtime(runtime);

    let error = backend
        .spawn(
            PathBuf::from("/tmp").as_path(),
            "must not launch",
            None,
            None,
        )
        .expect_err("secret storage failure");

    assert_eq!(launches.load(Ordering::SeqCst), 0);
    assert!(
        error
            .to_string()
            .to_ascii_lowercase()
            .contains("credential")
            || error.to_string().to_ascii_lowercase().contains("secret"),
        "{error}"
    );
}

#[test]
fn rejected_candidate_is_retried_by_a_later_independent_listing_probe() {
    let server = ScriptedServer::spawn_verified(secure_responses(vec![
        unauthorized_health(),
        healthy_response(),
        ScriptedResponse::json(
            200,
            json!([{
                "id": "ses_retried_candidate",
                "parentID": null,
                "directory": "/tmp",
                "title": "retried",
                "permission": null,
                "time": {"created": 1, "updated": 2}
            }]),
        ),
    ]));
    let missing_db = tempfile::TempDir::new().unwrap().path().join("missing.db");
    let mut backend = backend_with_db_and_runtime(
        missing_db,
        secure_runtime_for(
            &server,
            [server.addr, unused_addr()],
            Duration::from_millis(250),
            |_| Err(io::Error::other("launcher was not expected")),
        ),
    );

    backend
        .list()
        .expect_err("the first credential rejection remains visible");
    let listed = backend
        .list()
        .expect("a later listing retries the exact candidate");

    assert_eq!(
        listed
            .iter()
            .map(|session| session.id.as_str())
            .collect::<Vec<_>>(),
        vec!["ses_retried_candidate"]
    );
    let requests = server.wait_for_requests(6);
    assert_eq!(requests.len(), 6);
    assert_test_secret_pairs(&requests);
}

#[test]
fn http_error_body_never_leaks_credentials_or_terminal_controls() {
    let authorization = format!(
        "Basic {}",
        base64::engine::general_purpose::STANDARD.encode(b"agent-viewer:test-secret")
    );
    let response_body =
        format!("test-secret {authorization}\u{1b}]8;;https://example.test\u{7}\n\u{7f}\u{9b}");
    let server = ScriptedServer::spawn_verified(secure_responses(vec![
        healthy_response(),
        ScriptedResponse::json(
            200,
            json!([{
                "id": "ses_error_sanitized",
                "parentID": null,
                "directory": "/tmp",
                "title": "managed",
                "permission": managed_permission(),
                "time": {"created": 1, "updated": 2, "archived": 3}
            }]),
        ),
        healthy_response(),
        ScriptedResponse::raw(500, "text/plain", response_body),
    ]));
    let mut backend = backend_with_runtime(secure_runtime_for(
        &server,
        [server.addr, unused_addr()],
        Duration::from_millis(250),
        |_| Err(io::Error::other("launcher was not expected")),
    ));
    let managed = backend
        .list()
        .expect("establish managed id")
        .pop()
        .expect("managed row");

    let error = backend
        .rename(&managed, "must not leak")
        .expect_err("HTTP failure remains visible");
    let message = error.to_string();
    assert!(
        message.contains("PATCH /session/ses_error_sanitized"),
        "{message}"
    );
    assert!(message.contains("HTTP 500"), "{message}");
    assert!(!message.contains("test-secret"), "{message}");
    assert!(!message.contains(&authorization), "{message}");
    assert!(!message.contains('\u{1b}'), "{message}");
    assert!(!message.contains("]8;;"), "{message}");
    assert!(!message.contains('\n'), "{message}");
    assert!(!message.contains('\u{7f}'), "{message}");
    assert!(!message.contains('\u{9b}'), "{message}");
}

#[test]
fn delayed_old_listing_cannot_authorize_a_mutation_after_replacement_generation() {
    let proc_root = new_proc_root();
    let mut old_listing = ScriptedResponse::json(
        200,
        json!([{
            "id": "ses_old_generation",
            "parentID": null,
            "directory": "/old",
            "title": "old generation",
            "permission": managed_permission(),
            "time": {"created": 1, "updated": 2}
        }]),
    );
    old_listing.delay = Duration::from_millis(300);
    let old = ScriptedServer::spawn_on_verified_in(
        unused_addr(),
        secure_responses(vec![healthy_response(), old_listing]),
        Arc::clone(&proc_root),
        68001,
    )
    .expect("bind old generation server");
    let replacement = ScriptedServer::spawn_on_verified_routed_in(
        unused_addr(),
        Arc::clone(&proc_root),
        68002,
        |request| {
            let authorized = request.headers.contains_key("authorization");
            match (request.method.as_str(), request.target.as_str(), authorized) {
                ("GET", "/global/health", false) => unauthorized_health().keep_alive(),
                ("GET", "/global/health", true) => healthy_response(),
                ("GET", "/experimental/session?limit=10000&archived=true", true) => {
                    ScriptedResponse::json(200, json!([]))
                }
                ("PATCH", "/session/ses_old_generation", true) => {
                    ScriptedResponse::json(200, json!({"id": "ses_old_generation"}))
                }
                _ => ScriptedResponse::raw(500, "text/plain", "unexpected request"),
            }
        },
    )
    .expect("bind replacement generation server");
    let runtime = OpencodeRuntime::for_test_secure(OpencodeRuntimeTestConfig {
        candidates: [old.addr, replacement.addr],
        startup_timeout: Duration::from_millis(500),
        durable_cwd: PathBuf::from("/"),
        launcher: Arc::new(|_| Err(io::Error::other("launcher was not expected"))),
        viewer_db_path: proc_root.path().join("viewer.db"),
        proc_root: proc_root.path().to_path_buf(),
        password_override: Some("test-secret".to_string()),
        before_authorized_write: None,
    });

    let old_runtime = runtime.clone();
    let old_worker = std::thread::spawn(move || {
        let mut backend = backend_with_runtime(old_runtime);
        backend.list()
    });
    assert_eq!(old.wait_for_requests(4).len(), 4);
    old.process()
        .write_identity(2000, unsafe { libc::geteuid() }, None);

    let mut replacement_backend = backend_with_runtime(runtime.clone());
    assert!(
        replacement_backend
            .list()
            .expect("replacement generation listing")
            .is_empty()
    );
    let old_rows = old_worker
        .join()
        .expect("old listing worker")
        .expect("old listing response");
    assert_eq!(old_rows[0].id, "ses_old_generation");

    let current = backend_with_runtime(runtime);
    assert!(
        current.hide("ses_old_generation").is_err(),
        "an old listing must not authorize a current generation mutation"
    );
    let requests = replacement.requests();
    assert!(
        requests.iter().all(|request| request.method != "PATCH"),
        "no mutation may reach the replacement generation"
    );
    assert_eq!(
        requests.len(),
        4,
        "only the replacement listing may reach it"
    );
}
