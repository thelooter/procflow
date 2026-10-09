//! Drawing the interactive view. All state lives in [`App`].

use crate::fmt::{self, human_bytes};
use crate::tui::{App, Choice, Focus, LiveStatus, Row, Window, CHOICES};
use procflow_ipc::v1::GroupBy;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Cell, Paragraph, Sparkline, Table};
use ratatui::Frame;

// Tokyo Night. Only foregrounds are set, so the terminal keeps its own
// background.
const TEXT: Color = Color::Rgb(192, 202, 245);
const MUTED: Color = Color::Rgb(120, 130, 172);
const FAINT: Color = Color::Rgb(72, 80, 115);
const ACCENT: Color = Color::Rgb(125, 207, 255);
const TEAL: Color = Color::Rgb(42, 195, 222);
const INGRESS: Color = Color::Rgb(122, 162, 247);
const EGRESS: Color = Color::Rgb(255, 158, 100);
const GOOD: Color = Color::Rgb(158, 206, 106);
const WARN: Color = Color::Rgb(224, 175, 104);
const BAD: Color = Color::Rgb(247, 118, 142);
const SELECTION: Color = Color::Rgb(27, 110, 110);
/// Text on [`SELECTION`]. Spelled out because ANSI "white" is a light grey
/// in most terminal palettes.
const SELECTED_TEXT: Color = Color::Rgb(255, 255, 255);

/// Cells the table's trend sparkline or share gauge takes.
const TREND_WIDTH: usize = 16;

fn fg(color: Color) -> Style {
    Style::new().fg(color)
}

fn bold(color: Color) -> Style {
    fg(color).add_modifier(Modifier::BOLD)
}

/// `values` as one line of bars, newest at the right edge of `width`.
pub fn spark(values: &[u64], width: usize) -> String {
    const BARS: [char; 8] = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];
    let shown = &values[values.len().saturating_sub(width)..];
    let max = shown.iter().copied().max().unwrap_or(0).max(1);
    let bars = shown
        .iter()
        .map(|value| BARS[(value * 7).div_ceil(max).min(7) as usize]);
    std::iter::repeat_n(' ', width - shown.len())
        .chain(bars)
        .collect()
}

/// `part` of `whole` as a gauge `width` cells wide. Anything above zero
/// fills at least one cell.
fn gauge(part: u64, whole: u64, width: usize) -> String {
    let exact = part as f64 * width as f64 / whole.max(1) as f64;
    let filled = (exact.round() as usize)
        .max(usize::from(part > 0))
        .min(width);
    "▰".repeat(filled) + &"▱".repeat(width - filled)
}

fn rate(bytes_per_second: u64) -> String {
    format!("{}/s", human_bytes(bytes_per_second))
}

fn pane(title: &str, focused: bool) -> Block<'_> {
    let title_style = if focused { bold(ACCENT) } else { fg(MUTED) };
    Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(fg(if focused { MUTED } else { FAINT }))
        .title(Span::styled(format!(" {title} "), title_style))
}

/// A `[ key label ]` hint for a pane's border.
fn hint(key: &str, label: &str) -> Vec<Span<'static>> {
    vec![
        Span::styled("[ ", fg(FAINT)),
        Span::styled(key.to_string(), bold(ACCENT)),
        Span::styled(format!(" {label} ] "), fg(MUTED)),
    ]
}

fn group_label(group: GroupBy) -> &'static str {
    match group {
        GroupBy::Project => "project",
        GroupBy::Exe => "exe",
        GroupBy::User => "user",
        GroupBy::Identity | GroupBy::Unspecified => "identity",
    }
}

pub fn draw(frame: &mut Frame, app: &mut App) {
    let [header, body, filter, status] = Layout::vertical([
        Constraint::Length(3),
        Constraint::Min(8),
        Constraint::Length(3),
        Constraint::Length(1),
    ])
    .areas(frame.area());
    draw_header(frame, app, header);

    // Narrow terminals drop the detail pane first, then the sidebar.
    let sidebar_width = if body.width >= 84 { 22 } else { 0 };
    let detail_width = if body.width >= 124 { 38 } else { 0 };
    let [sidebar, table, detail] = Layout::horizontal([
        Constraint::Length(sidebar_width),
        Constraint::Min(40),
        Constraint::Length(detail_width),
    ])
    .areas(body);
    if sidebar.width > 0 {
        draw_sidebar(frame, app, sidebar);
    }
    draw_table(frame, app, table);
    if detail.width > 0 {
        draw_detail(frame, app, detail);
    }
    draw_filter(frame, app, filter);
    draw_status(frame, app, status);
}

