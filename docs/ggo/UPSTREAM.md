# GGO fork — upstream tracking runbook

Remotes: origin = gemdropgames/zed (push via HTTPS), upstream = zed-industries/zed.
Branches: `main` = pristine upstream mirror (never commit); `ggo` = ours (default).

## Update procedure
    git checkout main && git pull upstream main && git push origin main
    git checkout ggo && git merge upstream/main --no-edit
    cargo test -p ggo_hello && cargo check -p zed   # widen -p list as ggo crates grow
Then fix any ggo_* compile breaks (gpui churn) immediately — drift compounds.

## Conflict policy
- `Cargo.lock`: take upstream's (`git checkout --theirs Cargo.lock`), then `cargo check` to re-add ggo deps.
- Registration files (all GGO lines carry `# GGO` / `// GGO` markers):
  - root `Cargo.toml`: `"crates/ggo/*"` member + ggo `[workspace.dependencies]`
  - `crates/zed/Cargo.toml`: ggo deps
  - `crates/zed/src/main.rs`: `ggo_*::init(cx)` calls
  Resolution = keep upstream's version of the region, re-insert the marker lines.
- Anything else conflicting means upstream moved a registration site: relocate the marker line, never keep a stale copy of upstream code.
- After each merge: verify `reload_keymaps` (crates/zed/src/zed.rs) still ends with `keymap_editor::KeymapEventChannel::trigger_keymap_changed` — ggo_world_panel's keybindings silently die if that call disappears.

## Rules
- All GGO code lives in `crates/ggo/*`. Upstream files get registration lines only.
- Never commit non-ggo changes on `ggo`; upstream itches become upstream PRs off `main`.
- Cadence: merge at least per upstream stable tag (~weekly).

## Measurements
- 2026-08-07 cold `cargo build -p zed`: 7m 54s
- 2026-08-07 incremental rebuild after touching ggo_hello: 7.58s
- pane smoke: headless gpui test added (test_open_hello_action); human visual run still welcome
- 2026-08-07 merge drill: upstream/main was zero-delta (0 new commits since clone) — `git merge upstream/main --no-edit` was a no-op, no conflicts. `cargo test -p ggo_hello && cargo check -p zed` passed clean, fully cached (<1s). Procedure validated but not exercised against real upstream churn — repeat drill pending in a few days before F1 starts.
- 2026-08-08 merge drill #2 (real): 6 upstream commits, 0 conflicts, ggo_hello 3/3 + check clean in ~40s
- 2026-08-08 F1 wrap (W5-W7 landed: world panel skeleton/viewer/editor): `cargo test -p ggo_hello -p ggo_world_panel` 3+30 passed, `cargo clippy -p ggo_world_panel --all-targets` clean, `cargo build -p zed` 3m 25s (warm incremental, not cold)
- 2026-08-08 merge drill #3 (F1 wrap, real): 1 upstream commit (`08827f9208` terminal `starts_open` setting), 0 conflicts, `git merge upstream/main --no-edit` clean; `cargo test -p ggo_hello -p ggo_world_panel` 3+30 passed, `cargo clippy -p ggo_world_panel --all-targets` clean, `cargo check -p zed` 35s; `reload_keymaps` invariant re-verified intact post-merge
- 2026-08-08 F2 wrap (M2-M7 landed: metasprite panel viewer/editor/clips/transport/save + world panel add-delete/save-root parity): `cargo test -p ggo_world_panel -p ggo_metasprite_panel` 31+36 passed, `cargo clippy -p ggo_world_panel -p ggo_metasprite_panel --all-targets -- -D warnings` clean, `cargo build -p zed` 2m 45s (warm incremental, not cold)
