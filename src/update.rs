use std::fmt;
use std::io::{self, IsTerminal};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant, SystemTime};

use crate::app::{AppEvent, AppSender};

const REPOSITORY: &str = "https://github.com/preacherxp/super-docker";
const CHECK_TIMEOUT: Duration = Duration::from_secs(3);
const CHECK_INTERVAL: Duration = Duration::from_secs(24 * 60 * 60);

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct Version {
    major: u64,
    minor: u64,
    patch: u64,
}

impl fmt::Display for Version {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Release {
    tag: String,
    version: Version,
}

/// Check for releases on a worker thread.  The first frame never waits for
/// network, DNS, Git, or cache I/O.
pub fn spawn_check(tx: AppSender) {
    if std::env::var_os("SUPER_DOCKER_NO_UPDATE_CHECK").is_some()
        || !io::stdin().is_terminal()
        || !io::stdout().is_terminal()
    {
        return;
    }
    std::thread::spawn(move || {
        let cache = cache_path();
        if cache_is_fresh(&cache, SystemTime::now()) {
            return;
        }
        mark_checked(&cache);
        if let Some(release) = find_newer_release(env!("CARGO_PKG_VERSION")) {
            let _ = tx.send(AppEvent::UpdateAvailable {
                version: release.version.to_string(),
                tag: release.tag,
            });
        }
    });
}

/// Install one exact release selected in the TUI.
pub fn install_release(tag: &str) -> Result<(), String> {
    parse_version(tag).ok_or("invalid release tag")?;
    let executable = std::env::current_exe().map_err(|error| error.to_string())?;
    let directory = executable
        .parent()
        .ok_or("cannot locate install directory")?;
    let operation = crate::operations::begin("update", "application", "super-docker", tag);
    let result = match Command::new("sh")
        .args([
            "-c",
            include_str!("../install.sh"),
            "super-docker-installer",
            tag,
        ])
        .env("SUPER_DOCKER_INSTALL_DIR", directory)
        .status()
    {
        Ok(status) if status.success() => Ok(()),
        Ok(status) => Err(format!("binary installer exited with {status}")),
        Err(error) => Err(format!("could not run installer: {error}")),
    };
    operation.finish(&result);
    result
}

fn cache_path() -> PathBuf {
    if let Some(base) = std::env::var_os("XDG_CACHE_HOME").filter(|p| !p.is_empty()) {
        return PathBuf::from(base).join("super-docker/update-check");
    }
    if let Some(home) = std::env::var_os("HOME").filter(|p| !p.is_empty()) {
        return PathBuf::from(home).join(".cache/super-docker/update-check");
    }
    PathBuf::from(".super-docker-update-check")
}

fn cache_is_fresh(path: &Path, now: SystemTime) -> bool {
    let Ok(modified) = path.metadata().and_then(|metadata| metadata.modified()) else {
        return false;
    };
    now.duration_since(modified).unwrap_or_default() < CHECK_INTERVAL
}

fn mark_checked(path: &Path) {
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(path, []);
}

fn find_newer_release(current: &str) -> Option<Release> {
    let current = parse_version(current)?;
    let mut command = Command::new("curl");
    command.args([
        "--proto",
        "=https",
        "--proto-redir",
        "=https",
        "-fsSLI",
        "--max-time",
        "3",
        "--output",
        "/dev/null",
        "--write-out",
        "%{url_effective}",
        &format!("{REPOSITORY}/releases/latest"),
    ]);
    let output = output_with_timeout(&mut command, CHECK_TIMEOUT).ok()?;
    if !output.status.success() {
        return None;
    }
    let stdout = String::from_utf8(output.stdout).ok()?;
    latest_release(&stdout, current)
}

fn output_with_timeout(command: &mut Command, timeout: Duration) -> io::Result<Output> {
    let mut child = command
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()?;
    let deadline = Instant::now() + timeout;
    loop {
        if child.try_wait()?.is_some() {
            return child.wait_with_output();
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "update check timed out",
            ));
        }
        std::thread::sleep(Duration::from_millis(25));
    }
}

fn latest_release(url: &str, current: Version) -> Option<Release> {
    let tag = url
        .trim()
        .strip_prefix(&format!("{REPOSITORY}/releases/tag/"))?;
    let version = parse_version(tag)?;
    (version > current).then(|| Release {
        tag: tag.into(),
        version,
    })
}

fn parse_version(value: &str) -> Option<Version> {
    let value = value.strip_prefix('v').unwrap_or(value);
    if !value
        .bytes()
        .all(|byte| byte.is_ascii_digit() || byte == b'.')
    {
        return None;
    }
    let mut parts = value.split('.');
    let version = Version {
        major: parts.next()?.parse().ok()?,
        minor: parts.next()?.parse().ok()?,
        patch: parts.next()?.parse().ok()?,
    };
    parts.next().is_none().then_some(version)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_cache() -> PathBuf {
        static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        std::env::temp_dir().join(format!(
            "super-docker-update-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ))
    }

    #[test]
    fn parses_stable_semver_tags_only() {
        assert_eq!(
            parse_version("v1.2.3"),
            Some(Version {
                major: 1,
                minor: 2,
                patch: 3
            })
        );
        assert_eq!(parse_version("1.2.3"), parse_version("v1.2.3"));
        assert_eq!(parse_version("v1.2.3-beta.1"), None);
        assert_eq!(parse_version("v1.2"), None);
        assert_eq!(parse_version("latest"), None);
    }

    #[test]
    fn recognizes_a_newer_published_release() {
        let url = format!("{REPOSITORY}/releases/tag/v0.2.0");
        let release = latest_release(&url, parse_version("0.1.1").unwrap()).unwrap();
        assert_eq!(release.tag, "v0.2.0");
        assert_eq!(release.version.to_string(), "0.2.0");
    }

    #[test]
    fn does_not_offer_same_or_older_versions() {
        for tag in ["v0.1.0", "v0.1.1"] {
            let url = format!("{REPOSITORY}/releases/tag/{tag}");
            assert_eq!(latest_release(&url, parse_version("0.1.1").unwrap()), None);
        }
    }

    #[test]
    fn malformed_versions_and_release_urls_are_ignored() {
        for invalid in [
            "",
            "v",
            "1",
            "1.2",
            "1.2.3.4",
            "1.x.3",
            "v1.2.-3",
            "v+1.2.3",
            "v1.2.3;id",
        ] {
            assert_eq!(parse_version(invalid), None, "accepted {invalid:?}");
        }
        for url in ["", "https://example.com/releases/tag/v9.0.0", REPOSITORY] {
            assert_eq!(latest_release(url, parse_version("1.0.0").unwrap()), None);
        }
        assert!(install_release("v1.2.3;id").is_err());
    }

    #[test]
    fn check_cache_is_missing_then_fresh() {
        let path = test_cache();
        let _ = std::fs::remove_file(&path);
        assert!(!cache_is_fresh(&path, SystemTime::now()));
        mark_checked(&path);
        assert!(cache_is_fresh(&path, SystemTime::now()));
        let _ = std::fs::remove_file(path);
    }
}
