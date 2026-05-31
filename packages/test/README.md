# @cellshot/test

Typed terminal application testing client for `cellshot driver`, with stable screen snapshots, keyboard interaction, recordings, and opt-in failure evidence.

Install the package with Vitest after the initial npm publication:

```bash
bun add -d @cellshot/test vitest
```

The matching native `cellshot` binary is installed automatically on macOS or GNU/Linux on arm64 or x64:

```ts
import { createCellshot } from "@cellshot/test"

await using cellshot = await createCellshot()
```

For development or custom native builds, the runtime resolves an explicit `binaryPath` first, then `CELLSHOT_BINARY`, before the installed native package.

Visible screen text and frames are stable snapshot surfaces:

```ts
await using session = await cellshot.launch({ command: ["my-tui"] })
await session.screen.waitForText("Ready")
expect(await session.screen.text()).toMatchSnapshot()
```

Artifact and recording configuration is opt-in because terminal output and input may contain secrets. See the repository `README.md` for the complete workflow.
