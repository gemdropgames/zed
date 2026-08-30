# zedgg-emu-mcp — agent contract

An MCP (stdio) server that runs emulation scripts in the GGO emulator
panel inside a running Zed session. Script-only by design: one tool call
is one complete start → finish run, answered by one report. There is no
interactive drive surface that could leave an emulator half-driven.

## Wiring

Build once: `cargo build -p ggo_emu_mcp` (binary `zedgg-emu-mcp`).

Zed agent panel (settings.json):

```json
"context_servers": {
  "zedgg-emu": { "command": { "path": "/path/to/zedgg-emu-mcp" } }
}
```

Claude Code: `claude mcp add zedgg-emu -- /path/to/zedgg-emu-mcp`

## Targeting

Each Zed process hosting an emu panel advertises itself under
`$XDG_RUNTIME_DIR/zedgg-emu/` (`<pid>.json` + `<pid>.sock`); dead pids are
pruned on every listing. Tools take optional `session` (pid) and
`workspace` (absolute project root). A workspace uniquely hosted by one
session selects it; both may be omitted when exactly one candidate is
live. Start with `zed_sessions` when unsure. Both processes must share an
environment (same `XDG_RUNTIME_DIR`), or they compute different registry
dirs and never find each other.

## Tools

| tool | what |
|---|---|
| `zed_sessions` | live sessions: pid, workspaces, panel status |
| `emu_status` | what the target session's panels are doing |
| `emu_script` | run one complete emulation script; returns the report |

## emu_script

Pack a cart first (in the project: `emd pack-ggo [--world <stem>]`), then:

```json
{
  "cart": "wilds.ggo",
  "frames": 180,
  "steps": [
    { "at": 0,   "input": ["right"] },
    { "at": 60,  "screenshot": "sliding" },
    { "at": 60,  "input": [] },
    { "at": 120, "screenshot": "settled" }
  ]
}
```

Semantics — start: boots the cart in the panel (opening the panel if
needed; a missing cart file or load failure is the tool error), pauses,
and takes the parked frame as script frame 0. Body: steps run in `at`
order; `input` latches the held buttons from that frame on (level-
triggered; `[]` releases all); `screenshot` captures after `at` frames,
labeled. Finish: automatic — the final frame is always captured as
`final`, the run is stopped, and the report returns a text summary
(frames run + full uart log, i.e. the cart's own `log()` output) followed
by each captured frame as a PNG image.

Frame-exact: the host steps the emulator itself and replies only after
frames are actually delivered — no wall-clock races. `frames` is capped
at 7200 (two minutes); longer soaks are several scripts.

The `event` step slot (component insertion/removal, world edits mid-run)
is reserved and currently rejected at validation — the engine has no
mid-run mutation channel yet.
