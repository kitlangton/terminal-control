use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use portable_pty::{Child, CommandBuilder, ExitStatus, MasterPty, PtySize, native_pty_system};
use serde::{Deserialize, Serialize};
use vt100::Parser;

use crate::frame::from_screen;
use crate::recording::{self, InputOrigin};
use crate::shot::{self, Host, Options, Shot};

const OUTPUT_BATCH: usize = 32;
const SCROLLBACK_ROWS: usize = 10_000;

struct Output {
    at_ms: u64,
    bytes: Vec<u8>,
}

/// One running terminal application controlled in-process by its caller.
///
/// `Session` is the embedded equivalent of the CLI `session` lifecycle. It owns a PTY and the
/// visible terminal state, so callers can send input, wait for content, take shots, and resize
/// without spawning a new `cellshot` command for each action.
pub struct Session {
    master: Box<dyn MasterPty + Send>,
    child: Box<dyn Child + Send>,
    #[cfg(unix)]
    process_group: Option<i32>,
    parser: Parser,
    ansi: Vec<u8>,
    host: Host,
    receive: Receiver<Option<Output>>,
    max_bytes: usize,
    output_closed: bool,
    stopped: bool,
    exit: Option<ProcessExit>,
    last_output: Option<Instant>,
    recording: Option<recording::Writer>,
    cols: u16,
    rows: u16,
    cell_width: u16,
    cell_height: u16,
}

/// Lifecycle state of a running or completed session.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum SessionState {
    Running,
    Exited,
}

/// Termination information observed for a completed terminal application.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
pub struct ProcessExit {
    pub code: u32,
    pub signal: Option<String>,
    pub success: bool,
}

impl From<ExitStatus> for ProcessExit {
    fn from(status: ExitStatus) -> Self {
        Self {
            code: status.exit_code(),
            signal: status.signal().map(str::to_owned),
            success: status.success(),
        }
    }
}

/// Reason a session capture returned its visible shot.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum CaptureReason {
    Idle,
    Deadline,
    Exited,
    OutputClosed,
}

/// A visible shot together with the condition that made it observable.
#[derive(Deserialize, Serialize)]
pub struct CaptureResult {
    pub shot: Shot,
    pub reason: CaptureReason,
}

/// Observable state of one embedded or named terminal session.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct SessionStatus {
    pub state: SessionState,
    pub exit: Option<ProcessExit>,
    pub cols: u16,
    pub rows: u16,
    pub cell_width: u16,
    pub cell_height: u16,
    pub idle_for_ms: Option<u64>,
    pub has_visible_content: bool,
    pub recording: bool,
}

/// One named daemon session discovered in the local runtime directory.
#[derive(Debug, Serialize)]
pub struct NamedSessionStatus {
    pub name: String,
    pub status: Option<SessionStatus>,
    pub error: Option<String>,
}

impl Session {
    /// Start `command` inside a live PTY-backed session.
    pub fn start(
        command: &[String],
        cwd: Option<&Path>,
        record: Option<&Path>,
        options: &Options,
    ) -> Result<Self> {
        if command.is_empty() {
            bail!("provide a command after --");
        }
        if options.cols == 0 || options.rows == 0 {
            bail!("terminal dimensions must be greater than zero");
        }
        let started = Instant::now();
        let recording = record
            .map(|path| {
                recording::Writer::new(
                    path,
                    started,
                    options.cols,
                    options.rows,
                    options.cell_width,
                    options.cell_height,
                )
            })
            .transpose()?;
        let pair = native_pty_system()
            .openpty(PtySize {
                rows: options.rows,
                cols: options.cols,
                pixel_width: options.cell_width,
                pixel_height: options.cell_height,
            })
            .context("open session pseudo-terminal")?;
        let mut builder = CommandBuilder::new(&command[0]);
        builder.args(&command[1..]);
        shot::configure_pty_environment(&mut builder, options.color);
        if let Some(cwd) = cwd {
            builder.cwd(cwd);
        }
        let mut reader = pair
            .master
            .try_clone_reader()
            .context("open session PTY reader")?;
        let writer = pair
            .master
            .take_writer()
            .context("open session PTY writer")?;
        let child = pair
            .slave
            .spawn_command(builder)
            .context("spawn session command")?;
        drop(pair.slave);
        #[cfg(unix)]
        let process_group = child.process_id().and_then(|pid| i32::try_from(pid).ok());
        let (send, receive) = mpsc::sync_channel(32);
        thread::spawn(move || {
            let mut buffer = [0_u8; 16 * 1024];
            loop {
                match reader.read(&mut buffer) {
                    Ok(0) => break,
                    Ok(len) => {
                        if send
                            .send(Some(Output {
                                at_ms: started.elapsed().as_millis() as u64,
                                bytes: buffer[..len].to_vec(),
                            }))
                            .is_err()
                        {
                            return;
                        }
                    }
                    Err(_) => break,
                }
            }
            let _ = send.send(None);
        });
        Ok(Self {
            master: pair.master,
            child,
            #[cfg(unix)]
            process_group,
            parser: session_terminal(options.rows, options.cols),
            ansi: Vec::new(),
            host: Host::new(writer, options),
            receive,
            max_bytes: options.max_bytes,
            output_closed: false,
            stopped: false,
            exit: None,
            last_output: None,
            recording,
            cols: options.cols,
            rows: options.rows,
            cell_width: options.cell_width,
            cell_height: options.cell_height,
        })
    }

