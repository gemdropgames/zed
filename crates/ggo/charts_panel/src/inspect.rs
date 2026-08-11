//! The two per-function I$ surfaces' view models: the I$ profile table
//! (every function's totals across the run, sortable by misses) and
//! click-to-inspect (one frame's callers/callees, opened by clicking a
//! point on a plot).
//!
//! Mirrors `reports.rs`'s `profile_section` and `inspect_panel`, which
//! between them are the last two things ggo-ide's run-detail page showed
//! that this panel did not. The aggregation and the grouping themselves
//! are `ggo_worldlib::charts::reports::{profile, inspect}` (F5.4 Task R1);
//! what lives here is the assembly the panel would otherwise do at render
//! time, and the wording of what each surface says when it has nothing to
//! show.
//!
//! # Nothing here runs on the UI thread except a lookup
//!
//! [`build`] is called from `detail::build`, inside `select_run`'s
//! background spawn -- so the whole-run work (aggregating the table in
//! BOTH sort directions, and grouping the profile rows by frame) happens
//! off-thread, once per selection, under the guard `detail`'s module doc
//! describes.
//!
//! That leaves the two interactions with nothing to derive:
//!
//! * the sort header swaps between two already-sorted vectors
//!   ([`ProfileTable::rows`]) -- it does NOT re-sort, and it does not
//!   reverse either. `profile::aggregate_profile_sorted` is the sort, in
//!   both directions, and this crate never reimplements the ordering it
//!   chose (R1 ported it out of the widget for exactly that reason).
//! * a click resolves one frame's rows through [`FrameProfile`]'s index
//!   and groups only those ([`Profiles::inspect`]). The O(rows in the
//!   run) half -- the scan that finds them -- already happened in
//!   [`build`]; what is left is O(rows on the clicked frame), which is
//!   the handful of functions that missed during a single frame.
//!
//! # The ignore filter
//!
//! Applied HERE too, through the caller's set (`chart_set::ignore_set`,
//! the same one `report::build` and `chart_set::build_charts` use), and
//! for the same reason R1's concern (1) gives: no derivation in worldlib
//! applies it for you, and a table summarising a different frame set from
//! the KPI tiles directly above it is worse than no table. `build` keeps
//! the UNFILTERED row count as well, because "this run was never
//! profiled" and "this run's only profile rows are on an ignored frame"
//! are different facts and only the raw rows can tell them apart.

use std::collections::HashMap;
use std::collections::HashSet;

use ggo_worldlib::charts::reports::ignore;
use ggo_worldlib::charts::reports::inspect::{ProfileGroup, group_frame_profile};
use ggo_worldlib::charts::reports::profile::{ProfileAgg, aggregate_profile_sorted};

use crate::loader::ProfileRow;

/// The run recorded no profile rows at all.
///
/// ggo-ide's `NO_PROFILE_DATA_TEXT` says the same thing; the clause about
/// the emulator pane is this fork's, and is checked rather than assumed:
/// `ggo_emu_panel::drive` passes `None` for both `idump` and `ddump` when
/// it builds a run's perf JSON (function-level attribution needs the
/// cart's companion ELF and `ggo-emu`'s DWARF tooling, neither of which
/// `ggo-emu-core` has), so `ingest::parse_output` finds no `"profile"`
/// section and writes no rows. Every profile row in `ggo_ide.db` was
/// therefore ingested from a native `ggo-emu --profile <elf>` capture.
pub const NO_PROFILE_DATA: &str = "no profile data for this run — the emulator pane does not record it; \
     a native `ggo-emu --profile <elf>` capture does";

/// The run recorded profile rows, but every one of them is on an ignored
/// frame -- so the table has nothing to show even though the run WAS
/// profiled. ggo-ide's equivalent ends "remove a chip above to see it";
/// this panel has no chip editor yet (its ignore set is the fixed `{0}`),
/// so it states the situation without offering an action that does not
/// exist. Same split, and same reason for it, as `report`'s
/// `NO_FRAMES_RECORDED` vs `ALL_FRAMES_IGNORED`.
pub const ALL_PROFILE_IGNORED: &str = "this run's only profile rows are on ignored frames";

