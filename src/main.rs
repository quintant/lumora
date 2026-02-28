mod daemon;
mod fileops;
mod indexer;
mod languages;
mod mcp;
mod model;
mod parser;
mod paths;
mod storage;

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result};
use clap::{ArgAction, Args, Parser, Subcommand};
use serde::Serialize;
use serde_json::json;

use crate::indexer::{index_repository, IndexOptions};
use crate::mcp::run_mcp_stdio;
use crate::paths::{ensure_state_layout, resolve_runtime_paths, RuntimePaths};
use crate::storage::{
    CloneQueryOptions, GraphStore, ReferenceQueryOptions, SliceQueryOptions, SortOrder,
};

#[derive(Debug, Parser)]
#[command(name = "lumora")]
#[command(about = "Local semantic code graph + query engine", version)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Debug, Subcommand)]
enum Commands {
    /// Index a repository incrementally into a local sqlite graph.
    Index(IndexArgs),
    /// Run event-driven watcher daemon for continuous refresh.
    Serve(ServeArgs),
    /// Query the graph.
    Query(QueryArgs),
    /// Run as an MCP stdio server for agent/tool integration.
    Mcp(McpArgs),
    /// Register Lumora as a Codex MCP server from this machine.
    SetupCodex(SetupCodexArgs),
    /// Print generic MCP client config JSON snippet.
    PrintMcpConfig(PrintMcpConfigArgs),
}

#[derive(Debug, Args)]
struct IndexArgs {
    #[arg(long)]
    repo: Option<PathBuf>,
    #[arg(long)]
    state_dir: Option<PathBuf>,
    #[arg(long)]
    db: Option<PathBuf>,
    #[arg(long)]
    full: bool,
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Args)]
struct ServeArgs {
    #[arg(long)]
    repo: Option<PathBuf>,
    #[arg(long)]
    state_dir: Option<PathBuf>,
    #[arg(long)]
    db: Option<PathBuf>,
    #[arg(long)]
    full_first: bool,
    #[arg(long, default_value_t = 300)]
    debounce_ms: u64,
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Args)]
struct QueryArgs {
    #[arg(long)]
    repo: Option<PathBuf>,
    #[arg(long)]
    state_dir: Option<PathBuf>,
    #[arg(long)]
    db: Option<PathBuf>,
    #[arg(long)]
    json: bool,
    #[command(subcommand)]
    command: QueryCommands,
}

#[derive(Debug, Args)]
struct McpArgs {
    #[arg(long)]
    repo: Option<String>,
    #[arg(hide = true)]
    repo_tail: Vec<String>,
    #[arg(long)]
    state_dir: Option<PathBuf>,
    #[arg(long)]
    db: Option<PathBuf>,
    #[arg(long, default_value_t = false, action = ArgAction::Set)]
    auto_index: bool,
    #[arg(long)]
    full_first: bool,
}

#[derive(Debug, Args)]
struct SetupCodexArgs {
    #[arg(long)]
    repo: Option<PathBuf>,
    #[arg(long, default_value = "lumora")]
    name: String,
    #[arg(long, default_value = "lumora")]
    command: String,
    #[arg(long, default_value = "codex")]
    codex_command: String,
    #[arg(long)]
    replace: bool,
    #[arg(long)]
    dry_run: bool,
}

#[derive(Debug, Args)]
struct PrintMcpConfigArgs {
    #[arg(long)]
    repo: Option<PathBuf>,
    #[arg(long, default_value = "lumora")]
    name: String,
    #[arg(long, default_value = "lumora")]
    command: String,
}

