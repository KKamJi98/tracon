# Contributing

Thanks for looking at tracon. It's a small, single-maintainer project, so the
bar for contributions is mostly "does it keep working the way it does now."

## Setup

```sh
cargo build
cargo test
```

Supported platforms are macOS and Linux; there is no Windows support and no
plan to add one. No async runtime is used, and the dependency list is kept
short on purpose - please raise a new dependency in an issue before sending a
PR that adds one.

## Before opening a PR

Run the same checks CI runs:

```sh
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test --all
sh scripts/scrub-check.sh
```

`scripts/scrub-check.sh` is not about code style - it greps the tracked tree
for strings that should never leave a contributor's machine (real paths,
internal ticket keys, and the like). If you're adding a test fixture or a
screenshot, use invented values (fake paths, fake project names, fake UUIDs),
never output copied from your own machine.

## Making changes

- Keep changes scoped to what the issue or PR describes. This is not the
  place for drive-by refactors.
- If you change behavior, add or update a test for it. `cargo test` should
  tell the story of what changed.
- Match the existing code style and comment density rather than introducing a
  new one.

## Reporting a bug

Open an issue with what you ran, what you expected, and what happened
instead. `tracon --json` output is more useful than a screenshot description
if the bug is about session state.
