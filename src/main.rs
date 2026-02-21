mod daemon;
mod indexer;
mod mcp;
mod model;
mod parser;
mod paths;
mod storage;

use std::path::PathBuf;
use std::process::Command;

use anyhow::{Context, Result};
use clap::{ArgAction, Args, Parser, Subcommand};
use serde::Serialize;
use serde_json::json;

use crate::indexer::{index_repository, IndexOptions};
use crate::mcp::run_mcp_stdio;
use crate::paths::{ensure_state_layout, resolve_runtime_paths, RuntimePaths};
use crate::storage::GraphStore;

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
    #[arg(long, default_value = ".")]
    repo: PathBuf,
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
    #[arg(long, default_value = ".")]
    repo: PathBuf,
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
    #[arg(long, default_value = ".")]
    repo: PathBuf,
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
    #[arg(long, default_value = ".")]
    repo: PathBuf,
    #[arg(long)]
    state_dir: Option<PathBuf>,
    #[arg(long)]
    db: Option<PathBuf>,
    #[arg(long, default_value_t = true, action = ArgAction::Set)]
    auto_index: bool,
    #[arg(long)]
    full_first: bool,
}

#[derive(Debug, Args)]
struct SetupCodexArgs {
    #[arg(long, default_value = ".")]
    repo: PathBuf,
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
    #[arg(long, default_value = ".")]
    repo: PathBuf,
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
    },
    /// Find call sites for a symbol.
    Callers { name: String },
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
    },
    /// Find similar files by token-winnowing fingerprints.
    Clones {
        file: String,
        #[arg(long, default_value_t = 0.2)]
        min_similarity: f64,
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
    let paths = resolve_paths(&args.repo, args.state_dir.as_deref(), args.db.as_deref())?;
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
    let paths = resolve_paths(&args.repo, args.state_dir.as_deref(), args.db.as_deref())?;
    ensure_state_layout(&paths)?;

    daemon::run_watcher_daemon(&paths, args.full_first, args.debounce_ms, args.json)
}

fn run_query(args: QueryArgs) -> Result<()> {
    let paths = resolve_paths(&args.repo, args.state_dir.as_deref(), args.db.as_deref())?;
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
        QueryCommands::Refs { name, calls_only } => {
            let rows = if calls_only {
                store.symbol_references(&name, Some("calls"))?
            } else {
                store.symbol_references(&name, None)?
            };

            if args.json {
                print_json(&rows)?;
            } else if rows.is_empty() {
                println!("No references found for `{name}`");
            } else {
                for row in rows {
                    println!(
                        "{}:{}:{} [{}]",
                        row.file_path, row.line, row.col, row.edge_type
                    );
                }
            }
        }
        QueryCommands::Callers { name } => {
            let rows = store.symbol_references(&name, Some("calls"))?;
            if args.json {
                print_json(&rows)?;
            } else if rows.is_empty() {
                println!("No callers found for `{name}`");
            } else {
                for row in rows {
                    println!("{}:{}:{}", row.file_path, row.line, row.col);
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
        QueryCommands::Slice { file, line, depth } => {
            let result = store.minimal_slice(&file, line, depth.max(1))?;
            if args.json {
                print_json(&result)?;
            } else if let Some(slice) = result {
                println!(
                    "anchor: {} [{}]",
                    slice.anchor.key, slice.anchor.entity_type
                );
                for edge in slice.neighbors {
                    println!(
                        "{} {} -> {} [{}]",
                        edge.direction, edge.edge_type, edge.entity.key, edge.entity.entity_type
                    );
                }
            } else {
                println!("No slice anchor found for file `{file}`");
            }
        }
        QueryCommands::Clones {
            file,
            min_similarity,
        } => {
            let rows = store.clone_matches(&file, min_similarity)?;
            if args.json {
                print_json(&rows)?;
            } else if rows.is_empty() {
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

    Ok(())
}

fn run_mcp(args: McpArgs) -> Result<()> {
    let paths = resolve_paths(&args.repo, args.state_dir.as_deref(), args.db.as_deref())?;
    ensure_state_layout(&paths)?;
    run_mcp_stdio(paths, args.auto_index, args.full_first)
}

fn run_setup_codex(args: SetupCodexArgs) -> Result<()> {
    let paths = resolve_paths(&args.repo, None, None)?;
    let repo_display = paths.repo_root.to_string_lossy().to_string();
    let add_args = vec![
        "mcp".to_string(),
        "add".to_string(),
        args.name.clone(),
        "--".to_string(),
        args.command.clone(),
        "mcp".to_string(),
        "--repo".to_string(),
        repo_display.clone(),
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

    println!(
        "Registered MCP server `{}` for repo {}",
        args.name, repo_display
    );
    println!("Run `codex mcp get {}` to verify.", args.name);
    Ok(())
}

fn run_print_mcp_config(args: PrintMcpConfigArgs) -> Result<()> {
    let paths = resolve_paths(&args.repo, None, None)?;
    let snippet = json!({
        "mcpServers": {
            args.name: {
                "command": args.command,
                "args": ["mcp", "--repo", paths.repo_root.to_string_lossy().to_string()],
            }
        }
    });

    println!("{}", serde_json::to_string_pretty(&snippet)?);
    Ok(())
}

fn resolve_paths(
    repo: &std::path::Path,
    state_dir: Option<&std::path::Path>,
    db: Option<&std::path::Path>,
) -> Result<RuntimePaths> {
    resolve_runtime_paths(repo, state_dir, db)
}

fn print_json<T: Serialize>(value: &T) -> Result<()> {
    println!("{}", serde_json::to_string_pretty(value)?);
    Ok(())
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
