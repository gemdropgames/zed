# Flash to hardware — plan

Spec: `docs/superpowers/specs/2026-08-25-flash-to-hardware-design.md`.
Branch: zed `flash-to-hardware` (off `ggo`). No ggo-repo changes.

## Phase A — streaming runner (`ggo_common`)
What: `run_streaming_async` + `ProcStreamer`/`LineSink` + `system_proc_streamer`,
cancellable via `kill_on_drop`.
Why: every stage below reports through one channel; a flash is minutes
of silence without it.
Tests: a scripted binary (`sh -c`) streams lines in order; a dropped task
kills the child.

## Phase B — the rules + the icon
What: `emu_panel/src/hardware.rs` — `HardwareEnv`/`missing`, `flash_args`,
`flash_request`, `parse_stage`/`Stage`, `setup_steps`.
`assets/icons/ggo_flash_run.svg` + `IconName::GgoFlashRun`.
Why: pure, unit-testable rules; the panel stays glue.
Tests: spec §5's pure set.

## Phase C — flash flow in the emulator tab
What: transport button, `FlashRun` state, status row from `Stage::label()`,
raw lines to the console, press-again cancels.
Tests: scripted streamer through the stages to PASS; FAIL verdict; bare
non-zero exit; cancel; missing prerequisite spawns nothing.

## Phase D — setup flow + the world panel button
What: **Set up hardware tooling** running `setup_steps` in order through
the same console, stop at first failure, re-probe after;
`ggo_common::register_board_flasher` + the world panel's button.
Tests: step order and skipping; failure stops the rest; the world panel's
button reaches the emu panel through the hook.

## Phase E — wrap
Sweep tests + clippy (ggo_common, emu_panel, world_panel, ui, icons);
MIGRATION.md rows (the flashing row + the diagnostics row); tick task 9
in `tasks/editor-gaps.md`; review agent; fix findings; merge
`flash-to-hardware` → `ggo`, push `ggo` + `main`.
