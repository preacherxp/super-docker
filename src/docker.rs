//! Docker Engine API client — raw HTTP over the daemon socket (see
//! `http.rs`), hand-parsed JSON (see `json.rs`), std threads for streams.
//! No external dependencies.

use std::collections::{BTreeSet, HashMap};
use std::fmt;
use std::io::Read;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError, SyncSender, TrySendError};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use crate::app::{
    AppEvent, AppSender, ContainerRow, EventRow, ImageRow, NetworkRow, RowState, StatSample,
    VolumeRow, human_bytes,
};
use crate::http::{self, Transport};
use crate::json::{self, Value};
use crate::operations;

// ---------------------------------------------------------------- errors

#[derive(Debug)]
pub struct Error(String);

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Error {}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Error(e.to_string())
    }
}

// ---------------------------------------------------------------- client

#[derive(Debug, Clone)]
pub struct Docker {
    transport: Transport,
}

/// Resolve the daemon socket: `DOCKER_HOST` when set, else the first
/// existing well-known socket path.
pub fn connect() -> Result<Docker, Error> {
    if let Some(host) = std::env::var("DOCKER_HOST")
        .ok()
        .filter(|host| !host.is_empty())
    {
        if let Some(p) = host.strip_prefix("unix://") {
            return Ok(Docker {
                transport: Transport::Unix(PathBuf::from(p)),
            });
        }
        if let Some(a) = host.strip_prefix("tcp://") {
            return Ok(Docker {
                transport: Transport::Tcp(a.trim_end_matches('/').to_string()),
            });
        }
        return Err(Error(format!("unsupported DOCKER_HOST: {host}")));
    }
    let home = std::env::var("HOME").unwrap_or_default();
    let mut candidates = vec![PathBuf::from("/var/run/docker.sock")];
    if let Ok(xdg) = std::env::var("XDG_RUNTIME_DIR") {
        candidates.push(PathBuf::from(xdg).join("docker.sock"));
    }
    if !home.is_empty() {
        let home = PathBuf::from(home);
        candidates.push(home.join(".docker/run/docker.sock"));
        candidates.push(home.join(".colima/default/docker.sock"));
        candidates.push(home.join(".rd/docker.sock"));
    }
    for p in candidates {
        if p.exists() {
            return Ok(Docker {
                transport: Transport::Unix(p),
            });
        }
    }
    Err(Error(
        "no docker socket found (is the daemon running? try DOCKER_HOST)".into(),
    ))
}

impl Docker {
    /// Client that never touches a socket — for tests and benchmarks.
    #[doc(hidden)]
    pub fn dummy() -> Docker {
        Docker {
            transport: Transport::Unix(PathBuf::from("/nonexistent/docker.sock")),
        }
    }

    /// One API call; 4xx/5xx becomes `Err` with the daemon's message.
    fn call(&self, method: &str, path: &str) -> Result<http::Response, Error> {
        let mut resp = http::request(&self.transport, method, path)?;
        if resp.status >= 400 {
            let body = resp.read_all().unwrap_or_default();
            let text = String::from_utf8_lossy(&body);
            let msg = json::parse(text.trim())
                .ok()
                .and_then(|v| v.str_of("message"))
                .unwrap_or_else(|| text.trim().to_string());
            return Err(Error(if msg.is_empty() {
                format!("docker: http {}", resp.status)
            } else {
                msg
            }));
        }
        Ok(resp)
    }

    fn get_json(&self, path: &str) -> Result<Value, Error> {
        let mut resp = self.call("GET", path)?;
        let body = resp.read_all()?;
        json::parse(String::from_utf8_lossy(&body).trim()).map_err(Error)
    }

    fn get_json_cancellable(&self, path: &str, handle: &TaskHandle) -> Result<Value, Error> {
        let mut resp = self.call("GET", path)?;
        if !handle.register(&resp) {
            return Err(Error("request cancelled".into()));
        }
        let body = resp.read_all()?;
        json::parse(String::from_utf8_lossy(&body).trim()).map_err(Error)
    }

    /// POST/DELETE where only success matters; response body is drained.
    fn simple(&self, method: &str, path: &str) -> Result<(), Error> {
        let mut resp = self.call(method, path)?;
        let _ = resp.read_all();
        Ok(())
    }

    fn post_json(&self, path: &str) -> Result<Value, Error> {
        let mut resp = self.call("POST", path)?;
        let body = resp.read_all()?;
        json::parse(String::from_utf8_lossy(&body).trim()).map_err(Error)
    }

    /// Open a streaming endpoint (events / stats / logs) — caller reads.
    fn stream(&self, path: &str) -> Result<http::Response, Error> {
        self.call("GET", path)
    }

    fn version(&self) -> Result<String, Error> {
        Ok(self
            .get_json("/version")?
            .str_of("Version")
            .unwrap_or_default())
    }
}

// ------------------------------------------------------------ task handle

/// Cancellation for a streaming worker thread. `abort()` shuts down the
/// stream's socket, which unblocks the thread's read and ends its loop —
/// the std-thread stand-in for `JoinHandle::abort`.
#[derive(Clone, Default)]
pub struct TaskHandle(Arc<TaskInner>);

#[derive(Default)]
struct TaskInner {
    aborted: AtomicBool,
    aborter: Mutex<Option<http::Aborter>>,
}

impl TaskHandle {
    pub fn abort(&self) {
        self.0.aborted.store(true, Ordering::SeqCst);
        if let Some(a) = self.0.aborter.lock().unwrap().as_ref() {
            a.abort();
        }
    }

    /// Attach a live response; returns false when already aborted (the
    /// worker should bail instead of reading a stream nobody wants).
    fn register(&self, resp: &http::Response) -> bool {
        *self.0.aborter.lock().unwrap() = resp.aborter().ok();
        !self.0.aborted.load(Ordering::SeqCst)
    }
}

type StatTasks = Arc<Mutex<HashMap<String, TaskHandle>>>;

const REFRESH_DEBOUNCE: Duration = Duration::from_millis(75);
const DISCONNECTED_CONTAINER_POLL_TICKS: u64 = 1;
const HEALTHY_CONTAINER_POLL_TICKS: u64 = 15;
const RESOURCE_POLL_TICKS: u64 = 30;
const VOLUME_SIZE_POLL_TICKS: u64 = 150;

#[derive(Debug, Clone, Copy)]
enum RefreshKind {
    Containers,
    Images,
    Volumes,
    Networks,
    VolumeSizes,
}

#[derive(Default)]
struct RefreshSet {
    containers: bool,
    images: bool,
    volumes: bool,
    networks: bool,
    volume_sizes: bool,
}

impl RefreshSet {
    fn insert(&mut self, kind: RefreshKind) {
        match kind {
            RefreshKind::Containers => self.containers = true,
            RefreshKind::Images => self.images = true,
            RefreshKind::Volumes => self.volumes = true,
            RefreshKind::Networks => self.networks = true,
            RefreshKind::VolumeSizes => self.volume_sizes = true,
        }
    }
}

fn request_refresh(tx: &SyncSender<RefreshKind>, kind: RefreshKind) -> bool {
    match tx.try_send(kind) {
        Ok(()) | Err(TrySendError::Full(_)) => true,
        Err(TrySendError::Disconnected(_)) => false,
    }
}