    /// Send one input burst to the terminal application.
    pub fn send(&mut self, input: &[u8]) -> Result<()> {
        self.send_all(&[input.to_vec()], Duration::ZERO)
    }

    /// Send ordered input bursts, optionally pacing them for recorded interactions.
    pub fn send_all(&mut self, input: &[Vec<u8>], pace: Duration) -> Result<()> {
        self.consume()?;
        if self.has_exited()? || self.stopped {
            bail!("session command has exited");
        }
        let last = input.len().saturating_sub(1);
        for (index, bytes) in input.iter().enumerate() {
            self.host.send(bytes)?;
            if let Some(recording) = &mut self.recording {
                recording.input(InputOrigin::Client, bytes)?;
            }
            if !pace.is_zero() && index < last {
                thread::sleep(pace);
                self.consume()?;
            }
        }
        Ok(())
    }

    /// Wait until visible terminal text contains `text`.
    pub fn wait_for_text(&mut self, text: &str, timeout: Duration) -> Result<()> {
        let deadline = Instant::now() + timeout;
        loop {
            self.consume()?;
            if self.parser.screen().contents().contains(text) {
                return Ok(());
            }
            if self.has_exited()? || self.stopped {
                bail!("session ended before visible terminal included {text:?}");
            }
            if Instant::now() >= deadline {
                bail!("timed out waiting for visible terminal text {text:?}");
            }
            thread::sleep(Duration::from_millis(10));
        }
    }

    /// Wait until no terminal output has arrived for `settle`.
    pub fn wait_for_idle(&mut self, settle: Duration, timeout: Duration) -> Result<()> {
        let started = Instant::now();
        let deadline = started + timeout;
        loop {
            self.consume()?;
            if self.output_closed || self.last_output.unwrap_or(started).elapsed() >= settle {
                return Ok(());
            }
            if Instant::now() >= deadline {
                bail!("timed out waiting for terminal output to settle");
            }
            thread::sleep(Duration::from_millis(10));
        }
    }

    /// Capture visible terminal state and report whether it settled, exited, or reached a limit.
    pub fn capture(&mut self, settle: Duration, deadline: Duration) -> Result<CaptureResult> {
        let started = Instant::now();
        let deadline = started + deadline;
        loop {
            self.consume()?;
            let reason = if self.has_exited()? || self.stopped {
                Some(CaptureReason::Exited)
            } else if self.output_closed {
                Some(CaptureReason::OutputClosed)
            } else if self.last_output.unwrap_or(started).elapsed() >= settle {
                Some(CaptureReason::Idle)
            } else if Instant::now() >= deadline {
                Some(CaptureReason::Deadline)
            } else {
                None
            };
            if let Some(reason) = reason {
                return Ok(CaptureResult {
                    shot: Shot {
                        frame: from_screen(self.parser.screen()),
                        ansi: self.ansi.clone(),
                    },
                    reason,
                });
            }
            thread::sleep(Duration::from_millis(10));
        }
    }

