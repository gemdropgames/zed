# zedgg-emu-mcp — agent contract

An MCP (stdio) server that drives the GGO toolchain inside a running Zed
session: emulator control in both modes (LOCK-STEP by default — boot
paused, the frames you ask for per call, the cart's own world state back
— and free-running, which `emu_start` takes `freerun: true` for), the
PPU inspector, cart packing, hardware flash with its readiness probe,
perf reports, and reads of the authored world. Everything it does is
visible in the user's Zed as it happens.

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
| `emu_status` | what the target session's panels are doing: `{pid, workspaces[{workspace, cart, running, paused, frame, run_kind, world}]}`. `run_kind` is `cart` \| `world` \| `viewer` (the live world view's viewer cart); `world` is the world's rel path for the latter two |
| `emu_start { cart, freerun? }` | boot + pause at the first frame boundary (lock-step); returns initial world JSON. Focuses the emulator tab and marks it ` · MCP`. `freerun: true` boots free-running instead (the Run button, no lock-step, no world JSON) -- watch it with `emu_screenshot`/`emu_uart`, `emu_pause` |
| `emu_next_frame { buttons?, screenshot?, frames? }` | latch pad, run EXACTLY `frames` frames (default 1, max 60000); returns new world JSON (+ PNG if asked) |
| `emu_stop` | end the run; returns the cart's uart log |
| `emu_screenshot` | last presented frame as PNG, any run mode |
| `emu_uart { tail? }` | the run's UART/console log, readable mid-run |
| `emu_pause` / `emu_resume` | pause/resume the live run; `{ paused, frame, running }` |
| `emu_debug { view, bank?, palette?, layer? }` | PPU inspector: tiles / map / oam as PNG + data, palettes as hex |
| `cart_pack { world }` | `emd pack-ggo` one world into `target/ggo-emulate/`; `{ cart }` feeds `emu_start` |
| `hw_flash { world?, rebuild_gateware?, tty?, baud?, collect_seconds?, telemetry? }` | flash a world to the BOARD and run it; returns once the flash STARTS, with the effective `config` (defaults: cached gateware, first serial port, 460800 baud, 120s capture) |
| `hw_flash_status` | snapshot: `{ active, what, phase, detail, elapsed_s, phases[], diag_steps[], verdict, failure, diag_run_id, perf_run_id, transcript, console_tail[] }` — poll it for running context |
| `hw_flash_wait { timeout_s? }` | poll until the flash reaches a verdict (default 1800s) |
| `hw_env` | board readiness: `{ ready, missing[{code,label}], ports, version_skew }` — call before `hw_flash` |
| `hw_flash_cancel` | cancel the flash in flight; `{ cancelled }` |
| `list_ggo_reports { limit? }` | reports in the ggo database, newest first: perf runs with their ggo-diag log paths, then the ggo-uartd fault dumps after a `--- faults ---` line (fresh dumps imported on the way) |
| `fetch_ggo_report { run \| fault }` | paste-ready summary of one perf run (+ its ggo-diag log path), or one fault dump's digest (boot stage, telemetry, panics, asset failures, the fault line in context, raw path) |
| `open_ggo_report { run \| fault }` | open the Reports tab in Zed on that run or fault (`{requested: true}`; an id absent from the db is an error here, before any tab opens) |
| `close_ggo_report { run? }` | close the Reports tab (only if it shows `run`, when given) |
| `world_list` | every world in the project: `{ worlds: [{ stem, rel_path }] }` |
| `world_open { world }` | open a world in the World panel |
| `world_read { world? }` | the authored world: entities/components/pos, instances, backgrounds, selection, dirty |
| `world_screenshot { world?, full? }` | the authored world as PNG: device screen at the camera, or the whole scene |
| `sprite_list` | every `.spr` in the project: `{ sprites: [{ stem, rel_path }] }` |
| `sprite_read { sprite? }` | the authored sprite: bound tileset, frame footprint, clips |
| `sprite_clip_create { sprite, name, loop?, entries? }` | append a clip; UNSAVED until `sprite_save` |
| `sprite_clip_update { sprite, clip, name?, loop?, entries? }` | merge fields into a clip (`clip` is an index or a unique name); UNSAVED until `sprite_save` |
| `sprite_clip_delete { sprite, clip }` | delete a clip; UNSAVED until `sprite_save` |
| `sprite_save { sprite }` | persist the open sprite's unsaved edits: `{ saved: rel_path }` |
| `sprite_reference_sheet { sprite, path? }` | the bound tileset's source-art layout as PNG + `{ cols, rows, tiles }`; `path` also writes it to disk |
| `sprite_tileset_image { sprite, path? }` | the bound tileset's whole pool as a PNG grid; `path` also writes it to disk |

