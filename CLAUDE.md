# Claude Code project instructions

@AGENTS.md

`AGENTS.md` is the canonical project guide and must be followed for every task
in this repository. In particular:

- Preserve the synchronous thread/channel architecture and main-thread
  ownership of `App`.
- Use the direct Docker Engine API for reads; do not replace it with Docker CLI
  calls or a large SDK for convenience.
- Route worker results through `AppEvent` and keep long-lived streams
  cancellable.
- Keep destructive confirmations, operation-history recording, secret masking,
  and terminal restoration intact.
- Inspect the dirty worktree before editing and avoid changing unrelated user
  work.
- Add focused tests and run the validation commands from `AGENTS.md`; report
  unrelated baseline failures rather than silently rewriting surrounding code.

When changing user-visible behavior, update the in-app help, README, tests, and
VHS demo together where applicable.
