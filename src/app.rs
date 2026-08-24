use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::mpsc::Sender;
use std::time::Instant;

use crossterm::event::{
    Event, KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use ratatui::layout::{Position, Rect};
use ratatui::widgets::TableState;

use crate::compose::{self, ComposeAction};
use crate::docker::{self, CtrAction, Docker, TaskHandle};

pub const HISTORY_LEN: usize = 120;
pub const MAX_LOG_LINES: usize = 5000;
pub const MAX_EVENTS: usize = 500;
/// A container that dies this many times inside the window is restart-looping.
pub const RESTART_LOOP_THRESHOLD: usize = 3;
pub const RESTART_LOOP_WINDOW_SECS: i64 = 300;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RowState {
    Running,
    Paused,
    Restarting,
    Exited,
    Created,
    Dead,
    Other,
}

#[derive(Debug, Clone)]
pub struct ContainerRow {
    pub id: String,
    pub name: String,
    pub image: String,
    pub image_id: String,
    pub state: RowState,
    pub status: String,
    pub ports: String,
    pub created: i64,
    pub compose_project: Option<String>,
    pub compose_service: Option<String>,
    pub compose_files: String,
    pub compose_dir: String,
    /// Named volumes this container mounts.
    pub volumes: Vec<String>,
    /// Networks this container is attached to.
    pub networks: Vec<String>,
}

/// A compose project derived from `com.docker.compose.*` container labels.
#[derive(Debug, Clone)]
pub struct ComposeRow {
    pub name: String,
    pub config_files: String,
    pub working_dir: String,
    pub running: usize,
    pub total: usize,
}

#[derive(Debug, Clone)]
pub struct ImageRow {
    pub id: String,
    pub tag: String,
    pub size: i64,
    pub created: i64,
    pub containers: i64,
}

#[derive(Debug, Clone)]
pub struct VolumeRow {
    pub name: String,
    pub driver: String,
    pub mountpoint: String,
    pub created: String,
}

#[derive(Debug, Clone)]
pub struct NetworkRow {
    pub id: String,
    pub name: String,
    pub driver: String,
    pub scope: String,
    pub subnet: String,
}

#[derive(Debug, Clone)]
pub struct StatSample {
    pub id: String,
    pub cpu_pct: f64,
    /// CPU capacity reported by Docker. A container on four CPUs can reach
    /// 400%, so the UI must not treat 100% as the universal ceiling.
    pub cpu_cores: u64,
    pub mem_pct: f64,
    pub mem_used: u64,
    pub mem_limit: u64,
    /// Cumulative network counters since the container started.
    pub rx: u64,
    pub tx: u64,
    /// Throughput calculated from consecutive daemon samples.
    pub rx_rate: u64,
    pub tx_rate: u64,
    pub pids: u64,
}

#[derive(Debug, Default)]
pub struct StatsHistory {
    pub cpu: VecDeque<u64>,
    pub mem: VecDeque<u64>,
    pub rx_rate: VecDeque<u64>,
    pub tx_rate: VecDeque<u64>,
    pub last: Option<StatSample>,
}

/// One entry from the Docker daemon events stream.
#[derive(Debug, Clone)]
pub struct EventRow {
    /// Unix seconds.
    pub at: i64,
    /// Object type: container / image / volume / network / …
    pub typ: String,
    /// Action: start / die / oom / health_status: unhealthy / …
    pub action: String,
    pub id: String,
    pub name: String,
}

#[derive(Debug)]
pub enum AppEvent {
    Version(String),
    Containers(Vec<ContainerRow>),
    Images(Vec<ImageRow>),
    Volumes(Vec<VolumeRow>),
    Networks(Vec<NetworkRow>),
    Stat(StatSample),
    Log(String, String),
    Inspect(String, Vec<(String, String)>),
    Toast(String, bool),
    DockerErr(String),
    ComposeAvailable(bool),
    VolumeSizes(HashMap<String, i64>),
    Event(EventRow),
    /// Terminal input, pumped through the same channel as daemon events so
    /// the main loop blocks in one place. Handled in `main`, not `apply`.
    Input(Event),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Panel {
    Containers = 0,
    Compose = 1,
    Images = 2,
    Volumes = 3,
    Networks = 4,
}

impl Panel {
    pub fn next(self) -> Self {
        match self {
            Panel::Containers => Panel::Compose,
            Panel::Compose => Panel::Images,
            Panel::Images => Panel::Volumes,
            Panel::Volumes => Panel::Networks,
            Panel::Networks => Panel::Containers,
        }
    }
    pub fn prev(self) -> Self {
        self.next().next().next().next()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DetailTab {
    Logs = 0,
    Stats = 1,
    Info = 2,
}

/// Which side of the screen keyboard input targets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    Panels,
    Detail,
}

#[derive(Debug, Clone)]
pub enum ConfirmAction {
    RemoveContainer(String, String),
    RemoveImage(String, String),
    RemoveVolume(String),
    RemoveNetwork(String, String),
    PruneContainers,
    PruneImages,
    PruneVolumes,
    ComposeDown(ComposeRow),
    RemoveContainersBatch(Vec<(String, String)>),
    RemoveImagesBatch(Vec<(String, String)>),
    RemoveVolumesBatch(Vec<String>),
    RemoveNetworksBatch(Vec<(String, String)>),
}

impl ConfirmAction {
    /// Mass-destructive actions only accept an explicit `y` — Enter cancels,
    /// so a habitual Enter can't wipe a whole panel.
    pub fn needs_explicit_yes(&self) -> bool {
        matches!(
            self,
            ConfirmAction::RemoveContainersBatch(_)
                | ConfirmAction::RemoveImagesBatch(_)
                | ConfirmAction::RemoveVolumesBatch(_)
                | ConfirmAction::RemoveNetworksBatch(_)
        )
    }

    pub fn describe(&self) -> String {
        match self {
            ConfirmAction::RemoveContainer(_, name) => format!("Force-remove container '{name}'?"),
            ConfirmAction::RemoveImage(_, tag) => format!("Force-remove image '{tag}'?"),
            ConfirmAction::RemoveVolume(name) => format!("Remove volume '{name}'?"),
            ConfirmAction::RemoveNetwork(_, name) => format!("Remove network '{name}'?"),
            ConfirmAction::PruneContainers => "Prune all stopped containers?".into(),
            ConfirmAction::PruneImages => "Prune dangling images?".into(),
            ConfirmAction::PruneVolumes => "Prune unused anonymous volumes?".into(),
            ConfirmAction::ComposeDown(p) => {
                format!("Down project '{}' (removes containers + networks)?", p.name)
            }
            ConfirmAction::RemoveContainersBatch(v) => {
                format!("Force-remove {} containers?", v.len())
            }
            ConfirmAction::RemoveImagesBatch(v) => {
                format!("Force-remove {} images?", v.len())
            }
            ConfirmAction::RemoveVolumesBatch(v) => format!("Remove {} volumes?", v.len()),
            ConfirmAction::RemoveNetworksBatch(v) => format!("Remove {} networks?", v.len()),
        }
    }
}

#[derive(Debug, Clone)]
pub enum Mode {
    Normal,
    Filter,
    Confirm(ConfirmAction),
    /// Kill-signal picker for a running container: (id, name).
    Signal(String, String),
    Help,
    Events,
}

pub struct Toast {
    pub text: String,
    pub error: bool,
    pub at: Instant,
}

/// Screen regions captured during the last draw, used for mouse hit-testing.
#[derive(Debug, Default, Clone, Copy)]
pub struct LayoutMap {
    pub panels: [Rect; 5],
    pub tabs_row: Rect,
    pub detail: Rect,
}

pub const PANEL_ORDER: [Panel; 5] = [
    Panel::Containers,
    Panel::Compose,
    Panel::Images,
    Panel::Volumes,
    Panel::Networks,
];

/// Sort columns per panel, cycled with `,`. First entry is the default.
pub const SORT_COLS: [&[&str]; 5] = [
    &["state", "name", "image", "created", "cpu", "mem"],
    &["name", "running", "total"],
    &["created", "size", "tag", "containers"],
    &["name", "size", "driver", "created"],
    &["name", "driver", "scope"],
];

/// Numeric/time columns read best largest-or-newest-first.
fn default_desc(col: &str) -> bool {
    matches!(col, "created" | "size" | "cpu" | "mem" | "running" | "total" | "containers")
}

/// Running things first, then roughly by how dead they are.
fn state_rank(s: RowState) -> u8 {
    match s {
        RowState::Running => 0,
        RowState::Paused => 1,
        RowState::Restarting => 2,
        RowState::Created => 3,
        RowState::Exited => 4,
        RowState::Dead => 5,
        RowState::Other => 6,
    }
}

pub struct App {
    pub docker: Docker,
    pub tx: Sender<AppEvent>,

    pub version: String,
    pub containers: Vec<ContainerRow>,
    pub compose: Vec<ComposeRow>,
    pub compose_ok: bool,
    pub images: Vec<ImageRow>,
    pub volumes: Vec<VolumeRow>,
    pub volume_sizes: HashMap<String, i64>,
    pub networks: Vec<NetworkRow>,
    pub stats: HashMap<String, StatsHistory>,

    /// Rolling buffer of daemon events, oldest first.
    pub events: VecDeque<EventRow>,
    /// Scroll offset from the bottom of the events overlay (0 = newest).
    pub events_scroll: usize,
    /// Containers the daemon reported an `oom` event for; cleared on `start`.
    pub oom_ids: HashSet<String>,

    pub logs: Vec<String>,
    pub logs_id: Option<String>,
    pub logs_members: Vec<String>,
    pub logs_handles: Vec<TaskHandle>,
    /// Top rendered row in the log viewport. A single Docker log entry can
    /// occupy multiple rows when wrapping is enabled.
    pub log_scroll: usize,
    pub log_visual_rows: usize,
    pub log_viewport_rows: usize,
    pub follow: bool,
    pub wrap_logs: bool,

    pub inspect: Vec<(String, String)>,
    pub inspect_id: Option<String>,

    pub panel: Panel,
    pub focus: Focus,
    /// Zoom the focused pane (panel or detail) to the full body area.
    pub zoom: bool,
    /// Reveal env var values in the Info tab (masked by default).
    pub show_env: bool,
    pub sel: [usize; 5],
    /// Sort column per panel, indexing into `SORT_COLS`.
    pub sort: [usize; 5],
    pub sort_desc: [bool; 5],
    /// Multi-select marks per panel, keyed by row identity
    /// (container/image/network id, volume name; unused for compose).
    pub marked: [HashSet<String>; 5],
    pub table_states: [TableState; 5],
    pub layout: LayoutMap,
    pub detail: DetailTab,
    pub filter: String,
    pub mode: Mode,
    pub toast: Option<Toast>,
    pub docker_err: Option<String>,
    pub should_quit: bool,
    pub pending_exec: Option<String>,
}

impl App {
    pub fn new(docker: Docker, tx: Sender<AppEvent>) -> Self {
        Self {
            docker,
            tx,
            version: String::new(),
            containers: Vec::new(),
            compose: Vec::new(),
            compose_ok: true,
            images: Vec::new(),
            volumes: Vec::new(),
            volume_sizes: HashMap::new(),
            networks: Vec::new(),
            stats: HashMap::new(),
            events: VecDeque::new(),
            events_scroll: 0,
            oom_ids: HashSet::new(),
            logs: Vec::new(),
            logs_id: None,
            logs_members: Vec::new(),
            logs_handles: Vec::new(),
            log_scroll: 0,
            log_visual_rows: 0,
            log_viewport_rows: 0,
            follow: true,
            wrap_logs: true,
            inspect: Vec::new(),
            inspect_id: None,
            panel: Panel::Containers,
            focus: Focus::Panels,
            zoom: false,
            show_env: false,
            sel: [0; 5],
            sort: [0; 5],
            sort_desc: std::array::from_fn(|i| default_desc(SORT_COLS[i][0])),
            marked: Default::default(),
            table_states: Default::default(),
            layout: LayoutMap::default(),
            detail: DetailTab::Logs,
            filter: String::new(),
            mode: Mode::Normal,
            toast: None,
            docker_err: None,
            should_quit: false,
            pending_exec: None,
        }
    }

    #[cfg(test)]
    fn matches(&self, hay: &str) -> bool {
        let needle = self.filter.to_lowercase();
        Self::matches_normalized(hay, &needle)
    }

    fn matches_normalized(hay: &str, needle: &str) -> bool {
        if needle.is_empty() {
            return true;
        }
        let hay = hay.to_lowercase();
        if hay.contains(needle) {
            return true;
        }
        // subsequence match
        let mut chars = needle.chars();
        let mut cur = chars.next();
        for c in hay.chars() {
            if Some(c) == cur {
                cur = chars.next();
                if cur.is_none() {
                    return true;
                }
            }
        }
        false
    }

    /// Active sort column name and direction for a panel.
    fn sort_key(&self, panel: Panel) -> (&'static str, bool) {
        let i = panel as usize;
        (SORT_COLS[i][self.sort[i]], self.sort_desc[i])
    }

    /// `↓name`-style tag for panel titles.
    pub fn sort_indicator(&self, panel: Panel) -> String {
        let (col, desc) = self.sort_key(panel);
        format!("{}{col}", if desc { '↓' } else { '↑' })
    }

    /// Last live stat for a container; -1 sorts stopped containers below 0%.
    fn last_stat(&self, c: &ContainerRow, f: impl Fn(&StatSample) -> f64) -> f64 {
        self.stats.get(&c.id).and_then(|h| h.last.as_ref()).map(f).unwrap_or(-1.0)
    }

    pub fn filtered_containers(&self) -> Vec<&ContainerRow> {
        let needle = self.filter.to_lowercase();
        let mut v: Vec<&ContainerRow> = self
            .containers
            .iter()
            .filter(|c| {
                if needle.is_empty() {
                    return true;
                }
                let mut hay = String::with_capacity(c.name.len() + c.image.len() + 1);
                hay.push_str(&c.name);
                hay.push(' ');
                hay.push_str(&c.image);
                Self::matches_normalized(&hay, &needle)
            })
            .collect();
        let (col, desc) = self.sort_key(Panel::Containers);
        v.sort_by(|a, b| {
            let ord = match col {
                "name" => a.name.cmp(&b.name),
                "image" => a.image.cmp(&b.image),
                "created" => a.created.cmp(&b.created),
                "cpu" => self
                    .last_stat(a, |s| s.cpu_pct)
                    .total_cmp(&self.last_stat(b, |s| s.cpu_pct)),
                "mem" => self
                    .last_stat(a, |s| s.mem_pct)
                    .total_cmp(&self.last_stat(b, |s| s.mem_pct)),
                _ => state_rank(a.state).cmp(&state_rank(b.state)),
            };
            // tie-break stays ascending so `.` only flips the primary key
            let ord = if desc { ord.reverse() } else { ord };
            ord.then_with(|| a.name.cmp(&b.name))
        });
        v
    }
    pub fn filtered_compose(&self) -> Vec<&ComposeRow> {
        let needle = self.filter.to_lowercase();
        let mut v: Vec<&ComposeRow> = self
            .compose
            .iter()
            .filter(|p| Self::matches_normalized(&p.name, &needle))
            .collect();
        let (col, desc) = self.sort_key(Panel::Compose);
        v.sort_by(|a, b| {
            let ord = match col {
                "running" => a.running.cmp(&b.running),
                "total" => a.total.cmp(&b.total),
                _ => a.name.cmp(&b.name),
            };
            let ord = if desc { ord.reverse() } else { ord };
            ord.then_with(|| a.name.cmp(&b.name))
        });
        v
    }
    pub fn filtered_images(&self) -> Vec<&ImageRow> {
        let needle = self.filter.to_lowercase();
        let mut v: Vec<&ImageRow> = self
            .images
            .iter()
            .filter(|i| Self::matches_normalized(&i.tag, &needle))
            .collect();
        let (col, desc) = self.sort_key(Panel::Images);
        v.sort_by(|a, b| {
            let ord = match col {
                "size" => a.size.cmp(&b.size),
                "tag" => a.tag.cmp(&b.tag),
                "containers" => a.containers.cmp(&b.containers),
                _ => a.created.cmp(&b.created),
            };
            let ord = if desc { ord.reverse() } else { ord };
            ord.then_with(|| a.tag.cmp(&b.tag))
        });
        v
    }
    pub fn filtered_volumes(&self) -> Vec<&VolumeRow> {
        let needle = self.filter.to_lowercase();
        let mut v: Vec<&VolumeRow> = self
            .volumes
            .iter()
            .filter(|v| Self::matches_normalized(&v.name, &needle))
            .collect();
        let (col, desc) = self.sort_key(Panel::Volumes);
        let size = |v: &VolumeRow| self.volume_sizes.get(&v.name).copied().unwrap_or(-1);
        v.sort_by(|a, b| {
            let ord = match col {
                "size" => size(a).cmp(&size(b)),
                "driver" => a.driver.cmp(&b.driver),
                "created" => a.created.cmp(&b.created),
                _ => a.name.cmp(&b.name),
            };
            let ord = if desc { ord.reverse() } else { ord };
            ord.then_with(|| a.name.cmp(&b.name))
        });
        v
    }
    pub fn filtered_networks(&self) -> Vec<&NetworkRow> {
        let needle = self.filter.to_lowercase();
        let mut v: Vec<&NetworkRow> = self
            .networks
            .iter()
            .filter(|n| Self::matches_normalized(&n.name, &needle))
            .collect();
        let (col, desc) = self.sort_key(Panel::Networks);
        v.sort_by(|a, b| {
            let ord = match col {
                "driver" => a.driver.cmp(&b.driver),
                "scope" => a.scope.cmp(&b.scope),
                _ => a.name.cmp(&b.name),
            };
            let ord = if desc { ord.reverse() } else { ord };
            ord.then_with(|| a.name.cmp(&b.name))
        });
        v
    }

    fn panel_len(&self) -> usize {
        match self.panel {
            Panel::Containers => self.filtered_containers().len(),
            Panel::Compose => self.filtered_compose().len(),
            Panel::Images => self.filtered_images().len(),
            Panel::Volumes => self.filtered_volumes().len(),
            Panel::Networks => self.filtered_networks().len(),
        }
    }

    pub fn selected_container(&self) -> Option<&ContainerRow> {
        let list = self.filtered_containers();
        list.get(self.sel[Panel::Containers as usize]).copied()
    }
    pub fn selected_compose(&self) -> Option<&ComposeRow> {
        let list = self.filtered_compose();
        list.get(self.sel[Panel::Compose as usize]).copied()
    }
    pub fn selected_image(&self) -> Option<&ImageRow> {
        let list = self.filtered_images();
        list.get(self.sel[Panel::Images as usize]).copied()
    }
    pub fn selected_volume(&self) -> Option<&VolumeRow> {
        let list = self.filtered_volumes();
        list.get(self.sel[Panel::Volumes as usize]).copied()
    }
    pub fn selected_network(&self) -> Option<&NetworkRow> {
        let list = self.filtered_networks();
        list.get(self.sel[Panel::Networks as usize]).copied()
    }

    /// Containers of a compose project, as (id, service name) pairs.
    pub fn compose_members(&self, project: &str) -> Vec<(String, String)> {
        self.containers
            .iter()
            .filter(|c| c.compose_project.as_deref() == Some(project))
            .map(|c| {
                let service = c.compose_service.clone().unwrap_or_else(|| c.name.clone());
                (c.id.clone(), service)
            })
            .collect()
    }

    /// Rebuild compose project rows from container labels.
    fn rebuild_compose(&mut self) {
        let mut map: std::collections::BTreeMap<String, ComposeRow> =
            std::collections::BTreeMap::new();
        for c in &self.containers {
            let Some(project) = &c.compose_project else { continue };
            let row = map.entry(project.clone()).or_insert_with(|| ComposeRow {
                name: project.clone(),
                config_files: String::new(),
                working_dir: String::new(),
                running: 0,
                total: 0,
            });
            row.total += 1;
            if matches!(c.state, RowState::Running | RowState::Paused) {
                row.running += 1;
            }
            if row.config_files.is_empty() && !c.compose_files.is_empty() {
                row.config_files = c.compose_files.clone();
            }
            if row.working_dir.is_empty() && !c.compose_dir.is_empty() {
                row.working_dir = c.compose_dir.clone();
            }
        }
        self.compose = map.into_values().collect();
    }

    fn clamp_selections(&mut self) {
        let lens = [
            self.filtered_containers().len(),
            self.filtered_compose().len(),
            self.filtered_images().len(),
            self.filtered_volumes().len(),
            self.filtered_networks().len(),
        ];
        for (s, len) in self.sel.iter_mut().zip(lens) {
            if len == 0 {
                *s = 0;
            } else if *s >= len {
                *s = len - 1;
            }
        }
    }

    pub fn apply(&mut self, ev: AppEvent) {
        match ev {
            AppEvent::Version(v) => self.version = v,
            AppEvent::Containers(rows) => {
                self.docker_err = None;
                // drop stats history for containers that no longer exist
                let ids: std::collections::HashSet<&str> =
                    rows.iter().map(|r| r.id.as_str()).collect();
                self.stats.retain(|id, _| ids.contains(id.as_str()));
                self.oom_ids.retain(|id| ids.contains(id.as_str()));
                self.marked[Panel::Containers as usize].retain(|id| ids.contains(id.as_str()));
                self.containers = rows;
                self.rebuild_compose();
                self.clamp_selections();
                self.sync_selection();
            }
            AppEvent::Images(rows) => {
                let ids: HashSet<&str> = rows.iter().map(|r| r.id.as_str()).collect();
                self.marked[Panel::Images as usize].retain(|id| ids.contains(id.as_str()));
                self.images = rows;
                self.clamp_selections();
            }
            AppEvent::Volumes(rows) => {
                let names: HashSet<&str> = rows.iter().map(|r| r.name.as_str()).collect();
                self.marked[Panel::Volumes as usize].retain(|n| names.contains(n.as_str()));
                self.volumes = rows;
                self.clamp_selections();
            }
            AppEvent::Networks(rows) => {
                let ids: HashSet<&str> = rows.iter().map(|r| r.id.as_str()).collect();
                self.marked[Panel::Networks as usize].retain(|id| ids.contains(id.as_str()));
                self.networks = rows;
                self.clamp_selections();
            }
            AppEvent::Stat(s) => {
                let h = self.stats.entry(s.id.clone()).or_default();
                h.cpu.push_back(s.cpu_pct.round().max(0.0) as u64);
                h.mem.push_back(s.mem_pct.round().max(0.0) as u64);
                h.rx_rate.push_back(s.rx_rate);
                h.tx_rate.push_back(s.tx_rate);
                for values in [&mut h.cpu, &mut h.mem, &mut h.rx_rate, &mut h.tx_rate] {
                    while values.len() > HISTORY_LEN {
                        values.pop_front();
                    }
                }
                h.last = Some(s);
            }
            AppEvent::Log(id, chunk) => {
                if self.logs_id.as_deref() == Some(id.as_str()) {
                    for line in chunk.split('\n') {
                        let line = line.trim_end_matches('\r');
                        if !line.is_empty() {
                            self.logs.push(line.to_string());
                        }
                    }
                    if self.logs.len() > MAX_LOG_LINES {
                        let excess = self.logs.len() - MAX_LOG_LINES;
                        self.logs.drain(..excess);
                        self.log_scroll = self.log_scroll.saturating_sub(excess);
                    }
                }
            }
            AppEvent::Inspect(id, kv) => {
                if self.inspect_id.as_deref() == Some(id.as_str()) {
                    self.inspect = kv;
                }
            }
            AppEvent::Toast(text, error) => {
                self.toast = Some(Toast { text, error, at: Instant::now() });
            }
            AppEvent::DockerErr(e) => self.docker_err = Some(e),
            AppEvent::ComposeAvailable(ok) => self.compose_ok = ok,
            AppEvent::VolumeSizes(sizes) => self.volume_sizes = sizes,
            AppEvent::Event(ev) => {
                if ev.typ == "container" {
                    match ev.action.as_str() {
                        "oom" => {
                            self.oom_ids.insert(ev.id.clone());
                        }
                        "start" => {
                            self.oom_ids.remove(&ev.id);
                        }
                        _ => {}
                    }
                }
                self.events.push_back(ev);
                while self.events.len() > MAX_EVENTS {
                    self.events.pop_front();
                }
            }
            // input is routed in main, never applied here
            AppEvent::Input(_) => {}
        }
    }

    /// `die` events for this container inside the restart-loop window.
    pub fn recent_die_count(&self, id: &str, now: i64) -> usize {
        self.events
            .iter()
            .filter(|e| {
                e.typ == "container"
                    && e.action == "die"
                    && e.id == id
                    && now - e.at <= RESTART_LOOP_WINDOW_SECS
            })
            .count()
    }

    pub fn restart_looping(&self, id: &str, now: i64) -> bool {
        self.recent_die_count(id, now) >= RESTART_LOOP_THRESHOLD
    }

    fn scroll_events(&mut self, delta: i64) {
        let max = self.events.len().saturating_sub(1);
        let cur = self.events_scroll as i64 + delta;
        self.events_scroll = cur.clamp(0, max as i64) as usize;
    }

    /// Restart log/inspect streams when the selected container or compose
    /// project changes (or a project's member set changes, e.g. after `up`).
    pub fn sync_selection(&mut self) {
        let (target, members) = if self.panel == Panel::Compose {
            match self.selected_compose().map(|p| p.name.clone()) {
                Some(name) => (Some(compose::log_key(&name)), self.compose_members(&name)),
                None => (None, Vec::new()),
            }
        } else {
            (self.selected_container().map(|c| c.id.clone()), Vec::new())
        };
        let member_ids: Vec<String> = members.iter().map(|(id, _)| id.clone()).collect();
        if target == self.logs_id && member_ids == self.logs_members {
            return;
        }
        for h in self.logs_handles.drain(..) {
            h.abort();
        }
        self.logs.clear();
        self.log_scroll = 0;
        self.log_visual_rows = 0;
        self.log_viewport_rows = 0;
        self.follow = true;
        self.inspect.clear();
        self.logs_id = target.clone();
        self.logs_members = member_ids;
        if self.panel == Panel::Compose {
            self.inspect_id = None;
            if let Some(key) = target {
                self.logs_handles =
                    docker::spawn_compose_logs(&self.docker, &self.tx, key, members);
            }
        } else {
            self.inspect_id = target.clone();
            if let Some(id) = target {
                self.logs_handles.push(docker::spawn_logs(&self.docker, &self.tx, id.clone()));
                docker::spawn_inspect(&self.docker, &self.tx, id);
            }
        }
    }

    fn move_sel(&mut self, delta: i64) {
        let len = self.panel_len();
        if len == 0 {
            return;
        }
        let i = self.sel[self.panel as usize] as i64 + delta;
        self.sel[self.panel as usize] = i.clamp(0, len as i64 - 1) as usize;
        if matches!(self.panel, Panel::Containers | Panel::Compose) {
            self.sync_selection();
        }
    }

    /// g/G with the panel list focused: jump to the first or last row.
    fn jump_sel(&mut self, top: bool) {
        let len = self.panel_len();
        if len == 0 {
            return;
        }
        self.sel[self.panel as usize] = if top { 0 } else { len - 1 };
        if matches!(self.panel, Panel::Containers | Panel::Compose) {
            self.sync_selection();
        }
    }

    /// y/Y: copy the selected row's human handle (name/tag) or id to the
    /// system clipboard via OSC 52, so it works over ssh without any
    /// clipboard tool on either end.
    fn yank(&mut self, id_form: bool) {
        let text = match self.panel {
            Panel::Containers => self
                .selected_container()
                .map(|c| if id_form { c.id.clone() } else { c.name.clone() }),
            Panel::Compose => self.selected_compose().map(|p| p.name.clone()),
            Panel::Images => self
                .selected_image()
                .map(|i| if id_form { i.id.clone() } else { i.tag.clone() }),
            Panel::Volumes => self.selected_volume().map(|v| v.name.clone()),
            Panel::Networks => self
                .selected_network()
                .map(|n| if id_form { n.id.clone() } else { n.name.clone() }),
        };
        let Some(text) = text else { return };
        osc52_copy(&text);
        let short: String = text.chars().take(40).collect();
        let ellipsis = if short.len() < text.len() { "…" } else { "" };
        self.apply(AppEvent::Toast(format!("yanked {short}{ellipsis}"), false));
    }

    /// o: open the selected container's first published port in the browser.
    fn open_port(&mut self) {
        if self.panel != Panel::Containers {
            return;
        }
        let Some(ports) = self.selected_container().map(|c| c.ports.clone()) else { return };
        let Some(port) = first_public_port(&ports) else {
            self.apply(AppEvent::Toast("no published port".into(), true));
            return;
        };
        let url = format!("http://localhost:{port}");
        open_in_browser(&url);
        self.apply(AppEvent::Toast(format!("opening {url}"), false));
    }

    /// j/k movement: past either end hops to the neighbouring non-empty panel.
    fn nav(&mut self, delta: i64) {
        let len = self.panel_len();
        let i = self.sel[self.panel as usize] as i64 + delta;
        if len > 0 && i >= 0 && i < len as i64 {
            self.move_sel(delta);
            return;
        }
        let forward = delta > 0;
        let mut p = self.panel;
        for _ in 0..PANEL_ORDER.len() - 1 {
            p = if forward { p.next() } else { p.prev() };
            let plen = self.panel_len_at(p as usize);
            if plen > 0 {
                self.panel = p;
                self.sel[p as usize] = if forward { 0 } else { plen - 1 };
                self.sync_selection();
                return;
            }
        }
    }

    fn confirm(&mut self, action: ConfirmAction) {
        self.mode = Mode::Confirm(action);
    }

    /// `,`: next sort column, direction reset to that column's default.
    fn cycle_sort(&mut self) {
        let i = self.panel as usize;
        self.sort[i] = (self.sort[i] + 1) % SORT_COLS[i].len();
        self.sort_desc[i] = default_desc(SORT_COLS[i][self.sort[i]]);
        self.after_sort_change();
    }

    /// `.`: flip sort direction.
    fn reverse_sort(&mut self) {
        let i = self.panel as usize;
        self.sort_desc[i] = !self.sort_desc[i];
        self.after_sort_change();
    }

    /// Re-sorting moves a different row under the cursor — resync streams.
    fn after_sort_change(&mut self) {
        if matches!(self.panel, Panel::Containers | Panel::Compose) {
            self.sync_selection();
        }
    }

    /// Row identity of the current selection, as used by the mark sets.
    fn selected_key(&self) -> Option<String> {
        match self.panel {
            Panel::Containers => self.selected_container().map(|c| c.id.clone()),
            Panel::Images => self.selected_image().map(|i| i.id.clone()),
            Panel::Volumes => self.selected_volume().map(|v| v.name.clone()),
            Panel::Networks => self.selected_network().map(|n| n.id.clone()),
            Panel::Compose => None,
        }
    }

    fn toggle_mark(&mut self) {
        let Some(key) = self.selected_key() else { return };
        let set = &mut self.marked[self.panel as usize];
        if !set.remove(&key) {
            set.insert(key);
        }
        self.move_sel(1);
    }

    fn toggle_mark_all(&mut self) {
        let keys: Vec<String> = match self.panel {
            Panel::Containers => {
                self.filtered_containers().iter().map(|c| c.id.clone()).collect()
            }
            Panel::Images => self.filtered_images().iter().map(|i| i.id.clone()).collect(),
            Panel::Volumes => self.filtered_volumes().iter().map(|v| v.name.clone()).collect(),
            Panel::Networks => self.filtered_networks().iter().map(|n| n.id.clone()).collect(),
            Panel::Compose => Vec::new(),
        };
        if keys.is_empty() {
            return;
        }
        let set = &mut self.marked[self.panel as usize];
        if keys.iter().all(|k| set.contains(k)) {
            for k in &keys {
                set.remove(k);
            }
        } else {
            set.extend(keys);
        }
    }

    fn run_confirmed(&mut self, action: ConfirmAction) {
        match action {
            ConfirmAction::RemoveContainer(id, name) => {
                docker::container_action(&self.docker, &self.tx, CtrAction::Remove, id, name)
            }
            ConfirmAction::RemoveImage(id, tag) => docker::remove_image(&self.docker, &self.tx, id, tag),
            ConfirmAction::RemoveVolume(name) => docker::remove_volume(&self.docker, &self.tx, name),
            ConfirmAction::RemoveNetwork(id, name) => {
                docker::remove_network(&self.docker, &self.tx, id, name)
            }
            ConfirmAction::PruneContainers => docker::prune_containers(&self.docker, &self.tx),
            ConfirmAction::PruneImages => docker::prune_images(&self.docker, &self.tx),
            ConfirmAction::PruneVolumes => docker::prune_volumes(&self.docker, &self.tx),
            ConfirmAction::ComposeDown(p) => {
                compose::compose_action(&self.tx, ComposeAction::Down, p)
            }
            ConfirmAction::RemoveContainersBatch(items) => {
                self.marked[Panel::Containers as usize].clear();
                docker::remove_containers_batch(&self.docker, &self.tx, items);
            }
            ConfirmAction::RemoveImagesBatch(items) => {
                self.marked[Panel::Images as usize].clear();
                docker::remove_images_batch(&self.docker, &self.tx, items);
            }
            ConfirmAction::RemoveVolumesBatch(items) => {
                self.marked[Panel::Volumes as usize].clear();
                docker::remove_volumes_batch(&self.docker, &self.tx, items);
            }
            ConfirmAction::RemoveNetworksBatch(items) => {
                self.marked[Panel::Networks as usize].clear();
                docker::remove_networks_batch(&self.docker, &self.tx, items);
            }
        }
    }

    fn panel_len_at(&self, i: usize) -> usize {
        match PANEL_ORDER[i] {
            Panel::Containers => self.filtered_containers().len(),
            Panel::Compose => self.filtered_compose().len(),
            Panel::Images => self.filtered_images().len(),
            Panel::Volumes => self.filtered_volumes().len(),
            Panel::Networks => self.filtered_networks().len(),
        }
    }

    pub fn on_mouse(&mut self, ev: MouseEvent) {
        match &self.mode {
            Mode::Help => {
                if matches!(ev.kind, MouseEventKind::Down(_)) {
                    self.mode = Mode::Normal;
                }
                return;
            }
            Mode::Events => {
                match ev.kind {
                    MouseEventKind::ScrollUp => self.scroll_events(3),
                    MouseEventKind::ScrollDown => self.scroll_events(-3),
                    MouseEventKind::Down(_) => self.mode = Mode::Normal,
                    _ => {}
                }
                return;
            }
            Mode::Confirm(_) | Mode::Signal(..) => return,
            _ => {}
        }
        let pos = Position { x: ev.column, y: ev.row };
        match ev.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                for i in 0..PANEL_ORDER.len() {
                    let r = self.layout.panels[i];
                    if !r.contains(pos) {
                        continue;
                    }
                    self.panel = PANEL_ORDER[i];
                    self.focus = Focus::Panels;
                    // rows start after top border (1) + table header (1)
                    let data_top = r.y + 2;
                    if pos.y >= data_top && pos.y + 1 < r.y + r.height {
                        let idx =
                            self.table_states[i].offset() + (pos.y - data_top) as usize;
                        if idx < self.panel_len_at(i) {
                            self.sel[i] = idx;
                        }
                    }
                    self.sync_selection();
                    return;
                }
                if self.panel == Panel::Containers && self.layout.tabs_row.contains(pos) {
                    if let Some(tab) = tab_at(pos.x - self.layout.tabs_row.x) {
                        self.detail = tab;
                    }
                }
                if self.layout.detail.contains(pos) {
                    self.focus = Focus::Detail;
                }
            }
            MouseEventKind::ScrollUp | MouseEventKind::ScrollDown => {
                let delta: i64 = if ev.kind == MouseEventKind::ScrollUp { -1 } else { 1 };
                for i in 0..PANEL_ORDER.len() {
                    if self.layout.panels[i].contains(pos) {
                        self.panel = PANEL_ORDER[i];
                        self.move_sel(delta);
                        return;
                    }
                }
                if self.layout.detail.contains(pos)
                    && (self.panel == Panel::Compose
                        || (self.panel == Panel::Containers && self.detail == DetailTab::Logs))
                {
                    self.scroll_logs(delta * 3);
                }
            }
            _ => {}
        }
    }

    fn scroll_logs(&mut self, delta: i64) {
        self.follow = false;
        // Before the first draw there is no viewport geometry yet; retaining
        // the raw-line fallback also keeps keyboard navigation responsive in
        // very small terminals where the log viewport disappears.
        let max = if self.log_viewport_rows == 0 {
            self.logs.len().saturating_sub(1)
        } else {
            self.log_visual_rows.saturating_sub(self.log_viewport_rows)
        };
        let cur = self.log_scroll as i64 + delta;
        self.log_scroll = cur.clamp(0, max as i64) as usize;
    }

    pub fn on_key(&mut self, key: KeyEvent) {
        match self.mode.clone() {
            Mode::Help => {
                self.mode = Mode::Normal;
            }
            Mode::Events => match key.code {
                // offset counts back from the newest entry, so k = older
                KeyCode::Char('k') | KeyCode::Up => self.scroll_events(1),
                KeyCode::Char('j') | KeyCode::Down => self.scroll_events(-1),
                KeyCode::PageUp => self.scroll_events(10),
                KeyCode::PageDown => self.scroll_events(-10),
                KeyCode::Char('g') => {
                    self.events_scroll = self.events.len().saturating_sub(1)
                }
                KeyCode::Char('G') => self.events_scroll = 0,
                _ => self.mode = Mode::Normal,
            },
            Mode::Confirm(action) => {
                let yes = match key.code {
                    KeyCode::Char('y') => true,
                    KeyCode::Enter => !action.needs_explicit_yes(),
                    _ => false,
                };
                self.mode = Mode::Normal;
                if yes {
                    self.run_confirmed(action);
                }
            }
            Mode::Signal(id, name) => {
                let signal = match key.code {
                    KeyCode::Char('t') => Some("SIGTERM"),
                    KeyCode::Char('k') => Some("SIGKILL"),
                    KeyCode::Char('h') => Some("SIGHUP"),
                    _ => None,
                };
                self.mode = Mode::Normal;
                if let Some(signal) = signal {
                    docker::kill_container(&self.docker, &self.tx, id, name, signal);
                }
            }
            Mode::Filter => match key.code {
                KeyCode::Esc => {
                    self.filter.clear();
                    self.mode = Mode::Normal;
                    self.clamp_selections();
                    self.sync_selection();
                }
                KeyCode::Enter => self.mode = Mode::Normal,
                KeyCode::Backspace => {
                    self.filter.pop();
                    self.clamp_selections();
                    self.sync_selection();
                }
                KeyCode::Char(c) => {
                    self.filter.push(c);
                    self.clamp_selections();
                    self.sync_selection();
                }
                _ => {}
            },
            Mode::Normal => self.on_key_normal(key),
        }
    }

    fn on_key_normal(&mut self, key: KeyEvent) {
        if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
            self.should_quit = true;
            return;
        }
        match key.code {
            KeyCode::Char('q') => self.should_quit = true,
            KeyCode::Char('?') => self.mode = Mode::Help,
            KeyCode::Char('/') => self.mode = Mode::Filter,
            KeyCode::Char('E') => {
                self.events_scroll = 0;
                self.mode = Mode::Events;
            }
            KeyCode::Esc => {
                if self.focus == Focus::Detail {
                    self.focus = Focus::Panels;
                } else if !self.marked[self.panel as usize].is_empty() {
                    self.marked[self.panel as usize].clear();
                } else if !self.filter.is_empty() {
                    self.filter.clear();
                    self.clamp_selections();
                    self.sync_selection();
                }
            }
            KeyCode::Enter | KeyCode::Char('l') => self.focus = Focus::Detail,
            KeyCode::Char('h') => self.focus = Focus::Panels,
            KeyCode::Char('z') => self.zoom = !self.zoom,
            KeyCode::Char('x') => self.show_env = !self.show_env,
            KeyCode::Char(' ') => self.toggle_mark(),
            KeyCode::Char('A') => self.toggle_mark_all(),
            KeyCode::Char(',') => self.cycle_sort(),
            KeyCode::Char('.') => self.reverse_sort(),
            KeyCode::Tab => {
                self.panel = self.panel.next();
                self.focus = Focus::Panels;
                self.sync_selection();
            }
            KeyCode::BackTab => {
                self.panel = self.panel.prev();
                self.focus = Focus::Panels;
                self.sync_selection();
            }
            KeyCode::Char('1') => {
                self.panel = Panel::Containers;
                self.focus = Focus::Panels;
                self.sync_selection();
            }
            KeyCode::Char('2') => {
                self.panel = Panel::Compose;
                self.focus = Focus::Panels;
                self.sync_selection();
            }
            KeyCode::Char('3') => {
                self.panel = Panel::Images;
                self.focus = Focus::Panels;
                self.sync_selection();
            }
            KeyCode::Char('4') => {
                self.panel = Panel::Volumes;
                self.focus = Focus::Panels;
                self.sync_selection();
            }
            KeyCode::Char('5') => {
                self.panel = Panel::Networks;
                self.focus = Focus::Panels;
                self.sync_selection();
            }
            KeyCode::Char('j') | KeyCode::Down => {
                if self.focus == Focus::Detail {
                    self.scroll_logs(1);
                } else {
                    self.nav(1);
                }
            }
            KeyCode::Char('k') | KeyCode::Up => {
                if self.focus == Focus::Detail {
                    self.scroll_logs(-1);
                } else {
                    self.nav(-1);
                }
            }
            KeyCode::Char('[') | KeyCode::Left => {
                self.detail = match self.detail {
                    DetailTab::Logs => DetailTab::Info,
                    DetailTab::Stats => DetailTab::Logs,
                    DetailTab::Info => DetailTab::Stats,
                }
            }
            KeyCode::Char(']') | KeyCode::Right => {
                self.detail = match self.detail {
                    DetailTab::Logs => DetailTab::Stats,
                    DetailTab::Stats => DetailTab::Info,
                    DetailTab::Info => DetailTab::Logs,
                }
            }
            KeyCode::PageUp => self.scroll_logs(-(10)),
            KeyCode::PageDown => self.scroll_logs(10),
            KeyCode::Char('g') => {
                if self.focus == Focus::Detail {
                    self.follow = false;
                    self.log_scroll = 0;
                } else {
                    self.jump_sel(true);
                }
            }
            KeyCode::Char('G') => {
                if self.focus == Focus::Detail {
                    self.follow = true;
                } else {
                    self.jump_sel(false);
                }
            }
            KeyCode::Char('f') => {
                self.follow = true;
            }
            KeyCode::Char('w') => self.wrap_logs = !self.wrap_logs,
            KeyCode::Char('y') => self.yank(false),
            KeyCode::Char('Y') => self.yank(true),
            KeyCode::Char('o') => self.open_port(),
            _ => self.on_panel_key(key),
        }
    }

    fn on_panel_key(&mut self, key: KeyEvent) {
        match self.panel {
            Panel::Containers => {
                if key.code == KeyCode::Char('D') {
                    let items: Vec<(String, String)> = self
                        .filtered_containers()
                        .iter()
                        .map(|x| (x.id.clone(), x.name.clone()))
                        .collect();
                    if items.is_empty() {
                        self.apply(AppEvent::Toast("no containers to remove".into(), true));
                    } else {
                        self.confirm(ConfirmAction::RemoveContainersBatch(items));
                    }
                    return;
                }
                let Some(c) = self.selected_container().cloned() else { return };
                let (docker, tx) = (&self.docker, &self.tx);
                match key.code {
                    KeyCode::Char('s') => {
                        docker::container_action(docker, tx, CtrAction::Stop, c.id, c.name)
                    }
                    KeyCode::Char('S') => {
                        docker::container_action(docker, tx, CtrAction::Start, c.id, c.name)
                    }
                    KeyCode::Char('r') => {
                        docker::container_action(docker, tx, CtrAction::Restart, c.id, c.name)
                    }
                    KeyCode::Char('p') => {
                        let action = if c.state == RowState::Paused {
                            CtrAction::Unpause
                        } else {
                            CtrAction::Pause
                        };
                        docker::container_action(docker, tx, action, c.id, c.name)
                    }
                    KeyCode::Char('d') => {
                        let marked = &self.marked[Panel::Containers as usize];
                        if marked.is_empty() {
                            self.confirm(ConfirmAction::RemoveContainer(c.id, c.name));
                        } else {
                            let items: Vec<(String, String)> = self
                                .containers
                                .iter()
                                .filter(|x| marked.contains(&x.id))
                                .map(|x| (x.id.clone(), x.name.clone()))
                                .collect();
                            self.confirm(ConfirmAction::RemoveContainersBatch(items));
                        }
                    }
                    KeyCode::Char('C') => self.confirm(ConfirmAction::PruneContainers),
                    KeyCode::Char('K') => {
                        if c.state == RowState::Running {
                            self.mode = Mode::Signal(c.id, c.name);
                        } else {
                            self.apply(AppEvent::Toast("container not running".into(), true));
                        }
                    }
                    KeyCode::Char('e') => {
                        if c.state == RowState::Running {
                            self.pending_exec = Some(c.id);
                        } else {
                            self.apply(AppEvent::Toast("container not running".into(), true));
                        }
                    }
                    _ => {}
                }
            }
            Panel::Compose => {
                let Some(p) = self.selected_compose().cloned() else { return };
                let action = match key.code {
                    KeyCode::Char('u') => Some(ComposeAction::Up),
                    KeyCode::Char('s') => Some(ComposeAction::Stop),
                    KeyCode::Char('r') => Some(ComposeAction::Restart),
                    KeyCode::Char('b') => Some(ComposeAction::Build),
                    KeyCode::Char('d') => {
                        if self.compose_ok {
                            self.confirm(ConfirmAction::ComposeDown(p));
                        } else {
                            self.apply(AppEvent::Toast("docker compose plugin not found".into(), true));
                        }
                        return;
                    }
                    _ => None,
                };
                if let Some(action) = action {
                    if self.compose_ok {
                        compose::compose_action(&self.tx, action, p);
                    } else {
                        self.apply(AppEvent::Toast("docker compose plugin not found".into(), true));
                    }
                }
            }
            Panel::Images => {
                match key.code {
                    KeyCode::Char('d') => {
                        let marked = &self.marked[Panel::Images as usize];
                        if marked.is_empty() {
                            if let Some(i) = self.selected_image().cloned() {
                                self.confirm(ConfirmAction::RemoveImage(i.id, i.tag));
                            }
                        } else {
                            let items: Vec<(String, String)> = self
                                .images
                                .iter()
                                .filter(|x| marked.contains(&x.id))
                                .map(|x| (x.id.clone(), x.tag.clone()))
                                .collect();
                            self.confirm(ConfirmAction::RemoveImagesBatch(items));
                        }
                    }
                    KeyCode::Char('D') => {
                        let items: Vec<(String, String)> = self
                            .filtered_images()
                            .iter()
                            .map(|x| (x.id.clone(), x.tag.clone()))
                            .collect();
                        if items.is_empty() {
                            self.apply(AppEvent::Toast("no images to remove".into(), true));
                        } else {
                            self.confirm(ConfirmAction::RemoveImagesBatch(items));
                        }
                    }
                    KeyCode::Char('P') => self.confirm(ConfirmAction::PruneImages),
                    _ => {}
                }
            }
            Panel::Volumes => match key.code {
                KeyCode::Char('d') => {
                    let marked = &self.marked[Panel::Volumes as usize];
                    if marked.is_empty() {
                        if let Some(v) = self.selected_volume().cloned() {
                            self.confirm(ConfirmAction::RemoveVolume(v.name));
                        }
                    } else {
                        let items: Vec<String> = self
                            .volumes
                            .iter()
                            .filter(|x| marked.contains(&x.name))
                            .map(|x| x.name.clone())
                            .collect();
                        self.confirm(ConfirmAction::RemoveVolumesBatch(items));
                    }
                }
                KeyCode::Char('D') => {
                    let items: Vec<String> =
                        self.filtered_volumes().iter().map(|x| x.name.clone()).collect();
                    if items.is_empty() {
                        self.apply(AppEvent::Toast("no volumes to remove".into(), true));
                    } else {
                        self.confirm(ConfirmAction::RemoveVolumesBatch(items));
                    }
                }
                KeyCode::Char('P') => self.confirm(ConfirmAction::PruneVolumes),
                _ => {}
            },
            Panel::Networks => {
                if key.code == KeyCode::Char('D') {
                    let items: Vec<(String, String)> = self
                        .filtered_networks()
                        .iter()
                        .map(|x| (x.id.clone(), x.name.clone()))
                        .collect();
                    if items.is_empty() {
                        self.apply(AppEvent::Toast("no networks to remove".into(), true));
                    } else {
                        self.confirm(ConfirmAction::RemoveNetworksBatch(items));
                    }
                    return;
                }
                if key.code == KeyCode::Char('d') {
                    let marked = &self.marked[Panel::Networks as usize];
                    if marked.is_empty() {
                        if let Some(n) = self.selected_network().cloned() {
                            self.confirm(ConfirmAction::RemoveNetwork(n.id, n.name));
                        }
                    } else {
                        let items: Vec<(String, String)> = self
                            .networks
                            .iter()
                            .filter(|x| marked.contains(&x.id))
                            .map(|x| (x.id.clone(), x.name.clone()))
                            .collect();
                        self.confirm(ConfirmAction::RemoveNetworksBatch(items));
                    }
                }
            }
        }
    }
}