fn tick_due(ticks: u64, cadence: u64) -> bool {
    ticks.checked_rem(cadence) == Some(0)
}

// ---------------------------------------------------------------- worker

pub fn spawn_worker(docker: Docker, tx: AppSender) {
    let stat_tasks: StatTasks = Default::default();
    let events_healthy = Arc::new(AtomicBool::new(false));
    let (refresh_tx, refresh_rx) = mpsc::sync_channel::<RefreshKind>(64);

    // One scheduler owns list requests. Event storms and poll ticks only set
    // resource bits, so a compose burst cannot trigger hundreds of identical
    // full-list requests.
    {
        let docker = docker.clone();
        let tx = tx.clone();
        let stat_tasks = stat_tasks.clone();
        thread::spawn(move || {
            if let Ok(v) = docker.version() {
                let _ = tx.send(AppEvent::Version(v));
            }
            refresh_containers(&docker, &tx, &stat_tasks);
            refresh_images(&docker, &tx);
            refresh_volumes(&docker, &tx);
            refresh_networks(&docker, &tx);
            refresh_volume_sizes(&docker, &tx);
            loop {
                let first = match refresh_rx.recv() {
                    Ok(kind) => kind,
                    Err(_) => return,
                };
                let mut pending = RefreshSet::default();
                pending.insert(first);
                let deadline = Instant::now() + REFRESH_DEBOUNCE;
                loop {
                    match refresh_rx
                        .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                    {
                        Ok(kind) => pending.insert(kind),
                        Err(RecvTimeoutError::Timeout) => break,
                        Err(RecvTimeoutError::Disconnected) => return,
                    }
                }
                if pending.containers && !refresh_containers(&docker, &tx, &stat_tasks) {
                    // UI is gone — stop the stat streams and exit
                    for (_, h) in stat_tasks.lock().unwrap().drain() {
                        h.abort();
                    }
                    return;
                }
                if pending.images {
                    refresh_images(&docker, &tx);
                }
                if pending.volumes {
                    refresh_volumes(&docker, &tx);
                }
                if pending.networks {
                    refresh_networks(&docker, &tx);
                }
                if pending.volume_sizes {
                    refresh_volume_sizes(&docker, &tx);
                }
            }
        });
    }

    // Polling is a fast fallback only while the event stream is down. A slow
    // reconciliation remains while healthy in case the daemon drops an event.
    {
        let refresh_tx = refresh_tx.clone();
        let events_healthy = events_healthy.clone();
        thread::spawn(move || {
            let mut ticks = 0u64;
            loop {
                thread::sleep(Duration::from_secs(2));
                ticks = ticks.wrapping_add(1);
                let container_ticks = if events_healthy.load(Ordering::SeqCst) {
                    HEALTHY_CONTAINER_POLL_TICKS
                } else {
                    DISCONNECTED_CONTAINER_POLL_TICKS
                };
                if tick_due(ticks, container_ticks)
                    && !request_refresh(&refresh_tx, RefreshKind::Containers)
                {
                    return;
                }
                if tick_due(ticks, RESOURCE_POLL_TICKS) {
                    for kind in [
                        RefreshKind::Images,
                        RefreshKind::Volumes,
                        RefreshKind::Networks,
                    ] {
                        if !request_refresh(&refresh_tx, kind) {
                            return;
                        }
                    }
                }
                if tick_due(ticks, VOLUME_SIZE_POLL_TICKS)
                    && !request_refresh(&refresh_tx, RefreshKind::VolumeSizes)
                {
                    return;
                }
            }
        });
    }

    thread::spawn(move || {
        loop {
            if let Ok(mut resp) = docker.stream("/events") {
                events_healthy.store(true, Ordering::SeqCst);
                while let Ok(Some(line)) = resp.read_line() {
                    let Ok(msg) = json::parse(&line) else {
                        continue;
                    };
                    // `docker exec` produces several exec_* events but does not
                    // change any list data. Ignore the whole event here so one
                    // shell launch does not trigger a burst of identical API
                    // refreshes.
                    let Some(row) = event_row(&msg) else { continue };
                    if tx.send(AppEvent::Event(row)).is_err() {
                        return;
                    }
                    let connected = match msg.get("Type").and_then(Value::as_str) {
                        Some("container") => request_refresh(&refresh_tx, RefreshKind::Containers),
                        Some("image") => {
                            request_refresh(&refresh_tx, RefreshKind::Images)
                                && request_refresh(&refresh_tx, RefreshKind::Containers)
                        }
                        Some("volume") => request_refresh(&refresh_tx, RefreshKind::Volumes),
                        Some("network") => request_refresh(&refresh_tx, RefreshKind::Networks),
                        _ => true,
                    };
                    if !connected {
                        return;
                    }
                }
            }
            events_healthy.store(false, Ordering::SeqCst);
            thread::sleep(Duration::from_secs(2));
            if !request_refresh(&refresh_tx, RefreshKind::Containers) {
                return;
            }
        }
    });
}

/// Daemon event → display row for the events overlay.
/// `exec_*` churn is dropped — one shell session emits half a dozen of them.
fn event_row(msg: &Value) -> Option<EventRow> {
    let action = msg.str_of("Action").unwrap_or_default();
    if action.starts_with("exec_") {
        return None;
    }
    let (id, name) = msg
        .get("Actor")
        .map(|a| {
            let id = a.str_of("ID").unwrap_or_default();
            let name = a
                .get("Attributes")
                .and_then(|m| m.str_of("name"))
                .unwrap_or_else(|| id.chars().take(12).collect());
            (id, name)
        })
        .unwrap_or_default();
    Some(EventRow {
        at: msg.get("time").and_then(Value::as_i64).unwrap_or(0),
        typ: msg.str_of("Type").unwrap_or_default(),
        action,
        id,
        name,
    })
}

// -------------------------------------------------------- container list

#[derive(Debug, Default)]
pub struct PortSummary {
    pub private_port: u16,
    pub public_port: Option<u16>,
    pub typ: Option<String>,
}

#[derive(Debug, Default)]
pub struct ContainerSummary {
    pub id: Option<String>,
    pub names: Option<Vec<String>>,
    pub image: Option<String>,
    pub image_id: Option<String>,
    /// Daemon state string: running / paused / restarting / exited / …
    pub state: Option<String>,
    pub status: Option<String>,
    pub ports: Option<Vec<PortSummary>>,
    pub created: Option<i64>,
    pub labels: Option<HashMap<String, String>>,
    /// Named volumes from Mounts[].Name.
    pub mount_names: Vec<String>,
    /// Attached network names.
    pub networks: Vec<String>,
}

fn summary_from_value(c: &Value) -> ContainerSummary {
    let names = c.get("Names").and_then(Value::as_array).map(|a| {
        a.iter()
            .filter_map(|v| v.as_str().map(str::to_string))
            .collect()
    });
    let ports = c.get("Ports").and_then(Value::as_array).map(|a| {
        a.iter()
            .map(|p| PortSummary {
                private_port: p.get("PrivatePort").and_then(Value::as_u64).unwrap_or(0) as u16,
                public_port: p
                    .get("PublicPort")
                    .and_then(Value::as_u64)
                    .map(|v| v as u16),
                typ: p.str_of("Type"),
            })
            .collect()
    });
    let labels = c.get("Labels").and_then(Value::as_object).map(|m| {
        m.iter()
            .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
            .collect()
    });
    let mount_names = c
        .get("Mounts")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(|m| m.str_of("Name")).collect())
        .unwrap_or_default();
    let networks = c
        .get("NetworkSettings")
        .and_then(|ns| ns.get("Networks"))
        .and_then(Value::as_object)
        .map(|m| m.keys().cloned().collect())
        .unwrap_or_default();
    ContainerSummary {
        id: c.str_of("Id"),
        names,
        image: c.str_of("Image"),
        image_id: c.str_of("ImageID"),
        state: c.str_of("State"),
        status: c.str_of("Status"),
        ports,
        created: c.get("Created").and_then(Value::as_i64),
        labels,
        mount_names,
        networks,
    }
}

