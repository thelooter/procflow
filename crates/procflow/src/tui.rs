//! The interactive view: state, input and data flow. Drawing is in `ui`.
//!
//! Live traffic arrives as `Watch` chunks on a background thread; history
//! comes from `TopIdentities` and `Series` snapshots. Both are fetched per
//! Identity and for every scope, then grouped, scoped, filtered and sorted
//! here, so switching between those never waits on the daemon.

use crate::{client, fmt, ui};
use anyhow::{Context, Result};
use chrono::{DateTime, Datelike, Local};
use procflow_ipc::scope_matches;
use procflow_ipc::v1::{
    request, Chunk, CounterRow, GroupBy, HelloOk, Identity, Rows, Scope, Series, TimeRange,
    TopIdentities, Watch,
};
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::widgets::TableState;
use ratatui::DefaultTerminal;
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, Instant};

/// Poll intervals remembered per flow: the length of a trend sparkline.
pub const TREND_TICKS: usize = 60;
/// Poll intervals a shown rate is averaged over, so the ranking does not
/// reshuffle on every single one.
const RATE_TICKS: usize = 3;
/// How often history is fetched again while it is on screen.
const REFRESH: Duration = Duration::from_secs(10);
const RECONNECT: Duration = Duration::from_secs(2);

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Window {
    Live,
    Hour,
    Today,
    Week,
    Month,
    AllTime,
}

impl Window {
    pub fn label(self) -> &'static str {
        match self {
            Window::Live => "Live",
            Window::Hour => "Last hour",
            Window::Today => "Today",
            Window::Week => "Last 7 days",
            Window::Month => "This month",
            Window::AllTime => "All time",
        }
    }

    /// The stored range this window covers; `None` for the live view.
    fn range(self, now: DateTime<Local>) -> Option<TimeRange> {
        let now_ms = now.timestamp_millis();
        let today = now.date_naive();
        let from_unix_ms = match self {
            Window::Live => return None,
            Window::Hour => now_ms - 3_600_000,
            Window::Today => crate::local_midnight_ms(today),
            Window::Week => now_ms - 7 * 86_400_000,
            Window::Month => crate::local_midnight_ms(today.with_day(1).expect("day 1 exists")),
            Window::AllTime => 0,
        };
        Some(TimeRange {
            from_unix_ms,
            to_unix_ms: now_ms,
        })
    }
}

/// Which direction ranks the table. `Both` orders by the two added up.
/// That is an ordering only, and the columns stay separate (CONTEXT.md
/// "Direction").
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Sort {
    Both,
    Ingress,
    Egress,
}

