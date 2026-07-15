use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{
    Block, BorderType, Borders, Cell, Clear, Gauge, Paragraph, Row, Scrollbar, ScrollbarState,
    Sparkline, Table, TableState, Tabs, Wrap,
};
use ratatui::Frame;

use crate::app::{
    ago, exit_code_from_status, health_from_status, human_bytes, unix_now, App, ContainerRow,
    DetailTab, Focus, HealthState, Mode, Panel, RowState, PANEL_ORDER,
};

const ACCENT: Color = Color::Cyan;
// muted-but-readable gray for secondary text (headers, sizes, hints)
const DIM: Color = Color::Rgb(148, 155, 164);
// quieter gray reserved for unfocused borders so panels don't shout
const BORDER_DIM: Color = Color::Rgb(85, 92, 100);

/// Height of a collapsed (unfocused) side panel: borders + header + one row.
const COLLAPSED_PANEL_H: u16 = 4;

pub fn draw(f: &mut Frame, app: &mut App) {
    // reset hit-test rects so panes hidden this frame (zoom) can't catch clicks
    app.layout = Default::default();

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(2), Constraint::Min(5), Constraint::Length(1)])
        .split(f.area());

    draw_header(f, app, chunks[0]);

    if app.zoom {
        // zoom: the focused pane takes the whole body
        if app.focus == Focus::Detail {
            draw_detail(f, app, chunks[1]);
        } else {
            draw_panel(f, app, app.panel, chunks[1]);
        }
    } else {
        let body = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(38), Constraint::Percentage(62)])
            .split(chunks[1]);

        // accordion: the active panel takes the slack, the rest collapse
        let constraints: Vec<Constraint> = PANEL_ORDER
            .iter()
            .map(|p| {
                if *p == app.panel {
                    Constraint::Fill(1)
                } else {
                    Constraint::Length(COLLAPSED_PANEL_H)
                }
            })
            .collect();
        let left = Layout::default()
            .direction(Direction::Vertical)
            .constraints(constraints)
            .split(body[0]);

        for (i, p) in PANEL_ORDER.iter().enumerate() {
            draw_panel(f, app, *p, left[i]);
        }
        draw_detail(f, app, body[1]);
    }
    draw_footer(f, app, chunks[2]);

    match &app.mode {
        Mode::Help => draw_help(f),
        Mode::Events => draw_events(f, app),
        Mode::Confirm(action) => {
            draw_confirm(f, &action.describe(), action.needs_explicit_yes())
        }
        Mode::Signal(_, name) => draw_signal(f, name),
        _ => {}
    }
}

fn draw_panel(f: &mut Frame, app: &mut App, panel: Panel, area: Rect) {
    match panel {
        Panel::Containers => draw_containers_panel(f, app, area),
        Panel::Compose => draw_compose_panel(f, app, area),
        Panel::Images => draw_images_panel(f, app, area),
        Panel::Volumes => draw_volumes_panel(f, app, area),
        Panel::Networks => draw_networks_panel(f, app, area),
    }
}

fn draw_header(f: &mut Frame, app: &App, area: Rect) {
    let running = app
        .containers
        .iter()
        .filter(|c| c.state == RowState::Running)
        .count();
    let mut spans = vec![
        Span::raw(" "),
        Span::styled(
            concat!(" ⚡ super-docker v", env!("CARGO_PKG_VERSION"), " "),
            Style::default().fg(Color::Black).bg(ACCENT).bold(),
        ),
        Span::raw("  "),
        Span::styled(
            if app.version.is_empty() {
                "docker: connecting…".to_string()
            } else {
                format!("docker v{}", app.version)
            },
            Style::default().fg(DIM),
        ),
        Span::raw("  "),
        Span::styled(
            format!("{running}/{} running", app.containers.len()),
            Style::default().fg(Color::Green),
        ),
    ];
    if let Some(err) = &app.docker_err {
        spans.push(Span::raw("  "));
        spans.push(Span::styled(format!("⚠ {err}"), Style::default().fg(Color::Red)));
    }
    f.render_widget(Paragraph::new(Line::from(spans)), area);

    if let Some(t) = &app.toast {
        if t.at.elapsed().as_secs() < 4 {
            let color = if t.error { Color::Red } else { Color::Green };
            let p = Paragraph::new(Line::from(Span::styled(
                format!(" {} ", t.text),
                Style::default().fg(Color::Black).bg(color),
            )))
            .alignment(Alignment::Right);
            f.render_widget(p, area);
        }
    }
}

fn panel_block(title: String, focused: bool) -> Block<'static> {
    let border = if focused {
        Style::default().fg(ACCENT)
    } else {
        Style::default().fg(BORDER_DIM)
    };
    let title_style = if focused {
        Style::default().fg(ACCENT).bold()
    } else {
        Style::default().fg(Color::Rgb(200, 205, 212))
    };
    Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(border)
        .title(Span::styled(title, title_style))
}

/// Shows `shown/total` when a filter hides rows, so unfocused panels
/// don't silently change counts. `sort` is the `↓name`-style indicator.
fn panel_title(num: usize, name: &str, shown: usize, total: usize, marked: usize, sort: &str) -> String {
    let count = if shown == total {
        format!("{shown}")
    } else {
        format!("{shown}/{total}")
    };
    if marked == 0 {
        format!(" [{num}] {name} ({count}) {sort} ")
    } else {
        format!(" [{num}] {name} ({count} · {marked}✓) {sort} ")
    }
}

fn mark_span(marked: bool) -> Span<'static> {
    if marked {
        Span::styled("✓ ", Style::default().fg(Color::Yellow).bold())
    } else {
        Span::raw("")
    }
}

fn render_scrollbar(
    f: &mut Frame,
    area: Rect,
    content_len: usize,
    viewport_len: usize,
    position: usize,
) {
    if viewport_len == 0 || content_len <= viewport_len || area.width == 0 || area.height == 0 {
        return;
    }
    let scrollbar = Scrollbar::default()
        .begin_symbol(None)
        .end_symbol(None)
        .track_symbol(Some("│"))
        .track_style(Style::default().fg(BORDER_DIM))
        .thumb_symbol("█")
        .thumb_style(Style::default().fg(ACCENT));
    let mut state = ScrollbarState::new(content_len)
        .position(position)
        .viewport_content_length(viewport_len);
    f.render_stateful_widget(scrollbar, area, &mut state);
}

