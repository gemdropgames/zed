//! Minimal MCP JSON-RPC 2.0 layer: initialize, tools/list, tools/call.
//! Hand-rolled — three methods don't justify an SDK dependency.

use std::path::Path;

use serde_json::{Value, json};

use crate::tools::{Connector, call_tool, tool_list};

/// Protocol revisions this server actually implements (the initialize /
/// tools/list / tools/call subset is identical across them). Anything
/// else negotiates down to the newest supported instead of echoing a
/// version whose semantics we do not speak.
const SUPPORTED_VERSIONS: &[&str] = &["2024-11-05", "2025-03-26", "2025-06-18"];

/// Handle one stdin line. `None` means "no reply" (notifications and
/// unparseable garbage without an id).
pub fn handle_line(line: &str, registry_dir: &Path, connect: &Connector) -> Option<Value> {
    let msg: Value = match serde_json::from_str(line) {
        Ok(v) => v,
        Err(e) => {
            return Some(error_reply(Value::Null, -32700, &format!("parse error: {e}")));
        }
    };
    let id = msg.get("id").cloned();
    let method = msg.get("method").and_then(|m| m.as_str()).unwrap_or_default();
    match (method, id) {
        // Notifications (no id): nothing to answer, whatever the method.
        (_, None) => None,
        ("initialize", Some(id)) => {
            let requested = msg["params"]["protocolVersion"].as_str().unwrap_or_default();
            let version = if SUPPORTED_VERSIONS.contains(&requested) {
                requested
            } else {
                SUPPORTED_VERSIONS[SUPPORTED_VERSIONS.len() - 1]
            };
            Some(json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": {
                    "protocolVersion": version,
                    "capabilities": { "tools": {} },
                    "serverInfo": { "name": "zedgg-emu", "version": env!("CARGO_PKG_VERSION") },
                },
            }))
        }
        ("tools/list", Some(id)) => {
            Some(json!({ "jsonrpc": "2.0", "id": id, "result": tool_list() }))
        }
        ("tools/call", Some(id)) => {
            let name = msg["params"]["name"].as_str().unwrap_or_default();
            let args = &msg["params"]["arguments"];
            let (content, is_error) = call_tool(name, args, registry_dir, connect);
            Some(json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": { "content": content, "isError": is_error },
            }))
        }
        ("ping", Some(id)) => Some(json!({ "jsonrpc": "2.0", "id": id, "result": {} })),
        (other, Some(id)) => Some(error_reply(id, -32601, &format!("method not found: {other}"))),
    }
}

fn error_reply(id: Value, code: i64, message: &str) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn no_connect() -> Box<Connector> {
        Box::new(|_: &Path, _: &str, _: std::time::Duration| panic!("must not connect"))
    }

    fn dir() -> PathBuf {
        PathBuf::from("/nonexistent-registry")
    }

    #[test]
    fn initialize_answers_with_capabilities_and_echoed_version() {
        let reply = handle_line(
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-03-26"}}"#,
            &dir(),
            &*no_connect(),
        )
        .unwrap();
        assert_eq!(reply["id"], 1);
        assert_eq!(reply["result"]["protocolVersion"], "2025-03-26");
        assert_eq!(reply["result"]["serverInfo"]["name"], "zedgg-emu");
        assert!(reply["result"]["capabilities"]["tools"].is_object());
    }

    #[test]
    fn notifications_get_no_reply() {
        assert_eq!(
            handle_line(
                r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#,
                &dir(),
                &*no_connect()
            ),
            None
        );
    }

    #[test]
    fn tools_list_and_unknown_method() {
        let reply = handle_line(
            r#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#,
            &dir(),
            &*no_connect(),
        )
        .unwrap();
        assert_eq!(reply["result"]["tools"].as_array().unwrap().len(), 3);

        let err = handle_line(
            r#"{"jsonrpc":"2.0","id":3,"method":"resources/list"}"#,
            &dir(),
            &*no_connect(),
        )
        .unwrap();
        assert_eq!(err["error"]["code"], -32601);
    }

    #[test]
    fn tools_call_with_empty_registry_is_a_tool_error_not_a_crash() {
        let reply = handle_line(
            r#"{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"emu_status"}}"#,
            &dir(),
            &*no_connect(),
        )
        .unwrap();
        assert_eq!(reply["result"]["isError"], true);
        let text = reply["result"]["content"][0]["text"].as_str().unwrap();
        assert!(text.contains("no live zed session"), "{text}");
    }

    #[test]
    fn parse_error_replies_with_null_id() {
        let reply = handle_line("garbage", &dir(), &*no_connect()).unwrap();
        assert_eq!(reply["error"]["code"], -32700);
        assert!(reply["id"].is_null());
    }
}
