# Roadmap

Feature plan for super-docker. Decisions below were agreed on 2026-07-11.

## Decision log

1. **Scope** — Next feature batch targets all four directions: Docker Compose support, image/registry workflows, container lifecycle+, and power-user UX.
2. **Compose architecture (hybrid)** — Grouping, viewing, and logs for compose projects work via `com.docker.compose.*` container labels through bollard (pure API, no CLI). Shell out to the `docker compose` binary only for operations that have no daemon API: `up`, `down`, `build`. Requires the compose plugin for those actions; everything else works without it.
3. **Remote hosts (single active, switchable)** — One Docker connection at a time. Read docker contexts and `DOCKER_HOST`, provide an in-app host switcher that tears down and reconnects the Docker worker. No simultaneous multi-host support.
4. **Phase 1 = Compose panel** — Shipped first because it forces the shell-out plumbing and the new-panel pattern that later features reuse.
5. **Run/create wizard (minimal form)** — Single modal form: image, name, ports, env, volumes, restart policy. No multi-step wizard.
6. **Diagnostics first (2026-07-12)** — Next batch prioritizes "why is my container dead": exit code / OOM surfacing and an events panel, because both reuse the existing events stream and inspect data with near-zero new plumbing.

## Phases

### Phase 1 — Compose ✅
- [x] Compose projects panel: group containers by `com.docker.compose.project` / `.service` labels (bollard)
- [x] Per-service and per-project aggregated logs
- [x] Shell-out runner for `docker compose up / down / build` (with confirms, streamed output)
- [x] Detect missing compose plugin and degrade gracefully to label-only view

### Phase 2 — Image/registry workflows
- [ ] Build from Dockerfile (bollard build, streamed progress)
- [ ] Pull / push / tag from the Images panel
- [ ] Layer inspection
- [ ] Dangling image cleanup

### Phase 3 — Container lifecycle+
- [ ] Run/create wizard: minimal one-modal form (image, name, ports, env, volumes, restart policy)
- [x] Bulk multi-select actions (space/A mark, batch remove across containers/images/volumes/networks)
- [ ] Copy files in/out, rename

### Phase 4 — Power-user UX
- [ ] Config file (keybinds, theme)
- [ ] Log search / export
- [x] Sort options: per-panel sort column (`,` cycles, `.` reverses, indicator
  in panel title); containers also sort by live cpu/mem
- [ ] Split panes
- [ ] Remote host switcher (docker contexts + `DOCKER_HOST`, single active connection)

### Phase 5 — Diagnostics ✅ (core shipped 2026-07-12)
- [x] Exit code + OOM surfacing: container list shows a red `(137)` badge parsed
  from the status text, an OOM badge driven by daemon `oom` events (cleared on
  `start`), and `OOM killed` in the Info tab
- [x] Restart-loop detection: `↻loop` badge when a container logs ≥3 `die`
  events within 5 minutes (derived from the events buffer, not `RestartCount`)
- [x] Events overlay: `E` opens a scrollable 500-entry history of daemon events
  (exec_* churn filtered out), colored by action — answers "why did it restart"
- [ ] Health dashboard: partially shipped as `✚` health badges in the container
  list plus failing streak + last 3 probe outputs in the Info tab; a dedicated
  all-containers healthcheck view is still open

### Phase 6 — Quality-of-life quick wins (core shipped 2026-07-15)
- [x] Copy to clipboard: `y` yanks name/tag, `Y` yanks id, on every panel —
  OSC 52 with hand-rolled base64 and tmux DCS passthrough, so it works over
  ssh with no clipboard tool on either end
- [x] Port quick-open: `o` opens the lowest published tcp port as
  `localhost:PORT` via `open`/`xdg-open`
- [x] Kill signal picker: `K` opens a t/k/h (TERM / KILL / HUP) modal and hits
  the kill endpoint with an explicit `signal` param
- [ ] Attach to PID 1 (bollard `attach_container`) — distinct from exec, for
  interactive apps
- [ ] Container filesystem diff in the Info tab (`docker diff` endpoint)
- [ ] Commit container → image (bollard `commit`, pairs with diff)
- [ ] Prune preview: show what dies + reclaimed bytes before confirming
  (`system/df` already wired)

### Phase 7 — Bigger bets
- [ ] Log upgrades: timestamps toggle, `since` picker, regex highlight, JSON
  pretty-print for structured logs
- [ ] Resource limits editor: change mem/cpu live via `update_container`, no restart
- [ ] Network topology view: ASCII graph of containers × networks (data already loaded)
- [ ] Compose file viewer: read-only view via `com.docker.compose.project.config_files`
  label, with interpolated env
- [ ] Vulnerability scan: shell out to `trivy` / `docker scout` per image, results in
  Images detail (same shell-out pattern as compose)
- [ ] Watch alerts: desktop notification (OSC 777 / `notify-send`) on die / OOM /
  health-fail while the TUI is backgrounded
- [ ] Stats export: dump sparkline history to CSV/JSON for postmortems

## Shipped outside the phases
- [x] Volume disk usage via `/system/df` (size column + detail row, slow-cadence refresh)
- [x] Remove-all-listed key on every panel with confirm (now `D`)
- [x] Unit test suite across app/docker/compose/ui modules (edge cases: fuzzy filter, log buffer caps, stats math, marks pruning, batch confirms)
- [x] UX overhaul (2026-07-11): accordion left column (active panel expands, others
  collapse), `z` zoom, keyboard-focusable detail pane (`Enter`/`h`/`l`, `j`/`k` scroll),
  mass delete moved `b` → `D` (collided with compose build) and batch confirms now
  require an explicit `y`, filter counts in panel titles (`4/12`), related-containers
  list in image/volume/network detail views, env values masked in Info tab (`x` toggles)
- [x] Context-aware `g`/`G` (2026-07-15): jump to top/bottom of the panel list, or
  log top / re-follow when the detail pane is focused (`f` still follows)

## Deferred (low impact, decide during implementation)
- Config file format and location
- Keybind scheme
- Bulk-select interaction model
- Registry auth source (likely `~/.docker/config.json`)
- Log export format