fn row_from_summary(c: ContainerSummary) -> ContainerRow {
    let mut labels = c.labels.unwrap_or_default();
    let compose_project = labels.remove("com.docker.compose.project");
    let compose_service = labels.remove("com.docker.compose.service");
    let compose_files = labels
        .remove("com.docker.compose.project.config_files")
        .unwrap_or_default();
    let compose_dir = labels
        .remove("com.docker.compose.project.working_dir")
        .unwrap_or_default();
    let name = c
        .names
        .as_ref()
        .and_then(|n| n.first())
        .map(|n| n.trim_start_matches('/').to_string())
        .unwrap_or_else(|| "<unnamed>".into());
    let state = match c.state.as_deref() {
        Some("running") => RowState::Running,
        Some("paused") => RowState::Paused,
        Some("restarting") => RowState::Restarting,
        Some("exited") => RowState::Exited,
        Some("created") => RowState::Created,
        Some("dead") => RowState::Dead,
        _ => RowState::Other,
    };
    let ports = c
        .ports
        .as_ref()
        .map(|ps| {
            ps.iter()
                .map(|p| {
                    let proto = p.typ.clone().unwrap_or_default();
                    match p.public_port {
                        Some(pubp) => format!("{pubp}→{}/{proto}", p.private_port),
                        None => format!("{}/{proto}", p.private_port),
                    }
                })
                .collect::<BTreeSet<_>>()
                .into_iter()
                .collect::<Vec<_>>()
                .join(" ")
        })
        .unwrap_or_default();
    ContainerRow {
        id: c.id.unwrap_or_default(),
        name,
        image: c.image.unwrap_or_default(),
        image_id: c.image_id.unwrap_or_default(),
        state,
        status: c.status.unwrap_or_default(),
        ports,
        created: c.created.unwrap_or(0),
        compose_project,
        compose_service,
        compose_files,
        compose_dir,
        volumes: c.mount_names,
        networks: c.networks,
    }
}

/// Returns false once the UI side of the channel is gone.
fn refresh_containers(docker: &Docker, tx: &AppSender, stat_tasks: &StatTasks) -> bool {
    match docker.get_json("/containers/json?all=true") {
        Ok(list) => {
            // ordering is applied in App::filtered_* per the active sort
            let rows: Vec<ContainerRow> = list
                .as_array()
                .unwrap_or(&[])
                .iter()
                .map(|c| row_from_summary(summary_from_value(c)))
                .collect();

            let live: BTreeSet<&str> = rows
                .iter()
                .filter(|r| matches!(r.state, RowState::Running | RowState::Paused))
                .map(|r| r.id.as_str())
                .collect();
            {
                let mut tasks = stat_tasks.lock().unwrap();
                tasks.retain(|id, h| {
                    if live.contains(id.as_str()) {
                        true
                    } else {
                        h.abort();
                        false
                    }
                });
                for id in live {
                    if !tasks.contains_key(id) {
                        tasks.insert(
                            id.to_string(),
                            spawn_stats(docker.clone(), tx.clone(), id.to_string()),
                        );
                    }
                }
            }

            tx.send(AppEvent::Containers(rows)).is_ok()
        }
        Err(e) => tx.send(AppEvent::DockerErr(e.to_string())).is_ok(),
    }
}

fn refresh_images(docker: &Docker, tx: &AppSender) {
    if let Ok(list) = docker.get_json("/images/json") {
        let rows: Vec<ImageRow> = list
            .as_array()
            .unwrap_or(&[])
            .iter()
            .map(|i| ImageRow {
                id: i.str_of("Id").unwrap_or_default(),
                tag: i
                    .get("RepoTags")
                    .and_then(Value::as_array)
                    .and_then(|a| a.first())
                    .and_then(Value::as_str)
                    .unwrap_or("<none>:<none>")
                    .to_string(),
                size: i.get("Size").and_then(Value::as_i64).unwrap_or(0),
                created: i.get("Created").and_then(Value::as_i64).unwrap_or(0),
                containers: i.get("Containers").and_then(Value::as_i64).unwrap_or(-1),
            })
            .collect();
        let _ = tx.send(AppEvent::Images(rows));
    }
}

fn refresh_volumes(docker: &Docker, tx: &AppSender) {
    if let Ok(resp) = docker.get_json("/volumes") {
        let rows: Vec<VolumeRow> = resp
            .get("Volumes")
            .and_then(Value::as_array)
            .unwrap_or(&[])
            .iter()
            .map(|v| VolumeRow {
                name: v.str_of("Name").unwrap_or_default(),
                driver: v.str_of("Driver").unwrap_or_default(),
                mountpoint: v.str_of("Mountpoint").unwrap_or_default(),
                created: v.str_of("CreatedAt").unwrap_or_default(),
            })
            .collect();
        let _ = tx.send(AppEvent::Volumes(rows));
    }
}

fn refresh_networks(docker: &Docker, tx: &AppSender) {
    if let Ok(list) = docker.get_json("/networks") {
        let rows: Vec<NetworkRow> = list
            .as_array()
            .unwrap_or(&[])
            .iter()
            .map(|n| NetworkRow {
                id: n.str_of("Id").unwrap_or_default(),
                name: n.str_of("Name").unwrap_or_default(),
                driver: n.str_of("Driver").unwrap_or_default(),
                scope: n.str_of("Scope").unwrap_or_default(),
                subnet: n
                    .get("IPAM")
                    .and_then(|i| i.get("Config"))
                    .and_then(Value::as_array)
                    .and_then(|c| c.first())
                    .and_then(|c| c.str_of("Subnet"))
                    .unwrap_or_default(),
            })
            .collect();
        let _ = tx.send(AppEvent::Networks(rows));
    }
}

/// Volume sizes come from the `/system/df` endpoint — the volume list does
/// not report them. Refreshed on the slow cadence only; df walks the disk.
fn refresh_volume_sizes(docker: &Docker, tx: &AppSender) {
    if let Ok(df) = docker.get_json("/system/df") {
        let sizes: HashMap<String, i64> = df
            .get("Volumes")
            .and_then(Value::as_array)
            .unwrap_or(&[])
            .iter()
            .filter_map(|v| {
                let name = v.str_of("Name")?;
                let size = v.get("UsageData")?.get("Size")?.as_i64()?;
                Some((name, size))
            })
            .collect();
        let _ = tx.send(AppEvent::VolumeSizes(sizes));
    }
}

// ----------------------------------------------------------------- stats

#[derive(Debug, Default)]
pub struct CpuStats {
    pub total_usage: Option<u64>,
    pub system_cpu_usage: Option<u64>,
    pub online_cpus: Option<u64>,
}

