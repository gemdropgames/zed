//! Off-thread perf-run listing: reads the shared PostgreSQL database's
//! `run`/`cart` tables directly -- the charts-panel analog of
//! `ggo_world_panel::loader` / `ggo_sprite_panel::loader` (same one-shot
//! background-pass framing), reading the SAME database every other ggo
//! tool reaches through `ggo_db::pool_for`.
//!
//! **Per-run reads go through worldlib, not through this module.** F5.4
//! Task R1 moved ggo-ide's whole `backend/perf.rs` into
//! [`ggo_worldlib::charts::reports::perf_db`], so [`load_run_samples`] is
//! now a thin fan-out over that module's `run_frames`/`run_profile`/
//! `run_detail`/`run_uart` rather than the hand-copied SQL and
//! `FrameRow`/`ProfileRow` shadows this file carried through C2. Those
//! shadows are gone: [`FrameRow`]/[`ProfileRow`]/[`RunDetail`]/
//! [`UartLine`] are re-exports of worldlib's own types, so the panel and
//! ggo-ide are now decoding the same columns through the same code.
//!
//! [`list_runs`] is the one query that stays local, because worldlib has
//! no equivalent: `perf_db` exposes `carts` + `cart_runs` (ggo-ide's
//! two-level cart -> run drill-down), while this panel's picker is one
//! flat "every run, newest first" list across all carts.
//!
//! Unlike the other panels' loaders (a filesystem walk), everything here
//! goes through `ggo-db`'s connection pool, which lives on that crate's
//! own tokio runtime. `perf_db` (and [`list_runs`] below, matching it)
//! wraps its body in `ggo_db::block_on` and BLOCKS on it. That block is
//! only safe off the UI thread -- and `block_on` panics by design inside
//! a tokio runtime -- so every call site in this crate must be inside
//! `cx.background_spawn`, the same rule
//! `ggo_world_panel::loader::load_world` leans on for its own synchronous
//! fs walk, and R1's concern (5) restated.
//!
//! **There is no "missing database" state any more.** Before the
//! PostgreSQL migration every read here guarded on `db_path.exists()`,
//! because opening a missing SQLite file CREATED an empty, tableless one.
//! A url is not a file: `ggo_db::pool_for_async` migrates the database it
//! reaches, so an empty picker now comes back as zero rows from a real
//! `SELECT`, and a server that cannot be reached is an error carrying
//! [`ggo_db::INSTALL_HINT`] rather than a silent empty list.

use ggo_db::sqlx::postgres::PgRow;
use ggo_db::sqlx::{self, Row};
use ggo_worldlib::charts::reports::historic::{self, HistoricRunFrames};
use ggo_worldlib::charts::reports::perf_db;

// Worldlib owns these now (F5.4 Task R1). Re-exported rather than
// re-declared so `chart_set`'s specs, the KPI derivations in
// `ggo_worldlib::charts::reports::kpi`, and `ggo_emu_panel`'s ingest
// round-trip test all name one type -- a private copy is exactly how a
// panel's numbers drift from the page it mirrors.
pub use ggo_worldlib::charts::reports::perf_db::{FrameRow, ProfileRow, RunDetail, UartLine};

/// One row of the runs list: enough to identify a run in the picker.
/// Selecting one loads its samples through [`load_run_samples`].
#[derive(Debug, Clone, PartialEq)]
pub struct RunListing {
    pub id: i64,
    pub started_at: String,
    pub cart_name: String,
    pub label: Option<String>,
}

impl RunListing {
    /// The picker's display text: `"<cart> — <label>"` when the run has
    /// a non-empty label, else just the cart name -- mirrors how
    /// `RunPage.tsx`'s row title falls back on the TS side.
    pub fn display_title(&self) -> String {
        match self.label.as_deref() {
            Some(label) if !label.is_empty() => format!("{} — {label}", self.cart_name),
            _ => self.cart_name.clone(),
        }
    }
}

/// [`LIST_RUNS_SQL`]' select-list positions.
const LISTING_ID: usize = 0;
const LISTING_STARTED_AT: usize = 1;
const LISTING_CART_NAME: usize = 2;
const LISTING_LABEL: usize = 3;

