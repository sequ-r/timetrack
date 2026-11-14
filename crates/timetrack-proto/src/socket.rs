/* proto/socket.rs
 *
 * Copyright 2026 sequ
 *
 * SPDX-License-Identifier: GPL-3.0-or-later
 */

//! The local-socket transport (REQUIREMENTS §16, ADR 0013).
//!
//! Same contract as the typed D-Bus interface — the same view structs,
//! the same [`ServiceError`] refusals — over a Unix socket instead of
//! the session bus, so the service is reachable identically on every
//! desktop OS. Windows named pipes land with the Windows port (§16
//! step 5) behind this same module; on non-Unix targets today the
//! socket client reports no service and callers fall back as designed.
//!
//! # Wire shape
//!
//! One request per connection, deliberately: connect, send one
//! length-prefixed JSON [`Frame`], read one reply frame, close. There
//! is no multiplexing to get wrong, a wedged connection holds nothing
//! hostage, and the connect cost is invisible at this app's call rates
//! (a handful of actions a minute, a 30 s fallback poll). The one
//! long-lived connection is a subscription: send
//! [`Request::SubscribeChanged`] and the service holds the connection
//! open, writing one [`Frame::Changed`] per persisted mutation.
//!
//! # Which socket
//!
//! `TIMETRACK_SOCKET` wins when set (test rigs, explicit user choice);
//! otherwise `$XDG_RUNTIME_DIR/timetrack.sock`, falling back to the
//! temp dir keyed by home directory so two users on one machine never
//! share a path. The service and every client derive it through
//! [`socket_path`], so they cannot disagree.

use serde::{Deserialize, Serialize};
use std::io::{Read, Write};
use std::path::PathBuf;

use super::{
    CALL_TIMEOUT, ClientError, EntryView, ProjectView, RefreshView, ServiceError, Snapshot,
    SourceView, SuggestionView,
};

/// The socket protocol revision an `ApiVersion` reply reports. Bump
/// when a [`Request`]/[`Response`] shape changes; clients treat any
/// other value as "not our service" and fall back.
pub const SOCKET_API_VERSION: u32 = 1;

/// Environment override for the socket path (test rigs above all: a rig
/// must never reach the developer's real service).
pub const SOCKET_ENV: &str = "TIMETRACK_SOCKET";

/// Set (to anything) to forbid clients from auto-spawning the service.
pub const NO_AUTOSPAWN_ENV: &str = "TIMETRACK_NO_AUTOSPAWN";

/// Largest frame either side will read. The 10-year snapshot measured
/// 5.5 MB as JSON (ADR 0012); this is an order of magnitude of headroom
/// and still refuses a hostile or wedged peer's endless frame.
pub const MAX_FRAME_BYTES: u32 = 64 * 1024 * 1024;

/// The socket both sides use, derived one way (see module docs).
pub fn socket_path() -> PathBuf {
    socket_path_from(
        std::env::var_os(SOCKET_ENV).as_deref(),
        std::env::var_os("XDG_RUNTIME_DIR").as_deref(),
        std::env::var_os("HOME").as_deref(),
    )
}

fn socket_path_from(
    override_path: Option<&std::ffi::OsStr>,
    runtime_dir: Option<&std::ffi::OsStr>,
    home: Option<&std::ffi::OsStr>,
) -> PathBuf {
    if let Some(p) = override_path.filter(|p| !p.is_empty()) {
        return PathBuf::from(p);
    }
    if let Some(dir) = runtime_dir.filter(|p| !p.is_empty()) {
        return PathBuf::from(dir).join("timetrack.sock");
    }
    // No runtime dir (a bare SSH shell, a test): key the temp path by
    // home so two users never collide on one shared /tmp name.
    let who = home
        .map(|h| {
            PathBuf::from(h)
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| "user".to_string())
        })
        .unwrap_or_else(|| "user".to_string());
    std::env::temp_dir().join(format!("timetrack-{who}.sock"))
}