fn state_dot(state: RowState) -> Span<'static> {
    let (sym, color) = match state {
        RowState::Running => ("●", Color::Green),
        RowState::Paused => ("◐", Color::Yellow),
        RowState::Restarting => ("↻", Color::Yellow),
        RowState::Exited => ("○", Color::Red),
        RowState::Created => ("◌", Color::Blue),
        RowState::Dead => ("✕", Color::Red),
        RowState::Other => ("?", Color::Gray),
    };
    Span::styled(format!("{sym} "), Style::default().fg(color))
}

/// Return the single-line rows that can actually be displayed. Keeping this
/// calculation outside ratatui lets callers avoid allocating widgets for
/// hundreds of off-screen rows on every frame.
fn table_window(
    content_len: usize,
    viewport_len: usize,
    selected: usize,
    current_offset: usize,
) -> std::ops::Range<usize> {
    if content_len == 0 || viewport_len == 0 {
        return 0..0;
    }
    let selected = selected.min(content_len - 1);
    let max_offset = content_len.saturating_sub(viewport_len);
    let mut offset = current_offset.min(max_offset);
    if selected < offset {
        offset = selected;
    } else if selected >= offset.saturating_add(viewport_len) {
        offset = selected + 1 - viewport_len;
    }
    offset..(offset + viewport_len).min(content_len)
}

fn panel_window(app: &App, panel: Panel, area: Rect, content_len: usize) -> std::ops::Range<usize> {
    let i = panel as usize;
    table_window(
        content_len,
        area.height.saturating_sub(3) as usize,
        app.sel[i],
        app.table_states[i].offset(),
    )
}

fn render_table(
    f: &mut Frame,
    app: &mut App,
    panel_idx: usize,
    area: Rect,
    block: Block,
    header: Row,
    rows: Vec<Row>,
    content_len: usize,
    content_offset: usize,
    widths: &[Constraint],
    focused: bool,
) {
    app.layout.panels[panel_idx] = area;
    // border + header consume three vertical cells in total.
    let viewport_len = area.height.saturating_sub(3) as usize;
    let highlight = if focused {
        Style::default().bg(Color::Rgb(30, 50, 60)).add_modifier(Modifier::BOLD)
    } else {
        Style::default().add_modifier(Modifier::BOLD)
    };
    let table = Table::new(rows, widths.to_vec())
        .header(header.style(Style::default().fg(DIM)))
        .block(block)
        .row_highlight_style(highlight)
        .highlight_symbol("▎");
    let mut visible_state = TableState::default();
    if content_len > 0 {
        visible_state.select(Some(app.sel[panel_idx].saturating_sub(content_offset)));
    }
    f.render_stateful_widget(table, area, &mut visible_state);
    let scrollbar_area = Rect {
        y: area.y.saturating_add(2),
        height: area.height.saturating_sub(3),
        ..area
    };
    render_scrollbar(f, scrollbar_area, content_len, viewport_len, content_offset);

    // Store the global offset/selection for the next frame and mouse hit
    // testing; the table itself only received the visible slice above.
    let state = &mut app.table_states[panel_idx];
    *state.offset_mut() = content_offset;
    state.select((content_len > 0).then_some(app.sel[panel_idx]));
}

/// Warning badges shown after the container name: non-zero exit code,
/// OOM kill, restart loop, failing healthcheck.
fn container_badges(app: &App, c: &ContainerRow, now: i64) -> Vec<Span<'static>> {
    let mut spans = Vec::new();
    if let Some(code) = exit_code_from_status(&c.status) {
        if code != 0 {
            spans.push(Span::styled(
                format!(" ({code})"),
                Style::default().fg(Color::Red).bold(),
            ));
        }
    }
    if app.oom_ids.contains(&c.id) {
        spans.push(Span::styled(" OOM", Style::default().fg(Color::Red).bold()));
    }
    if app.restart_looping(&c.id, now) {
        spans.push(Span::styled(" ↻loop", Style::default().fg(Color::Yellow).bold()));
    }
    match health_from_status(&c.status) {
        HealthState::Unhealthy => {
            spans.push(Span::styled(" ✚", Style::default().fg(Color::Red).bold()))
        }
        HealthState::Starting => {
            spans.push(Span::styled(" ✚", Style::default().fg(Color::Yellow)))
        }
        _ => {}
    }
    spans
}

fn draw_containers_panel(f: &mut Frame, app: &mut App, area: Rect) {
    let list = app.filtered_containers();
    let content_len = list.len();
    let window = panel_window(app, Panel::Containers, area, content_len);
    let content_offset = window.start;
    let focused = app.panel == Panel::Containers && app.focus == Focus::Panels;
    let marked = &app.marked[Panel::Containers as usize];
    let now = unix_now();
    let block = panel_block(
        panel_title(
            1,
            "Containers",
            list.len(),
            app.containers.len(),
            marked.len(),
            &app.sort_indicator(Panel::Containers),
        ),
        focused,
    );
    let rows: Vec<Row> = list[window]
        .iter()
        .map(|c| {
            let stats = app.stats.get(&c.id).and_then(|h| h.last.as_ref());
            let cpu = stats.map(|s| format!("{:.0}%", s.cpu_pct)).unwrap_or_else(|| "-".into());
            let mem = stats.map(|s| format!("{:.0}%", s.mem_pct)).unwrap_or_else(|| "-".into());
            let mut name_spans = vec![
                state_dot(c.state),
                mark_span(marked.contains(&c.id)),
                Span::raw(c.name.clone()),
            ];
            name_spans.extend(container_badges(app, c, now));
            Row::new(vec![
                Cell::from(Line::from(name_spans)),
                Cell::from(cpu).style(Style::default().fg(Color::Magenta)),
                Cell::from(mem).style(Style::default().fg(Color::Blue)),
            ])
        })
        .collect();
    render_table(
        f,
        app,
        Panel::Containers as usize,
        area,
        block,
        Row::new(vec!["name", "cpu", "mem"]),
        rows,
        content_len,
        content_offset,
        &[Constraint::Fill(1), Constraint::Length(5), Constraint::Length(5)],
        focused,
    );
}