impl Sort {
    pub fn label(self) -> &'static str {
        match self {
            Sort::Both => "▼▲",
            Sort::Ingress => "▼ in",
            Sort::Egress => "▲ out",
        }
    }

    pub fn key(self, ingress: u64, egress: u64) -> u64 {
        match self {
            Sort::Both => ingress.saturating_add(egress),
            Sort::Ingress => ingress,
            Sort::Egress => egress,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Focus {
    Sidebar,
    Table,
}

/// A setting the sidebar offers.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Choice {
    Window(Window),
    Group(GroupBy),
    Scope(Scope),
}

pub const CHOICES: [Choice; 13] = [
    Choice::Window(Window::Live),
    Choice::Window(Window::Hour),
    Choice::Window(Window::Today),
    Choice::Window(Window::Week),
    Choice::Window(Window::Month),
    Choice::Window(Window::AllTime),
    Choice::Group(GroupBy::Identity),
    Choice::Group(GroupBy::Project),
    Choice::Group(GroupBy::Exe),
    Choice::Group(GroupBy::User),
    Choice::Scope(Scope::External),
    Choice::Scope(Scope::Loopback),
    Choice::Scope(Scope::All),
];

pub enum LiveStatus {
    Connecting,
    Live,
    /// Why there is no live traffic to show.
    Down(String),
}

pub enum Feed {
    Chunk(Chunk),
    Down(String),
}

/// One Identity's live traffic in one scope.
#[derive(Default)]
struct Flow {
    /// `(ingress, egress)` bytes per poll interval, newest last.
    samples: VecDeque<(u64, u64)>,
    /// Bytes since the view opened.
    session: (u64, u64),
}

/// What one Identity, or several added up, did in the current window and
/// scope.
#[derive(Default, Clone)]
pub struct Measure {
    /// Bytes per second in the live view, bytes in a history window.
    pub ingress: u64,
    pub egress: u64,
    session: (u64, u64),
    /// Live only: bytes per second for each remembered poll interval.
    pub samples: Vec<(u64, u64)>,
}

impl Measure {
    fn add(&mut self, other: &Measure) {
        self.ingress += other.ingress;
        self.egress += other.egress;
        self.session.0 += other.session.0;
        self.session.1 += other.session.1;
        if self.samples.len() < other.samples.len() {
            self.samples.resize(other.samples.len(), (0, 0));
        }
        for (sum, sample) in self.samples.iter_mut().zip(&other.samples) {
            sum.0 += sample.0;
            sum.1 += sample.1;
        }
    }
}

/// One table row: an Identity, or a group of them.
pub struct Row {
    /// Stable across refreshes, so the selection can follow the row.
    pub key: String,
    pub identity: Option<i64>,
    pub name: String,
    /// The two descriptive columns after the name.
    pub second: String,
    pub third: String,
    pub ingress: u64,
    pub egress: u64,
    pub session: (u64, u64),
    pub samples: Vec<(u64, u64)>,
    /// For a group: its Identities as `(name, ingress, egress)`, biggest first.
    pub members: Vec<(String, u64, u64)>,
    pub unresolved: bool,
    /// Everything the filter matches against, lowercased.
    haystack: String,
}

/// The selected Identity's history, for the detail pane.
#[derive(Default)]
pub struct History {
    pub ingress: Vec<u64>,
    pub egress: Vec<u64>,
    pub tier: &'static str,
}

/// Fetches one snapshot verb; a seam so tests run without a daemon.
pub type Rpc = fn(request::Body) -> Result<Rows>;

pub struct App {
    pub daemon: HelloOk,
    rpc: Rpc,
    feed: Receiver<Feed>,
    pub live: LiveStatus,
    pub identities: HashMap<i64, Identity>,
    flows: HashMap<(i64, Scope), Flow>,
    /// Length of each remembered poll interval, newest last.
    intervals: VecDeque<u32>,
    /// Per-Identity totals for the selected history window, all scopes.
    stored: Vec<CounterRow>,
    /// `(ingress, egress)` since local midnight, in the current scope.
    pub today: (u64, u64),
    /// The last history fetch's failure, until one succeeds.
    pub error: Option<String>,
    refreshed: Option<Instant>,
    pub history: History,
    history_for: Option<(i64, Window, Scope)>,

    pub window: Window,
    pub group: GroupBy,
    pub scope: Scope,
    pub sort: Sort,
    pub focus: Focus,
    pub filter: String,
    pub filtering: bool,
    /// Cursor position in [`CHOICES`].
    pub sidebar: usize,
    pub table: TableState,
    selected_key: Option<String>,

    /// Everything below is derived by `recompute`.
    pub rows: Vec<Row>,
    /// Rows each grouping would have, for the sidebar.
    pub group_sizes: [usize; 4],
    /// Live traffic of everything in scope.
    pub total: Measure,
}

impl App {
    pub fn new(
        daemon: HelloOk,
        rpc: Rpc,
        feed: Receiver<Feed>,
        scope: Scope,
        group: GroupBy,
    ) -> Self {
        App {
            daemon,
            rpc,
            feed,
            live: LiveStatus::Connecting,
            identities: HashMap::new(),
            flows: HashMap::new(),
            intervals: VecDeque::new(),
            stored: Vec::new(),
            today: (0, 0),
            error: None,
            refreshed: None,
            history: History::default(),
            history_for: None,
            window: Window::Live,
            group,
            scope,
            sort: Sort::Both,
            focus: Focus::Table,
            filter: String::new(),
            filtering: false,
            sidebar: 0,
            table: TableState::default(),
            selected_key: None,
            rows: Vec::new(),
            group_sizes: [0; 4],
            total: Measure::default(),
        }
    }

    /// Take in whatever arrived since the last frame and derive the view.
    pub fn tick(&mut self) {
        while let Ok(feed) = self.feed.try_recv() {
            match feed {
                Feed::Chunk(chunk) => self.on_chunk(chunk),
                Feed::Down(reason) => self.live = LiveStatus::Down(reason),
            }
        }
        if self.refreshed.is_none_or(|at| at.elapsed() >= REFRESH) {
            self.refresh();
        }
        self.recompute();
        self.fetch_history();
    }

    fn on_chunk(&mut self, chunk: Chunk) {
        self.live = LiveStatus::Live;
        let rows = chunk.rows.unwrap_or_default();
        self.identities.extend(
            rows.identities
                .into_iter()
                .map(|identity| (identity.id, identity)),
        );
        let mut deltas: HashMap<(i64, Scope), (u64, u64)> = HashMap::new();
        for counter in &rows.counters {
            let delta = deltas
                .entry((counter.identity_id, counter.scope()))
                .or_default();
            delta.0 += counter.ingress_bytes;
            delta.1 += counter.egress_bytes;
        }
        for key in deltas.keys() {
            self.flows.entry(*key).or_default();
        }
        // Every flow gets a sample per interval, so trends stay aligned and
        // a flow that went quiet shows it.
        for (key, flow) in &mut self.flows {
            let (ingress, egress) = deltas.get(key).copied().unwrap_or_default();
            flow.session.0 += ingress;
            flow.session.1 += egress;
            push_capped(&mut flow.samples, (ingress, egress));
        }
        push_capped(&mut self.intervals, chunk.interval_ms.max(1));
    }

    /// Fetch the stored totals on screen: today's, and the history window's.
    fn refresh(&mut self) {
        self.refreshed = Some(Instant::now());
        self.history_for = None;
        let now = Local::now();
        let top = |range: Option<TimeRange>, group_by: GroupBy| {
            (self.rpc)(request::Body::TopIdentities(TopIdentities {
                range,
                scope: Scope::All as i32,
                group_by: group_by as i32,
                ..Default::default()
            }))
        };
        let today = top(Window::Today.range(now), GroupBy::User);
        let stored = match self.window.range(now) {
            Some(range) => top(Some(range), GroupBy::Identity),
            None => Ok(Rows::default()),
        };
        match (today, stored) {
            (Ok(today), Ok(stored)) => {
                self.error = None;
                self.today = today
                    .counters
                    .iter()
                    .filter(|c| scope_matches(self.scope, c.scope()))
                    .fold((0, 0), |sum, c| {
                        (sum.0 + c.ingress_bytes, sum.1 + c.egress_bytes)
                    });
                self.identities.extend(
                    stored
                        .identities
                        .into_iter()
                        .map(|identity| (identity.id, identity)),
                );
                self.stored = stored.counters;
            }
            (Err(e), _) | (_, Err(e)) => self.error = Some(format!("{e:#}")),
        }
    }

    /// Fetch the selected Identity's series when the detail pane needs one.
    fn fetch_history(&mut self) {
        let selected = self.selected().and_then(|row| row.identity);
        let (Some(id), Some(range)) = (selected, self.window.range(Local::now())) else {
            return;
        };
        let wanted = Some((id, self.window, self.scope));
        if self.history_for == wanted {
            return;
        }
        self.history_for = wanted;
        let series = Series {
            identity_id: id,
            range: Some(range),
            scope: self.scope as i32,
            ..Default::default()
        };
        self.history = match (self.rpc)(request::Body::Series(series)) {
            Ok(rows) => {
                // With both scopes in view a bucket has two rows; the pane
                // draws one bar per bucket.
                let mut buckets: BTreeMap<i64, (u64, u64)> = BTreeMap::new();
                for counter in &rows.counters {
                    let bucket = buckets.entry(counter.bucket_unix_ms).or_default();
                    bucket.0 += counter.ingress_bytes;
                    bucket.1 += counter.egress_bytes;
                }
                History {
                    ingress: buckets.values().map(|b| b.0).collect(),
                    egress: buckets.values().map(|b| b.1).collect(),
                    tier: fmt::tier(rows.tier()),
                }
            }
            Err(e) => {
                self.error = Some(format!("{e:#}"));
                History::default()
            }
        };
    }

    /// Each Identity's stored totals for the history window, in scope.
    fn stored_measures(&self) -> HashMap<i64, Measure> {
        let mut measures: HashMap<i64, Measure> = HashMap::new();
        for counter in self
            .stored
            .iter()
            .filter(|c| scope_matches(self.scope, c.scope()))
        {
            let measure = measures.entry(counter.identity_id).or_default();
            measure.ingress += counter.ingress_bytes;
            measure.egress += counter.egress_bytes;
        }
        measures
    }

    /// Each Identity's live rates and trend, in scope.
    fn live_measures(&self) -> HashMap<i64, Measure> {
        let mut measures: HashMap<i64, Measure> = HashMap::new();
        let ticks = self.intervals.len();
        let recent_ms: u64 = self
            .intervals
            .iter()
            .rev()
            .take(RATE_TICKS)
            .map(|ms| *ms as u64)
            .sum();
        for ((id, scope), flow) in &self.flows {
            if !scope_matches(self.scope, *scope) {
                continue;
            }
            // A flow first seen mid-way has fewer samples than there were
            // intervals; align it to the newest.
            let mut samples = vec![(0, 0); ticks - flow.samples.len()];
            samples.extend(
                flow.samples
                    .iter()
                    .zip(self.intervals.iter().skip(ticks - flow.samples.len()))
                    .map(|((ingress, egress), ms)| {
                        (ingress * 1000 / *ms as u64, egress * 1000 / *ms as u64)
                    }),
            );
            let recent = flow
                .samples
                .iter()
                .rev()
                .take(RATE_TICKS)
                .fold((0, 0), |sum, s| (sum.0 + s.0, sum.1 + s.1));
            measures.entry(*id).or_default().add(&Measure {
                ingress: recent.0 * 1000 / recent_ms.max(1),
                egress: recent.1 * 1000 / recent_ms.max(1),
                session: flow.session,
                samples,
            });
        }
        measures
    }

    /// The row an Identity belongs to under `group`: `(key, name, second column)`.
    fn group_of(identity: &Identity, group: GroupBy) -> (String, String, String) {
        match group {
            GroupBy::Project => (
                identity.project_root.clone(),
                fmt::project(identity).to_string(),
                fmt::tilde(&identity.project_root),
            ),
            GroupBy::Exe => (
                identity.exe.clone(),
                fmt::basename(&identity.exe).to_string(),
                fmt::tilde(&identity.exe),
            ),
            GroupBy::User => (
                fmt::user(identity),
                fmt::user(identity),
                format!("uid {}", identity.uid),
            ),
            GroupBy::Identity | GroupBy::Unspecified => (
                identity.id.to_string(),
                fmt::name(identity).to_string(),
                fmt::project(identity).to_string(),
            ),
        }
    }

    fn recompute(&mut self) {
        // The header shows live traffic whichever window the table is on.
        let live = self.live_measures();
        self.total = Measure::default();
        for measure in live.values() {
            self.total.add(measure);
        }
        let measures = if self.window == Window::Live {
            live
        } else {
            self.stored_measures()
        };
        let grouped = !matches!(self.group, GroupBy::Identity | GroupBy::Unspecified);
        let mut rows: HashMap<String, Row> = HashMap::new();
        let mut keys: [std::collections::HashSet<String>; 4] = Default::default();
        for (id, measure) in &measures {
            let Some(identity) = self.identities.get(id) else {
                continue;
            };
            for (slot, group) in [
                GroupBy::Identity,
                GroupBy::Project,
                GroupBy::Exe,
                GroupBy::User,
            ]
            .into_iter()
            .enumerate()
            {
                keys[slot].insert(Self::group_of(identity, group).0);
            }
            let (key, name, second) = Self::group_of(identity, self.group);
            let row = rows.entry(key.clone()).or_insert_with(|| Row {
                key,
                identity: (!grouped).then_some(*id),
                name,
                second,
                third: if grouped {
                    String::new()
                } else {
                    fmt::user(identity)
                },
                ingress: 0,
                egress: 0,
                session: (0, 0),
                samples: Vec::new(),
                members: Vec::new(),
                unresolved: !grouped && identity.exe == "<unresolved>",
                haystack: String::new(),
            });
            // An Identity row matches on everything known about it, a group
            // on its key and its members' names.
            let searchable: &[&str] = if grouped {
                &[&row.key, fmt::name(identity)]
            } else {
                &[
                    fmt::name(identity),
                    &identity.project_root,
                    &identity.exe,
                    &identity.normalized_cmdline,
                    &row.third,
                ]
            };
            row.haystack
                .push_str(&format!("{}\n", searchable.join("\n").to_lowercase()));
            let mut sum = Measure {
                ingress: row.ingress,
                egress: row.egress,
                session: row.session,
                samples: std::mem::take(&mut row.samples),
            };
            sum.add(measure);
            (row.ingress, row.egress, row.session, row.samples) =
                (sum.ingress, sum.egress, sum.session, sum.samples);
            if grouped {
                row.members.push((
                    fmt::name(identity).to_string(),
                    measure.ingress,
                    measure.egress,
                ));
            }
        }
        self.group_sizes = keys.map(|keys| keys.len());

        let needle = self.filter.to_lowercase();
        let sort = self.sort;
        let mut rows: Vec<Row> = rows
            .into_values()
            .filter(|row| row.haystack.contains(&needle))
            .collect();
        for row in &mut rows {
            row.members.sort_by(|a, b| {
                sort.key(b.1, b.2)
                    .cmp(&sort.key(a.1, a.2))
                    .then_with(|| a.0.cmp(&b.0))
            });
            if grouped {
                row.third = match row.members.len() {
                    1 => "1 identity".to_string(),
                    n => format!("{n} identities"),
                };
            }
        }
        // Rate first, then what the row has moved this session, so rows
        // that went quiet keep a steady order instead of shuffling.
        rows.sort_by(|a, b| {
            sort.key(b.ingress, b.egress)
                .cmp(&sort.key(a.ingress, a.egress))
                .then_with(|| {
                    sort.key(b.session.0, b.session.1)
                        .cmp(&sort.key(a.session.0, a.session.1))
                })
                .then_with(|| a.name.cmp(&b.name))
                .then_with(|| a.key.cmp(&b.key))
        });
        self.rows = rows;

        // Keep the cursor on the same row as it moves through the ranking.
        let position = self
            .selected_key
            .as_ref()
            .and_then(|key| self.rows.iter().position(|row| &row.key == key));
        let index = position
            .unwrap_or(self.table.selected().unwrap_or(0))
            .min(self.rows.len().saturating_sub(1));
        self.select(index);
    }

    fn select(&mut self, index: usize) {
        self.table.select((!self.rows.is_empty()).then_some(index));
        self.selected_key = self.rows.get(index).map(|row| row.key.clone());
    }

    pub fn selected(&self) -> Option<&Row> {
        self.table.selected().and_then(|index| self.rows.get(index))
    }

    /// Seconds one poll interval lasts, once the first has arrived.
    pub fn poll_seconds(&self) -> Option<f64> {
        self.intervals.back().map(|ms| *ms as f64 / 1000.0)
    }

    fn choose(&mut self, choice: Choice) {
        match choice {
            Choice::Window(window) => self.window = window,
            Choice::Group(group) => self.group = group,
            Choice::Scope(scope) => self.scope = scope,
        }
        // A new window or scope changes the stored totals on screen.
        if !matches!(choice, Choice::Group(_)) {
            self.refreshed = None;
        }
        self.selected_key = None;
        self.table = TableState::default();
    }

    /// The choice after the current one of the same kind, wrapping around.
    fn cycle(&mut self, same_kind: fn(&Choice) -> bool) {
        let current = [
            Choice::Window(self.window),
            Choice::Group(self.group),
            Choice::Scope(self.scope),
        ];
        let options: Vec<Choice> = CHOICES.iter().copied().filter(same_kind).collect();
        let at = options
            .iter()
            .position(|option| current.contains(option))
            .unwrap_or(0);
        self.choose(options[(at + 1) % options.len()]);
    }

    fn step(&mut self, by: isize) {
        let (index, len) = match self.focus {
            Focus::Sidebar => (self.sidebar, CHOICES.len()),
            Focus::Table => (self.table.selected().unwrap_or(0), self.rows.len()),
        };
        let index = index.saturating_add_signed(by).min(len.saturating_sub(1));
        match self.focus {
            Focus::Sidebar => self.sidebar = index,
            Focus::Table => self.select(index),
        }
    }

    /// Handle a key press; true means quit.
    pub fn on_key(&mut self, key: KeyEvent) -> bool {
        if key.kind != KeyEventKind::Press {
            return false;
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            return true;
        }
        if self.filtering {
            match key.code {
                KeyCode::Esc => {
                    self.filter.clear();
                    self.filtering = false;
                }
                KeyCode::Enter => self.filtering = false,
                KeyCode::Backspace => {
                    self.filter.pop();
                }
                KeyCode::Char(c) => self.filter.push(c),
                _ => {}
            }
            return false;
        }
        match key.code {
            KeyCode::Char('q') => return true,
            KeyCode::Esc => self.filter.clear(),
            KeyCode::Char('/') => {
                self.filtering = true;
                self.focus = Focus::Table;
            }
            KeyCode::Tab
            | KeyCode::BackTab
            | KeyCode::Left
            | KeyCode::Right
            | KeyCode::Char('h' | 'l') => {
                self.focus = match self.focus {
                    Focus::Sidebar => Focus::Table,
                    Focus::Table => Focus::Sidebar,
                }
            }
            KeyCode::Down | KeyCode::Char('j') => self.step(1),
            KeyCode::Up | KeyCode::Char('k') => self.step(-1),
            KeyCode::PageDown => self.step(10),
            KeyCode::PageUp => self.step(-10),
            KeyCode::Home => self.step(isize::MIN),
            KeyCode::End => self.step(isize::MAX),
            KeyCode::Enter | KeyCode::Char(' ') if self.focus == Focus::Sidebar => {
                self.choose(CHOICES[self.sidebar])
            }
            KeyCode::Char('w') => self.cycle(|choice| matches!(choice, Choice::Window(_))),
            KeyCode::Char('g') => self.cycle(|choice| matches!(choice, Choice::Group(_))),
            KeyCode::Char('s') => self.cycle(|choice| matches!(choice, Choice::Scope(_))),
            KeyCode::Char('o') => {
                self.sort = match self.sort {
                    Sort::Both => Sort::Ingress,
                    Sort::Ingress => Sort::Egress,
                    Sort::Egress => Sort::Both,
                }
            }
            KeyCode::Char('r') => self.refreshed = None,
            _ => {}
        }
        false
    }

    fn run(&mut self, terminal: &mut DefaultTerminal) -> Result<()> {
        loop {
            self.tick();
            terminal.draw(|frame| ui::draw(frame, self))?;
            // Wake up often enough to show a new chunk promptly.
            if event::poll(Duration::from_millis(150))? {
                if let Event::Key(key) = event::read()? {
                    if self.on_key(key) {
                        return Ok(());
                    }
                }
            }
        }
    }
}

fn push_capped<T>(samples: &mut VecDeque<T>, sample: T) {
    if samples.len() == TREND_TICKS {
        samples.pop_front();
    }
    samples.push_back(sample);
}

/// Follow the daemon's live stream for as long as the view is open,
/// reconnecting if the daemon goes away.
fn spawn_feed() -> Receiver<Feed> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || loop {
        // Per Identity and in every scope: the view groups and scopes itself.
        let watch = Watch {
            scope: Scope::All as i32,
            ..Default::default()
        };
        let reason = match client::watch(watch) {
            Ok(stream) => {
                let mut reason = "the daemon ended the live stream".to_string();
                for chunk in stream {
                    match chunk {
                        Ok(chunk) => {
                            if tx.send(Feed::Chunk(chunk)).is_err() {
                                return; // the view closed
                            }
                        }
                        Err(e) => {
                            reason = format!("{e:#}");
                            break;
                        }
                    }
                }
                reason
            }
            Err(e) => format!("{e:#}"),
        };
        if tx.send(Feed::Down(reason)).is_err() {
            return;
        }
        std::thread::sleep(RECONNECT);
    });
    rx
}