/// One service call. Field-for-field the typed D-Bus methods, so the
/// service dispatches both transports onto the same functions.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Request {
    ApiVersion,
    Snapshot,
    SubscribeChanged,
    Add {
        project_id: String,
        description: String,
        started_at: i64,
        ended_at: i64,
    },
    AddDuration {
        project_id: String,
        description: String,
        duration_ms: i64,
    },
    AddDurationEnding {
        project_id: String,
        description: String,
        duration_ms: i64,
        ended_at: i64,
    },
    QuickAdd {
        project_id: String,
        duration_ms: i64,
    },
    SetTimes {
        id: String,
        started_at: i64,
        ended_at: i64,
    },
    SetText {
        id: String,
        description: String,
    },
    SetProject {
        id: String,
        project_id: String,
    },
    UndoQuickAdd {
        id: String,
    },
    DeleteEntry {
        id: String,
    },
    Split {
        id: String,
        at_ms: i64,
    },
    Merge {
        ids: Vec<String>,
    },
    AddProject {
        name: String,
    },
    UpdateProject {
        id: String,
        name: String,
        colour: i64,
    },
    SetArchived {
        id: String,
        archived: bool,
    },
    DeleteProject {
        id: String,
    },
    ExportCsv {
        scope: String,
    },
    ExportCsvAt {
        scope: String,
        anchor_ms: i64,
    },
    ExportCsvRange {
        from_ms: i64,
        to_ms: i64,
    },
    // --- external sources (§19, socket-only) ---
    AddSource {
        kind: String,
        locator: String,
        project_id: Option<String>,
        label: String,
    },
    ListSources,
    RefreshSource {
        /// `None` refreshes every source.
        id: Option<String>,
    },
    ListSuggestions,
    ConfirmSuggestion {
        id: String,
        project_id: Option<String>,
    },
    DismissSuggestion {
        id: String,
    },
    SetNote {
        id: String,
        note: Option<String>,
    },
}

/// One call's successful answer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Response {
    ApiVersion(u32),
    Snapshot(Box<Snapshot>),
    Entry(Box<EntryView>),
    EntryPair(Box<EntryView>, Box<EntryView>),
    Project(Box<ProjectView>),
    Csv(String),
    Unit,
    Subscribed,
    Source(Box<SourceView>),
    Sources(Vec<SourceView>),
    Suggestions(Vec<SuggestionView>),
    Refresh(RefreshView),
}

impl Response {
    fn mismatch(self, what: &str) -> ClientError {
        ClientError::Transport(format!(
            "the service answered a socket call with the wrong shape (wanted {what})"
        ))
    }

    pub fn snapshot(self) -> Result<Snapshot, ClientError> {
        match self {
            Response::Snapshot(s) => Ok(*s),
            other => Err(other.mismatch("a snapshot")),
        }
    }

    pub fn entry(self) -> Result<EntryView, ClientError> {
        match self {
            Response::Entry(e) => Ok(*e),
            other => Err(other.mismatch("an entry")),
        }
    }

    pub fn entry_pair(self) -> Result<(EntryView, EntryView), ClientError> {
        match self {
            Response::EntryPair(a, b) => Ok((*a, *b)),
            other => Err(other.mismatch("an entry pair")),
        }
    }

    pub fn project(self) -> Result<ProjectView, ClientError> {
        match self {
            Response::Project(p) => Ok(*p),
            other => Err(other.mismatch("a project")),
        }
    }

    pub fn csv(self) -> Result<String, ClientError> {
        match self {
            Response::Csv(s) => Ok(s),
            other => Err(other.mismatch("csv text")),
        }
    }

    pub fn unit(self) -> Result<(), ClientError> {
        match self {
            Response::Unit => Ok(()),
            other => Err(other.mismatch("no reply value")),
        }
    }

    pub fn source(self) -> Result<SourceView, ClientError> {
        match self {
            Response::Source(s) => Ok(*s),
            other => Err(other.mismatch("a source")),
        }
    }

    pub fn sources(self) -> Result<Vec<SourceView>, ClientError> {
        match self {
            Response::Sources(s) => Ok(s),
            other => Err(other.mismatch("sources")),
        }
    }

    pub fn suggestions(self) -> Result<Vec<SuggestionView>, ClientError> {
        match self {
            Response::Suggestions(s) => Ok(s),
            other => Err(other.mismatch("suggestions")),
        }
    }

    pub fn refresh(self) -> Result<RefreshView, ClientError> {
        match self {
            Response::Refresh(r) => Ok(r),
            other => Err(other.mismatch("a refresh tally")),
        }
    }
}