fn draw_header(frame: &mut Frame, app: &App, area: Rect) {
    let [facts, gauges, rule] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
    ])
    .areas(area);
    let divider = || Span::styled("  │  ", fg(FAINT));
    let fact = |label: &str, value: String, color: Color| {
        vec![
            Span::styled(format!("{label} "), fg(MUTED)),
            Span::styled(value, bold(color)),
        ]
    };

    let (collector, collector_color) = match &app.live {
        LiveStatus::Live => ("live", GOOD),
        LiveStatus::Connecting => ("connecting", WARN),
        LiveStatus::Down(_) => ("down", BAD),
    };
    let mut left = vec![Span::styled(" ⇅ procflow", bold(ACCENT)), divider()];
    left.extend(fact("daemon", app.daemon.daemon_version.clone(), TEXT));
    left.push(divider());
    left.extend(fact("collector", collector.to_string(), collector_color));
    let mut right = fact("window", app.window.label().to_lowercase(), WARN);
    right.push(divider());
    right.extend(fact("scope", fmt::scope(app.scope).to_string(), WARN));
    right.push(divider());
    right.extend(fact("by", group_label(app.group).to_string(), WARN));
    right.push(Span::raw(" "));
    frame.render_widget(Line::from(left), facts);
    frame.render_widget(Line::from(right).right_aligned(), facts);

    let mut line = vec![Span::raw(" ")];
    match &app.live {
        LiveStatus::Down(reason) => {
            line.push(Span::styled(format!("no live traffic: {reason}"), fg(WARN)))
        }
        _ => {
            let samples = |pick: fn(&(u64, u64)) -> u64| {
                app.total.samples.iter().map(pick).collect::<Vec<_>>()
            };
            for (label, color, now, trend) in [
                ("▼ IN ", INGRESS, app.total.ingress, samples(|s| s.0)),
                ("▲ OUT", EGRESS, app.total.egress, samples(|s| s.1)),
            ] {
                line.push(Span::styled(format!("{label} "), fg(MUTED)));
                line.push(Span::styled(spark(&trend, 24), fg(color)));
                line.push(Span::styled(format!(" {:<14}", rate(now)), bold(color)));
            }
        }
    }
    let today = vec![
        Span::styled("TODAY ", fg(MUTED)),
        Span::styled(format!("▼ {}", human_bytes(app.today.0)), bold(INGRESS)),
        Span::styled(format!("  ▲ {} ", human_bytes(app.today.1)), bold(EGRESS)),
    ];
    frame.render_widget(Line::from(line), gauges);
    frame.render_widget(Line::from(today).right_aligned(), gauges);
    frame.render_widget(
        Span::styled("╌".repeat(rule.width as usize), fg(FAINT)),
        rule,
    );
}

fn draw_sidebar(frame: &mut Frame, app: &App, area: Rect) {
    let focused = app.focus == Focus::Sidebar;
    let block = pane("Views", focused);
    let width = block.inner(area).width as usize;
    let mut lines = Vec::new();
    for (index, choice) in CHOICES.iter().enumerate() {
        let (heading, label, active, note) = match *choice {
            Choice::Window(window) => {
                let note = match (window, app.poll_seconds()) {
                    (Window::Live, Some(seconds)) => format!("{seconds:.0}s"),
                    _ => String::new(),
                };
                (
                    "WINDOW",
                    window.label().to_string(),
                    app.window == window,
                    note,
                )
            }
            Choice::Group(group) => {
                let mut label = group_label(group).to_string();
                label[..1].make_ascii_uppercase();
                let size = app.group_sizes[group as usize - 1];
                ("GROUP BY", label, app.group == group, size.to_string())
            }
            Choice::Scope(scope) => {
                let mut label = fmt::scope(scope).to_string();
                label[..1].make_ascii_uppercase();
                ("SCOPE", label, app.scope == scope, String::new())
            }
        };
        let first_of_kind = index == 0
            || std::mem::discriminant(&CHOICES[index - 1]) != std::mem::discriminant(choice);
        if first_of_kind {
            if index > 0 {
                lines.push(Line::default());
            }
            lines.push(Line::styled(format!(" ▾ {heading}"), bold(FAINT)));
        }
        let marker = if active { "▸" } else { " " };
        let text = format!("  {marker} {label}");
        let pad = width.saturating_sub(text.chars().count() + note.chars().count() + 1);
        let mut style = if active { bold(TEXT) } else { fg(MUTED) };
        let mut note_style = fg(if active { WARN } else { FAINT });
        if focused && index == app.sidebar {
            style = style.bg(SELECTION).fg(SELECTED_TEXT);
            note_style = note_style.bg(SELECTION);
        }
        lines.push(Line::from(vec![
            Span::styled(format!("{text}{}", " ".repeat(pad)), style),
            Span::styled(format!("{note} "), note_style),
        ]));
    }
    frame.render_widget(Paragraph::new(lines).block(block), area);
}