/// Map an x offset within the detail tabs row (" Logs │ Stats │ Info") to a tab.
fn tab_at(rel: u16) -> Option<DetailTab> {
    let mut start = 0u16;
    for (i, len) in [4u16, 5, 4].into_iter().enumerate() {
        let end = start + len + 2; // 1 char padding each side
        if rel >= start && rel < end {
            return Some(match i {
                0 => DetailTab::Logs,
                1 => DetailTab::Stats,
                _ => DetailTab::Info,
            });
        }
        start = end + 1; // divider
    }
    None
}

/// Lowest published host port in a row's ports string
/// (`"5432/tcp 8080→80/tcp"` → 8080). Udp-only mappings don't count —
/// a browser can't open them.
pub fn first_public_port(ports: &str) -> Option<u16> {
    ports
        .split_whitespace()
        .filter(|t| !t.ends_with("/udp"))
        .filter_map(|t| t.split_once('→')?.0.parse().ok())
        .min()
}

/// Standard base64 (RFC 4648) — a dozen lines beats a dependency, OSC 52
/// is the only consumer.
pub fn base64(data: &[u8]) -> String {
    const TBL: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b = [chunk[0], chunk.get(1).copied().unwrap_or(0), chunk.get(2).copied().unwrap_or(0)];
        let n = u32::from_be_bytes([0, b[0], b[1], b[2]]);
        out.push(TBL[(n >> 18 & 63) as usize] as char);
        out.push(TBL[(n >> 12 & 63) as usize] as char);
        out.push(if chunk.len() > 1 { TBL[(n >> 6 & 63) as usize] as char } else { '=' });
        out.push(if chunk.len() > 2 { TBL[(n & 63) as usize] as char } else { '=' });
    }
    out
}