/// One db row -> a [`RunListing`], or `None` if the row's leading columns
/// are NULL. `run.started_at` and `cart.name` are both nullable, and a run
/// missing either cannot be named in the picker -- it is skipped rather
/// than shown as a blank row. `label` is nullable by design (only the emu
/// pane's own runs carry one), so a NULL there is a `None`, not a skip.
fn row_to_listing(row: &PgRow) -> Option<RunListing> {
    Some(RunListing {
        id: row.try_get(LISTING_ID).ok()?,
        started_at: row.try_get(LISTING_STARTED_AT).ok()?,
        cart_name: row.try_get(LISTING_CART_NAME).ok()?,
        label: row.try_get(LISTING_LABEL).ok().flatten(),
    })
}

/// Every run across every cart, newest first. `started_at` is nullable
/// and PostgreSQL sorts NULLs FIRST under `DESC`, so `NULLS LAST` keeps a
/// run with no start stamp out of the top of the list -- the same
/// `ORDER BY` (and the same `id DESC` tie-break) `perf_db::run_index`
/// uses, so the panel's picker and ggo-ide's run list agree on which of
/// two runs is "newer".
const LIST_RUNS_SQL: &str = "SELECT r.id, r.started_at, c.name, r.label \
     FROM run r JOIN cart c ON c.id = r.cart_id \
     ORDER BY r.started_at DESC NULLS LAST, r.id DESC";

/// Every run across every cart, newest first -- the panel's picker feed.
///
/// A database with no runs in it reads as a clean empty list, not an
/// error: nothing is ingested until `ggo-ide`, `ggo-emu`, `ggo-server` or
/// this fork's emu pane records a run, and that is an ordinary "nothing
/// to show yet" state for a fresh machine, not a failure. A postgres that
/// cannot be reached IS an error, and carries [`ggo_db::INSTALL_HINT`].
///
/// **Blocking** -- off-thread only, see this module's doc.
pub fn list_runs(db_url: &str) -> Result<Vec<RunListing>, String> {
    ggo_db::block_on(async {
        let pool = ggo_db::pool_for_async(db_url).await?;
        let rows = sqlx::query(LIST_RUNS_SQL)
            .fetch_all(&pool)
            .await
            .map_err(|e| e.to_string())?;
        Ok(rows.iter().filter_map(row_to_listing).collect())
    })
}

// ------------------------------------------------------- per-run samples

/// Everything one run's detail view needs, fetched in a single
/// background pass: the frames every chart plots, the I$ profile rows the
/// per-function charts need, the `run` row the header's config line and
/// KPI budget come off, and the persisted guest UART the failure/panic
/// tables parse.
///
/// All four are UNFILTERED, exactly as worldlib hands them over: the
/// ignore filter is a view-level recalibration applied by whoever derives
/// from them (`ggo_worldlib::charts::reports::ignore::apply` /
/// `apply_profile`), never inside a query and never here. See
/// `charts::reports`'s module doc -- skipping it folds frame 0's
/// cold-cache burst into every KPI.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct RunSamples {
    pub frames: Vec<FrameRow>,
    pub profile: Vec<ProfileRow>,
    /// `None` when the run id has no `run` row (deleted mid-session, or
    /// an id that never existed) -- the header falls back to the picker's
    /// own listing rather than erroring.
    pub detail: Option<RunDetail>,
    /// Empty for a run recorded before UART persistence, and for a diag
    /// run whose serial log lives in `run_log` under a string id. That is
    /// NOT the same as "the run recorded UART with no failures in it" --
    /// `report::Diagnostics` is where the panel separates the two, and
    /// this emptiness is the only signal that can.
    pub uart: Vec<UartLine>,
}

