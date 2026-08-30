# Repository guide for coding agents

## Project

`super-docker` is a keyboard-first Docker TUI written in Rust 2024 with
ratatui and crossterm. It talks directly to the Docker Engine HTTP API over a
Unix or TCP socket. Keep the application small, synchronous, and event-driven.

Rust 1.85 or newer is required. The two binaries, `sd` and `super-docker`, are
aliases backed by the same thin `src/main.rs` entry point.

## Start here

- Read `README.md` for supported behavior and user-facing commands.
- Read `ROADMAP.md` before adding a feature or changing established scope.
- Inspect `git status --short` before editing. The worktree may contain user
  changes; preserve them and do not reformat or rewrite unrelated files.
- Prefer focused changes that follow existing patterns over new abstractions or
  dependencies.

## Module boundaries

- `src/main.rs`: terminal lifecycle, input/background queues, fairness, redraw
  scheduling, and interactive exec suspension. Keep it a thin binary wrapper.
- `src/lib.rs`: canonical library surface used by tests and both binaries.
- `src/app.rs`: application state, selection, filtering/sorting, key and mouse
  handling, confirmations, and conversion of `AppEvent`s into state.
- `src/ui.rs`: rendering only. It may update ratatui widget state needed for
  rendering, but Docker operations and business rules belong elsewhere.
- `src/docker.rs`: Docker Engine API models, reads, mutation workers, events,
  stats, logs, inspect streams, and targeted refreshes.
- `src/http.rs`: blocking HTTP/1.1 transport over Unix/TCP sockets. Preserve
  Content-Length, chunked, EOF-framed, and cancellable streaming behavior.
- `src/json.rs`: deliberately small JSON parser for Engine API responses.
- `src/compose.rs`: Compose label grouping and CLI-backed mutations that the
  Engine API cannot perform.
- `src/operations.rs`: persistent SQLite audit history for user mutations.
- `src/update.rs`: rate-limited release checks and the opt-in update prompt.

## Architecture invariants

- Do not introduce an async runtime without explicit project-level agreement.
  Background work uses standard threads and channels.
- Only the main loop owns and mutates `App`. Workers communicate through
  `AppEvent`; they must not reach into UI state directly.
- Keep terminal input separate from the bounded background channel. Continuous
  stats or event traffic must never starve input or redraws.
- Preserve the bounded/time-limited background drain and 100 ms background
  frame batching in `main.rs`. Interactive input should redraw immediately.
- Long-lived Docker streams must be cancellable by shutting down a cloned
  socket through `TaskHandle`. A selection change must not leave old log,
  inspect, or stats streams running.
- Docker events should cause targeted refreshes. Polling is a recovery and
  reconciliation mechanism, not the primary update path.
- Do not shell out to `docker` for read-only data. Direct Engine API calls are
  the default. The CLI is reserved for Compose mutations without an Engine API
  equivalent and interactive `docker exec`.
- Keep dependencies intentionally small. Adding Docker SDKs, async runtimes,
  serialization frameworks, or other large dependencies requires a concrete
  benefit that cannot be achieved cleanly with the existing code.

## Behavior and safety

- Destructive actions must go through the existing confirmation flow. Batch or
  remove-all actions require an explicit `y`; `Enter` must not confirm them.
- Every user-triggered mutation must create an `operations::begin` record and
  finish it with the actual success or failure. Passive reads are not logged.
- Environment values remain masked by default. Do not expose secrets in UI
  text, logs, toasts, test fixtures, or operation history.
- Docker errors should degrade into an `AppEvent`, toast, or visible connection
  state where possible; a worker failure must not crash the TUI.
- When adding or changing a key, update input handling, the `?` help overlay,
  footer hints where relevant, README key documentation, and tests together.
- Preserve terminal restoration on every exit path and around interactive exec.

## Docker protocol work

- Engine requests are bodyless in the current client. If a feature needs a
  request body, extend and test `http.rs` deliberately rather than bypassing it.
- Docker log responses may be raw TTY bytes or 8-byte multiplexed frames.
  Retain both paths and cap untrusted frame sizes.
- Event and stats endpoints are NDJSON streams. Parsing failures should skip a
  bad sample rather than terminate a healthy stream.
- Treat all daemon fields as optional or version-dependent. Default safely and
  add focused parser tests for new fields.
- Compose grouping comes from `com.docker.compose.*` labels and must remain
  useful even when the Compose plugin is absent.

## Tests and validation

Most tests use `Docker::dummy()` or local fixtures and do not require a daemon.
Add unit tests beside the module being changed, especially for parsing, state
transitions, confirmation semantics, stream framing, and layout edge cases.

Run the checks relevant to the change:

```sh
cargo test
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
cargo build --release
git diff --check
```

If an unrelated baseline check already fails, do not sweep-format or repair
user work outside the task. Report the existing failure and still run narrower
checks where possible.

For a live smoke test, use a disposable Docker workload and disable networked
update checks:

```sh
SUPER_DOCKER_NO_UPDATE_CHECK=1 cargo run --bin sd
```

Use a temporary `SUPER_DOCKER_DB` path for scripted tests that exercise
operation history. Never point tests at a developer's real history database.

## Documentation and generated assets

- Keep README instructions concise and accurate for the current CLI.
- `demo.tape` is the source for `docs/demo.gif`. If the visible workflow or key
  sequence changes, update the tape and regenerate the GIF with `vhs demo.tape`.
- The demo must use disposable, uniquely named Docker resources and clean them
  up when the recording finishes.
- Do not commit `target/`, temporary databases, mock sockets, or recording
  scratch files.
