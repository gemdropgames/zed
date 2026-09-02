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
//! [`merge_rows`], which is pure and separately tested. Faults are
//! imported from `~/.ggo/uartd/faults` into `ggo_ide.db` on every load
//! (`faults::import` is idempotent), because the daemon writes files and
//! nothing else ingests them.

use std::collections::HashSet;
use std::path::PathBuf;
use std::time::Duration;

use gpui::{
    Action, AnyElement, App, ClickEvent, Context, EventEmitter, FocusHandle, Focusable,
    IntoElement, Pixels, Render, Task, WeakEntity, Window, actions, div, px, uniform_list,
};
use ui::prelude::*;
use ui::{ListItem, Tooltip};
use workspace::Workspace;
use workspace::dock::{DockPosition, Panel, PanelEvent};

use ggo_charts_panel::history::{self, HISTORY_LIMIT};
use ggo_charts_panel::loader::{self, RunListing};
use ggo_charts_panel::{RunSummary, open_charts_item};
use ggo_worldlib::charts::reports::faults::{self, FaultRow};

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
const NO_HOME: &str = "no home directory, so ~/.ggo/ggo_ide.db cannot be resolved";
/// The daemon appends to its faults directory while the panel is open, so
/// a visible panel re-reads on a timer rather than only on activation.
const POLL_INTERVAL: Duration = Duration::from_secs(5);

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
    pub when: String,
    pub trailer: String,
    /// The DEVICE run a fault probably happened during -- never a perf
    /// run id (see `faults::probable_run`).
    pub run_id: Option<String>,
}