/// One run's frames, profile rows, `run` row and UART.
///
/// **Blocking** (four `perf_db` calls, each one `ggo_db::block_on`), so it
/// must only ever be called from `cx.background_spawn` -- see this
/// module's doc.
///
/// A run id with no rows reads as an empty result rather than an error:
/// `perf_db`'s list queries answer that way, and so does its `run_detail`
/// (`None`).
///
/// Every query returns EVERY row -- no `LIMIT`, no SQL-side bucketing --
/// which is ggo-ide's behavior too. Ingest caps a run at 100,000 frames,
/// so this is a large scan; acceptable because it runs off-thread, once
/// per selection.
///
/// Errors are stringified with `{e:#}`, not `{e}`: `perf_db` builds its
/// `anyhow::Error`s out of `.context(..)`, whose plain `Display` shows
/// only the outermost context -- the alternate form appends the chain,
/// which is where the actual sqlx failure is.
pub fn load_run_samples(db_url: &str, run_id: i64) -> Result<RunSamples, String> {
    // `{e:#}` (not `{e}`): `perf_db` returns `anyhow::Error`s built from
    // `.context(..)`, and the plain `Display` shows only the outermost
    // context -- the alternate form appends the chain, which is where the
    // actual sqlx failure is.
    Ok(RunSamples {
        frames: perf_db::run_frames(db_url, run_id).map_err(|e| format!("{e:#}"))?,
        // A run with no profile rows is the norm (only a native
        // `--profile` capture writes them), so an empty result here is
        // not an error -- `perf_db` already answers list queries that way.
        profile: perf_db::run_profile(db_url, run_id).map_err(|e| format!("{e:#}"))?,
        detail: perf_db::run_detail(db_url, run_id).map_err(|e| format!("{e:#}"))?,
        uart: perf_db::run_uart(db_url, run_id).map_err(|e| format!("{e:#}"))?,
    })
}

/// The runs the historic overlay draws underneath `run_id`, newest first.
///
/// **What "prior" means here, exactly** -- the brief asks for this to be
/// stated rather than implied, and `run` has no more columns to lean on
/// than `runs` did (R1's concern (6), R3's finding):
///
/// * **Same cart.** `run.cart_id` of the selected run, read off the
///   `RunDetail` the same pass already fetched. This is ggo-ide's scoping
///   (its Reports page drills cart -> runs -> detail, so its `prior`
///   resource can only ever see one cart's runs) and it is the ONLY
///   differentiation the schema offers: there is still no run-kind column,
///   so a cart's runs may mix this panel's own captures, `ggo-emu` runs
///   and `ggo-server` ingests, and nothing distinguishes them. Overlaying
///   two different workloads of the same cart is the assumption the
///   feature is built on, here as there.
/// * **Lower `run.id`, taken in descending id order**, capped at
///   `HISTORIC_OPACITY.len()` (5) -- `historic::pick_prior_ids`, which is
///   `RunPage.tsx`'s `runs.filter(r => r.id < run.id).sort(desc).slice(0, 5)`.
///   That assumes ids are allocated in INGEST order (they are -- a
///   `BIGSERIAL` primary key), and therefore that "lower id" means
///   "ingested earlier". It does NOT mean "captured earlier": `started_at`
///   is written at ingest by whoever ingested, so a capture replayed into
///   the db today gets a higher id than a run that happened after it. The
///   picker above sorts on `started_at` while this sorts on `id`, exactly
///   as ggo-ide does -- matched rather than "fixed", because changing it
///   would make this panel and that page disagree about which five runs a
///   ghost is.
/// * **No liveness assumption at all**, and none needed: a perf run's
///   `run` row and every one of its `frame` rows are written inside one
///   transaction by `ggo_emu_panel::ingest`, so a prior run is never
///   observed half-ingested. Even if a foreign producer wrote one, the
///   overlay aligns by index and stops at the shorter series -- a short
///   ghost, never an error.
///
/// A cart with no earlier runs yields an empty vec, which is a state the
/// panel names rather than a failure.
///
/// **Blocking**, and each id costs one `perf_db::run_frames` call: up to
/// 1 + 5 of them on top of [`load_run_samples`]' four. Off-thread only --
/// `detail::load` is the one caller, inside `select_run`'s existing
/// background spawn, and there is deliberately no second, lazier load
/// path behind the Historic toggle.
pub fn load_prior_runs(
    db_url: &str,
    run_id: i64,
    cart_id: Option<i64>,
) -> Result<Vec<HistoricRunFrames>, String> {
    let Some(cart_id) = cart_id else {
        return Ok(Vec::new());
    };
    let runs = perf_db::cart_runs(db_url, cart_id).map_err(|e| format!("{e:#}"))?;
    historic::pick_prior_ids(run_id, &runs)
        .into_iter()
        .map(|id| {
            perf_db::run_frames(db_url, id)
                .map(|frames| HistoricRunFrames { id, frames })
                .map_err(|e| format!("{e:#}"))
        })
        .collect()
}