#[derive(Debug, Subcommand)]
enum QueryCommands {
    /// Find where a symbol is defined.
    Symbol { name: String },
    /// Find where a symbol is referenced.
    Refs {
        name: String,
        #[arg(long)]
        calls_only: bool,
        #[arg(long, default_value_t = 200)]
        limit: usize,
        #[arg(long, default_value_t = 0)]
        offset: usize,
        #[arg(long, default_value_t = true, action = ArgAction::Set)]
        dedup: bool,
        #[arg(long, default_value = "score_desc")]
        order: String,
        #[arg(long)]
        file_glob: Option<String>,
        #[arg(long)]
        language: Option<String>,
        #[arg(long)]
        max_age_hours: Option<u64>,
        #[arg(long)]
        top_files: bool,
    },
    /// Find call sites for a symbol.
    Callers {
        name: String,
        #[arg(long, default_value_t = 200)]
        limit: usize,
        #[arg(long, default_value_t = 0)]
        offset: usize,
        #[arg(long, default_value_t = true, action = ArgAction::Set)]
        dedup: bool,
        #[arg(long, default_value = "score_desc")]
        order: String,
        #[arg(long)]
        file_glob: Option<String>,
        #[arg(long)]
        language: Option<String>,
        #[arg(long)]
        max_age_hours: Option<u64>,
        #[arg(long)]
        top_files: bool,
    },
    /// Find dependency path A -> B using graph edges.
    Deps {
        from: String,
        to: String,
        #[arg(long, default_value_t = 8)]
        max_depth: usize,
    },
    /// Return a minimal context slice around file/line.
    Slice {
        file: String,
        #[arg(long)]
        line: Option<i64>,
        #[arg(long, default_value_t = 2)]
        depth: usize,
        #[arg(long, default_value_t = 40)]
        max_neighbors: usize,
        #[arg(long, default_value_t = true, action = ArgAction::Set)]
        dedup: bool,
        #[arg(long, default_value_t = true, action = ArgAction::Set)]
        suppress_low_signal_repeats: bool,
        #[arg(long, default_value_t = 1)]
        low_signal_name_cap: usize,
        #[arg(long, default_value_t = true, action = ArgAction::Set)]
        prefer_project_symbols: bool,
    },
    /// Find similar files by token-winnowing fingerprints.
    Clones {
        file: String,
        #[arg(long, default_value_t = 0.02)]
        min_similarity: f64,
        #[arg(long, default_value_t = 50)]
        limit: usize,
        #[arg(long, default_value_t = 0)]
        offset: usize,
        #[arg(long)]
        hotspots: bool,
    },
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Commands::Index(args) => run_index(args),
        Commands::Serve(args) => run_serve(args),
        Commands::Query(args) => run_query(args),
        Commands::Mcp(args) => run_mcp(args),
        Commands::SetupCodex(args) => run_setup_codex(args),
        Commands::PrintMcpConfig(args) => run_print_mcp_config(args),
    }
}

fn run_index(args: IndexArgs) -> Result<()> {
    let paths = resolve_paths(
        args.repo.as_deref(),
        args.state_dir.as_deref(),
        args.db.as_deref(),
    )?;
    ensure_state_layout(&paths)?;

    let mut store = GraphStore::open(&paths.db_path)?;
    let report = index_repository(
        &mut store,
        &paths.repo_root,
        IndexOptions { full: args.full },
    )?;

    if args.json {
        print_json(&report)?;
    } else {
        println!("repo: {}", paths.repo_root.display());
        println!("state: {}", paths.state_dir.display());
        println!("db: {}", paths.db_path.display());
        println!("indexed: {}", report.indexed_files);
        println!("skipped: {}", report.skipped_files);
        println!("removed: {}", report.removed_files);
        println!("parse_failures: {}", report.parse_failures);
        if !report.errors.is_empty() {
            println!("errors:");
            for error in report.errors {
                println!("  - {error}");
            }
        }
    }

    Ok(())
}

fn run_serve(args: ServeArgs) -> Result<()> {
    let paths = resolve_paths(
        args.repo.as_deref(),
        args.state_dir.as_deref(),
        args.db.as_deref(),
    )?;
    ensure_state_layout(&paths)?;

    daemon::run_watcher_daemon(&paths, args.full_first, args.debounce_ms, args.json)
}