fn draw_table(frame: &mut Frame, app: &mut App, area: Rect) {
    let live = app.window == Window::Live;
    let grouped = !matches!(app.group, GroupBy::Identity | GroupBy::Unspecified);
    let title = format!("Top talkers · {}", app.window.label().to_lowercase());
    let mut hints = hint("/", "filter");
    hints.extend(hint("o", &format!("sort {}", app.sort.label())));
    let block =
        pane(&title, app.focus == Focus::Table).title_top(Line::from(hints).right_aligned());

    if app.rows.is_empty() {
        let message = match (&app.live, live, app.filter.is_empty()) {
            (_, _, false) => "nothing matches the filter, esc clears it".to_string(),
            (LiveStatus::Down(reason), true, _) => format!("no live traffic: {reason}"),
            (LiveStatus::Connecting, true, _) => "waiting for the first poll interval…".to_string(),
            (_, true, _) => "no traffic in this scope yet".to_string(),
            (_, false, _) => "no traffic recorded in this window".to_string(),
        };
        let inner = block.inner(area);
        frame.render_widget(block, area);
        let [_, middle, _] = Layout::vertical([
            Constraint::Fill(1),
            Constraint::Length(1),
            Constraint::Fill(1),
        ])
        .areas(inner);
        frame.render_widget(Line::styled(message, fg(MUTED)).centered(), middle);
        return;
    }

    // Columns give way as the pane narrows: session totals first, then the
    // third descriptive column, the trend, and the second column.
    let room = area.width.saturating_sub(2);
    let (show_second, show_trend, show_third) = (room >= 60, room >= 76, room >= 100);
    let show_session = live && room >= 124;
    let number_width = if live { 12 } else { 10 };

    let name_header = if grouped {
        group_label(app.group).to_uppercase()
    } else {
        "NAME".to_string()
    };
    let (second, third) = match app.group {
        GroupBy::Project | GroupBy::Exe => ("PATH", "MEMBERS"),
        GroupBy::User => ("UID", "MEMBERS"),
        GroupBy::Identity | GroupBy::Unspecified => ("PROJECT", "USER"),
    };
    let right = |text: &str| Cell::from(Line::from(text.to_string()).right_aligned());
    // A group's second column is a path, which wants the room more than
    // its short name does.
    let (name_fill, second_fill) = if grouped { (2, 3) } else { (3, 2) };
    let mut header = vec![right("#"), Cell::from(name_header)];
    let mut widths = vec![Constraint::Length(3), Constraint::Fill(name_fill)];
    let mut column = |show: bool, cell: Cell<'static>, width: Constraint| {
        if show {
            header.push(cell);
            widths.push(width);
        }
    };
    column(
        show_second,
        Cell::from(second),
        Constraint::Fill(second_fill),
    );
    column(show_third, Cell::from(third), Constraint::Length(13));
    let (ingress, egress) = if live {
        ("▼ IN/s", "▲ OUT/s")
    } else {
        ("▼ INGRESS", "▲ EGRESS")
    };
    column(true, right(ingress), Constraint::Length(number_width));
    column(true, right(egress), Constraint::Length(number_width));
    column(show_session, right("▼ SESSION"), Constraint::Length(10));
    column(show_session, right("▲ SESSION"), Constraint::Length(10));
    column(
        show_trend,
        Cell::from(if live { "TREND" } else { "SHARE" }),
        Constraint::Length(TREND_WIDTH as u16),
    );

    let sort = app.sort;
    let biggest = app
        .rows
        .iter()
        .map(|row| sort.key(row.ingress, row.egress))
        .max()
        .unwrap_or(0);
    let rows = app.rows.iter().enumerate().map(|(index, row)| {
        let idle = live && row.ingress == 0 && row.egress == 0;
        let tone = |color: Color| fg(if idle { FAINT } else { color });
        let number = |value: String, color: Color| {
            Cell::from(Line::styled(value, tone(color)).right_aligned())
        };
        let text =
            |value: &str, color: Color| Cell::from(Span::styled(value.to_string(), tone(color)));
        let bytes = |value: u64| {
            if live {
                rate(value)
            } else {
                human_bytes(value)
            }
        };
        let last = if live {
            let trend: Vec<u64> = row.samples.iter().map(|s| sort.key(s.0, s.1)).collect();
            spark(&trend, TREND_WIDTH)
        } else {
            gauge(sort.key(row.ingress, row.egress), biggest, TREND_WIDTH)
        };
        let cells = [
            (true, number((index + 1).to_string(), FAINT)),
            (
                true,
                text(&row.name, if row.unresolved { WARN } else { TEXT }),
            ),
            (show_second, text(&row.second, MUTED)),
            (show_third, text(&row.third, MUTED)),
            (true, number(bytes(row.ingress), INGRESS)),
            (true, number(bytes(row.egress), EGRESS)),
            (show_session, number(human_bytes(row.session.0), MUTED)),
            (show_session, number(human_bytes(row.session.1), MUTED)),
            (show_trend, text(&last, TEAL)),
        ];
        ratatui::widgets::Row::new(
            cells
                .into_iter()
                .filter(|(show, _)| *show)
                .map(|(_, cell)| cell),
        )
    });

    let table = Table::new(rows, widths)
        .header(ratatui::widgets::Row::new(header).style(bold(MUTED)))
        .column_spacing(2)
        .row_highlight_style(
            Style::new()
                .bg(SELECTION)
                .fg(SELECTED_TEXT)
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol("▌")
        .block(block);
    frame.render_stateful_widget(table, area, &mut app.table);
}