/// A url pointing at a socket directory that does not exist, so every read
/// through it fails to reach a server. The postgres analog of the old
/// "a file that exists but is not a database" fixture, shared with
/// `history`'s and the panel's own error-path tests.
#[cfg(test)]
pub(crate) const UNREACHABLE_DB_URL: &str =
    "postgres://ggo@localhost/ggo?host=/nonexistent/ggo-pg-socket";

#[cfg(test)]
mod tests {
    use super::*;
    use ggo_db::TestDb;

    /// Run every statement in order against `db`. Parents before children
    /// throughout: postgres enforces the `cart` -> `run` -> `frame` /
    /// `profile` / `uart` foreign keys the SQLite fixtures never did.
    fn exec(db: &TestDb, statements: &[&str]) {
        let pool = db.pool();
        ggo_db::block_on(async {
            for sql in statements {
                sqlx::query(sql).execute(&pool).await.unwrap();
            }
        });
    }

    #[test]
    fn display_title_falls_back_to_cart_name_when_unlabeled_or_empty() {
        let unlabeled = RunListing {
            id: 1,
            started_at: "2026-08-01T00:00:00Z".to_string(),
            cart_name: "demo".to_string(),
            label: None,
        };
        assert_eq!(unlabeled.display_title(), "demo");

        let empty_label = RunListing {
            label: Some(String::new()),
            ..unlabeled
        };
        assert_eq!(empty_label.display_title(), "demo");
    }

    #[test]
    fn display_title_shows_the_label_when_present() {
        let listing = RunListing {
            id: 1,
            started_at: "2026-08-01T00:00:00Z".to_string(),
            cart_name: "demo".to_string(),
            label: Some("arena".to_string()),
        };
        assert_eq!(listing.display_title(), "demo — arena");
    }

    /// A postgres nobody is listening on is an `Err`, not an empty picker:
    /// this is the only producer of the panel's `LoadState::Error`, and
    /// silently showing "no runs" over a database the panel simply could
    /// not reach would hide the one signal the user has that something is
    /// wrong. The hint is what makes the message actionable.
    #[test]
    fn list_runs_of_an_unreachable_database_is_an_error_that_says_what_to_do() {
        let message = list_runs(UNREACHABLE_DB_URL).expect_err("an unreachable server must fail");
        assert!(
            message.contains(ggo_db::INSTALL_HINT),
            "the error tells the user how to fix it: {message}"
        );
    }

    /// A fixture database on the real migrated schema, seeded with one
    /// cart and two runs -- pins both the newest-first ordering and the
    /// label/no-label column mapping in one pass.
    #[test]
    fn list_runs_reads_seeded_rows_newest_first() {
        let db = TestDb::new();
        exec(
            &db,
            &[
                "INSERT INTO cart (id, name) VALUES (1, 'demo')",
                "INSERT INTO run (id, cart_id, started_at, frames, label)
                 VALUES (1, 1, '2026-08-01T00:00:00Z', 3, 'first')",
                "INSERT INTO run (id, cart_id, started_at, frames, label)
                 VALUES (2, 1, '2026-08-02T00:00:00Z', 2, NULL)",
            ],
        );

        let runs = list_runs(db.url()).unwrap();
        assert_eq!(runs.len(), 2);
        assert_eq!(runs[0].id, 2, "newest started_at sorts first");
        assert_eq!(runs[0].label, None);
        assert_eq!(runs[1].id, 1);
        assert_eq!(runs[1].label.as_deref(), Some("first"));
    }

    /// Two runs ingested with the SAME `started_at` (a batch replay, or a
    /// producer with second-granularity timestamps) order by `id DESC` --
    /// the tie-break `perf_db::run_index` uses, so the panel's picker and
    /// ggo-ide's run list agree on which of the two is "newer". The lower
    /// id is inserted first, so a scan without the ORDER BY would come
    /// back ascending and fail this.
    ///
    /// The run with NO `started_at` at all sorts LAST rather than first,
    /// which is what `NULLS LAST` is for: postgres orders NULLs first
    /// under `DESC`, and a half-written run at the top of the picker is
    /// exactly the wrong place for it.
    #[test]
    fn list_runs_breaks_a_started_at_tie_by_id_descending_and_sorts_nulls_last() {
        let db = TestDb::new();
        exec(
            &db,
            &[
                "INSERT INTO cart (id, name) VALUES (1, 'demo')",
                "INSERT INTO run (id, cart_id, started_at, frames)
                 VALUES (1, 1, '2026-08-01T00:00:00Z', 1)",
                "INSERT INTO run (id, cart_id, started_at, frames)
                 VALUES (2, 1, '2026-08-01T00:00:00Z', 1)",
            ],
        );

        let ids: Vec<i64> = list_runs(db.url()).unwrap().iter().map(|r| r.id).collect();
        assert_eq!(ids, vec![2, 1], "equal started_at ties break by id DESC");
    }

