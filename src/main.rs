mod capture;
mod frame;
mod recording;
mod render;
mod session;

use std::fs;
use std::io::{self, Read};
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use clap::{Args, Parser, Subcommand, ValueEnum};

const HELP: &str = "\
cellshot is built for terminal UI inspection and agent workflows. It executes a command in a
real pseudo-terminal, captures pipe-only command output, or renders existing ANSI input, then
writes inspectable artifacts from the visible terminal frame. Use the .txt output to inspect
content, .png/.svg for visual review, .json for structured processing, and .ansi to replay or
diagnose the original terminal stream.";

const ROOT_EXAMPLES: &str = "\
Examples:
  cellshot capture --out captures/app -- my-terminal-app
  cellshot capture --cols 100 --rows 32 --wait-for 'Commands' -s ctrl-p --out captures/menu -- my-terminal-app
  cellshot capture --mode pipe --out captures/log -- my-log-command
  cellshot launch --name demo --host opentui -- opencode
  cellshot wait demo '/connect' && cellshot send demo text:/connect enter
  cellshot wait demo 'Connect a provider' && cellshot snapshot demo --out captures/provider
  cellshot close demo
  printf '\\033[32msuccess\\033[0m\\n' | cellshot ansi --out captures/stdin

Use `capture` for one final state or `launch` plus session commands for multi-step workflows.";

const CAPTURE_HELP: &str = "\
Capture flow:
  1. Start COMMAND inside a PTY with TERM=xterm-truecolor, or use --mode pipe for commands
     that only print when stdout/stderr are not terminals.
  2. Optionally wait for --initial-delay-ms and visible --wait-for text.
  3. If --send input is queued, send the ordered events as one input burst.
  4. Freeze the visible frame once PTY output is idle for --settle-ms or --deadline-ms expires.
  5. Write OUT.svg, OUT.png, OUT.json, OUT.txt, and OUT.ansi.

Use --wait-for whenever an interaction must occur only after a UI is mounted. If its text is not
visible before the command exits or deadline expires, capture fails rather than exporting the
wrong screen. Send keys by name and text as `text:<value>`, for example `-s ctrl-p text:model
enter`. For multiple interaction steps on one live application, use `launch`, `wait`, `send`,
`snapshot`, and `close` instead of `capture`.

Use `--host opentui` only for OpenTUI applications, including OpenCode, that query terminal
capabilities before painting their interface. Generic programs do not need a host profile.
Use `--mode pipe` for CLIs that skip output when stdout is a TTY; pipe mode captures stdout and
stderr without terminal input, normalizing plain line feeds as terminal line endings.
Use `--color always` to remove NO_COLOR and set common force-color environment variables.

Examples:
  cellshot capture --host opentui --cols 100 --rows 32 --out captures/home -- opencode
  cellshot capture --host opentui --cols 100 --rows 32 --wait-for '/connect' -s ctrl-p text:model enter --out captures/model -- opencode
  cellshot capture --wait-for 'Choose model' -s down enter --out captures/chosen -- my-tui
  cellshot capture --color always --cols 100 --rows 16 --out captures/streak -- bunx opcd-streak
  cellshot capture --cwd ./app --deadline-ms 8000 --out /tmp/app -- bun run dev";

const ANSI_HELP: &str = "\
ANSI mode does not launch a command. It parses input as a terminal stream at --cols by --rows
and exports the final visible frame.

Examples:
  printf '\\033[44;97m status \\033[0m\\n' | cellshot ansi --out captures/status
  cellshot ansi --cols 120 --rows 40 --input debug.ansi --out captures/replay";

const LAUNCH_HELP: &str = "\
Launch starts one background PTY session and returns once its local control socket is available.
The application stays alive until `cellshot close NAME`, so later commands interact with the
same screen and application state. Persistent sessions currently require macOS or Linux. Session
sockets are local control endpoints protected for the current user; recordings contain terminal
output and any input sent through cellshot, so treat them as sensitive artifacts.

Example:
  cellshot launch --name demo --host opentui --cols 112 --rows 34 -- opencode
  cellshot wait demo '/connect'
  cellshot send demo text:/connect enter
  cellshot wait demo 'Connect a provider'
  cellshot snapshot demo --out captures/provider
  cellshot close demo";

