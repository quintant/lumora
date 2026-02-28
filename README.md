# Lumora

**One MCP server. Full codebase intelligence. Complete file operations.**

Lumora gives AI coding agents everything they need to understand and modify a codebase — semantic code search, dependency graphs, symbol lookup, file reading, editing, and creation — through a single MCP connection.

No cloud. No API keys. Just `cargo install lumora` and go.

## The Problem

AI coding agents are powerful, but they're flying blind. They `cat` entire files, `grep` for strings, and hope for the best. The result: bloated context windows, missed connections, and wasted tokens.

**Without Lumora**, your agent:
- Reads entire files when it only needs 10 lines
- Can't tell you what calls a function without scanning every file
- Has no concept of dependency paths between modules
- Burns tokens on irrelevant code just to find what it needs

**With Lumora**, your agent:
- Reads exactly the lines it needs with automatic truncation
- Finds all callers, references, and definitions through a semantic graph
- Traces dependency paths between any two files or symbols
- Gets minimal, high-signal context slices — not entire files

## Install

```bash
cargo install lumora
```

Requires a C toolchain for tree-sitter and bundled SQLite:
- **Linux**: `sudo apt install build-essential` (or equivalent)
- **macOS**: `xcode-select --install`
- **Windows**: Visual Studio C++ Build Tools

## Quick Start

Lumora auto-detects your repository root from the current directory. No `--repo` flag needed.

```bash
# Index your codebase (run from anywhere inside the repo)
lumora index

# Search the graph
lumora query symbol main
lumora query refs index_repository --order score_desc --limit 50

# Start the MCP server
lumora mcp
```

### MCP Setup

Lumora works with any MCP-compatible coding tool. Pick your client below.

<details>
<summary><strong>Claude Code</strong></summary>

```bash
claude mcp add lumora lumora mcp
```
</details>

<details>
<summary><strong>OpenCode</strong></summary>

Add to `opencode.json`:

```json
{
  "mcp": {
    "lumora": {
      "type": "local",
      "command": ["lumora", "mcp"],
      "enabled": true
    }
  }
}
```
</details>

<details>
<summary><strong>Cursor</strong></summary>

Add to `.cursor/mcp.json`:

```json
{
  "mcpServers": {
    "lumora": {
      "command": "lumora",
      "args": ["mcp"]
    }
  }
}
```
</details>

<details>
<summary><strong>VS Code / GitHub Copilot</strong></summary>

Add to `.vscode/mcp.json`:

```json
{
  "servers": {
    "lumora": {
      "type": "stdio",
      "command": "lumora",
      "args": ["mcp"]
    }
  }
}
```
</details>

<details>
<summary><strong>Codex</strong></summary>

```bash
lumora setup-codex --replace
```

Or manually in `~/.codex/config.toml`:

```toml
[mcp_servers.lumora]
command = "lumora"
args = ["mcp"]
```
</details>

<details>
<summary><strong>Windsurf</strong></summary>

Add to `~/.codeium/windsurf/mcp_config.json`:

```json
{
  "mcpServers": {
    "lumora": {
      "command": "lumora",
      "args": ["mcp"]
    }
  }
}
```
</details>

<details>
<summary><strong>Other MCP clients</strong></summary>

Any client that supports stdio MCP servers works. The server command is:

```
lumora mcp
```

Generate a config snippet: `lumora print-mcp-config`
</details>

## What It Does

### 17 MCP Tools in One Server

Lumora replaces a patchwork of file-reading and search tools with a single, purpose-built MCP server. Every tool is designed to minimize token usage and maximize signal.

#### Semantic Code Graph (8 tools)

| Tool | What it does |
|------|-------------|
| `lumora.index_repository` | Incremental or full re-index of the codebase |
| `lumora.symbol_definitions` | Jump to where a symbol is defined |
| `lumora.symbol_references` | Find every reference to a symbol, ranked and deduped |
| `lumora.symbol_callers` | Find all call sites of a function |
| `lumora.dependency_path` | Trace how module A depends on module B |
| `lumora.minimal_slice` | Extract a minimal context graph around a specific line |
| `lumora.clone_matches` | Detect duplicate or similar code blocks |
| `lumora.selector_discover` | Fuzzy-find symbols and files by partial name |

