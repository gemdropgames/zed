//! `zedgg-emu-mcp`: an MCP (stdio, JSON-RPC 2.0) server that lets an
//! agent drive the GGO emulator panel inside a running Zed session.
//!
//! Bridge, not emulator: every tool call is one JSON line over the
//! target session's unix socket (see `ggo_emu_remote`), answered by the
//! Zed-side host in `ggo_emu_panel::agent_remote`. Stateless per call —
//! the run itself lives in Zed, visibly, in the panel.
//!
//! Wire this into any MCP client, e.g. Zed's own agent panel:
//!
//! ```json
//! "context_servers": {
//!   "zedgg-emu": { "command": { "path": "zedgg-emu-mcp" } }
//! }
//! ```

mod png;
mod rpc;
mod tools;

use std::io::{BufRead, Write};

fn main() {
    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout();
    let dir = ggo_emu_remote::registry::dir();
    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        if line.trim().is_empty() {
            continue;
        }
        let Some(reply) = rpc::handle_line(&line, &dir, &tools::socket_call) else {
            continue; // notification — nothing to say
        };
        let mut out = reply.to_string();
        out.push('\n');
        if stdout.write_all(out.as_bytes()).and_then(|_| stdout.flush()).is_err() {
            break;
        }
    }
}
