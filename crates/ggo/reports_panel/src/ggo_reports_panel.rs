//! GGO Reports panel: the right dock's one list of everything that was
//! recorded about a run -- emulator perf runs, `ggo-diag` device runs and
//! `ggo-uartd` fault dumps -- newest first, whatever produced them.
//!
//! The panel owns no report rendering at all. Clicking a row opens (or
//! re-focuses) the center Reports tab through
//! [`ggo_charts_panel::open_charts_item`] and hands it the selection; the
//! dock is a spine of entry points, the tab is the reader.
//!
//! The three sources are read on the background executor and merged by
//! [`merge_rows`], which is pure and separately tested. All three live in
//! the one shared PostgreSQL database; only the daemon's dumps are still
//! files, so they are imported from `~/.ggo/uartd/faults` into the `fault`
//! table on every load (`faults::import` is idempotent), because nothing
//! else ingests them.
//!
//! **The three sources do not agree on what a timestamp looks like.** A
//! perf run carries ISO-UTC (`2026-09-02T17:25:37Z`, ggo-server's ingest);
//! a device run and a fault carry a LOCAL underscore stamp
//! (`2026-09-02_08-49-33`, from the daemons' file names). They are ordered
//! through [`parse_when`], never as strings -- `'_' > 'T'` alone would put
//! every device row of a day above every perf row of it, and the two are
//! in different zones besides.

use std::collections::HashSet;
use std::time::Duration;

use chrono::{DateTime, Local, NaiveDateTime, TimeZone};
use gpui::{
    Action, AnyElement, App, ClickEvent, Context, EventEmitter, FocusHandle, Focusable,
    IntoElement, Pixels, Render, Task, WeakEntity, Window, actions, div, px,
};
use ui::prelude::*;
use ui::{ListItem, Tooltip};
use workspace::Workspace;
use workspace::dock::{DockPosition, Panel, PanelEvent};

use ggo_charts_panel::history::{self, HISTORY_LIMIT};
use ggo_charts_panel::loader::{self, RunListing};
use ggo_charts_panel::{RunSummary, open_charts_item};
use ggo_daemon_client::{Connect, FaultRow};

actions!(
    ggo_reports,
    [
        /// Toggles focus on the GGO Reports panel.
        ToggleFocus,
        /// Re-reads the report lists now.
        Refresh,
    ]
);

const PANEL_KEY: &str = "GgoReportsPanel";
const KEY_CONTEXT: &str = "GgoReportsPanel";
const DEFAULT_WIDTH: Pixels = px(300.);
const EMPTY_MESSAGE: &str = "no reports yet";
const LOADING_MESSAGE: &str = "reading reports…";
/// The shortest the list may get. It is the only `flex_1` child of the
/// root, so without a floor a wrapped header over a multi-line note
/// squeezes it to nothing in a short panel instead of making the panel
/// scroll.
const MIN_LIST_HEIGHT: Pixels = px(120.);
/// The daemon appends to its faults directory while the panel is open, so
/// a visible panel re-reads on a timer rather than only on activation.
const POLL_INTERVAL: Duration = Duration::from_secs(5);
/// How the two halves of a load differ in cost: importing dumps and
/// listing them is local file work plus one query, while reconciling the
/// device history is a second full scan of `ggo-diag`'s run table -- which,
/// during a live run, is a growing table re-read every time. Every sixth
/// poll tick (30 s) is often enough for a list of finished runs; see
/// [`reconcile_history_this_tick`].
const HISTORY_EVERY_TICKS: u64 = 6;
/// How a row's time is shown, whatever shape its producer recorded it in.
const WHEN_FORMAT: &str = "%Y-%m-%d %H:%M";

/// Why a row's three file entries are dead. A perf run and a device run
/// are rows in the shared database -- there is no file to copy, reveal or
/// unlink, and an enabled entry that reports that only after the click is
/// a worse affordance than one that says so up front. **Names a STATE**:
/// a fault row lands here too once `ggo-uartd` has rotated its dump away.
const NO_REPORT_FILE: &str = "no file on disk for this report";

/// Why Reveal is dead on a row that DOES have a file: it is not inside
/// any folder this window has open, so the project panel has nowhere to
/// put it. A state, not a cause -- the daemon's dump directory is outside
/// the project on most machines and inside it on some.
const NOT_IN_PROJECT: &str = "this file is not inside an open project folder";

// Handles for the regions whose overflow behaviour the layout tests
// assert. `LIST_SELECTOR` is the list's element id as well; the rest are
// `debug_selector`s, which gpui records only in test builds (the closure
// is discarded unevaluated otherwise), so they cost nothing shipped.
const HEADER_SELECTOR: &str = "ggo-reports-header";
const LIST_SELECTOR: &str = "ggo-reports-list";

/// The title cell of the row at list index `ix` -- the widest thing the
/// list paints, and what a layout test measures against the panel.
fn row_title_selector(ix: usize) -> String {
    format!("ggo-reports-row-title-{ix}")
}

/// The row's own entries, wrapped one selector each so a render test can
/// aim a real click at the button rather than reaching past it to the
/// handler ([`ggo_charts_panel`]'s `PROFILE_SORT_SELECTOR` pattern).
fn row_copy_selector(ix: usize) -> String {
    format!("ggo-reports-row-copy-{ix}")
}

fn row_reveal_selector(ix: usize) -> String {
    format!("ggo-reports-row-reveal-{ix}")
}

fn row_delete_selector(ix: usize) -> String {
    format!("ggo-reports-row-delete-{ix}")
}

pub fn init(cx: &mut App) {
    cx.observe_new(|workspace: &mut Workspace, window, cx| {
        let Some(window) = window else {
            return;
        };
        let weak_workspace = workspace.weak_handle();
        let panel = cx.new(|cx| ReportsPanel::new(Some(weak_workspace), cx));
        workspace.add_panel(panel, window, cx);
        workspace.register_action(|workspace, _: &ToggleFocus, window, cx| {
            workspace.toggle_panel_focus::<ReportsPanel>(window, cx);
        });
    })
    .detach();
}

// ------------------------------------------------------------------ rows

/// Which producer a row came from. The rank is also the tie-break order
/// within one timestamp: a fault is why the reader came, so it sits above
/// the runs it happened during.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ReportKind {
    Perf,
    Device,
    Fault,
}

impl ReportKind {
    pub const ALL: [ReportKind; 3] = [ReportKind::Fault, ReportKind::Device, ReportKind::Perf];

    fn rank(self) -> u8 {
        match self {
            ReportKind::Fault => 0,
            ReportKind::Device => 1,
            ReportKind::Perf => 2,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            ReportKind::Perf => "perf",
            ReportKind::Device => "device",
            ReportKind::Fault => "fault",
        }
    }

    fn selector(self) -> &'static str {
        match self {
            ReportKind::Perf => "ggo-reports-kind-perf",
            ReportKind::Device => "ggo-reports-kind-device",
            ReportKind::Fault => "ggo-reports-kind-fault",
        }
    }

    fn icon(self) -> IconName {
        match self {
            ReportKind::Perf => IconName::FileDoc,
            ReportKind::Device => IconName::Debug,
            ReportKind::Fault => IconName::Warning,
        }
    }
}

/// One line of the merged list. `id` identifies the row to its producer:
/// a fault's dump stem, a device run's `ggo-diag` id, or a perf run's
/// number formatted -- and `perf_id` carries that number unformatted, so
/// the click never has to parse its own display text back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReportRow {
    pub kind: ReportKind,
    pub id: String,
    pub perf_id: Option<i64>,
    pub title: String,
    /// Display only, one format for every producer (see [`WHEN_FORMAT`]).
    /// Never compared -- ordering is [`ReportRow::sort_key`]'s job.
    pub when: String,
    /// Unix seconds, so the three producers' incompatible stamps order
    /// against each other. [`i64::MIN`] for a stamp that did not parse, so
    /// an unreadable row sorts last instead of jumping the list.
    pub sort_key: i64,
    pub trailer: String,
    /// The DEVICE run a fault probably happened during -- never a perf
    /// run id (see `faults::probable_run`).
    pub run_id: Option<String>,
    /// The file this report was read out of, when it has one and it is
    /// still there -- the dump `ggo-uartd` wrote. A perf run and a device
    /// run are rows in the shared database and nothing else, so theirs is
    /// always `None`, and the row's three file entries say so rather than
    /// doing nothing.
    ///
    /// Never filled by [`merge_rows`], which is pure and has no daemon to
    /// ask: [`ReportsPanel::load`] resolves it, once per load.
    pub path: Option<std::path::PathBuf>,
}