fn draw_compose_panel(f: &mut Frame, app: &mut App, area: Rect) {
    let list = app.filtered_compose();
    let content_len = list.len();
    let window = panel_window(app, Panel::Compose, area, content_len);
    let content_offset = window.start;
    let focused = app.panel == Panel::Compose && app.focus == Focus::Panels;
    let mut title = panel_title(
        2,
        "Compose",
        list.len(),
        app.compose.len(),
        0,
        &app.sort_indicator(Panel::Compose),
    );
    if !app.compose_ok {
        title.push_str("· plugin n/a ");
    }
    let block = panel_block(title, focused);
    let rows: Vec<Row> = list[window]
        .iter()
        .map(|p| {
            let (sym, color) = if p.total == 0 || p.running == 0 {
                ("○", Color::Red)
            } else if p.running < p.total {
                ("◐", Color::Yellow)
            } else {
                ("●", Color::Green)
            };
            Row::new(vec![
                Cell::from(Line::from(vec![
                    Span::styled(format!("{sym} "), Style::default().fg(color)),
                    Span::raw(p.name.clone()),
                ])),
                Cell::from(format!("{}/{}", p.running, p.total)).style(Style::default().fg(DIM)),
            ])
        })
        .collect();
    render_table(
        f,
        app,
        Panel::Compose as usize,
        area,
        block,
        Row::new(vec!["project", "up"]),
        rows,
        content_len,
        content_offset,
        &[Constraint::Fill(1), Constraint::Length(5)],
        focused,
    );
}

fn draw_images_panel(f: &mut Frame, app: &mut App, area: Rect) {
    let list = app.filtered_images();
    let content_len = list.len();
    let window = panel_window(app, Panel::Images, area, content_len);
    let content_offset = window.start;
    let focused = app.panel == Panel::Images && app.focus == Focus::Panels;
    let marked = &app.marked[Panel::Images as usize];
    let block = panel_block(
        panel_title(
            3,
            "Images",
            list.len(),
            app.images.len(),
            marked.len(),
            &app.sort_indicator(Panel::Images),
        ),
        focused,
    );
    let rows: Vec<Row> = list[window]
        .iter()
        .map(|i| {
            Row::new(vec![
                Cell::from(Line::from(vec![
                    mark_span(marked.contains(&i.id)),
                    Span::raw(i.tag.clone()),
                ])),
                Cell::from(human_bytes(i.size.max(0) as u64)).style(Style::default().fg(DIM)),
            ])
        })
        .collect();
    render_table(
        f,
        app,
        Panel::Images as usize,
        area,
        block,
        Row::new(vec!["tag", "size"]),
        rows,
        content_len,
        content_offset,
        &[Constraint::Fill(1), Constraint::Length(9)],
        focused,
    );
}

fn draw_volumes_panel(f: &mut Frame, app: &mut App, area: Rect) {
    let list = app.filtered_volumes();
    let content_len = list.len();
    let window = panel_window(app, Panel::Volumes, area, content_len);
    let content_offset = window.start;
    let focused = app.panel == Panel::Volumes && app.focus == Focus::Panels;
    let marked = &app.marked[Panel::Volumes as usize];
    let block = panel_block(
        panel_title(
            4,
            "Volumes",
            list.len(),
            app.volumes.len(),
            marked.len(),
            &app.sort_indicator(Panel::Volumes),
        ),
        focused,
    );
    let rows: Vec<Row> = list[window]
        .iter()
        .map(|v| {
            let size = match app.volume_sizes.get(&v.name) {
                Some(s) if *s >= 0 => human_bytes(*s as u64),
                _ => "-".into(),
            };
            Row::new(vec![
                Cell::from(Line::from(vec![
                    mark_span(marked.contains(&v.name)),
                    Span::raw(v.name.clone()),
                ])),
                Cell::from(size).style(Style::default().fg(DIM)),
            ])
        })
        .collect();
    render_table(
        f,
        app,
        Panel::Volumes as usize,
        area,
        block,
        Row::new(vec!["name", "size"]),
        rows,
        content_len,
        content_offset,
        &[Constraint::Fill(1), Constraint::Length(8)],
        focused,
    );
}

fn draw_networks_panel(f: &mut Frame, app: &mut App, area: Rect) {
    let list = app.filtered_networks();
    let content_len = list.len();
    let window = panel_window(app, Panel::Networks, area, content_len);
    let content_offset = window.start;
    let focused = app.panel == Panel::Networks && app.focus == Focus::Panels;
    let marked = &app.marked[Panel::Networks as usize];
    let block = panel_block(
        panel_title(
            5,
            "Networks",
            list.len(),
            app.networks.len(),
            marked.len(),
            &app.sort_indicator(Panel::Networks),
        ),
        focused,
    );
    let rows: Vec<Row> = list[window]
        .iter()
        .map(|n| {
            Row::new(vec![
                Cell::from(Line::from(vec![
                    mark_span(marked.contains(&n.id)),
                    Span::raw(n.name.clone()),
                ])),
                Cell::from(n.driver.clone()).style(Style::default().fg(DIM)),
            ])
        })
        .collect();
    render_table(
        f,
        app,
        Panel::Networks as usize,
        area,
        block,
        Row::new(vec!["name", "driver"]),
        rows,
        content_len,
        content_offset,
        &[Constraint::Fill(1), Constraint::Length(8)],
        focused,
    );
}

