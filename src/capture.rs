use std::io::{Read, Write};
use std::path::Path;
use std::sync::mpsc::{self, RecvTimeoutError};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use portable_pty::{CommandBuilder, PtySize, native_pty_system};
use serde::{Deserialize, Serialize};
use vt100::Parser;

use crate::frame::{DEFAULT_BACKGROUND, DEFAULT_FOREGROUND, Frame, from_screen};

const OPENTUI_QUERY: &[u8] = b"\x1b]10;?\x07\x1b]11;?\x07";

pub struct Options {
    pub cols: u16,
    pub rows: u16,
    pub cell_width: u16,
    pub cell_height: u16,
    pub settle: Duration,
    pub deadline: Duration,
    pub input: Vec<u8>,
    pub initial_delay: Duration,
    pub wait_for: Option<String>,
    pub max_bytes: usize,
    pub opentui_host: bool,
}

#[derive(Deserialize, Serialize)]
pub struct Captured {
    pub frame: Frame,
    pub ansi: Vec<u8>,
}

pub fn ansi(bytes: Vec<u8>, rows: u16, cols: u16, max_bytes: usize) -> Result<Captured> {
    if bytes.len() > max_bytes {
        bail!("terminal input exceeds --max-bytes ({max_bytes})");
    }
    let mut parser = terminal(rows, cols);
    parser.process(&bytes);
    Ok(Captured {
        frame: from_screen(parser.screen()),
        ansi: bytes,
    })
}

pub fn command(command: &[String], cwd: Option<&Path>, options: &Options) -> Result<Captured> {
    if command.is_empty() {
        bail!("provide a command after --");
    }
    let pair = native_pty_system()
        .openpty(PtySize {
            rows: options.rows,
            cols: options.cols,
            pixel_width: options.cell_width,
            pixel_height: options.cell_height,
        })
        .context("open pseudo-terminal")?;
    let mut builder = CommandBuilder::new(&command[0]);
    builder.args(&command[1..]);
    builder.env("TERM", "xterm-truecolor");
    builder.env("COLORTERM", "truecolor");
    if let Some(cwd) = cwd {
        builder.cwd(cwd);
    }
    let mut reader = pair.master.try_clone_reader().context("open PTY reader")?;
    let writer = pair.master.take_writer().context("open PTY writer")?;
    let mut child = pair
        .slave
        .spawn_command(builder)
        .context("spawn terminal command")?;
    drop(pair.slave);
    #[cfg(unix)]
    let process_group = child.process_id().and_then(|pid| i32::try_from(pid).ok());
    let (send, receive) = mpsc::sync_channel::<Option<Vec<u8>>>(32);
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
    let result = (|| {
        let mut terminal = terminal(options.rows, options.cols);
        let mut ansi = Vec::new();
        let mut host = Host::new(writer, options);
        let started = Instant::now();
        let deadline = started + options.deadline;
        let closed = consume_until_ready(
            &receive,
            &mut terminal,
            &mut ansi,
            &mut host,
            options,
            started,
            deadline,
        )?;
        if let Some(pattern) = options.wait_for.as_deref()
            && !terminal.screen().contents().contains(pattern)
        {
            bail!(
                "visible terminal did not include --wait-for {pattern:?} before command ended or deadline elapsed"
            );
        }
        if !closed && Instant::now() < deadline && !options.input.is_empty() {
            host.send(&options.input)?;
            consume_until_settled(
                &receive,
                &mut terminal,
                &mut ansi,
                &mut host,
                options,
                deadline,
            )?;
        }
        Ok(Captured {
            frame: from_screen(terminal.screen()),
            ansi,
        })
    })();
    #[cfg(unix)]
    if let Some(process_group) = process_group {
        // portable-pty spawns the application as a session leader; kill its group so helpers do
        // not retain the slave PTY after a frozen snapshot is returned.
        unsafe {
            libc::kill(-process_group, libc::SIGKILL);
        }
    }
    let _ = child.kill();
    drop(receive);
    let teardown_deadline = Instant::now() + Duration::from_secs(1);
    while Instant::now() < teardown_deadline {
        if child.try_wait().ok().flatten().is_some() {
            break;
        }
        thread::sleep(Duration::from_millis(10));
    }
    result
}

pub(crate) fn terminal(rows: u16, cols: u16) -> Parser {
    Parser::new(rows, cols, 0)
}

fn consume_until_ready(
    receive: &mpsc::Receiver<Option<Vec<u8>>>,
    terminal: &mut Parser,
    ansi: &mut Vec<u8>,
    host: &mut Host,
    options: &Options,
    started: Instant,
    deadline: Instant,
) -> Result<bool> {
    let mut closed = false;
    let delay_end = (started + options.initial_delay).min(deadline);
    while !closed && Instant::now() < delay_end {
        closed = matches!(
            receive_chunk(receive, terminal, ansi, host, options.max_bytes, deadline)?,
            Chunk::Closed
        );
    }
    if let Some(pattern) = &options.wait_for {
        while !closed
            && Instant::now() < deadline
            && !terminal.screen().contents().contains(pattern)
        {
            closed = matches!(
                receive_chunk(receive, terminal, ansi, host, options.max_bytes, deadline)?,
                Chunk::Closed
            );
        }
    }
    if closed || Instant::now() >= deadline {
        return Ok(closed);
    }
    if options.wait_for.is_some() && !options.input.is_empty() {
        return Ok(false);
    }
    consume_until_settled(receive, terminal, ansi, host, options, deadline)
}

enum Chunk {
    Output,
    Timeout,
    Closed,
}