#[derive(Debug, Default)]
pub struct MemoryStats {
    pub usage: Option<u64>,
    pub limit: Option<u64>,
    /// Page cache value subtracted by `docker stats` (cgroups v1 or v2).
    pub cache: Option<u64>,
}

#[derive(Debug, Default)]
pub struct StatsResponse {
    pub cpu_stats: Option<CpuStats>,
    pub precpu_stats: Option<CpuStats>,
    pub memory_stats: Option<MemoryStats>,
    /// Network totals across all interfaces; None when the daemon omits them.
    pub networks: Option<(u64, u64)>,
    pub pids: Option<u64>,
}

fn cpu_from_value(v: &Value) -> CpuStats {
    CpuStats {
        total_usage: v
            .get("cpu_usage")
            .and_then(|u| u.get("total_usage"))
            .and_then(Value::as_u64),
        system_cpu_usage: v.get("system_cpu_usage").and_then(Value::as_u64),
        online_cpus: v.get("online_cpus").and_then(Value::as_u64),
    }
}

fn stats_from_value(v: &Value) -> StatsResponse {
    let memory_stats = v.get("memory_stats").map(|m| MemoryStats {
        usage: m.get("usage").and_then(Value::as_u64),
        limit: m.get("limit").and_then(Value::as_u64),
        cache: m
            .get("stats")
            .and_then(Value::as_object)
            .and_then(|s| {
                s.get("inactive_file")
                    .or_else(|| s.get("total_inactive_file"))
            })
            .and_then(Value::as_u64),
    });
    let networks = v.get("networks").and_then(Value::as_object).map(|m| {
        m.values().fold((0u64, 0u64), |(rx, tx), n| {
            (
                rx.saturating_add(n.get("rx_bytes").and_then(Value::as_u64).unwrap_or(0)),
                tx.saturating_add(n.get("tx_bytes").and_then(Value::as_u64).unwrap_or(0)),
            )
        })
    });
    StatsResponse {
        cpu_stats: v.get("cpu_stats").map(cpu_from_value),
        precpu_stats: v.get("precpu_stats").map(cpu_from_value),
        memory_stats,
        networks,
        pids: v
            .get("pids_stats")
            .and_then(|p| p.get("current"))
            .and_then(Value::as_u64),
    }
}

fn spawn_stats(docker: Docker, tx: AppSender, id: String) -> TaskHandle {
    let handle = TaskHandle::default();
    let h = handle.clone();
    thread::spawn(move || {
        let path = format!("/containers/{id}/stats?stream=true");
        let Ok(mut resp) = docker.stream(&path) else {
            return;
        };
        if !h.register(&resp) {
            return;
        }
        let mut previous_network: Option<(Instant, u64, u64)> = None;
        while let Ok(Some(line)) = resp.read_line() {
            let Ok(v) = json::parse(&line) else { continue };
            if let Some(mut sample) = compute_sample(&id, &stats_from_value(&v)) {
                let now = Instant::now();
                if let Some((at, rx, tx)) = previous_network {
                    (sample.rx_rate, sample.tx_rate) = network_rates(
                        (rx, tx),
                        (sample.rx, sample.tx),
                        now.saturating_duration_since(at),
                    );
                }
                previous_network = Some((now, sample.rx, sample.tx));
                if tx.send(AppEvent::Stat(sample)).is_err() {
                    return;
                }
            }
        }
    });
    handle
}

fn network_rates(previous: (u64, u64), current: (u64, u64), elapsed: Duration) -> (u64, u64) {
    let seconds = elapsed.as_secs_f64();
    if seconds <= f64::EPSILON {
        return (0, 0);
    }
    let per_second =
        |before: u64, now: u64| (now.saturating_sub(before) as f64 / seconds).round() as u64;
    (
        per_second(previous.0, current.0),
        per_second(previous.1, current.1),
    )
}

fn compute_sample(id: &str, st: &StatsResponse) -> Option<StatSample> {
    let cpu = st.cpu_stats.as_ref()?;
    let total = cpu.total_usage?;
    let pre = st.precpu_stats.as_ref();
    let pre_total = pre.and_then(|p| p.total_usage).unwrap_or(0);
    let sys = cpu.system_cpu_usage.unwrap_or(0);
    let pre_sys = pre.and_then(|p| p.system_cpu_usage).unwrap_or(0);
    let cpu_cores = cpu.online_cpus.unwrap_or(1).max(1);
    let online = cpu_cores as f64;
    let cpu_pct = if sys > pre_sys && total >= pre_total {
        (total - pre_total) as f64 / (sys - pre_sys) as f64 * online * 100.0
    } else {
        0.0
    };

    let mem = st.memory_stats.as_ref();
    let usage = mem.and_then(|m| m.usage).unwrap_or(0);
    // Match `docker stats`: subtract page cache (inactive_file on cgroups v2,
    // total_inactive_file on v1).
    let cache = mem.and_then(|m| m.cache).unwrap_or(0);
    let mem_used = usage.saturating_sub(cache);
    let mem_limit = mem.and_then(|m| m.limit).unwrap_or(0);
    let mem_pct = if mem_limit > 0 {
        mem_used as f64 / mem_limit as f64 * 100.0
    } else {
        0.0
    };

    let (rx, tx) = st.networks.unwrap_or((0, 0));
    let pids = st.pids.unwrap_or(0);

    Some(StatSample {
        id: id.to_string(),
        cpu_pct,
        cpu_cores,
        mem_pct,
        mem_used,
        mem_limit,
        rx,
        tx,
        rx_rate: 0,
        tx_rate: 0,
        pids,
    })
}

// ------------------------------------------------------------------ logs

/// Read one payload of a log stream. Docker multiplexes stdout/stderr as
/// 8-byte-header frames unless the container has a TTY, in which case the
/// stream is raw. `multiplexed` is decided once from the first bytes.
fn read_log_payload(resp: &mut http::Response, multiplexed: bool) -> Option<Vec<u8>> {
    if multiplexed {
        let mut header = [0u8; 8];
        read_exact(resp, &mut header)?;
        let size = u32::from_be_bytes([header[4], header[5], header[6], header[7]]) as usize;
        // cap a corrupt frame rather than allocating gigabytes
        if size > 16 * 1024 * 1024 {
            return None;
        }
        let mut payload = vec![0u8; size];
        read_exact(resp, &mut payload)?;
        Some(payload)
    } else {
        let mut buf = vec![0u8; 8192];
        match resp.read(&mut buf) {
            Ok(0) | Err(_) => None,
            Ok(n) => {
                buf.truncate(n);
                Some(buf)
            }
        }
    }
}

fn read_exact(r: &mut impl Read, buf: &mut [u8]) -> Option<()> {
    let mut filled = 0;
    while filled < buf.len() {
        match r.read(&mut buf[filled..]) {
            Ok(0) | Err(_) => return None,
            Ok(n) => filled += n,
        }
    }
    Some(())
}

/// Sniff the framing: a multiplexed frame starts with stream id 0/1/2 and
/// three zero bytes; raw TTY output is text. Content-Type decides when the
/// daemon sends one; the sniff covers older daemons that don't.
fn is_multiplexed(content_type: &str, first: &[u8; 8]) -> bool {
    match content_type {
        "application/vnd.docker.multiplexed-stream" => true,
        "application/vnd.docker.raw-stream" => false,
        _ => matches!(first[0], 0..=2) && first[1] == 0 && first[2] == 0 && first[3] == 0,
    }
}