fn draw_detail(f: &mut Frame, app: &mut App, area: Rect) {
    app.layout.detail = area;
    match app.panel {
        Panel::Containers => draw_container_detail(f, app, area),
        Panel::Compose => draw_compose_detail(f, app, area),
        Panel::Images => {
            let related = app
                .selected_image()
                .map(|i| {
                    related_lines(app.containers.iter().filter(|c| {
                        (i.tag != "<none>:<none>" && c.image == i.tag)
                            || (!i.id.is_empty() && c.image_id == i.id)
                    }))
                })
                .unwrap_or_default();
            draw_kv_detail(f, app, area, " Image ", image_kv(app), "containers using image", related);
        }
        Panel::Volumes => {
            let related = app
                .selected_volume()
                .map(|v| {
                    related_lines(
                        app.containers.iter().filter(|c| c.volumes.contains(&v.name)),
                    )
                })
                .unwrap_or_default();
            draw_kv_detail(f, app, area, " Volume ", volume_kv(app), "containers mounting", related);
        }
        Panel::Networks => {
            let related = app
                .selected_network()
                .map(|n| {
                    related_lines(
                        app.containers.iter().filter(|c| c.networks.contains(&n.name)),
                    )
                })
                .unwrap_or_default();
            draw_kv_detail(f, app, area, " Network ", network_kv(app), "containers attached", related);
        }
    }
}

fn related_lines<'a>(ctrs: impl Iterator<Item = &'a ContainerRow>) -> Vec<Line<'static>> {
    ctrs.map(|c| {
        Line::from(vec![
            state_dot(c.state),
            Span::raw(c.name.clone()),
            Span::styled(format!("  {}", c.status), Style::default().fg(DIM)),
        ])
    })
    .collect()
}

fn draw_compose_detail(f: &mut Frame, app: &mut App, area: Rect) {
    let Some(p) = app.selected_compose().cloned() else {
        let block = panel_block(" no compose project ".into(), app.focus == Focus::Detail);
        f.render_widget(
            Paragraph::new("containers started via `docker compose` show up here")
                .style(Style::default().fg(DIM))
                .block(block),
            area,
        );
        return;
    };
    let title = format!(" {} · {}/{} running ", p.name, p.running, p.total);
    let block = panel_block(title, app.focus == Focus::Detail);
    let inner = block.inner(area);
    f.render_widget(block, area);

    let services: Vec<&crate::app::ContainerRow> = app
        .containers
        .iter()
        .filter(|c| c.compose_project.as_deref() == Some(p.name.as_str()))
        .collect();
    // services table on top (header + rows), aggregated logs below
    let table_h = (services.len() as u16 + 1).min(inner.height / 2);
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(table_h), Constraint::Min(3)])
        .split(inner);

    let rows: Vec<Row> = services
        .iter()
        .map(|c| {
            let service = c.compose_service.clone().unwrap_or_else(|| c.name.clone());
            Row::new(vec![
                Cell::from(Line::from(vec![state_dot(c.state), Span::raw(service)])),
                Cell::from(c.status.clone()).style(Style::default().fg(DIM)),
                Cell::from(c.ports.clone()).style(Style::default().fg(DIM)),
            ])
        })
        .collect();
    let table = Table::new(
        rows,
        [Constraint::Fill(1), Constraint::Fill(1), Constraint::Fill(1)],
    )
    .header(Row::new(vec!["service", "status", "ports"]).style(Style::default().fg(DIM)));
    f.render_widget(table, chunks[0]);

    draw_logs(f, app, chunks[1]);
}

fn image_kv(app: &App) -> Vec<(String, String)> {
    let Some(i) = app.selected_image() else { return vec![] };
    vec![
        ("Tag".into(), i.tag.clone()),
        ("ID".into(), i.id.clone()),
        ("Size".into(), human_bytes(i.size.max(0) as u64)),
        ("Created".into(), ago(i.created)),
        ("Containers".into(), i.containers.max(0).to_string()),
    ]
}

fn volume_kv(app: &App) -> Vec<(String, String)> {
    let Some(v) = app.selected_volume() else { return vec![] };
    let size = match app.volume_sizes.get(&v.name) {
        Some(s) if *s >= 0 => human_bytes(*s as u64),
        _ => "unknown".into(),
    };
    vec![
        ("Name".into(), v.name.clone()),
        ("Driver".into(), v.driver.clone()),
        ("Size".into(), size),
        ("Mountpoint".into(), v.mountpoint.clone()),
        ("Created".into(), v.created.clone()),
    ]
}

fn network_kv(app: &App) -> Vec<(String, String)> {
    let Some(n) = app.selected_network() else { return vec![] };
    vec![
        ("Name".into(), n.name.clone()),
        ("ID".into(), n.id.chars().take(12).collect()),
        ("Driver".into(), n.driver.clone()),
        ("Scope".into(), n.scope.clone()),
        ("Subnet".into(), n.subnet.clone()),
    ]
}

fn draw_kv_detail(
    f: &mut Frame,
    app: &App,
    area: Rect,
    title: &str,
    kv: Vec<(String, String)>,
    related_label: &str,
    related: Vec<Line<'static>>,
) {
    let block = panel_block(title.to_string(), app.focus == Focus::Detail);
    let inner = block.inner(area);
    f.render_widget(block, area);
    if inner.height == 0 {
        return;
    }

    let kv_h = (kv.len() as u16).min(inner.height);
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(kv_h),
            Constraint::Length(2),
            Constraint::Min(0),
        ])
        .split(inner);

    let rows: Vec<Row> = kv
        .into_iter()
        .map(|(k, v)| {
            Row::new(vec![
                Cell::from(k).style(Style::default().fg(ACCENT)),
                Cell::from(v),
            ])
        })
        .collect();
    let table = Table::new(rows, [Constraint::Length(14), Constraint::Fill(1)]);
    f.render_widget(table, chunks[0]);

    let header = Line::from(Span::styled(
        format!("{related_label} ({})", related.len()),
        Style::default().fg(DIM),
    ));
    f.render_widget(
        Paragraph::new(header).block(
            Block::default()
                .borders(Borders::TOP)
                .border_style(Style::default().fg(BORDER_DIM)),
        ),
        chunks[1],
    );
    f.render_widget(Paragraph::new(Text::from(related)), chunks[2]);
}