#### File Operations (9 tools)

All file operations are **sandboxed to the repository root** — no path traversal allowed.

| Tool | What it does |
|------|-------------|
| `lumora.read_file` | Read with optional line range; default cap of 500 lines |
| `lumora.file_outline` | AST-derived structure (definitions only, zero source content) |
| `lumora.search_files` | Regex or literal search with context lines and glob filtering |
| `lumora.list_directory` | Directory listing with metadata, recursive option, glob filtering |
| `lumora.write_file` | Create or overwrite files, with optional parent directory creation |
| `lumora.edit_file` | Exact search-and-replace (must match once); supports dry run |
| `lumora.multi_read` | Batch-read multiple files in one call with a shared line budget |
| `lumora.move_file` | Move or rename a file within the repo |
| `lumora.delete_file` | Delete a file |

### Why Not Just Use Existing Tools?

**vs. `cat`/`head`/`tail`**: Lumora's `read_file` auto-caps output, supports line ranges, and reports total line count so the agent knows what it's missing. `multi_read` batches multiple reads into one round trip with a shared token budget.

**vs. `grep`/`ripgrep`**: Lumora's `search_files` is fine for text search, but `symbol_references` and `symbol_callers` understand *semantic* relationships — not just string matches. "Where is `Config` referenced?" finds actual usage, not comments and strings.

**vs. reading whole files for structure**: `file_outline` returns AST-parsed definitions (functions, classes, structs) with line numbers — no source code. An agent can scan a 2,000-line file's structure in a few dozen tokens.

**vs. multiple MCP servers**: One server, one connection, one index. No juggling a file-system MCP, a search MCP, and a code-intelligence MCP separately.

## Token Efficiency

Lumora is built to keep context windows small:

- **Bounded reads**: `read_file` defaults to 500 lines max. `multi_read` shares a 2,000-line budget across files.
- **Structure without content**: `file_outline` gives you the shape of a file in a fraction of the tokens.
- **Ranked results**: References and callers are scored and deduped — top results first, no noise.
- **Pagination**: Every list endpoint supports `limit`, `offset`, and returns `has_more` metadata.
- **Compact mode**: Set `verbosity: "compact"` to strip optional metadata from responses.
- **Smart defaults**: `minimal_slice` ships with aggressive dedup, low-signal suppression, and project-symbol preference out of the box.

## Advanced Query Features

The semantic graph tools support rich filtering and ranking:

- **Ordering**: `score_desc`, `line_asc`, `line_desc`
- **Filtering**: `file_glob`, `language`, `max_age_hours`
- **Deduplication**: Collapse repeated references to the same location
- **Pagination**: `limit`, `offset` with `total`/`has_more`/`next_offset` metadata
- **Freshness**: Optional `include_freshness: true` for index staleness info
- **Verbosity**: `compact`, `normal`, `debug`

### Dependency Paths

Trace how one file or symbol depends on another:

```bash
lumora query deps src/main.rs src/storage.rs --max-depth 10
```

For best results, use explicit selectors: `file:src/a.rs`, `symbol:my_function`, or `symbol_name:rust:Config`.

### Clone Detection

Find duplicate code across your codebase:

```bash
lumora query clones src/main.rs --limit 20 --hotspots
```

Returns similarity scores, shared fingerprint counts, and hotspot directories — useful for refactoring decisions.

## How It Works

1. **Index**: Lumora parses 23 languages with tree-sitter (`.rs`, `.py`, `.js/.jsx/.mjs/.cjs`, `.ts/.mts/.cts`, `.tsx`, `.go`, `.java`, `.c/.h`, `.cpp/.cc/.cxx/.hpp/.hxx/.hh`, `.cs`, `.rb`, `.sh/.bash/.zsh`, `.css`, `.html/.htm`, `.json`, `.toml`, `.yml/.yaml`, `.scala/.sc`, `.kt/.kts`, `.lua`, `.ex/.exs`, `.hs/.lhs`, `.swift`), extracting definitions, references, imports, and call edges into a local SQLite database (`.lumora/graph.db`).