fn receive_chunk(
    receive: &mpsc::Receiver<Option<Vec<u8>>>,
    terminal: &mut Parser,
    ansi: &mut Vec<u8>,
    host: &mut Host,
    max_bytes: usize,
    deadline: Instant,
) -> Result<Chunk> {
    let timeout = deadline
        .saturating_duration_since(Instant::now())
        .min(Duration::from_millis(20));
    if timeout.is_zero() {
        return Ok(Chunk::Timeout);
    }
    match receive.recv_timeout(timeout) {
        Ok(Some(bytes)) => {
            host.respond(&bytes)?;
            retain(ansi, &bytes, max_bytes)?;
            terminal.process(&bytes);
            Ok(Chunk::Output)
        }
        Ok(None) | Err(RecvTimeoutError::Disconnected) => Ok(Chunk::Closed),
        Err(RecvTimeoutError::Timeout) => Ok(Chunk::Timeout),
    }
}

fn consume_until_settled(
    receive: &mpsc::Receiver<Option<Vec<u8>>>,
    terminal: &mut Parser,
    ansi: &mut Vec<u8>,
    host: &mut Host,
    options: &Options,
    deadline: Instant,
) -> Result<bool> {
    let mut last_output = Instant::now();
    let mut has_output = false;
    loop {
        match receive_chunk(receive, terminal, ansi, host, options.max_bytes, deadline)? {
            Chunk::Output => {
                has_output = true;
                last_output = Instant::now();
            }
            Chunk::Closed => return Ok(true),
            Chunk::Timeout => {
                if Instant::now() >= deadline {
                    return Ok(false);
                }
            }
        }
        if has_output && last_output.elapsed() >= options.settle {
            return Ok(false);
        }
    }
}

pub(crate) fn retain(ansi: &mut Vec<u8>, bytes: &[u8], max_bytes: usize) -> Result<()> {
    if ansi.len() + bytes.len() > max_bytes {
        bail!("terminal output exceeds --max-bytes ({max_bytes})");
    }
    ansi.extend_from_slice(bytes);
    Ok(())
}

pub(crate) struct Host {
    writer: Box<dyn Write + Send>,
    enabled: bool,
    opentui_replied: bool,
    probe: Vec<u8>,
    pixel_width: u32,
    pixel_height: u32,
}

impl Host {
    pub(crate) fn new(writer: Box<dyn Write + Send>, options: &Options) -> Self {
        Self {
            writer,
            enabled: options.opentui_host,
            opentui_replied: false,
            probe: Vec::new(),
            pixel_width: u32::from(options.cols) * u32::from(options.cell_width),
            pixel_height: u32::from(options.rows) * u32::from(options.cell_height),
        }
    }

    pub(crate) fn send(&mut self, input: &[u8]) -> Result<()> {
        self.writer
            .write_all(input)
            .context("send terminal input")?;
        self.writer.flush().context("flush terminal input")
    }

    pub(crate) fn respond(&mut self, output: &[u8]) -> Result<()> {
        if !self.enabled || self.opentui_replied {
            return Ok(());
        }
        self.probe.extend_from_slice(output);
        if !self
            .probe
            .windows(OPENTUI_QUERY.len())
            .any(|window| window == OPENTUI_QUERY)
        {
            let retain = OPENTUI_QUERY.len().saturating_sub(1);
            if self.probe.len() > retain {
                self.probe.drain(..self.probe.len() - retain);
            }
            return Ok(());
        }
        self.writer
            .write_all(
                format!(
                     "\x1b]10;rgb:{:02x}{:02x}/{:02x}{:02x}/{:02x}{:02x}\x1b\\\x1b]11;rgb:{:02x}{:02x}/{:02x}{:02x}/{:02x}{:02x}\x1b\\\x1bP>|cellshot {}\x1b\\\x1b[1;1R\x1b[?1016;0$y\x1b[?2027;0$y\x1b[?2031;2$y\x1b[?1004;1$y\x1b[?2004;2$y\x1b[?2026;2$y\x1b[?0u\x1b[1;1R\x1b[1;1R\x1b[4;{};{}t\x1b[?6c",
                     DEFAULT_FOREGROUND.r,
                     DEFAULT_FOREGROUND.r,
                     DEFAULT_FOREGROUND.g,
                     DEFAULT_FOREGROUND.g,
                     DEFAULT_FOREGROUND.b,
                     DEFAULT_FOREGROUND.b,
                     DEFAULT_BACKGROUND.r,
                     DEFAULT_BACKGROUND.r,
                     DEFAULT_BACKGROUND.g,
                     DEFAULT_BACKGROUND.g,
                     DEFAULT_BACKGROUND.b,
                     DEFAULT_BACKGROUND.b,
                     env!("CARGO_PKG_VERSION"),
                     self.pixel_height,
                     self.pixel_width,
                )
                .as_bytes(),
            )
            .context("write OpenTUI host response")?;
        self.writer.flush().context("flush OpenTUI host response")?;
        self.opentui_replied = true;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    struct Writer(Arc<Mutex<Vec<u8>>>);

    impl Write for Writer {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn responds_to_split_opentui_query_with_requested_geometry() {
        let result = Arc::new(Mutex::new(Vec::new()));
        let mut host = Host::new(
            Box::new(Writer(result.clone())),
            &Options {
                cols: 100,
                rows: 24,
                cell_width: 9,
                cell_height: 20,
                settle: Duration::ZERO,
                deadline: Duration::ZERO,
                input: Vec::new(),
                initial_delay: Duration::ZERO,
                wait_for: None,
                max_bytes: 1,
                opentui_host: true,
            },
        );

        host.respond(b"\x1b]10;?\x07").unwrap();
        host.respond(b"\x1b]11;?\x07").unwrap();

        let output = String::from_utf8(result.lock().unwrap().clone()).unwrap();
        assert!(output.contains("\x1b[4;480;900t"));
    }
}