/// One length-prefixed message on a socket connection.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Frame {
    Request(Request),
    /// A call's outcome: the value, or the service's refusal, typed
    /// exactly as on the D-Bus interfaces.
    Reply(Result<Response, ServiceError>),
    /// A persisted mutation happened: refresh (the `Changed` signal).
    Changed,
}

/// Write one frame: a big-endian u32 length, then the JSON payload.
pub fn write_frame(w: &mut impl Write, frame: &Frame) -> Result<(), ClientError> {
    let body = serde_json::to_vec(frame).map_err(|e| ClientError::Transport(e.to_string()))?;
    let len = u32::try_from(body.len())
        .map_err(|_| ClientError::Transport("socket frame too large".into()))?;
    w.write_all(&len.to_be_bytes())
        .and_then(|()| w.write_all(&body))
        .and_then(|()| w.flush())
        .map_err(map_io)
}

/// Read one frame. `Ok(None)` is a clean end of connection.
pub fn read_frame(r: &mut impl Read) -> Result<Option<Frame>, ClientError> {
    let mut len_buf = [0u8; 4];
    match r.read_exact(&mut len_buf) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(map_io(e)),
    }
    let len = u32::from_be_bytes(len_buf);
    if len > MAX_FRAME_BYTES {
        return Err(ClientError::Transport(format!(
            "socket frame of {len} bytes exceeds the {MAX_FRAME_BYTES}-byte cap"
        )));
    }
    let mut body = vec![0u8; len as usize];
    r.read_exact(&mut body).map_err(map_io)?;
    serde_json::from_slice(&body)
        .map(Some)
        .map_err(|e| ClientError::Transport(e.to_string()))
}

/// Read one frame's raw payload (for async readers that drive their own
/// buffering, like the `Changed` subscription stream).
pub fn decode_frame(body: &[u8]) -> Result<Frame, ClientError> {
    serde_json::from_slice(body).map_err(|e| ClientError::Transport(e.to_string()))
}

/// The bytes of one frame without its length prefix.
pub fn encode_frame(frame: &Frame) -> Result<Vec<u8>, ClientError> {
    let body = serde_json::to_vec(frame).map_err(|e| ClientError::Transport(e.to_string()))?;
    let len = u32::try_from(body.len())
        .map_err(|_| ClientError::Transport("socket frame too large".into()))?;
    let mut out = Vec::with_capacity(4 + body.len());
    out.extend_from_slice(&len.to_be_bytes());
    out.extend_from_slice(&body);
    Ok(out)
}

/// Sort a socket failure the way bus failures are sorted: a socket with
/// nobody listening is `NoService`, a silent peer is the timeout wording,
/// everything else is transport with the OS's own words.
pub fn map_io(e: std::io::Error) -> ClientError {
    use std::io::ErrorKind;
    match e.kind() {
        ErrorKind::NotFound | ErrorKind::ConnectionRefused => ClientError::NoService,
        ErrorKind::TimedOut | ErrorKind::WouldBlock => {
            ClientError::Transport(super::TIMEOUT_WORDING.to_string())
        }
        ErrorKind::UnexpectedEof => ClientError::NoService,
        _ => ClientError::Transport(e.to_string()),
    }
}

/// A client for the socket transport: one connection per call (see the
/// module docs). Cheap to clone — it is only a path.
#[derive(Debug, Clone)]
pub struct SocketClient {
    path: PathBuf,
}

impl SocketClient {
    pub fn new(path: PathBuf) -> Self {
        SocketClient { path }
    }

    pub fn at_default_path() -> Self {
        SocketClient::new(socket_path())
    }

    pub fn path(&self) -> &std::path::Path {
        &self.path
    }

    /// Whether a TimeTrack service of our revision answers on this
    /// socket. Any failure is "no", never an error: probing decides a
    /// fallback, exactly like the typed-interface probe. The probe's
    /// timeout is short (see `PROBE_TIMEOUT` use below): a wedged
    /// listener must not stall every client's connect for the full
    /// call timeout.
    pub fn probe(&self) -> bool {
        matches!(
            self.call_with_timeout(Request::ApiVersion, std::time::Duration::from_millis(500)),
            Ok(Response::ApiVersion(SOCKET_API_VERSION))
        )
    }