fn draw_container_detail(f: &mut Frame, app: &mut App, area: Rect) {
    let title = app
        .selected_container()
        .map(|c| format!(" {} — {} · created {} ", c.name, c.status, ago(c.created)))
        .unwrap_or_else(|| " no container selected ".into());
    let block = panel_block(title, app.focus == Focus::Detail);
    let inner = block.inner(area);
    f.render_widget(block, area);

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1), Constraint::Min(3)])
        .split(inner);

    app.layout.tabs_row = chunks[0];
    let tabs = Tabs::new(vec!["Logs", "Stats", "Info"])
        .select(app.detail as usize)
        .style(Style::default().fg(DIM))
        .highlight_style(Style::default().fg(ACCENT).bold())
        .divider("│");
    f.render_widget(tabs, chunks[0]);

    match app.detail {
        DetailTab::Logs => draw_logs(f, app, chunks[1]),
        DetailTab::Stats => draw_stats(f, app, chunks[1]),
        DetailTab::Info => {
            let table = Table::new(
                app.inspect
                    .iter()
                    .map(|(k, v)| {
                        // env values masked unless toggled — screens get shared
                        let shown = if k == "Env" && !app.show_env {
                            mask_env(v)
                        } else {
                            v.clone()
                        };
                        Row::new(vec![
                            Cell::from(k.clone()).style(Style::default().fg(ACCENT)),
                            Cell::from(shown),
                        ])
                    })
                    .collect::<Vec<_>>(),
                [Constraint::Length(16), Constraint::Fill(1)],
            );
            f.render_widget(table, chunks[1]);
        }
    }
}

/// `KEY=value` → `KEY=•••`; entries without a value pass through untouched.
fn mask_env(v: &str) -> String {
    match v.split_once('=') {
        Some((k, _)) => format!("{k}=•••"),
        None => v.to_string(),
    }
}

fn log_line_style(line: &str) -> Style {
    let lower = line.to_lowercase();
    if lower.contains("error") || lower.contains("fatal") || lower.contains("panic") {
        Style::default().fg(Color::Red)
    } else if lower.contains("warn") {
        Style::default().fg(Color::Yellow)
    } else if lower.contains("debug") || lower.contains("trace") {
        Style::default().fg(DIM)
    } else {
        Style::default()
    }
}

fn draw_logs(f: &mut Frame, app: &App, area: Rect) {
    // The top border used for the follow indicator is outside the log
    // viewport itself.
    let h = area.height.saturating_sub(1) as usize;
    let len = app.logs.len();
    let start = if app.follow {
        len.saturating_sub(h)
    } else {
        app.log_scroll.min(len.saturating_sub(1))
    };
    let end = (start + h).min(len);
    let lines: Vec<Line> = app.logs[start..end]
        .iter()
        .map(|l| Line::from(Span::styled(l.clone(), log_line_style(l))))
        .collect();
    let mode = if app.follow {
        Span::styled(" following ", Style::default().fg(Color::Black).bg(Color::Green))
    } else {
        Span::styled(
            format!(" {}/{} (f: follow) ", end, len),
            Style::default().fg(Color::Black).bg(Color::Yellow),
        )
    };
    let block = Block::default()
        .borders(Borders::TOP)
        .border_style(Style::default().fg(BORDER_DIM))
        .title_alignment(Alignment::Right)
        .title(mode);
    f.render_widget(Paragraph::new(Text::from(lines)).block(block), area);
    let scrollbar_area = Rect {
        y: area.y.saturating_add(1),
        height: area.height.saturating_sub(1),
        ..area
    };
    render_scrollbar(f, scrollbar_area, len, h, start);
}

fn draw_stats(f: &mut Frame, app: &App, area: Rect) {
    let Some(c) = app.selected_container() else { return };
    let Some(hist) = app.stats.get(&c.id) else {
        f.render_widget(
            Paragraph::new("waiting for live stats — the container may not be running")
                .style(Style::default().fg(DIM))
                .alignment(Alignment::Center)
                .wrap(Wrap { trim: true }),
            area,
        );
        return;
    };
    let Some(last) = hist.last.as_ref() else { return };

    if area.width < 52 || area.height < 13 {
        draw_stats_compact(f, area, c, last);
        return;
    }

    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Fill(1),
            Constraint::Fill(1),
            Constraint::Length(1),
        ])
        .spacing(1)
        .split(area);
    let top = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
        .spacing(1)
        .split(rows[0]);
    let bottom = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
        .spacing(1)
        .split(rows[1]);

    let cpu_capacity = last.cpu_cores.max(1).saturating_mul(100);
    draw_gauge_stat_card(
        f,
        top[0],
        format!(
            "CPU · {} {}",
            last.cpu_cores,
            if last.cpu_cores == 1 { "core" } else { "cores" }
        ),
        format!("{:.1}%", last.cpu_pct),
        (last.cpu_pct / cpu_capacity as f64).clamp(0.0, 1.0),
        &hist.cpu,
        cpu_capacity,
        Color::Magenta,
    );
    let mem_label = if last.mem_limit == 0 {
        format!("{:.1}% · {}", last.mem_pct, human_bytes(last.mem_used))
    } else {
        format!(
            "{:.1}% · {} / {}",
            last.mem_pct,
            human_bytes(last.mem_used),
            human_bytes(last.mem_limit)
        )
    };
    draw_gauge_stat_card(
        f,
        top[1],
        "MEMORY".into(),
        mem_label,
        (last.mem_pct / 100.0).clamp(0.0, 1.0),
        &hist.mem,
        100,
        Color::Blue,
    );
    draw_rate_stat_card(
        f,
        bottom[0],
        "↓ RECEIVE",
        last.rx_rate,
        last.rx,
        &hist.rx_rate,
        Color::Green,
    );
    draw_rate_stat_card(
        f,
        bottom[1],
        "↑ TRANSMIT",
        last.tx_rate,
        last.tx,
        &hist.tx_rate,
        Color::Yellow,
    );

    let ports = if c.ports.is_empty() { "-" } else { &c.ports };
    f.render_widget(
        Paragraph::new(format!(
            "live · {} samples · {} pids · ports {ports}",
            hist.cpu.len(),
            last.pids
        ))
        .style(Style::default().fg(DIM))
        .alignment(Alignment::Center),
        rows[2],
    );
}

fn stat_card(title: String, color: Color) -> Block<'static> {
    Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(BORDER_DIM))
        .title(Span::styled(
            format!(" {title} "),
            Style::default().fg(color).bold(),
        ))
}