    /// Freeze the current terminal state without inspecting the capture reason.
    pub fn shot(&mut self, settle: Duration, deadline: Duration) -> Result<Shot> {
        Ok(self.capture(settle, deadline)?.shot)
    }

    /// Inspect session lifecycle, geometry, and whether a visible frame is available.
    pub fn status(&mut self) -> Result<SessionStatus> {
        self.consume()?;
        Ok(SessionStatus {
            state: if self.has_exited()? || self.stopped {
                SessionState::Exited
            } else {
                SessionState::Running
            },
            exit: self.exit.clone(),
            cols: self.cols,
            rows: self.rows,
            cell_width: self.cell_width,
            cell_height: self.cell_height,
            idle_for_ms: self
                .last_output
                .map(|last| last.elapsed().as_millis() as u64),
            has_visible_content: from_screen(self.parser.screen()).has_visible_content(),
            recording: self.recording.is_some(),
        })
    }

    /// Return readable normal-screen scrollback, or the exact retained ANSI/VT stream.
    pub fn history(&mut self, ansi: bool) -> Result<Vec<u8>> {
        self.consume()?;
        if ansi {
            return Ok(self.ansi.clone());
        }
        let mut screen = self.parser.screen().clone();
        screen.set_scrollback(usize::MAX);
        let mut offset = screen.scrollback();
        let mut lines = Vec::new();
        while offset > 0 {
            screen.set_scrollback(offset);
            let count = offset.min(usize::from(self.rows));
            lines.extend(
                screen
                    .rows(0, self.cols)
                    .take(count)
                    .map(|line| line.trim_end().to_owned()),
            );
            offset = offset.saturating_sub(usize::from(self.rows));
        }
        screen.set_scrollback(0);
        lines.extend(
            screen
                .rows(0, self.cols)
                .map(|line| line.trim_end().to_owned()),
        );
        Ok(lines.join("\n").trim_end().as_bytes().to_vec())
    }

    /// Resize the PTY and reflow subsequent terminal parsing at the new dimensions.
    pub fn resize(
        &mut self,
        cols: u16,
        rows: u16,
        cell_width: u16,
        cell_height: u16,
    ) -> Result<()> {
        if cols == 0 || rows == 0 {
            bail!("terminal dimensions must be greater than zero");
        }
        if self.recording.is_some() {
            bail!("resizing recorded sessions is not yet supported");
        }
        self.consume()?;
        self.master
            .resize(PtySize {
                rows,
                cols,
                pixel_width: cell_width,
                pixel_height: cell_height,
            })
            .context("resize session pseudo-terminal")?;
        self.host.resize(cols, rows, cell_width, cell_height);
        self.parser = session_terminal(rows, cols);
        self.parser.process(&self.ansi);
        self.cols = cols;
        self.rows = rows;
        self.cell_width = cell_width;
        self.cell_height = cell_height;
        Ok(())
    }

    /// Terminate the application owned by this session.
    pub fn stop(&mut self) -> Result<()> {
        self.terminate();
        Ok(())
    }

    fn consume(&mut self) -> Result<()> {
        while self.consume_one()? {}
        Ok(())
    }

    fn consume_batch(&mut self) -> Result<()> {
        for _ in 0..OUTPUT_BATCH {
            if !self.consume_one()? {
                break;
            }
        }
        Ok(())
    }

    fn consume_one(&mut self) -> Result<bool> {
        match self.receive.try_recv() {
            Ok(Some(output)) => {
                if let Some(recording) = &mut self.recording {
                    recording.output(output.at_ms, &output.bytes)?;
                }
                let response = self.host.respond(&output.bytes)?;
                if !response.is_empty()
                    && let Some(recording) = &mut self.recording
                {
                    recording.input(InputOrigin::Host, &response)?;
                }
                shot::retain(&mut self.ansi, &output.bytes, self.max_bytes)?;
                self.parser.process(&output.bytes);
                self.last_output = Some(Instant::now());
                Ok(true)
            }
            Ok(None) | Err(TryRecvError::Disconnected) => {
                self.output_closed = true;
                Ok(false)
            }
            Err(TryRecvError::Empty) => Ok(false),
        }
    }

