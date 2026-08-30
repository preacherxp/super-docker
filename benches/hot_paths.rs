use std::hint::black_box;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use ratatui::Terminal;
use ratatui::backend::TestBackend;
use super_docker::app::{App, AppEvent, ImageRow};
use super_docker::{docker::Docker, json, ui};

fn measure(name: &str, iterations: usize, mut operation: impl FnMut()) {
    for _ in 0..10 {
        operation();
    }
    let started = Instant::now();
    for _ in 0..iterations {
        operation();
    }
    let elapsed = started.elapsed();
    println!(
        "{name}: {:.2} µs/iteration ({iterations} iterations)",
        elapsed.as_secs_f64() * 1_000_000.0 / iterations as f64
    );
}

fn main() {
    let payload = format!(
        "[{}]",
        (0..1_000)
            .map(|i| format!(r#"{{"id":{i},"name":"container-{i}","running":true}}"#))
            .collect::<Vec<_>>()
            .join(",")
    );
    measure("json/1000-objects", 100, || {
        black_box(json::parse(black_box(&payload)).unwrap());
    });

    let (tx, rx) = mpsc::sync_channel(8);
    let mut app = App::new(Docker::dummy(), tx);
    std::mem::forget(rx);
    app.apply(AppEvent::Images(
        (0..10_000)
            .map(|i| ImageRow {
                id: format!("sha256:{i}"),
                tag: format!("registry/service-{i}:latest"),
                size: i * 1024,
                created: i,
                containers: i as usize % 4,
            })
            .collect(),
    ));
    app.filter = "service-99".into();
    // First lookup builds the view; the benchmark measures the cached path.
    black_box(app.filtered_images());
    measure("cached-filter/10000-images", 10_000, || {
        black_box(app.filtered_images());
    });

    app.logs_id = Some("benchmark".into());
    app.apply(AppEvent::Log(
        "benchmark".into(),
        (0..5_000)
            .map(|i| format!("INFO request={i} latency_ms={}", i % 100))
            .collect::<Vec<_>>()
            .join("\n"),
    ));
    let mut terminal = Terminal::new(TestBackend::new(120, 40)).unwrap();
    measure("render/5000-log-lines", 100, || {
        terminal.draw(|frame| ui::draw(frame, &mut app)).unwrap();
    });

    // Make accidental multi-second regressions fail loudly while leaving wide
    // headroom for shared and emulated CI machines.
    let started = Instant::now();
    for _ in 0..100 {
        black_box(app.filtered_images());
    }
    assert!(started.elapsed() < Duration::from_secs(5));
}
