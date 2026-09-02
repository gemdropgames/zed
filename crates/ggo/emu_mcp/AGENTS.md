# zedgg-emu-mcp — agent contract

An MCP (stdio) server for LOCK-STEP play of the GGO emulator panel inside
a running Zed session: boot paused, then one frame per call — the caller
(a script, an AI, any system) sends pad input and gets the cart's own
world state back. The run is visible in Zed's panel throughout.

## Wiring

Build once: `cargo build -p ggo_emu_mcp` (binary `zedgg-emu-mcp`).

Zed agent panel (settings.json):

```json
"context_servers": {
  "zedgg-emu": { "command": { "path": "/path/to/zedgg-emu-mcp" } }
}
```

Claude Code: `claude mcp add zedgg-emu -- /path/to/zedgg-emu-mcp`

Both processes must share an environment (same `XDG_RUNTIME_DIR`) or they
compute different registry dirs and never find each other.

## Targeting

Sessions advertise under `$XDG_RUNTIME_DIR/zedgg-emu/` (`<pid>.json` +
`<pid>.sock`); dead pids prune on listing. Tools take optional `session`
(pid) and `workspace` (absolute project root); a workspace uniquely hosted
by one session selects it, and both may be omitted when exactly one
candidate is live. Start with `zed_sessions` when unsure.

## Tools

| tool | what |
|---|---|
| `zed_sessions` | live sessions: pid, workspaces, panel status |
| `emu_status` | what the target session's panels are doing |
| `emu_start { cart }` | boot + pause at the first frame boundary; returns initial world JSON |
| `emu_next_frame { buttons?, screenshot? }` | latch pad, run EXACTLY one frame; returns new world JSON (+ PNG if asked) |
| `emu_stop` | end the run; returns the cart's uart log |
| `emu_screenshot` | last presented frame as PNG, any run mode |
| `emu_uart { tail? }` | the run's UART/console log, readable mid-run |
| `emu_run { cart }` | boot a cart free-running (the Run button); watch with `emu_screenshot`/`emu_uart` |
| `emu_pause` / `emu_resume` | pause/resume the live run; `{ paused, frame, running }` |
| `emu_debug { view, bank?, palette?, layer? }` | PPU inspector: tiles / map / oam as PNG + data, palettes as hex |
| `cart_pack { world }` | `emd pack-ggo` one world into `target/ggo-emulate/`; `{ cart }` feeds `emu_start`/`emu_run` |
| `hw_flash { world?, rebuild_gateware?, tty?, baud?, collect_seconds?, telemetry? }` | flash a world to the BOARD and run it; returns once the flash STARTS, with the effective `config` (defaults: cached gateware, first serial port, 115200 baud, 120s capture) |
| `hw_flash_status` | snapshot: `{ active, what, phase, detail, elapsed_s, phases[], diag_steps[], verdict, failure, diag_run_id, perf_run_id, transcript, console_tail[] }` — poll it for running context |
| `hw_flash_wait { timeout_s? }` | poll until the flash reaches a verdict (default 1800s) |
| `hw_env` | board readiness: `{ ready, missing[{code,label}], ports, version_skew }` — call before `hw_flash` |
| `hw_flash_cancel` | cancel the flash in flight; `{ cancelled }` |
| `list_ggo_reports { limit? }` | perf runs in `~/.ggo/ggo_ide.db`, newest first, with ggo-diag log paths |
| `fetch_ggo_report { run }` | paste-ready summary of one perf run (+ its ggo-diag log path) |
| `open_ggo_report { run }` | open the Reports tab in Zed on that run (`{requested: true}` — the panel lands on the runs list if its db lacks the run) |
| `close_ggo_report { run? }` | close the Reports tab (only if it shows `run`, when given) |
| `world_list` | every world in the project: `[{ stem, rel_path }]` |
| `world_open { world }` | open a world in the World panel |
| `world_read { world? }` | the authored world: entities/components/pos, instances, backgrounds, selection, dirty |
| `world_screenshot { world?, full? }` | the authored world as PNG: device screen at the camera, or the whole scene |

## Worlds

`world_list` / `world_open` / `world_read` / `world_screenshot` read the
WORLD panel — the level as the designer authored it (entity components,
`[[instance]]` placements, background slots, the current selection, and
whether the document has unsaved edits) — not the running game; that is
`emu_next_frame`'s `world`. Paths are worktree-relative (what the
explorer shows); `world_open` also accepts the stem `emd`, `cart_pack`
and `hw_flash` take. `world_open` is a click minus the modal: the world
already open is left exactly as it is, and a swap away from a world with
unsaved edits is refused rather than discarding them. `world_screenshot`
draws that same authored layout: by default the 320x240 device screen
framed on the world's active camera (the engine's own centring rule), or
the whole scene's bounding box with `full`. Sprites, tilemaps and
backgrounds composite as real pixels; text and placeholder entities are
flat boxes, and the editor's selection outline is left out.

## Hardware

Call `hw_env` first: a missing prerequisite there is what `hw_flash`
would fail on. Flashing occupies the board (~20 min with
`rebuild_gateware`) — CONFIRM WITH THE USER before `hw_flash`, and pass
an explicit `world` stem: with none, the panel flashes whichever world
it last remembered. Then `hw_flash_wait` → `verdict` (true=PASS) and,
for a passed run, `perf_run_id` → `fetch_ggo_report { run }`.
`list_ggo_reports` and `fetch_ggo_report` read the database directly, so
they need no live session; `open_ggo_report`/`close_ggo_report` drive the
Reports tab.

This bridge is single-threaded: `main` reads one stdin line, serves it,
and only then reads the next. Every other tool is bounded by a 15s socket
timeout, but `hw_flash_wait` blocks for as long as its `timeout_s` (default
1800s) — and while it does, NO other call here is served, `hw_flash_status`
included. Many MCP clients give up well before 1800s. Nothing is lost when
that happens: the flash runs inside Zed, not here, and flash status is
per-run and persists, so calling `hw_flash_wait` again resumes waiting on
the same flash (likewise after a socket blip aborts one). Prefer a
`timeout_s` you will actually sit through (~300) and re-call.

## The loop

Pack a cart first (`cart_pack { world: "worlds/arena" }`, or `emd
pack-ggo --world <stem>` in a shell), then:

1. `emu_start { cart: "wilds.ggo" }` → `{ started, frame, world }`
2. repeat `emu_next_frame { buttons: ["right"] }` → `{ frame, world }`
   — buttons are level-triggered (held until changed; `[]`/omitted
   releases all). Names: `z x a s up down left right q w e r t y u i
   enter select`. Add `screenshot: true` when you want to see the frame.
3. `emu_stop` → `{ uart }`

## Where `world` comes from

The emulator host cannot serialize the game — it only sees RV32 RAM. The
CART serializes itself: emerald's `inspect` module (compiled only under
the engine's `inspect` cargo feature — retail builds carry none of it)
dumps every entity's registered scene components to JSON each frame into
a magic-tagged RAM buffer the host reads out.

No world file opt-in exists. The tap ships DISARMED; `emu_start` arms it
by writing the tap's `enabled` word in guest RAM, so every world booted
through this MCP serializes automatically, and ordinary in-editor runs
never pay for serialization at all.

Carts built without the feature play fine but return `world: null` —
drive by screenshot instead (a game enables it via
`emerald-world = { ..., features = ["inspect"] }`; drop for shipping).
`Fixed` values print as 4-decimal numbers; a `tap_seq` field counts
dumps. Dumps are capped at 64 KiB (`truncated_raw` appears if clipped).
Custom `SceneField` types must implement `emerald_world::JsonField`
(compile error otherwise).