fn draw_detail(frame: &mut Frame, app: &App, area: Rect) {
    let [detail, keys] = Layout::vertical([Constraint::Min(6), Constraint::Length(10)]).areas(area);

    let title = match app.selected() {
        Some(row) if row.identity.is_some() => "Identity".to_string(),
        Some(_) => {
            let mut label = group_label(app.group).to_string();
            label[..1].make_ascii_uppercase();
            label
        }
        None => "Identity".to_string(),
    };
    let block = pane(&title, false);
    let inner = block.inner(detail);
    frame.render_widget(block, detail);
    if let Some(row) = app.selected() {
        draw_selection(frame, app, row, inner);
    }

    let block = pane("Keys", false);
    let lines: Vec<Line> = [
        ("w", "Window"),
        ("g", "Group by"),
        ("s", "Scope"),
        ("o", "Sort column"),
        ("/", "Filter"),
        ("tab", "Switch pane"),
        ("r", "Refresh history"),
        ("q", "Quit"),
    ]
    .iter()
    .map(|(key, action)| {
        Line::from(vec![
            Span::styled(" [", fg(FAINT)),
            Span::styled(*key, bold(ACCENT)),
            Span::styled("] ", fg(FAINT)),
            Span::styled(*action, fg(TEXT)),
        ])
    })
    .collect();
    frame.render_widget(Paragraph::new(lines).block(block), keys);
}

