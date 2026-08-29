# zedgg-emu-mcp — agent contract

An MCP (stdio) server that drives the GGO emulator panel inside a running
Zed session. The run happens visibly in Zed's panel; this binary is a
stateless bridge, one unix-socket round trip per tool call.

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
`workspace` (absolute project root). Both may be omitted when exactly one
candidate is live; ambiguity is an error that names the candidates. Start
with `zed_sessions` when unsure.

## Tools

| tool | what |
|---|---|
| `zed_sessions` | live sessions: pid, workspaces, panel status |
| `emu_boot { cart }` | start a project-relative cart; restarts a live run |
| `emu_input { buttons }` | latch held pad buttons (level-triggered); `[]` releases all. Names: `z x a s up down left right q w e r t y u i enter select` |
| `emu_pause` / `emu_resume` | park at / leave the next frame boundary |
| `emu_step { frames }` | while paused, run exactly N frames |
| `emu_screenshot` | last presented frame as PNG image content |
| `emu_uart { tail? }` | run diagnostics + the cart's own `log()` lines |
| `emu_status` | what the target session's panels are doing |
| `emu_stop` | end the run |

## Deterministic driving

Input is level-triggered state, not events — the cart samples "what is
held now" once per frame. For reproducible probes: `emu_pause`, then loop
`emu_input` → `emu_step` → `emu_screenshot`. Frame-exact, no wall-clock
races. `emu_resume` to let it free-run again.
