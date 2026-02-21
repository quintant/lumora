# Lumora: Local Semantic Code Graph + MCP Server for AI Coding Agents

Lumora is a local-first semantic code search and code graph engine built for AI-assisted software development.
It runs as a CLI and an MCP server, so coding agents can fetch precise repository context instead of reading large file chunks blindly.

If you use tools like Codex, Claude Code, Cline, Cursor, Roo, or custom MCP clients, Lumora improves context quality, reduces noisy retrieval, and helps agents make safer edits.

## Why Lumora

- Semantic over text-only search: definitions, references, callers, dependency paths, and minimal graph slices.
- Better context for AI agents: bounded results with ranking, deduplication, paging, diagnostics, and freshness metadata.
- Local and offline: SQLite-backed index under `.lumora/` in your repo.
- Fast incremental indexing: re-index only what changed.
- MCP-native: easy to plug into Codex and other MCP-compatible coding tools.

## Where It Helps (Compared to Standard Tools)

### 1) "Where is this actually used?"
Text search:

```bash
rg "index_repository" -n
```

Semantic query:

```bash
lumora query --repo . refs index_repository --order score_desc --limit 50 --dedup true --top-files
```

Why it helps: ranked and deduped call/reference results, plus file-level summary for faster triage.

### 2) "What calls this function?"
Text search:

```bash
rg "run_mcp_stdio\\(" -n
```

Semantic query:

```bash
lumora query --repo . callers run_mcp_stdio --order score_desc --limit 25
```

Why it helps: direct call edges, optional filters, and paging metadata.

### 3) "How does A depend on B?"
Manual trace:

- inspect imports
- follow call chains by hand

Semantic dependency path:

```bash
lumora query --repo . deps src/main.rs src/storage.rs --max-depth 10
```

Why it helps: explicit graph path instead of manual reconstruction.

### 4) "Give the agent only relevant context for this line"
Manual approach:

- copy large file snippets

Graph slice:

```bash
lumora query --repo . slice src/main.rs --line 351 --depth 2 --max-neighbors 40 --dedup true --suppress-low-signal-repeats true --prefer-project-symbols true
```

Why it helps: small, high-signal context windows for agent prompts.

## Install

```bash
cargo install lumora
```

## Quickstart

```bash
lumora index --repo .
lumora query --repo . symbol main
lumora serve --repo . --full-first
```

## MCP Setup

### Codex (recommended)

```bash
lumora setup-codex --repo . --replace
codex mcp get lumora
codex mcp list
```

This registers Lumora with `--auto-index false` for fast MCP startup.

If `codex` is not on PATH:

```bash
lumora setup-codex --repo . --replace --codex-command /path/to/codex
```

### Manual Codex registration

```bash
# macOS/Linux
codex mcp remove lumora
codex mcp add lumora -- lumora mcp --auto-index false

# Windows (absolute binary path)
codex mcp remove lumora
codex mcp add lumora -- C:\Users\<you>\.cargo\bin\lumora.exe mcp --auto-index false
```

Then launch Codex from the target repo directory.

### Other MCP clients

```bash
lumora print-mcp-config --repo .
```

## MCP Tools

- `lumora.index_repository`
- `lumora.symbol_definitions`
- `lumora.symbol_references`
- `lumora.symbol_callers`
- `lumora.dependency_path`
- `lumora.minimal_slice`
- `lumora.clone_matches`
- `lumora.selector_discover`

## Query Capabilities for AI-Agent Context

High-volume endpoints support:

- `limit`, `offset`, `order` (`score_desc`, `line_asc`, plus `asc`/`desc` compatibility aliases)
- dedup and summary modes
- `file_glob`, `language`, and `max_age_hours` filters (references/callers)
- verbosity levels (`compact`, `normal`, `debug`)
- pagination metadata (`total`, `has_more`, `next_offset`)
- optional freshness metadata (`include_freshness: true`)

`minimal_slice` defaults prioritize signal:

- `max_neighbors=40`
- `dedup=true`
- `suppress_low_signal_repeats=true`
- `low_signal_name_cap=1`
- `prefer_project_symbols=true`

`clone_matches` supports:

- `mode=matches` and `mode=hotspots`
- actionable `analysis` fields (candidate counts, threshold fallout, suggested threshold)
- pagination metadata

`selector_discover` supports:

- fuzzy discovery (`fuzzy=true` by default)
- scope hints: `file_glob`, `entity_type`
- ranked results with score/explanation metadata

## Dependency Path Semantics

- File-to-file selectors (`file:src/a.rs` -> `file:src/b.rs`) are best for module/file impact analysis.
- Symbol-based selectors can be less intuitive when names are overloaded.
- For predictable paths, prefer explicit selectors:
  - `file:<path>`
  - `symbol_name:<lang>:<name>`
  - `symbol:<name>`

## Current Scope

- Language parsing: Rust (`.rs`) and Python (`.py`)
- Config/entrypoint signals include: `Cargo.toml`, `pyproject.toml`, `setup.cfg`, `package.json`
- Storage: SQLite

## State Layout

By default Lumora stores generated state under:

- `.lumora/graph.db`
- future state/index files under `.lumora/`

## Common Commands

```bash
# Index
lumora index --repo .
lumora index --repo . --full --json

# Watcher daemon
lumora serve --repo . --full-first

# Query examples
lumora query --repo . symbol index_repository
lumora query --repo . refs ensure_entity_with_tx --calls-only --order score_desc --limit 50 --dedup true --top-files
lumora query --repo . callers index_repository --file-glob "src/*.rs" --order line_asc --limit 25
lumora query --repo . deps src/main.rs src/storage.rs --max-depth 10
lumora query --repo . slice src/main.rs --line 351 --depth 2 --max-neighbors 40 --dedup true --suppress-low-signal-repeats true --prefer-project-symbols true
lumora query --repo . clones src/main.rs --limit 20 --offset 0 --hotspots

# MCP server mode
lumora mcp --repo .
```

For CLI dependency queries, plain file selectors like `src/main.rs` are often easiest in shells.

## Troubleshooting MCP Startup

```bash
cargo install lumora --force
codex mcp remove lumora
codex mcp add lumora -- lumora mcp --auto-index false
```

If needed, increase startup timeout in `~/.codex/config.toml`:

```toml
[mcp_servers.lumora]
startup_timeout_sec = 30
```

## Platform Support

Lumora targets Linux, macOS, and Windows.

Prerequisite on all platforms: a working C toolchain (required by `tree-sitter` and bundled `sqlite`).

- Linux: `build-essential` (or GCC/Clang equivalent)
- macOS: Xcode Command Line Tools (`xcode-select --install`)
- Windows: Visual Studio C++ Build Tools

Cross-platform CI runs on Ubuntu, macOS, and Windows via `.github/workflows/ci.yml`.

## Maintainer Release Workflow

```bash
cargo fmt --check
cargo check --all-targets
cargo test --all-targets
cargo package
cargo publish --dry-run
```

Then tag and publish.

## Documentation and Project Links

- Changelog: `CHANGELOG.md`
- Contributing: `CONTRIBUTING.md`
- Security policy: `SECURITY.md`

## License

Dual licensed under either:

- MIT license (`LICENSE-MIT`)
- Apache License 2.0 (`LICENSE-APACHE`)

at your option.

