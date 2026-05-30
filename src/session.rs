use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};

use crate::capture::{Captured, Options};

#[derive(Serialize, Deserialize)]
enum Request {
    Ping,
    Wait { text: String, timeout_ms: u64 },
    Send { input: Vec<Vec<u8>>, pace_ms: u64 },
    Snapshot { settle_ms: u64, deadline_ms: u64 },
    Close,
}

#[derive(Serialize, Deserialize)]
struct Response {
    error: Option<String>,
    captured: Option<Captured>,
}

pub fn launch(
    name: &str,
    command: &[String],
    cwd: Option<&Path>,
    record: Option<&Path>,
    options: &Options,
) -> Result<()> {
    validate_name(name)?;
    implementation::launch(name, command, cwd, record, options)
}

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

pub fn snapshot(name: &str, settle: Duration, deadline: Duration) -> Result<Captured> {
    request(
        name,
        Request::Snapshot {
            settle_ms: settle.as_millis() as u64,
            deadline_ms: deadline.as_millis() as u64,
        },
    )?
    .captured
    .ok_or_else(|| anyhow::anyhow!("session did not return a snapshot"))
}

pub fn close(name: &str) -> Result<()> {
    request(name, Request::Close)?;
    Ok(())
}

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
    use std::sync::mpsc::{self, Receiver, TryRecvError};
    use std::thread;
    use std::time::{Duration, Instant};

    use anyhow::{Context, Result, bail};
    use portable_pty::{CommandBuilder, PtySize, native_pty_system};
    use vt100::Parser;

    use super::{Request, Response};
    use crate::capture::{self, Captured, Host, Options};
    use crate::frame::from_screen;
    use crate::recording::{self, InputOrigin};

    const MAX_REQUEST_BYTES: u64 = 1024 * 1024;
    const CONTROL_TIMEOUT: Duration = Duration::from_secs(2);
    const OUTPUT_BATCH: usize = 32;

    struct Output {
        at_ms: u64,
        bytes: Vec<u8>,
    }

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

    pub fn launch(
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
            fs::remove_file(&socket)
                .with_context(|| format!("remove stale {}", socket.display()))?;
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
        let started = Instant::now();
        let recording = record
            .as_deref()
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
        let listener =
            UnixListener::bind(&socket).with_context(|| format!("bind {}", socket.display()))?;
        fs::set_permissions(&socket, fs::Permissions::from_mode(0o600))
            .with_context(|| format!("secure {}", socket.display()))?;
        listener
            .set_nonblocking(true)
            .context("set session socket nonblocking")?;
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
        builder.env("TERM", "xterm-truecolor");
        builder.env("COLORTERM", "truecolor");
        if let Some(cwd) = cwd.as_deref() {
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
        let mut child = pair
            .slave
            .spawn_command(builder)
            .context("spawn session command")?;
        drop(pair.slave);
        let process_group = child.process_id().and_then(|pid| i32::try_from(pid).ok());
        let (send, receive) = mpsc::sync_channel(32);
        let _reader_thread = thread::spawn(move || {
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
        let mut state = State {
            parser: capture::terminal(options.rows, options.cols),
            ansi: Vec::new(),
            host: Host::new(writer, &options),
            receive,
            max_bytes: options.max_bytes,
            closed: false,
            last_output: None,
            recording,
        };
        let result = run(&listener, &mut state);
        if let Some(process_group) = process_group {
            unsafe {
                libc::kill(-process_group, libc::SIGKILL);
            }
        }
        let _ = child.kill();
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

    fn run(listener: &UnixListener, state: &mut State) -> Result<()> {
        loop {
            state.consume()?;
            match listener.accept() {
                Ok((stream, _)) => {
                    if handle(stream, state)? {
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

    fn handle(mut stream: UnixStream, state: &mut State) -> Result<bool> {
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
            },
            Ok(_) => match serde_json::from_slice::<Request>(&bytes) {
                Ok(request) => {
                    let close = matches!(request, Request::Close);
                    let response = match state.respond(request) {
                        Ok(captured) => Response {
                            error: None,
                            captured,
                        },
                        Err(error) => Response {
                            error: Some(format!("{error:#}")),
                            captured: None,
                        },
                    };
                    if write_response(&mut stream, &response).is_ok() && close {
                        return Ok(true);
                    }
                    return Ok(false);
                }
                Err(error) => Response {
                    error: Some(format!("invalid session request: {error}")),
                    captured: None,
                },
            },
            Err(error) => Response {
                error: Some(format!("failed to read session request: {error}")),
                captured: None,
            },
        };
        let _ = write_response(&mut stream, &response);
        Ok(false)
    }

    fn write_response(stream: &mut UnixStream, response: &Response) -> Result<()> {
        serde_json::to_writer(&mut *stream, response).context("write session response")?;
        stream.flush().context("flush session response")
    }

    struct State {
        parser: Parser,
        ansi: Vec<u8>,
        host: Host,
        receive: Receiver<Option<Output>>,
        max_bytes: usize,
        closed: bool,
        last_output: Option<Instant>,
        recording: Option<recording::Writer>,
    }

    impl State {
        fn consume(&mut self) -> Result<()> {
            for _ in 0..OUTPUT_BATCH {
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
                        capture::retain(&mut self.ansi, &output.bytes, self.max_bytes)?;
                        self.parser.process(&output.bytes);
                        self.last_output = Some(Instant::now());
                    }
                    Ok(None) | Err(TryRecvError::Disconnected) => {
                        self.closed = true;
                        return Ok(());
                    }
                    Err(TryRecvError::Empty) => return Ok(()),
                }
            }
            Ok(())
        }

        fn respond(&mut self, request: Request) -> Result<Option<Captured>> {
            match request {
                Request::Ping => Ok(None),
                Request::Send { input, pace_ms } => {
                    if self.closed {
                        bail!("session command has exited");
                    }
                    let last = input.len().saturating_sub(1);
                    for (index, bytes) in input.into_iter().enumerate() {
                        self.host.send(&bytes)?;
                        if let Some(recording) = &mut self.recording {
                            recording.input(InputOrigin::Client, &bytes)?;
                        }
                        if pace_ms > 0 && index < last {
                            thread::sleep(Duration::from_millis(pace_ms));
                            self.consume()?;
                        }
                    }
                    Ok(None)
                }
                Request::Wait { text, timeout_ms } => {
                    let deadline = Instant::now() + Duration::from_millis(timeout_ms);
                    loop {
                        self.consume()?;
                        if self.parser.screen().contents().contains(&text) {
                            return Ok(None);
                        }
                        if self.closed {
                            bail!("session ended before visible terminal included {text:?}");
                        }
                        if Instant::now() >= deadline {
                            bail!("timed out waiting for visible terminal text {text:?}");
                        }
                        thread::sleep(Duration::from_millis(10));
                    }
                }
                Request::Snapshot {
                    settle_ms,
                    deadline_ms,
                } => {
                    let started = Instant::now();
                    let deadline = started + Duration::from_millis(deadline_ms);
                    loop {
                        self.consume()?;
                        if self.closed
                            || self.last_output.unwrap_or(started).elapsed()
                                >= Duration::from_millis(settle_ms)
                            || Instant::now() >= deadline
                        {
                            return Ok(Some(Captured {
                                frame: from_screen(self.parser.screen()),
                                ansi: self.ansi.clone(),
                            }));
                        }
                        thread::sleep(Duration::from_millis(10));
                    }
                }
                Request::Close => Ok(None),
            }
        }
    }
}

#[cfg(not(unix))]
mod implementation {
    use super::{Options, Request, Response};
    use anyhow::{Result, bail};
    use std::path::{Path, PathBuf};

    pub fn runtime_dir() -> Result<PathBuf> {
        bail!("persistent sessions require Unix sockets")
    }
    pub fn launch(
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