    fn has_exited(&mut self) -> Result<bool> {
        if self.exit.is_some() {
            return Ok(true);
        }
        if let Some(status) = self.child.try_wait().context("poll session command")? {
            self.exit = Some(status.into());
            return Ok(true);
        }
        Ok(false)
    }

    fn terminate(&mut self) {
        if self.stopped {
            return;
        }
        #[cfg(unix)]
        if let Some(process_group) = self.process_group.take() {
            unsafe {
                libc::kill(-process_group, libc::SIGKILL);
            }
        }
        let _ = self.child.kill();
        if self.exit.is_none()
            && let Ok(status) = self.child.wait()
        {
            self.exit = Some(status.into());
        }
        self.output_closed = true;
        self.stopped = true;
    }
}

fn session_terminal(rows: u16, cols: u16) -> Parser {
    Parser::new(rows, cols, SCROLLBACK_ROWS)
}

impl Drop for Session {
    fn drop(&mut self) {
        self.terminate();
    }
}

#[derive(Serialize, Deserialize)]
enum Request {
    Ping,
    Status,
    Wait {
        text: String,
        timeout_ms: u64,
    },
    Send {
        input: Vec<Vec<u8>>,
        pace_ms: u64,
    },
    Shot {
        settle_ms: u64,
        deadline_ms: u64,
    },
    History {
        ansi: bool,
    },
    Resize {
        cols: u16,
        rows: u16,
        cell_width: Option<u16>,
        cell_height: Option<u16>,
    },
    Stop,
}

#[derive(Serialize, Deserialize)]
struct Response {
    error: Option<String>,
    captured: Option<Shot>,
    status: Option<SessionStatus>,
    history: Option<Vec<u8>>,
}

#[doc(hidden)]
pub fn start(
    name: &str,
    command: &[String],
    cwd: Option<&Path>,
    record: Option<&Path>,
    options: &Options,
) -> Result<()> {
    validate_name(name)?;
    implementation::start(name, command, cwd, record, options)
}

#[doc(hidden)]
pub fn restart(
    name: &str,
    command: &[String],
    cwd: Option<&Path>,
    record: Option<&Path>,
    options: &Options,
) -> Result<()> {
    if request(name, Request::Stop).is_ok() {
        let deadline = Instant::now() + Duration::from_secs(2);
        while request(name, Request::Ping).is_ok() {
            if Instant::now() >= deadline {
                bail!("timed out stopping session {name:?} before restart");
            }
            thread::sleep(Duration::from_millis(10));
        }
    }
    start(name, command, cwd, record, options)
}

#[doc(hidden)]
pub fn wait(name: &str, text: String, timeout: Duration) -> Result<()> {
    request(
        name,
        Request::Wait {
            text,
            timeout_ms: timeout.as_millis() as u64,
        },
    )?;
    Ok(())
}

#[doc(hidden)]
pub fn status(name: &str) -> Result<SessionStatus> {
    request(name, Request::Status)?
        .status
        .ok_or_else(|| anyhow::anyhow!("session did not return status"))
}

#[doc(hidden)]
pub fn send(name: &str, input: Vec<Vec<u8>>, pace: Duration) -> Result<()> {
    request(
        name,
        Request::Send {
            input,
            pace_ms: pace.as_millis() as u64,
        },
    )?;
    Ok(())
}

#[doc(hidden)]
pub fn shot(name: &str, settle: Duration, deadline: Duration) -> Result<Shot> {
    request(
        name,
        Request::Shot {
            settle_ms: settle.as_millis() as u64,
            deadline_ms: deadline.as_millis() as u64,
        },
    )?
    .captured
    .ok_or_else(|| anyhow::anyhow!("session did not return a shot"))
}

#[doc(hidden)]
pub fn resize(
    name: &str,
    cols: u16,
    rows: u16,
    cell_width: Option<u16>,
    cell_height: Option<u16>,
) -> Result<()> {
    request(
        name,
        Request::Resize {
            cols,
            rows,
            cell_width,
            cell_height,
        },
    )?;
    Ok(())
}

#[doc(hidden)]
pub fn history(name: &str, ansi: bool) -> Result<Vec<u8>> {
    request(name, Request::History { ansi })?
        .history
        .ok_or_else(|| anyhow::anyhow!("session did not return history"))
}