    /// A run with no `started_at` cannot be named in the picker, so it is
    /// skipped rather than listed as a blank row -- [`row_to_listing`]'s
    /// one filtering rule, exercised through a real NULL column.
    #[test]
    fn list_runs_skips_a_run_that_has_no_started_at() {
        let db = TestDb::new();
        exec(
            &db,
            &[
                "INSERT INTO cart (id, name) VALUES (1, 'demo')",
                "INSERT INTO run (id, cart_id, started_at, frames) VALUES (1, 1, NULL, 1)",
                "INSERT INTO run (id, cart_id, started_at, frames)
                 VALUES (2, 1, '2026-08-01T00:00:00Z', 1)",
            ],
        );

        let ids: Vec<i64> = list_runs(db.url()).unwrap().iter().map(|r| r.id).collect();
        assert_eq!(ids, vec![2], "the unnameable run is dropped, not blanked");
    }

    #[test]
    fn list_runs_is_empty_for_a_freshly_migrated_db_with_no_runs() {
        let db = TestDb::new();
        assert_eq!(list_runs(db.url()).unwrap(), Vec::new());
    }

    // -------------------------------------------------- per-run samples

    // The three tests that used to live here decoded hand-built driver
    // rows through this module's own `row_to_frame`, which no longer
    // exists -- `perf_db` decodes those rows now. What each of
    // them was pinning, and where that landed, exactly:
    //
    // * **Select-list order.** Pinned by worldlib
    //   (`perf_db::tests::run_frames_returns_rows_in_frame_order_with_budget`
    //   asserts a whole `FrameRow` field for field against a seeded db).
    //   Genuinely relocated; not repeated here.
    // * **The NULL budget -> `None` case.** NOT pinned by worldlib -- no
    //   test there asserts `frame_budget_cycles: None`. It is pinned by
    //   this module's own
    //   `load_run_samples_of_a_run_without_profile_rows_is_not_an_error`
    //   below, which seeds a run with no `frame_budget_cycles` and asserts
    //   the `None`. Keep that assertion: it is the only one anywhere, and
    //   a budget line drawn at 0 instead of omitted is the regression.
    // * **The short-row case.** Gone, not relocated. `run_frames` reads a
    //   fixed select list by index rather than skipping a short row, so
    //   there is no behaviour left to test. Harmless: the select list is a
    //   literal in the same function, so a row can only be short if the
    //   query and the decoder are edited apart.
    //
    // What remains this crate's business is the fan-out below -- that
    // `load_run_samples` asks for all four pieces against the real
    // migrated schema, and that an unknown run reads as empty.

    /// The `{e:#}` stringification is load-bearing: `perf_db` wraps its
    /// failures in `.context(..)` layers whose plain `Display` shows ONLY
    /// the outermost context ("querying frame rows" -- true but useless),
    /// and the alternate form appends the chain, which is where the actual
    /// failure lives. A regression to `{e}` here keeps every test that
    /// only checks `is_err()` green while the panel's error banner stops
    /// saying what went wrong.
    ///
    /// The fixture is a REACHABLE database with the `frame` table dropped
    /// out from under the reader, which is the only way to reach a
    /// `.context(..)` layer at all: a connection failure is reported
    /// before any query is issued, so it would prove nothing about the
    /// chain.
    #[test]
    fn load_run_samples_errors_carry_the_context_chain_and_the_root_cause() {
        let db = TestDb::new();
        exec(&db, &["DROP TABLE frame"]);

        let message =
            load_run_samples(db.url(), 1).expect_err("a missing table must fail the read");
        assert!(
            message.contains("querying frame rows"),
            "the context layer names the failing step: {message}"
        );
        assert!(
            message.contains("frame") && message.contains("does not exist"),
            "and the chain continues past the context into postgres's own \
             complaint, which is the half that says WHAT is wrong: {message}"
        );
    }

