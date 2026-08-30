use std::io::stdout;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::time::{Duration, Instant};

use crossterm::event::{DisableMouseCapture, EnableMouseCapture, Event, KeyEventKind};
use crossterm::execute;

use super_docker::app::{App, AppEvent};
use super_docker::{compose, docker, operations, ui, update};

/// Daemon streams can deliver one update per running container at nearly the
/// same instant. Batch those updates into a single terminal frame while still
/// drawing keyboard and mouse input immediately.
const BACKGROUND_FRAME_INTERVAL: Duration = Duration::from_millis(100);
const CLOCK_TICK: Duration = Duration::from_secs(1);
const INPUT_WAKE_INTERVAL: Duration = Duration::from_millis(16);
const BACKGROUND_QUEUE_CAPACITY: usize = 2_048;
const MAX_BACKGROUND_EVENTS_PER_TURN: usize = 256;
const MAX_BACKGROUND_DRAIN_TIME: Duration = Duration::from_millis(2);

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().collect();
    if args.iter().any(|a| a == "--version" || a == "-V") {
        println!("super-docker {}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }
    if args.iter().any(|a| a == "--history" || a == "--operations") {
        let _operation_db = operations::init()?;
        operations::print_history();
        return Ok(());
    }
    // Database creation/migration and stale-operation recovery are not on the
    // first-frame critical path. Action workers lazily initialize it too.
    std::thread::spawn(|| {
        let _ = operations::init();
    });

    let docker = match docker::connect() {
        Ok(d) => d,
        Err(e) => {
            eprintln!("cannot connect to docker daemon: {e}");
            std::process::exit(1);
        }
    };

    let (tx, rx) = mpsc::sync_channel::<AppEvent>(BACKGROUND_QUEUE_CAPACITY);
    let (input_tx, input_rx) = mpsc::channel::<Event>();
    docker::spawn_worker(docker.clone(), tx.clone());
    compose::spawn_probe(tx.clone());
    if !args.iter().any(|a| a == "--no-update-check") {
        update::spawn_check(tx.clone());
    }

    // Terminal input pumped into the same channel as daemon events, so the
    // main loop blocks in one place. Paused while `docker exec` owns the
    // tty — polling would steal the shell's keystrokes.
    let input_paused = Arc::new(AtomicBool::new(false));
    {
        let paused = input_paused.clone();
        std::thread::spawn(move || {
            loop {
                if paused.load(Ordering::SeqCst) {
                    std::thread::sleep(Duration::from_millis(50));
                    continue;
                }
                match crossterm::event::poll(Duration::from_millis(100)) {
                    Ok(true) => {
                        if paused.load(Ordering::SeqCst) {
                            continue;
                        }
                        let Ok(ev) = crossterm::event::read() else {
                            return;
                        };
                        if input_tx.send(ev).is_err() {
                            return;
                        }
                    }
                    Ok(false) => {}
                    Err(_) => return,
                }
            }
        });
    }

    let mut terminal = ratatui::init();
    let _ = execute!(stdout(), EnableMouseCapture);

    let mut app = App::new(docker, tx);
    let mut last_draw = Instant::now() - BACKGROUND_FRAME_INTERVAL;
    let mut next_tick = Instant::now();
    let mut dirty = true;

    loop {
        let mut interactive = false;
        while let Ok(ev) = input_rx.try_recv() {
            interactive |= handle_input(&mut app, ev);
        }

        let now = Instant::now();
        let wake_at = if dirty {
            (last_draw + BACKGROUND_FRAME_INTERVAL).min(next_tick)
        } else {
            next_tick
        }
        .min(now + INPUT_WAKE_INTERVAL);

        match rx.recv_timeout(wake_at.saturating_duration_since(now)) {
            Ok(ev) => {
                app.apply(ev);
                dirty = true;
                // A continuous telemetry producer must not postpone terminal
                // input or a frame indefinitely.
                let drain_started = Instant::now();
                for _ in 1..MAX_BACKGROUND_EVENTS_PER_TURN {
                    if drain_started.elapsed() >= MAX_BACKGROUND_DRAIN_TIME {
                        break;
                    }
                    let Ok(ev) = rx.try_recv() else { break };
                    app.apply(ev);
                }
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }

        while let Ok(ev) = input_rx.try_recv() {
            interactive |= handle_input(&mut app, ev);
        }
        dirty |= app.flush_selection_sync();
        dirty |= interactive;

        if let Some(id) = app.pending_exec.take() {
            let name = app
                .containers
                .iter()
                .find(|container| container.id == id)
                .map(|container| container.name.as_str())
                .unwrap_or(&id)
                .to_string();
            let operation = operations::begin("exec shell", "container", &name, &id);
            input_paused.store(true, Ordering::SeqCst);
            let result = exec_shell(&mut terminal, &id);
            input_paused.store(false, Ordering::SeqCst);
            operation.finish(&result);
            if let Err(error) = result {
                app.apply(AppEvent::Toast(format!("docker exec: {error}"), true));
            }
            interactive = true;
            dirty = true;
        }

        if let Some((version, tag)) = app.pending_update.take() {
            input_paused.store(true, Ordering::SeqCst);
            let result = install_update(&mut terminal, &tag);
            input_paused.store(false, Ordering::SeqCst);
            let message = match &result {
                Ok(()) => format!("updated to v{version}; restart sd to use it"),
                Err(error) => format!("update failed: {error}"),
            };
            app.apply(AppEvent::Toast(message, result.is_err()));
            interactive = true;
            dirty = true;
        }

        if app.should_quit {
            break;
        }

        let now = Instant::now();
        let periodic = now >= next_tick;
        if periodic {
            next_tick = now + CLOCK_TICK;
        }
        if periodic
            || (dirty
                && (interactive || now.duration_since(last_draw) >= BACKGROUND_FRAME_INTERVAL))
        {
            terminal.draw(|f| ui::draw(f, &mut app))?;
            last_draw = now;
            dirty = false;
        }
    }

    let _ = execute!(stdout(), DisableMouseCapture);
    ratatui::restore();
    Ok(())
}

/// Returns true for user/terminal events, which should redraw without the
/// background-update batching delay.
fn handle_input(app: &mut App, ev: Event) -> bool {
    match ev {
        Event::Key(key) if key.kind != KeyEventKind::Release => {
            app.on_key(key);
            true
        }
        Event::Mouse(m) => {
            app.on_mouse(m);
            true
        }
        _ => true, // resize is handled by the redraw
    }
}

/// Suspend the TUI and drop the user into a shell inside the container.
fn exec_shell(terminal: &mut ratatui::DefaultTerminal, id: &str) -> Result<(), String> {
    let _ = execute!(stdout(), DisableMouseCapture);
    ratatui::restore();

    let status = std::process::Command::new("docker")
        .args([
            "exec",
            "-it",
            id,
            "sh",
            "-c",
            "command -v bash >/dev/null 2>&1 && exec bash || exec sh",
        ])
        .status();

    *terminal = ratatui::init();
    let _ = execute!(stdout(), EnableMouseCapture);
    let _ = terminal.clear();

    match status {
        Ok(status) if status.success() => Ok(()),
        Ok(status) => Err(status.to_string()),
        Err(error) => Err(error.to_string()),
    }
}

fn install_update(terminal: &mut ratatui::DefaultTerminal, tag: &str) -> Result<(), String> {
    let _ = execute!(stdout(), DisableMouseCapture);
    ratatui::restore();
    println!("Installing super-docker {tag}…");
    let result = update::install_release(tag);
    *terminal = ratatui::init();
    let _ = execute!(stdout(), EnableMouseCapture);
    let _ = terminal.clear();
    result
}