const SEND_HELP: &str = "\
Send one ordered input burst to a live session. Key names are `ctrl-p`, `enter`, `escape`, `up`,
`down`, `left`, `right`, and `tab`; text uses `text:<value>`.
Add `--pace-ms 35` when producing a human-readable recording so typed text appears character by
character in the terminal instead of as one immediate paste.

Examples:
  cellshot send demo ctrl-p text:model enter
  cellshot send demo --pace-ms 35 'text:Write a terminal haiku.' enter";

const VIDEO_HELP: &str = "\
Replay a recording produced by `launch --record` into a video artifact. Output is sampled at --fps and
begins at the first visible terminal content while preserving real timing afterward. Pass
--from-launch to include blank startup/negotiation frames or --max-idle-ms when you explicitly
want to shorten long quiet gaps for a condensed edit. The source `.cellshot` file retains observed
timing, terminal bytes, client input, and automatic host input until the session is closed.
Video export requires `ffmpeg` to be installed.

Example:
  cellshot launch --name demo --record captures/demo.cellshot -- opencode
  cellshot send demo text:/connect enter
  cellshot close demo
  cellshot video captures/demo.cellshot --out captures/demo.mp4";

#[derive(Parser)]
#[command(
    name = "cellshot",
    version,
    about = "Capture styled terminal frames as SVG, PNG, JSON, and text",
    long_about = HELP,
    after_help = ROOT_EXAMPLES
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run a terminal command under a PTY and capture its settled screen.
    #[command(long_about = "Run a terminal command under a PTY and capture its settled visible screen.", after_help = CAPTURE_HELP)]
    Capture(CaptureArgs),
    /// Start a named persistent terminal session.
    #[command(after_help = LAUNCH_HELP)]
    Launch(LaunchArgs),
    /// Wait until a named session includes visible text.
    Wait(WaitArgs),
    /// Send ordered input to a named session.
    #[command(after_help = SEND_HELP)]
    Send(SendArgs),
    /// Export the current settled screen from a named session.
    Snapshot(SnapshotArgs),
    /// Terminate a named session.
    Close(SessionArgs),
    /// Export a video from a recorded persistent session.
    #[command(after_help = VIDEO_HELP)]
    Video(VideoArgs),
    /// Render ANSI/VT bytes from a file or stdin without spawning a process.
    #[command(long_about = "Render ANSI/VT bytes from a file or stdin without spawning a process.", after_help = ANSI_HELP)]
    Ansi(AnsiArgs),
    #[command(name = "__serve", hide = true)]
    Serve(ServeArgs),
}

#[derive(Args)]
struct RenderArgs {
    /// Cell width used for terminal geometry and rendering.
    #[arg(long, default_value_t = 9)]
    cell_width: u16,
    /// Cell height used for terminal geometry and rendering.
    #[arg(long, default_value_t = 18)]
    cell_height: u16,
    /// Outer padding around the rendered terminal in pixels.
    #[arg(long, default_value_t = 18.0)]
    padding: f32,
    /// Font family used in SVG/PNG output.
    #[arg(
        long,
        default_value = "JetBrains Mono, SFMono-Regular, Menlo, monospace"
    )]
    font_family: String,
    /// Scale PNG output for sharp HiDPI viewing; SVG output is unchanged.
    #[arg(long, default_value_t = 2.0)]
    pixel_ratio: f32,
    /// Output path stem; extensions are added automatically.
    #[arg(short, long, default_value = "capture")]
    out: PathBuf,
    /// Hide the terminal cursor in rendered output.
    #[arg(long)]
    hide_cursor: bool,
    /// Do not write the PNG artifact.
    #[arg(long)]
    no_png: bool,
    /// Do not write the SVG artifact.
    #[arg(long)]
    no_svg: bool,
}

#[derive(Args)]
struct OutputArgs {
    /// Terminal width in cells.
    #[arg(long, default_value_t = 80)]
    cols: u16,
    /// Terminal height in cells.
    #[arg(long, default_value_t = 24)]
    rows: u16,
    #[command(flatten)]
    render: RenderArgs,
}

