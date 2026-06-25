# Changesets

`@kitlangton/terminal-control` and its native binary packages are published as one fixed-version group.

For user-facing npm changes, create a changeset with `bun run changeset`, commit the generated metadata, run `bun run version-packages`, refresh `bun.lock`, commit the versioned package metadata, then dispatch the npm release workflow to validate and publish the fixed package set together.
