# Performance and test strategy

The UI thread owns only state transitions and rendering. Docker I/O, SQLite
initialization, release checks, Compose commands, and mutations run on standard
worker threads. Input has a dedicated channel; background work uses a bounded
2,048-event queue, drains at most 256 events or 2 ms per turn, and renders
background bursts at most every 100 ms.

## Runtime safeguards

- Docker event bursts are coalesced for 75 ms into one targeted refresh per
  resource kind.
- Container polling is a 2-second recovery path while the event stream is down
  and a 30-second reconciliation path while healthy. Other resources refresh
  every 60 seconds; volume disk usage every 5 minutes.
- Selection changes debounce log and inspect startup for 125 ms and cancel the
  previous sockets immediately.
- Logs are capped at 5,000 entries, 8 MiB total, and 256 KiB per entry. Severity
  is classified once at ingestion and visual rows append incrementally until
  wrapping or viewport width changes.
- Filtered/sorted table indices are cached by data, stats, filter, and sort
  revision. Restart-loop lookup is indexed per container.
- SQLite migration/recovery and the rate-limited release check do not delay the
  first terminal frame.

## Local checks

```sh
cargo test
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
cargo build --release
cargo bench --bench hot_paths
cargo llvm-cov --lib --summary-only
```

True Rust branch instrumentation currently requires nightly:

```sh
cargo +nightly llvm-cov --lib --branch --summary-only
```

CI rejects drops below the checked-in baseline (73% line coverage and 59%
instrumented branch coverage). These are regression floors, not claims of
complete coverage. The suite deliberately enumerates the meaningful branches
of parsers, HTTP framing, state transitions, destructive confirmations,
refresh coalescing, bounded logs, persistence recovery, and rendering modes;
unavailable-daemon, process-launch, and operating-system failures are handled
defensively and supplemented by weekly fuzzing and mutation testing.

Run fuzzers manually with nightly Rust and `cargo-fuzz`:

```sh
cargo fuzz --fuzz-dir fuzz run json
cargo fuzz --fuzz-dir fuzz run log_ingest
```

Benchmarks report three representative hot paths: parsing a 1,000-object
Engine response, repeated cached filtering of 10,000 images, and rendering a
5,000-line log buffer. Compare output before and after a performance change on
the same machine; absolute timings vary across terminals and CI hosts.