#[derive(Args)]
struct CaptureArgs {
    #[command(flatten)]
    output: OutputArgs,
    /// Capture backend to use for the command.
    #[arg(long, value_enum, default_value = "pty")]
    mode: CaptureMode,
    /// Color environment policy for the captured command.
    #[arg(long, value_enum, default_value = "auto")]
    color: ColorMode,
    /// Capture after this many milliseconds without PTY output.
    #[arg(long, default_value_t = 250)]
    settle_ms: u64,
    /// Capture and terminate after this deadline even if output continues.
    #[arg(long, default_value_t = 5000)]
    deadline_ms: u64,
    /// Wait this long before allowing the initial screen to settle.
    #[arg(long, default_value_t = 0)]
    initial_delay_ms: u64,
    /// Wait until the visible terminal includes this text before interacting or capturing.
    #[arg(long)]
    wait_for: Option<String>,
    /// Fail if terminal output exceeds this many bytes.
    #[arg(long, default_value_t = 16 * 1024 * 1024)]
    max_bytes: usize,
    /// Working directory for the terminal command.
    #[arg(long)]
    cwd: Option<PathBuf>,
    /// Terminal-host compatibility response profile.
    #[arg(long, value_enum)]
    host: Option<HostProfile>,
    /// Ordered input after readiness: key name or `text:<value>` (repeatable/groupable).
    #[arg(short = 's', long, value_name = "INPUT", num_args = 1..)]
    send: Vec<String>,
    /// Command and arguments to launch, following `--`.
    #[arg(required = true, trailing_var_arg = true, allow_hyphen_values = true)]
    command: Vec<String>,
}

#[derive(Args)]
struct LaunchArgs {
    /// Stable local name used by later session commands.
    #[arg(long)]
    name: String,
    /// Terminal width in cells.
    #[arg(long, default_value_t = 80)]
    cols: u16,
    /// Terminal height in cells.
    #[arg(long, default_value_t = 24)]
    rows: u16,
    /// Terminal cell width in pixels.
    #[arg(long, default_value_t = 9)]
    cell_width: u16,
    /// Terminal cell height in pixels.
    #[arg(long, default_value_t = 18)]
    cell_height: u16,
    /// Maximum raw terminal bytes retained by the live session.
    #[arg(long, default_value_t = 16 * 1024 * 1024)]
    max_bytes: usize,
    /// Working directory for the terminal command.
    #[arg(long)]
    cwd: Option<PathBuf>,
    /// Write timestamped terminal output and all sent input to this private recording file.
    #[arg(long)]
    record: Option<PathBuf>,
    /// Terminal-host compatibility response profile.
    #[arg(long, value_enum)]
    host: Option<HostProfile>,
    /// Command and arguments to launch, following `--`.
    #[arg(required = true, trailing_var_arg = true, allow_hyphen_values = true)]
    command: Vec<String>,
}

#[derive(Args)]
struct WaitArgs {
    /// Name of a running session.
    name: String,
    /// Visible text that must appear in the session screen.
    text: String,
    /// Maximum time to wait before returning an error.
    #[arg(long, default_value_t = 5000)]
    timeout_ms: u64,
}

#[derive(Args)]
struct SendArgs {
    /// Name of a running session.
    name: String,
    /// Delay between input atoms; text is split into characters when set.
    #[arg(long, default_value_t = 0)]
    pace_ms: u64,
    /// Ordered input: key name or `text:<value>`.
    #[arg(value_name = "INPUT", num_args = 1..)]
    input: Vec<String>,
}

#[derive(Args)]
struct SnapshotArgs {
    /// Name of a running session.
    name: String,
    #[command(flatten)]
    output: RenderArgs,
    /// Wait for this many milliseconds without output before freezing the frame.
    #[arg(long, default_value_t = 250)]
    settle_ms: u64,
    /// Return a frame after this deadline even if output continues.
    #[arg(long, default_value_t = 5000)]
    deadline_ms: u64,
}

#[derive(Args)]
struct SessionArgs {
    /// Name of a running session.
    name: String,
}

