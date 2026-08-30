//! Docker Compose integration.
//!
//! Grouping and logs come from `com.docker.compose.*` container labels via
//! the daemon API. Compose has no daemon API for `up`/`down`/`build`,
//! so those shell out to the `docker compose` plugin.

use std::io::{BufRead, BufReader, Read};
use std::process::{Command, Stdio};
use std::thread::{self, JoinHandle};

use crate::app::{AppEvent, AppSender, ComposeRow};
use crate::operations;

/// Log-buffer key for a compose project (kept distinct from container ids).
pub fn log_key(project: &str) -> String {
    format!("compose:{project}")
}

/// Detect the compose plugin once at startup.
pub fn spawn_probe(tx: AppSender) {
    thread::spawn(move || {
        let ok = Command::new("docker")
            .args(["compose", "version"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        let _ = tx.send(AppEvent::ComposeAvailable(ok));
    });
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ComposeAction {
    Up,
    Down,
    Stop,
    Restart,
    Build,
}

impl ComposeAction {
    pub fn verb(self) -> &'static str {
        match self {
            ComposeAction::Up => "up",
            ComposeAction::Down => "down",
            ComposeAction::Stop => "stop",
            ComposeAction::Restart => "restart",
            ComposeAction::Build => "build",
        }
    }

    /// `up`/`build` must resolve the compose file; the rest work off the
    /// project name alone.
    fn needs_files(self) -> bool {
        matches!(self, ComposeAction::Up | ComposeAction::Build)
    }
}

/// Run `docker compose <action>` for a project, streaming output into the
/// project's log buffer and toasting the final status.
pub fn compose_action(tx: &AppSender, action: ComposeAction, p: ComposeRow) {
    let tx = tx.clone();
    thread::spawn(move || {
        let operation = operations::begin(action.verb(), "compose", &p.name, "");
        let verb = action.verb();
        let mut cmd = Command::new("docker");
        cmd.arg("compose").arg("-p").arg(&p.name);
        if action.needs_files() {
            if p.config_files.is_empty() {
                let result: Result<(), &str> = Err("compose file unknown");
                operation.finish(&result);
                let _ = tx.send(AppEvent::Toast(
                    format!("{}: compose file unknown, cannot {verb}", p.name),
                    true,
                ));
                return;
            }
            // label value is a comma-separated path list
            for f in p.config_files.split(',').filter(|f| !f.is_empty()) {
                cmd.arg("-f").arg(f);
            }
            if !p.working_dir.is_empty() {
                cmd.arg("--project-directory").arg(&p.working_dir);
            }
        }
        match action {
            ComposeAction::Up => {
                cmd.args(["up", "-d"]);
            }
            _ => {
                cmd.arg(verb);
            }
        }
        cmd.stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        let key = log_key(&p.name);
        let _ = tx.send(AppEvent::Log(
            key.clone(),
            format!("compose ▸ {verb} {}…", p.name),
        ));
        let mut child = match cmd.spawn() {
            Ok(c) => c,
            Err(e) => {
                let result: Result<(), String> = Err(e.to_string());
                operation.finish(&result);
                let _ = tx.send(AppEvent::Toast(format!("docker compose: {e}"), true));
                return;
            }
        };

        let mut pumps = Vec::new();
        if let Some(out) = child.stdout.take() {
            pumps.push(stream_lines(out, tx.clone(), key.clone()));
        }
        if let Some(err) = child.stderr.take() {
            pumps.push(stream_lines(err, tx.clone(), key.clone()));
        }
        let status = child.wait();
        for pump in pumps {
            let _ = pump.join();
        }

        let result: Result<(), String> = match status {
            Ok(s) if s.success() => Ok(()),
            Ok(s) => Err(s.to_string()),
            Err(e) => Err(e.to_string()),
        };
        operation.finish(&result);
        let _ = match result {
            Ok(()) => tx.send(AppEvent::Toast(
                format!("compose {verb} {} done", p.name),
                false,
            )),
            Err(error) => tx.send(AppEvent::Toast(
                format!("compose {verb} {} failed ({error})", p.name),
                true,
            )),
        };
    });
}

fn stream_lines<R>(reader: R, tx: AppSender, key: String) -> JoinHandle<()>
where
    R: Read + Send + 'static,
{
    thread::spawn(move || {
        for line in BufReader::new(reader).lines() {
            let Ok(line) = line else { return };
            if line.trim().is_empty() {
                continue;
            }
            if tx
                .send(AppEvent::Log(key.clone(), format!("compose ▸ {line}")))
                .is_err()
            {
                return;
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn log_key_is_namespaced() {
        // must never collide with a container id (hex)
        assert_eq!(log_key("myproj"), "compose:myproj");
    }

    #[test]
    fn action_verbs() {
        assert_eq!(ComposeAction::Up.verb(), "up");
        assert_eq!(ComposeAction::Down.verb(), "down");
        assert_eq!(ComposeAction::Stop.verb(), "stop");
        assert_eq!(ComposeAction::Restart.verb(), "restart");
        assert_eq!(ComposeAction::Build.verb(), "build");
    }

    #[test]
    fn only_up_and_build_need_files() {
        assert!(ComposeAction::Up.needs_files());
        assert!(ComposeAction::Build.needs_files());
        assert!(!ComposeAction::Down.needs_files());
        assert!(!ComposeAction::Stop.needs_files());
        assert!(!ComposeAction::Restart.needs_files());
    }

    #[test]
    fn up_without_config_files_toasts_error() {
        let (tx, rx) = std::sync::mpsc::sync_channel(32);
        let p = ComposeRow {
            name: "p".into(),
            config_files: String::new(),
            working_dir: String::new(),
            running: 0,
            total: 0,
        };
        compose_action(&tx, ComposeAction::Up, p);
        let ev = rx.recv().unwrap();
        match ev {
            AppEvent::Toast(text, error) => {
                assert!(error);
                assert!(text.contains("compose file unknown"));
            }
            other => panic!("expected toast, got {other:?}"),
        }
    }

    #[test]
    fn stream_lines_prefixes_and_skips_blank() {
        let (tx, rx) = std::sync::mpsc::sync_channel(32);
        let input: &[u8] = b"first\n\n  \nsecond\n";
        stream_lines(input, tx, "k".into()).join().unwrap();
        let mut got = Vec::new();
        while let Ok(ev) = rx.try_recv() {
            if let AppEvent::Log(key, line) = ev {
                assert_eq!(key, "k");
                got.push(line);
            }
        }
        assert_eq!(got, vec!["compose ▸ first", "compose ▸ second"]);
    }
}