/// Unix seconds for either stamp shape the producers write: ISO-UTC
/// (`2026-09-02T17:25:37Z`) from ggo-server's perf ingest, and the
/// daemons' LOCAL `2026-09-02_08-49-33`. `None` for anything else.
///
/// Pure, and deliberately not `chrono`'s lenient parsing: these are the
/// two shapes that exist, and a third one appearing should sort last and
/// be visible as raw text rather than be guessed at.
pub fn parse_when(stamp: &str) -> Option<i64> {
    if let Ok(utc) = NaiveDateTime::parse_from_str(stamp, "%Y-%m-%dT%H:%M:%SZ") {
        return Some(utc.and_utc().timestamp());
    }
    let local = NaiveDateTime::parse_from_str(stamp, "%Y-%m-%d_%H-%M-%S").ok()?;
    // A wall clock is ambiguous twice a year: the hour a DST fall-back
    // repeats maps to two instants (take the earlier -- the daemon wrote
    // the file the first time round more often than the second), and the
    // hour a spring-forward skips maps to none at all, which is `None`.
    Local
        .from_local_datetime(&local)
        .earliest()
        .map(|when| when.timestamp())
}

/// `sort_key` as the reader sees it, falling back to the producer's raw
/// text when it did not parse -- an unreadable stamp is still evidence.
fn display_when(sort_key: i64, raw: &str) -> String {
    match DateTime::from_timestamp(sort_key, 0) {
        Some(when) => when.with_timezone(&Local).format(WHEN_FORMAT).to_string(),
        None => raw.to_string(),
    }
}

fn row_time(raw: &str) -> (i64, String) {
    let sort_key = parse_when(raw).unwrap_or(i64::MIN);
    (sort_key, display_when(sort_key, raw))
}

/// Merge the three sources into one list, newest first by
/// [`parse_when`]'s normalized instant; ties fall back to
/// [`ReportKind::rank`].
pub fn merge_rows(
    perf: Vec<RunListing>,
    device: Vec<RunSummary>,
    faults: Vec<FaultRow>,
) -> Vec<ReportRow> {
    let mut rows: Vec<ReportRow> = Vec::with_capacity(perf.len() + device.len() + faults.len());
    for run in faults {
        let (sort_key, when) = row_time(&run.at);
        rows.push(ReportRow {
            kind: ReportKind::Fault,
            id: run.id,
            perf_id: None,
            title: format!("{}: {}", run.kind, run.detail),
            when,
            sort_key,
            // No trailer: the title already leads with the kind, and a
            // second copy of it under the title says nothing.
            trailer: String::new(),
            run_id: run.run_id,
            path: None,
        });
    }
    for run in device {
        let verdict = run.verdict.unwrap_or_else(|| "no verdict".to_string());
        let (sort_key, when) = row_time(&run.started_at);
        rows.push(ReportRow {
            kind: ReportKind::Device,
            id: run.id.clone(),
            perf_id: None,
            title: run.id,
            when,
            sort_key,
            trailer: format!("{} · {verdict}", run.state),
            run_id: None,
            path: None,
        });
    }
    for run in perf {
        let (sort_key, when) = row_time(&run.started_at);
        rows.push(ReportRow {
            kind: ReportKind::Perf,
            id: run.id.to_string(),
            perf_id: Some(run.id),
            title: run.display_title(),
            when,
            sort_key,
            trailer: String::new(),
            run_id: None,
            path: None,
        });
    }
    rows.sort_by(|a, b| {
        b.sort_key
            .cmp(&a.sort_key)
            .then(a.kind.rank().cmp(&b.kind.rank()))
    });
    rows
}

/// Whether the poll tick numbered `tick` reconciles the device history.
/// Pure so the cadence is testable without a clock.
fn reconcile_history_this_tick(tick: u64) -> bool {
    tick.is_multiple_of(HISTORY_EVERY_TICKS)
}

// ------------------------------------------------------------ view state

#[derive(Debug, Clone, PartialEq, Eq)]
enum LoadState {
    Empty,
    Loading,
    Ready,
    Error(String),
}

pub struct ReportsPanel {
    workspace: Option<WeakEntity<Workspace>>,
    focus_handle: FocusHandle,
    position: DockPosition,
    rows: Vec<ReportRow>,
    /// The device runs the last RECONCILED load found. Kept because most
    /// loads skip the reconcile (see [`HISTORY_EVERY_TICKS`]) and must
    /// still merge those rows in rather than dropping them from the list.
    device: Vec<RunSummary>,
    /// [`history::History::note`] from that same reconcile, carried for
    /// the same reason its rows are: a load that skipped the reconcile
    /// learned nothing new about the device history, and dropping the
    /// reason would make it blink out of the empty state every tick.
    device_note: Option<String>,
    /// What went wrong that was not fatal to the load -- a fault import
    /// that failed, a device history that could not be read. Shown only
    /// when there is nothing to list (see [`Self::note`]): a reason is
    /// what an empty panel owes the reader, and a populated list is never
    /// blanked to make room for one.
    notes: Vec<String>,
    /// Indices into `rows` that the filter chips let through, in display
    /// order -- what a click's `ix` means.
    visible: Vec<usize>,
    state: LoadState,
    hidden: HashSet<ReportKind>,
    generation: u64,
    /// A load is in flight. The poll fires on a wall clock, not on the
    /// previous load finishing, so without this a slow tick stacks a
    /// second full reconcile on top of the first.
    loading: bool,
    /// Poll ticks since this activation, for [`reconcile_history_this_tick`].
    poll_tick: u64,
    /// What the last row action had to say for itself -- a delete that
    /// could not unlink. Rendered under the header, unlike [`Self::notes`],
    /// which is the EMPTY state's: a failure the user just caused has to
    /// be visible over a populated list.
    action_note: Option<String>,
    /// How this panel reaches the daemon. Perf runs, device runs and
    /// faults all arrive over that socket; the panel opens no database of
    /// its own and never reads the dump directory itself.
    connect: Connect,
    _load_task: Option<Task<()>>,
    _poll_task: Option<Task<()>>,
}

impl ReportsPanel {
    fn new(workspace: Option<WeakEntity<Workspace>>, cx: &mut Context<Self>) -> Self {
        Self {
            workspace,
            focus_handle: cx.focus_handle(),
            position: DockPosition::Right,
            rows: Vec::new(),
            device: Vec::new(),
            device_note: None,
            notes: Vec::new(),
            visible: Vec::new(),
            state: LoadState::Empty,
            hidden: HashSet::new(),
            generation: 0,
            loading: false,
            poll_tick: 0,
            action_note: None,
            connect: ggo_daemon_client::system_connect(),
            _load_task: None,
            _poll_task: None,
        }
    }

    /// Reload everything, device history included. The activation and
    /// the Refresh button's entry point.
    pub fn refresh(&mut self, cx: &mut Context<Self>) {
        self.load(true, cx);
    }

    /// One poll tick: always the cheap half, the device reconcile only
    /// every [`HISTORY_EVERY_TICKS`]th tick.
    fn poll(&mut self, cx: &mut Context<Self>) {
        self.poll_tick += 1;
        self.load(reconcile_history_this_tick(self.poll_tick), cx);
    }