#[derive(Args)]
struct ServeArgs {
    #[arg(long)]
    socket: PathBuf,
    #[arg(long)]
    cwd: Option<PathBuf>,
    #[arg(long)]
    record: Option<PathBuf>,
    #[arg(long)]
    opentui_host: bool,
    #[arg(long)]
    cols: u16,
    #[arg(long)]
    rows: u16,
    #[arg(long)]
    cell_width: u16,
    #[arg(long)]
    cell_height: u16,
    #[arg(long)]
    max_bytes: usize,
    #[arg(required = true, trailing_var_arg = true, allow_hyphen_values = true)]
    command: Vec<String>,
}

#[derive(Args)]
struct VideoArgs {
    /// Recording created by `launch --record`.
    input: PathBuf,
    /// Override the recorded terminal cell width in rendered pixels.
    #[arg(long)]
    cell_width: Option<u16>,
    /// Override the recorded terminal cell height in rendered pixels.
    #[arg(long)]
    cell_height: Option<u16>,
    /// Outer padding around the rendered terminal in pixels.
    #[arg(long, default_value_t = 18.0)]
    padding: f32,
    /// Font family used in video output.
    #[arg(
        long,
        default_value = "JetBrains Mono, SFMono-Regular, Menlo, monospace"
    )]
    font_family: String,
    /// Scale video frames for sharp HiDPI viewing.
    #[arg(long, default_value_t = 2.0)]
    pixel_ratio: f32,
    /// Output video file path.
    #[arg(short, long, default_value = "capture.mp4")]
    out: PathBuf,
    /// Hide the terminal cursor in rendered output.
    #[arg(long)]
    hide_cursor: bool,
    /// Maximum sampled frames per second.
    #[arg(long, default_value_t = 20)]
    fps: u32,
    /// Optionally collapse longer gaps between changed screens to this duration.
    #[arg(long)]
    max_idle_ms: Option<u64>,
    /// Hold the final frame for this duration.
    #[arg(long, default_value_t = 1000)]
    tail_ms: u64,
    /// Include leading contentless startup/terminal negotiation frames.
    #[arg(long)]
    from_launch: bool,
}

#[derive(Args)]
struct AnsiArgs {
    #[command(flatten)]
    output: OutputArgs,
    /// ANSI/VT input file; defaults to stdin.
    #[arg(long)]
    input: Option<PathBuf>,
    /// Fail if ANSI input exceeds this many bytes.
    #[arg(long, default_value_t = 16 * 1024 * 1024)]
    max_bytes: usize,
}

#[derive(Clone, Copy, ValueEnum)]
enum HostProfile {
    /// Respond to OpenTUI startup terminal capability queries.
    Opentui,
}

#[derive(Clone, Copy, ValueEnum)]
enum CaptureMode {
    /// Run the command inside a pseudo-terminal.
    Pty,
    /// Run the command with stdout/stderr pipes and render plain output as terminal text.
    Pipe,
}

#[derive(Clone, Copy, ValueEnum)]
enum ColorMode {
    /// Preserve the current process color environment.
    Auto,
    /// Remove NO_COLOR and set common force-color environment variables.
    Always,
    /// Set common no-color environment variables.
    Never,
}

impl From<ColorMode> for capture::ColorMode {
    fn from(value: ColorMode) -> Self {
        match value {
            ColorMode::Auto => capture::ColorMode::Auto,
            ColorMode::Always => capture::ColorMode::Always,
            ColorMode::Never => capture::ColorMode::Never,
        }
    }
}