pub fn run(scope: Scope, group: GroupBy) -> Result<()> {
    // Before the screen is taken over: a down daemon is a plain error.
    let daemon = client::hello()?;
    let mut app = App::new(daemon, client::rows, spawn_feed(), scope, group);
    let mut terminal = ratatui::try_init().context(
        "the interactive view needs a terminal; use a subcommand, or `procflow watch --json`",
    )?;
    let result = app.run(&mut terminal);
    ratatui::restore();
    result
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    fn identity(id: i64, comm: &str, project: &str, user: &str) -> Identity {
        Identity {
            id,
            uid: 1000,
            comm: comm.into(),
            exe: format!("/usr/bin/{comm}"),
            project_root: project.into(),
            username: user.into(),
            normalized_cmdline: format!("{comm} --serve"),
            ..Default::default()
        }
    }

    fn counter(
        identity_id: i64,
        scope: Scope,
        ingress_bytes: u64,
        egress_bytes: u64,
    ) -> CounterRow {
        CounterRow {
            identity_id,
            scope: scope as i32,
            ingress_bytes,
            egress_bytes,
            ..Default::default()
        }
    }

    /// Stands in for the daemon: three Identities with stored history.
    fn rpc(body: request::Body) -> Result<Rows> {
        Ok(match body {
            request::Body::TopIdentities(q) if q.group_by() == GroupBy::User => Rows {
                counters: vec![
                    counter(0, Scope::External, 5 << 30, 1 << 30),
                    counter(0, Scope::Loopback, 9, 9),
                ],
                ..Default::default()
            },
            request::Body::TopIdentities(_) => Rows {
                counters: vec![
                    counter(1, Scope::External, 3 << 30, 100 << 20),
                    counter(2, Scope::External, 1 << 20, 2 << 30),
                    counter(3, Scope::Loopback, 7 << 30, 7 << 30),
                ],
                identities: identities(),
                ..Default::default()
            },
            request::Body::Series(_) => Rows {
                counters: (0..24)
                    .map(|hour| CounterRow {
                        bucket_unix_ms: hour * 3_600_000,
                        ..counter(1, Scope::External, hour as u64 * 1000, 500)
                    })
                    .collect(),
                tier: procflow_ipc::v1::Tier::Hour as i32,
                ..Default::default()
            },
            other => anyhow::bail!("unexpected request {other:?}"),
        })
    }

    fn identities() -> Vec<Identity> {
        vec![
            identity(1, "firefox", "<none>", "eve"),
            identity(2, "node", "/home/eve/web", "eve"),
            identity(3, "postgres", "/home/eve/web", "eve"),
        ]
    }

    /// An app that has seen `ticks` one-second chunks of live traffic.
    pub(crate) fn app(ticks: u64) -> App {
        let (tx, feed) = mpsc::channel();
        let daemon = HelloOk {
            daemon_version: "0.1.0".into(),
            collector_active: true,
            ..Default::default()
        };
        let mut app = App::new(daemon, rpc, feed, Scope::External, GroupBy::Identity);
        for tick in 0..ticks {
            let rows = Rows {
                counters: vec![
                    counter(1, Scope::External, 2_000_000 + tick * 1000, 100_000),
                    counter(2, Scope::External, 50_000, 400_000),
                    counter(3, Scope::Loopback, 9_000_000, 9_000_000),
                ],
                identities: if tick == 0 { identities() } else { Vec::new() },
                ..Default::default()
            };
            tx.send(Feed::Chunk(Chunk {
                rows: Some(rows),
                interval_ms: 1000,
            }))
            .unwrap();
        }
        app.tick();
        app
    }

    fn press(app: &mut App, keys: &str) {
        for c in keys.chars() {
            assert!(!app.on_key(KeyEvent::from(KeyCode::Char(c))));
            app.tick();
        }
    }

    fn names(app: &App) -> Vec<&str> {
        app.rows.iter().map(|row| row.name.as_str()).collect()
    }

    #[test]
    fn live_rows_show_rates_ranked_within_the_scope() {
        let mut app = app(5);
        // External only by default: postgres talks over loopback.
        assert_eq!(names(&app), ["firefox", "node"]);
        let firefox = &app.rows[0];
        assert_eq!((firefox.ingress, firefox.egress), (2_003_000, 100_000));
        assert_eq!(firefox.session, (10_010_000, 500_000));
        assert_eq!(firefox.samples.len(), 5);
        assert_eq!(app.total.ingress, 2_053_000);

        press(&mut app, "o"); // rank by ingress
        press(&mut app, "o"); // rank by egress
        assert_eq!(names(&app), ["node", "firefox"]);
        press(&mut app, "ss"); // external → loopback → all
        assert_eq!(app.scope, Scope::All);
        assert_eq!(names(&app), ["postgres", "node", "firefox"]);
    }

    #[test]
    fn grouping_and_filtering_happen_in_the_view() {
        let mut app = app(3);
        press(&mut app, "ss"); // all scopes
        press(&mut app, "g"); // by project
        assert_eq!(app.group, GroupBy::Project);
        assert_eq!(names(&app), ["web", "-"]);
        let web = &app.rows[0];
        assert_eq!(web.third, "2 identities");
        assert_eq!(web.members[0].0, "postgres");
        assert_eq!(web.ingress, 9_050_000);
        assert_eq!(app.group_sizes, [3, 2, 3, 1]);

        press(&mut app, "ggg"); // exe → user → identity
        press(&mut app, "/fire");
        assert!(app.filtering);
        assert_eq!(names(&app), ["firefox"]);
        assert!(!app.on_key(KeyEvent::from(KeyCode::Esc)));
        app.tick();
        assert_eq!(names(&app).len(), 3);

        // The filter sees the whole project path and the command line, not
        // only what the columns have room for.
        press(&mut app, "/EVE/WEB");
        assert_eq!(names(&app), ["postgres", "node"]);
        assert!(!app.on_key(KeyEvent::from(KeyCode::Esc)));
        press(&mut app, "/--serve");
        assert_eq!(names(&app).len(), 3);
    }

    #[test]
    fn the_cursor_follows_its_row_through_the_ranking() {
        let mut app = app(3);
        press(&mut app, "j");
        assert_eq!(app.selected().unwrap().name, "node");
        press(&mut app, "oo"); // egress ranks node first
        assert_eq!(app.table.selected(), Some(0));
        assert_eq!(app.selected().unwrap().name, "node");
    }

    #[test]
    fn history_windows_show_stored_totals_and_a_series() {
        let mut app = app(1);
        press(&mut app, "ww"); // live → last hour → today
        assert_eq!(app.window, Window::Today);
        assert_eq!(names(&app), ["firefox", "node"]);
        assert_eq!(
            (app.rows[0].ingress, app.rows[0].egress),
            (3 << 30, 100 << 20)
        );
        assert_eq!(app.today, (5 << 30, 1 << 30));
        assert_eq!(app.history.ingress.len(), 24);
        assert_eq!(app.history.tier, "hour");
        assert!(app.error.is_none());
    }

    #[test]
    fn the_sidebar_applies_the_choice_under_its_cursor() {
        let mut app = app(1);
        assert!(!app.on_key(KeyEvent::from(KeyCode::Tab)));
        assert_eq!(app.focus, Focus::Sidebar);
        press(&mut app, "jjjjjjj"); // down to "Project"
        assert!(!app.on_key(KeyEvent::from(KeyCode::Enter)));
        assert_eq!(app.group, GroupBy::Project);
        assert!(app.on_key(KeyEvent::from(KeyCode::Char('q'))));
    }
}
