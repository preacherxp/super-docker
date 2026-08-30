# super-docker

A fast, keyboard-first terminal UI for Docker containers, Compose projects,
images, volumes, and networks.

![super-docker terminal demo](docs/demo.gif)

`super-docker` talks directly to the Docker Engine API and reacts to its event
stream. It does not poll aggressively or shell out to the Docker CLI for data.

## Quick start

You need a running Docker daemon and Rust 1.85 or newer.

```sh
git clone https://github.com/preacherxp/super-docker.git
cd super-docker
cargo run --bin sd -- --no-update-check
```

Install it when you are ready to use it outside the repository:

```sh
cargo install --path .
sd
```

`cargo install` provides both `sd` and `super-docker`. The Docker Compose plugin
is optional and only needed for Compose actions such as `up`, `down`, and
`build`.

## What it gives you

- Live containers, Compose projects, images, volumes, and networks in one TUI
- Streaming logs, CPU and memory stats, inspect data, and Docker events
- Start, stop, restart, pause, kill, exec, remove, prune, and batch actions
- Compose project grouping and lifecycle actions
- Fuzzy filtering, sorting, mouse support, and OSC 52 clipboard copy
- Confirmations for destructive actions and persistent SQLite operation history

Press `?` in the app for the complete key map. The keys used most often are:

| Key | Action |
| --- | --- |
| `j` / `k`, arrows | Move or scroll |
| `Tab`, `Shift-Tab`, `1`–`5` | Change panel |
| `Enter` / `Esc` | Focus / leave the detail pane |
| `[` / `]` | Change detail tab |
| `/` | Filter rows |
| `space` / `A` | Mark one / all rows |
| `s` / `S` / `r` | Stop / start / restart |
| `e` | Open a shell in the selected container |
| `d` | Remove selected or marked rows |
| `E` / `O` | Docker events / operation history |
| `?` / `q` | Help / quit |

## Development

```sh
cargo test
cargo build --release
```

Run the development build with:

```sh
cargo run --bin sd -- --no-update-check
```

The update check is disabled here so development runs are deterministic. A
normal interactive `sd` launch checks stable GitHub tags and offers to install
a newer release. Set `SUPER_DOCKER_NO_UPDATE_CHECK=1` to disable that behavior
globally.

### Record the terminal demo

The README demo is generated with
[VHS](https://github.com/charmbracelet/vhs). With Docker running:

```sh
brew install vhs # or use another installation method from the VHS project
vhs demo.tape
```

The tape starts a temporary `nginx:alpine` container, records the real TUI to
`docs/demo.gif`, and removes the container when it finishes.

## Architecture

The project deliberately uses plain threads and a single `mpsc` channel rather
than an async runtime:

```text
terminal input ─┐
Docker events ──┤
stats streams ──┼─> main event loop ─> App state ─> ratatui frame
logs/inspect ───┤
slow fallback ──┘
```

- [`src/http.rs`](src/http.rs) is a small blocking HTTP/1.1 client for Unix and
  TCP Docker sockets.
- [`src/json.rs`](src/json.rs) contains the minimal JSON parser used for Engine
  API responses.
- [`src/docker.rs`](src/docker.rs) owns Engine API calls and background streams.
- [`src/app.rs`](src/app.rs) contains application state, input handling, and
  actions.
- [`src/ui.rs`](src/ui.rs) renders the ratatui interface.
- [`src/compose.rs`](src/compose.rs) groups Compose resources and invokes the
  Compose CLI only for mutations unavailable through the Engine API.
- [`src/operations.rs`](src/operations.rs) stores mutation history in SQLite.

Docker events trigger targeted refreshes. Polling is only a fallback: containers
refresh every 2 seconds and other resources every 14 seconds. Stats use one
stream per running container; log and inspect streams follow the current
selection.

See [ROADMAP.md](ROADMAP.md) for planned work.

## Runtime reference

`DOCKER_HOST` takes precedence when set and supports `unix://` and `tcp://`.
Otherwise `sd` checks the common Docker Desktop, Colima, Rancher Desktop,
rootless Docker, and system socket locations.

| Setting | Purpose |
| --- | --- |
| `DOCKER_HOST` | Select the Docker daemon |
| `SUPER_DOCKER_NO_UPDATE_CHECK=1` | Disable update checks |
| `SUPER_DOCKER_DB=/path/to/file.sqlite3` | Override the history database path |
| `XDG_STATE_HOME` | Change the base state directory |

Use `sd --history` to print the latest operation records without starting the
TUI. By default they are stored in
`$XDG_STATE_HOME/super-docker/operations.sqlite3`, or
`~/.local/state/super-docker/operations.sqlite3` when `XDG_STATE_HOME` is not
set. Passive reads are not recorded.