fn run_query(args: QueryArgs) -> Result<()> {
    let paths = resolve_paths(
        args.repo.as_deref(),
        args.state_dir.as_deref(),
        args.db.as_deref(),
    )?;
    ensure_state_layout(&paths)?;

    let store = GraphStore::open(&paths.db_path)?;

    match args.command {
        QueryCommands::Symbol { name } => {
            let rows = store.symbol_definitions(&name)?;
            if args.json {
                print_json(&rows)?;
            } else if rows.is_empty() {
                println!("No definitions found for `{name}`");
            } else {
                for row in rows {
                    println!(
                        "{}:{}:{} [{}] {}",
                        row.file_path, row.line, row.col, row.kind, row.qualname
                    );
                }
            }
        }
        QueryCommands::Refs {
            name,
            calls_only,
            limit,
            offset,
            dedup,
            order,
            file_glob,
            language,
            max_age_hours,
            top_files,
        } => {
            let edge_type_filter = if calls_only {
                Some("calls".to_string())
            } else {
                None
            };
            let options = ReferenceQueryOptions {
                edge_type_filter,
                file_glob,
                language,
                max_age_hours,
                limit: limit.max(1),
                offset,
                dedup,
                order: parse_sort_order(&order)?,
            };
            let (rows, pagination) = store.symbol_references_page(&name, &options)?;

            if args.json {
                print_json(&json!({
                    "rows": rows,
                    "pagination": pagination
                }))?;
            } else if rows.is_empty() {
                println!("No references found for `{name}`");
            } else {
                for row in &rows {
                    if let Some(score) = row.score {
                        println!(
                            "{}:{}:{} [{}] score={:.2}",
                            row.file_path, row.line, row.col, row.edge_type, score
                        );
                    } else {
                        println!(
                            "{}:{}:{} [{}]",
                            row.file_path, row.line, row.col, row.edge_type
                        );
                    }
                }
                if top_files {
                    let summary = store.top_reference_files(&rows, 10);
                    println!("top files:");
                    for item in summary {
                        println!("  {} ({})", item.file_path, item.count);
                    }
                }
            }
        }
        QueryCommands::Callers {
            name,
            limit,
            offset,
            dedup,
            order,
            file_glob,
            language,
            max_age_hours,
            top_files,
        } => {
            let options = ReferenceQueryOptions {
                edge_type_filter: Some("calls".to_string()),
                file_glob,
                language,
                max_age_hours,
                limit: limit.max(1),
                offset,
                dedup,
                order: parse_sort_order(&order)?,
            };
            let (rows, pagination) = store.symbol_references_page(&name, &options)?;
            if args.json {
                print_json(&json!({
                    "rows": rows,
                    "pagination": pagination
                }))?;
            } else if rows.is_empty() {
                println!("No callers found for `{name}`");
            } else {
                for row in &rows {
                    if let Some(score) = row.score {
                        println!(
                            "{}:{}:{} score={:.2}",
                            row.file_path, row.line, row.col, score
                        );
                    } else {
                        println!("{}:{}:{}", row.file_path, row.line, row.col);
                    }
                }
                if top_files {
                    let summary = store.top_reference_files(&rows, 10);
                    println!("top caller files:");
                    for item in summary {
                        println!("  {} ({})", item.file_path, item.count);
                    }
                }
            }
        }
        QueryCommands::Deps {
            from,
            to,
            max_depth,
        } => {
            let path = store.dependency_path(&from, &to, max_depth.max(1))?;
            if args.json {
                print_json(&path)?;
            } else if !path.found {
                println!("No path found from `{from}` to `{to}`");
            } else {
                for (idx, hop) in path.hops.iter().enumerate() {
                    println!("{}. {} [{}]", idx + 1, hop.entity_key, hop.entity_type);
                }
            }
        }
        QueryCommands::Slice {
            file,
            line,
            depth,
            max_neighbors,
            dedup,
            suppress_low_signal_repeats,
            low_signal_name_cap,
            prefer_project_symbols,
        } => {
            let result = store.minimal_slice_with_options(
                &file,
                line,
                depth.max(1),
                &SliceQueryOptions {
                    max_neighbors,
                    dedup,
                    suppress_low_signal_repeats,
                    low_signal_name_cap,
                    prefer_project_symbols,
                },
            )?;
            if args.json {
                print_json(&result)?;
            } else if let Some(slice) = result {
                println!(
                    "anchor: {} [{}]",
                    slice.anchor.key, slice.anchor.entity_type
                );
                for edge in slice.neighbors {
                    println!(
                        "{} {} -> {} [{}] score={:.2}",
                        edge.direction,
                        edge.edge_type,
                        edge.entity.key,
                        edge.entity.entity_type,
                        edge.score.unwrap_or_default()
                    );
                }
            } else {
                println!("No slice anchor found for file `{file}`");
            }
        }
        QueryCommands::Clones {
            file,
            min_similarity,
            limit,
            offset,
            hotspots,
        } => {
            let options = CloneQueryOptions {
                min_similarity,
                limit,
                offset,
            };
            if args.json {
                if hotspots {
                    let (rows, pagination, analysis) =
                        store.clone_hotspots_page(&file, &options)?;
                    print_json(&json!({
                        "rows": rows,
                        "pagination": pagination,
                        "analysis": analysis,
                        "mode": "hotspots"
                    }))?;
                } else {
                    let (rows, pagination, analysis) = store.clone_matches_page(&file, &options)?;
                    print_json(&json!({
                        "rows": rows,
                        "pagination": pagination,
                        "analysis": analysis,
                        "mode": "matches"
                    }))?;
                }
            } else if hotspots {
                let rows = store.clone_hotspots(&file, &options)?;
                if rows.is_empty() {
                    println!("No clone hotspots found for `{file}`");
                } else {
                    for row in rows {
                        println!(
                            "{} files={} avg_similarity={:.3} max_similarity={:.3}",
                            row.directory, row.files, row.avg_similarity, row.max_similarity
                        );
                    }
                }
            } else {
                let rows = store.clone_matches_with_options(&file, &options)?;
                if rows.is_empty() {
                    println!("No clone candidates found for `{file}`");
                } else {
                    for row in rows {
                        println!(
                            "{} similarity={:.3} shared={}",
                            row.other_file, row.similarity, row.shared_fingerprints
                        );
                    }
                }
            }
        }
    }

    Ok(())
}

