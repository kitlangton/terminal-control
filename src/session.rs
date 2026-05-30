use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};

use crate::capture::{Captured, Options};

#[derive(Serialize, Deserialize)]
enum Request {
    Ping,
    Wait { text: String, timeout_ms: u64 },
    Send { input: Vec<u8> },
    Snapshot { settle_ms: u64, deadline_ms: u64 },
    Close,
}

#[derive(Serialize, Deserialize)]
struct Response {
    error: Option<String>,
    captured: Option<Captured>,
}

pub fn launch(name: &str, command: &[String], cwd: Option<&Path>, options: &Options) -> Result<()> {
    validate_name(name)?;
    implementation::launch(name, command, cwd, options)
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

pub fn send(name: &str, input: Vec<u8>) -> Result<()> {
    request(name, Request::Send { input })?;
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
    options: Options,
) -> Result<()> {
    implementation::serve(socket, command, cwd, options)
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

    pub fn runtime_dir() -> Result<PathBuf> {
        let path = std::env::var_os("CELLSHOT_RUNTIME_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                PathBuf::from(format!("/tmp/cellshot-{}", unsafe { libc::geteuid() }))
            });
        fs::create_dir_all(&path).with_context(|| format!("create {}", path.display()))?;
        Ok(path)
    }

    pub fn launch(
        name: &str,
        command: &[String],
        cwd: Option<&Path>,
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
        options: Options,
    ) -> Result<()> {
        ensure_socket_path(&socket)?;
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
                        if send.send(Some(buffer[..len].to_vec())).is_err() {
                            return;
                        }
                    }
                    Err(_) => break,
                }
            }
            let _ = send.send(None);
        });
        let listener =
            UnixListener::bind(&socket).with_context(|| format!("bind {}", socket.display()))?;
        listener
            .set_nonblocking(true)
            .context("set session socket nonblocking")?;
        let mut state = State {
            parser: capture::terminal(options.rows, options.cols),
            ansi: Vec::new(),
            host: Host::new(writer, &options),
            receive,
            max_bytes: options.max_bytes,
            closed: false,
            last_output: None,
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
                Ok((mut stream, _)) => {
                    let request =
                        serde_json::from_reader(&mut stream).context("parse session request")?;
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
                    serde_json::to_writer(&mut stream, &response)
                        .context("write session response")?;
                    stream.flush().context("flush session response")?;
                    if close {
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

    struct State {
        parser: Parser,
        ansi: Vec<u8>,
        host: Host,
        receive: Receiver<Option<Vec<u8>>>,
        max_bytes: usize,
        closed: bool,
        last_output: Option<Instant>,
    }

    impl State {
        fn consume(&mut self) -> Result<()> {
            loop {
                match self.receive.try_recv() {
                    Ok(Some(bytes)) => {
                        self.host.respond(&bytes)?;
                        capture::retain(&mut self.ansi, &bytes, self.max_bytes)?;
                        self.parser.process(&bytes);
                        self.last_output = Some(Instant::now());
                    }
                    Ok(None) | Err(TryRecvError::Disconnected) => {
                        self.closed = true;
                        return Ok(());
                    }
                    Err(TryRecvError::Empty) => return Ok(()),
                }
            }
        }

        fn respond(&mut self, request: Request) -> Result<Option<Captured>> {
            match request {
                Request::Ping => Ok(None),
                Request::Send { input } => {
                    if self.closed {
                        bail!("session command has exited");
                    }
                    self.host.send(&input)?;
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
    pub fn launch(_: &str, _: &[String], _: Option<&Path>, _: &Options) -> Result<()> {
        bail!("persistent sessions require Unix sockets")
    }
    pub fn request(_: PathBuf, _: &Request) -> Result<Response> {
        bail!("persistent sessions require Unix sockets")
    }
    pub fn serve(_: PathBuf, _: Vec<String>, _: Option<PathBuf>, _: Options) -> Result<()> {
        bail!("persistent sessions require Unix sockets")
    }
}