    /// Read the sources off-thread and replace the list. Every read is
    /// blocking (each drives `ggo-db`'s shared runtime through
    /// `block_on`, which PANICS inside a tokio one), so none of this may
    /// touch the UI thread. A load that lands after a newer one started is
    /// dropped on the generation guard.
    ///
    /// With `reconcile_history` the device runs are re-read from the
    /// `runs` table; without it the ones from the last reconcile are
    /// merged in again, so skipping that read never empties the list of
    /// device rows.
    fn load(&mut self, reconcile_history: bool, cx: &mut Context<Self>) {
        // The poll fires on a wall clock. A tick that arrives while the
        // previous load is still running is dropped rather than queued:
        // the next one is 5 s away and reads the same sources anyway.
        if self.loading {
            return;
        }
        self.generation += 1;
        let generation = self.generation;
        let connect = self.connect.clone();
        let known_device = self.device.clone();
        let known_device_note = self.device_note.clone();
        self.loading = true;
        // A refresh over an already-painted list must not blank it: the
        // poll runs every 5 s and would flash the loading line each time.
        if self.rows.is_empty() {
            self.state = LoadState::Loading;
        }
        cx.notify();
        let load = cx.background_spawn(async move {
            // A failed import is NOT fatal -- the rows already in the
            // database still list -- but it is also not nothing: the
            // dumps the daemon wrote are then missing from a list that
            // would otherwise read as complete. Same sentence the MCP's
            // `import_failure_note` prints, so an agent and the dock
            // describe one failure one way.
            // The import is the daemon's now: `ggo_faults` digests every
            // new dump on its way to answering. Its failure comes back
            // WITH the rows rather than as a separate step, and is pushed
            // onto `notes` below.
            let mut notes = Vec::new();
            let mut failure = None;
            let perf = match loader::list_runs(&connect) {
                Ok(runs) => runs,
                Err(error) => {
                    failure = Some(error);
                    Vec::new()
                }
            };
            // `list_runs` has no LIMIT of its own -- the picker wants
            // every run -- so the cap is the dock's, and it is applied
            // BEFORE the merge: the list is newest-first and capped at
            // the same `HISTORY_LIMIT` the other two sources already use,
            // and sorting a full run table to throw most of it away is
            // work the panel can decline.
            let perf = perf.into_iter().take(HISTORY_LIMIT as usize).collect();
            let (device, device_note) = if reconcile_history {
                let history = history::load(&connect, HISTORY_LIMIT);
                (history.runs, history.note)
            } else {
                (known_device, known_device_note)
            };
            if let Some(note) = device_note.as_ref() {
                notes.push(note.clone());
            }
            // An unreachable database is an ERROR here, not an empty
            // list: a fault section that silently reads as "no dumps"
            // when the server is down hides the one signal the user has.
            let client = connect().map_err(|error| format!("{error:#}"));
            let faults = match client.as_ref().map_err(Clone::clone).and_then(|client| {
                client
                    .faults(Some(HISTORY_LIMIT))
                    .map_err(|error| format!("{error:#}"))
            }) {
                Ok(list) => {
                    // An import that failed is not fatal -- the rows
                    // already stored still list -- but it is also not
                    // nothing: the dumps the daemon wrote are then missing
                    // from a list that would otherwise read as complete.
                    if let Some(note) = list.import_error {
                        log::warn!("reports: {note}");
                        notes.push(note);
                    }
                    list.rows
                }
                Err(error) => {
                    failure = failure.or(Some(error));
                    Vec::new()
                }
            };
            let mut rows = merge_rows(perf, device.clone(), faults);
            // The file behind each fault row, resolved once per load
            // rather than once per render: deciding it is a `stat` over
            // the socket, and all three of the row's file entries need
            // the same answer. A dump the daemon has since rotated away
            // (`KEEP_DUMPS`) leaves the row with no file, which is what
            // its entries then say.
            if let Ok(client) = &client {
                for row in rows.iter_mut().filter(|row| row.kind == ReportKind::Fault) {
                    row.path = client
                        .fault_raw_path(&row.id)
                        .ok()
                        .filter(|path| path.is_file());
                }
            }
            (rows, device, device_note, notes, failure)
        });
        self._load_task = Some(cx.spawn(async move |this, cx| {
            let (rows, device, device_note, notes, failure) = load.await;
            this.update(cx, |this, cx| {
                // The flag is cleared even for a superseded load: it
                // guards the spawn, not the store.
                this.loading = false;
                if this.generation != generation {
                    return;
                }
                this.rows = rows;
                this.device = device;
                this.device_note = device_note;
                this.notes = notes;
                this.state = match failure {
                    Some(error) => LoadState::Error(error),
                    None if this.rows.is_empty() => LoadState::Empty,
                    None => LoadState::Ready,
                };
                this.rebuild_visible();
                cx.notify();
            })
            .ok();
        }));
    }

    fn rebuild_visible(&mut self) {
        self.visible = self
            .rows
            .iter()
            .enumerate()
            .filter(|(_, row)| !self.hidden.contains(&row.kind))
            .map(|(ix, _)| ix)
            .collect();
    }

    fn toggle_kind(&mut self, kind: ReportKind, cx: &mut Context<Self>) {
        if !self.hidden.remove(&kind) {
            self.hidden.insert(kind);
        }
        self.rebuild_visible();
        cx.notify();
    }

    /// Every row the last load produced, filters ignored -- test hook.
    pub fn all_rows(&self) -> &[ReportRow] {
        &self.rows
    }

    /// Open the `ix`th VISIBLE row in the center Reports tab. This is the
    /// row's real `on_click` body as well as the test entry point.
    pub fn click_row(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(row) = self
            .visible
            .get(ix)
            .and_then(|ix| self.rows.get(*ix))
            .cloned()
        else {
            return;
        };
        let Some(workspace) = self.workspace.as_ref().and_then(|w| w.upgrade()) else {
            return;
        };
        // A dock click opening a center tab: the pane work is the
        // workspace's, so it runs deferred, never inside this panel's
        // lease (the fork's hook rule).
        cx.defer_in(window, move |_, window, cx| {
            workspace.update(cx, |workspace, cx| {
                open_charts_item(workspace, window, cx, |charts, _, cx| match row.kind {
                    ReportKind::Perf => {
                        if let Some(id) = row.perf_id {
                            charts.open_run(id, cx);
                        }
                    }
                    ReportKind::Device => charts.open_device_run(row.id.clone(), cx),
                    ReportKind::Fault => charts.open_fault(row.id.clone(), cx),
                });
            });
        });
    }

    /// The `ix`th VISIBLE row, which is what a row entry's `ix` means.
    fn visible_row(&self, ix: usize) -> Option<&ReportRow> {
        self.visible.get(ix).and_then(|ix| self.rows.get(*ix))
    }

    /// The project entry the `ix`th visible row's file IS, when this
    /// window has a folder open that contains it. `None` is what makes
    /// the Reveal entry `.disabled(..)` with [`NOT_IN_PROJECT`] on it
    /// rather than a click that goes nowhere.
    ///
    /// Reading the workspace from the panel's own render is safe: the
    /// entity being updated here is the panel, not the workspace.
    fn row_project_entry(&self, row: &ReportRow, cx: &App) -> Option<project::ProjectEntryId> {
        let path = row.path.as_ref()?;
        let workspace = self.workspace.as_ref()?.upgrade()?;
        let project = workspace.read(cx).project().read(cx);
        let project_path = project.find_project_path(path, cx)?;
        Some(project.entry_for_path(&project_path, cx)?.id)
    }

    /// Show the `ix`th visible row's file in the project panel. The panel
    /// reveals and activates ITSELF off `project::Event::RevealInProjectPanel`,
    /// so this emits and nothing more.
    ///
    /// Deferred: the reveal is the workspace's work, so it never runs
    /// inside this panel's lease (the fork's hook rule), and the entry id
    /// is resolved at RENDER time and handed in rather than looked up
    /// from inside the deferred body, which may not read the workspace it
    /// re-enters.
    fn reveal_row(
        &mut self,
        entry: project::ProjectEntryId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(workspace) = self.workspace.as_ref().and_then(|w| w.upgrade()) else {
            return;
        };
        cx.defer_in(window, move |_, _window, cx| {
            workspace.update(cx, |workspace, cx| {
                workspace.project().update(cx, |_, cx| {
                    cx.emit(project::Event::RevealInProjectPanel(entry))
                });
            });
        });
    }