#[doc(hidden)]
pub fn list() -> Result<Vec<NamedSessionStatus>> {
    implementation::list()
}

#[doc(hidden)]
pub fn stop(name: &str) -> Result<()> {
    request(name, Request::Stop)?;
    Ok(())
}

#[doc(hidden)]
pub fn serve(
    socket: PathBuf,
    command: Vec<String>,
    cwd: Option<PathBuf>,
    record: Option<PathBuf>,
    options: Options,
) -> Result<()> {
    implementation::serve(socket, command, cwd, record, options)
}

fn request(name: &str, request: Request) -> Result<Response> {
    validate_name(name)?;
    let response = implementation::request(socket_path(name)?, &request)?;
    if let Some(error) = response.error {
        bail!(error);
    }
    Ok(response)
}

fn validate_name(name: &str) -> Result<()> {
    if name.is_empty()
        || !name
            .chars()
            .all(|char| char.is_ascii_alphanumeric() || matches!(char, '-' | '_' | '.'))
    {
        bail!("session names may contain only ASCII letters, digits, '.', '-', and '_'");
    }
    Ok(())
}

fn socket_path(name: &str) -> Result<PathBuf> {
    Ok(implementation::runtime_dir()?.join(format!("{name}.sock")))
}

#[cfg(unix)]
mod implementation {
    use std::fs;
    use std::io::{ErrorKind, Read, Write};
    use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
    use std::os::unix::net::{UnixListener, UnixStream};
    use std::path::{Path, PathBuf};
    use std::process::{Command, Stdio};
    use std::thread;
    use std::time::{Duration, Instant};

    use anyhow::{Context, Result, bail};

    use super::{NamedSessionStatus, Request, Response, Session};
    use crate::shot::{self, Options};