/// Merge the three sources into one list, newest first. Timestamps are
/// `YYYY-MM-DD_HH-MM-SS` from every producer, so a string compare orders
/// them; ties fall back to [`ReportKind::rank`].
pub fn merge_rows(
    perf: Vec<RunListing>,
    device: Vec<RunSummary>,
    faults: Vec<FaultRow>,
) -> Vec<ReportRow> {
    let mut rows: Vec<ReportRow> = Vec::with_capacity(perf.len() + device.len() + faults.len());
    for run in faults {
        rows.push(ReportRow {
            kind: ReportKind::Fault,
            id: run.id,
            perf_id: None,
            title: format!("{}: {}", run.kind, run.detail),
            when: run.at,
            trailer: run.kind,
            run_id: run.run_id,
        });
    }
    for run in device {
        let verdict = run.verdict.unwrap_or_else(|| "no verdict".to_string());
        rows.push(ReportRow {
            kind: ReportKind::Device,
            id: run.id.clone(),
            perf_id: None,
            title: run.id,
            when: run.started_at,
            trailer: format!("{} · {verdict}", run.state),
            run_id: None,
        });
    }
    for run in perf {
        rows.push(ReportRow {
            kind: ReportKind::Perf,
            id: run.id.to_string(),
            perf_id: Some(run.id),
            title: run.display_title(),
            when: run.started_at.clone(),
            trailer: String::new(),
            run_id: None,
        });
    }
    rows.sort_by(|a, b| b.when.cmp(&a.when).then(a.kind.rank().cmp(&b.kind.rank())));
    rows
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
    /// Indices into `rows` that the filter chips let through, in display
    /// order -- what a click's `ix` means.
    visible: Vec<usize>,
    state: LoadState,
    hidden: HashSet<ReportKind>,
    generation: u64,
    db_path_override: Option<PathBuf>,
    diag_db_path_override: Option<PathBuf>,
    faults_dir_override: Option<PathBuf>,
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
            visible: Vec::new(),
            state: LoadState::Empty,
            hidden: HashSet::new(),
            generation: 0,
            db_path_override: None,
            diag_db_path_override: None,
            faults_dir_override: None,
            _load_task: None,
            _poll_task: None,
        }
    }

    /// This app's own database, `~/.ggo/ggo_ide.db`.
    fn db_path(&self) -> Option<PathBuf> {
        self.db_path_override
            .clone()
            .or_else(ggo_common::default_db_path)
    }

    /// `ggo-diag`'s database, a different file owned by a different tool.
    fn diag_db_path(&self) -> Option<PathBuf> {
        self.diag_db_path_override
            .clone()
            .or_else(ggo_common::default_diag_db_path)
    }

    /// Where `ggo-uartd` drops its dumps. Same `HOME`/`USERPROFILE` rule
    /// as [`ggo_common::default_db_path`], and the same directory the
    /// charts panel resolves for a dump's raw path.
    fn faults_dir(&self) -> Option<PathBuf> {
        if let Some(dir) = self.faults_dir_override.clone() {
            return Some(dir);
        }
        let home = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE"))?;
        Some(
            PathBuf::from(home)
                .join(".ggo")
                .join("uartd")
                .join("faults"),
        )
    }

    /// Read all three sources off-thread and replace the list. Every read
    /// is blocking (each spins its own current-thread tokio runtime), so
    /// none of this may touch the UI thread. A load that lands after a
    /// newer one started is dropped on the generation guard.
    pub fn refresh(&mut self, cx: &mut Context<Self>) {
        let Some(ide_db) = self.db_path() else {
            self.state = LoadState::Error(NO_HOME.to_string());
            cx.notify();
            return;
        };
        let diag_db = self.diag_db_path();
        let faults_dir = self.faults_dir();
        self.generation += 1;
        let generation = self.generation;
        // A refresh over an already-painted list must not blank it: the
        // poll runs every 5 s and would flash the loading line each time.
        if self.rows.is_empty() {
            self.state = LoadState::Loading;
        }
        cx.notify();
        let load = cx.background_spawn(async move {
            if let Some(dir) = faults_dir.as_ref()
                && let Err(error) = faults::import(dir, &ide_db)
            {
                log::warn!("reports: importing faults from {}: {error}", dir.display());
            }
            let mut failure = None;
            let perf = match loader::list_runs(&ide_db) {
                Ok(runs) => runs,
                Err(error) => {
                    failure = Some(error);
                    Vec::new()
                }
            };
            let device = match diag_db {
                Some(diag_db) => history::load(&diag_db, &ide_db, HISTORY_LIMIT).runs,
                None => Vec::new(),
            };
            let faults = match faults::list(&ide_db, HISTORY_LIMIT) {
                Ok(rows) => rows,
                Err(error) => {
                    failure = failure.or(Some(error));
                    Vec::new()
                }
            };
            (merge_rows(perf, device, faults), failure)
        });
        self._load_task = Some(cx.spawn(async move |this, cx| {
            let (rows, failure) = load.await;
            this.update(cx, |this, cx| {
                if this.generation != generation {
                    return;
                }
                this.rows = rows;
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
    pub fn rows(&self) -> &[ReportRow] {
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

    /// Read reports from `path` instead of `~/.ggo/ggo_ide.db`. Test hook:
    /// production resolves its own path.
    pub fn set_db_path_override(&mut self, path: PathBuf) {
        self.db_path_override = Some(path);
    }

    /// Read device runs from `path` instead of `~/.ggo/diag.db`.
    pub fn set_diag_db_path_override(&mut self, path: PathBuf) {
        self.diag_db_path_override = Some(path);
    }

    /// Import dumps from `path` instead of `~/.ggo/uartd/faults`.
    pub fn set_faults_dir_override(&mut self, path: PathBuf) {
        self.faults_dir_override = Some(path);
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
                                Icon::new(row.kind.icon())
                                    .size(IconSize::Small)
                                    .color(Color::Muted),
                            )
                            .child(Label::new(row.title.clone()).size(LabelSize::Small))
                            .child(div().flex_1())
                            .child(
                                Label::new(row.when.clone())
                                    .size(LabelSize::XSmall)
                                    .color(Color::Muted),
                            ),
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

    /// The one line under the header when the list has nothing to say for
    /// itself: a reason, never an empty panel.
    fn render_note(&self) -> Option<impl IntoElement> {
        let note = match &self.state {
            LoadState::Empty => EMPTY_MESSAGE.to_string(),
            LoadState::Loading => LOADING_MESSAGE.to_string(),
            LoadState::Error(error) => error.clone(),
            LoadState::Ready => return None,
        };
        Some(
            div().px_2().py_1().child(
                ggo_common::CopyableText::new("ggo-reports-note", note).size(LabelSize::Small),
            ),
        )
    }
}

impl Render for ReportsPanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let row_count = self.visible.len();
        v_flex()
            .key_context(KEY_CONTEXT)
            .size_full()
            .track_focus(&self.focus_handle)
            .bg(cx.theme().colors().panel_background)
            .on_action(cx.listener(|this, _: &Refresh, _, cx| this.refresh(cx)))
            .child(self.render_header(cx))
            .children(self.render_note())
            .child(
                div().id("ggo-reports-list").flex_1().min_h_0().child(
                    uniform_list(
                        "ggo-reports-rows",
                        row_count,
                        cx.processor(|this, range: std::ops::Range<usize>, _window, cx| {
                            range
                                .filter_map(|ix| {
                                    let row = this
                                        .visible
                                        .get(ix)
                                        .and_then(|row_ix| this.rows.get(*row_ix))
                                        .cloned()?;
                                    Some(this.render_row(ix, &row, cx))
                                })
                                .collect()
                        }),
                    )
                    .size_full(),
                ),
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
        self.refresh(cx);
        self._poll_task = Some(cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(POLL_INTERVAL).await;
                if this.update(cx, |this, cx| this.refresh(cx)).is_err() {
                    return;
                }
            }
        }));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::TestAppContext;
    use project::{FakeFs, Project};
    use workspace::{AppState, MultiWorkspace};

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
        assert_eq!(rows[0].trailer, "marker");
        assert_eq!(rows[0].run_id.as_deref(), Some("d1"));
        assert_eq!(rows[2].title, "wilds — run7");
        assert_eq!(
            rows[2].perf_id,
            Some(7),
            "the click opens a perf run by number, never by re-parsing its display id"
        );
        assert_eq!(rows[1].trailer, "done · PASS");
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
            assert_eq!(panel.rows().len(), 3, "hiding is not forgetting");
        });
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

        let temp = tempfile::tempdir().expect("tempdir");
        let ide_db = temp.path().join("ggo_ide.db");
        let faults_dir = temp.path().join("faults");
        let fault_id = "2026-09-02_08-49-33_marker";
        write_dump(&faults_dir, fault_id);

        // The center tab reads the same fixture files the dock does; in
        // production both resolve `~/.ggo` themselves. The tab is built
        // and aimed BEFORE it is added, because `open_charts_item`
        // refreshes on the way in -- and that refresh CLONES `diag.db`'s
        // runs into `ggo_ide.db`, i.e. an unaimed tab would drag the
        // developer's real device runs into this fixture database.
        workspace.update_in(cx, |workspace, window, cx| {
            let item = cx.new(|cx| ggo_charts_panel::ChartsItem::new(workspace.weak_handle(), cx));
            let charts = item.read(cx).panel().clone();
            charts.update(cx, |charts, _| {
                charts.set_db_path_override(ide_db.clone());
                charts.set_diag_db_path_override(temp.path().join("diag.db"));
            });
            workspace.add_item_to_active_pane(Box::new(item), None, true, window, cx);
        });

        let panel = workspace
            .read_with(cx, |workspace, cx| workspace.panel::<ReportsPanel>(cx))
            .expect("init adds the panel to every workspace");
        panel.update(cx, |panel, cx| {
            panel.set_db_path_override(ide_db.clone());
            panel.set_diag_db_path_override(temp.path().join("diag.db"));
            panel.set_faults_dir_override(faults_dir.clone());
            panel.refresh(cx);
        });
        cx.run_until_parked();
        panel.read_with(cx, |panel, _| {
            let ids: Vec<&str> = panel.rows().iter().map(|row| row.id.as_str()).collect();
            assert_eq!(ids, [fault_id], "the imported dump is the list");
            assert_eq!(panel.rows()[0].kind, ReportKind::Fault);
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
}
