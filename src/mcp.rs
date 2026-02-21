use std::io::{self, BufRead, BufReader, Write};

use anyhow::{anyhow, Context, Result};
use serde_json::{json, Value};

use crate::indexer::{index_repository, IndexOptions};
use crate::paths::RuntimePaths;
use crate::storage::GraphStore;

const PROTOCOL_VERSION: &str = "2025-11-25";

pub fn run_mcp_stdio(paths: RuntimePaths, auto_index: bool, full_first: bool) -> Result<()> {
    if auto_index {
        let mut store = GraphStore::open(&paths.db_path)?;
        let _ = index_repository(
            &mut store,
            &paths.repo_root,
            IndexOptions { full: full_first },
        )?;
    }

    let stdin = io::stdin();
    let stdout = io::stdout();
    let mut reader = BufReader::new(stdin.lock());
    let mut writer = stdout.lock();

    while let Some(message) = read_frame(&mut reader)? {
        if let Some(method) = message.get("method").and_then(Value::as_str) {
            let id = message.get("id").cloned();
            if let Some(id) = id {
                let response = handle_request(method, message.get("params"), id, &paths)?;
                write_frame(&mut writer, &response)?;
            }
        }
    }

    Ok(())
}

fn handle_request(
    method: &str,
    params: Option<&Value>,
    id: Value,
    paths: &RuntimePaths,
) -> Result<Value> {
    let response = match method {
        "initialize" => success_response(id, initialize_result()),
        "ping" => success_response(id, json!({})),
        "tools/list" => success_response(id, json!({ "tools": tool_descriptors() })),
        "tools/call" => {
            let Some(params) = params else {
                return Ok(error_response(
                    Some(id),
                    -32602,
                    "Missing params for tools/call",
                ));
            };

            let tool_name = match params.get("name").and_then(Value::as_str) {
                Some(name) => name,
                None => {
                    return Ok(error_response(
                        Some(id),
                        -32602,
                        "tools/call requires string field `name`",
                    ))
                }
            };
            let arguments = params
                .get("arguments")
                .cloned()
                .unwrap_or_else(|| json!({}));

            match call_tool(tool_name, &arguments, paths) {
                Ok(structured_content) => success_response(id, tool_ok(structured_content)),
                Err(ToolCallError::InvalidParams(msg)) => {
                    error_response(Some(id), -32602, &format!("Invalid tool params: {msg}"))
                }
                Err(ToolCallError::Runtime(msg)) => success_response(id, tool_error(msg)),
            }
        }
        _ => error_response(Some(id), -32601, &format!("Unknown method `{method}`")),
    };

    Ok(response)
}

