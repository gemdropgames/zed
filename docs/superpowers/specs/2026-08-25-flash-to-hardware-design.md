# Flash to hardware — design

Task 9 of `tasks/editor-gaps.md`, un-deferred. Two asks: a **flash + run**
action that puts the open game on a real GemdropGo board, and ZedGG
**launching or installing every tool** that flow needs.

Decisions taken with the user (2026-08-25): flash with the cached
bitstream (`--skip-pnr`); ZedGG may fetch the OSS CAD Suite, install
`ggo-diag`/`emd`, and clone the GGO repo; the button lives in the
emulator transport and beside the world panel's Play; the icon is one
composed SVG; progress streams with parsed stage verdicts; setup happens
inline in the emulator tab.

## 0. What already exists (and what does not)

`ggo-diag --project <dir> --tty <port> --skip-pnr` is the entire flash
pipeline today: it packs the project with `emd pack-ggo`, writes the
flash-backed sd-emu card image (GemOS + assets + the game), flashes the
cached bitstream with `fujprog`, boot-verifies over UART, and records the
run in `~/.ggo/diag.db` — the same rows the charts panel reads.
`ggo_diag::toolchain::ensure` already downloads the OSS CAD Suite
(yosys / nextpnr / ecppack / fujprog) into `<repo>/.tools` via
`scripts/setup.sh`.

**`ggo-flash` is a stub** (`exit(2)`, "not yet implemented"), so nothing
here writes a `.cart` to a cartridge's QSPI NOR. Delivery is the card
image. Cart-to-QSPI is out of scope until that binary exists.

The fork already runs `ggo-diag --launch` for the built-in diagnostic
cart (`emu_panel/src/menu.rs`), capture-only, no streaming, no cancel.

## 1. `ggo_common` — a streaming process runner

`run_capture` returns only at exit; a flash is minutes of silence under
that. Add, beside it:

```rust
pub type ProcStreamer = Arc<dyn Fn(ProcRequest, LineSink) -> ProcCapture + Send + Sync>;
pub type LineSink = Box<dyn FnMut(&str) + Send>;

pub async fn run_streaming_async(request: &ProcRequest, on_line: LineSink) -> ProcCapture;
pub fn system_proc_streamer() -> ProcStreamer;
```

Spawns with piped stdout+stderr, reads both line-by-line as they arrive,
calls `on_line` for each, and returns the same `ProcCapture` the capture
runner does (so existing verdict handling is unchanged). `kill_on_drop`,
exactly as `run_capture_async` has it: dropping the task kills the child,
which is the cancel button. Injectable like `ProcRunner`, so panel tests
drive scripted output without spawning.

## 2. `emu_panel/src/hardware.rs` — the rules, as pure functions

**Environment.** One probe, one struct:

```rust
pub struct HardwareEnv {
    pub diag_bin: Option<String>,   // GGO_DIAG_BIN, else `ggo-diag` on PATH
    pub emd_bin: Option<String>,    // `emd` on PATH
    pub repo: Option<PathBuf>,      // GGO_REPO, else walk up from the project
    pub ports: Vec<String>,         // /dev/serial/by-id, then /dev/ttyUSB*
    pub cargo: bool, pub git: bool, // can we install at all?
}
```

`missing(&self) -> Vec<Missing>` names each gap as a value, not a string,
so both the status line and the setup steps read from one source.

**Flash.** `flash_args(project_rel_root, tty) -> Vec<String>` →
`["--project", <abs project dir>, "--tty", <port>, "--skip-pnr"]`, run
with `cwd` = the GGO repo (that CLI walks up from its cwd to find the
repo). `flash_request(env, project) -> Result<ProcRequest, String>` —
the `Err` is the human-readable list of what is missing, the same shape
`diag_request` already uses.

**Stage parsing.** `ggo-diag`'s stdout has a fixed grammar
(`diag/event.rs`), so parse it rather than guess:

| line | meaning |
|---|---|
| `==> <title>` | phase: Compile firmware, Component PnR, Full SoC PnR, Provision SD card, Flash board, Boot verify (UART), Report |
| `--> component <name>` / `<-- component <name>: …` | per-component PnR progress |
| `  [boot] <stage>` / `  [boot] <stage> — <detail>` | boot-verify stage |
| `diag step <n>: running\|PASS\|FAIL\|info` | diagnostic-cart step |
| `RESULT: PASS` / `RESULT: FAIL` | final verdict |

`parse_stage(line) -> Option<Stage>` returns an enum; `Stage::label()`
gives the status-row text. Unknown lines are `None` — they still reach
the console, they just do not move the status. A `RESULT:` line sets the
run's verdict; absent one, a non-zero exit is the failure.

**Setup steps.** `setup_steps(env) -> Vec<SetupStep>`, ordered, each with
a label and a `ProcRequest`:

1. **Clone the GGO repo** (only if absent) — `git clone
   git@github.com:gemdropgames/ggo.git <dest>`, dest defaulting to
   `~/.ggo/ggo`, and `GGO_REPO` wins when set.
2. **Install `ggo-diag`** — `cargo install --path <repo>/tools/ggo-diag`
   when the checkout exists, else `cargo install --git <url>`.
3. **Install `emd`** — same rule against an `emerald` checkout beside the
   GGO repo, else `--git git@github.com:gemdropgames/emerald.git`.

The OSS CAD Suite is deliberately **not** a step: `ggo-diag` fetches it
on the first run and that download streams through the same console. No
step ever runs when its tool is already present, and `cargo`/`git`
missing is reported, never worked around.

## 3. Icon

`assets/icons/ggo_flash_run.svg`: a bolt in the upper two-thirds of a
16×16 viewBox with a play triangle centred beneath it, both on
`currentColor`, stroke widths matching the sibling icons. Registered as
`IconName::GgoFlashRun` in `crates/icons/src/icons.rs` (one line, GGO-
marked — the same kind of upstream touch the fork already makes in
`project_panel` and `workspace`).

## 4. Wiring

**Emu panel.** A `GgoFlashRun` button in the transport row, beside Run
and Watch. Pressed: resolve `HardwareEnv`, then either start the flash or
render the gap. Panel state gains `flash: Option<FlashRun>` holding the
current stage, the streaming task and the verdict. The status row shows
`Stage::label()`; raw lines go to the existing console pane (already
built for UART output); pressing the button during a run cancels it by
dropping the task.

**World panel.** The same action beside the existing Play / Play-popout,
dispatched through a registered hook in `ggo_common` (`register_board_flasher`,
mirroring `register_world_emulator`) so `ggo_world_panel` never names
`ggo_emu_panel` — the dependency edge stays one-way.

**Setup.** When `missing()` is non-empty the status row names every gap
and offers **Set up hardware tooling**, which runs `setup_steps` in order
through the same streaming console, stopping at the first failure. After
it finishes the env is re-probed, so the flash button lights up without a
restart.

## 5. Testing

Pure (no process, no gpui): `flash_args` shape; `flash_request`'s error
text per missing prerequisite; `parse_stage` over a recorded transcript
of every line form above, including unknown lines and both verdicts;
`setup_steps` ordering, skipping present tools, and `--path` vs `--git`
selection.

Panel (fake streamer): a scripted run drives the status row through the
stages and lands on PASS; a `RESULT: FAIL` run surfaces as an error; a
non-zero exit with no verdict still fails; cancelling mid-run drops the
task and clears the stage; a missing prerequisite renders the gap and no
process is spawned; the setup flow runs its steps in order and re-probes.

## 6. Out of scope

`.cart` → QSPI (needs `ggo-flash`); a serial-port picker UI (the first
scanned port, `GGO_DIAG_TTY` to override); baud / collect-seconds forms;
full-SoC PnR from the button; flash history beyond what `~/.ggo/diag.db`
already stores and the charts panel already shows.