fn main() -> Result<()> {
    match Cli::parse().command {
        Command::Capture(args) => {
            validate_terminal_size(args.output.cols, args.output.rows)?;
            if matches!(args.mode, CaptureMode::Pipe) && !args.send.is_empty() {
                bail!("--send is only supported with --mode pty");
            }
            if matches!(args.mode, CaptureMode::Pipe) && args.host.is_some() {
                bail!("--host is only supported with --mode pty");
            }
            let options = capture::Options {
                cols: args.output.cols,
                rows: args.output.rows,
                cell_width: args.output.render.cell_width,
                cell_height: args.output.render.cell_height,
                settle: Duration::from_millis(args.settle_ms),
                deadline: Duration::from_millis(args.deadline_ms),
                input: capture_input(&args.send)?,
                initial_delay: Duration::from_millis(args.initial_delay_ms),
                wait_for: args.wait_for,
                max_bytes: args.max_bytes,
                opentui_host: matches!(args.host, Some(HostProfile::Opentui)),
                color: args.color.into(),
            };
            let captured = match args.mode {
                CaptureMode::Pty => capture::command(&args.command, args.cwd.as_deref(), &options),
                CaptureMode::Pipe => {
                    capture::pipe_command(&args.command, args.cwd.as_deref(), &options)
                }
            }?;
            write_outputs(&captured, &args.output.render)?;
        }
        Command::Launch(args) => {
            validate_terminal_size(args.cols, args.rows)?;
            session::launch(
                &args.name,
                &args.command,
                args.cwd.as_deref(),
                args.record.as_deref(),
                &capture::Options {
                    cols: args.cols,
                    rows: args.rows,
                    cell_width: args.cell_width,
                    cell_height: args.cell_height,
                    settle: Duration::ZERO,
                    deadline: Duration::ZERO,
                    input: Vec::new(),
                    initial_delay: Duration::ZERO,
                    wait_for: None,
                    max_bytes: args.max_bytes,
                    opentui_host: matches!(args.host, Some(HostProfile::Opentui)),
                    color: capture::ColorMode::Auto,
                },
            )?;
            println!("{}", args.name);
        }
        Command::Wait(args) => {
            session::wait(
                &args.name,
                args.text,
                Duration::from_millis(args.timeout_ms),
            )?;
        }
        Command::Send(args) => {
            session::send(
                &args.name,
                session_input(&args.input, args.pace_ms > 0)?,
                Duration::from_millis(args.pace_ms),
            )?;
        }
        Command::Snapshot(args) => {
            let captured = session::snapshot(
                &args.name,
                Duration::from_millis(args.settle_ms),
                Duration::from_millis(args.deadline_ms),
            )?;
            write_outputs(&captured, &args.output)?;
        }
        Command::Close(args) => {
            session::close(&args.name)?;
        }
        Command::Video(args) => {
            recording::video(
                &args.input,
                &recording::VideoOptions {
                    out: args.out,
                    cell_width: args.cell_width,
                    cell_height: args.cell_height,
                    padding: args.padding,
                    font_family: args.font_family,
                    pixel_ratio: args.pixel_ratio,
                    hide_cursor: args.hide_cursor,
                    fps: args.fps,
                    max_idle: args.max_idle_ms.map(Duration::from_millis),
                    tail: Duration::from_millis(args.tail_ms),
                    from_launch: args.from_launch,
                },
            )?;
        }
        Command::Ansi(args) => {
            validate_terminal_size(args.output.cols, args.output.rows)?;
            let mut input = Vec::new();
            let limit = args.max_bytes.saturating_add(1) as u64;
            if let Some(path) = args.input.as_ref() {
                fs::File::open(path)
                    .with_context(|| format!("open {}", path.display()))?
                    .take(limit)
                    .read_to_end(&mut input)
                    .with_context(|| format!("read {}", path.display()))?;
            } else {
                io::stdin()
                    .take(limit)
                    .read_to_end(&mut input)
                    .context("read ANSI input")?;
            }
            if input.len() > args.max_bytes {
                bail!("terminal input exceeds --max-bytes ({})", args.max_bytes);
            }
            let captured =
                capture::ansi(input, args.output.rows, args.output.cols, args.max_bytes)?;
            write_outputs(&captured, &args.output.render)?;
        }
        Command::Serve(args) => {
            session::serve(
                args.socket,
                args.command,
                args.cwd,
                args.record,
                capture::Options {
                    cols: args.cols,
                    rows: args.rows,
                    cell_width: args.cell_width,
                    cell_height: args.cell_height,
                    settle: Duration::ZERO,
                    deadline: Duration::ZERO,
                    input: Vec::new(),
                    initial_delay: Duration::ZERO,
                    wait_for: None,
                    max_bytes: args.max_bytes,
                    opentui_host: args.opentui_host,
                    color: capture::ColorMode::Auto,
                },
            )?;
        }
    }
    Ok(())
}