fn call_tool(
    tool_name: &str,
    args: &Value,
    paths: &RuntimePaths,
) -> std::result::Result<Value, ToolCallError> {
    match tool_name {
        "lumora.index_repository" => {
            let full = opt_bool(args, "full")?.unwrap_or(false);
            let mut store = open_store(paths)?;
            let report = index_repository(&mut store, &paths.repo_root, IndexOptions { full })
                .map_err(|err| ToolCallError::Runtime(err.to_string()))?;
            serde_json::to_value(report)
                .map_err(|err| ToolCallError::Runtime(format!("serialization error: {err}")))
        }
        "lumora.symbol_definitions" => {
            let symbol = required_str(args, "name")?;
            let store = open_store(paths)?;
            let rows = store
                .symbol_definitions(symbol)
                .map_err(|err| ToolCallError::Runtime(err.to_string()))?;
            Ok(json!({ "rows": rows }))
        }
        "lumora.symbol_references" => {
            let symbol = required_str(args, "name")?;
            let calls_only = opt_bool(args, "calls_only")?.unwrap_or(false);
            let store = open_store(paths)?;
            let rows = if calls_only {
                store
                    .symbol_references(symbol, Some("calls"))
                    .map_err(|err| ToolCallError::Runtime(err.to_string()))?
            } else {
                store
                    .symbol_references(symbol, None)
                    .map_err(|err| ToolCallError::Runtime(err.to_string()))?
            };
            Ok(json!({ "rows": rows }))
        }
        "lumora.symbol_callers" => {
            let symbol = required_str(args, "name")?;
            let store = open_store(paths)?;
            let rows = store
                .symbol_references(symbol, Some("calls"))
                .map_err(|err| ToolCallError::Runtime(err.to_string()))?;
            Ok(json!({ "rows": rows }))
        }
        "lumora.dependency_path" => {
            let from = required_str(args, "from")?;
            let to = required_str(args, "to")?;
            let max_depth = opt_u64(args, "max_depth")?.unwrap_or(8).max(1) as usize;
            let store = open_store(paths)?;
            let value = store
                .dependency_path(from, to, max_depth)
                .map_err(|err| ToolCallError::Runtime(err.to_string()))?;
            serde_json::to_value(value)
                .map_err(|err| ToolCallError::Runtime(format!("serialization error: {err}")))
        }
        "lumora.minimal_slice" => {
            let file = required_str(args, "file")?;
            let line = opt_i64(args, "line")?;
            let depth = opt_u64(args, "depth")?.unwrap_or(2).max(1) as usize;
            let store = open_store(paths)?;
            let value = store
                .minimal_slice(file, line, depth)
                .map_err(|err| ToolCallError::Runtime(err.to_string()))?;
            Ok(json!({ "slice": value }))
        }
        "lumora.clone_matches" => {
            let file = required_str(args, "file")?;
            let min_similarity = opt_f64(args, "min_similarity")?.unwrap_or(0.2);
            let store = open_store(paths)?;
            let rows = store
                .clone_matches(file, min_similarity)
                .map_err(|err| ToolCallError::Runtime(err.to_string()))?;
            Ok(json!({ "rows": rows }))
        }
        _ => Err(ToolCallError::InvalidParams(format!(
            "Unknown tool `{tool_name}`"
        ))),
    }
}

fn open_store(paths: &RuntimePaths) -> std::result::Result<GraphStore, ToolCallError> {
    GraphStore::open(&paths.db_path).map_err(|err| ToolCallError::Runtime(err.to_string()))
}

fn initialize_result() -> Value {
    json!({
        "protocolVersion": PROTOCOL_VERSION,
        "capabilities": {
            "tools": {
                "listChanged": false
            }
        },
        "serverInfo": {
            "name": "lumora",
            "version": env!("CARGO_PKG_VERSION")
        }
    })
}

fn tool_descriptors() -> Vec<Value> {
    vec![
        json!({
            "name": "lumora.index_repository",
            "description": "Run incremental or full indexing for the configured repository.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "full": { "type": "boolean", "description": "Set true for full rebuild." }
                }
            }
        }),
        json!({
            "name": "lumora.symbol_definitions",
            "description": "Find symbol definition locations by name.",
            "inputSchema": {
                "type": "object",
                "required": ["name"],
                "properties": {
                    "name": { "type": "string" }
                }
            }
        }),
        json!({
            "name": "lumora.symbol_references",
            "description": "Find references (and optionally calls-only) for a symbol name.",
            "inputSchema": {
                "type": "object",
                "required": ["name"],
                "properties": {
                    "name": { "type": "string" },
                    "calls_only": { "type": "boolean" }
                }
            }
        }),
        json!({
            "name": "lumora.symbol_callers",
            "description": "Find call sites for a symbol name.",
            "inputSchema": {
                "type": "object",
                "required": ["name"],
                "properties": {
                    "name": { "type": "string" }
                }
            }
        }),
        json!({
            "name": "lumora.dependency_path",
            "description": "Find a dependency path from selector A to selector B.",
            "inputSchema": {
                "type": "object",
                "required": ["from", "to"],
                "properties": {
                    "from": { "type": "string" },
                    "to": { "type": "string" },
                    "max_depth": { "type": "integer", "minimum": 1 }
                }
            }
        }),
        json!({
            "name": "lumora.minimal_slice",
            "description": "Return a minimal graph slice around a file and optional line.",
            "inputSchema": {
                "type": "object",
                "required": ["file"],
                "properties": {
                    "file": { "type": "string" },
                    "line": { "type": ["integer", "null"] },
                    "depth": { "type": "integer", "minimum": 1 }
                }
            }
        }),
        json!({
            "name": "lumora.clone_matches",
            "description": "Find likely clone files using token winnowing fingerprints.",
            "inputSchema": {
                "type": "object",
                "required": ["file"],
                "properties": {
                    "file": { "type": "string" },
                    "min_similarity": { "type": "number", "minimum": 0.0, "maximum": 1.0 }
                }
            }
        }),
    ]
}

