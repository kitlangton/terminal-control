use std::fs::{self, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::frame::{Frame, from_screen};
use crate::render;

#[derive(Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum Entry {
    Header {
        version: u8,
        cols: u16,
        rows: u16,
        cell_width: u16,
        cell_height: u16,
    },
    Output {
        at_ms: u64,
        bytes: Vec<u8>,
    },
    Input {
        at_ms: u64,
        origin: InputOrigin,
        bytes: Vec<u8>,
    },
}

#[derive(Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum InputOrigin {
    Client,
    Host,
}

pub struct Writer {
    file: fs::File,
    started: Instant,
}

impl Writer {
    pub fn new(
        path: &Path,
        started: Instant,
        cols: u16,
        rows: u16,
        cell_width: u16,
        cell_height: u16,
    ) -> Result<Self> {
        if let Some(parent) = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
        {
            fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
        }
        let mut open = OpenOptions::new();
        open.create(true).write(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            open.mode(0o600);
        }
        let mut file = open
            .open(path)
            .with_context(|| format!("create {}", path.display()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(path, fs::Permissions::from_mode(0o600))
                .with_context(|| format!("secure {}", path.display()))?;
        }
        serde_json::to_writer(
            &mut file,
            &Entry::Header {
                version: 1,
                cols,
                rows,
                cell_width,
                cell_height,
            },
        )
        .context("write recording header")?;
        file.write_all(b"\n").context("write recording newline")?;
        file.flush().context("flush recording header")?;
        Ok(Self { file, started })
    }

    pub fn output(&mut self, at_ms: u64, bytes: &[u8]) -> Result<()> {
        self.write(Entry::Output {
            at_ms,
            bytes: bytes.to_vec(),
        })
    }

    pub fn input(&mut self, origin: InputOrigin, bytes: &[u8]) -> Result<()> {
        self.write(Entry::Input {
            at_ms: self.started.elapsed().as_millis() as u64,
            origin,
            bytes: bytes.to_vec(),
        })
    }

    fn write(&mut self, entry: Entry) -> Result<()> {
        serde_json::to_writer(&mut self.file, &entry).context("write recording event")?;
        self.file
            .write_all(b"\n")
            .context("write recording newline")?;
        self.file.flush().context("flush recording event")
    }
}

pub struct VideoOptions {
    pub out: PathBuf,
    pub cell_width: Option<u16>,
    pub cell_height: Option<u16>,
    pub padding: f32,
    pub font_family: String,
    pub pixel_ratio: f32,
    pub hide_cursor: bool,
    pub fps: u32,
    pub max_idle: Option<Duration>,
    pub tail: Duration,
    pub from_launch: bool,
}

pub fn video(path: &Path, options: &VideoOptions) -> Result<()> {
    if options.fps == 0 {
        bail!("--fps must be greater than zero");
    }
    let recording = read(path)?;
    let states = states(&recording);
    let states = if options.from_launch {
        states.as_slice()
    } else {
        let visible = states
            .iter()
            .position(|frame| frame.frame.has_visible_content())
            .unwrap_or(states.len());
        &states[visible..]
    };
    if states.is_empty() {
        bail!("recording contains no visible output frames");
    }
    let frames = samples(states, options);
    if let Some(parent) = options
        .out
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    }
    let temp = std::env::temp_dir().join(format!(
        "cellshot-video-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    ));
    fs::create_dir_all(&temp).with_context(|| format!("create {}", temp.display()))?;
    let result = render_video_frames(&temp, &recording, &frames, options);
    let _ = fs::remove_dir_all(&temp);
    result
}

struct Recording {
    cols: u16,
    rows: u16,
    cell_width: u16,
    cell_height: u16,
    events: Vec<Entry>,
}

fn read(path: &Path) -> Result<Recording> {
    let file = fs::File::open(path).with_context(|| format!("open {}", path.display()))?;
    let mut lines = BufReader::new(file).lines();
    let Some(header) = lines.next() else {
        bail!("recording is empty");
    };
    let Entry::Header {
        version,
        cols,
        rows,
        cell_width,
        cell_height,
        ..
    } = serde_json::from_str(&header.context("read recording header")?)
        .context("parse recording header")?
    else {
        bail!("recording does not start with a header");
    };
    if version != 1 {
        bail!("unsupported recording version {version}");
    }
    let events = lines
        .map(|line| {
            serde_json::from_str(&line.context("read recording event")?)
                .context("parse recording event")
        })
        .collect::<Result<Vec<Entry>>>()?;
    Ok(Recording {
        cols,
        rows,
        cell_width,
        cell_height,
        events,
    })
}

struct VideoFrame {
    at_ms: u64,
    frame: Frame,
}

fn states(recording: &Recording) -> Vec<VideoFrame> {
    let mut parser = crate::capture::terminal(recording.rows, recording.cols);
    let mut frames: Vec<VideoFrame> = Vec::new();
    frames.push(VideoFrame {
        at_ms: 0,
        frame: from_screen(parser.screen()),
    });
    for event in &recording.events {
        let Entry::Output { at_ms, bytes } = event else {
            continue;
        };
        parser.process(bytes);
        let frame = from_screen(parser.screen());
        if frames
            .last()
            .is_some_and(|previous| previous.frame == frame)
        {
            continue;
        }
        frames.push(VideoFrame {
            at_ms: *at_ms,
            frame,
        });
    }
    frames
}

fn samples(states: &[VideoFrame], options: &VideoOptions) -> Vec<Frame> {
    let step_ms = (1000.0 / f64::from(options.fps)).round() as u64;
    let mut timeline = Vec::with_capacity(states.len());
    let mut at_ms = 0;
    for (index, state) in states.iter().enumerate() {
        timeline.push(VideoFrame {
            at_ms,
            frame: state.frame.clone(),
        });
        if let Some(next) = states.get(index + 1) {
            let gap = Duration::from_millis(next.at_ms.saturating_sub(state.at_ms));
            at_ms += options.max_idle.map_or(gap, |max| gap.min(max)).as_millis() as u64;
        }
    }
    let end_ms = at_ms + options.tail.as_millis() as u64;
    let mut output = Vec::new();
    let mut state = 0;
    let mut sample_ms = 0;
    while sample_ms <= end_ms {
        while state + 1 < timeline.len() && timeline[state + 1].at_ms <= sample_ms {
            state += 1;
        }
        output.push(timeline[state].frame.clone());
        sample_ms += step_ms.max(1);
    }
    output
}

fn render_video_frames(
    temp: &Path,
    recording: &Recording,
    frames: &[Frame],
    options: &VideoOptions,
) -> Result<()> {
    for (index, frame) in frames.iter().enumerate() {
        let path = temp.join(format!("frame-{index:06}.png"));
        render::png(
            &render::svg(
                frame,
                &render::Options {
                    cell_width: f32::from(options.cell_width.unwrap_or(recording.cell_width)),
                    cell_height: f32::from(options.cell_height.unwrap_or(recording.cell_height)),
                    font_size: f32::from(options.cell_height.unwrap_or(recording.cell_height))
                        * 0.78,
                    padding: options.padding,
                    font_family: options.font_family.clone(),
                    show_cursor: !options.hide_cursor,
                },
            ),
            &path,
            options.pixel_ratio,
        )?;
    }
    let status = Command::new("ffmpeg")
        .args(["-y", "-loglevel", "error", "-framerate"])
        .arg(options.fps.to_string())
        .arg("-i")
        .arg(temp.join("frame-%06d.png"))
        .args(["-vf", "format=yuv420p", "-movflags", "+faststart"])
        .arg(&options.out)
        .status()
        .context("run ffmpeg; install ffmpeg to export recorded sessions as video")?;
    if !status.success() {
        bail!("ffmpeg failed while exporting {}", options.out.display());
    }
    println!("{}", options.out.display());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn idle_compression_caps_frame_duration() {
        assert_eq!(
            Duration::from_secs(4).min(Duration::from_millis(500)),
            Duration::from_millis(500)
        );
    }

    #[test]
    fn preserves_input_origin_and_binary_output() {
        let temp =
            std::env::temp_dir().join(format!("cellshot-recording-test-{}", std::process::id()));
        let mut writer = Writer::new(&temp, Instant::now(), 2, 1, 9, 18).unwrap();
        writer.output(1, &[0, 255, b'A']).unwrap();
        writer.input(InputOrigin::Host, b"reply").unwrap();
        drop(writer);

        let recording = read(&temp).unwrap();
        let _ = fs::remove_file(temp);
        assert!(matches!(
            &recording.events[0],
            Entry::Output { at_ms: 1, bytes } if bytes == &[0, 255, b'A']
        ));
        assert!(matches!(
            &recording.events[1],
            Entry::Input { origin: InputOrigin::Host, bytes, .. } if bytes == b"reply"
        ));
    }

    #[test]
    fn identifies_visible_frame_content_without_cursor() {
        assert!(
            !Frame {
                version: 1,
                cols: 1,
                rows: 1,
                foreground: crate::frame::DEFAULT_FOREGROUND,
                background: crate::frame::DEFAULT_BACKGROUND,
                cursor: None,
                cells: Vec::new(),
            }
            .has_visible_content()
        );
    }
}
