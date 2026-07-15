mod app;
mod compose;
mod docker;
mod http;
mod json;
mod ui;

use std::io::stdout;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crossterm::event::{DisableMouseCapture, EnableMouseCapture, Event, KeyEventKind};
use crossterm::execute;

use app::{App, AppEvent};

/// Daemon streams can deliver one update per running container at nearly the
/// same instant. Batch those updates into a single terminal frame while still
/// drawing keyboard and mouse input immediately.
const BACKGROUND_FRAME_INTERVAL: Duration = Duration::from_millis(100);
const CLOCK_TICK: Duration = Duration::from_secs(1);

fn main() -> Result<(), Box<dyn std::error::Error>> {
    if std::env::args().any(|a| a == "--version" || a == "-V") {
        println!("super-docker {}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }

    let docker = match docker::connect() {
        Ok(d) => d,
        Err(e) => {
            eprintln!("cannot connect to docker daemon: {e}");
            std::process::exit(1);
        }
    };

    let (tx, rx) = mpsc::channel::<AppEvent>();
    docker::spawn_worker(docker.clone(), tx.clone());
    compose::spawn_probe(tx.clone());

    // Terminal input pumped into the same channel as daemon events, so the
    // main loop blocks in one place. Paused while `docker exec` owns the
    // tty — polling would steal the shell's keystrokes.
    let input_paused = Arc::new(AtomicBool::new(false));
    {
        let tx = tx.clone();
        let paused = input_paused.clone();
        std::thread::spawn(move || loop {
            if paused.load(Ordering::SeqCst) {
                std::thread::sleep(Duration::from_millis(50));
                continue;
            }
            match crossterm::event::poll(Duration::from_millis(100)) {
                Ok(true) => {
                    if paused.load(Ordering::SeqCst) {
                        continue;
                    }
                    let Ok(ev) = crossterm::event::read() else { return };
                    if tx.send(AppEvent::Input(ev)).is_err() {
                        return;
                    }
                }
                Ok(false) => {}
                Err(_) => return,
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
        let now = Instant::now();
        let wake_at = if dirty {
            (last_draw + BACKGROUND_FRAME_INTERVAL).min(next_tick)
        } else {
            next_tick
        };
        let mut interactive = false;

        match rx.recv_timeout(wake_at.saturating_duration_since(now)) {
            Ok(ev) => {
                interactive |= handle(&mut app, ev);
                dirty = true;
                // Drain a burst of daemon samples before redrawing. This is
                // especially important when many containers are running.
                while let Ok(ev) = rx.try_recv() {
                    interactive |= handle(&mut app, ev);
                }
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }

        if let Some(id) = app.pending_exec.take() {
            input_paused.store(true, Ordering::SeqCst);
            exec_shell(&mut terminal, &id);
            input_paused.store(false, Ordering::SeqCst);
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
fn handle(app: &mut App, ev: AppEvent) -> bool {
    match ev {
        AppEvent::Input(Event::Key(key)) if key.kind != KeyEventKind::Release => {
            app.on_key(key);
            true
        }
        AppEvent::Input(Event::Mouse(m)) => {
            app.on_mouse(m);
            true
        }
        AppEvent::Input(_) => true, // resize is handled by the redraw
        ev => {
            app.apply(ev);
            false
        }
    }
}

/// Suspend the TUI and drop the user into a shell inside the container.
fn exec_shell(terminal: &mut ratatui::DefaultTerminal, id: &str) {
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

    if let Err(e) = status {
        eprintln!("docker exec failed: {e}");
    }
}