/// The detail pane's contents for the selected row.
fn draw_selection(frame: &mut Frame, app: &App, row: &Row, area: Rect) {
    let width = area.width.saturating_sub(2) as usize;
    let field = |label: &str, value: &str| {
        Line::from(vec![
            Span::styled(format!(" {label:<9}"), fg(MUTED)),
            Span::styled(fmt::ellipsis(value, width.saturating_sub(9)), fg(TEXT)),
        ])
    };
    let mut lines = vec![Line::styled(
        format!(" {}", fmt::ellipsis(&row.name, width)),
        bold(ACCENT),
    )];
    match row.identity.and_then(|id| app.identities.get(&id)) {
        Some(identity) => {
            lines.push(Line::styled(
                format!(" {}", fmt::ellipsis(&identity.normalized_cmdline, width)),
                fg(MUTED),
            ));
            lines.push(Line::default());
            lines.push(field("user", &fmt::user(identity)));
            lines.push(field("project", &fmt::tilde(&identity.project_root)));
            lines.push(field("exe", &fmt::tilde(&identity.exe)));
            lines.push(field("unit", fmt::basename(&identity.unit_or_cgroup)));
            if row.unresolved {
                lines.push(Line::styled(" exited before it could be read", fg(WARN)));
            }
        }
        None => {
            lines.push(Line::styled(
                format!(" {}", fmt::ellipsis(&row.second, width)),
                fg(MUTED),
            ));
            lines.push(Line::default());
            for (name, ingress, egress) in row.members.iter().take(6) {
                lines.push(Line::from(vec![
                    Span::styled(format!(" {:<12}", fmt::ellipsis(name, 12)), fg(TEXT)),
                    Span::styled(format!("{:>11}", human_bytes(*ingress)), fg(INGRESS)),
                    Span::styled(format!("{:>11}", human_bytes(*egress)), fg(EGRESS)),
                ]));
            }
        }
    }
    let text_height = lines.len() as u16 + 1;
    frame.render_widget(Paragraph::new(lines), area);

    // Two charts under the text: live samples, or the stored series.
    let live = app.window == Window::Live;
    let (ingress, egress, captions): (Vec<u64>, Vec<u64>, [String; 2]) = if live {
        let session =
            [row.session.0, row.session.1].map(|bytes| format!("total {}", human_bytes(bytes)));
        (
            row.samples.iter().map(|s| s.0).collect(),
            row.samples.iter().map(|s| s.1).collect(),
            session,
        )
    } else if row.identity.is_some() {
        let per_tier = format!("per {}", app.history.tier);
        (
            app.history.ingress.clone(),
            app.history.egress.clone(),
            [per_tier.clone(), per_tier],
        )
    } else {
        return;
    };
    let charts = Rect {
        y: area.y + text_height,
        height: area.height.saturating_sub(text_height),
        ..area
    };
    if charts.height < 5 {
        return;
    }
    // A heading and a chart per direction; the charts grow with the pane.
    let chart_height = ((charts.height - 3) / 2).min(4);
    let rows = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(chart_height),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(chart_height),
    ])
    .split(charts);
    let unit = if live { "/s" } else { "" };
    for (slot, (label, color, values, now)) in [
        ("▼ in", INGRESS, &ingress, row.ingress),
        ("▲ out", EGRESS, &egress, row.egress),
    ]
    .into_iter()
    .enumerate()
    {
        let heading = Line::from(vec![
            Span::styled(format!(" {label:<6}"), fg(MUTED)),
            Span::styled(format!("{}{unit}", human_bytes(now)), bold(color)),
        ]);
        let caption =
            Line::from(Span::styled(format!("{} ", captions[slot]), fg(FAINT))).right_aligned();
        frame.render_widget(caption, rows[slot * 3]);
        frame.render_widget(heading, rows[slot * 3]);
        let chart = Rect {
            x: area.x + 1,
            width: area.width.saturating_sub(2),
            ..rows[slot * 3 + 1]
        };
        // The widget draws from the left and cuts what does not fit. Keep
        // the newest samples and push them to the right edge, like the
        // table's trend column.
        let width = chart.width as usize;
        let shown = &values[values.len().saturating_sub(width)..];
        let padded = std::iter::repeat_n(0, width - shown.len()).chain(shown.iter().copied());
        frame.render_widget(Sparkline::default().data(padded).style(fg(color)), chart);
    }
}

fn draw_filter(frame: &mut Frame, app: &App, area: Rect) {
    let block =
        pane("Filter", app.filtering).title_top(Line::from(hint("esc", "clear")).right_aligned());
    let line = if app.filtering || !app.filter.is_empty() {
        let cursor = if app.filtering { "▏" } else { "" };
        Line::from(vec![
            Span::styled(" ❯ ", bold(ACCENT)),
            Span::styled(app.filter.clone(), fg(TEXT)),
            Span::styled(cursor, fg(ACCENT)),
        ])
    } else {
        Line::from(vec![
            Span::styled(" ❯ ", fg(FAINT)),
            Span::styled("press / to filter by name, project or user", fg(FAINT)),
        ])
    };
    frame.render_widget(Paragraph::new(line).block(block), area);
}