#[allow(clippy::too_many_arguments)]
fn draw_gauge_stat_card(
    f: &mut Frame,
    area: Rect,
    title: String,
    label: String,
    ratio: f64,
    history: &std::collections::VecDeque<u64>,
    max: u64,
    color: Color,
) {
    let block = stat_card(title, color);
    let inner = block.inner(area);
    f.render_widget(block, area);
    if inner.height == 0 {
        return;
    }
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1), Constraint::Min(0)])
        .split(inner);
    f.render_widget(
        Gauge::default()
            .ratio(ratio)
            .label(label)
            .gauge_style(Style::default().fg(color).bg(Color::Rgb(30, 30, 30))),
        chunks[0],
    );
    if chunks[1].height > 0 {
        f.render_widget(
            Sparkline::default()
                .data(history.iter().copied().collect::<Vec<_>>())
                .max(max.max(1))
                .style(Style::default().fg(color)),
            chunks[1],
        );
    }
}

fn draw_rate_stat_card(
    f: &mut Frame,
    area: Rect,
    title: &str,
    rate: u64,
    total: u64,
    history: &std::collections::VecDeque<u64>,
    color: Color,
) {
    let block = stat_card(format!("{title} · {}", human_rate(rate)), color).title_bottom(
        Line::from(Span::styled(
            format!(" total {} ", human_bytes(total)),
            Style::default().fg(DIM),
        ))
        .alignment(Alignment::Right),
    );
    f.render_widget(
        Sparkline::default()
            .data(history.iter().copied().collect::<Vec<_>>())
            .style(Style::default().fg(color))
            .block(block),
        area,
    );
}

fn draw_stats_compact(
    f: &mut Frame,
    area: Rect,
    c: &ContainerRow,
    sample: &crate::app::StatSample,
) {
    let ports = if c.ports.is_empty() { "-" } else { &c.ports };
    let cpu_unit = if sample.cpu_cores == 1 { "core" } else { "cores" };
    let memory = if sample.mem_limit == 0 {
        human_bytes(sample.mem_used)
    } else {
        format!(
            "{} / {}",
            human_bytes(sample.mem_used),
            human_bytes(sample.mem_limit)
        )
    };
    let lines = vec![
        Line::from(vec![
            Span::styled("CPU  ", Style::default().fg(Color::Magenta).bold()),
            Span::raw(format!("{:.1}% · {} {cpu_unit}", sample.cpu_pct, sample.cpu_cores)),
        ]),
        Line::from(vec![
            Span::styled("MEM  ", Style::default().fg(Color::Blue).bold()),
            Span::raw(format!("{:.1}% · {memory}", sample.mem_pct)),
        ]),
        Line::from(vec![
            Span::styled("NET  ", Style::default().fg(Color::Green).bold()),
            Span::raw(format!(
                "↓{}  ↑{}",
                human_rate(sample.rx_rate),
                human_rate(sample.tx_rate)
            )),
        ]),
        Line::from(Span::styled(
            format!("{} pids · ports {ports}", sample.pids),
            Style::default().fg(DIM),
        )),
    ];
    f.render_widget(Paragraph::new(lines), area);
}

fn human_rate(bytes_per_second: u64) -> String {
    format!("{}/s", human_bytes(bytes_per_second))
}

fn draw_footer(f: &mut Frame, app: &App, area: Rect) {
    let line = match &app.mode {
        Mode::Filter => Line::from(vec![
            Span::styled(" / ", Style::default().fg(Color::Black).bg(Color::Yellow)),
            Span::raw(format!("{}▏", app.filter)),
            Span::styled("  Enter keep · Esc clear", Style::default().fg(DIM)),
        ]),
        _ => {
            let hints: &[(&str, &str)] = if app.focus == Focus::Detail {
                &[
                    ("j/k", "scroll"),
                    ("[/]", "tab"),
                    ("f", "follow"),
                    ("g", "top"),
                    ("x", "env"),
                    ("z", "zoom"),
                    ("esc", "back"),
                    ("q", "quit"),
                ]
            } else {
                match app.panel {
                    Panel::Containers => &[
                        ("j/k", "select"),
                        ("↵", "logs"),
                        ("spc", "mark"),
                        ("s/S", "stop/start"),
                        ("r", "restart"),
                        ("e", "shell"),
                        ("K", "kill"),
                        ("y", "yank"),
                        ("o", "port"),
                        ("d", "remove"),
                        ("E", "events"),
                        ("/", "filter"),
                        ("?", "help"),
                    ],
                    Panel::Compose => &[
                        ("j/k", "select"),
                        ("↵", "logs"),
                        (",/.", "sort"),
                        ("u", "up -d"),
                        ("s", "stop"),
                        ("r", "restart"),
                        ("b", "build"),
                        ("d", "down"),
                        ("/", "filter"),
                        ("?", "help"),
                        ("q", "quit"),
                    ],
                    Panel::Images => &[
                        ("j/k", "select"),
                        ("spc/A", "mark"),
                        (",/.", "sort"),
                        ("y", "yank"),
                        ("d", "remove"),
                        ("D", "del all"),
                        ("P", "prune"),
                        ("/", "filter"),
                        ("?", "help"),
                        ("q", "quit"),
                    ],
                    Panel::Volumes => &[
                        ("j/k", "select"),
                        ("spc/A", "mark"),
                        (",/.", "sort"),
                        ("y", "yank"),
                        ("d", "remove"),
                        ("D", "del all"),
                        ("P", "prune"),
                        ("?", "help"),
                        ("q", "quit"),
                    ],
                    Panel::Networks => &[
                        ("j/k", "select"),
                        ("spc/A", "mark"),
                        (",/.", "sort"),
                        ("y", "yank"),
                        ("d", "remove"),
                        ("D", "del all"),
                        ("?", "help"),
                        ("q", "quit"),
                    ],
                }
            };
            let mut spans = Vec::new();
            if !app.filter.is_empty() {
                spans.push(Span::styled(
                    format!(" filter:{} ", app.filter),
                    Style::default().fg(Color::Black).bg(Color::Yellow),
                ));
                spans.push(Span::raw(" "));
            }
            for (k, v) in hints {
                spans.push(Span::styled(*k, Style::default().fg(ACCENT).bold()));
                spans.push(Span::styled(format!(" {v}  "), Style::default().fg(DIM)));
            }
            Line::from(spans)
        }
    };
    f.render_widget(Paragraph::new(line), area);
}