    const MAX_REQUEST_BYTES: u64 = 1024 * 1024;
    const CONTROL_TIMEOUT: Duration = Duration::from_secs(2);
    pub fn runtime_dir() -> Result<PathBuf> {
        let path = std::env::var_os("CELLSHOT_RUNTIME_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                PathBuf::from(format!("/tmp/cellshot-{}", unsafe { libc::geteuid() }))
            });
        match fs::symlink_metadata(&path) {
            Ok(metadata) => require_private_runtime_dir(&path, &metadata)?,
            Err(error) if error.kind() == ErrorKind::NotFound => {
                fs::DirBuilder::new()
                    .mode(0o700)
                    .create(&path)
                    .with_context(|| format!("create {}", path.display()))?;
            }
            Err(error) => return Err(error).with_context(|| format!("inspect {}", path.display())),
        }
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700))
            .with_context(|| format!("secure {}", path.display()))?;
        Ok(path)
    }

    fn require_private_runtime_dir(path: &Path, metadata: &fs::Metadata) -> Result<()> {
        if !metadata.file_type().is_dir() || metadata.file_type().is_symlink() {
            bail!(
                "session runtime path must be a real directory: {}",
                path.display()
            );
        }
        if metadata.uid() != unsafe { libc::geteuid() } {
            bail!(
                "session runtime directory is not owned by the current user: {}",
                path.display()
            );
        }
        Ok(())
    }

    pub fn start(
        name: &str,
        command: &[String],
        cwd: Option<&Path>,
        record: Option<&Path>,
        options: &Options,
    ) -> Result<()> {
        if command.is_empty() {
            bail!("provide a command after --");
        }
        let socket = runtime_dir()?.join(format!("{name}.sock"));
        ensure_socket_path(&socket)?;
        if socket.exists() {
            if request(socket.clone(), &Request::Ping).is_ok() {
                bail!("session {name:?} is already running");
            }
            match fs::remove_file(&socket) {
                Ok(()) => {}
                Err(error) if error.kind() == ErrorKind::NotFound => {}
                Err(error) => {
                    return Err(error)
                        .with_context(|| format!("remove stale {}", socket.display()));
                }
            }
        }
        let mut daemon =
            Command::new(std::env::current_exe().context("locate cellshot executable")?);
        daemon
            .arg("__serve")
            .arg("--socket")
            .arg(&socket)
            .arg("--cols")
            .arg(options.cols.to_string())
            .arg("--rows")
            .arg(options.rows.to_string())
            .arg("--cell-width")
            .arg(options.cell_width.to_string())
            .arg("--cell-height")
            .arg(options.cell_height.to_string())
            .arg("--max-bytes")
            .arg(options.max_bytes.to_string());
        if options.opentui_host {
            daemon.arg("--opentui-host");
        }
        match options.color {
            shot::ColorMode::Auto => {}
            shot::ColorMode::Always => {
                daemon.arg("--color").arg("always");
            }
            shot::ColorMode::Never => {
                daemon.arg("--color").arg("never");
            }
        }
        if let Some(cwd) = cwd {
            daemon.arg("--cwd").arg(cwd);
        }
        if let Some(record) = record {
            let record = if record.is_absolute() {
                record.to_owned()
            } else {
                std::env::current_dir()
                    .context("resolve recording output directory")?
                    .join(record)
            };
            daemon.arg("--record").arg(record);
        }
        daemon
            .arg("--")
            .args(command)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let mut daemon = daemon.spawn().context("start session daemon")?;
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if request(socket.clone(), &Request::Ping).is_ok() {
                return Ok(());
            }
            if let Some(status) = daemon.try_wait().context("poll session daemon")? {
                bail!("session daemon exited before becoming ready: {status}");
            }
            if Instant::now() >= deadline {
                let _ = daemon.kill();
                bail!("timed out starting session {name:?}");
            }
            thread::sleep(Duration::from_millis(20));
        }
    }

    pub fn request(socket: PathBuf, request: &Request) -> Result<Response> {
        ensure_socket_path(&socket)?;
        let mut stream = UnixStream::connect(&socket).with_context(|| {
            format!("connect to session at {}; is it running?", socket.display())
        })?;
        serde_json::to_writer(&mut stream, request).context("write session request")?;
        stream
            .shutdown(std::net::Shutdown::Write)
            .context("finish session request")?;
        serde_json::from_reader(stream).context("read session response")
    }

    pub fn list() -> Result<Vec<NamedSessionStatus>> {
        let mut sessions = Vec::new();
        for entry in fs::read_dir(runtime_dir()?).context("read session runtime directory")? {
            let path = entry.context("read session runtime entry")?.path();
            if path.extension().and_then(|extension| extension.to_str()) != Some("sock") {
                continue;
            }
            let Some(name) = path
                .file_stem()
                .and_then(|name| name.to_str())
                .map(str::to_owned)
            else {
                continue;
            };
            let (status, error) = match request(path, &Request::Status) {
                Ok(response) => (response.status, response.error),
                Err(error) => (None, Some(format!("{error:#}"))),
            };
            sessions.push(NamedSessionStatus {
                name,
                status,
                error,
            });
        }
        sessions.sort_by(|left, right| left.name.cmp(&right.name));
        Ok(sessions)
    }

    pub fn serve(
        socket: PathBuf,
        command: Vec<String>,
        cwd: Option<PathBuf>,
        record: Option<PathBuf>,
        options: Options,
    ) -> Result<()> {
        ensure_socket_path(&socket)?;
        if command.is_empty() {
            bail!("provide a command after --");
        }
        let listener =
            UnixListener::bind(&socket).with_context(|| format!("bind {}", socket.display()))?;
        fs::set_permissions(&socket, fs::Permissions::from_mode(0o600))
            .with_context(|| format!("secure {}", socket.display()))?;
        listener
            .set_nonblocking(true)
            .context("set session socket nonblocking")?;
        let mut session = Session::start(&command, cwd.as_deref(), record.as_deref(), &options)?;
        let result = run(&listener, &mut session);
        let _ = session.stop();
        let _ = fs::remove_file(&socket);
        result
    }

    fn ensure_socket_path(path: &Path) -> Result<()> {
        if path.as_os_str().as_encoded_bytes().len() >= 100 {
            bail!(
                "session socket path is too long for portable Unix sockets: {}; set CELLSHOT_RUNTIME_DIR to a shorter directory",
                path.display()
            );
        }
        Ok(())
    }

    fn run(listener: &UnixListener, session: &mut Session) -> Result<()> {
        loop {
            // Keep parsing and recording output even when no control request is in flight.
            session.consume_batch()?;
            match listener.accept() {
                Ok((stream, _)) => {
                    if handle(stream, session)? {
                        return Ok(());
                    }
                }
                Err(error) if error.kind() == ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(10));
                }
                Err(error) => return Err(error).context("accept session request"),
            }
        }
    }

    fn handle(mut stream: UnixStream, session: &mut Session) -> Result<bool> {
        stream
            .set_nonblocking(false)
            .context("set session connection blocking")?;
        stream
            .set_read_timeout(Some(CONTROL_TIMEOUT))
            .context("set session request timeout")?;
        stream
            .set_write_timeout(Some(CONTROL_TIMEOUT))
            .context("set session response timeout")?;
        let mut bytes = Vec::new();
        let response = match Read::by_ref(&mut stream)
            .take(MAX_REQUEST_BYTES + 1)
            .read_to_end(&mut bytes)
        {
            Ok(_) if bytes.len() as u64 > MAX_REQUEST_BYTES => Response {
                error: Some("session request exceeds 1 MiB".to_owned()),
                captured: None,
                status: None,
                history: None,
            },
            Ok(_) => match serde_json::from_slice::<Request>(&bytes) {
                Ok(request) => {
                    let stop = matches!(request, Request::Stop);
                    let response = match respond(session, request) {
                        Ok(response) => response,
                        Err(error) => Response {
                            error: Some(format!("{error:#}")),
                            captured: None,
                            status: None,
                            history: None,
                        },
                    };
                    if write_response(&mut stream, &response).is_ok() && stop {
                        return Ok(true);
                    }
                    return Ok(false);
                }
                Err(error) => Response {
                    error: Some(format!("invalid session request: {error}")),
                    captured: None,
                    status: None,
                    history: None,
                },
            },
            Err(error) => Response {
                error: Some(format!("failed to read session request: {error}")),
                captured: None,
                status: None,
                history: None,
            },
        };
        let _ = write_response(&mut stream, &response);
        Ok(false)
    }

    fn write_response(stream: &mut UnixStream, response: &Response) -> Result<()> {
        serde_json::to_writer(&mut *stream, response).context("write session response")?;
        stream.flush().context("flush session response")
    }

    fn respond(session: &mut Session, request: Request) -> Result<Response> {
        let mut response = Response {
            error: None,
            captured: None,
            status: None,
            history: None,
        };
        match request {
            Request::Ping => {}
            Request::Status => response.status = Some(session.status()?),
            Request::Send { input, pace_ms } => {
                session.send_all(&input, Duration::from_millis(pace_ms))?;
            }
            Request::Wait { text, timeout_ms } => {
                session.wait_for_text(&text, Duration::from_millis(timeout_ms))?;
            }
            Request::Shot {
                settle_ms,
                deadline_ms,
            } => {
                response.captured = Some(session.shot(
                    Duration::from_millis(settle_ms),
                    Duration::from_millis(deadline_ms),
                )?);
            }
            Request::History { ansi } => response.history = Some(session.history(ansi)?),
            Request::Resize {
                cols,
                rows,
                cell_width,
                cell_height,
            } => {
                let status = session.status()?;
                session.resize(
                    cols,
                    rows,
                    cell_width.unwrap_or(status.cell_width),
                    cell_height.unwrap_or(status.cell_height),
                )?;
            }
            Request::Stop => session.stop()?,
        }
        Ok(response)
    }
}

