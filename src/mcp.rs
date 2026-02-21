use std::fs;
use std::io::{self, BufRead, BufReader, Write};

use anyhow::{anyhow, Context, Result};
use serde_json::{json, Value};

use crate::indexer::{index_repository, IndexOptions};
use crate::paths::RuntimePaths;
use crate::storage::{
    CloneQueryOptions, GraphStore, ReferenceQueryOptions, SelectorSuggestOptions,
    SliceQueryOptions, SortOrder,
};

const DEFAULT_PROTOCOL_VERSION: &str = "2025-06-18";

#[derive(Clone, Copy)]
enum FrameStyle {
    ContentLength,
    LineDelimited,
}

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

    while let Some(frame) = read_frame(&mut reader)? {
        let message = frame.value;
        if let Some(method) = message.get("method").and_then(Value::as_str) {
            let id = message.get("id").cloned();
            if let Some(id) = id {
                let response = handle_request(method, message.get("params"), id, &paths)?;
                write_frame(&mut writer, &response, frame.style)?;
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
        "initialize" => success_response(id, initialize_result(params)),
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
            let verbosity = opt_verbosity(args, "verbosity")?.unwrap_or(Verbosity::Normal);
            let limit = opt_u64(args, "limit")?.unwrap_or(200) as usize;
            let offset = opt_u64(args, "offset")?.unwrap_or(0) as usize;
            let dedup = opt_bool(args, "dedup")?.unwrap_or(true);
            let order = opt_order(args, "order")?.unwrap_or(SortOrder::ScoreDesc);
            let file_glob = opt_string(args, "file_glob")?;
            let language = opt_string(args, "language")?;
            let max_age_hours = opt_u64(args, "max_age_hours")?;
            let summary_mode = opt_string(args, "summary_mode")?;
            let include_freshness = opt_bool(args, "include_freshness")?.unwrap_or(false);
            let calls_only = opt_bool(args, "calls_only")?.unwrap_or(false);
            let edge_type = opt_string(args, "edge_type")?;

            let effective_edge_type = if let Some(edge_type) = edge_type {
                Some(edge_type)
            } else if calls_only {
                Some("calls".to_string())
            } else {
                None
            };

            let options = ReferenceQueryOptions {
                edge_type_filter: effective_edge_type,
                file_glob,
                language,
                max_age_hours,
                limit: limit.max(1),
                offset,
                dedup,
                order,
            };
            let store = open_store(paths)?;
            let (rows, pagination) = store
                .symbol_references_page(symbol, &options)
                .map_err(|err| ToolCallError::Runtime(err.to_string()))?;
            let summary = if summary_mode.as_deref() == Some("top_files") {
                Some(store.top_reference_files(&rows, 10))
            } else {
                None
            };

            let mut response = json!({ "rows": rows, "pagination": pagination });
            if let Some(summary) = summary {
                response["top_files"] = serde_json::to_value(summary)
                    .map_err(|err| ToolCallError::Runtime(format!("serialization error: {err}")))?;
            }
            attach_diagnostics(
                &store,
                &mut response,
                verbosity,
                include_freshness,
                json!({
                    "query": {
                        "name": symbol,
                        "limit": limit.max(1),
                        "offset": offset,
                        "dedup": dedup,
                        "order": order_name(order),
                        "file_glob": options.file_glob,
                        "language": options.language,
                        "max_age_hours": options.max_age_hours,
                        "edge_type": options.edge_type_filter
                    }
                }),
            )?;
            Ok(compact_if_needed(response, verbosity))
        }
        "lumora.symbol_callers" => {
            let symbol = required_str(args, "name")?;
            let verbosity = opt_verbosity(args, "verbosity")?.unwrap_or(Verbosity::Normal);
            let limit = opt_u64(args, "limit")?.unwrap_or(200) as usize;
            let offset = opt_u64(args, "offset")?.unwrap_or(0) as usize;
            let dedup = opt_bool(args, "dedup")?.unwrap_or(true);
            let order = opt_order(args, "order")?.unwrap_or(SortOrder::ScoreDesc);
            let file_glob = opt_string(args, "file_glob")?;
            let language = opt_string(args, "language")?;
            let max_age_hours = opt_u64(args, "max_age_hours")?;
            let summary_mode = opt_string(args, "summary_mode")?;
            let include_freshness = opt_bool(args, "include_freshness")?.unwrap_or(false);

            let options = ReferenceQueryOptions {
                edge_type_filter: Some("calls".to_string()),
                file_glob,
                language,
                max_age_hours,
                limit: limit.max(1),
                offset,
                dedup,
                order,
            };
            let store = open_store(paths)?;
            let (rows, pagination) = store
                .symbol_references_page(symbol, &options)
                .map_err(|err| ToolCallError::Runtime(err.to_string()))?;
            let summary = if summary_mode.as_deref() == Some("top_files") {
                Some(store.top_reference_files(&rows, 10))
            } else {
                None
            };
            let mut response = json!({ "rows": rows, "pagination": pagination });
            if let Some(summary) = summary {
                response["top_files"] = serde_json::to_value(summary)
                    .map_err(|err| ToolCallError::Runtime(format!("serialization error: {err}")))?;
            }

            attach_diagnostics(
                &store,
                &mut response,
                verbosity,
                include_freshness,
                json!({
                    "query": {
                        "name": symbol,
                        "limit": limit.max(1),
                        "offset": offset,
                        "dedup": dedup,
                        "order": order_name(order),
                        "file_glob": options.file_glob,
                        "language": options.language,
                        "max_age_hours": options.max_age_hours
                    }
                }),
            )?;
            Ok(compact_if_needed(response, verbosity))
        }
        "lumora.dependency_path" => {
            let from = required_str(args, "from")?;
            let to = required_str(args, "to")?;
            let verbosity = opt_verbosity(args, "verbosity")?.unwrap_or(Verbosity::Normal);
            let include_freshness = opt_bool(args, "include_freshness")?.unwrap_or(false);
            let max_depth = opt_u64(args, "max_depth")?.unwrap_or(8).max(1) as usize;
            let store = open_store(paths)?;
            let (path, from_diag, to_diag) = store
                .dependency_path_with_diagnostics(from, to, max_depth)
                .map_err(|err| {
                    let msg = err.to_string();
                    if msg.contains("selector") || msg.contains("invalid `") {
                        ToolCallError::InvalidParams(format!(
                            "{msg}. Selector examples: file:src/main.rs, symbol_name:rust:main, symbol:main"
                        ))
                    } else {
                        ToolCallError::Runtime(msg)
                    }
                })?;
            let mut response = serde_json::to_value(path)
                .map_err(|err| ToolCallError::Runtime(format!("serialization error: {err}")))?;
            attach_diagnostics(
                &store,
                &mut response,
                verbosity,
                include_freshness,
                json!({
                    "selector": {
                        "from": from_diag,
                        "to": to_diag
                    },
                    "query": {
                        "from": from,
                        "to": to,
                        "max_depth": max_depth
                    }
                }),
            )?;
            Ok(compact_if_needed(response, verbosity))
        }
        "lumora.minimal_slice" => {
            let file = required_str(args, "file")?;
            let line = opt_i64(args, "line")?;
            let depth = opt_u64(args, "depth")?.unwrap_or(2).max(1) as usize;
            let max_neighbors = opt_u64(args, "max_neighbors")?.unwrap_or(40) as usize;
            let dedup = opt_bool(args, "dedup")?.unwrap_or(true);
            let suppress_low_signal_repeats =
                opt_bool(args, "suppress_low_signal_repeats")?.unwrap_or(true);
            let low_signal_name_cap = opt_u64(args, "low_signal_name_cap")?.unwrap_or(1) as usize;
            let prefer_project_symbols = opt_bool(args, "prefer_project_symbols")?.unwrap_or(true);
            let include_freshness = opt_bool(args, "include_freshness")?.unwrap_or(false);
            let verbosity = opt_verbosity(args, "verbosity")?.unwrap_or(Verbosity::Normal);
            let store = open_store(paths)?;
            let options = SliceQueryOptions {
                max_neighbors,
                dedup,
                suppress_low_signal_repeats,
                low_signal_name_cap,
                prefer_project_symbols,
            };
            let value = store
                .minimal_slice_with_options(file, line, depth, &options)
                .map_err(|err| ToolCallError::Runtime(err.to_string()))?;
            let mut response = json!({ "slice": value });
            attach_diagnostics(
                &store,
                &mut response,
                verbosity,
                include_freshness,
                json!({
                    "query": {
                        "file": file,
                        "line": line,
                        "depth": depth,
                        "max_neighbors": max_neighbors,
                        "dedup": dedup,
                        "suppress_low_signal_repeats": suppress_low_signal_repeats,
                        "low_signal_name_cap": low_signal_name_cap,
                        "prefer_project_symbols": prefer_project_symbols
                    }
                }),
            )?;
            Ok(compact_if_needed(response, verbosity))
        }
        "lumora.clone_matches" => {
            let file = required_str(args, "file")?;
            let min_similarity = opt_f64(args, "min_similarity")?.unwrap_or(0.02);
            let limit = opt_u64(args, "limit")?.unwrap_or(50) as usize;
            let offset = opt_u64(args, "offset")?.unwrap_or(0) as usize;
            let mode = opt_string(args, "mode")?.unwrap_or_else(|| "matches".to_string());
            let include_freshness = opt_bool(args, "include_freshness")?.unwrap_or(false);
            let verbosity = opt_verbosity(args, "verbosity")?.unwrap_or(Verbosity::Normal);
            let store = open_store(paths)?;
            let options = CloneQueryOptions {
                min_similarity,
                limit,
                offset,
            };
            let mut response = if mode == "hotspots" {
                let (rows, pagination, analysis) = store
                    .clone_hotspots_page(file, &options)
                    .map_err(|err| ToolCallError::Runtime(err.to_string()))?;
                json!({ "rows": rows, "mode": "hotspots", "pagination": pagination, "analysis": analysis })
            } else {
                let (rows, pagination, analysis) = store
                    .clone_matches_page(file, &options)
                    .map_err(|err| ToolCallError::Runtime(err.to_string()))?;
                json!({ "rows": rows, "mode": "matches", "pagination": pagination, "analysis": analysis })
            };
            if response["rows"]
                .as_array()
                .map(|rows| rows.is_empty())
                .unwrap_or(false)
            {
                if let Some(reason) = response["analysis"]["empty_reason"].as_str() {
                    response["warning"] = json!(reason);
                }
            }
            attach_diagnostics(
                &store,
                &mut response,
                verbosity,
                include_freshness,
                json!({
                    "query": {
                        "file": file,
                        "min_similarity": min_similarity,
                        "limit": limit,
                        "offset": offset,
                        "mode": mode
                    }
                }),
            )?;
            Ok(compact_if_needed(response, verbosity))
        }
        "lumora.selector_discover" => {
            let query = opt_string(args, "query")?;
            let limit = opt_u64(args, "limit")?.unwrap_or(50).max(1) as usize;
            let file_glob = opt_string(args, "file_glob")?;
            let entity_type = opt_string(args, "entity_type")?;
            let fuzzy = opt_bool(args, "fuzzy")?.unwrap_or(true);
            let store = open_store(paths)?;
            let rows = store
                .selector_suggestions_advanced(&SelectorSuggestOptions {
                    query,
                    file_glob: file_glob.clone(),
                    entity_type: entity_type.clone(),
                    limit,
                    fuzzy,
                })
                .map_err(|err| ToolCallError::Runtime(err.to_string()))?;
            Ok(json!({
                "rows": rows,
                "query_info": {
                    "fuzzy": fuzzy,
                    "file_glob": file_glob,
                    "entity_type": entity_type
                },
                "selector_examples": [
                    "file:src/main.rs",
                    "symbol_name:rust:run_mcp_stdio",
                    "symbol:main"
                ]
            }))
        }
        _ => Err(ToolCallError::InvalidParams(format!(
            "Unknown tool `{tool_name}`"
        ))),
    }
}