    /// A database that cannot be reached at all fails before any query,
    /// with the hint that says how to fix it -- the other error shape the
    /// panel's banner has to render.
    #[test]
    fn load_run_samples_of_an_unreachable_database_says_what_to_do() {
        let message =
            load_run_samples(UNREACHABLE_DB_URL, 1).expect_err("an unreachable server must fail");
        assert!(
            message.contains(ggo_db::INSTALL_HINT),
            "the error tells the user how to fix it: {message}"
        );
    }

    /// A fixture on the real schema, seeded with two frames (inserted out
    /// of order, to pin `ORDER BY f.n`), one profile row and two UART
    /// lines -- proves all four queries decode against the ACTUAL column
    /// names/types.
    #[test]
    fn load_run_samples_reads_frames_in_frame_number_order_with_profile_rows() {
        let db = TestDb::new();
        exec(
            &db,
            &[
                "INSERT INTO cart (id, name) VALUES (1, 'demo')",
                "INSERT INTO run (id, cart_id, started_at, frames, frame_budget_cycles, label)
                 VALUES (1, 1, '2026-08-01T00:00:00Z', 2, 555549, 'arena')",
                "INSERT INTO frame
                   (run_id, n, instrs, i_hits, i_misses, d_hits, d_misses,
                    scanout_wire, blit_wire, miss_wire, wire_total, over_budget,
                    sc_upload, bg_evictions, apu_fetch_wire)
                 VALUES (1, 1, 1000, 0, 20, 0, 5, 164400, 30, 40, 200, 0, 7, 2, 9)",
                "INSERT INTO frame
                   (run_id, n, instrs, i_hits, i_misses, d_hits, d_misses,
                    scanout_wire, blit_wire, miss_wire, wire_total, over_budget,
                    sc_upload, bg_evictions, apu_fetch_wire)
                 VALUES (1, 0, 1000, 0, 10, 0, 5, 164400, 30, 40, 100, 0, 7, 2, 9)",
                "INSERT INTO profile (run_id, frame, caller, func, misses, evicted)
                 VALUES (1, 1, '', 'update_entities', 12, 4)",
                "INSERT INTO uart (perf_run_id, seq, text) VALUES (1, 0, '== GGO OS booted ==')",
                "INSERT INTO uart (perf_run_id, seq, text)
                 VALUES (1, 1, 'asset: MISS \"x.til\"')",
            ],
        );

        let samples = load_run_samples(db.url(), 1).unwrap();
        assert_eq!(samples.frames.len(), 2);
        assert_eq!(samples.frames[0].n, 0, "ORDER BY f.n, not insertion order");
        assert_eq!(samples.frames[0].wire_total, 100);
        assert_eq!(samples.frames[1].n, 1);
        assert_eq!(samples.frames[1].i_misses, 20);
        assert_eq!(samples.frames[0].scanout_wire, 164_400);
        assert_eq!(samples.frames[0].sc_upload, 7);
        assert_eq!(samples.frames[0].bg_evictions, 2);
        assert_eq!(samples.frames[0].apu_fetch_wire, 9);
        assert_eq!(samples.frames[0].frame_budget_cycles, Some(555_549));
        assert_eq!(samples.profile.len(), 1);
        assert_eq!(samples.profile[0].func, "update_entities");
        assert_eq!(samples.profile[0].evicted, 4);

        // The three columns the panel's own `FrameRow` shadow used to
        // drop, and which every KPI tile needs -- the reason that shadow
        // is gone.
        assert_eq!(samples.frames[0].i_hits, 0);
        assert_eq!(samples.frames[0].d_hits, 0);
        assert!(!samples.frames[0].over_budget);

        // The `run` row (config line + KPI budget) and the UART the
        // failure tables parse, both fetched in this same pass.
        let detail = samples.detail.as_ref().expect("the run row exists");
        assert_eq!(detail.id, 1);
        assert_eq!(detail.cart_name, "demo");
        assert_eq!(detail.frame_budget_cycles, Some(555_549));
        assert_eq!(samples.uart.len(), 2, "UART comes back in `seq` order");
        assert_eq!(samples.uart[1], "asset: MISS \"x.til\"");
    }