/// Copy text to the system clipboard with an OSC 52 escape sequence.
/// Terminals pass it through even in raw mode; tmux needs the sequence
/// wrapped in a DCS passthrough with ESCs doubled.
#[cfg(not(test))]
fn osc52_copy(text: &str) {
    use std::io::Write;
    let seq = format!("\x1b]52;c;{}\x07", base64(text.as_bytes()));
    let seq = if std::env::var_os("TMUX").is_some() {
        format!("\x1bPtmux;{}\x1b\\", seq.replace('\x1b', "\x1b\x1b"))
    } else {
        seq
    };
    let mut out = std::io::stdout();
    let _ = out.write_all(seq.as_bytes());
    let _ = out.flush();
}

/// Test builds skip the real escape write — it would land in the terminal
/// running `cargo test` and clobber the developer's clipboard.
#[cfg(test)]
fn osc52_copy(_text: &str) {}

/// Fire-and-forget `open`/`xdg-open`; a missing opener just does nothing.
fn open_in_browser(url: &str) {
    #[cfg(target_os = "macos")]
    let cmd = "open";
    #[cfg(not(target_os = "macos"))]
    let cmd = "xdg-open";
    let _ = std::process::Command::new(cmd)
        .arg(url)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
}

pub fn human_bytes(n: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut v = n as f64;
    let mut u = 0;
    while v >= 1024.0 && u < UNITS.len() - 1 {
        v /= 1024.0;
        u += 1;
    }
    if u == 0 {
        format!("{n}B")
    } else {
        format!("{v:.1}{}", UNITS[u])
    }
}