fn open_store(paths: &RuntimePaths) -> std::result::Result<GraphStore, ToolCallError> {
    if let Some(parent) = paths.db_path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    GraphStore::open(&paths.db_path).map_err(|err| ToolCallError::Runtime(err.to_string()))
}

fn initialize_result(params: Option<&Value>) -> Value {
    let protocol_version = params
        .and_then(|value| value.get("protocolVersion"))
        .and_then(Value::as_str)
        .unwrap_or(DEFAULT_PROTOCOL_VERSION);

    json!({
        "protocolVersion": protocol_version,
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
            "description": "Find references for a symbol name with ranking, paging, filtering, and summary controls.",
            "inputSchema": {
                "type": "object",
                "required": ["name"],
                "properties": {
                    "name": { "type": "string" },
                    "calls_only": { "type": "boolean" },
                    "edge_type": { "type": "string", "enum": ["references", "calls"] },
                    "file_glob": { "type": "string" },
                    "language": { "type": "string" },
                    "max_age_hours": { "type": "integer", "minimum": 1 },
                    "limit": { "type": "integer", "minimum": 1 },
                    "offset": { "type": "integer", "minimum": 0 },
                    "dedup": { "type": "boolean" },
                    "order": { "type": "string", "enum": ["asc", "desc", "score_desc", "line_asc", "line_desc"] },
                    "summary_mode": { "type": "string", "enum": ["top_files"] },
                    "include_freshness": { "type": "boolean" },
                    "verbosity": { "type": "string", "enum": ["compact", "normal", "debug"] }
                }
            }
        }),
        json!({
            "name": "lumora.symbol_callers",
            "description": "Find call sites for a symbol name with ranking, paging, filtering, and summary controls.",
            "inputSchema": {
                "type": "object",
                "required": ["name"],
                "properties": {
                    "name": { "type": "string" },
                    "file_glob": { "type": "string" },
                    "language": { "type": "string" },
                    "max_age_hours": { "type": "integer", "minimum": 1 },
                    "limit": { "type": "integer", "minimum": 1 },
                    "offset": { "type": "integer", "minimum": 0 },
                    "dedup": { "type": "boolean" },
                    "order": { "type": "string", "enum": ["asc", "desc", "score_desc", "line_asc", "line_desc"] },
                    "summary_mode": { "type": "string", "enum": ["top_files"] },
                    "include_freshness": { "type": "boolean" },
                    "verbosity": { "type": "string", "enum": ["compact", "normal", "debug"] }
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
                    "max_depth": { "type": "integer", "minimum": 1 },
                    "include_freshness": { "type": "boolean" },
                    "verbosity": { "type": "string", "enum": ["compact", "normal", "debug"] }
                }
            }
        }),
        json!({
            "name": "lumora.minimal_slice",
            "description": "Return a bounded graph slice around a file and optional line.",
            "inputSchema": {
                "type": "object",
                "required": ["file"],
                "properties": {
                    "file": { "type": "string" },
                    "line": { "type": ["integer", "null"] },
                    "depth": { "type": "integer", "minimum": 1 },
                    "max_neighbors": { "type": "integer", "minimum": 1 },
                    "dedup": { "type": "boolean" },
                    "suppress_low_signal_repeats": { "type": "boolean" },
                    "low_signal_name_cap": { "type": "integer", "minimum": 1 },
                    "prefer_project_symbols": { "type": "boolean" },
                    "include_freshness": { "type": "boolean" },
                    "verbosity": { "type": "string", "enum": ["compact", "normal", "debug"] }
                }
            }
        }),
        json!({
            "name": "lumora.clone_matches",
            "description": "Find likely clone files or near-duplicate hotspots.",
            "inputSchema": {
                "type": "object",
                "required": ["file"],
                "properties": {
                    "file": { "type": "string" },
                    "min_similarity": { "type": "number", "minimum": 0.0, "maximum": 1.0 },
                    "limit": { "type": "integer", "minimum": 1 },
                    "offset": { "type": "integer", "minimum": 0 },
                    "mode": { "type": "string", "enum": ["matches", "hotspots"] },
                    "include_freshness": { "type": "boolean" },
                    "verbosity": { "type": "string", "enum": ["compact", "normal", "debug"] }
                }
            }
        }),
        json!({
            "name": "lumora.selector_discover",
            "description": "List known selectors (files, symbol names, keys) to help construct queries.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "query": { "type": "string" },
                    "limit": { "type": "integer", "minimum": 1 },
                    "file_glob": { "type": "string" },
                    "entity_type": { "type": "string", "enum": ["file", "symbol", "symbol_name", "module", "config", "entrypoint"] },
                    "fuzzy": { "type": "boolean" }
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

struct InboundFrame {
    style: FrameStyle,
    value: Value,
}

fn read_frame(reader: &mut impl BufRead) -> Result<Option<InboundFrame>> {
    let mut first_line = String::new();
    let first_n = reader.read_line(&mut first_line)?;
    if first_n == 0 {
        return Ok(None);
    }

    let first_trimmed = first_line.trim_end_matches(['\r', '\n']).trim();
    if first_trimmed.is_empty() {
        return read_frame(reader);
    }

    if first_trimmed.starts_with('{') || first_trimmed.starts_with('[') {
        let value = serde_json::from_str::<Value>(first_trimmed)
            .context("invalid line-delimited JSON frame")?;
        return Ok(Some(InboundFrame {
            style: FrameStyle::LineDelimited,
            value,
        }));
    }

    let mut content_length = parse_content_length_header(first_trimmed)?;

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

        if let Some(parsed) = parse_content_length_header(trimmed)? {
            content_length = Some(parsed);
        }
    }

    let len = content_length.ok_or_else(|| anyhow!("missing Content-Length header"))?;
    let mut payload = vec![0_u8; len];
    reader.read_exact(&mut payload)?;
    let value = serde_json::from_slice::<Value>(&payload)?;
    Ok(Some(InboundFrame {
        style: FrameStyle::ContentLength,
        value,
    }))
}

fn parse_content_length_header(line: &str) -> Result<Option<usize>> {
    let Some((name, value)) = line.split_once(':') else {
        return Ok(None);
    };

    if !name.trim().eq_ignore_ascii_case("Content-Length") {
        return Ok(None);
    }

    let parsed = value
        .trim()
        .parse::<usize>()
        .context("invalid Content-Length header")?;
    Ok(Some(parsed))
}

fn write_frame(writer: &mut impl Write, payload: &Value, style: FrameStyle) -> Result<()> {
    let serialized = serde_json::to_vec(payload)?;
    match style {
        FrameStyle::ContentLength => {
            write!(writer, "Content-Length: {}\r\n\r\n", serialized.len())?;
            writer.write_all(&serialized)?;
        }
        FrameStyle::LineDelimited => {
            writer.write_all(&serialized)?;
            writer.write_all(b"\n")?;
        }
    }
    writer.flush()?;
    Ok(())
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Verbosity {
    Compact,
    Normal,
    Debug,
}

fn compact_if_needed(mut value: Value, verbosity: Verbosity) -> Value {
    if verbosity == Verbosity::Compact {
        value.as_object_mut().map(|obj| obj.remove("diagnostics"));
        strip_compact_fields(&mut value);
    }
    value
}

fn strip_compact_fields(value: &mut Value) {
    match value {
        Value::Object(map) => {
            map.remove("why");
            map.remove("meta_json");
            map.remove("diagnostics");
            for nested in map.values_mut() {
                strip_compact_fields(nested);
            }
        }
        Value::Array(items) => {
            for item in items {
                strip_compact_fields(item);
            }
        }
        _ => {}
    }
}

fn attach_diagnostics(
    store: &GraphStore,
    response: &mut Value,
    verbosity: Verbosity,
    include_freshness: bool,
    mut details: Value,
) -> std::result::Result<(), ToolCallError> {
    let warning = store
        .index_warning(24)
        .map_err(|err| ToolCallError::Runtime(err.to_string()))?;
    if let Some(warning) = warning {
        response["warning"] = json!(warning);
    }

    if include_freshness || verbosity == Verbosity::Debug {
        let freshness = store
            .freshness_info(24)
            .map_err(|err| ToolCallError::Runtime(err.to_string()))?;
        response["freshness"] = serde_json::to_value(&freshness)
            .map_err(|err| ToolCallError::Runtime(format!("serialization error: {err}")))?;
    }

    if verbosity == Verbosity::Debug {
        if let Some(warning) = response.get("warning").cloned() {
            details["index_warning"] = warning;
        }
        if let Some(freshness) = response.get("freshness").cloned() {
            details["freshness"] = freshness;
        }
        response["diagnostics"] = details;
    }
    Ok(())
}

fn order_name(order: SortOrder) -> &'static str {
    match order {
        SortOrder::ScoreDesc => "score_desc",
        SortOrder::LineAsc => "line_asc",
        SortOrder::LineDesc => "line_desc",
    }
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

fn opt_string(args: &Value, key: &str) -> std::result::Result<Option<String>, ToolCallError> {
    match args.get(key) {
        Some(v) if v.is_null() => Ok(None),
        Some(v) => v
            .as_str()
            .map(|value| Some(value.to_string()))
            .ok_or_else(|| ToolCallError::InvalidParams(format!("`{key}` must be string"))),
        None => Ok(None),
    }
}

fn opt_order(args: &Value, key: &str) -> std::result::Result<Option<SortOrder>, ToolCallError> {
    let Some(value) = opt_string(args, key)? else {
        return Ok(None);
    };
    match value.as_str() {
        "asc" => Ok(Some(SortOrder::LineAsc)),
        "desc" => Ok(Some(SortOrder::LineDesc)),
        "score_desc" => Ok(Some(SortOrder::ScoreDesc)),
        "line_asc" => Ok(Some(SortOrder::LineAsc)),
        "line_desc" => Ok(Some(SortOrder::LineDesc)),
        _ => Err(ToolCallError::InvalidParams(format!(
            "`{key}` must be one of: asc, desc, score_desc, line_asc, line_desc"
        ))),
    }
}

fn opt_verbosity(args: &Value, key: &str) -> std::result::Result<Option<Verbosity>, ToolCallError> {
    let Some(value) = opt_string(args, key)? else {
        return Ok(None);
    };
    match value.as_str() {
        "compact" => Ok(Some(Verbosity::Compact)),
        "normal" => Ok(Some(Verbosity::Normal)),
        "debug" => Ok(Some(Verbosity::Debug)),
        _ => Err(ToolCallError::InvalidParams(format!(
            "`{key}` must be one of: compact, normal, debug"
        ))),
    }
}

enum ToolCallError {
    InvalidParams(String),
    Runtime(String),
}
