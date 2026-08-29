//! Shared wire contract between a Zed session hosting the GGO emulator
//! panel and the `zedgg-emu-mcp` bridge binary.
//!
//! Two halves, both dependency-light so either side can link it:
//!
//! - [`protocol`]: the JSON-lines request/response types spoken over the
//!   per-session unix socket.
//! - [`registry`]: how live sessions advertise themselves (one JSON file +
//!   one socket per Zed process under the runtime dir) and how the bridge
//!   discovers and prunes them.

pub mod protocol;
pub mod registry;