    /// The overwhelmingly common case: a run with frames but no profile
    /// rows (anything but a native `--profile` capture). The profile
    /// query must come back empty, not error.
    #[test]
    fn load_run_samples_of_a_run_without_profile_rows_is_not_an_error() {
        let db = TestDb::new();
        exec(
            &db,
            &[
                "INSERT INTO cart (id, name) VALUES (1, 'demo')",
                "INSERT INTO run (id, cart_id, started_at, frames) VALUES (1, 1, 'x', 1)",
                "INSERT INTO frame (run_id, n, wire_total) VALUES (1, 0, 42)",
            ],
        );

        let samples = load_run_samples(db.url(), 1).unwrap();
        assert_eq!(samples.frames.len(), 1);
        assert_eq!(samples.frames[0].wire_total, 42);
        assert!(samples.profile.is_empty());
        assert_eq!(
            samples.frames[0].frame_budget_cycles, None,
            "an un-set budget column reads as None, so no budget line is drawn"
        );
    }

    // ------------------------------------------------ historic overlays

    /// Two carts; cart 1 has runs 1..=8, cart 2 has run 100. Every run
    /// gets one frame carrying its own id as `wire_total`, so a prior
    /// run's frames can be traced back to the run they came from.
    fn seed_two_carts() -> TestDb {
        let db = TestDb::new();
        let pool = db.pool();
        ggo_db::block_on(async {
            for (id, name) in [(1i64, "demo"), (2, "other")] {
                sqlx::query("INSERT INTO cart (id, name) VALUES ($1, $2)")
                    .bind(id)
                    .bind(name)
                    .execute(&pool)
                    .await
                    .unwrap();
            }
            for (run_id, cart_id) in (1i64..=8).map(|i| (i, 1i64)).chain([(100, 2)]) {
                sqlx::query(
                    "INSERT INTO run (id, cart_id, started_at, frames) VALUES ($1, $2, 'x', 1)",
                )
                .bind(run_id)
                .bind(cart_id)
                .execute(&pool)
                .await
                .unwrap();
                sqlx::query("INSERT INTO frame (run_id, n, wire_total) VALUES ($1, 0, $2)")
                    .bind(run_id)
                    .bind(run_id)
                    .execute(&pool)
                    .await
                    .unwrap();
            }
        });
        db
    }

    /// The overlay's five runs: same cart, lower id, nearest first. The
    /// other cart's run 100 is excluded even though its id is higher than
    /// every candidate -- cart scoping is the only differentiation the
    /// schema offers, and it is applied before the id rule, not after.
    #[test]
    fn load_prior_runs_takes_the_five_nearest_lower_ids_of_the_same_cart() {
        let db = seed_two_carts();

        let prior = load_prior_runs(db.url(), 8, Some(1)).unwrap();
        let ids: Vec<i64> = prior.iter().map(|r| r.id).collect();
        assert_eq!(ids, vec![7, 6, 5, 4, 3], "capped at the ramp's five steps");
        assert_eq!(
            prior[0].frames.len(),
            1,
            "each prior run brings its own frames"
        );
        assert_eq!(
            prior[0].frames[0].wire_total, 7,
            "and they are that run's frames, not the selected run's"
        );
    }

    /// A cart's FIRST run has nothing behind it. Not an error, and not an
    /// occasion to reach into another cart.
    #[test]
    fn load_prior_runs_of_a_carts_first_run_is_empty() {
        let db = seed_two_carts();
        assert!(load_prior_runs(db.url(), 1, Some(1)).unwrap().is_empty());
        // ...and a run whose `run` row is gone has no cart to scope to --
        // answered without touching the database at all.
        assert!(
            load_prior_runs(UNREACHABLE_DB_URL, 8, None)
                .unwrap()
                .is_empty()
        );
    }

    /// A run id with no rows at all (deleted/never-ingested) is an empty
    /// sample set, not an error -- the panel shows its no-samples message.
    #[test]
    fn load_run_samples_of_an_unknown_run_is_empty() {
        let db = TestDb::new();
        assert_eq!(
            load_run_samples(db.url(), 999).unwrap(),
            RunSamples::default()
        );
    }
}