/// Follow one container's logs; each payload goes to `tx` under `key`,
/// optionally prefixed per line with a compose service name.
fn log_stream(
    docker: Docker,
    tx: AppSender,
    handle: TaskHandle,
    key: String,
    id: String,
    tail: &'static str,
    service: Option<String>,
) {
    let path = format!("/containers/{id}/logs?follow=true&stdout=true&stderr=true&tail={tail}");
    let Ok(mut resp) = docker.stream(&path) else {
        return;
    };
    if !handle.register(&resp) {
        return;
    }

    // First 8 bytes decide the framing (and are part of the data when raw).
    let mut first = [0u8; 8];
    let Some(()) = read_exact(&mut resp, &mut first) else {
        return;
    };
    let multiplexed = is_multiplexed(&resp.content_type, &first);

    let mut pending: Option<Vec<u8>> = if multiplexed {
        // `first` was a frame header: read that frame's payload directly
        let size = u32::from_be_bytes([first[4], first[5], first[6], first[7]]) as usize;
        if size > 16 * 1024 * 1024 {
            return;
        }
        let mut payload = vec![0u8; size];
        match read_exact(&mut resp, &mut payload) {
            Some(()) => Some(payload),
            None => return,
        }
    } else {
        Some(first.to_vec())
    };

    loop {
        let payload = match pending.take() {
            Some(p) => p,
            None => match read_log_payload(&mut resp, multiplexed) {
                Some(p) => p,
                None => return,
            },
        };
        let text = String::from_utf8_lossy(&payload).into_owned();
        let out = match &service {
            None => text,
            Some(service) => text
                .split('\n')
                .map(|l| l.trim_end_matches('\r'))
                .filter(|l| !l.is_empty())
                .map(|l| format!("{service} ▏{l}\n"))
                .collect(),
        };
        if !out.is_empty() && tx.send(AppEvent::Log(key.clone(), out)).is_err() {
            return;
        }
    }
}

pub fn spawn_logs(docker: &Docker, tx: &AppSender, id: String) -> TaskHandle {
    let handle = TaskHandle::default();
    let (docker, tx, h) = (docker.clone(), tx.clone(), handle.clone());
    thread::spawn(move || log_stream(docker, tx, h, id.clone(), id, "500", None));
    handle
}

/// Follow logs for every container of a compose project, each line prefixed
/// with its service name, all funneled into one log buffer under `key`.
pub fn spawn_compose_logs(
    docker: &Docker,
    tx: &AppSender,
    key: String,
    members: Vec<(String, String)>,
) -> Vec<TaskHandle> {
    members
        .into_iter()
        .map(|(id, service)| {
            let handle = TaskHandle::default();
            let (docker, tx, h, key) = (docker.clone(), tx.clone(), handle.clone(), key.clone());
            thread::spawn(move || log_stream(docker, tx, h, key, id, "200", Some(service)));
            handle
        })
        .collect()
}

// --------------------------------------------------------------- inspect

pub fn spawn_inspect(docker: &Docker, tx: &AppSender, id: String) -> TaskHandle {
    let handle = TaskHandle::default();
    let task_handle = handle.clone();
    let docker = docker.clone();
    let tx = tx.clone();
    thread::spawn(move || {
        let Ok(d) = docker.get_json_cancellable(&format!("/containers/{id}/json"), &task_handle)
        else {
            return;
        };
        let mut kv: Vec<(String, String)> = Vec::new();
        let mut push = |k: &str, v: String| {
            if !v.is_empty() {
                kv.push((k.to_string(), v));
            }
        };
        push(
            "ID",
            d.str_of("Id")
                .map(|i| i[..12.min(i.len())].to_string())
                .unwrap_or_default(),
        );
        if let Some(cfg) = d.get("Config") {
            push("Image", cfg.str_of("Image").unwrap_or_default());
        }
        if let Some(st) = d.get("State") {
            push("Status", st.str_of("Status").unwrap_or_default());
            push("Started", st.str_of("StartedAt").unwrap_or_default());
            if let Some(code) = st
                .get("ExitCode")
                .and_then(Value::as_i64)
                .filter(|code| *code != 0)
            {
                push("Exit code", code.to_string());
            }
            if st.get("OOMKilled").and_then(Value::as_bool) == Some(true) {
                push("OOM killed", "yes".into());
            }
            if let Some(h) = st.get("Health") {
                push("Health", h.str_of("Status").unwrap_or_default());
                if let Some(streak) = h
                    .get("FailingStreak")
                    .and_then(Value::as_i64)
                    .filter(|streak| *streak > 0)
                {
                    push("Health fails", streak.to_string());
                }
                // last few probe results, newest first — first line only
                let log = h.get("Log").and_then(Value::as_array).unwrap_or(&[]);
                for r in log.iter().rev().take(3) {
                    let code = r
                        .get("ExitCode")
                        .and_then(Value::as_i64)
                        .unwrap_or_default();
                    let out = r.str_of("Output").unwrap_or_default();
                    let line: String = out.lines().next().unwrap_or("").chars().take(120).collect();
                    push("Probe", format!("[{code}] {line}").trim_end().to_string());
                }
            }
        }
        let args = d
            .get("Args")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(Value::as_str)
                    .collect::<Vec<_>>()
                    .join(" ")
            })
            .unwrap_or_default();
        let cmd = format!("{} {}", d.str_of("Path").unwrap_or_default(), args);
        push("Command", cmd.trim().to_string());
        push(
            "Restarts",
            d.get("RestartCount")
                .and_then(Value::as_i64)
                .unwrap_or(0)
                .to_string(),
        );
        if let Some(rp) = d.get("HostConfig").and_then(|hc| hc.get("RestartPolicy")) {
            push("Restart policy", rp.str_of("Name").unwrap_or_default());
        }
        if let Some(nets) = d
            .get("NetworkSettings")
            .and_then(|ns| ns.get("Networks"))
            .and_then(Value::as_object)
        {
            for (name, ep) in nets {
                let ip = ep.str_of("IPAddress").unwrap_or_default();
                if !ip.is_empty() {
                    push(&format!("Net {name}"), ip);
                }
            }
        }
        for m in d.get("Mounts").and_then(Value::as_array).unwrap_or(&[]) {
            let src = m.str_of("Source").unwrap_or_default();
            let dst = m.str_of("Destination").unwrap_or_default();
            if !dst.is_empty() {
                push("Mount", format!("{src} → {dst}"));
            }
        }
        for e in d
            .get("Config")
            .and_then(|cfg| cfg.get("Env"))
            .and_then(Value::as_array)
            .unwrap_or(&[])
        {
            push("Env", e.as_str().unwrap_or_default().to_string());
        }
        let _ = tx.send(AppEvent::Inspect(id, kv));
    });
    handle
}

// --------------------------------------------------------------- actions

#[derive(Debug, Clone, Copy)]
pub enum CtrAction {
    Start,
    Stop,
    Restart,
    Pause,
    Unpause,
    Remove,
}

impl CtrAction {
    fn verb(self) -> &'static str {
        match self {
            CtrAction::Start => "start",
            CtrAction::Stop => "stop",
            CtrAction::Restart => "restart",
            CtrAction::Pause => "pause",
            CtrAction::Unpause => "unpause",
            CtrAction::Remove => "remove",
        }
    }
}