fn tool_ok(structured_content: Value) -> Value {
    json!({
        "content": [
            {
                "type": "text",
                "text": serde_json::to_string_pretty(&structured_content)
                    .unwrap_or_else(|_| "{}".to_string())
            }
        ],
        "structuredContent": structured_content
    })
}

fn tool_error(message: String) -> Value {
    json!({
        "content": [
            {
                "type": "text",
                "text": message
            }
        ],
        "isError": true
    })
}

fn success_response(id: Value, result: Value) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "result": result
    })
}

fn error_response(id: Option<Value>, code: i64, message: &str) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": {
            "code": code,
            "message": message
        }
    })
}

fn read_frame(reader: &mut impl BufRead) -> Result<Option<Value>> {
    let mut content_length = None;

    loop {
        let mut line = String::new();
        let n = reader.read_line(&mut line)?;
        if n == 0 {
            return Ok(None);
        }

        let trimmed = line.trim_end_matches(['\r', '\n']);
        if trimmed.is_empty() {
            break;
        }

        if let Some(rest) = trimmed.strip_prefix("Content-Length:") {
            let parsed = rest
                .trim()
                .parse::<usize>()
                .context("invalid Content-Length header")?;
            content_length = Some(parsed);
        }
    }

    let len = content_length.ok_or_else(|| anyhow!("missing Content-Length header"))?;
    let mut payload = vec![0_u8; len];
    reader.read_exact(&mut payload)?;
    let value = serde_json::from_slice::<Value>(&payload)?;
    Ok(Some(value))
}

fn write_frame(writer: &mut impl Write, payload: &Value) -> Result<()> {
    let serialized = serde_json::to_vec(payload)?;
    write!(writer, "Content-Length: {}\r\n\r\n", serialized.len())?;
    writer.write_all(&serialized)?;
    writer.flush()?;
    Ok(())
}

fn required_str<'a>(args: &'a Value, key: &str) -> std::result::Result<&'a str, ToolCallError> {
    args.get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| ToolCallError::InvalidParams(format!("missing string field `{key}`")))
}

fn opt_bool(args: &Value, key: &str) -> std::result::Result<Option<bool>, ToolCallError> {
    match args.get(key) {
        Some(v) => v
            .as_bool()
            .map(Some)
            .ok_or_else(|| ToolCallError::InvalidParams(format!("`{key}` must be boolean"))),
        None => Ok(None),
    }
}

fn opt_u64(args: &Value, key: &str) -> std::result::Result<Option<u64>, ToolCallError> {
    match args.get(key) {
        Some(v) => v
            .as_u64()
            .map(Some)
            .ok_or_else(|| ToolCallError::InvalidParams(format!("`{key}` must be an integer"))),
        None => Ok(None),
    }
}

fn opt_i64(args: &Value, key: &str) -> std::result::Result<Option<i64>, ToolCallError> {
    match args.get(key) {
        Some(v) if v.is_null() => Ok(None),
        Some(v) => v.as_i64().map(Some).ok_or_else(|| {
            ToolCallError::InvalidParams(format!("`{key}` must be integer or null"))
        }),
        None => Ok(None),
    }
}

fn opt_f64(args: &Value, key: &str) -> std::result::Result<Option<f64>, ToolCallError> {
    match args.get(key) {
        Some(v) => v
            .as_f64()
            .map(Some)
            .ok_or_else(|| ToolCallError::InvalidParams(format!("`{key}` must be numeric"))),
        None => Ok(None),
    }
}

enum ToolCallError {
    InvalidParams(String),
    Runtime(String),
}