#[cfg(not(unix))]
mod implementation {
    use super::{NamedSessionStatus, Options, Request, Response};
    use anyhow::{Result, bail};
    use std::path::{Path, PathBuf};

    pub fn runtime_dir() -> Result<PathBuf> {
        bail!("persistent sessions require Unix sockets")
    }
    pub fn start(
        _: &str,
        _: &[String],
        _: Option<&Path>,
        _: Option<&Path>,
        _: &Options,
    ) -> Result<()> {
        bail!("persistent sessions require Unix sockets")
    }
    pub fn request(_: PathBuf, _: &Request) -> Result<Response> {
        bail!("persistent sessions require Unix sockets")
    }
    pub fn list() -> Result<Vec<NamedSessionStatus>> {
        bail!("persistent sessions require Unix sockets")
    }
    pub fn serve(
        _: PathBuf,
        _: Vec<String>,
        _: Option<PathBuf>,
        _: Option<PathBuf>,
        _: Options,
    ) -> Result<()> {
        bail!("persistent sessions require Unix sockets")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn embedded_session_waits_sends_resizes_and_takes_a_shot() {
        let mut session = Session::start(
            &[
                "sh".to_owned(),
                "-c".to_owned(),
                "printf ready; IFS= read -r line; printf '\\r\\ngot:%s' \"$line\"; sleep 1"
                    .to_owned(),
            ],
            None,
            None,
            &Options {
                cols: 20,
                rows: 4,
                settle: Duration::from_millis(10),
                deadline: Duration::from_secs(2),
                ..Options::default()
            },
        )
        .unwrap();

        session
            .wait_for_text("ready", Duration::from_secs(2))
            .unwrap();
        session.send(b"hello\r").unwrap();
        session
            .wait_for_text("got:hello", Duration::from_secs(2))
            .unwrap();
        assert_eq!(session.status().unwrap().state, SessionState::Running);
        session
            .wait_for_idle(Duration::from_millis(10), Duration::from_secs(2))
            .unwrap();
        session.resize(30, 5, 9, 18).unwrap();
        let shot = session
            .shot(Duration::from_millis(10), Duration::from_secs(2))
            .unwrap();

        assert_eq!((shot.frame.cols, shot.frame.rows), (30, 5));
        assert!(shot.frame.text().contains("got:hello"));
        session.stop().unwrap();
        assert_eq!(session.status().unwrap().state, SessionState::Exited);
        assert!(session.shot(Duration::ZERO, Duration::ZERO).is_ok());
    }

    #[cfg(unix)]
    #[test]
    fn recorded_session_rejects_resize_until_timelines_support_geometry_changes() {
        let record = std::env::temp_dir().join(format!(
            "cellshot-recorded-resize-test-{}.cellshot",
            std::process::id()
        ));
        let mut session = Session::start(
            &["sh".to_owned(), "-c".to_owned(), "sleep 1".to_owned()],
            None,
            Some(&record),
            &Options::default(),
        )
        .unwrap();

        assert_eq!(
            session.resize(100, 32, 9, 18).unwrap_err().to_string(),
            "resizing recorded sessions is not yet supported"
        );
        session.stop().unwrap();
        let _ = std::fs::remove_file(record);
    }

    #[cfg(unix)]
    #[test]
    fn history_exposes_normal_screen_scrollback_and_raw_stream() {
        let mut session = Session::start(
            &[
                "sh".to_owned(),
                "-c".to_owned(),
                "printf 'one\r\ntwo\r\nthree\r\nfour\r\nfive\r\n'; sleep 1".to_owned(),
            ],
            None,
            None,
            &Options {
                cols: 20,
                rows: 2,
                ..Options::default()
            },
        )
        .unwrap();
        session
            .wait_for_text("five", Duration::from_secs(2))
            .unwrap();

        let history = String::from_utf8(session.history(false).unwrap()).unwrap();
        assert!(history.contains("one"));
        assert!(history.contains("five"));
        assert!(
            session
                .history(true)
                .unwrap()
                .windows(3)
                .any(|bytes| bytes == b"one")
        );
        session.stop().unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn stopping_after_pty_eof_terminates_still_running_process() {
        let pid_path = std::env::temp_dir().join(format!(
            "cellshot-pty-eof-owner-test-{}.pid",
            std::process::id()
        ));
        let script = format!(
            "printf '%s' $$ > '{}'; exec >/dev/null 2>&1; sleep 30",
            pid_path.display()
        );
        let mut session = Session::start(
            &["sh".to_owned(), "-c".to_owned(), script],
            None,
            None,
            &Options::default(),
        )
        .unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        let pid = loop {
            if let Ok(pid) = std::fs::read_to_string(&pid_path) {
                break pid.parse::<i32>().unwrap();
            }
            assert!(Instant::now() < deadline, "child did not write its pid");
            thread::sleep(Duration::from_millis(10));
        };
        thread::sleep(Duration::from_millis(20));

        assert_eq!(session.status().unwrap().state, SessionState::Running);
        session.stop().unwrap();
        assert_eq!(unsafe { libc::kill(pid, 0) }, -1);
        let _ = std::fs::remove_file(pid_path);
    }
}