2. **Query**: The semantic graph supports symbol lookup, reference tracing, caller discovery, dependency paths, and code clone detection — all with ranking, dedup, and pagination.

3. **Serve**: The MCP server exposes all 17 tools over stdin/stdout JSON-RPC. Agents call tools, get precise results, and stay within their token budget.

Indexing is incremental — only changed files are re-processed. A full re-index is available with `--full`.

## CLI Reference

```bash
# Indexing
lumora index                    # Incremental index
lumora index --full --json      # Full rebuild, JSON output

# Watcher daemon
lumora serve --full-first       # Index then watch for changes

# Queries
lumora query symbol main
lumora query refs my_function --order score_desc --limit 50 --dedup true --top-files
lumora query callers handle_request --file-glob "src/*.rs" --limit 25
lumora query deps src/main.rs src/storage.rs --max-depth 10
lumora query slice src/main.rs --line 42 --depth 2
lumora query clones src/main.rs --limit 20 --hotspots

# MCP server
lumora mcp

# Helpers
lumora print-mcp-config          # Generate config snippet for any client
lumora setup-codex --replace     # One-command Codex registration
```

All commands auto-detect the repository root from your current directory. Use `--repo <path>` to override.

## Supported Languages

| Support Level | Languages | Parsing | File Operations |
|---------------|-----------|---------|-----------------|
| Full parsing | Rust (`.rs`), Python (`.py`) | Definitions, references, imports, calls | Read, write, edit, search, move, delete |
| Standard parsing | JavaScript (`.js`, `.jsx`, `.mjs`, `.cjs`), TypeScript (`.ts`, `.mts`, `.cts`), TSX (`.tsx`), Go (`.go`), Java (`.java`), C (`.c`, `.h`), C++ (`.cpp`, `.cc`, `.cxx`, `.hpp`, `.hxx`, `.hh`), C# (`.cs`), Ruby (`.rb`), Scala (`.scala`, `.sc`), Kotlin (`.kt`, `.kts`), Swift (`.swift`) | Definitions, references, calls (imports where available by grammar/query) | Read, write, edit, search, move, delete |
| Basic parsing | Bash (`.sh`, `.bash`, `.zsh`), Lua (`.lua`), Elixir (`.ex`, `.exs`), Haskell (`.hs`, `.lhs`) | Definitions, calls | Read, write, edit, search, move, delete |
| Structure only | JSON (`.json`), TOML (`.toml`), YAML (`.yml`, `.yaml`), CSS (`.css`), HTML (`.html`, `.htm`) | Structural definitions (keys/sections/selectors/elements) | Read, write, edit, search, move, delete |
| File operations only | All other files | — | Read, write, edit, search, move, delete |

## State & Storage

Lumora stores its index under `.lumora/` in your repository root:

```
.lumora/
  graph.db    # SQLite database with the semantic graph
```

Add `.lumora/` to your `.gitignore`. The index is fully regenerable from source.

## Platform Support

Linux, macOS, and Windows. CI runs on all three via GitHub Actions.

## Troubleshooting

**MCP server not starting?** Reinstall and re-register:

```bash
cargo install lumora --force
```

Then re-add the MCP server in your client (see setup instructions above).

**Slow startup?** Some MCP clients have configurable timeouts. If your client supports it, increase the MCP server startup timeout to 30 seconds.

**Index stale?** Run `lumora index` or use `lumora serve --full-first` for automatic re-indexing on file changes.

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md) for development setup, and [CHANGELOG.md](CHANGELOG.md) for release history.

## License

Dual-licensed under [MIT](LICENSE-MIT) or [Apache 2.0](LICENSE-APACHE), at your option.