    /// One call: connect, send, read the reply, close. Runs synchronously
    /// on the caller's thread with [`CALL_TIMEOUT`] on both directions —
    /// every consumer drives this client from a dedicated thread or a
    /// one-shot `block_on`, as they already did for the bus.
    pub fn call(&self, request: Request) -> Result<Response, ClientError> {
        self.call_with_timeout(request, CALL_TIMEOUT)
    }

    fn call_with_timeout(
        &self,
        request: Request,
        timeout: std::time::Duration,
    ) -> Result<Response, ClientError> {
        let mut stream = self.connect(timeout)?;
        write_frame(&mut stream, &Frame::Request(request))?;
        match read_frame(&mut stream)? {
            Some(Frame::Reply(Ok(response))) => Ok(response),
            Some(Frame::Reply(Err(service))) => Err(ClientError::Service(service)),
            Some(_) => Err(ClientError::Transport(
                "the service answered a socket call with a non-reply frame".into(),
            )),
            None => Err(ClientError::NoService),
        }
    }

    #[cfg(unix)]
    fn connect(
        &self,
        timeout: std::time::Duration,
    ) -> Result<std::os::unix::net::UnixStream, ClientError> {
        let stream = std::os::unix::net::UnixStream::connect(&self.path).map_err(map_io)?;
        stream
            .set_read_timeout(Some(timeout))
            .and_then(|()| stream.set_write_timeout(Some(timeout)))
            .map_err(map_io)?;
        Ok(stream)
    }

    #[cfg(windows)]
    fn connect(
        &self,
        _timeout: std::time::Duration,
    ) -> Result<interprocess::local_socket::Stream, ClientError> {
        use interprocess::local_socket::{GenericNamespaced, ToNsName};
        let name = pipe_name_of(&self.path)?
            .to_ns_name::<GenericNamespaced>()
            .map_err(map_io)?;
        interprocess::local_socket::ConnectOptions::new()
            .name(name)
            .connect_sync()
            .map_err(map_io)
    }

    #[cfg(not(any(unix, windows)))]
    fn connect(
        &self,
        _timeout: std::time::Duration,
    ) -> Result<std::io::Cursor<Vec<u8>>, ClientError> {
        Err(ClientError::NoService)
    }

    /// Open a `Changed` subscription connection and hand back its
    /// async reader. The service confirms with `Subscribed` first, then
    /// writes [`Frame::Changed`] per persisted mutation until either
    /// side closes.
    #[cfg(unix)]
    pub async fn subscribe_changed(
        &self,
    ) -> Result<async_io::Async<std::os::unix::net::UnixStream>, ClientError> {
        use futures_lite::AsyncWriteExt;
        let mut stream = async_io::Async::<std::os::unix::net::UnixStream>::connect(&self.path)
            .await
            .map_err(map_io)?;
        stream
            .write_all(&encode_frame(&Frame::Request(Request::SubscribeChanged))?)
            .await
            .map_err(map_io)?;
        let frame = read_frame_async(&mut stream).await?;
        match frame {
            Some(Frame::Reply(Ok(Response::Subscribed))) => Ok(stream),
            Some(Frame::Reply(Err(service))) => Err(ClientError::Service(service)),
            _ => Err(ClientError::NoService),
        }
    }
}

/// One accepted/connected stream, per platform: a Unix socket on
/// Unix, a named pipe on Windows. Both are plain `Read + Write`.
#[cfg(unix)]
pub type ConnStream = std::os::unix::net::UnixStream;
#[cfg(windows)]
pub type ConnStream = interprocess::local_socket::Stream;
#[cfg(not(any(unix, windows)))]
pub type ConnStream = std::io::Cursor<Vec<u8>>;

/// The Windows pipe name for a socket path: pipes live in a flat
/// namespace, so the path's file name is the identity both sides
/// derive from the same `socket_path()`.
#[cfg(windows)]
fn pipe_name_of(path: &std::path::Path) -> Result<String, ClientError> {
    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .filter(|n| !n.is_empty())
        .ok_or_else(|| ClientError::Transport("socket path has no file name".into()))
}