/// The selected frame has no per-function rows.
///
/// **A state, not a cause** -- the lesson R2's review cost a blocker for.
/// The signal is exactly "the `profile` table holds no row for this run
/// and this frame", and that is all this says. "This frame missed
/// nothing" and "this frame executed no instrumented function" are both
/// readings the absence of rows cannot distinguish between, so neither is
/// claimed. (ggo-ide asserts the first of them --
/// `"No I$ misses recorded this frame."` -- which is the same shape of
/// over-claim its console section avoids; deviated from deliberately.)
pub const NO_FRAME_PROFILE: &str = "no per-function rows recorded for this frame";

/// Both profile surfaces, derived once per run selection.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Profiles {
    table: ProfileTable,
    frames: FrameProfile,
    /// Whether the run recorded ANY profile rows, before the ignore
    /// filter -- what separates [`NO_PROFILE_DATA`] from
    /// [`ALL_PROFILE_IGNORED`].
    recorded: bool,
}

/// Derive both surfaces from a run's raw profile rows.
///
/// `rows` is the run's `profile` table verbatim (`perf_db::run_profile`);
/// `ignored` is the panel's one ignore set. **Blocking-free but not
/// free**: sorts and aggregates the whole set, so it belongs in
/// `detail::build`, not in a listener and not in a render.
pub fn build(rows: &[ProfileRow], ignored: &HashSet<i64>) -> Profiles {
    let kept = ignore::apply_profile(rows, ignored);
    Profiles {
        table: ProfileTable::build(&kept),
        frames: FrameProfile::build(&kept),
        recorded: !rows.is_empty(),
    }
}

impl Profiles {
    pub fn table(&self) -> &ProfileTable {
        &self.table
    }

    /// What the I$ profile table shows instead of rows, or `None` when it
    /// has some. Three states, because they are three different facts:
    /// never profiled, profiled but only on ignored frames, or populated.
    pub fn table_empty_state(&self) -> Option<&'static str> {
        match (self.table.is_empty(), self.recorded) {
            (false, _) => None,
            (true, false) => Some(NO_PROFILE_DATA),
            (true, true) => Some(ALL_PROFILE_IGNORED),
        }
    }

    /// The inspect pane for `frame`, as opened by a click on chart `chart`.
    ///
    /// A frame is only reachable from a plot's own x-axis, which is the
    /// ignore-filtered one, so the frame passed here is never itself
    /// ignored -- the filtered rows are the right set to group.
    pub fn inspect(&self, chart: usize, frame: i64) -> FrameInspect {
        FrameInspect {
            chart,
            frame,
            groups: self.frames.groups_for(frame),
            recorded: self.recorded,
        }
    }
}

// ------------------------------------------------------- the profile table

/// The I$ profile table's rows, in both sort directions.
///
/// Two vectors rather than one plus a `reverse()` at render time: the
/// sort is `profile::aggregate_profile_sorted`'s to own (R1 ported it out
/// of ggo-ide's widget precisely so the panel would stop expressing it),
/// and holding both directions makes a header click a pointer swap
/// instead of an aggregation on the UI thread. The cost is one extra
/// vector of one row per distinct FUNCTION -- not per row and not per
/// frame -- which is a table a human is expected to read.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct ProfileTable {
    descending: Vec<ProfileAgg>,
    ascending: Vec<ProfileAgg>,
}

impl ProfileTable {
    fn build(rows: &[ProfileRow]) -> Self {
        Self {
            descending: aggregate_profile_sorted(rows, false),
            ascending: aggregate_profile_sorted(rows, true),
        }
    }

    /// The rows in the requested order. Both orders came out of
    /// `aggregate_profile_sorted`; nothing here sorts, reverses or
    /// re-ranks them.
    pub fn rows(&self, ascending: bool) -> &[ProfileAgg] {
        if ascending {
            &self.ascending
        } else {
            &self.descending
        }
    }