    /// Unlink the `ix`th visible row's file, once the user has confirmed
    /// it by name. The row itself stays: the `fault` row lives in the
    /// shared database, which this panel only reads -- the cascade line
    /// says so rather than letting the reader assume a delete here means
    /// the report is gone.
    fn delete_row(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some((id, path)) = self
            .visible_row(ix)
            .and_then(|row| Some((row.id.clone(), row.path.clone()?)))
        else {
            return;
        };
        let name = path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| id.clone());
        let confirmed = ggo_common::confirm_destructive_cascade(
            &format!("Delete the dump file {name}?"),
            &[
                format!("report {id} stays listed -- only the file is removed"),
                path.to_string_lossy().into_owned(),
            ],
            "Delete",
            false,
            window,
            cx,
        );
        cx.spawn(async move |this, cx| {
            if !confirmed.await {
                return;
            }
            let removed = cx
                .background_spawn(async move {
                    std::fs::remove_file(&path).map_err(|error| format!("{name}: {error}"))
                })
                .await;
            this.update(cx, |this, cx| {
                this.action_note = removed.err();
                this.refresh(cx);
            })
            .ok();
        })
        .detach();
    }

    /// Reach a different daemon than the one the environment names.
    /// Test hook: production connects through
    /// [`ggo_daemon_client::system_connect`].
    pub fn set_connect(&mut self, connect: Connect) {
        self.connect = connect;
    }

    // -------------------------------------------------------------- render

    fn render_kind_toggle(&self, kind: ReportKind, cx: &mut Context<Self>) -> AnyElement {
        Button::new(kind.selector(), kind.label())
            .label_size(LabelSize::Small)
            .toggle_state(!self.hidden.contains(&kind))
            .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| this.toggle_kind(kind, cx)))
            .into_any_element()
    }

    fn render_header(&self, cx: &mut Context<Self>) -> impl IntoElement {
        h_flex()
            .debug_selector(|| HEADER_SELECTOR.to_string())
            // The chips and the refresh button are the only way to unhide
            // a kind or force a re-read, and a dock can be dragged
            // narrower than the row they make: it wraps rather than
            // pushing them past an edge nothing scrolls back from.
            .flex_wrap()
            .gap_0p5()
            .px_1()
            .py_0p5()
            .w_full()
            .border_b_1()
            .border_color(cx.theme().colors().border)
            .child(
                Label::new("Reports")
                    .size(LabelSize::Small)
                    .color(Color::Muted),
            )
            .child(div().flex_1())
            .children(
                ReportKind::ALL
                    .into_iter()
                    .map(|kind| self.render_kind_toggle(kind, cx)),
            )
            .child(
                IconButton::new("ggo-reports-refresh", IconName::HistoryRerun)
                    .icon_size(IconSize::Small)
                    .tooltip(Tooltip::text("Refresh"))
                    .on_click(cx.listener(|this, _: &ClickEvent, _, cx| this.refresh(cx))),
            )
    }

    /// A row's three file entries. Every one of them is `.disabled(..)`
    /// with its reason in the tooltip when it has nothing to act on, so
    /// the affordance is the same on a perf row (which never has a file)
    /// as on a fault row whose dump has been rotated away -- and neither
    /// is a click that silently does nothing.
    fn render_row_actions(&self, ix: usize, row: &ReportRow, cx: &mut Context<Self>) -> AnyElement {
        let text = row
            .path
            .as_ref()
            .map(|path| path.to_string_lossy().into_owned());
        let copy_tooltip = match &text {
            Some(text) => format!("Copy path\n{text}"),
            None => NO_REPORT_FILE.to_string(),
        };
        let clipboard = text.clone();
        let entry = self.row_project_entry(row, cx);
        let reveal_tooltip = match (&text, entry) {
            (Some(_), Some(_)) => "Reveal in project panel".to_string(),
            (Some(_), None) => NOT_IN_PROJECT.to_string(),
            (None, _) => NO_REPORT_FILE.to_string(),
        };
        let delete_tooltip = match &text {
            Some(text) => format!("Delete {text}"),
            None => NO_REPORT_FILE.to_string(),
        };
        h_flex()
            .gap_0p5()
            .child(
                div().debug_selector(move || row_copy_selector(ix)).child(
                    IconButton::new(("ggo-reports-row-copy", ix), IconName::Copy)
                        .icon_size(IconSize::XSmall)
                        .disabled(clipboard.is_none())
                        .tooltip(Tooltip::text(copy_tooltip))
                        .on_click(move |_: &ClickEvent, _, cx| {
                            if let Some(text) = clipboard.clone() {
                                cx.write_to_clipboard(gpui::ClipboardItem::new_string(text));
                            }
                        }),
                ),
            )
            .child(
                div().debug_selector(move || row_reveal_selector(ix)).child(
                    IconButton::new(("ggo-reports-row-reveal", ix), IconName::FileTree)
                        .icon_size(IconSize::XSmall)
                        .disabled(entry.is_none())
                        .tooltip(Tooltip::text(reveal_tooltip))
                        .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                            if let Some(entry) = entry {
                                this.reveal_row(entry, window, cx);
                            }
                        })),
                ),
            )
            .child(
                div().debug_selector(move || row_delete_selector(ix)).child(
                    IconButton::new(("ggo-reports-row-delete", ix), IconName::Trash)
                        .icon_size(IconSize::XSmall)
                        .disabled(text.is_none())
                        .tooltip(Tooltip::text(delete_tooltip))
                        .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                            this.delete_row(ix, window, cx);
                        })),
                ),
            )
            .into_any_element()
    }

    fn render_row(&self, ix: usize, row: &ReportRow, cx: &mut Context<Self>) -> AnyElement {
        let trailer = row.trailer.clone();
        let run_id = row.run_id.clone();
        ListItem::new(("ggo-reports-row", ix))
            .on_click(
                cx.listener(move |this, _: &ClickEvent, window, cx| this.click_row(ix, window, cx)),
            )
            .child(
                v_flex()
                    .w_full()
                    .child(
                        h_flex()
                            .gap_1()
                            .w_full()
                            .child(
                                // The kind is otherwise a glyph and
                                // nothing else: the filter chips name the
                                // three, but a row on its own does not.
                                div()
                                    .id(("ggo-reports-row-kind", ix))
                                    .tooltip(Tooltip::text(row.kind.label()))
                                    .child(
                                        Icon::new(row.kind.icon())
                                            .size(IconSize::Small)
                                            .color(Color::Muted),
                                    ),
                            )
                            .child(
                                // In a ROW the automatic minimum is the
                                // child's content size, and gpui measures
                                // a label's min-content UNWRAPPED: without
                                // this the title painted its full width
                                // out of the dock rather than wrapping.
                                div()
                                    .debug_selector(move || row_title_selector(ix))
                                    .min_w_0()
                                    .child(Label::new(row.title.clone()).size(LabelSize::Small)),
                            )
                            .child(div().flex_1())
                            .child(
                                Label::new(row.when.clone())
                                    .size(LabelSize::XSmall)
                                    .color(Color::Muted),
                            )
                            .child(self.render_row_actions(ix, row, cx)),
                    )
                    .when(!trailer.is_empty(), |this| {
                        this.child(
                            Label::new(trailer)
                                .size(LabelSize::XSmall)
                                .color(Color::Muted),
                        )
                    })
                    .children(run_id.map(|run_id| {
                        div().pl_2().child(
                            Label::new(format!("during run {run_id}"))
                                .size(LabelSize::XSmall)
                                .color(Color::Muted),
                        )
                    })),
            )
            .into_any_element()
    }

    /// What goes under the header when the list has nothing to say for
    /// itself: a reason, never an empty panel.
    ///
    /// `None` once there are rows -- a note is the empty state's, and a
    /// populated list is never blanked to make room for one. An empty
    /// list prefers the non-fatal reasons the load collected over the
    /// bare [`EMPTY_MESSAGE`], because "no reports yet" is a claim, and
    /// a failed import means nobody is in a position to make it.
    fn note(&self) -> Option<String> {
        match &self.state {
            LoadState::Empty if !self.notes.is_empty() => Some(self.notes.join("\n")),
            LoadState::Empty => Some(EMPTY_MESSAGE.to_string()),
            LoadState::Loading => Some(LOADING_MESSAGE.to_string()),
            LoadState::Error(error) => Some(error.clone()),
            LoadState::Ready => None,
        }
    }

    fn render_note(&self) -> Option<impl IntoElement> {
        let note = self.note()?;
        Some(
            div().px_2().py_1().child(
                ggo_common::CopyableText::new("ggo-reports-note", note).size(LabelSize::Small),
            ),
        )
    }
}