## Worlds

`world_list` / `world_open` / `world_read` / `world_screenshot` read the
WORLD panel — the level as the designer authored it (entity components,
`[[instance]]` placements, background slots, the current selection, and
whether the document has unsaved edits) — not the running game; that is
`emu_next_frame`'s `world`. Paths are worktree-relative (what the
explorer shows); `world_open` also accepts the stem `emd`, `cart_pack`
and `hw_flash` take. `world_open` is a click: every world gets its own
editor tab, a world that already has one is brought to the front rather
than reloaded (unsaved edits, undo history and camera survive), and
opening one world never disturbs another. `world_screenshot`
draws that same authored layout: by default the 320x240 device screen
framed on the world's active camera (the engine's own centring rule), or
the whole scene's bounding box with `full`. Sprites and
backgrounds composite as real pixels; text and placeholder entities are
flat boxes, and the editor's selection outline is left out.

## Sprites

`sprite_list` / `sprite_read` / the three clip tools / `sprite_save` /
`sprite_reference_sheet` / `sprite_tileset_image` read and edit the
SPRITE panel's documents — clip animations over a shared tile pool, not
the running game. Every tool that names a `sprite` opens (or focuses) its
editor tab the same way `world_open` does: a sprite that already has a
tab is brought to the front rather than reloaded, so unsaved edits, undo
history and playback survive, and opening one sprite never disturbs
another.

**Look at the reference sheet before you touch clips.** `.spr` documents
store their tiles in a POOL, not in drawing order — the pool is
deduplicated and reordered at import time, so pool index order tells you
nothing about which tile is which pose or frame. `sprite_reference_sheet`
is the source artwork laid out exactly as it was drawn (poses, facing,
sequence order): read it first, work out which `entries[].frame` values
you want, THEN call the clip tools. `sprite_tileset_image` is the raw
pool instead — reach for it only when you actually need pool-index order
(e.g. lining up `sprite_read`'s `entries[].frame` against real pixels),
not as a substitute for the reference sheet.

**Clip edits are unsaved until you say so.** `sprite_clip_create` /
`sprite_clip_update` / `sprite_clip_delete` apply immediately (undoable
in the editor, visible in the panel) but leave the document DIRTY on
purpose, so several edits fold into one save. Call `sprite_save` once
you're done with a batch, not after every single clip op — check
`sprite_read`'s `dirty` field if you're unsure whether anything is
still unsaved. `sprite_clip_update`/`sprite_clip_delete`'s `clip`
argument is an index or a clip's (unique) name; a name shared by more
than one clip is refused and asks for an index instead of guessing.

## Hardware

Call `hw_env` first: a missing prerequisite there is what `hw_flash`
would fail on. Flashing occupies the board (~20 min with
`rebuild_gateware`) — CONFIRM WITH THE USER before `hw_flash`, and pass
an explicit `world` stem: with none, the panel flashes whichever world
it last remembered. Then `hw_flash_wait` → `verdict` (true=PASS) and,
for a passed run, `perf_run_id` → `fetch_ggo_report { run }`. When the
board misbehaves outside a flash, `ggo-uartd`'s dumps land in the same
list: `list_ggo_reports` → a `fault <id>` line → `fetch_ggo_report { fault }`.
`list_ggo_reports` and `fetch_ggo_report` read the database directly, so
they need no live session; `open_ggo_report`/`close_ggo_report` drive the
Reports tab.

This bridge is single-threaded: `main` reads one stdin line, serves it,
and only then reads the next. Most tools are bounded by a 15s socket
timeout, but two are not. `cart_pack` blocks for the whole build (up to
10 min here, 15 min host-side), and `hw_flash_wait` blocks for as long as
its `timeout_s` (default 1800s) — and while either does, NO other call
here is served, `hw_flash_status` included. Zed's host is single-threaded
too: a `cart_pack` in flight also stalls every OTHER agent's tools against
that same Zed until the pack ends.

Many MCP clients give up well before 1800s. Nothing is lost when that
happens: the flash runs inside Zed, not here, and flash status is per-run
and persists, so calling `hw_flash_wait` again resumes waiting on the same
flash (likewise after a socket blip aborts one). Prefer a `timeout_s` you
will actually sit through (~300) and re-call.

## The loop

Pack a cart first (`cart_pack { world: "arena" }`, or
`emd pack-ggo --world <stem>` in a shell), then:

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