    pub fn is_empty(&self) -> bool {
        self.descending.is_empty()
    }
}

// --------------------------------------------------------- click-to-inspect

/// A run's profile rows, grouped by frame so one frame's can be found
/// without re-scanning the run.
///
/// The rows are stably sorted by frame at build time and `at` records
/// each frame's half-open range in them. Stability is load-bearing:
/// `inspect::group_frame_profile` is insertion-order-stable (its doc says
/// so -- ties preserve first-seen row order, which is what makes it match
/// `RunPage.tsx`'s `selTree`), so a stable sort by frame leaves each
/// frame's rows in exactly the relative order a scan of the whole set
/// would have visited them in. `the_index_groups_a_frame_exactly_as_a_
/// whole_run_scan_would` is what holds that.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct FrameProfile {
    rows: Vec<ProfileRow>,
    at: HashMap<i64, (usize, usize)>,
}

impl FrameProfile {
    fn build(rows: &[ProfileRow]) -> Self {
        let mut rows = rows.to_vec();
        rows.sort_by_key(|r| r.frame);
        let mut at: HashMap<i64, (usize, usize)> = HashMap::new();
        let mut start = 0;
        for i in 0..rows.len() {
            let last = i + 1 == rows.len();
            if last || rows[i + 1].frame != rows[i].frame {
                at.insert(rows[i].frame, (start, i + 1));
                start = i + 1;
            }
        }
        Self { rows, at }
    }

    /// One frame's caller/callee groups, sorted misses-descending --
    /// `inspect::group_frame_profile` over that frame's rows alone.
    fn groups_for(&self, frame: i64) -> Vec<ProfileGroup> {
        let Some(&(start, end)) = self.at.get(&frame) else {
            return Vec::new();
        };
        group_frame_profile(&self.rows[start..end], frame)
    }
}

/// What the inspect pane shows for one clicked frame.
#[derive(Debug, Clone, PartialEq)]
pub struct FrameInspect {
    /// The chart whose click opened this. The pane renders directly
    /// beneath that chart: ggo-ide has one fixed slot for it (between the
    /// tile-working-set chart and the per-function charts), which works
    /// on a full-width page but not in a 360 px dock, where a fixed slot
    /// is usually scrolled off-screen from wherever the click happened.
    pub chart: usize,
    pub frame: i64,
    groups: Vec<ProfileGroup>,
    recorded: bool,
}

impl FrameInspect {
    pub fn groups(&self) -> &[ProfileGroup] {
        &self.groups
    }

