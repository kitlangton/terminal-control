# cellshot

Native terminal visual capture for agents, TUI developers, and review workflows.

[![crates.io](https://img.shields.io/crates/v/cellshot.svg)](https://crates.io/crates/cellshot)
[![CI](https://github.com/kitlangton/cellshot/actions/workflows/ci.yml/badge.svg)](https://github.com/kitlangton/cellshot/actions/workflows/ci.yml)

`cellshot` runs terminal programs at explicit dimensions, interprets their terminal state, and exports reviewable artifacts:

- SVG screenshots with foreground/background styling.
- PNG screenshots derived from SVG.
- JSON styled-frame data for future visual diffs and remote protocols.
- Text snapshots for logs and chat output.
- Raw ANSI/VT streams for replay through alternate terminal backends.

PNG artifacts render at 2x pixel density by default for sharp HiDPI viewing. SVG artifacts remain resolution-independent.

`cellshot` supports both a concise one-shot capture path and named persistent sessions for multi-step terminal interaction.

## See It Work

This is OpenCode, captured by `cellshot` while asking OpenCode to write haikus about being captured by `cellshot`:

![OpenCode answering a playful request for cellshot haikus](https://raw.githubusercontent.com/kitlangton/cellshot/main/docs/screenshots/opencode-haikus.png)

The same persistent session before interaction:

![OpenCode home screen rendered by cellshot](https://raw.githubusercontent.com/kitlangton/cellshot/main/docs/screenshots/opencode-home.png)

Both images were produced from one live TUI process using the session API:

```bash
cellshot launch --name meta --host opentui --cols 112 --rows 35 -- opencode
cellshot wait meta "Ask anything"
cellshot snapshot meta --hide-cursor --out docs/screenshots/opencode-home
cellshot send meta --pace-ms 35 'text:Write exactly three tiny haikus about cellshot photographing OpenCode while you answer. Start with the exact words Pixel paparazzi. Keep it playful and do not use tools.' enter
cellshot wait meta "Build · Big Pickle ·" --timeout-ms 60000
cellshot snapshot meta --hide-cursor --out docs/screenshots/opencode-haikus
cellshot close meta
```

## Requirements

- Rust 1.93 or newer.

The first working backend uses the pure-Rust `vt100` parser to keep installation small and dependable. A Ghostty VT adapter is planned for advanced protocol fidelity; the current published Rust binding was evaluated but its vendored Zig build is not yet dependable on this macOS toolchain.

## Install

Install the public crate from crates.io:

```bash
cargo install cellshot
cellshot --help
```

Update an existing installation:

```bash
cargo install --force cellshot
```

Install the current GitHub source head instead of the latest registry release:

```bash
cargo install --locked --git https://github.com/kitlangton/cellshot cellshot
```

The repository is a binary crate: the installed product is the `cellshot` executable. No application embedding API is promised yet.

## Usage

### Persistent Sessions

Use a persistent session when an agent needs to inspect and drive more than one state of the same running application:

```bash
cellshot launch --name demo --host opentui --cols 112 --rows 34 -- opencode
cellshot wait demo "/connect"
cellshot snapshot demo --out captures/home
cellshot send demo text:/connect enter
cellshot wait demo "Connect a provider"
cellshot snapshot demo --out captures/provider
cellshot close demo
```

`launch` creates a background local PTY owner for the named session. `wait`, `send`, and `snapshot` are separate CLI invocations connected to that same live process, so an agent can navigate a TUI and capture several states without restarting it. Session control currently uses local Unix sockets and therefore supports macOS and Linux.

Use `cellshot close <name>` when finished. Session sockets are owner-only local control endpoints, but they control a live terminal application and can expose its visible contents. If an earlier controller process crashes, relaunching the same name cleans up a stale session socket once no running daemon responds.

### Record A Session

Record a timestamped terminal timeline, then render a video with the real interaction timing preserved:

```bash
cellshot launch --name demo --record captures/demo.cellshot --host opentui -- opencode
cellshot wait demo "Ask anything"
cellshot send demo --pace-ms 35 'text:Write a tiny terminal haiku.' enter
cellshot wait demo "haiku" --timeout-ms 60000
cellshot close demo

cellshot video captures/demo.cellshot --hide-cursor \
  --out captures/demo.mp4
```

Recordings are JSON Lines timelines containing terminal output plus client and automatic host input observed until the session is closed. They may contain prompts, secrets, and terminal output; store them accordingly. Wait for the final visible state before `close` when recording a demo. Video export uses `ffmpeg`, omits leading terminal negotiation/blank frames by default, and preserves the original pace after the first visible terminal content. Use `--from-launch` to include startup frames or `--max-idle-ms 600` for an intentionally condensed demo; the `.cellshot` file retains its observed timing for debugging or future renderers.

### One-Shot Capture

Capture a real PTY command after its screen output becomes idle:

```bash
cellshot capture --cols 90 --rows 28 --out captures/colors -- \
  sh -lc 'printf "\033[48;2;30;34;42m\033[38;2;196;215;240m  cellshot  \033[0m\n\033[31merror\033[0m and \033[32msuccess\033[0m\n"'
```

Use `--pixel-ratio 1` when a smaller PNG is preferable, or `--pixel-ratio 3` for extra-large review assets.

Some CLIs change behavior when stdout is a terminal. Use pipe mode for commands that only produce
their useful output when stdout/stderr are ordinary pipes:

```bash
cellshot capture --mode pipe --cols 100 --rows 16 \
  --out captures/streak -- bunx opcd-streak
```

Pipe mode does not support `--send` or `--host`; it runs the command with stdin closed, captures
stdout and stderr, normalizes plain line feeds as terminal line endings, and renders the final
visible frame through the same SVG/PNG/JSON/TXT/ANSI export path.

When the surrounding agent environment disables color, force color for the captured command:

```bash
cellshot capture --color always --cols 100 --rows 16 \
  --out captures/streak -- bunx opcd-streak
```

`--color always` removes `NO_COLOR` and sets common force-color variables for the child process.
`--color never` sets common no-color variables. The default, `--color auto`, preserves the
current environment.

Capture a long-running terminal UI after an idle checkpoint or deadline:

```bash
cellshot capture --cols 100 --rows 32 --settle-ms 500 --deadline-ms 4000 \
  --out captures/app -- my-terminal-app
```

Drive a menu open after the application's initial render, then capture the resulting state:

```bash
cellshot capture --cols 100 --rows 32 -s ctrl-p text:model enter \
  --out captures/command-menu -- my-terminal-app
```

Applications with startup logs can be gated until the intended UI has mounted:

```bash
cellshot capture --initial-delay-ms 1500 --wait-for "Commands" \
  -s ctrl-p --out captures/menu -- my-terminal-app
```

Render raw ANSI/VT bytes from stdin:

```bash
printf '\033[44;97m terminal output \033[0m\n' | cellshot ansi --out captures/stdin
```

Each command produces:

```text
captures/colors.svg
captures/colors.png
captures/colors.json
captures/colors.txt
captures/colors.ansi
```

## Agent Quick Reference

An agent driving a single target TUI state can use `capture`:

1. Run `cellshot capture --cols <width> --rows <height> --out <stem> -- <command> [args...]` for a static initial screen.
2. Add `--wait-for '<visible text>'` before `-s` / `--send` when opening a dialog or selecting a view. A missing readiness checkpoint is an error, not a screenshot.
3. Add ordered input after one `-s` flag: key values are `ctrl-p`, `enter`, `escape`, `up`, `down`, `left`, `right`, and `tab`; typed input is `text:<value>`. Example: `-s ctrl-p text:model enter`. Quote events containing spaces, such as `-s ctrl-p 'text:dark mode' enter`.
4. Read `<stem>.txt` to confirm visible labels and `<stem>.json` for structured cells; open `<stem>.png` for visual review. Keep `<stem>.ansi` when diagnosing parsing or host-handshake behavior.
5. Increase `--deadline-ms` when startup is slow, increase `--settle-ms` for animations, and use `--pixel-ratio 1` only when a smaller PNG matters more than sharp review output.
6. Use `--mode pipe` when a CLI emits useful output in ordinary shell capture but appears blank under the default PTY capture.

For multiple states in one live TUI, prefer this sequence:

1. Run `cellshot launch --name <session> [--host opentui] -- <command> [args...]` once.
2. Run `cellshot wait <session> '<visible text>'` before interacting with each expected view.
3. Run `cellshot snapshot <session> --out <stem>` at each state worth reviewing.
4. Run `cellshot send <session> ctrl-p text:model enter` to continue navigating without relaunching.
5. Run `cellshot close <session>` when finished.

Both `capture` and `snapshot` write SVG and PNG visual output plus JSON, text, and raw ANSI artifacts. Use `--no-png` or `--no-svg` only to skip the corresponding visual file. `ansi` performs the same export from a recorded terminal stream or stdin without launching a process.

### Example Agent Calls

Capture the initial app view and inspect it as text before requesting visual review:

```bash
cellshot capture --cols 110 --rows 36 --out /tmp/app-home -- my-terminal-app
cat /tmp/app-home.txt
```

Open a command palette, type a search, accept it, and capture the resulting view in one process launch:

```bash
cellshot capture --cols 110 --rows 36 --deadline-ms 8000 \
  --wait-for "Commands" \
  -s ctrl-p text:theme enter \
  --out /tmp/app-theme -- my-terminal-app
```

Capture a real OpenCode dialog once its welcome screen exposes the `/connect` command hint:

```bash
cellshot capture --cols 112 --rows 34 --deadline-ms 10000 \
  --host opentui --wait-for "/connect" -s text:/connect enter \
  --out /tmp/opencode-connect -- opencode
```

`capture` is efficient for one disposable target state: it launches a command, freezes one frame, and terminates that process group. An ordered input burst avoids relaunching solely for `type -> enter` flows. `--send` remains repeatable when constructing one-shot commands programmatically, while `-s ctrl-p text:model enter` is the concise form. Persistent sessions are efficient for galleries, recording, and longer navigation flows because the TUI remains alive between snapshots.

Use `--host opentui` for OpenTUI programs such as OpenCode that request terminal capability responses during startup. Leave it unset for ordinary terminal programs; the generic capture path does not impersonate a richer terminal host.

## Current Scope

Implemented now:

- PTY command launch at explicit terminal dimensions.
- Pipe-backed command capture for non-interactive CLIs that suppress or alter output on a TTY.
- Named persistent sessions with `launch`, `wait`, `send`, `snapshot`, and `close` on macOS/Linux.
- Timestamped session recording plus real-time video export through `ffmpeg`, with optional idle compression.
- Idle/deadline snapshot capture for running applications.
- Ordered post-readiness input for driving menus and forms (`-s` / `--send`).
- Initial delay and visible-text gates for applications that log before mounting a TUI.
- Input checkpoints: with `--wait-for` plus queued keys/text, interaction begins as soon as the target content appears rather than waiting on continuously animated screens to become idle.
- An opt-in OpenTUI startup handshake response (`--host opentui`) so applications waiting on terminal capabilities can render under capture without changing generic PTY behavior.
- Screen freezing before process teardown, preserving alternate-screen TUI frames in exported artifacts.
- Bounded raw-stream retention (`--max-bytes`, default 16 MiB) and bounded teardown for captured PTY processes.
- ANSI/stdin rendering without process launch.
- Raw VT stream retention for debugging and backend replay.
- Styled visible frame extraction from the initial pure-Rust VT backend.
- SVG, PNG, JSON, text, and ANSI artifact output.
- HiDPI PNG export (`--pixel-ratio`, default `2`).

## OpenTUI Support

`--host opentui` responds to startup and palette capability probes used by current OpenTUI applications such as OpenCode. Kitty graphics are reported unavailable because the current `vt100` renderer does not decode image payloads; this avoids silently dropping application content that chooses a graphics path. The profile is deliberately opt-in so generic terminal commands are not given application-specific host responses.

Next layers:

- Resize and coordinate/click controls for live sessions.
- Session listing, crash metadata, and explicit daemon lifecycle/status inspection.
- Recording manifests, annotations, alternative video formats, and richer playback controls.
- HTML galleries and cell-level visual diffs.
- Native attach UI.
- Authenticated remote/SSH-forwarded control.
- Ghostty VT adapter, bundled deterministic fonts, and richer glyph/protocol rendering.

## Design

The central design choice is to preserve terminal state as structured visual data rather than only retaining ANSI bytes or pixels:

```text
PTY or ANSI bytes
  -> terminal backend state (`vt100` now, Ghostty adapter planned)
  -> cellshot styled frame JSON
  -> SVG / PNG / text / future diffs and galleries
```

This lets terminal screenshots become inspectable and diffable review artifacts rather than opaque image captures.