fn centered(area: Rect, w: u16, h: u16) -> Rect {
    let w = w.min(area.width);
    let h = h.min(area.height);
    Rect::new(
        area.x + (area.width - w) / 2,
        area.y + (area.height - h) / 2,
        w,
        h,
    )
}

fn draw_modal(f: &mut Frame, title: &str, color: Color, text: &str, hint: &str) {
    let w = (text.len() as u16 + 6).max(hint.len() as u16 + 4).max(30);
    let area = centered(f.area(), w, 5);
    f.render_widget(Clear, area);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(color))
        .title(title.to_string());
    let p = Paragraph::new(vec![
        Line::from(text.to_string()),
        Line::from(""),
        Line::from(Span::styled(hint.to_string(), Style::default().fg(DIM))),
    ])
    .alignment(Alignment::Center)
    .block(block);
    f.render_widget(p, area);
}

fn draw_confirm(f: &mut Frame, text: &str, explicit_yes: bool) {
    let hint = if explicit_yes {
        "y confirm · any other key cancel"
    } else {
        "y/Enter confirm · any other key cancel"
    };
    draw_modal(f, " confirm ", Color::Red, text, hint);
}

fn draw_signal(f: &mut Frame, name: &str) {
    draw_modal(
        f,
        " kill ",
        Color::Yellow,
        &format!("Send a signal to '{name}'?"),
        "t TERM · k KILL · h HUP · any other key cancel",
    );
}

fn draw_help(f: &mut Frame) {
    let keys: &[(&str, &str)] = &[
        ("j/k ↑/↓", "move selection / scroll detail"),
        ("Tab / 1-5", "switch panel"),
        ("Enter / l", "focus detail pane"),
        ("h / Esc", "back to panels"),
        ("z", "zoom focused pane"),
        ("[ ] ←/→", "logs / stats / info"),
        ("/", "fuzzy filter (Esc clears)"),
        ("space / A", "mark row / mark all"),
        (", / .", "cycle sort column / reverse"),
        ("Esc", "detail, then marks, then filter"),
        ("g / G", "top / bottom of list or logs"),
        ("y / Y", "yank name / id to clipboard"),
        ("o", "open published port in browser"),
        ("s / S", "stop / start container"),
        ("r", "restart container"),
        ("p", "pause / unpause"),
        ("K", "kill via signal picker"),
        ("e", "exec shell into container"),
        ("d", "remove selected or marked (confirm)"),
        ("D", "remove ALL listed in panel (y confirms)"),
        ("C", "prune stopped containers"),
        ("P", "prune images / volumes"),
        ("u / b", "compose up -d / build"),
        ("d (compose)", "compose down (confirm)"),
        ("x", "show/hide env values (Info tab)"),
        ("E", "docker events overlay"),
        ("mouse", "click select/focus/tabs, wheel scroll"),
        ("PgUp/PgDn", "scroll logs"),
        ("f", "follow logs"),
        ("q / Ctrl-c", "quit"),
    ];
    let w = 52;
    let h = keys.len() as u16 + 4;
    let area = centered(f.area(), w, h);
    f.render_widget(Clear, area);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(ACCENT))
        .title(" keys ");
    let lines: Vec<Line> = keys
        .iter()
        .map(|(k, v)| {
            Line::from(vec![
                Span::styled(format!("  {k:<16}"), Style::default().fg(ACCENT).bold()),
                Span::raw(v.to_string()),
            ])
        })
        .chain(std::iter::once(Line::from("")))
        .chain(std::iter::once(Line::from(Span::styled(
            "  any key to close",
            Style::default().fg(DIM),
        ))))
        .collect();
    f.render_widget(Paragraph::new(lines).block(block).wrap(Wrap { trim: false }), area);
}

fn event_action_color(action: &str) -> Color {
    if action.contains("unhealthy")
        || matches!(action, "die" | "kill" | "oom" | "destroy" | "delete")
    {
        Color::Red
    } else if action.contains("healthy")
        || matches!(action, "start" | "create" | "restart" | "unpause")
    {
        Color::Green
    } else if matches!(action, "stop" | "pause") {
        Color::Yellow
    } else {
        DIM
    }
}