fn run_mcp(args: McpArgs) -> Result<()> {
    let repo_str = match (&args.repo, args.repo_tail.is_empty()) {
        (Some(r), true) => Some(r.clone()),
        (Some(r), false) => Some(format!("{} {}", r, args.repo_tail.join(" "))),
        (None, _) => None,
    };
    let repo_path = repo_str.map(PathBuf::from);
    let paths = resolve_paths(
        repo_path.as_deref(),
        args.state_dir.as_deref(),
        args.db.as_deref(),
    )?;

    // Keep MCP handshake fast/robust: avoid early write requirements unless indexing on startup.
    if args.auto_index {
        ensure_state_layout(&paths)?;
    }
    run_mcp_stdio(paths, args.auto_index, args.full_first)
}

fn run_setup_codex(args: SetupCodexArgs) -> Result<()> {
    let add_args = vec![
        "mcp".to_string(),
        "add".to_string(),
        args.name.clone(),
        "--".to_string(),
        args.command.clone(),
        "mcp".to_string(),
        "--auto-index".to_string(),
        "false".to_string(),
    ];

    if args.dry_run {
        println!("{} {}", args.codex_command, add_args.join(" "));
        return Ok(());
    }

    if args.replace {
        let remove_args = vec!["mcp".to_string(), "remove".to_string(), args.name.clone()];
        let _ = run_codex_cli(&args.codex_command, &remove_args);
    }

    let status = run_codex_cli(&args.codex_command, &add_args).with_context(|| {
        format!(
            "failed to launch `{}`; ensure Codex CLI is installed and on PATH",
            args.codex_command
        )
    })?;

    if !status.success() {
        return Err(anyhow::anyhow!(
            "codex mcp registration failed. Try running this manually: {} {}",
            args.codex_command,
            add_args.join(" ")
        ));
    }

    println!("Registered MCP server `{}`", args.name);
    println!("Run `codex mcp get {}` to verify.", args.name);
    Ok(())
}

fn run_print_mcp_config(args: PrintMcpConfigArgs) -> Result<()> {
    let snippet = json!({
        "mcpServers": {
            args.name: {
                "command": args.command,
                "args": ["mcp"],
            }
        }
    });

    println!("{}", serde_json::to_string_pretty(&snippet)?);
    Ok(())
}

fn resolve_paths(
    repo: Option<&std::path::Path>,
    state_dir: Option<&std::path::Path>,
    db: Option<&std::path::Path>,
) -> Result<RuntimePaths> {
    let repo_hint = repo.unwrap_or_else(|| Path::new("."));
    resolve_runtime_paths(repo_hint, state_dir, db)
}

fn print_json<T: Serialize>(value: &T) -> Result<()> {
    println!("{}", serde_json::to_string_pretty(value)?);
    Ok(())
}

fn parse_sort_order(raw: &str) -> Result<SortOrder> {
    match raw {
        "asc" | "line_asc" => Ok(SortOrder::LineAsc),
        "desc" | "line_desc" => Ok(SortOrder::LineDesc),
        "score_desc" => Ok(SortOrder::ScoreDesc),
        other => Err(anyhow::anyhow!(
            "invalid --order `{other}`; expected one of: asc, desc, score_desc, line_asc, line_desc"
        )),
    }
}

fn run_codex_cli(codex_command: &str, args: &[String]) -> Result<std::process::ExitStatus> {
    #[cfg(windows)]
    {
        if codex_command.to_ascii_lowercase().ends_with(".ps1") {
            return Command::new("powershell")
                .args([
                    "-NoProfile",
                    "-ExecutionPolicy",
                    "Bypass",
                    "-File",
                    codex_command,
                ])
                .args(args)
                .status()
                .map_err(Into::into);
        }
    }

    match Command::new(codex_command).args(args).status() {
        Ok(status) => Ok(status),
        Err(primary_err) => {
            #[cfg(windows)]
            {
                if codex_command.eq_ignore_ascii_case("codex") {
                    return Command::new("codex.cmd")
                        .args(args)
                        .status()
                        .map_err(Into::into);
                }
            }
            Err(primary_err.into())
        }
    }
}
