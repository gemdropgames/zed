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
| `hw_flash { world?, rebuild_gateware? }` | flash a world to the BOARD and run it; returns once the flash STARTS |
| `hw_flash_status` | snapshot: `{ active, phase, verdict, diag_run_id, perf_run_id }` |
| `hw_flash_wait { timeout_s? }` | poll until the flash reaches a verdict (default 1800s) |
| `perf_report { run }` | paste-ready summary of one perf run from `~/.ggo/ggo_ide.db` |

## Hardware

Flashing occupies the board (~20 min with `rebuild_gateware`) — CONFIRM
WITH THE USER before `hw_flash`, and pass an explicit `world` stem: with
none, the panel flashes whichever world it last remembered. Then
`hw_flash_wait` → `verdict` (true=PASS) and, for a passed run,
`perf_run_id` → `perf_report { run }`. `perf_report` reads the database
directly, so it needs no live session.

## The loop

Pack a cart in the project first (`emd pack-ggo [--world <stem>]`), then:

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