fn draw_events(f: &mut Frame, app: &App) {
    let w = f.area().width.saturating_sub(6).min(100);
    let h = f.area().height.saturating_sub(4);
    let area = centered(f.area(), w, h);
    f.render_widget(Clear, area);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(ACCENT))
        .title(format!(" events ({}) ", app.events.len()))
        .title_bottom(
            Line::from(Span::styled(
                " j/k scroll · g/G ends · Esc close ",
                Style::default().fg(DIM),
            ))
            .alignment(Alignment::Right),
        );
    let inner_h = area.height.saturating_sub(2) as usize;
    let len = app.events.len();
    let end = len - app.events_scroll.min(len.saturating_sub(1));
    let start = end.saturating_sub(inner_h);
    let lines: Vec<Line> = app
        .events
        .range(start..end)
        .map(|e| {
            Line::from(vec![
                Span::styled(format!("{:>8}  ", ago(e.at)), Style::default().fg(DIM)),
                Span::styled(format!("{:<9} ", e.typ), Style::default().fg(DIM)),
                Span::styled(
                    format!("{:<24} ", e.action),
                    Style::default().fg(event_action_color(&e.action)),
                ),
                Span::raw(e.name.clone()),
            ])
        })
        .collect();
    if lines.is_empty() {
        f.render_widget(
            Paragraph::new("no docker events yet").style(Style::default().fg(DIM)).block(block),
            area,
        );
    } else {
        f.render_widget(Paragraph::new(Text::from(lines)).block(block), area);
        let scrollbar_area = Rect {
            y: area.y.saturating_add(1),
            height: area.height.saturating_sub(2),
            ..area
        };
        render_scrollbar(f, scrollbar_area, len, inner_h, start);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    fn rendered_app() -> App {
        let (tx, rx) = std::sync::mpsc::channel();
        std::mem::forget(rx);
        App::new(crate::docker::Docker::dummy(), tx)
    }

    #[test]
    fn table_window_keeps_selection_visible_and_caps_rendered_rows() {
        assert_eq!(table_window(100, 10, 0, 0), 0..10);
        assert_eq!(table_window(100, 10, 25, 0), 16..26);
        assert_eq!(table_window(100, 10, 18, 16), 16..26);
        assert_eq!(table_window(100, 10, 4, 16), 4..14);
        assert_eq!(table_window(3, 10, 2, 0), 0..3);
        assert_eq!(table_window(3, 0, 2, 0), 0..0);
    }

    #[test]
    fn click_expands_collapsed_section_and_overflow_draws_scrollbar() {
        let mut app = rendered_app();
        app.images = (0..100)
            .map(|i| crate::app::ImageRow {
                id: format!("i{i}"),
                tag: format!("image:{i}"),
                size: 0,
                created: i,
                containers: 0,
            })
            .collect();
        let mut terminal = Terminal::new(TestBackend::new(120, 40)).unwrap();

        terminal.draw(|f| draw(f, &mut app)).unwrap();
        let collapsed = app.layout.panels[Panel::Images as usize];
        assert_eq!(collapsed.height, COLLAPSED_PANEL_H);
        assert!(terminal.backend().buffer().content().iter().any(|c| c.symbol() == "█"));

        app.on_mouse(crossterm::event::MouseEvent {
            kind: crossterm::event::MouseEventKind::Down(crossterm::event::MouseButton::Left),
            column: collapsed.x + 2,
            row: collapsed.y + 2,
            modifiers: crossterm::event::KeyModifiers::NONE,
        });
        terminal.draw(|f| draw(f, &mut app)).unwrap();

        assert_eq!(app.panel, Panel::Images);
        assert!(app.layout.panels[Panel::Images as usize].height > COLLAPSED_PANEL_H);
        assert_eq!(app.layout.panels[Panel::Containers as usize].height, COLLAPSED_PANEL_H);
    }

    #[test]
    fn stats_dashboard_shows_capacity_rates_and_processes() {
        let mut app = rendered_app();
        app.containers.push(ContainerRow {
            id: "c1".into(),
            name: "api".into(),
            image: "api:latest".into(),
            image_id: String::new(),
            state: RowState::Running,
            status: "Up".into(),
            ports: "8080→8080/tcp".into(),
            created: 0,
            compose_project: None,
            compose_service: None,
            compose_files: String::new(),
            compose_dir: String::new(),
            volumes: Vec::new(),
            networks: Vec::new(),
        });
        app.apply(crate::app::AppEvent::Stat(crate::app::StatSample {
            id: "c1".into(),
            cpu_pct: 235.5,
            cpu_cores: 4,
            mem_pct: 25.0,
            mem_used: 256 * 1024 * 1024,
            mem_limit: 1024 * 1024 * 1024,
            rx: 10 * 1024,
            tx: 20 * 1024,
            rx_rate: 1024,
            tx_rate: 2048,
            pids: 42,
        }));
        let mut terminal = Terminal::new(TestBackend::new(90, 24)).unwrap();

        terminal.draw(|f| draw_stats(f, &app, f.area())).unwrap();

        let text = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(text.contains("CPU · 4 cores"));
        assert!(text.contains("1.0KiB/s"));
        assert!(text.contains("2.0KiB/s"));
        assert!(text.contains("42 pids"));
    }

    #[test]
    fn human_rate_uses_readable_binary_units() {
        assert_eq!(human_rate(0), "0B/s");
        assert_eq!(human_rate(1536), "1.5KiB/s");
    }

    #[test]
    fn panel_title_with_and_without_marks() {
        assert_eq!(panel_title(3, "Images", 12, 12, 0, "↓created"), " [3] Images (12) ↓created ");
        assert_eq!(
            panel_title(3, "Images", 12, 12, 4, "↓created"),
            " [3] Images (12 · 4✓) ↓created "
        );
    }

    #[test]
    fn panel_title_shows_filtered_counts() {
        assert_eq!(panel_title(3, "Images", 4, 12, 0, "↑tag"), " [3] Images (4/12) ↑tag ");
        assert_eq!(panel_title(3, "Images", 4, 12, 2, "↑tag"), " [3] Images (4/12 · 2✓) ↑tag ");
    }

    #[test]
    fn mask_env_hides_values_only() {
        assert_eq!(mask_env("SECRET=hunter2"), "SECRET=•••");
        assert_eq!(mask_env("A=B=C"), "A=•••");
        assert_eq!(mask_env("NOVALUE"), "NOVALUE");
    }

    #[test]
    fn log_line_style_matches_levels() {
        assert_eq!(log_line_style("ERROR: boom").fg, Some(Color::Red));
        assert_eq!(log_line_style("fatal crash").fg, Some(Color::Red));
        assert_eq!(log_line_style("thread panicked").fg, Some(Color::Red));
        assert_eq!(log_line_style("WARN slow query").fg, Some(Color::Yellow));
        assert_eq!(log_line_style("DEBUG noise").fg, Some(DIM));
        assert_eq!(log_line_style("plain info line").fg, None);
    }

    #[test]
    fn centered_never_exceeds_area() {
        let area = Rect::new(0, 0, 20, 10);
        let r = centered(area, 100, 100);
        assert!(r.width <= area.width && r.height <= area.height);
        let r2 = centered(area, 10, 4);
        assert_eq!((r2.x, r2.y, r2.width, r2.height), (5, 3, 10, 4));
    }

    #[test]
    fn mark_span_content() {
        assert_eq!(mark_span(true).content, "✓ ");
        assert_eq!(mark_span(false).content, "");
    }

    #[test]
    fn event_action_colors() {
        assert_eq!(event_action_color("die"), Color::Red);
        assert_eq!(event_action_color("oom"), Color::Red);
        assert_eq!(event_action_color("health_status: unhealthy"), Color::Red);
        assert_eq!(event_action_color("health_status: healthy"), Color::Green);
        assert_eq!(event_action_color("start"), Color::Green);
        assert_eq!(event_action_color("stop"), Color::Yellow);
        assert_eq!(event_action_color("attach"), DIM);
    }
}
