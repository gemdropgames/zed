# GGO fork — upstream tracking runbook

Remotes: origin = gemdropgames/zed (push via HTTPS), upstream = zed-industries/zed.
Branches: `main` = pristine upstream mirror (never commit); `ggo` = ours (default).

## Update procedure
    git checkout main && git pull upstream main && git push origin main
    git checkout ggo && git merge upstream/main --no-edit
    cargo test -p ggo_common -p ggo_world_panel -p ggo_metasprite_panel \
              -p ggo_charts_panel -p ggo_emu_panel -p ggo_language \
              -p ggo_tileset_panel && cargo check -p zed   # all seven ggo crates
Then fix any ggo_* compile breaks (gpui churn) immediately — drift compounds.

## Conflict policy
- `Cargo.lock`: take upstream's (`git checkout --theirs Cargo.lock`), then `cargo check` to re-add ggo deps.
- Registration files (all GGO lines carry `# GGO` / `// GGO` markers):
  - root `Cargo.toml`: `"crates/ggo/*"` member + ggo `[workspace.dependencies]`
  - `crates/zed/Cargo.toml`: ggo deps
  - `crates/zed/src/main.rs`: `ggo_*::init(cx)` calls
  Resolution = keep upstream's version of the region, re-insert the marker lines.
- Behavioural hooks in high-churn upstream files — NOT registration lines, so
  "keep upstream's region and re-insert the markers" does NOT apply; re-apply
  these by hand and re-run the guard tests:
  - `crates/workspace/src/dock.rs`: `Panel::prepare_to_close` (defaulted, so
    upstream panels are unaffected), its `PanelHandle` forwarder, and
    `Dock::panels()` — 4 `GGO` markers, ~24 lines.
  - `crates/workspace/src/workspace.rs`: the panel poll inside
    `Workspace::prepare_to_close` — 2 `GGO` markers, 21 lines. Ordering is
    load-bearing: it must sit AFTER the active-call prompt and BEFORE
    `save_all_internal`.
  Verify with `cargo test -p ggo_world_panel -p ggo_metasprite_panel close_guard`
  (8 tests) plus `test_dirty_panel_vetoes_workspace_prepare_to_close`.
  - `crates/workspace/src/workspace.rs`: the `PathOpenInterceptor` registry
    (`register_path_open_interceptor` + `Workspace::intercept_path_open`) — one
    GGO-marked block, ~46 lines, placed right after `register_project_item`
    (F4). Opening AND closing `// GGO` markers (X4 added the closing one), so
    `grep -n GGO crates/workspace/src/workspace.rs` shows the block's extent
    rather than just where it starts. `try_global`-gated: with an empty
    registry every call site behaves exactly as it did before.
  - `crates/project_panel/src/project_panel.rs`: two guarded call sites (F4) —
    the `Event::OpenedEntry` funnel (`open_path_preview`, currently `:893-924`)
    and the `Event::SplitEntry` path (`split_path_preview`, currently
    `:945-964`) — each wraps the existing call in
    `if !workspace.intercept_path_open(&ggo_project_path, window, cx) { … }`.
    **NEW territory for our hooks** as of F4 (previously only `dock.rs` +
    `workspace.rs`'s `prepare_to_close` poll above), and `project_panel.rs`
    churns considerably more than `dock.rs` upstream — expect to hand-reapply
    this one more often. The body is not re-indented; the only edit inside it
    is the `ProjectPath` literal hoisted to a `ggo_project_path` local (needed
    twice — once by the interceptor, once by the open). That hoist is the
    8 deleted lines in the F4 diff; everything else inside the guard is the
    original statement, unmoved (`cargo fmt --check` clean before and after).
    Verify with `cargo test -p workspace --lib -p project_panel --lib` plus
    `cargo test -p ggo_world_panel -p ggo_metasprite_panel -p ggo_tileset_panel`
    (interceptor-routing + already-open-fast-path tests) and, above all, the
    two tests in the fork that each enter at one of the real
    `project_panel::Event` subscriptions, so a dropped guard fails the
    matching test rather than merging in silently: `cargo test -p
    ggo_emu_panel test_project_panel_opened_entry` covers the
    `Event::OpenedEntry` funnel (X4; verified by deleting the guard and
    watching it go red) and `cargo test -p ggo_emu_panel
    test_project_panel_split_entry` covers the `Event::SplitEntry` path
    (F4 review follow-up; same red/green drill). Both guarded call sites are
    now covered. Every other routing test enters at
    `Workspace::intercept_path_open` and would stay green.
  - `crates/workspace/src/workspace.rs`: the `ContextMenuContributor` registry
    (`register_context_menu_contributor` + `Workspace::context_menu_contributions`)
    — the fork's **third** behavioural hook (F5.0/G1), one GGO-marked block of
    71 lines with opening AND closing markers, placed immediately after the
    `PathOpenInterceptor` block above. Same `try_global` gate: an absent or
    empty registry returns an empty `Vec` and the menu is byte-identical to
    upstream's. A contributor returns `Vec<ui::ContextMenuItem>` rather than
    taking and returning the half-built `ContextMenu` on purpose — `ContextMenu`
    exposes no item count, so "did anything get added?" would be unanswerable
    and the separator the call site emits ahead of the fork's block would be
    stray whenever every contributor declined. Non-local projects (SSH remote,
    collab guest) contribute nothing, mirroring
    `ggo_common::rel_in_primary_worktree`'s rule.
  - `crates/project_panel/src/project_panel.rs`: a THIRD guarded call site
    (F5.0/G1), and the **second** hook in this file — inside
    `deploy_context_menu` (currently `:1122-1261`): a `ggo_items` local
    computed just before `ContextMenu::build`, and a trailing `.map(…)` chained
    onto the builder closure's existing `.map(…)`. 26 added lines, **0
    deleted** — unlike the F4 sites, nothing upstream is wrapped, re-indented
    or moved, so a conflict here should always resolve as "take upstream's
    region, paste both GGO chunks back in". Order is load-bearing: the fork's
    entries go last, after everything upstream builds, and the `separator()`
    is inside the `else` branch so an empty registry adds no divider.
    Verify with `cargo test -p workspace --lib -p project_panel --lib`
    (237 + 116) plus, above all, `cargo test -p ggo_emu_panel context_menu`
    (3 tests) — `test_project_panel_context_menu_shows_a_contributed_entry`
    builds a REAL `ProjectPanel`, docks it, right-clicks a real row and looks
    for the contributed entry in the RENDERED menu, so a dropped hook fails it
    rather than merging in silently (verified red twice: with the trailing
    `.map` neutered, and with both GGO chunks deleted).
- Anything else conflicting means upstream moved a registration site: relocate the marker line, never keep a stale copy of upstream code.
- After each merge: verify `reload_keymaps` (crates/zed/src/zed.rs) still ends with `keymap_editor::KeymapEventChannel::trigger_keymap_changed` — ggo_world_panel's, ggo_metasprite_panel's, ggo_charts_panel's, and ggo_emu_panel's keybindings all silently die if that call disappears.

## Rules
- All GGO code lives in `crates/ggo/*`. Upstream files get registration lines only.
- Never commit non-ggo changes on `ggo`; upstream itches become upstream PRs off `main`.
- Cadence: merge at least per upstream stable tag (~weekly).

## Measurements

Entries below are a dated log and are **not** rewritten after the fact. Several
mention `ggo_hello`, the F0 fork-wiring smoke crate: it was deleted on
2026-08-09 (X4) once five real panels proved the same wiring, so any `-p
ggo_hello` in an older line is history, not a command to run today. The current
sweep is the seven-crate one in the update procedure above.

- 2026-08-07 cold `cargo build -p zed`: 7m 54s
- 2026-08-07 incremental rebuild after touching ggo_hello: 7.58s
- (2026-08-07, ggo_hello since deleted) pane smoke: headless gpui test added (test_open_hello_action); human visual run still welcome
- 2026-08-07 merge drill: upstream/main was zero-delta (0 new commits since clone) — `git merge upstream/main --no-edit` was a no-op, no conflicts. `cargo test -p ggo_hello && cargo check -p zed` passed clean, fully cached (<1s). Procedure validated but not exercised against real upstream churn — repeat drill pending in a few days before F1 starts.
- 2026-08-08 merge drill #2 (real): 6 upstream commits, 0 conflicts, ggo_hello 3/3 + check clean in ~40s
- 2026-08-08 F1 wrap (W5-W7 landed: world panel skeleton/viewer/editor): `cargo test -p ggo_hello -p ggo_world_panel` 3+30 passed, `cargo clippy -p ggo_world_panel --all-targets` clean, `cargo build -p zed` 3m 25s (warm incremental, not cold)
- 2026-08-08 merge drill #3 (F1 wrap, real): 1 upstream commit (`08827f9208` terminal `starts_open` setting), 0 conflicts, `git merge upstream/main --no-edit` clean; `cargo test -p ggo_hello -p ggo_world_panel` 3+30 passed, `cargo clippy -p ggo_world_panel --all-targets` clean, `cargo check -p zed` 35s; `reload_keymaps` invariant re-verified intact post-merge
- 2026-08-08 F2 wrap (M2-M7 landed: metasprite panel viewer/editor/clips/transport/save + world panel add-delete/save-root parity): `cargo test -p ggo_world_panel -p ggo_metasprite_panel` 32+36 passed, `cargo clippy -p ggo_world_panel -p ggo_metasprite_panel --all-targets -- -D warnings` clean, `cargo build -p zed` 2m 45s (warm incremental, not cold)
- 2026-08-08 F3 wrap (C1-C2 charts panel, E1-E2 emu pane, L1 GGO World language, T1 `.zed` task layer, cleanup landed): `cargo test -p ggo_hello -p ggo_common -p ggo_world_panel -p ggo_metasprite_panel -p ggo_charts_panel -p ggo_emu_panel -p ggo_language` = 3+5+37+41+95+76+15 = 272 passed, 0 failed; `cargo clippy` on all seven `--all-targets -- -D warnings` clean; `cargo fmt --check` clean; merge drill #4: `git rev-list --count ggo..upstream/main` = 0 (upstream/main still `08827f9208`, unchanged since drill #3) — no merge to exercise, `reload_keymaps` invariant re-verified intact (`crates/zed/src/zed.rs:2356`). Feature disposition audit vs ggo-ide: `docs/ggo/MIGRATION.md`.
- 2026-08-09 merge drill #5 (F4 wrap, real): `git rev-list --count ggo..upstream/main` = 2 (`371a7d4ba2` editor cmd-click references fallback, `59b2ebf103` gpui `img` aspect-ratio fix — neither touches `workspace.rs`, `dock.rs`, or `project_panel.rs`); `git checkout main && git pull upstream main && git push origin main` fast-forward `08827f9208..371a7d4ba2`; `git checkout ggo && git merge upstream/main --no-edit` clean, 0 conflicts (`ort` strategy, 4 files outside `crates/ggo/*`); `reload_keymaps` invariant re-verified intact (still ends with `keymap_editor::KeymapEventChannel::trigger_keymap_changed`, `crates/zed/src/zed.rs:2356`).
- 2026-08-09 F4 wrap (X1-X3: explorer-driven panel routing, `ggo_tileset_panel` read-only `.til` viewer, docs + sweep), post-merge: `cargo test -p ggo_hello -p ggo_common -p ggo_world_panel -p ggo_metasprite_panel -p ggo_charts_panel -p ggo_emu_panel -p ggo_language -p ggo_tileset_panel` = 3+5+43+46+95+76+15+13 = **296 passed, 0 failed** (all EIGHT ggo crates); `cargo clippy` on all eight `--all-targets -- -D warnings` clean; `cargo fmt --all --check` clean; `script/check-licenses` exit 0; `cargo check -p zed` clean; `cargo test -p workspace --lib` = 237 passed; `cargo test -p project_panel --lib` = 116 passed (both touched by X1's interceptor hooks). Feature disposition re-tally: `docs/ggo/MIGRATION.md` Counts, dropped → partial by one row (`.til` viewer).
- 2026-08-09 F4 X4 (`.cart` explorer routing — the last in-panel picker removed; F4 final-review doc corrections; `ggo_hello` deleted): `cargo test -p ggo_common -p ggo_world_panel -p ggo_metasprite_panel -p ggo_charts_panel -p ggo_emu_panel -p ggo_language -p ggo_tileset_panel` = 5+43+46+95+71+15+13 = **288 passed, 0 failed** (all SEVEN remaining ggo crates; `ggo_emu_panel` 76 → 71: −9 for the deleted enumeration/dropdown tests, +4 for routing); `cargo clippy -p ggo_emu_panel -p ggo_common --all-targets -- -D warnings` clean; `cargo fmt --all --check` clean; `script/check-licenses` exit 0; `cargo check -p zed` clean (the real test of the `ggo_hello` de-registration); `cargo test -p workspace --lib` = 237 passed; `cargo test -p project_panel --lib` = 116 passed. New: `ggo_emu_panel`'s `test_project_panel_opened_entry_routes_a_cart_into_the_panel` enters at the REAL `project_panel` `Event::OpenedEntry` subscription — verified red by deleting the GGO guard, green with it. Feature disposition: no status moved, `MIGRATION.md` Counts unchanged.
- 2026-08-09 F4 X4 review follow-up (closes the review's remaining items): added `ggo_emu_panel::test_project_panel_split_entry_routes_a_cart_into_the_panel`, the `Event::SplitEntry` sibling of the `OpenedEntry` test above — same real-`ProjectPanel` shape, verified red by neutralizing the `:955` `split_path_preview` guard and green again with it restored (`git diff crates/project_panel/` empty after). Both project_panel.rs call sites are now covered. Also fixed three stale comments in `ggo_emu_panel.rs` (module doc, `run_generation` field doc, completion-closure comment) left over from `refresh_carts`/`load_generation`'s deletion in 51f351a9c3, and dated the `UPSTREAM.md` pane-smoke line as `ggo_hello`-era history. `cargo test -p ggo_common -p ggo_world_panel -p ggo_metasprite_panel -p ggo_charts_panel -p ggo_emu_panel -p ggo_language -p ggo_tileset_panel` = 5+43+46+95+72+15+13 = **289 passed, 0 failed**; `cargo clippy -p ggo_emu_panel --all-targets -- -D warnings` clean; `cargo fmt --all --check` clean; `script/check-licenses` exit 0; `cargo check -p zed` clean; `cargo test -p project_panel --lib` = 116 passed.
