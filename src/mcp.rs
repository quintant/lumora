use std::fs;
use std::io::{self, BufRead, BufReader, Write};

use anyhow::{anyhow, Context, Result};
use serde_json::{json, Value};

use crate::fileops;
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
        "lumora.read_file" => {
            let path = required_str(args, "path")?;
            let start_line = opt_u64(args, "start_line")?;
            let end_line = opt_u64(args, "end_line")?;
            let max_lines = opt_u64(args, "max_lines")?.unwrap_or(500);
            fileops::read_file_contents(&paths.repo_root, path, start_line, end_line, max_lines)
                .map_err(|err| ToolCallError::Runtime(err.to_string()))
        }
        "lumora.file_outline" => {
            let path = required_str(args, "path")?;
            let max_depth = opt_u64(args, "max_depth")?.map(|v| v as usize);
            fileops::file_outline(&paths.repo_root, path, max_depth)
                .map_err(|err| ToolCallError::Runtime(err.to_string()))
        }
        "lumora.search_files" => {
            let pattern = required_str(args, "pattern")?;
            let file_glob = opt_string(args, "file_glob")?;
            let context_lines = opt_u64(args, "context_lines")?.unwrap_or(2);
            let max_results = opt_u64(args, "max_results")?.unwrap_or(50);
            let is_regex = opt_bool(args, "is_regex")?.unwrap_or(false);
            fileops::search_in_files(
                &paths.repo_root,
                pattern,
                file_glob.as_deref(),
                context_lines,
                max_results,
                is_regex,
            )
            .map_err(|err| ToolCallError::Runtime(err.to_string()))
        }
        "lumora.list_directory" => {
            let path = opt_string(args, "path")?.unwrap_or_else(|| ".".to_string());
            let recursive = opt_bool(args, "recursive")?.unwrap_or(false);
            let max_depth = opt_u64(args, "max_depth")?.unwrap_or(3);
            let file_glob = opt_string(args, "file_glob")?;
            fileops::list_dir(
                &paths.repo_root,
                &path,
                recursive,
                max_depth,
                file_glob.as_deref(),
            )
            .map_err(|err| ToolCallError::Runtime(err.to_string()))
        }
        "lumora.write_file" => {
            let path = required_str(args, "path")?;
            let content = required_str(args, "content")?;
            let create_dirs = opt_bool(args, "create_dirs")?.unwrap_or(true);
            fileops::write_file_contents(&paths.repo_root, path, content, create_dirs)
                .map_err(|err| ToolCallError::Runtime(err.to_string()))
        }
        "lumora.edit_file" => {
            let path = required_str(args, "path")?;
            let old_text = required_str(args, "old_text")?;
            let new_text = required_str(args, "new_text")?;
            let dry_run = opt_bool(args, "dry_run")?.unwrap_or(false);
            fileops::edit_file_contents(&paths.repo_root, path, old_text, new_text, dry_run)
                .map_err(|err| ToolCallError::Runtime(err.to_string()))
        }
        "lumora.multi_read" => {
            let reads_arg = args
                .get("reads")
                .ok_or_else(|| ToolCallError::InvalidParams("missing field `reads`".to_string()))?;
            let reads_array = reads_arg.as_array().ok_or_else(|| {
                ToolCallError::InvalidParams("`reads` must be an array".to_string())
            })?;

            let mut reads = Vec::with_capacity(reads_array.len());
            for (idx, item) in reads_array.iter().enumerate() {
                let obj = item.as_object().ok_or_else(|| {
                    ToolCallError::InvalidParams(format!("`reads[{idx}]` must be an object"))
                })?;
                let path = obj
                    .get("path")
                    .and_then(Value::as_str)
                    .ok_or_else(|| {
                        ToolCallError::InvalidParams(format!(
                            "`reads[{idx}].path` must be a string"
                        ))
                    })?
                    .to_string();

                let start_line = match obj.get("start_line") {
                    Some(value) => Some(value.as_u64().ok_or_else(|| {
                        ToolCallError::InvalidParams(format!(
                            "`reads[{idx}].start_line` must be an integer"
                        ))
                    })?),
                    None => None,
                };
                let end_line = match obj.get("end_line") {
                    Some(value) => Some(value.as_u64().ok_or_else(|| {
                        ToolCallError::InvalidParams(format!(
                            "`reads[{idx}].end_line` must be an integer"
                        ))
                    })?),
                    None => None,
                };

                reads.push(fileops::MultiReadRequest {
                    path,
                    start_line,
                    end_line,
                });
            }

            let max_total_lines = opt_u64(args, "max_total_lines")?.unwrap_or(2000);
            fileops::multi_read(&paths.repo_root, &reads, max_total_lines)
                .map_err(|err| ToolCallError::Runtime(err.to_string()))
        }
        "lumora.move_file" => {
            let source = required_str(args, "source")?;
            let destination = required_str(args, "destination")?;
            fileops::move_file_op(&paths.repo_root, source, destination)
                .map_err(|err| ToolCallError::Runtime(err.to_string()))
        }
        "lumora.delete_file" => {
            let path = required_str(args, "path")?;
            fileops::delete_file_op(&paths.repo_root, path)
                .map_err(|err| ToolCallError::Runtime(err.to_string()))
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
        json!({
            "name": "lumora.read_file",
            "description": "Read file contents with optional line range for efficient partial reads.",
            "inputSchema": {
                "type": "object",
                "required": ["path"],
                "properties": {
                    "path": { "type": "string" },
                    "start_line": { "type": "integer" },
                    "end_line": { "type": "integer" },
                    "max_lines": { "type": "integer", "default": 500 }
                }
            }
        }),
        json!({
            "name": "lumora.file_outline",
            "description": "Get AST-derived structure outline of a file (definitions only, no content). Fast symbol lookup.",
            "inputSchema": {
                "type": "object",
                "required": ["path"],
                "properties": {
                    "path": { "type": "string" },
                    "max_depth": { "type": "integer" }
                }
            }
        }),
        json!({
            "name": "lumora.search_files",
            "description": "Search file contents with regex or literal patterns. Returns matches with context.",
            "inputSchema": {
                "type": "object",
                "required": ["pattern"],
                "properties": {
                    "pattern": { "type": "string" },
                    "file_glob": { "type": "string" },
                    "context_lines": { "type": "integer", "default": 2 },
                    "max_results": { "type": "integer", "default": 50 },
                    "is_regex": { "type": "boolean", "default": false }
                }
            }
        }),
        json!({
            "name": "lumora.list_directory",
            "description": "List directory contents with metadata.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "path": { "type": "string", "default": "." },
                    "recursive": { "type": "boolean", "default": false },
                    "max_depth": { "type": "integer", "default": 3 },
                    "file_glob": { "type": "string" }
                }
            }
        }),
        json!({
            "name": "lumora.write_file",
            "description": "Create or overwrite a file.",
            "inputSchema": {
                "type": "object",
                "required": ["path", "content"],
                "properties": {
                    "path": { "type": "string" },
                    "content": { "type": "string" },
                    "create_dirs": { "type": "boolean", "default": true }
                }
            }
        }),
        json!({
            "name": "lumora.edit_file",
            "description": "Search-and-replace edit. old_text must match exactly once in the file.",
            "inputSchema": {
                "type": "object",
                "required": ["path", "old_text", "new_text"],
                "properties": {
                    "path": { "type": "string" },
                    "old_text": { "type": "string" },
                    "new_text": { "type": "string" },
                    "dry_run": { "type": "boolean", "default": false }
                }
            }
        }),
        json!({
            "name": "lumora.multi_read",
            "description": "Batch read multiple files in one call to reduce round trips.",
            "inputSchema": {
                "type": "object",
                "required": ["reads"],
                "properties": {
                    "reads": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "required": ["path"],
                            "properties": {
                                "path": { "type": "string" },
                                "start_line": { "type": "integer" },
                                "end_line": { "type": "integer" }
                            }
                        }
                    },
                    "max_total_lines": { "type": "integer", "default": 2000 }
                }
            }
        }),
        json!({
            "name": "lumora.move_file",
            "description": "Move or rename a file within the repository.",
            "inputSchema": {
                "type": "object",
                "required": ["source", "destination"],
                "properties": {
                    "source": { "type": "string" },
                    "destination": { "type": "string" }
                }
            }
        }),
        json!({
            "name": "lumora.delete_file",
            "description": "Delete a file from the repository.",
            "inputSchema": {
                "type": "object",
                "required": ["path"],
                "properties": {
                    "path": { "type": "string" }
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
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

#[derive(Debug)]
enum ToolCallError {
    InvalidParams(String),
    Runtime(String),
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::io::Cursor;
    use tempfile::TempDir;

    fn test_paths() -> (RuntimePaths, TempDir) {
        let dir = TempDir::new().unwrap();
        let repo_root = dir.path().to_path_buf();
        let state_dir = dir.path().join(".lumora");
        std::fs::create_dir_all(&state_dir).unwrap();
        let db_path = state_dir.join("graph.db");
        let paths = RuntimePaths {
            repo_root,
            state_dir,
            db_path,
        };
        (paths, dir)
    }

    // ── Parameter helpers ───────────────────────────────────────────

    #[test]
    fn test_required_str_present() {
        let args = json!({"name": "foo"});
        let result = required_str(&args, "name");
        assert!(result.is_ok(), "should succeed for present string");
        assert_eq!(result.unwrap(), "foo", "should return the string value");
    }

    #[test]
    fn test_required_str_missing() {
        let args = json!({});
        assert!(
            required_str(&args, "name").is_err(),
            "should fail for missing key"
        );
    }

    #[test]
    fn test_required_str_wrong_type() {
        let args = json!({"name": 42});
        assert!(
            required_str(&args, "name").is_err(),
            "should fail for non-string value"
        );
    }

    #[test]
    fn test_opt_bool_present() {
        let args = json!({"x": true});
        let result = opt_bool(&args, "x");
        assert!(result.is_ok(), "should succeed");
        assert_eq!(result.unwrap(), Some(true), "should return Some(true)");
    }

    #[test]
    fn test_opt_bool_missing() {
        let args = json!({});
        let result = opt_bool(&args, "x");
        assert!(result.is_ok(), "should succeed for missing key");
        assert_eq!(result.unwrap(), None, "should return None");
    }

    #[test]
    fn test_opt_bool_wrong_type() {
        let args = json!({"x": "yes"});
        assert!(opt_bool(&args, "x").is_err(), "should fail for non-bool");
    }

    #[test]
    fn test_opt_u64_present() {
        let args = json!({"x": 42});
        let result = opt_u64(&args, "x");
        assert!(result.is_ok(), "should succeed");
        assert_eq!(result.unwrap(), Some(42), "should return Some(42)");
    }

    #[test]
    fn test_opt_u64_missing() {
        let args = json!({});
        let result = opt_u64(&args, "x");
        assert!(result.is_ok(), "should succeed for missing key");
        assert_eq!(result.unwrap(), None, "should return None");
    }

    #[test]
    fn test_opt_i64_present() {
        let args = json!({"x": -5});
        let result = opt_i64(&args, "x");
        assert!(result.is_ok(), "should succeed");
        assert_eq!(result.unwrap(), Some(-5), "should return Some(-5)");
    }

    #[test]
    fn test_opt_i64_null() {
        let args = json!({"x": null});
        let result = opt_i64(&args, "x");
        assert!(result.is_ok(), "null should succeed");
        assert_eq!(result.unwrap(), None, "null should return None");
    }

    #[test]
    fn test_opt_f64_present() {
        let args = json!({"x": 1.5});
        let result = opt_f64(&args, "x");
        assert!(result.is_ok(), "should succeed");
        let val = result.unwrap().expect("should be Some");
        assert!((val - 1.5).abs() < f64::EPSILON, "should be ~1.5");
    }

    #[test]
    fn test_opt_string_present() {
        let args = json!({"x": "hello"});
        let result = opt_string(&args, "x");
        assert!(result.is_ok(), "should succeed");
        assert_eq!(
            result.unwrap(),
            Some("hello".to_string()),
            "should return Some(hello)"
        );
    }

    #[test]
    fn test_opt_string_null() {
        let args = json!({"x": null});
        let result = opt_string(&args, "x");
        assert!(result.is_ok(), "null should succeed");
        assert_eq!(result.unwrap(), None, "null should return None");
    }

    #[test]
    fn test_opt_string_missing() {
        let args = json!({});
        let result = opt_string(&args, "x");
        assert!(result.is_ok(), "missing should succeed");
        assert_eq!(result.unwrap(), None, "missing should return None");
    }

    #[test]
    fn test_opt_order_score_desc() {
        let args = json!({"x": "score_desc"});
        let result = opt_order(&args, "x");
        assert!(result.is_ok(), "should succeed");
        assert_eq!(
            result.unwrap(),
            Some(SortOrder::ScoreDesc),
            "should return ScoreDesc"
        );
    }

    #[test]
    fn test_opt_order_asc_alias() {
        let args = json!({"x": "asc"});
        let result = opt_order(&args, "x");
        assert!(result.is_ok(), "should succeed");
        assert_eq!(
            result.unwrap(),
            Some(SortOrder::LineAsc),
            "asc should map to LineAsc"
        );
    }

    #[test]
    fn test_opt_order_invalid() {
        let args = json!({"x": "invalid_order"});
        assert!(opt_order(&args, "x").is_err(), "invalid order should error");
    }

    #[test]
    fn test_opt_verbosity_compact() {
        let args = json!({"x": "compact"});
        let result = opt_verbosity(&args, "x");
        assert!(result.is_ok(), "should succeed");
        assert_eq!(
            result.unwrap(),
            Some(Verbosity::Compact),
            "should return Compact"
        );
    }

    #[test]
    fn test_opt_verbosity_normal() {
        let args = json!({"x": "normal"});
        let result = opt_verbosity(&args, "x");
        assert!(result.is_ok(), "should succeed");
        assert_eq!(
            result.unwrap(),
            Some(Verbosity::Normal),
            "should return Normal"
        );
    }

    #[test]
    fn test_opt_verbosity_debug() {
        let args = json!({"x": "debug"});
        let result = opt_verbosity(&args, "x");
        assert!(result.is_ok(), "should succeed");
        assert_eq!(
            result.unwrap(),
            Some(Verbosity::Debug),
            "should return Debug"
        );
    }

    #[test]
    fn test_opt_verbosity_invalid() {
        let args = json!({"x": "xxx"});
        assert!(
            opt_verbosity(&args, "x").is_err(),
            "invalid verbosity should error"
        );
    }

    // ── Frame reading/writing ───────────────────────────────────────

    #[test]
    fn test_read_frame_line_delimited() {
        let data = b"{\"jsonrpc\":\"2.0\",\"method\":\"ping\"}\n";
        let mut cursor = Cursor::new(&data[..]);
        let frame = read_frame(&mut cursor)
            .expect("read_frame should succeed")
            .expect("should return Some frame");
        assert_eq!(frame.value["method"], "ping", "method should be ping");
        match frame.style {
            FrameStyle::LineDelimited => {} // expected
            FrameStyle::ContentLength => panic!("expected LineDelimited, got ContentLength"),
        }
    }

    #[test]
    fn test_read_frame_content_length() {
        let json_payload = b"{\"jsonrpc\":\"2.0\",\"method\":\"ping\"}";
        let header = format!("Content-Length: {}\r\n\r\n", json_payload.len());
        let mut data = Vec::new();
        data.extend_from_slice(header.as_bytes());
        data.extend_from_slice(json_payload);
        let mut cursor = Cursor::new(data);
        let frame = read_frame(&mut cursor)
            .expect("read_frame should succeed")
            .expect("should return Some frame");
        assert_eq!(frame.value["method"], "ping", "method should be ping");
        match frame.style {
            FrameStyle::ContentLength => {} // expected
            FrameStyle::LineDelimited => panic!("expected ContentLength, got LineDelimited"),
        }
    }

    #[test]
    fn test_read_frame_eof() {
        let mut cursor = Cursor::new(b"" as &[u8]);
        let result = read_frame(&mut cursor).expect("read_frame should succeed on eof");
        assert!(result.is_none(), "eof should return None");
    }

    #[test]
    fn test_read_frame_skips_blank_lines() {
        let data = b"\n\n{\"method\":\"ping\"}\n";
        let mut cursor = Cursor::new(&data[..]);
        let frame = read_frame(&mut cursor)
            .expect("read_frame should succeed")
            .expect("should skip blank lines and return frame");
        assert_eq!(
            frame.value["method"], "ping",
            "should parse the JSON after blanks"
        );
    }

    #[test]
    fn test_write_frame_line_delimited() {
        let mut buf = Vec::new();
        let payload = json!({"test": true});
        write_frame(&mut buf, &payload, FrameStyle::LineDelimited)
            .expect("write_frame should succeed");
        let output = String::from_utf8(buf).expect("should be valid utf8");
        assert!(
            output.ends_with('\n'),
            "line-delimited should end with newline"
        );
        assert!(
            output.contains("\"test\""),
            "output should contain the JSON"
        );
    }

    #[test]
    fn test_write_frame_content_length() {
        let mut buf = Vec::new();
        let payload = json!({"test": true});
        write_frame(&mut buf, &payload, FrameStyle::ContentLength)
            .expect("write_frame should succeed");
        let output = String::from_utf8(buf).expect("should be valid utf8");
        assert!(
            output.starts_with("Content-Length:"),
            "content-length should start with header"
        );
    }

    // ── Response builders ──────────────────────────────────────────

    #[test]
    fn test_success_response() {
        let resp = success_response(json!(1), json!({"ok": true}));
        assert_eq!(resp["jsonrpc"], "2.0", "jsonrpc should be 2.0");
        assert_eq!(resp["id"], 1, "id should be 1");
        assert_eq!(resp["result"]["ok"], true, "result.ok should be true");
    }

    #[test]
    fn test_error_response() {
        let resp = error_response(Some(json!(2)), -32601, "not found");
        assert_eq!(resp["error"]["code"], -32601, "error code should match");
        assert_eq!(
            resp["error"]["message"], "not found",
            "error message should match"
        );
    }

    #[test]
    fn test_tool_ok() {
        let result = tool_ok(json!({"data": 1}));
        let content = &result["content"];
        assert!(content.is_array(), "content should be array");
        assert_eq!(
            content[0]["type"], "text",
            "first content item type should be text"
        );
        assert!(
            result["structuredContent"].is_object(),
            "structuredContent should be present"
        );
    }

    #[test]
    fn test_tool_error() {
        let result = tool_error("boom".to_string());
        assert_eq!(
            result["content"][0]["text"], "boom",
            "error text should be boom"
        );
        assert_eq!(result["isError"], true, "isError should be true");
    }

    // ── handle_request integration tests ───────────────────────────

    #[test]
    fn test_handle_initialize() {
        let (paths, _dir) = test_paths();
        let params = json!({"protocolVersion": "2025-06-18"});
        let resp = handle_request("initialize", Some(&params), json!(1), &paths)
            .expect("handle_request initialize should succeed");
        assert!(
            resp["result"]["protocolVersion"].is_string(),
            "should have protocolVersion"
        );
        assert!(
            resp["result"]["capabilities"]["tools"].is_object(),
            "should have tools capability"
        );
    }

    #[test]
    fn test_handle_ping() {
        let (paths, _dir) = test_paths();
        let resp = handle_request("ping", None, json!(2), &paths)
            .expect("handle_request ping should succeed");
        assert!(resp["result"].is_object(), "ping result should be object");
    }

    #[test]
    fn test_handle_tools_list() {
        let (paths, _dir) = test_paths();
        let resp = handle_request("tools/list", None, json!(3), &paths)
            .expect("handle_request tools/list should succeed");
        let tools = &resp["result"]["tools"];
        assert!(tools.is_array(), "tools should be an array");
        assert_eq!(tools.as_array().unwrap().len(), 17, "should list 17 tools");
    }

    #[test]
    fn test_handle_unknown_method() {
        let (paths, _dir) = test_paths();
        let resp = handle_request("foo/bar", None, json!(4), &paths)
            .expect("handle_request unknown method should succeed");
        assert_eq!(
            resp["error"]["code"], -32601,
            "unknown method should return -32601"
        );
    }

    #[test]
    fn test_handle_tools_call_missing_params() {
        let (paths, _dir) = test_paths();
        let resp = handle_request("tools/call", None, json!(5), &paths)
            .expect("handle_request should succeed");
        assert!(
            resp["error"].is_object(),
            "missing params should produce error"
        );
    }

    #[test]
    fn test_handle_tools_call_missing_name() {
        let (paths, _dir) = test_paths();
        let params = json!({"arguments": {}});
        let resp = handle_request("tools/call", Some(&params), json!(6), &paths)
            .expect("handle_request should succeed");
        assert!(
            resp["error"].is_object(),
            "missing name should produce error"
        );
    }

    #[test]
    fn test_handle_symbol_definitions_tool() {
        let (paths, _dir) = test_paths();
        // First index the repository to create the DB
        let _index_resp = handle_request(
            "tools/call",
            Some(&json!({"name": "lumora.index_repository", "arguments": {}})),
            json!(10),
            &paths,
        )
        .expect("index should succeed");
        // Then query for a nonexistent symbol
        let resp = handle_request(
            "tools/call",
            Some(
                &json!({"name": "lumora.symbol_definitions", "arguments": {"name": "nonexistent"}}),
            ),
            json!(11),
            &paths,
        )
        .expect("symbol_definitions should succeed");
        assert!(
            resp["result"]["structuredContent"].is_object(),
            "should have structuredContent"
        );
    }

    // ── compact_if_needed and strip_compact_fields ─────────────────

    #[test]
    fn test_compact_if_needed_compact() {
        let value = json!({
            "rows": [{"why": "reason", "meta_json": "data", "name": "x"}],
            "diagnostics": {"detail": true}
        });
        let compacted = compact_if_needed(value, Verbosity::Compact);
        assert!(
            compacted.get("diagnostics").is_none(),
            "diagnostics should be removed in compact mode"
        );
        let row = &compacted["rows"][0];
        assert!(
            row.get("why").is_none(),
            "why should be stripped in compact mode"
        );
        assert!(
            row.get("meta_json").is_none(),
            "meta_json should be stripped in compact mode"
        );
        assert_eq!(row["name"], "x", "non-compact fields should be preserved");
    }

    #[test]
    fn test_compact_if_needed_normal() {
        let value = json!({
            "rows": [{"why": "reason", "meta_json": "data", "name": "x"}],
            "diagnostics": {"detail": true}
        });
        let result = compact_if_needed(value, Verbosity::Normal);
        assert!(
            result.get("diagnostics").is_some(),
            "diagnostics should be preserved in normal mode"
        );
        assert!(
            result["rows"][0].get("why").is_some(),
            "why should be preserved in normal mode"
        );
    }
}
