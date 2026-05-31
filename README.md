# cellshot

Native screenshots and interaction recordings of real terminal applications for agents and TUI review.

[![crates.io](https://img.shields.io/crates/v/cellshot.svg)](https://crates.io/crates/cellshot)
[![CI](https://github.com/kitlangton/cellshot/actions/workflows/ci.yml/badge.svg)](https://github.com/kitlangton/cellshot/actions/workflows/ci.yml)

![OpenCode answering a playful request for cellshot haikus](https://raw.githubusercontent.com/kitlangton/cellshot/main/docs/screenshots/opencode-haikus.png)

Captured from one live OpenCode session using `session` and `shot`.

## Install

Requires Rust 1.93 or newer. Video export also requires `ffmpeg`.

```bash
cargo install cellshot
cellshot --help
```

Install the current repository head instead of the latest crate release:

```bash
cargo install --locked --git https://github.com/kitlangton/cellshot cellshot
```

## Export One Shot

Run a program in a PTY and export its visible terminal state:

```bash
cellshot shot --cols 100 --rows 32 --out captures/home -- my-terminal-app
```

By default, each shot writes inspectable artifacts:

```text
captures/home.png    visual review, rendered at 2x scale by default
captures/home.svg    resolution-independent image
captures/home.txt    visible text
captures/home.json   styled terminal cells
captures/home.ansi   captured terminal stream
```

Wait for an application to mount, then interact before taking the screenshot:

```bash
cellshot shot --cols 100 --rows 32 --wait-for "Commands" \
  -s ctrl-p text:model enter --out captures/model -- my-terminal-app
```

OpenTUI applications such as OpenCode require the opt-in host handshake:

```bash
cellshot shot --host opentui --cols 112 --rows 34 \
  --wait-for "/connect" --out captures/opencode -- opencode
```

## Drive A Live TUI

Use a named session when several shots should come from the same running application:

```bash
cellshot session start demo --host opentui --cols 112 --rows 34 -- opencode
cellshot session status demo --json
cellshot session wait demo "/connect"
cellshot shot --session demo --stdout
cellshot shot --session demo --format png --hide-cursor --out captures/home
cellshot session send demo text:/connect enter
cellshot session resize demo --cols 132 --rows 38
cellshot session wait demo "Connect a provider"
cellshot shot --session demo --format png --hide-cursor --out captures/provider
cellshot session stop demo
```

`session status` reports `running` or `exited`; an exited session retains its final screen for `shot --session` until it is stopped. Use `session list --json` to discover local named sessions and stale sockets.

`session send` accepts `ctrl-a` through `ctrl-z`, keys such as `enter`, `escape`, arrows, `tab`, `shift-tab`, `backspace`, `delete`, `home`, `end`, `page-up`, and `page-down`, plus typed input as `text:<value>`. Use `ctrl-c` to interrupt work or pipe exact prompt bytes with `--stdin`:

```bash
printf '%s' 'Summarize the active view.' | cellshot session send demo --stdin
```

`session resize` controls the terminal viewport and records geometry changes in `.cellshot` timelines when recording is enabled. A session whose retained ANSI transcript has already been truncated cannot be resized because its current screen cannot be replayed at a new size safely.

For normal-screen tools and long-running log processes, inspect retained scrollback directly:

```bash
cellshot session history demo
cellshot session history demo --ansi > captures/demo-history.ansi
```

Full-screen alternate-screen TUIs do not provide terminal scrollback; observe those with `shot` or a recording timeline instead. Session status exposes `history_truncated` after raw retained ANSI reaches `--max-bytes`; the session continues running and retains its most recent transcript bytes.

Restart a single named owner safely when deploying updated code:

```bash
cellshot session restart demo --host opentui --cols 112 --rows 34 -- opencode
```

## Record A Video

Record a session timeline and replay it as an MP4:

```bash
cellshot session start demo --record captures/demo.cellshot \
  --host opentui --cols 112 --rows 34 -- opencode
cellshot session wait demo "Ask anything"
cellshot session send demo --pace-ms 35 'text:Write a short terminal haiku. End with the uppercase form of done.' enter
cellshot session wait demo "DONE" --timeout-ms 60000
cellshot shot --session demo --format png --out captures/answer
cellshot session stop demo

cellshot video captures/demo.cellshot --hide-cursor --out captures/demo.mp4
```

Video export trims startup frames before non-whitespace text by default, while still preserving recordings that only paint terminal backgrounds. Use `--include-startup` to keep all startup frames, or `--max-idle-ms 600` to intentionally shorten quiet gaps.

Recordings are JSON Lines files containing terminal output, client input, and automatic host input; they can include prompts or secrets. Treat them as sensitive artifacts.

## Select Formats And Sources

Repeat `--format` to export only what you need:

```bash
cellshot shot --format png --format txt --out captures/home -- my-terminal-app
```

Write the current visible text directly to stdout for agent inspection, or select JSON/ANSI/SVG explicitly:

```bash
cellshot shot --session demo --stdout
cellshot shot --session demo --stdout --format json
```

For commands whose useful output is piped, use `--pipe`. Pipe shots force color by default; pass `--color never` for plain output:

```bash
cellshot shot --pipe --format png --format txt --cols 100 --rows 16 \
  --out captures/log -- my-command
```

Shots own disposable command processes: once a command shot completes or reaches its deadline, its launched process tree is terminated. Use a named `session` for long-running applications.

Render an existing ANSI/VT terminal stream without launching a process. An `.ansi` file is a conventionally named byte stream of terminal output and escape sequences, not a separate container format:

```bash
printf '\033[44;97m terminal output \033[0m\n' | cellshot shot --input - --out captures/stdin
```

## Rust Library And Formats

The crate also exposes the shot engine, live sessions, and artifact model to Rust callers. The CLI is built on the same `cellshot::shot`, `cellshot::session`, `cellshot::frame`, `cellshot::render`, and `cellshot::recording` modules:

```rust
let shot = cellshot::shot::from_ansi(b"\x1b[32mready\x1b[0m".to_vec(), 1, 20, 1024)?;
assert_eq!(shot.frame.text(), "ready");
let svg = cellshot::render::svg(&shot.frame, &cellshot::render::Options::default());
```

A library session keeps one PTY-backed application in process for fast test interaction without repeatedly invoking the CLI:

```rust
use std::time::Duration;

let mut session = cellshot::session::Session::start(
    &["my-terminal-app".to_owned()],
    None,
    None,
    &cellshot::shot::Options::default(),
)?;
session.wait_for_text("Ready", Duration::from_secs(5))?;
let status = session.status()?;
session.send(b"help\r")?;
session.wait_for_idle(Duration::from_millis(250), Duration::from_secs(5))?;
let capture = session.capture(Duration::from_millis(250), Duration::from_secs(5))?;
let shot = capture.shot;
let exit = session.wait_for_exit(Duration::from_secs(5))?;
session.stop()?;
```

Structured output is versioned for external tools:

- A `--format json` shot is a `Frame` object with `version: 1`, described by `schemas/frame-v1.schema.json`.
- A `.cellshot` recording is JSON Lines: its first line is a versioned header and subsequent lines are timed output, input, or resize entries, each described by `schemas/recording-entry-v1.schema.json`.
- Recording byte arrays contain the original terminal or input bytes as integers from `0` to `255`; recordings can contain sensitive text or input.

`session::Session` is the embedded lifecycle interface; the named CLI session commands and the external driver are adapters over the same implementation.

## External Driver

External agent tooling can keep multiple embedded sessions alive through a versioned JSON Lines protocol over stdin/stdout:

```bash
cellshot driver
```

The driver writes a `hello` message with protocol and cellshot versions, then accepts typed operations including `launch`, `status`, `send`, `waitForText`, `waitForIdle`, `waitForExit`, `shot`, `history`, `recording`, `resize`, `stop`, and `shutdown`. It is intended for clients such as a TypeScript TUI test or agent-control library, while the shell-facing `session` commands remain convenient for individual workflows.

```json
{"type":"hello","protocolVersion":1,"cellshotVersion":"<installed-version>"}
{"id":1,"method":"launch","sessionId":"app","params":{"command":["my-terminal-app"],"cols":100,"rows":30,"inheritEnv":false,"env":{"TERM":"xterm-256color"}}}
{"id":2,"method":"waitForText","sessionId":"app","params":{"text":"Ready","timeoutMs":5000}}
{"id":3,"method":"send","sessionId":"app","params":{"input":[{"type":"text","value":"help"},{"type":"key","value":"enter"}]}}
{"id":4,"method":"shot","sessionId":"app","params":{"settleMs":250,"deadlineMs":5000}}
```

A driver `shot` response contains a structured visible frame, derived `text`, and a capture `reason`: `idle`, `deadline`, `exited`, or `outputclosed`. Raw ANSI is omitted by default; request `includeAnsi: true` for retained transcript bytes or `includeSvg: true` for rendered visual evidence. A test client should normally require `idle` or `exited` instead of accepting a deadline fallback as a stable snapshot. Driver input is intentionally exact: text, raw bytes, known key values, and single-letter control input are supported without claiming unimplemented key chords.

## TypeScript Client

`@cellshot/test` exposes the driver as isolated typed test sessions. It deliberately separates the visible screen from retained history and the exact ANSI/VT transcript. Its npm distribution includes an optional native package for macOS or GNU/Linux on arm64 or x64, so consumers do not need a Rust toolchain or separate `cellshot` installation.

After the initial npm publication:

```bash
bun add -d @cellshot/test vitest
```

```ts
import { createCellshot } from "@cellshot/test"

await using cellshot = await createCellshot({
  artifacts: {
    directory: ".cellshot-artifacts",
    onFailure: true,
    includeTranscript: false,
    includeRecording: true,
  },
})
await using session = await cellshot.launch({
  command: ["/absolute/path/to/my-terminal-app"],
  viewport: { cols: 100, rows: 30 },
  inheritEnv: false,
  env: { TERM: "xterm-256color", HOME: "/tmp/test-home" },
  record: "on-failure",
})

await session.screen.waitForText(/Ready/)
await session.keyboard.type("help")
await session.keyboard.press("Enter")

const text = await session.screen.text()
const frame = await session.screen.frame()
const history = await session.history.text()
const transcript = await session.transcript.ansi()

expect(text).toMatchSnapshot()
expect(frame).toMatchSnapshot()

const exit = await session.waitForExit({ timeoutMs: 5_000 })
expect(exit).toMatchObject({ reason: "exited", exit: { code: 0 } })
```

When working directly from this repository before installing the npm artifacts, pass `binaryPath: "./target/release/cellshot"` or set `CELLSHOT_BINARY`.

`session.screen.text()` and `session.screen.frame()` wait for a settled capture and reject deadline or output-closed fallback by default. A test that intentionally needs an intermediate frame can request it explicitly:

```ts
const capture = await session.screen.capture({ allowIncomplete: true })
console.log(capture.reason, capture.text, capture.frame)
```

This makes ordinary text or frame snapshots stable by default while retaining explicit access to live, incomplete terminal state.

Keyboard presses are typed as the sequences Cellshot encodes exactly, such as `"Enter"`, `"ArrowDown"`, or `"Control+C"`. Use `session.keyboard.write(bytes)` when a test deliberately needs exact terminal bytes outside that supported key set.

Vitest users can add a screen-aware assertion that writes configured artifacts on failure. Standard `toMatchSnapshot()` and `toMatchInlineSnapshot()` remain the simplest snapshot format because visible text is reviewable in source control:

```ts
import { expect } from "vitest"
import { extendCellshotMatchers } from "@cellshot/test/vitest"

extendCellshotMatchers(expect)

await expect(session).toHaveScreenText("Ready\n\nChoose an option")
await expect(session.screen.text()).resolves.toMatchInlineSnapshot()
```

`session.writeArtifacts(name)` and failing `toHaveScreenText(...)` assertions can write `screen.txt`, `screen.json`, `screen.svg`, `history.txt`, and `metadata.json`. Environment variable values are redacted in metadata. `transcript.ansi` and `recording.cellshot` are opt-in because terminal streams and typed input may contain secrets. Wrap ordinary snapshot assertions when evidence should be saved on any thrown assertion:

```ts
await session.withArtifactsOnFailure("settings-snapshot", async () => {
  await expect(session.screen.text()).resolves.toMatchSnapshot()
})
```

Enable a recording with `record: true` or `record: "on-failure"`; a test may explicitly save it before disposing the session:

```ts
await session.resize({ cols: 120, rows: 40 })
await session.saveRecording("artifacts/navigation.cellshot")
```

### npm Release

The npm workspace publishes `@cellshot/test` with fixed-version platform packages: `@cellshot/darwin-arm64`, `@cellshot/darwin-x64`, `@cellshot/linux-arm64-gnu`, and `@cellshot/linux-x64-gnu`. The client is compiled to ESM JavaScript with declarations; each native package receives the release Rust executable during the `npm release` workflow.

The initial unpublished npm packages are prepared at `0.1.0`. For subsequent user-facing npm changes, create a Changeset with `bunx changeset`, commit the generated release metadata, and apply version changes before running the workflow. Run the workflow with `publish: false` to assemble packages only, or `publish: true` to publish assembled tarballs after its clean Bun and Node/Vitest consumer validation passes.

## Notes

- Persistent sessions use owner-only local Unix sockets and are supported on macOS and Linux.
- `--host opentui` answers startup probes needed by current OpenTUI applications; Kitty graphics are reported unavailable because the current renderer does not decode image payloads.
- The current renderer uses a pure-Rust `vt100` terminal backend and exports PNG, SVG, JSON, text, and raw ANSI stream artifacts.
- Run `cellshot <command> --help` for dimensions, timing, color, rendering, and output options.