fn capture_input(events: &[String]) -> Result<Vec<u8>> {
    let mut input = Vec::new();
    for event in events {
        if let Some(text) = event.strip_prefix("text:") {
            input.extend_from_slice(text.as_bytes());
            continue;
        }
        input.extend_from_slice(match event.as_str() {
            "ctrl-p" => b"\x10",
            "enter" => b"\r",
            "escape" | "esc" => b"\x1b",
            "up" => b"\x1b[A",
            "down" => b"\x1b[B",
            "left" => b"\x1b[D",
            "right" => b"\x1b[C",
            "tab" => b"\t",
            _ => anyhow::bail!(
                "unsupported --send event {event:?}; use text:<value>, ctrl-p, enter, escape, up, down, left, right, or tab"
            ),
        });
    }
    Ok(input)
}

fn session_input(events: &[String], paced: bool) -> Result<Vec<Vec<u8>>> {
    if !paced {
        return Ok(vec![capture_input(events)?]);
    }
    let mut input = Vec::new();
    for event in events {
        if let Some(text) = event.strip_prefix("text:") {
            input.extend(text.chars().map(|char| char.to_string().into_bytes()));
            continue;
        }
        input.push(capture_input(std::slice::from_ref(event))?);
    }
    Ok(input)
}

fn validate_terminal_size(cols: u16, rows: u16) -> Result<()> {
    if cols == 0 || rows == 0 {
        bail!("terminal dimensions must be greater than zero");
    }
    Ok(())
}

fn write_outputs(captured: &capture::Captured, args: &RenderArgs) -> Result<()> {
    if let Some(parent) = args
        .out
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    }
    let json_path = args.out.with_extension("json");
    let text_path = args.out.with_extension("txt");
    let ansi_path = args.out.with_extension("ansi");
    fs::write(&json_path, serde_json::to_vec_pretty(&captured.frame)?)
        .with_context(|| format!("write {}", json_path.display()))?;
    fs::write(&text_path, captured.frame.text())
        .with_context(|| format!("write {}", text_path.display()))?;
    fs::write(&ansi_path, &captured.ansi)
        .with_context(|| format!("write {}", ansi_path.display()))?;
    let svg = (!args.no_svg || !args.no_png).then(|| {
        render::svg(
            &captured.frame,
            &render::Options {
                cell_width: f32::from(args.cell_width),
                cell_height: f32::from(args.cell_height),
                font_size: f32::from(args.cell_height) * 0.78,
                padding: args.padding,
                font_family: args.font_family.clone(),
                show_cursor: !args.hide_cursor,
            },
        )
    });
    if let Some(svg) = svg.as_ref().filter(|_| !args.no_svg) {
        let path = args.out.with_extension("svg");
        fs::write(&path, svg).with_context(|| format!("write {}", path.display()))?;
        println!("{}", path.display());
    }
    if let Some(svg) = svg.as_ref().filter(|_| !args.no_png) {
        let path = args.out.with_extension("png");
        render::png(svg, &path, args.pixel_ratio)?;
        println!("{}", path.display());
    }
    println!("{}", json_path.display());
    println!("{}", text_path.display());
    println!("{}", ansi_path.display());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preserves_ordered_input_events() {
        assert_eq!(
            capture_input(&[
                "ctrl-p".to_owned(),
                "text:model".to_owned(),
                "enter".to_owned()
            ])
            .unwrap(),
            b"\x10model\r"
        );
    }

    #[test]
    fn rejects_unsupported_input_events() {
        assert!(capture_input(&["space".to_owned()]).is_err());
    }

    #[test]
    fn parses_compact_ordered_input_sequence() {
        let cli = Cli::try_parse_from([
            "cellshot",
            "capture",
            "--wait-for",
            "ready",
            "-s",
            "ctrl-p",
            "text:model",
            "enter",
            "--",
            "app",
        ])
        .unwrap();
        let Command::Capture(args) = cli.command else {
            panic!("expected capture command");
        };
        assert_eq!(args.send, ["ctrl-p", "text:model", "enter"]);
    }

    #[test]
    fn rejects_zero_terminal_dimensions() {
        assert!(validate_terminal_size(0, 24).is_err());
        assert!(validate_terminal_size(80, 0).is_err());
    }

    #[test]
    fn paced_session_input_splits_text_without_splitting_keys() {
        assert_eq!(
            session_input(&["text:hi".to_owned(), "enter".to_owned()], true).unwrap(),
            vec![b"h".to_vec(), b"i".to_vec(), b"\r".to_vec()]
        );
    }
}