pub fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Exit code from a summary status like `Exited (137) 2 hours ago`.
pub fn exit_code_from_status(status: &str) -> Option<i32> {
    let rest = status.strip_prefix("Exited (")?;
    let end = rest.find(')')?;
    rest[..end].parse().ok()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HealthState {
    None,
    Starting,
    Healthy,
    Unhealthy,
}

/// Healthcheck state as embedded in the summary status text,
/// e.g. `Up 2 hours (healthy)`.
pub fn health_from_status(status: &str) -> HealthState {
    if status.contains("(unhealthy)") {
        HealthState::Unhealthy
    } else if status.contains("(healthy)") {
        HealthState::Healthy
    } else if status.contains("(health: starting)") {
        HealthState::Starting
    } else {
        HealthState::None
    }
}

pub fn ago(ts: i64) -> String {
    let d = (unix_now() - ts).max(0);
    match d {
        0..=59 => format!("{d}s ago"),
        60..=3599 => format!("{}m ago", d / 60),
        3600..=86399 => format!("{}h ago", d / 3600),
        _ => format!("{}d ago", d / 86400),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_app() -> App {
        // dummy client: constructing it never touches a socket, so tests
        // run without a docker daemon
        let docker = Docker::dummy();
        let (tx, rx) = std::sync::mpsc::channel();
        // keep the receiver alive so sends don't fail
        std::mem::forget(rx);
        App::new(docker, tx)
    }

    fn ctr(id: &str, name: &str, state: RowState) -> ContainerRow {
        ContainerRow {
            id: id.into(),
            name: name.into(),
            image: "img".into(),
            image_id: String::new(),
            state,
            status: "status".into(),
            ports: String::new(),
            created: 0,
            compose_project: None,
            compose_service: None,
            compose_files: String::new(),
            compose_dir: String::new(),
            volumes: Vec::new(),
            networks: Vec::new(),
        }
    }

    fn compose_ctr(id: &str, name: &str, state: RowState, project: &str, service: &str) -> ContainerRow {
        let mut c = ctr(id, name, state);
        c.compose_project = Some(project.into());
        c.compose_service = Some(service.into());
        c
    }

    fn img(id: &str, tag: &str) -> ImageRow {
        ImageRow { id: id.into(), tag: tag.into(), size: 0, created: 0, containers: 0 }
    }

    fn vol(name: &str) -> VolumeRow {
        VolumeRow {
            name: name.into(),
            driver: "local".into(),
            mountpoint: String::new(),
            created: String::new(),
        }
    }

    fn net(name: &str) -> NetworkRow {
        NetworkRow {
            id: name.into(),
            name: name.into(),
            driver: "bridge".into(),
            scope: "local".into(),
            subnet: String::new(),
        }
    }

    fn key(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE)
    }

    // ---- fuzzy filter ----

    #[test]
    fn filter_empty_matches_everything() {
        let app = test_app();
        assert!(app.matches("anything"));
        assert!(app.matches(""));
    }

    #[test]
    fn filter_substring_and_case_insensitive() {
        let mut app = test_app();
        app.filter = "NGI".into();
        assert!(app.matches("my-nginx-1"));
        assert!(!app.matches("postgres"));
    }

    #[test]
    fn filter_subsequence_match() {
        let mut app = test_app();
        app.filter = "pgs".into();
        assert!(app.matches("postgres"));
        app.filter = "pgz".into();
        assert!(!app.matches("postgres"));
    }

    #[test]
    fn filter_subsequence_needs_order() {
        let mut app = test_app();
        app.filter = "ba".into();
        assert!(!app.matches("ab"));
        assert!(app.matches("bca"));
    }

    // ---- human_bytes / ago ----

    #[test]
    fn human_bytes_edges() {
        assert_eq!(human_bytes(0), "0B");
        assert_eq!(human_bytes(512), "512B");
        assert_eq!(human_bytes(1023), "1023B");
        assert_eq!(human_bytes(1024), "1.0KiB");
        assert_eq!(human_bytes(1536), "1.5KiB");
        assert_eq!(human_bytes(1024 * 1024), "1.0MiB");
        // saturates at TiB unit, no panic
        assert!(human_bytes(u64::MAX).ends_with("TiB"));
    }

    #[test]
    fn ago_boundaries() {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        assert!(ago(now - 5).ends_with("s ago"));
        assert!(ago(now - 90).starts_with("1m"));
        assert!(ago(now - 3700).starts_with("1h"));
        assert!(ago(now - 90_000).starts_with("1d"));
        // future timestamp clamps to 0s, no underflow
        assert_eq!(ago(now + 1000), "0s ago");
    }

    // ---- panel cycling ----

    #[test]
    fn panel_next_cycles_through_all_five() {
        let mut p = Panel::Containers;
        let mut seen = Vec::new();
        for _ in 0..5 {
            seen.push(p);
            p = p.next();
        }
        assert_eq!(p, Panel::Containers);
        assert_eq!(seen.len(), 5);
        for pair in PANEL_ORDER {
            assert!(seen.contains(&pair));
        }
    }

    #[test]
    fn panel_prev_is_inverse_of_next() {
        for p in PANEL_ORDER {
            assert_eq!(p.next().prev(), p);
            assert_eq!(p.prev().next(), p);
        }
    }

    // ---- tab hit-testing ----

    #[test]
    fn tab_at_maps_offsets() {
        assert_eq!(tab_at(0), Some(DetailTab::Logs));
        assert_eq!(tab_at(5), Some(DetailTab::Logs));
        assert_eq!(tab_at(8), Some(DetailTab::Stats));
        assert_eq!(tab_at(16), Some(DetailTab::Info));
        assert_eq!(tab_at(30), None);
    }

    // ---- compose derivation ----

    #[test]
    fn rebuild_compose_groups_and_counts() {
        let mut app = test_app();
        let mut a = compose_ctr("c1", "web-1", RowState::Running, "proj", "web");
        a.compose_files = "/x/docker-compose.yml".into();
        a.compose_dir = "/x".into();
        app.containers = vec![
            a,
            compose_ctr("c2", "db-1", RowState::Exited, "proj", "db"),
            compose_ctr("c3", "other-1", RowState::Running, "zeta", "svc"),
            ctr("c4", "loose", RowState::Running),
        ];
        app.rebuild_compose();

        assert_eq!(app.compose.len(), 2);
        // BTreeMap keeps projects sorted by name
        assert_eq!(app.compose[0].name, "proj");
        assert_eq!(app.compose[1].name, "zeta");
        assert_eq!(app.compose[0].total, 2);
        assert_eq!(app.compose[0].running, 1);
        assert_eq!(app.compose[0].config_files, "/x/docker-compose.yml");
        assert_eq!(app.compose[0].working_dir, "/x");
    }

    #[test]
    fn rebuild_compose_paused_counts_as_running() {
        let mut app = test_app();
        app.containers = vec![compose_ctr("c1", "w", RowState::Paused, "p", "w")];
        app.rebuild_compose();
        assert_eq!(app.compose[0].running, 1);
    }

    #[test]
    fn rebuild_compose_empty_without_labels() {
        let mut app = test_app();
        app.containers = vec![ctr("c1", "a", RowState::Running)];
        app.rebuild_compose();
        assert!(app.compose.is_empty());
    }

    #[test]
    fn compose_members_falls_back_to_container_name() {
        let mut app = test_app();
        let mut c = compose_ctr("c1", "fallback-name", RowState::Running, "p", "svc");
        c.compose_service = None;
        app.containers = vec![c, compose_ctr("c2", "n2", RowState::Running, "p", "db")];
        let members = app.compose_members("p");
        assert_eq!(members.len(), 2);
        assert_eq!(members[0], ("c1".to_string(), "fallback-name".to_string()));
        assert_eq!(members[1], ("c2".to_string(), "db".to_string()));
    }

    // ---- log buffer ----

    #[test]
    fn log_event_splits_lines_and_trims_cr() {
        let mut app = test_app();
        app.logs_id = Some("x".into());
        app.apply(AppEvent::Log("x".into(), "one\r\ntwo\n\nthree".into()));
        assert_eq!(app.logs, vec!["one", "two", "three"]);
    }

    #[test]
    fn log_event_for_other_id_is_ignored() {
        let mut app = test_app();
        app.logs_id = Some("x".into());
        app.apply(AppEvent::Log("y".into(), "nope".into()));
        assert!(app.logs.is_empty());
    }

    #[test]
    fn log_buffer_caps_and_adjusts_scroll() {
        let mut app = test_app();
        app.logs_id = Some("x".into());
        app.log_scroll = 10;
        let chunk = (0..MAX_LOG_LINES + 100).map(|i| format!("l{i}\n")).collect::<String>();
        app.apply(AppEvent::Log("x".into(), chunk));
        assert_eq!(app.logs.len(), MAX_LOG_LINES);
        assert_eq!(app.logs[0], "l100");
        // scroll pulled back by the trimmed amount, saturating at 0
        assert_eq!(app.log_scroll, 0);
    }

    // ---- stats history ----

    #[test]
    fn stats_history_capped() {
        let mut app = test_app();
        for i in 0..(HISTORY_LEN + 10) {
            app.apply(AppEvent::Stat(StatSample {
                id: "c".into(),
                cpu_pct: i as f64,
                cpu_cores: 1,
                mem_pct: 0.0,
                mem_used: 0,
                mem_limit: 0,
                rx: 0,
                tx: 0,
                rx_rate: i as u64,
                tx_rate: i as u64,
                pids: 0,
            }));
        }
        let h = app.stats.get("c").unwrap();
        assert_eq!(h.cpu.len(), HISTORY_LEN);
        assert_eq!(h.mem.len(), HISTORY_LEN);
        assert_eq!(h.rx_rate.len(), HISTORY_LEN);
        assert_eq!(h.tx_rate.len(), HISTORY_LEN);
        assert_eq!(h.last.as_ref().unwrap().cpu_pct, (HISTORY_LEN + 9) as f64);
    }

    #[test]
    fn negative_cpu_clamped_in_history() {
        let mut app = test_app();
        app.apply(AppEvent::Stat(StatSample {
            id: "c".into(),
            cpu_pct: -5.0,
            cpu_cores: 1,
            mem_pct: -1.0,
            mem_used: 0,
            mem_limit: 0,
            rx: 0,
            tx: 0,
            rx_rate: 0,
            tx_rate: 0,
            pids: 0,
        }));
        let h = app.stats.get("c").unwrap();
        assert_eq!(h.cpu[0], 0);
        assert_eq!(h.mem[0], 0);
    }

    // ---- refresh events prune state ----

    #[test]
    fn containers_event_prunes_stats_and_marks() {
        let mut app = test_app();
        app.stats.insert("gone".into(), StatsHistory::default());
        app.stats.insert("kept".into(), StatsHistory::default());
        app.marked[Panel::Containers as usize].insert("gone".into());
        app.marked[Panel::Containers as usize].insert("kept".into());
        app.apply(AppEvent::Containers(vec![ctr("kept", "kept", RowState::Running)]));
        assert!(app.stats.contains_key("kept"));
        assert!(!app.stats.contains_key("gone"));
        assert!(app.marked[Panel::Containers as usize].contains("kept"));
        assert!(!app.marked[Panel::Containers as usize].contains("gone"));
    }

    #[test]
    fn images_event_prunes_marks() {
        let mut app = test_app();
        app.marked[Panel::Images as usize].insert("gone".into());
        app.marked[Panel::Images as usize].insert("kept".into());
        app.apply(AppEvent::Images(vec![img("kept", "kept:latest")]));
        assert_eq!(app.marked[Panel::Images as usize].len(), 1);
        assert!(app.marked[Panel::Images as usize].contains("kept"));
    }

    #[test]
    fn volumes_event_prunes_marks_by_name() {
        let mut app = test_app();
        app.marked[Panel::Volumes as usize].insert("gone".into());
        app.marked[Panel::Volumes as usize].insert("kept".into());
        app.apply(AppEvent::Volumes(vec![vol("kept")]));
        assert_eq!(app.marked[Panel::Volumes as usize].len(), 1);
    }

    #[test]
    fn volume_sizes_event_stored() {
        let mut app = test_app();
        let mut sizes = HashMap::new();
        sizes.insert("v1".to_string(), 42i64);
        app.apply(AppEvent::VolumeSizes(sizes));
        assert_eq!(app.volume_sizes.get("v1"), Some(&42));
    }

    // ---- selection & clamping ----

    #[test]
    fn clamp_selection_to_shorter_list() {
        let mut app = test_app();
        app.images = vec![img("a", "a"), img("b", "b"), img("c", "c")];
        app.sel[Panel::Images as usize] = 2;
        app.apply(AppEvent::Images(vec![img("a", "a")]));
        assert_eq!(app.sel[Panel::Images as usize], 0);
    }

    #[test]
    fn clamp_selection_empty_list_resets_to_zero() {
        let mut app = test_app();
        app.images = vec![img("a", "a")];
        app.sel[Panel::Images as usize] = 0;
        app.apply(AppEvent::Images(vec![]));
        assert_eq!(app.sel[Panel::Images as usize], 0);
        assert!(app.selected_image().is_none());
    }

    #[test]
    fn move_sel_on_empty_panel_is_noop() {
        let mut app = test_app();
        app.panel = Panel::Images;
        app.on_key(key('j'));
        assert_eq!(app.sel[Panel::Images as usize], 0);
    }

    #[test]
    fn move_sel_clamps_at_bounds() {
        let mut app = test_app();
        app.panel = Panel::Images;
        app.images = vec![img("a", "a"), img("b", "b")];
        app.on_key(key('k')); // up from 0, no other panel: stays 0
        assert_eq!(app.sel[Panel::Images as usize], 0);
        app.on_key(key('j'));
        app.on_key(key('j')); // past end, no other panel: clamps
        assert_eq!(app.sel[Panel::Images as usize], 1);
        assert_eq!(app.panel, Panel::Images);
    }

    #[test]
    fn nav_past_bottom_hops_to_next_nonempty_panel() {
        let mut app = test_app();
        app.panel = Panel::Images;
        app.images = vec![img("a", "a"), img("b", "b")];
        app.volumes = vec![vol("v1"), vol("v2")];
        app.sel[Panel::Images as usize] = 1;
        app.on_key(key('j')); // bottom of Images -> top of Volumes
        assert_eq!(app.panel, Panel::Volumes);
        assert_eq!(app.sel[Panel::Volumes as usize], 0);
    }

    #[test]
    fn nav_past_top_hops_to_prev_nonempty_panel_bottom() {
        let mut app = test_app();
        app.panel = Panel::Volumes;
        app.images = vec![img("a", "a"), img("b", "b")];
        app.volumes = vec![vol("v1")];
        app.on_key(key('k')); // top of Volumes -> bottom of Images (Compose empty, skipped)
        assert_eq!(app.panel, Panel::Images);
        assert_eq!(app.sel[Panel::Images as usize], 1);
    }

    #[test]
    fn nav_skips_empty_panels_and_wraps() {
        let mut app = test_app();
        app.panel = Panel::Networks;
        app.networks = vec![net("n1")];
        app.images = vec![img("a", "a")];
        app.on_key(key('j')); // bottom of Networks wraps past empty panels to Images
        assert_eq!(app.panel, Panel::Images);
        assert_eq!(app.sel[Panel::Images as usize], 0);
    }

    // ---- marking ----

    #[test]
    fn toggle_mark_adds_removes_and_advances() {
        let mut app = test_app();
        app.panel = Panel::Images;
        app.images = vec![img("a", "a"), img("b", "b")];
        app.on_key(key(' '));
        assert!(app.marked[Panel::Images as usize].contains("a"));
        assert_eq!(app.sel[Panel::Images as usize], 1); // advanced
        // go back and unmark
        app.on_key(key('k'));
        app.on_key(key(' '));
        assert!(!app.marked[Panel::Images as usize].contains("a"));
    }

    #[test]
    fn toggle_mark_all_respects_filter_and_toggles_off() {
        let mut app = test_app();
        app.panel = Panel::Images;
        app.images = vec![img("a", "nginx"), img("b", "redis"), img("c", "nginx-proxy")];
        app.filter = "nginx".into();
        app.on_key(key('A'));
        let marked = &app.marked[Panel::Images as usize];
        assert!(marked.contains("a") && marked.contains("c") && !marked.contains("b"));
        // all filtered already marked -> second A unmarks them
        app.on_key(key('A'));
        assert!(app.marked[Panel::Images as usize].is_empty());
    }

    #[test]
    fn mark_ignored_on_compose_panel() {
        let mut app = test_app();
        app.panel = Panel::Compose;
        app.compose = vec![ComposeRow {
            name: "p".into(),
            config_files: String::new(),
            working_dir: String::new(),
            running: 0,
            total: 1,
        }];
        app.on_key(key(' '));
        assert!(app.marked.iter().all(|s| s.is_empty()));
    }

    #[test]
    fn esc_clears_marks_before_filter() {
        let mut app = test_app();
        app.panel = Panel::Images;
        app.images = vec![img("a", "a")];
        app.filter = "a".into();
        app.marked[Panel::Images as usize].insert("a".into());
        app.on_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert!(app.marked[Panel::Images as usize].is_empty());
        assert_eq!(app.filter, "a"); // filter survives first Esc
        app.on_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert!(app.filter.is_empty());
    }

    // ---- batch confirm flows ----

    #[test]
    fn d_with_marks_confirms_batch_only_marked() {
        let mut app = test_app();
        app.panel = Panel::Images;
        app.images = vec![img("a", "a:1"), img("b", "b:1"), img("c", "c:1")];
        app.marked[Panel::Images as usize].insert("a".into());
        app.marked[Panel::Images as usize].insert("c".into());
        app.on_key(key('d'));
        match &app.mode {
            Mode::Confirm(ConfirmAction::RemoveImagesBatch(items)) => {
                let ids: Vec<&str> = items.iter().map(|(id, _)| id.as_str()).collect();
                assert_eq!(ids, vec!["a", "c"]);
            }
            other => panic!("expected batch confirm, got {other:?}"),
        }
    }

    #[test]
    fn d_without_marks_confirms_single() {
        let mut app = test_app();
        app.panel = Panel::Images;
        app.images = vec![img("a", "a:1")];
        app.on_key(key('d'));
        assert!(matches!(app.mode, Mode::Confirm(ConfirmAction::RemoveImage(..))));
    }

    #[test]
    fn delete_all_confirms_all_filtered_images_without_marks() {
        let mut app = test_app();
        app.panel = Panel::Images;
        app.images = vec![img("a", "nginx:1"), img("b", "redis:1")];
        app.filter = "nginx".into();
        app.on_key(key('D'));
        match &app.mode {
            Mode::Confirm(ConfirmAction::RemoveImagesBatch(items)) => {
                assert_eq!(items.len(), 1);
                assert_eq!(items[0].0, "a");
            }
            other => panic!("expected batch confirm, got {other:?}"),
        }
    }

    #[test]
    fn delete_all_with_no_images_toasts_error() {
        let mut app = test_app();
        app.panel = Panel::Images;
        app.on_key(key('D'));
        assert!(matches!(app.mode, Mode::Normal));
        assert!(app.toast.as_ref().unwrap().error);
    }

    #[test]
    fn batch_confirm_rejects_enter_requires_y() {
        let mut app = test_app();
        app.panel = Panel::Images;
        app.images = vec![img("a", "a:1"), img("b", "b:1")];
        app.on_key(key('D'));
        assert!(matches!(app.mode, Mode::Confirm(_)));
        // Enter cancels a batch confirm instead of running it
        app.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(matches!(app.mode, Mode::Normal));
        assert_eq!(app.images.len(), 2);
    }

    #[test]
    fn needs_explicit_yes_only_for_batches() {
        let two = vec![("a".to_string(), "a".to_string())];
        assert!(ConfirmAction::RemoveImagesBatch(two.clone()).needs_explicit_yes());
        assert!(ConfirmAction::RemoveContainersBatch(two.clone()).needs_explicit_yes());
        assert!(ConfirmAction::RemoveVolumesBatch(vec!["v".into()]).needs_explicit_yes());
        assert!(ConfirmAction::RemoveNetworksBatch(two).needs_explicit_yes());
        assert!(!ConfirmAction::RemoveImage("a".into(), "a".into()).needs_explicit_yes());
        assert!(!ConfirmAction::PruneImages.needs_explicit_yes());
    }

    #[test]
    fn confirm_cancel_on_any_other_key() {
        let mut app = test_app();
        app.panel = Panel::Images;
        app.images = vec![img("a", "a:1")];
        app.on_key(key('d'));
        assert!(matches!(app.mode, Mode::Confirm(_)));
        app.on_key(key('n'));
        assert!(matches!(app.mode, Mode::Normal));
        // image untouched — no remove ran (no daemon in tests anyway)
        assert_eq!(app.images.len(), 1);
    }

    #[test]
    fn describe_batch_counts() {
        let two = vec![("a".to_string(), "a".to_string()), ("b".to_string(), "b".to_string())];
        assert_eq!(
            ConfirmAction::RemoveImagesBatch(two.clone()).describe(),
            "Force-remove 2 images?"
        );
        assert_eq!(
            ConfirmAction::RemoveContainersBatch(two.clone()).describe(),
            "Force-remove 2 containers?"
        );
        assert_eq!(
            ConfirmAction::RemoveVolumesBatch(vec!["v".into()]).describe(),
            "Remove 1 volumes?"
        );
        assert_eq!(ConfirmAction::RemoveNetworksBatch(two).describe(), "Remove 2 networks?");
    }

    #[test]
    fn delete_all_confirms_all_filtered_containers() {
        let mut app = test_app();
        app.panel = Panel::Containers;
        app.containers = vec![
            ctr("c1", "nginx-1", RowState::Running),
            ctr("c2", "redis-1", RowState::Exited),
        ];
        app.filter = "nginx".into();
        app.on_key(key('D'));
        match &app.mode {
            Mode::Confirm(ConfirmAction::RemoveContainersBatch(items)) => {
                assert_eq!(items.len(), 1);
                assert_eq!(items[0].0, "c1");
            }
            other => panic!("expected batch confirm, got {other:?}"),
        }
    }

    #[test]
    fn delete_all_confirms_all_volumes_and_networks() {
        let mut app = test_app();
        app.panel = Panel::Volumes;
        app.volumes = vec![vol("v1"), vol("v2")];
        app.on_key(key('D'));
        match &app.mode {
            Mode::Confirm(ConfirmAction::RemoveVolumesBatch(items)) => {
                assert_eq!(items, &vec!["v1".to_string(), "v2".to_string()]);
            }
            other => panic!("expected batch confirm, got {other:?}"),
        }

        let mut app = test_app();
        app.panel = Panel::Networks;
        app.networks = vec![NetworkRow {
            id: "n1".into(),
            name: "bridge".into(),
            driver: "bridge".into(),
            scope: "local".into(),
            subnet: String::new(),
        }];
        app.on_key(key('D'));
        assert!(matches!(
            &app.mode,
            Mode::Confirm(ConfirmAction::RemoveNetworksBatch(items)) if items.len() == 1
        ));
    }

    #[test]
    fn delete_all_on_empty_container_and_volume_panels_toasts() {
        for panel in [Panel::Containers, Panel::Volumes, Panel::Networks] {
            let mut app = test_app();
            app.panel = panel;
            app.on_key(key('D'));
            assert!(matches!(app.mode, Mode::Normal));
            assert!(app.toast.as_ref().unwrap().error, "panel {panel:?} should toast");
        }
    }

    // ---- detail focus / zoom / env toggle ----

    #[test]
    fn enter_focuses_detail_and_esc_returns() {
        let mut app = test_app();
        app.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(app.focus, Focus::Detail);
        app.on_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert_eq!(app.focus, Focus::Panels);
    }

    #[test]
    fn h_and_l_move_focus() {
        let mut app = test_app();
        app.on_key(key('l'));
        assert_eq!(app.focus, Focus::Detail);
        app.on_key(key('h'));
        assert_eq!(app.focus, Focus::Panels);
    }

    #[test]
    fn jk_scroll_logs_when_detail_focused() {
        let mut app = test_app();
        app.containers = vec![ctr("c1", "a", RowState::Running)];
        app.logs = (0..100).map(|i| i.to_string()).collect();
        app.focus = Focus::Detail;
        app.on_key(key('j'));
        assert!(!app.follow);
        assert_eq!(app.log_scroll, 1);
        app.on_key(key('k'));
        assert_eq!(app.log_scroll, 0);
        // selection untouched while detail is focused
        assert_eq!(app.sel[Panel::Containers as usize], 0);
        assert_eq!(app.panel, Panel::Containers);
    }

    #[test]
    fn log_scrolling_uses_rendered_row_bounds() {
        let mut app = test_app();
        app.logs = vec!["one raw line".into()];
        app.log_visual_rows = 20;
        app.log_viewport_rows = 5;
        app.log_scroll = 15;
        app.focus = Focus::Detail;

        app.on_key(key('j'));
        assert_eq!(app.log_scroll, 15);
        app.on_key(key('k'));
        assert_eq!(app.log_scroll, 14);
    }

    #[test]
    fn w_toggles_log_wrapping() {
        let mut app = test_app();
        assert!(app.wrap_logs);
        app.on_key(key('w'));
        assert!(!app.wrap_logs);
        app.on_key(key('w'));
        assert!(app.wrap_logs);
    }

    #[test]
    fn tab_and_digits_return_focus_to_panels() {
        let mut app = test_app();
        app.focus = Focus::Detail;
        app.on_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        assert_eq!(app.focus, Focus::Panels);
        app.focus = Focus::Detail;
        app.on_key(key('3'));
        assert_eq!(app.focus, Focus::Panels);
        assert_eq!(app.panel, Panel::Images);
    }

    #[test]
    fn clicking_last_visible_row_activates_collapsed_panel() {
        let mut app = test_app();
        app.images = (0..5).map(|i| img(&format!("i{i}"), &format!("t{i}"))).collect();
        // A collapsed section is four rows high: border, header, one visible
        // data row, border. Its table has scrolled that data row to the end.
        app.layout.panels[Panel::Images as usize] = Rect::new(2, 10, 20, 4);
        *app.table_states[Panel::Images as usize].offset_mut() = 4;

        app.on_mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 5,
            row: 12,
            modifiers: KeyModifiers::NONE,
        });

        assert_eq!(app.panel, Panel::Images);
        assert_eq!(app.focus, Focus::Panels);
        assert_eq!(app.sel[Panel::Images as usize], 4);
    }

    #[test]
    fn esc_releases_detail_focus_before_marks_and_filter() {
        let mut app = test_app();
        app.panel = Panel::Images;
        app.images = vec![img("a", "a")];
        app.filter = "a".into();
        app.marked[Panel::Images as usize].insert("a".into());
        app.focus = Focus::Detail;
        app.on_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert_eq!(app.focus, Focus::Panels);
        assert!(!app.marked[Panel::Images as usize].is_empty());
        assert_eq!(app.filter, "a");
    }

    #[test]
    fn zoom_and_env_toggles() {
        let mut app = test_app();
        app.on_key(key('z'));
        assert!(app.zoom);
        app.on_key(key('z'));
        assert!(!app.zoom);
        app.on_key(key('x'));
        assert!(app.show_env);
        app.on_key(key('x'));
        assert!(!app.show_env);
    }

    // ---- filter mode ----

    #[test]
    fn filter_mode_types_and_escapes() {
        let mut app = test_app();
        app.panel = Panel::Images;
        app.images = vec![img("a", "nginx"), img("b", "redis")];
        app.on_key(key('/'));
        assert!(matches!(app.mode, Mode::Filter));
        app.on_key(key('r'));
        app.on_key(key('e'));
        assert_eq!(app.filter, "re");
        assert_eq!(app.filtered_images().len(), 1);
        // space in filter mode is text, not a mark
        app.on_key(key(' '));
        assert_eq!(app.filter, "re ");
        assert!(app.marked[Panel::Images as usize].is_empty());
        app.on_key(KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE));
        assert_eq!(app.filter, "re");
        app.on_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert!(app.filter.is_empty());
        assert!(matches!(app.mode, Mode::Normal));
    }

    #[test]
    fn filter_enter_keeps_filter() {
        let mut app = test_app();
        app.on_key(key('/'));
        app.on_key(key('x'));
        app.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(app.filter, "x");
        assert!(matches!(app.mode, Mode::Normal));
    }

    // ---- panel switching ----

    #[test]
    fn digit_keys_switch_panels() {
        let mut app = test_app();
        for (c, p) in [
            ('1', Panel::Containers),
            ('2', Panel::Compose),
            ('3', Panel::Images),
            ('4', Panel::Volumes),
            ('5', Panel::Networks),
        ] {
            app.on_key(key(c));
            assert_eq!(app.panel, p);
        }
    }

    #[test]
    fn quit_keys() {
        let mut app = test_app();
        app.on_key(key('q'));
        assert!(app.should_quit);

        let mut app2 = test_app();
        app2.on_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL));
        assert!(app2.should_quit);
    }

    // ---- events buffer / diagnostics ----

    fn event(at: i64, typ: &str, action: &str, id: &str) -> EventRow {
        EventRow {
            at,
            typ: typ.into(),
            action: action.into(),
            id: id.into(),
            name: id.into(),
        }
    }

    #[test]
    fn exit_code_from_status_parses() {
        assert_eq!(exit_code_from_status("Exited (137) 2 hours ago"), Some(137));
        assert_eq!(exit_code_from_status("Exited (0) 5 seconds ago"), Some(0));
        assert_eq!(exit_code_from_status("Up 2 hours"), None);
        assert_eq!(exit_code_from_status("Exited (bogus) now"), None);
    }

    #[test]
    fn health_from_status_variants() {
        assert_eq!(health_from_status("Up 2 hours (healthy)"), HealthState::Healthy);
        assert_eq!(health_from_status("Up 2 hours (unhealthy)"), HealthState::Unhealthy);
        assert_eq!(health_from_status("Up 3 seconds (health: starting)"), HealthState::Starting);
        assert_eq!(health_from_status("Up 2 hours"), HealthState::None);
    }

    #[test]
    fn events_buffer_caps() {
        let mut app = test_app();
        for i in 0..(MAX_EVENTS + 50) {
            app.apply(AppEvent::Event(event(i as i64, "container", "start", "c")));
        }
        assert_eq!(app.events.len(), MAX_EVENTS);
        assert_eq!(app.events.front().unwrap().at, 50);
    }

    #[test]
    fn oom_badge_set_and_cleared_on_start() {
        let mut app = test_app();
        app.apply(AppEvent::Event(event(1, "container", "oom", "c1")));
        assert!(app.oom_ids.contains("c1"));
        // oom on an image type is ignored
        app.apply(AppEvent::Event(event(2, "image", "oom", "i1")));
        assert!(!app.oom_ids.contains("i1"));
        app.apply(AppEvent::Event(event(3, "container", "start", "c1")));
        assert!(!app.oom_ids.contains("c1"));
    }

    #[test]
    fn oom_ids_pruned_when_container_disappears() {
        let mut app = test_app();
        app.apply(AppEvent::Event(event(1, "container", "oom", "gone")));
        app.apply(AppEvent::Containers(vec![ctr("kept", "kept", RowState::Running)]));
        assert!(!app.oom_ids.contains("gone"));
    }

    #[test]
    fn restart_loop_counts_dies_inside_window_only() {
        let mut app = test_app();
        let now = 10_000i64;
        // one stale die outside the window, two fresh ones
        app.apply(AppEvent::Event(event(now - RESTART_LOOP_WINDOW_SECS - 1, "container", "die", "c1")));
        app.apply(AppEvent::Event(event(now - 100, "container", "die", "c1")));
        app.apply(AppEvent::Event(event(now - 10, "container", "die", "c1")));
        // other container and other action don't count
        app.apply(AppEvent::Event(event(now - 5, "container", "die", "c2")));
        app.apply(AppEvent::Event(event(now - 5, "container", "stop", "c1")));
        assert_eq!(app.recent_die_count("c1", now), 2);
        assert!(!app.restart_looping("c1", now));
        app.apply(AppEvent::Event(event(now - 1, "container", "die", "c1")));
        assert!(app.restart_looping("c1", now));
    }

    #[test]
    fn events_overlay_opens_scrolls_and_closes() {
        let mut app = test_app();
        for i in 0..10 {
            app.apply(AppEvent::Event(event(i, "container", "start", "c")));
        }
        app.on_key(key('E'));
        assert!(matches!(app.mode, Mode::Events));
        assert_eq!(app.events_scroll, 0);
        app.on_key(key('k'));
        app.on_key(key('k'));
        assert_eq!(app.events_scroll, 2);
        app.on_key(key('j'));
        assert_eq!(app.events_scroll, 1);
        app.on_key(key('g'));
        assert_eq!(app.events_scroll, 9); // oldest
        app.on_key(key('G'));
        assert_eq!(app.events_scroll, 0); // newest
        // j at the bottom clamps
        app.on_key(key('j'));
        assert_eq!(app.events_scroll, 0);
        app.on_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert!(matches!(app.mode, Mode::Normal));
    }

    #[test]
    fn events_scroll_clamps_on_empty_buffer() {
        let mut app = test_app();
        app.on_key(key('E'));
        app.on_key(key('k'));
        assert_eq!(app.events_scroll, 0);
    }

    // ---- sorting ----

    #[test]
    fn containers_default_sort_running_first_then_name() {
        let mut app = test_app();
        app.containers = vec![
            ctr("c1", "zeta", RowState::Running),
            ctr("c2", "alpha", RowState::Exited),
            ctr("c3", "beta", RowState::Running),
        ];
        let names: Vec<&str> = app.filtered_containers().iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, vec!["beta", "zeta", "alpha"]);
    }

    #[test]
    fn images_default_sort_newest_first() {
        let mut app = test_app();
        let mut old = img("a", "old:1");
        old.created = 100;
        let mut new = img("b", "new:1");
        new.created = 200;
        app.images = vec![old, new];
        let tags: Vec<&str> = app.filtered_images().iter().map(|i| i.tag.as_str()).collect();
        assert_eq!(tags, vec!["new:1", "old:1"]);
    }

    #[test]
    fn comma_cycles_column_and_resets_direction() {
        let mut app = test_app();
        app.panel = Panel::Images;
        assert_eq!(app.sort_indicator(Panel::Images), "↓created");
        app.on_key(key(','));
        assert_eq!(app.sort_indicator(Panel::Images), "↓size");
        app.on_key(key(','));
        assert_eq!(app.sort_indicator(Panel::Images), "↑tag");
        app.on_key(key(','));
        app.on_key(key(','));
        // wraps back to the default column with its default direction
        assert_eq!(app.sort_indicator(Panel::Images), "↓created");
    }

    #[test]
    fn dot_reverses_sort_direction() {
        let mut app = test_app();
        app.panel = Panel::Volumes;
        app.volumes = vec![vol("b"), vol("a")];
        let names: Vec<&str> = app.filtered_volumes().iter().map(|v| v.name.as_str()).collect();
        assert_eq!(names, vec!["a", "b"]);
        app.on_key(key('.'));
        assert_eq!(app.sort_indicator(Panel::Volumes), "↓name");
        let names: Vec<&str> = app.filtered_volumes().iter().map(|v| v.name.as_str()).collect();
        assert_eq!(names, vec!["b", "a"]);
    }

    #[test]
    fn container_cpu_sort_puts_missing_stats_last() {
        let mut app = test_app();
        app.containers = vec![
            ctr("idle", "idle", RowState::Running),
            ctr("busy", "busy", RowState::Running),
            ctr("dead", "dead", RowState::Exited),
        ];
        app.apply(AppEvent::Stat(StatSample {
            id: "busy".into(),
            cpu_pct: 90.0,
            cpu_cores: 1,
            mem_pct: 0.0,
            mem_used: 0,
            mem_limit: 0,
            rx: 0,
            tx: 0,
            rx_rate: 0,
            tx_rate: 0,
            pids: 0,
        }));
        app.apply(AppEvent::Stat(StatSample {
            id: "idle".into(),
            cpu_pct: 1.0,
            cpu_cores: 1,
            mem_pct: 0.0,
            mem_used: 0,
            mem_limit: 0,
            rx: 0,
            tx: 0,
            rx_rate: 0,
            tx_rate: 0,
            pids: 0,
        }));
        // cycle: state -> name -> image -> created -> cpu
        for _ in 0..4 {
            app.on_key(key(','));
        }
        assert_eq!(app.sort_indicator(Panel::Containers), "↓cpu");
        let names: Vec<&str> = app.filtered_containers().iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, vec!["busy", "idle", "dead"]);
    }

    #[test]
    fn volume_size_sort_uses_df_sizes() {
        let mut app = test_app();
        app.panel = Panel::Volumes;
        app.volumes = vec![vol("small"), vol("big"), vol("unknown")];
        let mut sizes = HashMap::new();
        sizes.insert("small".to_string(), 10i64);
        sizes.insert("big".to_string(), 1000i64);
        app.apply(AppEvent::VolumeSizes(sizes));
        app.on_key(key(',')); // name -> size (desc)
        assert_eq!(app.sort_indicator(Panel::Volumes), "↓size");
        let names: Vec<&str> = app.filtered_volumes().iter().map(|v| v.name.as_str()).collect();
        assert_eq!(names, vec!["big", "small", "unknown"]);
    }

    #[test]
    fn sort_is_per_panel() {
        let mut app = test_app();
        app.panel = Panel::Networks;
        app.on_key(key(','));
        assert_eq!(app.sort_indicator(Panel::Networks), "↑driver");
        // other panels keep their defaults
        assert_eq!(app.sort_indicator(Panel::Containers), "↑state");
        assert_eq!(app.sort_indicator(Panel::Images), "↓created");
    }

    #[test]
    fn exec_requires_running_container() {
        let mut app = test_app();
        app.containers = vec![ctr("c1", "stopped", RowState::Exited)];
        app.panel = Panel::Containers;
        app.on_key(key('e'));
        assert!(app.pending_exec.is_none());
        assert!(app.toast.as_ref().unwrap().error);
    }

    // ---- base64 / OSC 52 ----

    #[test]
    fn base64_rfc4648_vectors() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64(b"foob"), "Zm9vYg==");
        assert_eq!(base64(b"fooba"), "Zm9vYmE=");
        assert_eq!(base64(b"foobar"), "Zm9vYmFy");
    }

    // ---- published port parsing ----

    #[test]
    fn first_public_port_picks_lowest_tcp() {
        assert_eq!(first_public_port(""), None);
        assert_eq!(first_public_port("80/tcp"), None); // exposed, not published
        assert_eq!(first_public_port("5432/tcp 8080→80/tcp"), Some(8080));
        assert_eq!(first_public_port("9000→9000/udp"), None);
        assert_eq!(first_public_port("10000→1/tcp 8080→80/tcp"), Some(8080));
    }

    #[test]
    fn open_port_without_publish_toasts_error() {
        let mut app = test_app();
        app.containers = vec![ctr("c1", "web", RowState::Running)];
        app.panel = Panel::Containers;
        app.on_key(key('o'));
        assert!(app.toast.as_ref().unwrap().error);
    }

    // ---- g/G list jumps ----

    #[test]
    fn g_and_shift_g_jump_list_when_panels_focused() {
        let mut app = test_app();
        app.panel = Panel::Images;
        app.images = vec![img("a", "a"), img("b", "b"), img("c", "c")];
        app.on_key(key('G'));
        assert_eq!(app.sel[Panel::Images as usize], 2);
        app.on_key(key('g'));
        assert_eq!(app.sel[Panel::Images as usize], 0);
        // follow state untouched — these were list jumps, not log commands
        assert!(app.follow);
    }

    #[test]
    fn g_and_shift_g_control_logs_when_detail_focused() {
        let mut app = test_app();
        app.containers = vec![ctr("c1", "a", RowState::Running)];
        app.logs = (0..50).map(|i| i.to_string()).collect();
        app.focus = Focus::Detail;
        app.on_key(key('g'));
        assert!(!app.follow);
        assert_eq!(app.log_scroll, 0);
        app.on_key(key('G'));
        assert!(app.follow);
        // selection untouched
        assert_eq!(app.sel[Panel::Containers as usize], 0);
    }

    // ---- kill signal picker ----

    #[test]
    fn kill_picker_opens_for_running_container_and_cancels() {
        let mut app = test_app();
        app.containers = vec![ctr("c1", "web", RowState::Running)];
        app.panel = Panel::Containers;
        app.on_key(key('K'));
        match &app.mode {
            Mode::Signal(id, name) => {
                assert_eq!(id, "c1");
                assert_eq!(name, "web");
            }
            other => panic!("expected signal picker, got {other:?}"),
        }
        // any non-signal key cancels
        app.on_key(key('x'));
        assert!(matches!(app.mode, Mode::Normal));
        app.on_key(key('K'));
        app.on_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert!(matches!(app.mode, Mode::Normal));
    }

    #[test]
    fn kill_picker_requires_running_container() {
        let mut app = test_app();
        app.containers = vec![ctr("c1", "dead", RowState::Exited)];
        app.panel = Panel::Containers;
        app.on_key(key('K'));
        assert!(matches!(app.mode, Mode::Normal));
        assert!(app.toast.as_ref().unwrap().error);
    }

    // ---- yank ----

    #[test]
    fn yank_toasts_selected_identity() {
        let mut app = test_app();
        app.panel = Panel::Images;
        app.images = vec![img("sha256:abc", "nginx:latest")];
        app.on_key(key('y'));
        let t = app.toast.as_ref().unwrap();
        assert!(!t.error);
        assert_eq!(t.text, "yanked nginx:latest");
        app.on_key(key('Y'));
        assert_eq!(app.toast.as_ref().unwrap().text, "yanked sha256:abc");
    }

    #[test]
    fn yank_on_empty_panel_is_noop() {
        let mut app = test_app();
        app.panel = Panel::Volumes;
        app.on_key(key('y'));
        assert!(app.toast.is_none());
    }
}