    /// What the pane says instead of rows, or `None` when it has rows.
    ///
    /// Asked for by the renderer rather than passed to it as a literal:
    /// R2's review blocked an empty-state sentence that asserted a cause,
    /// and R3's found that literals at the call site let that exact
    /// sentence be reinstated with the whole suite still green. A test
    /// can read this; it cannot read a painted string.
    pub fn empty_state(&self) -> Option<&'static str> {
        if !self.groups.is_empty() {
            return None;
        }
        // A run that was never profiled says so; a run that WAS says only
        // what is true of this frame. (ggo-ide keys this off its
        // ignore-filtered slice, so a run whose only rows are on frame 0
        // gets told it has no profile data at all -- false, and avoided
        // here by asking the unfiltered signal.)
        Some(if self.recorded {
            NO_FRAME_PROFILE
        } else {
            NO_PROFILE_DATA
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(frame: i64, caller: &str, func: &str, misses: i64, evicted: i64) -> ProfileRow {
        ProfileRow {
            frame,
            caller: caller.to_string(),
            func: func.to_string(),
            misses,
            evicted,
        }
    }

    fn ignored() -> HashSet<i64> {
        HashSet::from([0])
    }

    fn sample_rows() -> Vec<ProfileRow> {
        vec![
            row(0, "boot", "boot", 900, 400),
            row(1, "main", "update", 30, 2),
            row(1, "main", "render", 10, 8),
            row(2, "main", "update", 5, 1),
            row(2, "render", "blit", 40, 0),
        ]
    }

    // ------------------------------------------------------ the sort

    #[test]
    fn the_table_totals_every_kept_frames_rows_by_function() {
        let profiles = build(&sample_rows(), &ignored());
        let rows = profiles.table().rows(false);
        let named: Vec<(&str, i64, i64)> = rows
            .iter()
            .map(|a| (a.func.as_str(), a.misses, a.evicted))
            .collect();
        assert_eq!(
            named,
            vec![("blit", 40, 0), ("update", 35, 3), ("render", 10, 8)],
            "descending by misses, and frame 0's burst is excluded"
        );
    }

    /// The header toggle is a direction, not a re-sort: both directions
    /// are `aggregate_profile_sorted`'s own output, so toggling twice is
    /// a round trip and the two are exact inverses (which is what keeps
    /// the `evicted`/name tie-breaks mirrored rather than re-decided).
    #[test]
    fn toggling_the_sort_reverses_the_order_and_round_trips() {
        let profiles = build(&sample_rows(), &ignored());
        let table = profiles.table();
        let desc: Vec<&str> = table.rows(false).iter().map(|a| a.func.as_str()).collect();
        let asc: Vec<&str> = table.rows(true).iter().map(|a| a.func.as_str()).collect();
        assert_eq!(asc, desc.iter().rev().copied().collect::<Vec<_>>());
        assert_eq!(
            table.rows(false),
            aggregate_profile_sorted(&ignore::apply_profile(&sample_rows(), &ignored()), false),
            "the panel's order IS worldlib's, not a re-derivation of it"
        );
    }

    /// Ties on misses must survive the toggle in mirrored order, or the
    /// two directions stop being inverses and a double click moves rows.
    #[test]
    fn the_sort_is_stable_across_a_tie() {
        let rows = vec![
            row(1, "main", "alpha", 5, 3),
            row(1, "main", "beta", 5, 3),
            row(1, "main", "gamma", 5, 3),
        ];
        let profiles = build(&rows, &ignored());
        let table = profiles.table();
        let desc: Vec<&str> = table.rows(false).iter().map(|a| a.func.as_str()).collect();
        assert_eq!(desc, vec!["alpha", "beta", "gamma"], "ties break by name");
        let asc: Vec<&str> = table.rows(true).iter().map(|a| a.func.as_str()).collect();
        assert_eq!(asc, vec!["gamma", "beta", "alpha"]);
    }

    // ------------------------------------------------- the empty states

    #[test]
    fn a_run_with_no_profile_rows_at_all_says_so() {
        let profiles = build(&[], &ignored());
        assert_eq!(profiles.table_empty_state(), Some(NO_PROFILE_DATA));
        assert_eq!(
            profiles.inspect(0, 3).empty_state(),
            Some(NO_PROFILE_DATA),
            "the inspect pane agrees with the table about the run"
        );
    }

    /// Profiled, but only on the ignored frame: the table is empty for a
    /// reason that has nothing to do with the run never being profiled,
    /// and saying the latter would be false.
    #[test]
    fn a_run_profiled_only_on_an_ignored_frame_is_not_an_unprofiled_run() {
        let profiles = build(&[row(0, "boot", "boot", 9, 1)], &ignored());
        assert_eq!(profiles.table_empty_state(), Some(ALL_PROFILE_IGNORED));
        assert_ne!(profiles.table_empty_state(), Some(NO_PROFILE_DATA));
        assert_eq!(
            profiles.inspect(0, 4).empty_state(),
            Some(NO_FRAME_PROFILE),
            "and a frame of that run reports only what is true of the frame"
        );
    }

    /// The empty state is a FACT about the rows, not a claim about what
    /// the guest did. Absence of rows cannot distinguish "missed nothing"
    /// from "nothing instrumented ran", so the sentence names neither --
    /// the same rule `report::NO_UART`'s doc states, and the one R2's
    /// review blocked a message for breaking.
    #[test]
    fn the_empty_frame_state_names_a_state_and_never_a_cause() {
        let profiles = build(&sample_rows(), &ignored());
        let empty = profiles.inspect(0, 3);
        assert!(empty.groups().is_empty());
        let message = empty.empty_state().expect("a frame with no rows");
        assert_eq!(
            message, "no per-function rows recorded for this frame",
            "the SENTENCE is pinned here, character for character, and \
             that is the point: this state has been re-worded into a \
             cause claim once already (R2's blocked NoUart), and a test \
             that only forbade a list of phrasings would pass on the \
             next paraphrase of the same claim -- \"this frame ran \
             without a single function missing the I-cache\" is ggo-ide's \
             NO_FRAME_MISSES_TEXT in other words, and no denylist catches \
             it. Rewording this string is therefore not a passing edit: \
             it fails here, and the replacement has to be defended on the \
             rule in NO_FRAME_PROFILE's doc -- name the state, never a \
             cause -- rather than slipped past a filter."
        );
        assert_eq!(message, NO_FRAME_PROFILE, "and it is what the pane shows");
    }

    #[test]
    fn a_frame_with_rows_has_no_empty_state() {
        let profiles = build(&sample_rows(), &ignored());
        assert_eq!(profiles.inspect(0, 1).empty_state(), None);
    }

    // ------------------------------------------------------ the index

    #[test]
    fn inspect_groups_the_clicked_frames_rows_by_caller() {
        let profiles = build(&sample_rows(), &ignored());
        let inspected = profiles.inspect(2, 1);
        assert_eq!(inspected.frame, 1);
        assert_eq!(
            inspected.chart, 2,
            "the pane renders under the chart clicked"
        );
        let groups = inspected.groups();
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].caller, "main");
        assert_eq!(groups[0].misses, 40);
        assert_eq!(groups[0].evicted, 10);
        let leaves: Vec<&str> = groups[0].leaves.iter().map(|l| l.func.as_str()).collect();
        assert_eq!(leaves, vec!["update", "render"], "misses desc");
    }