fn draw_status(frame: &mut Frame, app: &App, area: Rect) {
    let (dot, text) = match (&app.error, &app.live, app.window) {
        (Some(error), _, _) => (BAD, format!("daemon unreachable: {error}")),
        (None, LiveStatus::Down(reason), Window::Live) => {
            (WARN, format!("no live traffic: {reason}"))
        }
        (None, _, Window::Live) => {
            let poll = app
                .poll_seconds()
                .map_or("waiting for the daemon".to_string(), |s| {
                    format!("{s:.0}s polls")
                });
            (GOOD, format!("live · {poll} · {} rows", app.rows.len()))
        }
        (None, _, window) => (
            TEAL,
            format!(
                "history · {} · {} rows",
                window.label().to_lowercase(),
                app.rows.len()
            ),
        ),
    };
    let left = Line::from(vec![
        Span::styled(" ● ", fg(dot)),
        Span::styled(text, fg(MUTED)),
    ]);
    let keys = "tab panes · ↑↓ move · enter apply · / filter · q quit ";
    // The key hints give way to a long message. They are styled per span:
    // a style on the line itself would repaint the whole row.
    if left.width() + keys.chars().count() < area.width as usize {
        let hints = Line::from(Span::styled(keys, fg(FAINT))).right_aligned();
        frame.render_widget(hints, area);
    }
    frame.render_widget(left, area);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::tests::app;
    use ratatui::backend::TestBackend;
    use ratatui::crossterm::event::{KeyCode, KeyEvent};
    use ratatui::Terminal;

    /// The frame as text, one line per row.
    fn render(app: &mut App, width: u16, height: u16) -> String {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal.draw(|frame| draw(frame, app)).unwrap();
        let buffer = terminal.backend().buffer();
        let lines: Vec<String> = (0..height)
            .map(|y| {
                (0..width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
                    .trim_end()
                    .to_string()
            })
            .collect();
        lines.join("\n")
    }

    #[test]
    fn sparklines_and_gauges_scale_to_their_width() {
        // Anything above zero stands clear of the baseline.
        assert_eq!(spark(&[0, 1, 4, 8], 6), "  ▁▂▅█");
        assert_eq!(spark(&[1, 2, 3, 4, 8], 2), "▅█");
        assert_eq!(spark(&[], 3), "   ");
        assert_eq!(gauge(1, 2, 4), "▰▰▱▱");
        assert_eq!(gauge(95, 100, 16), "▰▰▰▰▰▰▰▰▰▰▰▰▰▰▰▱");
        assert_eq!(gauge(1, 1000, 4), "▰▱▱▱");
        assert_eq!(gauge(0, 0, 3), "▱▱▱");
    }

    #[test]
    fn the_live_view_renders_every_pane() {
        let mut app = app(30);
        let screen = render(&mut app, 150, 36);
        println!("{screen}");
        for expected in [
            "⇅ procflow",
            "collector live",
            "▾ WINDOW",
            "▸ Live",
            "▸ Identity",
            "Top talkers · live",
            "firefox",
            "1.9 MiB/s",
            "Identity",
            "/usr/bin/firefox",
            "[w] Window",
            "press / to filter",
            "live · 1s polls · 2 rows",
        ] {
            assert!(screen.contains(expected), "missing {expected:?}");
        }
        assert!(!screen.contains("postgres"), "loopback traffic is opt-in");
    }

    #[test]
    fn a_history_window_renders_totals_and_narrow_terminals_drop_panes() {
        let mut app = app(1);
        for key in ['w', 'w', 'g'] {
            app.on_key(KeyEvent::from(KeyCode::Char(key)));
            app.tick();
        }
        let screen = render(&mut app, 190, 36);
        println!("{screen}");
        assert!(
            screen.contains("2.0 MiB/s"),
            "the header keeps showing live rates"
        );
        for expected in [
            "Top talkers · today",
            "▼ INGRESS",
            "3.0 GiB",
            "SHARE",
            "▰",
            "PROJECT",
            "1 identity",
        ] {
            assert!(screen.contains(expected), "missing {expected:?}");
        }

        let narrow = render(&mut app, 70, 20);
        assert!(narrow.contains("Top talkers · today"));
        assert!(!narrow.contains("▾ WINDOW") && !narrow.contains("[w] Window"));
    }
}