impl Render for ReportsPanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let rows: Vec<AnyElement> = self
            .visible
            .iter()
            .enumerate()
            .filter_map(|(ix, row_ix)| Some(self.render_row(ix, self.rows.get(*row_ix)?, cx)))
            .collect();
        v_flex()
            .id("ggo-reports-root")
            .key_context(KEY_CONTEXT)
            .size_full()
            // The header wraps onto more rows in a narrow dock and the
            // note under it is an unbounded reason: past a short enough
            // panel that chrome no longer fits, and the panel scrolls
            // rather than eating the list's [`MIN_LIST_HEIGHT`].
            .overflow_y_scroll()
            .track_focus(&self.focus_handle)
            .bg(cx.theme().colors().panel_background)
            .on_action(cx.listener(|this, _: &Refresh, _, cx| this.refresh(cx)))
            .child(self.render_header(cx))
            .children(self.action_note.clone().map(|note| {
                div().px_2().py_1().child(
                    ggo_common::CopyableText::new("ggo-reports-action-note", note)
                        .size(LabelSize::Small)
                        .color(Color::Error),
                )
            }))
            .children(self.render_note())
            .child(
                // Not a `uniform_list`: these rows are one to three lines
                // (a trailer, a fault's "during run" line), and a uniform
                // list would pin every row to row 0's measured height.
                v_flex()
                    .id(LIST_SELECTOR)
                    .debug_selector(|| LIST_SELECTOR.to_string())
                    .flex_1()
                    .min_h(MIN_LIST_HEIGHT)
                    .overflow_y_scroll()
                    .children(rows),
            )
    }
}

impl Focusable for ReportsPanel {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EventEmitter<PanelEvent> for ReportsPanel {}

impl Panel for ReportsPanel {
    fn persistent_name() -> &'static str {
        "GGO Reports"
    }

