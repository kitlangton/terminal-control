# AGENTS.md

## Repository

- `cellshot` is a Rust library and CLI binary. Public CLI vocabulary is `show` for reading visible terminal state, `save` for explicit retained artifacts, a named `session` for the live terminal lifecycle managed by flat control commands, `logs` for readable retained output, and `video` for a recorded timeline export.
- Keep `README.md` and the Clap help in `src/main.rs` aligned when changing commands, formats, sessions, recording, or OpenTUI support.
- Prefer focused fixes with unit tests in the affected module.

## Validation

Run the CI checks before finishing code changes:

```bash
cargo fmt --all -- --check
cargo test
cargo clippy --all-targets -- -D warnings
cargo build --release
cargo package --list
```

## Artifacts

- Do not commit generated `.ansi`, `.json`, `.svg`, or `.txt` sidecars under `docs/screenshots/`; PNG documentation images remain commit-eligible.
- Treat `.cellshot` recordings and terminal artifacts as potentially sensitive because they may contain terminal output plus client or host input.