fn run_ctr_action(docker: &Docker, action: CtrAction, id: &str) -> Result<(), Error> {
    match action {
        CtrAction::Start => docker.simple("POST", &format!("/containers/{id}/start")),
        CtrAction::Stop => docker.simple("POST", &format!("/containers/{id}/stop")),
        CtrAction::Restart => docker.simple("POST", &format!("/containers/{id}/restart")),
        CtrAction::Pause => docker.simple("POST", &format!("/containers/{id}/pause")),
        CtrAction::Unpause => docker.simple("POST", &format!("/containers/{id}/unpause")),
        CtrAction::Remove => docker.simple("DELETE", &format!("/containers/{id}?force=true")),
    }
}

pub fn container_action(
    docker: &Docker,
    tx: &AppSender,
    action: CtrAction,
    id: String,
    name: String,
) {
    let docker = docker.clone();
    let tx = tx.clone();
    thread::spawn(move || {
        let operation = operations::begin(action.verb(), "container", &name, &id);
        let verb = match action {
            CtrAction::Start => "started",
            CtrAction::Stop => "stopped",
            CtrAction::Restart => "restarted",
            CtrAction::Pause => "paused",
            CtrAction::Unpause => "unpaused",
            CtrAction::Remove => "removed",
        };
        let result = run_ctr_action(&docker, action, &id);
        operation.finish(&result);
        let _ = match result {
            Ok(()) => tx.send(AppEvent::Toast(format!("{verb} {name}"), false)),
            Err(e) => tx.send(AppEvent::Toast(format!("{name}: {e}"), true)),
        };
    });
}

/// Kill a container with an explicit signal (`K` picker: TERM / KILL / HUP).
pub fn kill_container(
    docker: &Docker,
    tx: &AppSender,
    id: String,
    name: String,
    signal: &'static str,
) {
    let action = format!("kill {signal}");
    let docker = docker.clone();
    let tx = tx.clone();
    thread::spawn(move || {
        let operation = operations::begin(&action, "container", &name, &id);
        let result = docker.simple("POST", &format!("/containers/{id}/kill?signal={signal}"));
        operation.finish(&result);
        let _ = match result {
            Ok(()) => tx.send(AppEvent::Toast(format!("sent {signal} to {name}"), false)),
            Err(e) => tx.send(AppEvent::Toast(format!("{name}: {e}"), true)),
        };
    });
}

pub fn remove_image(docker: &Docker, tx: &AppSender, id: String, tag: String) {
    let docker = docker.clone();
    let tx = tx.clone();
    thread::spawn(move || {
        let operation = operations::begin("remove", "image", &tag, &id);
        let result = docker.simple("DELETE", &format!("/images/{id}?force=true"));
        operation.finish(&result);
        let _ = match result {
            Ok(()) => tx.send(AppEvent::Toast(format!("removed {tag}"), false)),
            Err(e) => tx.send(AppEvent::Toast(format!("{tag}: {e}"), true)),
        };
    });
}

pub fn remove_volume(docker: &Docker, tx: &AppSender, name: String) {
    let docker = docker.clone();
    let tx = tx.clone();
    thread::spawn(move || {
        let operation = operations::begin("remove", "volume", &name, "");
        let result = docker.simple("DELETE", &format!("/volumes/{name}"));
        operation.finish(&result);
        let _ = match result {
            Ok(()) => tx.send(AppEvent::Toast(format!("removed {name}"), false)),
            Err(e) => tx.send(AppEvent::Toast(format!("{name}: {e}"), true)),
        };
    });
}

pub fn remove_network(docker: &Docker, tx: &AppSender, id: String, name: String) {
    let docker = docker.clone();
    let tx = tx.clone();
    thread::spawn(move || {
        let operation = operations::begin("remove", "network", &name, &id);
        let result = docker.simple("DELETE", &format!("/networks/{id}"));
        operation.finish(&result);
        let _ = match result {
            Ok(()) => tx.send(AppEvent::Toast(format!("removed {name}"), false)),
            Err(e) => tx.send(AppEvent::Toast(format!("{name}: {e}"), true)),
        };
    });
}

fn batch_toast(tx: &AppSender, what: &str, total: usize, failed: usize, last_err: String) {
    let _ = if failed == 0 {
        tx.send(AppEvent::Toast(format!("removed {total} {what}"), false))
    } else {
        tx.send(AppEvent::Toast(
            format!("removed {}/{total} {what} — {last_err}", total - failed),
            true,
        ))
    };
}

pub fn remove_containers_batch(docker: &Docker, tx: &AppSender, items: Vec<(String, String)>) {
    let target = items
        .iter()
        .map(|(_, name)| name.as_str())
        .collect::<Vec<_>>()
        .join(", ");
    let docker = docker.clone();
    let tx = tx.clone();
    thread::spawn(move || {
        let operation = operations::begin("remove batch", "container", &target, "");
        let total = items.len();
        let mut failed = 0;
        let mut last_err = String::new();
        for (id, name) in items {
            if let Err(e) = docker.simple("DELETE", &format!("/containers/{id}?force=true")) {
                failed += 1;
                last_err = format!("{name}: {e}");
            }
        }
        let result: Result<(), String> = if failed == 0 {
            Ok(())
        } else {
            Err(last_err.clone())
        };
        operation.finish(&result);
        batch_toast(&tx, "containers", total, failed, last_err);
    });
}

pub fn remove_images_batch(docker: &Docker, tx: &AppSender, items: Vec<(String, String)>) {
    let target = items
        .iter()
        .map(|(_, tag)| tag.as_str())
        .collect::<Vec<_>>()
        .join(", ");
    let docker = docker.clone();
    let tx = tx.clone();
    thread::spawn(move || {
        let operation = operations::begin("remove batch", "image", &target, "");
        let total = items.len();
        let mut failed = 0;
        let mut last_err = String::new();
        for (id, tag) in items {
            if let Err(e) = docker.simple("DELETE", &format!("/images/{id}?force=true")) {
                failed += 1;
                last_err = format!("{tag}: {e}");
            }
        }
        let result: Result<(), String> = if failed == 0 {
            Ok(())
        } else {
            Err(last_err.clone())
        };
        operation.finish(&result);
        batch_toast(&tx, "images", total, failed, last_err);
    });
}

pub fn remove_volumes_batch(docker: &Docker, tx: &AppSender, items: Vec<String>) {
    let target = items.join(", ");
    let docker = docker.clone();
    let tx = tx.clone();
    thread::spawn(move || {
        let operation = operations::begin("remove batch", "volume", &target, "");
        let total = items.len();
        let mut failed = 0;
        let mut last_err = String::new();
        for name in items {
            if let Err(e) = docker.simple("DELETE", &format!("/volumes/{name}")) {
                failed += 1;
                last_err = format!("{name}: {e}");
            }
        }
        let result: Result<(), String> = if failed == 0 {
            Ok(())
        } else {
            Err(last_err.clone())
        };
        operation.finish(&result);
        batch_toast(&tx, "volumes", total, failed, last_err);
    });
}

