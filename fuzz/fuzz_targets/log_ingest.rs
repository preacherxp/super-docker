#![no_main]

use std::sync::mpsc;

use libfuzzer_sys::fuzz_target;
use super_docker::app::{App, AppEvent, MAX_LOG_BYTES, MAX_LOG_LINE_BYTES, MAX_LOG_LINES};
use super_docker::docker::Docker;

fuzz_target!(|data: &[u8]| {
    let (tx, rx) = mpsc::sync_channel(1);
    let mut app = App::new(Docker::dummy(), tx);
    app.logs_id = Some("fuzz".into());
    app.apply(AppEvent::Log(
        "fuzz".into(),
        String::from_utf8_lossy(data).into_owned(),
    ));
    assert!(app.logs.len() <= MAX_LOG_LINES);
    assert!(app.log_bytes <= MAX_LOG_BYTES);
    assert!(app.logs.iter().all(|line| line.text.len() <= MAX_LOG_LINE_BYTES));
    drop(rx);
});