/// Apply a read timeout where the platform stream supports one
/// (Windows pipes rely on the service answering; consumers all read
/// on dedicated threads, as they did for the bus).
pub fn set_read_timeout(stream: &ConnStream, timeout: Option<std::time::Duration>) {
    #[cfg(unix)]
    let _ = stream.set_read_timeout(timeout);
    #[cfg(not(unix))]
    let _ = (stream, timeout);
}

/// The listening side, per platform. Used by the service only.
pub struct Listener {
    #[cfg(unix)]
    inner: std::os::unix::net::UnixListener,
    #[cfg(windows)]
    inner: interprocess::local_socket::Listener,
}

impl Listener {
    /// Bind `path`. On Unix the file is created mode `0600`; the
    /// caller removes stale files first (ownership is probed, never
    /// stolen). On Windows the pipe name derives from the path.
    pub fn bind(path: &std::path::Path) -> Result<Self, ClientError> {
        #[cfg(unix)]
        {
            let inner = std::os::unix::net::UnixListener::bind(path).map_err(map_io)?;
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
            Ok(Listener { inner })
        }
        #[cfg(windows)]
        {
            use interprocess::local_socket::{GenericNamespaced, ListenerOptions, ToNsName};
            let name = pipe_name_of(path)?
                .to_ns_name::<GenericNamespaced>()
                .map_err(map_io)?;
            let inner = ListenerOptions::new()
                .name(name)
                .create_sync()
                .map_err(map_io)?;
            Ok(Listener { inner })
        }
        #[cfg(not(any(unix, windows)))]
        {
            let _ = path;
            Err(ClientError::Transport(
                "no local-socket transport on this platform yet".into(),
            ))
        }
    }

    /// The next incoming connection, blocking.
    pub fn accept(&self) -> Result<ConnStream, ClientError> {
        #[cfg(unix)]
        {
            self.inner.accept().map(|(s, _)| s).map_err(map_io)
        }
        #[cfg(windows)]
        {
            use interprocess::local_socket::traits::Listener as _;
            self.inner.accept().map_err(map_io)
        }
        #[cfg(not(any(unix, windows)))]
        {
            Err(ClientError::NoService)
        }
    }
}

impl SocketClient {
    /// Subscribe over a blocking connection, forwarded onto a channel
    /// by a reader thread (the Windows shape; Unix uses the async
    /// stream directly). The receiver closes when the connection does.
    #[cfg(not(unix))]
    pub fn subscribe_changed_channel(&self) -> Result<std::sync::mpsc::Receiver<()>, ClientError> {
        let mut stream = self.connect(CALL_TIMEOUT)?;
        write_frame(&mut stream, &Frame::Request(Request::SubscribeChanged))?;
        match read_frame(&mut stream)? {
            Some(Frame::Reply(Ok(Response::Subscribed))) => {}
            Some(Frame::Reply(Err(service))) => return Err(ClientError::Service(service)),
            _ => return Err(ClientError::NoService),
        }
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            while matches!(read_frame(&mut stream), Ok(Some(Frame::Changed))) {
                if tx.send(()).is_err() {
                    return;
                }
            }
        });
        Ok(rx)
    }
}

/// Read one frame from an async stream (the subscription reader).
#[cfg(unix)]
pub async fn read_frame_async(
    stream: &mut async_io::Async<std::os::unix::net::UnixStream>,
) -> Result<Option<Frame>, ClientError> {
    use futures_lite::AsyncReadExt;
    let mut len_buf = [0u8; 4];
    match stream.read_exact(&mut len_buf).await {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(map_io(e)),
    }
    let len = u32::from_be_bytes(len_buf);
    if len > MAX_FRAME_BYTES {
        return Err(ClientError::Transport(format!(
            "socket frame of {len} bytes exceeds the {MAX_FRAME_BYTES}-byte cap"
        )));
    }
    let mut body = vec![0u8; len as usize];
    stream.read_exact(&mut body).await.map_err(map_io)?;
    decode_frame(&body).map(Some)
}