pub fn remove_networks_batch(docker: &Docker, tx: &AppSender, items: Vec<(String, String)>) {
    let target = items
        .iter()
        .map(|(_, name)| name.as_str())
        .collect::<Vec<_>>()
        .join(", ");
    let docker = docker.clone();
    let tx = tx.clone();
    thread::spawn(move || {
        let operation = operations::begin("remove batch", "network", &target, "");
        let total = items.len();
        let mut failed = 0;
        let mut last_err = String::new();
        for (id, name) in items {
            if let Err(e) = docker.simple("DELETE", &format!("/networks/{id}")) {
                failed += 1;
                last_err = format!("{name}: {e}");
            }
        }
        let result: Result<(), String> = if failed == 0 {
            Ok(())
        } else {
            Err(last_err.clone())
        };
        operation.finish(&result);
        batch_toast(&tx, "networks", total, failed, last_err);
    });
}

pub fn prune_containers(docker: &Docker, tx: &AppSender) {
    let docker = docker.clone();
    let tx = tx.clone();
    thread::spawn(move || {
        let operation = operations::begin("prune", "container", "all stopped", "");
        let result = docker.post_json("/containers/prune");
        operation.finish(&result);
        let _ = match result {
            Ok(r) => {
                let n = r
                    .get("ContainersDeleted")
                    .and_then(Value::as_array)
                    .map(|v| v.len())
                    .unwrap_or(0);
                tx.send(AppEvent::Toast(format!("pruned {n} containers"), false))
            }
            Err(e) => tx.send(AppEvent::Toast(format!("prune: {e}"), true)),
        };
    });
}

pub fn prune_images(docker: &Docker, tx: &AppSender) {
    let docker = docker.clone();
    let tx = tx.clone();
    thread::spawn(move || {
        let operation = operations::begin("prune", "image", "dangling", "");
        let result = docker.post_json("/images/prune");
        operation.finish(&result);
        let _ = match result {
            Ok(r) => {
                let freed = human_bytes(
                    r.get("SpaceReclaimed")
                        .and_then(Value::as_i64)
                        .unwrap_or(0)
                        .max(0) as u64,
                );
                tx.send(AppEvent::Toast(
                    format!("pruned images, freed {freed}"),
                    false,
                ))
            }
            Err(e) => tx.send(AppEvent::Toast(format!("prune: {e}"), true)),
        };
    });
}

