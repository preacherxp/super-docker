# super-docker

A fast, keyboard-first terminal UI for Docker containers, Compose projects,
images, volumes, and networks.

![super-docker terminal demo](docs/demo.gif)

## Install

Requirements:

- A running Docker daemon
- The Docker Compose plugin for Compose actions such as `up` and `build`

Install a prebuilt binary on macOS or Linux (Intel/AMD64 or ARM64). No Rust
toolchain is needed; the installer uses `curl`, `tar`, and a SHA-256 utility:

```sh
curl -fsSL https://github.com/preacherxp/super-docker/releases/latest/download/install.sh | sh
~/.local/bin/sd
```

The installer verifies the release checksum and puts `sd` and `super-docker`
in `~/.local/bin`, without sudo. If that directory is not on your PATH, add
`export PATH="$HOME/.local/bin:$PATH"` to your shell configuration. Set
`SUPER_DOCKER_INSTALL_DIR` on the `sh` command to choose another directory.
Rerun the installer to update, or accept the in-app update prompt.

For a manual install, download your platform's archive and matching `.sha256`
file from [Releases](https://github.com/preacherxp/super-docker/releases),
verify it with `sha256sum -c` (Linux) or `shasum -a 256 -c` (macOS), and extract
both executables into a directory on your PATH.

Building from source is still available with a current stable Rust toolchain:

```sh
git clone https://github.com/preacherxp/super-docker.git
cd super-docker
cargo install --path .
sd
```

Releases before the binary installer was introduced are source-only. To publish
binaries, bump the Cargo package version and push its matching `vX.Y.Z` tag.
The release workflow builds all four platforms and publishes the installer,
archives, and checksums together after every build succeeds.

Both `sd` and `super-docker` launch the same application. Press `?` for the
complete key map and `q` to quit.

Useful options:

```sh
sd --no-update-check  # disable the release check for this run
sd --history          # print recent Docker mutation history
sd --version          # print the installed version
```

Set `DOCKER_HOST` to use a non-default Unix or TCP daemon. Update checks can be
disabled globally with `SUPER_DOCKER_NO_UPDATE_CHECK=1`, and operation-history
storage can be overridden with `SUPER_DOCKER_DB=/path/to/history.sqlite3`.

## Implementation

`super-docker` talks directly to the Docker Engine HTTP API over Unix or TCP
sockets. It uses plain Rust threads, bounded channels, cancellable streams, and
event-driven targeted refreshes; the Docker CLI is reserved for Compose
mutations and interactive `docker exec`.

## Website

The landing page lives in [`website/`](website/README.md), using Astro, Vue,
and Tailwind CSS. With Node.js 22.19+ installed:

```sh
cd website
npm ci
npm run dev
```