    /// Frame 0's rows are filtered out before the index is built, so the
    /// pane and the charts above it are looking at one frame set -- R1's
    /// concern (1), on the surface most likely to expose it (frame 0 is
    /// the cold-cache burst, so its numbers are enormous).
    #[test]
    fn the_index_never_holds_an_ignored_frames_rows() {
        let profiles = build(&sample_rows(), &ignored());
        assert!(profiles.inspect(0, 0).groups().is_empty());
        assert!(
            !profiles
                .table()
                .rows(false)
                .iter()
                .any(|a| a.func == "boot"),
            "and neither does the table"
        );
    }

    /// The index is an optimisation, so it has to be invisible: grouping
    /// a frame through it must equal grouping it by scanning the whole
    /// run, INCLUDING the insertion-order tie-breaks
    /// `group_frame_profile` promises. Fed deliberately out of frame
    /// order, which is the case a non-stable sort would break.
    #[test]
    fn the_index_groups_a_frame_exactly_as_a_whole_run_scan_would() {
        let rows = vec![
            row(7, "main", "late", 5, 5),
            row(1, "main", "first", 5, 5),
            row(7, "main", "later", 5, 5),
            row(1, "draw", "second", 5, 5),
            row(3, "main", "mid", 1, 0),
            row(1, "main", "third", 5, 5),
        ];
        let profiles = build(&rows, &HashSet::new());
        for frame in [1, 3, 7, 9] {
            assert_eq!(
                profiles.inspect(0, frame).groups(),
                group_frame_profile(&rows, frame),
                "frame {frame} must group identically either way"
            );
        }
    }

    #[test]
    fn a_frame_the_run_never_reached_groups_to_nothing() {
        let profiles = build(&sample_rows(), &ignored());
        assert!(profiles.inspect(0, 9_999).groups().is_empty());
    }
}