    fn panel_key() -> &'static str {
        PANEL_KEY
    }

    fn position(&self, _window: &Window, _cx: &App) -> DockPosition {
        self.position
    }

    fn position_is_valid(&self, position: DockPosition) -> bool {
        matches!(position, DockPosition::Left | DockPosition::Right)
    }

    fn set_position(
        &mut self,
        position: DockPosition,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.position = position;
        cx.notify();
    }

    fn default_size(&self, _window: &Window, _cx: &App) -> Pixels {
        DEFAULT_WIDTH
    }

    fn icon(&self, _window: &Window, _cx: &App) -> Option<IconName> {
        Some(IconName::FileDoc)
    }

    fn icon_tooltip(&self, _window: &Window, _cx: &App) -> Option<&'static str> {
        Some("Reports")
    }

    fn toggle_action(&self) -> Box<dyn Action> {
        Box::new(ToggleFocus)
    }

    fn activation_priority(&self) -> u32 {
        // Built-ins 0-7, GGO panels 8-15 (grep activation_priority across
        // crates/): 14 was `ggo_map_panel`'s and is free since that panel
        // was retired.
        14
    }

    fn set_active(&mut self, active: bool, _window: &mut Window, cx: &mut Context<Self>) {
        if !active {
            // Dropping the task cancels the loop: an invisible panel does
            // no database work.
            self._poll_task = None;
            return;
        }
        self.poll_tick = 0;
        self.refresh(cx);
        self._poll_task = Some(cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(POLL_INTERVAL).await;
                if this.update(cx, |this, cx| this.poll(cx)).is_err() {
                    return;
                }
            }
        }));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ggo_db::TestDb;
    use gpui::TestAppContext;
    use project::{FakeFs, Project};
    use workspace::{AppState, MultiWorkspace};

    /// A url whose socket directory does not exist, so no read through it
    /// can reach a server -- what a stopped `ggo-pg` looks like to the
    /// panel. The postgres analog of the old "a path nothing can be
    /// created at" fixture.
    const UNREACHABLE_DB_URL: &str = "postgres://ggo@localhost/ggo?host=/nonexistent/ggo-pg-socket";

    /// A [`Connect`] onto an in-process daemon over `db_url`, reading
    /// dumps from `faults_dir`.
    ///
    /// Journeys SEED with `TestDb` and a fixture dump directory, then read
    /// back the way the panel does. The daemon does the importing, so a
    /// dump written under `faults_dir` reaches the list exactly as one
    /// written by `ggo-uartd` would.
    fn test_connect(db_url: &str, faults_dir: &std::path::Path) -> Connect {
        ggo_daemon_client::test_daemon::ingesting_connect(db_url, faults_dir)
    }

    fn perf(id: i64, at: &str) -> RunListing {
        RunListing {
            id,
            started_at: at.into(),
            cart_name: "wilds".into(),
            label: Some(format!("run{id}")),
        }
    }

    fn device(id: &str, at: &str) -> RunSummary {
        RunSummary {
            id: id.into(),
            started_at: at.into(),
            state: "done".into(),
            verdict: Some("PASS".into()),
        }
    }

    fn fault(id: &str, at: &str) -> FaultRow {
        FaultRow {
            id: id.into(),
            source: "uartd".into(),
            at: at.into(),
            kind: "marker".into(),
            detail: "trap: mcause=".into(),
            tty: "/dev/ttyUSB1".into(),
            boot_stage: None,
            frames: 0,
            run_id: Some("d1".into()),
        }
    }

    #[test]
    fn rows_merge_newest_first_with_faults_ahead_on_ties() {
        let rows = merge_rows(
            vec![perf(7, "2026-09-02_08-00-00")],
            vec![device("d1", "2026-09-02_08-40-00")],
            vec![
                fault("f1", "2026-09-02_08-40-00"),
                fault("f0", "2026-09-01_08-00-00"),
            ],
        );
        let ids: Vec<&str> = rows.iter().map(|r| r.id.as_str()).collect();
        assert_eq!(ids, ["f1", "d1", "7", "f0"]);
        assert_eq!(
            rows[0].trailer, "",
            "a fault's title already leads with its kind"
        );
        assert_eq!(rows[0].run_id.as_deref(), Some("d1"));
        assert_eq!(rows[2].title, "wilds — run7");
        assert_eq!(
            rows[2].perf_id,
            Some(7),
            "the click opens a perf run by number, never by re-parsing its display id"
        );
        assert_eq!(rows[1].trailer, "done · PASS");
    }

    /// The producers disagree about timestamps: a perf run's is ISO-UTC,
    /// a fault's is a local underscore stamp. Sorted as strings, `'_'`
    /// beats `'T'` and every fault of a day jumps every perf run of it --
    /// so this pins the ordering to the real instants, with the expected
    /// order DERIVED through chrono rather than an assumed UTC offset.
    #[test]
    fn mixed_stamp_shapes_order_by_the_instant_not_the_text() {
        let perf_at = "2026-09-02T17:25:37Z";
        let fault_at = "2026-09-02_08-49-33";
        let rows = merge_rows(
            vec![perf(7, perf_at)],
            Vec::new(),
            vec![fault("f1", fault_at)],
        );
        let perf_key = parse_when(perf_at).expect("ISO-UTC parses");
        let fault_key = parse_when(fault_at).expect("the local stamp parses");
        let expected = if perf_key >= fault_key {
            ["7", "f1"]
        } else {
            ["f1", "7"]
        };
        let ids: Vec<&str> = rows.iter().map(|row| row.id.as_str()).collect();
        assert_eq!(ids, expected, "newest first by instant");
        assert!(
            fault_at > perf_at,
            "and a raw string compare would have said the opposite"
        );
        assert_eq!(
            rows.iter().map(|row| row.sort_key).max(),
            Some(perf_key.max(fault_key))
        );
        for row in &rows {
            assert_eq!(
                row.when,
                display_when(row.sort_key, ""),
                "one display format for every producer"
            );
        }
    }

    #[test]
    fn parse_when_takes_both_shapes_and_refuses_anything_else() {
        assert_eq!(
            parse_when("2026-09-02T17:25:37Z"),
            Some(
                NaiveDateTime::parse_from_str("2026-09-02T17:25:37Z", "%Y-%m-%dT%H:%M:%SZ")
                    .expect("fixture")
                    .and_utc()
                    .timestamp()
            ),
            "ISO-UTC is read as UTC"
        );
        assert_eq!(
            parse_when("2026-09-02_08-49-33"),
            Local
                .with_ymd_and_hms(2026, 9, 2, 8, 49, 33)
                .earliest()
                .map(|when| when.timestamp()),
            "the daemons' stamp is read as LOCAL wall time"
        );
        assert_eq!(parse_when("not a timestamp"), None);
        assert_eq!(parse_when("2026-09-02"), None, "a date is not an instant");
        assert_eq!(
            merge_rows(Vec::new(), Vec::new(), vec![fault("f", "garbage")])[0].sort_key,
            i64::MIN,
            "an unreadable stamp sorts last, never first"
        );
    }

    /// The cheap half of a load runs every tick; the device reconcile --
    /// a second full read of the `runs` table -- runs every sixth.
    #[test]
    fn the_device_reconcile_runs_every_sixth_tick() {
        let reconciling: Vec<u64> = (1..=12)
            .filter(|t| reconcile_history_this_tick(*t))
            .collect();
        assert_eq!(reconciling, [6, 12]);
    }

    /// The filter chips decide what a click's index MEANS: hiding a kind
    /// must not leave the rows below it opening their old neighbours.
    #[gpui::test]
    fn hiding_a_kind_reindexes_the_clickable_rows(cx: &mut gpui::App) {
        let panel = cx.new(|cx| ReportsPanel::new(None, cx));
        panel.update(cx, |panel, cx| {
            panel.rows = merge_rows(
                vec![perf(7, "2026-09-02_08-00-00")],
                vec![device("d1", "2026-09-02_08-40-00")],
                vec![fault("f1", "2026-09-02_08-40-00")],
            );
            panel.rebuild_visible();
            let visible: Vec<&str> = panel
                .visible
                .iter()
                .filter_map(|ix| panel.rows.get(*ix))
                .map(|row| row.id.as_str())
                .collect();
            assert_eq!(visible, ["f1", "d1", "7"]);

            panel.toggle_kind(ReportKind::Fault, cx);
            let visible: Vec<&str> = panel
                .visible
                .iter()
                .filter_map(|ix| panel.rows.get(*ix))
                .map(|row| row.id.as_str())
                .collect();
            assert_eq!(visible, ["d1", "7"], "the fault chip hides fault rows");
            assert_eq!(panel.all_rows().len(), 3, "hiding is not forgetting");
        });
    }

    /// The poll fires on a wall clock, so a tick can arrive while the
    /// previous load is still running. It must be dropped, not stacked:
    /// a full reconcile re-reads the whole device history and two of them
    /// at once is the cost doubled for the same answer.
    #[gpui::test]
    async fn a_refresh_while_one_is_in_flight_does_not_start_a_second(cx: &mut TestAppContext) {
        let db = TestDb::new();
        let temp = tempfile::tempdir().expect("tempdir");
        let faults_dir = temp.path().join("faults");
        write_dump(&faults_dir, "2026-09-02_08-49-33_marker");
        let panel = cx.update(|cx| cx.new(|cx| ReportsPanel::new(None, cx)));
        panel.update(cx, |panel, cx| {
            panel.set_connect(test_connect(db.url(), &faults_dir));
            panel.refresh(cx);
            panel.refresh(cx);
            panel.refresh(cx);
            assert!(panel.loading, "the first load is still in flight");
            assert_eq!(
                panel.generation, 1,
                "the two refreshes on top of it started nothing"
            );
        });
        cx.run_until_parked();
        panel.update(cx, |panel, cx| {
            assert!(!panel.loading, "the load cleared the flag");
            assert_eq!(panel.all_rows().len(), 1, "and it landed");
            panel.refresh(cx);
            assert_eq!(panel.generation, 2, "a refresh after it lands does load");
        });
        cx.run_until_parked();
    }

    /// A dump `ggo-uartd` would have written: the header line the
    /// importer requires, then one line of decoded text.
    fn write_dump(dir: &std::path::Path, id: &str) {
        std::fs::create_dir_all(dir).expect("faults dir");
        std::fs::write(
            dir.join(format!("{id}.log")),
            "# ggo-uartd marker trap: mcause=0x2 — last 30s of /dev/ttyUSB1\n\
             trap: mcause=0x2 mepc=0x80000010\n",
        )
        .expect("dump");
    }

    /// An empty list is a claim ("nothing was recorded"), and it must not
    /// be made when the truth is "the sources could not be read". The
    /// fixture points every read at a url no server answers on, which is
    /// what a stopped `ggo-pg` looks like -- so the import fails, the two
    /// list reads fail, and the panel owes the reader all of it: the
    /// failure's own hint, and never a bare "no reports yet".
    #[gpui::test]
    async fn an_empty_list_says_why_rather_than_claiming_there_is_nothing(cx: &mut TestAppContext) {
        let temp = tempfile::tempdir().expect("tempdir");
        let faults_dir = temp.path().join("faults");
        write_dump(&faults_dir, "2026-09-02_08-49-33_marker");
        let panel = cx.update(|cx| cx.new(|cx| ReportsPanel::new(None, cx)));
        panel.update(cx, |panel, cx| {
            panel.set_connect(test_connect(UNREACHABLE_DB_URL, &faults_dir));
            panel.refresh(cx);
        });
        cx.run_until_parked();

        panel.update(cx, |panel, _| {
            assert!(panel.all_rows().is_empty(), "nothing could be listed");
            // Unlike a missing FILE, an unreachable server is a read that
            // FAILED -- the panel says so outright rather than showing an
            // empty list with a footnote.
            let LoadState::Error(error) = &panel.state else {
                panic!("an unreachable database must land the list in Error, not Empty");
            };
            assert!(
                error.contains(ggo_db::INSTALL_HINT),
                "the failure tells the user how to fix it: {error}"
            );
            let note = panel
                .note()
                .expect("an empty list owes the reader a reason");
            assert_ne!(note, EMPTY_MESSAGE, "'no reports yet' would be a lie here");
            assert_eq!(note, *error, "and the reason shown IS that failure");
            // The device history is best-effort and recorded its own
            // reason on the way past. It is one root cause with the
            // failure above -- the server is down -- so the note the
            // reader sees is that one line, but the reason was not
            // swallowed.
            //
            // There is no separate "importing faults from <dir>" note any
            // more: the daemon digests new dumps on its way to answering
            // `ggo_faults`, so an import that cannot happen IS the fault
            // list failing, which is the `LoadState::Error` asserted
            // above. One failure, reported once.
            assert!(
                panel.notes.iter().any(|n| n.contains("device runs")),
                "the device history's own reason comes through: {:?}",
                panel.notes
            );

            // A populated list is never blanked by a reason: the note is
            // the empty state's, not the panel's.
            panel.rows = merge_rows(
                vec![perf(7, "2026-09-02T08:00:00Z")],
                Vec::new(),
                Vec::new(),
            );
            panel.state = LoadState::Ready;
            assert_eq!(panel.note(), None, "rows outrank reasons");
        });
    }

    /// End to end: the panel imports a dump the daemon left behind, lists
    /// it, and its click lands on that fault in the ONE center Reports
    /// tab -- the dock's whole job.
    #[gpui::test]
    async fn a_row_click_opens_that_report_in_the_center_tab(cx: &mut TestAppContext) {
        cx.update(|cx| {
            AppState::test(cx);
            init(cx);
        });
        let fs = FakeFs::new(cx.executor());
        let project = Project::test(fs, [], cx).await;
        let (multi_workspace, cx) =
            cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = multi_workspace.read_with(cx, |mw, _| mw.workspace().clone());

        let db = TestDb::new();
        let temp = tempfile::tempdir().expect("tempdir");
        let faults_dir = temp.path().join("faults");
        let fault_id = "2026-09-02_08-49-33_marker";
        write_dump(&faults_dir, fault_id);

        // The center tab reads the same fixture database the dock does; in
        // production both resolve `ggo_db::url` themselves. The tab is
        // built and aimed BEFORE it is added, because `open_charts_item`
        // refreshes on the way in -- an unaimed tab would read (and its
        // fault import would WRITE) the developer's real database.
        workspace.update_in(cx, |workspace, window, cx| {
            let item = cx.new(|cx| ggo_charts_panel::ChartsItem::new(workspace.weak_handle(), cx));
            let charts = item.read(cx).panel().clone();
            charts.update(cx, |charts, _| {
                // The tab resolves the clicked dump's raw path through
                // the daemon; pointed at the same in-process one it lands
                // in this fixture's directory rather than the developer's
                // real ~/.ggo/uartd/faults.
                charts.set_connect(test_connect(db.url(), &faults_dir));
            });
            workspace.add_item_to_active_pane(Box::new(item), None, true, window, cx);
        });

        let panel = workspace
            .read_with(cx, |workspace, cx| workspace.panel::<ReportsPanel>(cx))
            .expect("init adds the panel to every workspace");
        panel.update(cx, |panel, cx| {
            panel.set_connect(test_connect(db.url(), &faults_dir));
            panel.refresh(cx);
        });
        cx.run_until_parked();
        panel.read_with(cx, |panel, _| {
            let ids: Vec<&str> = panel.all_rows().iter().map(|row| row.id.as_str()).collect();
            assert_eq!(ids, [fault_id], "the imported dump is the list");
            assert_eq!(panel.all_rows()[0].kind, ReportKind::Fault);
        });

        panel.update_in(cx, |panel, window, cx| panel.click_row(0, window, cx));
        cx.run_until_parked();
        workspace.read_with(cx, |workspace, cx| {
            let items: Vec<_> = workspace
                .items_of_type::<ggo_charts_panel::ChartsItem>(cx)
                .collect();
            assert_eq!(items.len(), 1, "one Reports tab, re-focused");
            assert_eq!(
                items[0].read(cx).panel().read(cx).selected_fault_id(),
                Some(fault_id),
                "the click landed on the fault it named"
            );
        });
    }

    // ------------------------------------------------ layout / overflow

    /// The panel as the ROOT of a real window, so the layout tests below
    /// read bounds a prepaint actually produced and a resize redraws the
    /// panel at the new size. Nothing is loaded: the header and the empty
    /// state are chrome the panel paints on its own.
    fn ready_panel_in_window(
        cx: &mut TestAppContext,
    ) -> (gpui::Entity<ReportsPanel>, &mut gpui::VisualTestContext) {
        cx.update(|cx| {
            AppState::test(cx);
        });
        let (panel, cx) = cx.add_window_view(|_, cx| ReportsPanel::new(None, cx));
        cx.run_until_parked();
        (panel, cx)
    }

    /// Resize the window and let the panel redraw at the new size.
    fn resize(cx: &mut gpui::VisualTestContext, width: f32, height: f32) {
        cx.simulate_resize(gpui::size(px(width), px(height)));
        cx.run_until_parked();
    }

    fn wheel(cx: &mut gpui::VisualTestContext, at: gpui::Point<Pixels>, dx: f32, dy: f32) {
        cx.simulate_event(gpui::ScrollWheelEvent {
            position: at,
            delta: gpui::ScrollDelta::Pixels(gpui::point(px(dx), px(dy))),
            modifiers: gpui::Modifiers::default(),
            touch_phase: gpui::TouchPhase::default(),
        });
        cx.run_until_parked();
    }

    /// A list holding one fault row whose title is a realistic length --
    /// `ggo-uartd` writes "<kind>: <detail>", and a trap detail runs to
    /// most of a line.
    fn panel_with_one_long_titled_row(
        cx: &mut TestAppContext,
    ) -> (gpui::Entity<ReportsPanel>, &mut gpui::VisualTestContext) {
        let (panel, cx) = ready_panel_in_window(cx);
        panel.update(cx, |panel, cx| {
            let mut row = fault("2026-09-02_08-49-33_marker", "2026-09-02_08-49-33");
            row.detail = "trap: mcause=0x2 mepc=0x80000010 on /dev/ttyUSB1".to_string();
            panel.rows = merge_rows(Vec::new(), Vec::new(), vec![row]);
            panel.state = LoadState::Ready;
            panel.rebuild_visible();
            cx.notify();
        });
        cx.run_until_parked();
        (panel, cx)
    }

    /// A row's title sits in a ROW flex, where a child's automatic
    /// minimum is its content size -- and gpui measures a label's
    /// min-content UNWRAPPED (`elements/text.rs`: a wrap width comes only
    /// from a DEFINITE available width). So a fault title painted
    /// straight out of the dock, past an edge nothing scrolls back from.
    /// The title cell can shrink now, which gives the text a width to
    /// wrap to.
    ///
    /// Deliberately not a sideways scroller: the title is ordinary
    /// wrapping text, so once it can shrink the list has no horizontal
    /// range at all, and an `overflow_x_scroll` there would do nothing
    /// but swallow horizontal trackpad deltas.
    #[gpui::test]
    async fn test_a_a_long_row_title_wraps_inside_the_dock(cx: &mut TestAppContext) {
        let (_panel, cx) = panel_with_one_long_titled_row(cx);

        resize(cx, 1600., 700.);
        let wide = cx
            .debug_bounds("ggo-reports-row-title-0")
            .expect("row title bounds recorded at paint");

        resize(cx, 300., 700.);
        let list = cx
            .debug_bounds(LIST_SELECTOR)
            .expect("list bounds recorded at paint");
        let narrow = cx
            .debug_bounds("ggo-reports-row-title-0")
            .expect("row title bounds recorded at paint");

        assert!(
            narrow.origin.x + narrow.size.width <= list.origin.x + list.size.width,
            "the title must stay inside the list -- the dock's edge is \
             where it would otherwise be cut, with nothing to scroll it \
             back: list {list:?}, title {narrow:?}"
        );
        assert!(
            narrow.size.height >= wide.size.height * 2.,
            "and it must WRAP rather than be truncated: one line is \
             {wide:?}, narrow is {narrow:?}"
        );
    }

    /// Class C: the header wraps onto more rows in a narrow dock, and the
    /// note beneath it is unbounded -- a failed load prints the reason
    /// plus `ggo-db`'s install hint, several lines of it. In a short panel
    /// that chrome outgrew the window while the list, the panel's only
    /// scroller, shrank to nothing under it: the tail of the reason was
    /// cut off with nothing left to scroll it back. The list keeps
    /// [`MIN_LIST_HEIGHT`] now and the root column scrolls instead.
    #[gpui::test]
    async fn test_c_a_short_panel_keeps_the_list_and_scrolls_the_root(cx: &mut TestAppContext) {
        let (panel, cx) = ready_panel_in_window(cx);
        panel.update(cx, |panel, cx| {
            panel.state = LoadState::Error(format!(
                "reading reports failed: connection refused\n{}",
                ggo_db::INSTALL_HINT
            ));
            cx.notify();
        });
        cx.run_until_parked();
        resize(cx, 150., 200.);

        let before = cx
            .debug_bounds(LIST_SELECTOR)
            .expect("list bounds recorded at paint");
        assert!(
            before.size.height >= MIN_LIST_HEIGHT,
            "a wrapped header over a multi-line note must not crush the \
             list: {before:?}"
        );

        let header = cx
            .debug_bounds(HEADER_SELECTOR)
            .expect("header bounds recorded at paint");
        wheel(cx, header.center(), 0., -80.);

        let after = cx
            .debug_bounds(LIST_SELECTOR)
            .expect("list bounds after the scroll");
        assert!(
            after.origin.y < before.origin.y,
            "a downward wheel over the chrome must scroll the root column, \
             carrying the note's tail into view: before {:?}, after {:?}",
            before.origin,
            after.origin
        );
    }

    /// Class B: the header is a title, three filter chips and the refresh
    /// button. In a narrow dock one row pushed the chips -- the only way
    /// to unhide a kind -- past the panel's edge with nothing to scroll
    /// them back; it wraps onto more rows instead.
    #[gpui::test]
    async fn test_b_the_header_wraps_when_the_dock_is_narrow(cx: &mut TestAppContext) {
        let (_panel, cx) = ready_panel_in_window(cx);

        resize(cx, 1600., 700.);
        let wide = cx
            .debug_bounds(HEADER_SELECTOR)
            .expect("header bounds recorded at paint");

        resize(cx, 150., 700.);
        let narrow = cx
            .debug_bounds(HEADER_SELECTOR)
            .expect("header bounds recorded at paint");

        // Two rows of chips, not two headers: the row's own vertical
        // padding is paid once either way, so a wrapped header comes in a
        // little under twice a single row's height.
        assert!(
            narrow.size.height >= wide.size.height * 1.5,
            "a 150px-wide header must wrap onto at least two rows: \
             one row is {wide:?}, narrow is {narrow:?}"
        );
    }

    // ---------------------------------------------- per-row actions (P5)

    /// The dump `dump_panel_in_window` leaves on disk.
    const DUMP_FIXTURE_ID: &str = "2026-09-02_08-49-33_marker";

    /// A panel as the window ROOT, listing the one dump written under its
    /// fixture faults directory -- the only report kind that HAS a file,
    /// and so the only one whose three file entries are live.
    async fn dump_panel_in_window(
        cx: &mut TestAppContext,
    ) -> (
        tempfile::TempDir,
        TestDb,
        gpui::Entity<ReportsPanel>,
        &mut gpui::VisualTestContext,
    ) {
        cx.update(|cx| {
            AppState::test(cx);
        });
        let db = TestDb::new();
        let temp = tempfile::tempdir().expect("tempdir");
        let faults_dir = temp.path().join("faults");
        write_dump(&faults_dir, DUMP_FIXTURE_ID);

        let (panel, cx) = cx.add_window_view(|_, cx| ReportsPanel::new(None, cx));
        panel.update(cx, |panel, cx| {
            panel.set_connect(test_connect(db.url(), &faults_dir));
            panel.refresh(cx);
        });
        cx.run_until_parked();
        resize(cx, 600., 600.);
        (temp, db, panel, cx)
    }

    /// **P5.** A report row was a click and nothing else. It carries the
    /// three entries a file-backed row owes a reader now -- copy its
    /// path, reveal it, delete it -- each wrapped in its own selector.
    #[gpui::test]
    async fn test_a_report_rows_own_entries_are_painted(cx: &mut TestAppContext) {
        let (_temp, _db, panel, cx) = dump_panel_in_window(cx).await;
        panel.read_with(cx, |panel, _| {
            let rows = panel.all_rows();
            assert_eq!(rows.len(), 1, "the imported dump is the list");
            assert!(
                rows[0].path.is_some(),
                "a fault row carries the dump file it was read from"
            );
        });

        assert_eq!(row_copy_selector(0), "ggo-reports-row-copy-0");
        assert_eq!(row_reveal_selector(0), "ggo-reports-row-reveal-0");
        assert_eq!(row_delete_selector(0), "ggo-reports-row-delete-0");
        assert!(cx.debug_bounds("ggo-reports-row-copy-0").is_some());
        assert!(cx.debug_bounds("ggo-reports-row-reveal-0").is_some());
        assert!(cx.debug_bounds("ggo-reports-row-delete-0").is_some());
    }

    /// **P4 through P5's Delete.** Unlinking what a daemon wrote always
    /// confirms, the prompt names the file by name, and a cancelled
    /// confirm leaves it exactly where it was.
    #[gpui::test]
    async fn test_deleting_a_report_confirms_before_unlinking_the_dump(cx: &mut TestAppContext) {
        let (temp, _db, _panel, cx) = dump_panel_in_window(cx).await;
        let dump = temp
            .path()
            .join("faults")
            .join(format!("{DUMP_FIXTURE_ID}.log"));
        assert!(dump.is_file(), "the fixture dump is on disk to begin with");

        let delete = cx
            .debug_bounds("ggo-reports-row-delete-0")
            .expect("the row paints a Delete entry");
        cx.simulate_click(delete.center(), gpui::Modifiers::default());
        cx.run_until_parked();
        let (message, detail) = cx
            .pending_prompt()
            .expect("a delete must always confirm, never act on the click");
        assert!(
            message.contains(&format!("{DUMP_FIXTURE_ID}.log")),
            "the prompt names the file it is about to unlink: {message}"
        );
        assert!(
            detail.contains(DUMP_FIXTURE_ID),
            "and what survives it: {detail}"
        );
        cx.simulate_prompt_answer("Cancel");
        cx.run_until_parked();
        assert!(dump.is_file(), "a cancelled delete keeps the file");

        let delete = cx
            .debug_bounds("ggo-reports-row-delete-0")
            .expect("the row still paints its Delete entry");
        cx.simulate_click(delete.center(), gpui::Modifiers::default());
        cx.run_until_parked();
        assert!(cx.has_pending_prompt(), "the second delete confirms too");
        cx.simulate_prompt_answer("Delete");
        cx.run_until_parked();
        assert!(!dump.exists(), "a confirmed delete unlinks the dump");
    }

    /// **Reveal's enablement, which is the whole of that entry's
    /// honesty.** A dump under a folder this window has open resolves to
    /// a project entry, so the button is live; a file outside every open
    /// folder, and a row with no file at all, resolve to none -- and the
    /// entry is then `.disabled(..)` with the reason on it rather than a
    /// click that goes nowhere.
    #[gpui::test]
    async fn test_reveal_is_live_only_for_a_file_inside_an_open_folder(cx: &mut TestAppContext) {
        // A real worktree scan blocks on real IO.
        cx.executor().allow_parking();
        cx.update(|cx| {
            AppState::test(cx);
        });
        let db = TestDb::new();
        let temp = tempfile::tempdir().expect("tempdir");
        write_dump(&temp.path().join("faults"), DUMP_FIXTURE_ID);
        let elsewhere = tempfile::tempdir().expect("tempdir");

        // A REAL fs, because the question is whether a path on disk is
        // inside a worktree -- which a fake one cannot answer about the
        // directory the daemon actually wrote to.
        let project = Project::test(
            std::sync::Arc::new(project::RealFs::new(None, cx.executor())),
            [temp.path()],
            cx,
        )
        .await;
        let (multi_workspace, cx) =
            cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = multi_workspace.read_with(cx, |mw, _| mw.workspace().clone());
        let panel =
            cx.update(|_, cx| cx.new(|cx| ReportsPanel::new(Some(workspace.downgrade()), cx)));
        panel.update(cx, |panel, cx| {
            panel.set_connect(test_connect(db.url(), &temp.path().join("faults")));
            panel.refresh(cx);
        });
        cx.run_until_parked();

        panel.read_with(cx, |panel, cx| {
            let row = panel
                .all_rows()
                .first()
                .cloned()
                .expect("the imported dump is listed");
            assert!(
                panel.row_project_entry(&row, cx).is_some(),
                "a dump under an open folder is a project entry: {:?}",
                row.path
            );

            let mut outside = row.clone();
            outside.path = Some(elsewhere.path().join("stray.log"));
            assert!(
                panel.row_project_entry(&outside, cx).is_none(),
                "a file outside every open folder is not"
            );

            let mut fileless = row;
            fileless.path = None;
            assert!(
                panel.row_project_entry(&fileless, cx).is_none(),
                "and neither is a row with no file at all"
            );
        });
    }
}