pub fn prune_volumes(docker: &Docker, tx: &AppSender) {
    let docker = docker.clone();
    let tx = tx.clone();
    thread::spawn(move || {
        let operation = operations::begin("prune", "volume", "unused anonymous", "");
        let result = docker.post_json("/volumes/prune");
        operation.finish(&result);
        let _ = match result {
            Ok(r) => {
                let freed = human_bytes(
                    r.get("SpaceReclaimed")
                        .and_then(Value::as_i64)
                        .unwrap_or(0)
                        .max(0) as u64,
                );
                tx.send(AppEvent::Toast(
                    format!("pruned volumes, freed {freed}"),
                    false,
                ))
            }
            Err(e) => tx.send(AppEvent::Toast(format!("prune: {e}"), true)),
        };
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refresh_set_coalesces_every_resource_kind() {
        let mut set = RefreshSet::default();
        for kind in [
            RefreshKind::Containers,
            RefreshKind::Images,
            RefreshKind::Volumes,
            RefreshKind::Networks,
            RefreshKind::VolumeSizes,
            RefreshKind::Containers,
        ] {
            set.insert(kind);
        }
        assert!(set.containers);
        assert!(set.images);
        assert!(set.volumes);
        assert!(set.networks);
        assert!(set.volume_sizes);
    }

    #[test]
    fn refresh_request_treats_full_as_coalesced_and_disconnect_as_stop() {
        let (tx, rx) = mpsc::sync_channel(1);
        assert!(request_refresh(&tx, RefreshKind::Images));
        assert!(request_refresh(&tx, RefreshKind::Volumes));
        drop(rx);
        assert!(!request_refresh(&tx, RefreshKind::Networks));
    }

    // ---- row_from_summary ----

    #[test]
    fn summary_name_strips_leading_slash() {
        let c = ContainerSummary {
            names: Some(vec!["/my-ctr".into()]),
            ..Default::default()
        };
        assert_eq!(row_from_summary(c).name, "my-ctr");
    }

    #[test]
    fn summary_missing_name_gets_placeholder() {
        let row = row_from_summary(ContainerSummary::default());
        assert_eq!(row.name, "<unnamed>");
        assert_eq!(row.state, RowState::Other);
        assert_eq!(row.created, 0);
    }

    #[test]
    fn summary_state_mapping() {
        for (e, s) in [
            ("running", RowState::Running),
            ("paused", RowState::Paused),
            ("restarting", RowState::Restarting),
            ("exited", RowState::Exited),
            ("created", RowState::Created),
            ("dead", RowState::Dead),
            ("bogus", RowState::Other),
        ] {
            let c = ContainerSummary {
                state: Some(e.into()),
                ..Default::default()
            };
            assert_eq!(row_from_summary(c).state, s);
        }
    }

    #[test]
    fn summary_ports_dedup_and_format() {
        let port = |private: u16, public: Option<u16>| PortSummary {
            private_port: private,
            public_port: public,
            typ: Some("tcp".into()),
        };
        let c = ContainerSummary {
            // duplicate mapping appears twice (e.g. ipv4 + ipv6) -> dedup
            ports: Some(vec![
                port(80, Some(8080)),
                port(80, Some(8080)),
                port(5432, None),
            ]),
            ..Default::default()
        };
        assert_eq!(row_from_summary(c).ports, "5432/tcp 8080→80/tcp");
    }

    #[test]
    fn summary_extracts_compose_labels() {
        let mut labels = HashMap::new();
        labels.insert("com.docker.compose.project".to_string(), "proj".to_string());
        labels.insert("com.docker.compose.service".to_string(), "web".to_string());
        labels.insert(
            "com.docker.compose.project.config_files".to_string(),
            "/a/compose.yml".to_string(),
        );
        labels.insert(
            "com.docker.compose.project.working_dir".to_string(),
            "/a".to_string(),
        );
        let c = ContainerSummary {
            labels: Some(labels),
            ..Default::default()
        };
        let row = row_from_summary(c);
        assert_eq!(row.compose_project.as_deref(), Some("proj"));
        assert_eq!(row.compose_service.as_deref(), Some("web"));
        assert_eq!(row.compose_files, "/a/compose.yml");
        assert_eq!(row.compose_dir, "/a");
    }

    #[test]
    fn summary_without_labels_has_no_compose() {
        let row = row_from_summary(ContainerSummary::default());
        assert!(row.compose_project.is_none());
        assert!(row.compose_service.is_none());
        assert!(row.compose_files.is_empty());
    }

    // ---- summary_from_value (API JSON -> summary) ----

    #[test]
    fn summary_parsed_from_api_json() {
        let v = json::parse(
            r#"{
                "Id": "abc123",
                "Names": ["/web"],
                "Image": "nginx:latest",
                "ImageID": "sha256:fff",
                "State": "running",
                "Status": "Up 2 hours",
                "Created": 1700000000,
                "Ports": [{"PrivatePort": 80, "PublicPort": 8080, "Type": "tcp"}],
                "Labels": {"com.docker.compose.project": "proj"},
                "Mounts": [{"Name": "data"}, {"Source": "/host"}],
                "NetworkSettings": {"Networks": {"bridge": {}}}
            }"#,
        )
        .unwrap();
        let row = row_from_summary(summary_from_value(&v));
        assert_eq!(row.id, "abc123");
        assert_eq!(row.name, "web");
        assert_eq!(row.image, "nginx:latest");
        assert_eq!(row.state, RowState::Running);
        assert_eq!(row.ports, "8080→80/tcp");
        assert_eq!(row.created, 1700000000);
        assert_eq!(row.compose_project.as_deref(), Some("proj"));
        assert_eq!(row.volumes, vec!["data".to_string()]);
        assert_eq!(row.networks, vec!["bridge".to_string()]);
    }

    // ---- event_row ----

    #[test]
    fn event_row_parses_and_skips_exec() {
        let v = json::parse(
            r#"{"Type":"container","Action":"start",
                "Actor":{"ID":"abc","Attributes":{"name":"web"}},"time":123}"#,
        )
        .unwrap();
        let row = event_row(&v).unwrap();
        assert_eq!(row.typ, "container");
        assert_eq!(row.action, "start");
        assert_eq!(row.id, "abc");
        assert_eq!(row.name, "web");
        assert_eq!(row.at, 123);

        let v = json::parse(r#"{"Type":"container","Action":"exec_start: sh"}"#).unwrap();
        assert!(event_row(&v).is_none());
    }

    // ---- compute_sample ----

    fn stats(total: u64, pre_total: u64, sys: u64, pre_sys: u64, online: u64) -> StatsResponse {
        StatsResponse {
            cpu_stats: Some(CpuStats {
                total_usage: Some(total),
                system_cpu_usage: Some(sys),
                online_cpus: Some(online),
            }),
            precpu_stats: Some(CpuStats {
                total_usage: Some(pre_total),
                system_cpu_usage: Some(pre_sys),
                online_cpus: None,
            }),
            ..Default::default()
        }
    }

    #[test]
    fn cpu_percent_happy_path() {
        // used half the delta on 2 cpus -> 100%
        let st = stats(1_500_000, 1_000_000, 2_000_000, 1_000_000, 2);
        let s = compute_sample("id", &st).unwrap();
        assert!((s.cpu_pct - 100.0).abs() < 0.001);
        assert_eq!(s.cpu_cores, 2);
    }

    #[test]
    fn network_rates_use_elapsed_time_and_handle_counter_reset() {
        assert_eq!(
            network_rates((1_000, 500), (3_000, 1_500), Duration::from_secs(2)),
            (1_000, 500)
        );
        assert_eq!(
            network_rates((3_000, 1_500), (100, 50), Duration::from_secs(1)),
            (0, 0)
        );
        assert_eq!(network_rates((0, 0), (100, 100), Duration::ZERO), (0, 0));
    }

    #[test]
    fn cpu_zero_when_system_clock_not_advancing() {
        let st = stats(2_000_000, 1_000_000, 1_000_000, 1_000_000, 1);
        let s = compute_sample("id", &st).unwrap();
        assert_eq!(s.cpu_pct, 0.0);
    }

    #[test]
    fn cpu_zero_when_counter_went_backwards() {
        // container restarted: total < pre_total
        let st = stats(100, 1_000_000, 2_000_000, 1_000_000, 1);
        let s = compute_sample("id", &st).unwrap();
        assert_eq!(s.cpu_pct, 0.0);
    }

    #[test]
    fn sample_none_without_cpu_stats() {
        assert!(compute_sample("id", &StatsResponse::default()).is_none());
    }

    #[test]
    fn memory_subtracts_page_cache() {
        let mut st = stats(2, 1, 4, 2, 1);
        st.memory_stats = Some(MemoryStats {
            usage: Some(1000),
            limit: Some(2000),
            cache: Some(300),
        });
        let s = compute_sample("id", &st).unwrap();
        assert_eq!(s.mem_used, 700);
        assert!((s.mem_pct - 35.0).abs() < 0.001);
    }

    #[test]
    fn memory_cache_larger_than_usage_saturates() {
        let mut st = stats(2, 1, 4, 2, 1);
        st.memory_stats = Some(MemoryStats {
            usage: Some(1000),
            limit: Some(2000),
            cache: Some(5000),
        });
        let s = compute_sample("id", &st).unwrap();
        assert_eq!(s.mem_used, 0);
    }

    #[test]
    fn memory_zero_limit_means_zero_percent() {
        let mut st = stats(2, 1, 4, 2, 1);
        st.memory_stats = Some(MemoryStats {
            usage: Some(1000),
            limit: Some(0),
            cache: None,
        });
        let s = compute_sample("id", &st).unwrap();
        assert_eq!(s.mem_pct, 0.0);
    }

    #[test]
    fn network_totals_are_copied_to_sample() {
        let mut st = stats(2, 1, 4, 2, 1);
        st.networks = Some((30, 3));
        st.pids = Some(7);
        let s = compute_sample("id", &st).unwrap();
        assert_eq!(s.rx, 30);
        assert_eq!(s.tx, 3);
        assert_eq!(s.pids, 7);
    }

    #[test]
    fn stats_parsed_from_api_json() {
        let v = json::parse(
            r#"{
                "cpu_stats": {"cpu_usage": {"total_usage": 1500000},
                              "system_cpu_usage": 2000000, "online_cpus": 2},
                "precpu_stats": {"cpu_usage": {"total_usage": 1000000},
                                 "system_cpu_usage": 1000000},
                "memory_stats": {"usage": 1000, "limit": 2000,
                                 "stats": {"inactive_file": 300}},
                "networks": {
                    "eth0": {"rx_bytes": 10, "tx_bytes": 1},
                    "eth1": {"rx_bytes": 20, "tx_bytes": 2}
                },
                "pids_stats": {"current": 7}
            }"#,
        )
        .unwrap();
        let s = compute_sample("id", &stats_from_value(&v)).unwrap();
        assert!((s.cpu_pct - 100.0).abs() < 0.001);
        assert_eq!(s.mem_used, 700);
        assert_eq!(s.rx, 30);
        assert_eq!(s.tx, 3);
        assert_eq!(s.pids, 7);
    }

    // ---- log stream framing ----

    #[test]
    fn multiplexed_detection() {
        let frame = [1u8, 0, 0, 0, 0, 0, 0, 5];
        assert!(is_multiplexed(
            "application/vnd.docker.multiplexed-stream",
            &[b'h'; 8]
        ));
        assert!(!is_multiplexed("application/vnd.docker.raw-stream", &frame));
        assert!(is_multiplexed("", &frame));
        assert!(!is_multiplexed("", b"hello wo"));
    }

    // ---- batch toast summaries ----

    #[test]
    fn batch_toast_success_and_partial_failure() {
        let (tx, rx) = std::sync::mpsc::sync_channel(32);
        batch_toast(&tx, "images", 3, 0, String::new());
        match rx.try_recv().unwrap() {
            AppEvent::Toast(text, error) => {
                assert_eq!(text, "removed 3 images");
                assert!(!error);
            }
            _ => panic!("expected toast"),
        }
        batch_toast(&tx, "volumes", 4, 2, "v1: busy".into());
        match rx.try_recv().unwrap() {
            AppEvent::Toast(text, error) => {
                assert_eq!(text, "removed 2/4 volumes — v1: busy");
                assert!(error);
            }
            _ => panic!("expected toast"),
        }
    }
}
