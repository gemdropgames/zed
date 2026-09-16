//! The end-of-run perf ingest, as this panel sees it.
//!
//! The writer itself lives in `ggo_worldlib::charts::reports::ingest` and
//! runs inside the daemon: the editor hands a finished run to
//! `ggo_ingest_run` and never opens a database of its own. See
//! `docs/daemon-api-plan.md` §4.2 in the GGO repo for why.
//!
//! What is left here is the panel's side of that contract:
//!
//! - [`MAX_FRAMES`], which the ingest row's "truncated to N of M frames"
//!   wording quotes. Re-exported rather than re-declared so the number the
//!   user is shown is the number the writer actually applied.
//! - the two cross-panel round-trip tests below, which cannot move to
//!   worldlib because they read back through `ggo_charts_panel`'s own
//!   query functions and drive this crate's emulator.

/// Hard cap on frames per run, as the daemon-side writer applies it.
pub use ggo_worldlib::charts::reports::ingest::MAX_FRAMES;

#[cfg(test)]
mod tests {
    use ggo_worldlib::charts::reports::ingest::ingest_run;
    use serde_json::Value as Json;

    /// The 13 REQUIRED frame arrays, in the writer's column order.
    ///
    /// Spelled out here rather than imported: worldlib keeps its column
    /// list private, and it is pinned there by its own
    /// `frame_columns_match_the_insert_head` test. These tests only need
    /// SOME valid body -- what they assert is the round trip, not the
    /// column order.
    const FRAME_COLS: [&str; 13] = [
        "n",
        "instrs",
        "i_hits",
        "i_misses",
        "d_hits",
        "d_misses",
        "d_writebacks",
        "evictions",
        "blit_wire",
        "miss_wire",
        "scanout_wire",
        "wire_total",
        "over_budget",
    ];

    /// A minimal, valid perf-JSON body with `n` frames.
    fn output_for(cart: &str, n: usize) -> String {
        let mut frames = serde_json::Map::new();
        for name in FRAME_COLS {
            let col: Vec<i64> = match name {
                "n" => (0..n as i64).collect(),
                "over_budget" => vec![0; n],
                _ => (0..n as i64).map(|i| i + 1).collect(),
            };
            frames.insert(name.to_string(), serde_json::json!(col));
        }
        serde_json::json!({
            "cart": cart,
            "frame_budget_cycles": 555_549,
            "scanout_wire_cycles": 164_400,
            "refill_cycles": 8,
            "writeback_cycles": 8,
            "wire_wait_cycles": 2,
            "frames": Json::Object(frames),
        })
        .to_string()
    }

    // ---------------------------------------------- cross-panel round trip

    /// THE schema-fidelity test: a run written through the ingest path is
    /// read back through `ggo_charts_panel`'s OWN query functions
    /// (`list_runs`/`load_run_samples`, the real read side of the charts
    /// panel, not a re-implementation here). If the writer and the reader
    /// ever disagreed about a table, a column name or a column order, this
    /// fails.
    ///
    /// Calls the worldlib writer directly rather than going through the
    /// daemon: what is under test is the SCHEMA agreement between the two
    /// panels, and a socket in the middle would only add a way for the
    /// test to fail for an unrelated reason.
    #[test]
    fn a_run_ingested_here_reads_back_through_the_charts_panels_queries() {
        use ggo_charts_panel::loader;

        let db = ggo_db::TestDb::new();

        let mut v: Json = serde_json::from_str(&output_for("Green Fix", 3)).unwrap();
        // A couple of the optional columns the charts read, so the
        // assertion covers the optional tail of the frame insert too, not
        // just the 13 required ones.
        v["frames"]["bg_evictions"] = serde_json::json!([1, 2, 3]);
        v["frames"]["apu_underruns"] = serde_json::json!([0, 0, 4]);
        v["frames"]["spr_tiles_distinct"] = serde_json::json!([7, 8, 9]);
        let out = ingest_run(
            db.url(),
            &v.to_string(),
            &["[run] green.cart".to_string()],
            Some("carts/green.cart"),
        )
        .unwrap();

        let runs = loader::list_runs(db.url()).unwrap();
        assert_eq!(runs.len(), 1, "the charts panel's picker sees the run");
        assert_eq!(runs[0].id, out.run_id);
        assert_eq!(runs[0].cart_name, "Green Fix");
        assert_eq!(runs[0].label.as_deref(), Some("carts/green.cart"));
        assert!(!runs[0].started_at.is_empty());

        let samples = loader::load_run_samples(db.url(), out.run_id).unwrap();
        assert_eq!(samples.frames.len(), 3);
        // Column order: n, instrs, i_hits, i_misses, ... -- so frame 0 has
        // n = 0, instrs = 1, i_hits = 1, i_misses = 1.
        assert_eq!(samples.frames[0].n, 0);
        assert_eq!(samples.frames[1].n, 1, "ORDER BY n on the read side");
        assert_eq!(samples.frames[0].instrs, 1);
        assert_eq!(samples.frames[2].instrs, 3);
        assert_eq!(
            samples.frames[0].frame_budget_cycles,
            Some(555_549),
            "the budget reference line comes off the joined run row"
        );
        assert_eq!(samples.frames[0].bg_evictions, 1);
        assert_eq!(samples.frames[2].apu_underruns, 4);
        assert_eq!(samples.frames[2].spr_tiles_distinct, 9);
        assert!(
            samples.profile.is_empty(),
            "a cart run has no function attribution -- and that is not an error"
        );
    }

    /// The other half of the round trip: real perf JSON, produced by
    /// `ggo_emu_core::perfsim::perf_json` from an actual cart run driven
    /// through [`crate::drive`], ingests cleanly and lands the right frame
    /// count. This is what pins the ingest against the EMITTER rather than
    /// against a hand-written fixture of it.
    #[test]
    fn real_perf_json_from_a_cart_run_ingests_cleanly() {
        use ggo_charts_panel::loader;

        let finished = crate::drive::tests_support::run_green_cart_briefly(5);
        let perf = finished.perf.expect("a cart that ran has a perf snapshot");
        assert!(perf.frames >= 5, "{} frames recorded", perf.frames);

        let db = ggo_db::TestDb::new();
        let out = ingest_run(db.url(), &perf.perf_json, &finished.uart, Some("green.cart")).unwrap();

        let runs = loader::list_runs(db.url()).unwrap();
        assert_eq!(runs.len(), 1);
        assert_eq!(
            runs[0].cart_name, "Green Fix",
            "the perf-JSON cart identity is the cart header's own title, \
             exactly as ggo-ide's CartStepper reports it"
        );
        let samples = loader::load_run_samples(db.url(), out.run_id).unwrap();
        assert_eq!(samples.frames.len() as u64, perf.frames);
        assert!(
            samples
                .frames
                .iter()
                .all(|f| f.frame_budget_cycles.is_some()),
            "the wire model was enabled, so every frame has a budget"
        );
        assert!(
            samples.frames.iter().any(|f| f.instrs > 0),
            "the perf sim actually counted the cart's instructions"
        );
        assert!(
            !finished.uart.is_empty(),
            "the run's own diagnostics are ingested alongside the frames"
        );
    }
}