/// Try once to start the sibling `timetrack-service` binary (the one
/// beside the running executable — the AppImage layout and a cargo
/// target dir agree), unless `TIMETRACK_NO_AUTOSPAWN` forbids it.
/// Best effort and silent on failure: callers keep their ordinary
/// "service not running" path either way. The spawned service replaces
/// what the AppImage's AppRun shell used to be the only starter of.
pub fn try_spawn_service() {
    if std::env::var_os(NO_AUTOSPAWN_ENV).is_some() {
        return;
    }
    // An explicit socket override means somebody else owns the service
    // lifecycle (a test rig above all): spawning our sibling with
    // default store paths there would race the rig's own service for
    // the socket and could win with the wrong store.
    if std::env::var_os(SOCKET_ENV).is_some() {
        return;
    }
    let Ok(exe) = std::env::current_exe() else {
        return;
    };
    let Some(dir) = exe.parent() else { return };
    let name = if cfg!(windows) {
        "timetrack-service.exe"
    } else {
        "timetrack-service"
    };
    let candidate = dir.join(name);
    if !candidate.is_file() {
        return;
    }
    let _ = std::process::Command::new(candidate)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_round_trip_and_eof_is_clean() {
        let mut buf: Vec<u8> = Vec::new();
        let frame = Frame::Request(Request::QuickAdd {
            project_id: "p1".into(),
            duration_ms: 900_000,
        });
        write_frame(&mut buf, &frame).unwrap();
        write_frame(&mut buf, &Frame::Changed).unwrap();
        let mut reader = std::io::Cursor::new(buf);
        assert_eq!(read_frame(&mut reader).unwrap(), Some(frame));
        assert_eq!(read_frame(&mut reader).unwrap(), Some(Frame::Changed));
        assert_eq!(read_frame(&mut reader).unwrap(), None);
    }

    #[test]
    fn an_oversized_frame_is_refused_before_allocating() {
        let mut buf = Vec::new();
        buf.extend_from_slice(&(MAX_FRAME_BYTES + 1).to_be_bytes());
        let mut reader = std::io::Cursor::new(buf);
        let err = read_frame(&mut reader).unwrap_err();
        assert!(err.to_string().contains("cap"), "{err}");
    }

    #[test]
    fn socket_path_prefers_override_then_runtime_then_temp() {
        assert_eq!(
            socket_path_from(
                Some(std::ffi::OsStr::new("/x/s.sock")),
                Some(std::ffi::OsStr::new("/run/user/1000")),
                None
            ),
            PathBuf::from("/x/s.sock")
        );
        assert_eq!(
            socket_path_from(None, Some(std::ffi::OsStr::new("/run/user/1000")), None),
            PathBuf::from("/run/user/1000/timetrack.sock")
        );
        let fell_back = socket_path_from(None, None, Some(std::ffi::OsStr::new("/home/sequ")));
        assert!(
            fell_back.ends_with("timetrack-sequ.sock"),
            "{fell_back:?} must be keyed by the home owner"
        );
        // An empty override counts as unset, like the empty runtime dir.
        assert_eq!(
            socket_path_from(
                Some(std::ffi::OsStr::new("")),
                Some(std::ffi::OsStr::new("/r")),
                None
            ),
            PathBuf::from("/r/timetrack.sock")
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_call_round_trips_against_a_one_shot_server() {
        use std::os::unix::net::UnixListener;
        let dir = std::env::temp_dir().join(format!("tt-sock-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("s.sock");
        let listener = UnixListener::bind(&path).unwrap();
        let server = std::thread::spawn(move || {
            let (mut conn, _) = listener.accept().unwrap();
            match read_frame(&mut conn).unwrap() {
                Some(Frame::Request(Request::ApiVersion)) => {
                    write_frame(
                        &mut conn,
                        &Frame::Reply(Ok(Response::ApiVersion(SOCKET_API_VERSION))),
                    )
                    .unwrap();
                }
                other => panic!("unexpected frame {other:?}"),
            }
        });
        let client = SocketClient::new(path.clone());
        assert!(client.probe(), "our revision must probe true");
        server.join().unwrap();

        // Nobody listens any more (listener dropped with the thread):
        // that is NoService, never a hang or a transport mystery.
        std::fs::remove_file(&path).unwrap();
        assert_eq!(
            client.call(Request::ApiVersion),
            Err(ClientError::NoService)
        );
        assert!(!client.probe());
    }
}
