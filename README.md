# Lumora

Lumora is an offline semantic code graph + query engine with an MCP server interface for agent tooling.

- Incremental indexing
- Event-driven watcher daemon
- SQLite-backed graph queries
- MCP stdio tools for agents

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

## Platform support

Lumora targets Linux, macOS, and Windows.

Prerequisite on all platforms: a working C toolchain (required by `tree-sitter` and bundled `sqlite`).

- Linux: `build-essential` (or GCC/Clang equivalent)
- macOS: Xcode Command Line Tools (`xcode-select --install`)
- Windows: Visual Studio C++ Build Tools

Cross-platform CI runs on Ubuntu, macOS, and Windows via `.github/workflows/ci.yml`.

## State layout

By default, Lumora stores generated state under:

- `.lumora/graph.db`
- future state/index files under `.lumora/`

## MCP setup

### Codex (one command)

```bash
lumora setup-codex --repo . --replace
```

If `codex` is not on PATH:

```bash
lumora setup-codex --repo . --replace --codex-command /path/to/codex
```

Verify:

```bash
codex mcp get lumora
codex mcp list
```

### Other MCP clients

Print a portable JSON snippet:

```bash
lumora print-mcp-config --repo .
```

Supported tool names:

- `lumora.index_repository`
- `lumora.symbol_definitions`
- `lumora.symbol_references`
- `lumora.symbol_callers`
- `lumora.dependency_path`
- `lumora.minimal_slice`
- `lumora.clone_matches`

## Common commands

```bash
# Index
lumora index --repo .
lumora index --repo . --full --json

# Watcher daemon
lumora serve --repo . --full-first

# Query examples
lumora query --repo . symbol index_repository
lumora query --repo . refs ensure_entity_with_tx --calls-only
lumora query --repo . deps file:src/main.rs file:src/storage.rs --max-depth 10

# MCP server mode
lumora mcp --repo .
```

## Release checklist (maintainers)

```bash
cargo fmt
cargo check
cargo test
cargo package
cargo publish --dry-run
```

Then tag and publish.

## Publish to crates.io (maintainers)

```bash
# one-time: authenticate (token from https://crates.io/settings/tokens)
cargo login <CRATES_IO_TOKEN>

# validate package
cargo publish --dry-run

# publish
cargo publish
```

## Documentation and project links

- Changelog: `CHANGELOG.md`
- Contributing: `CONTRIBUTING.md`
- Security policy: `SECURITY.md`

## License

Dual licensed under either:

- MIT license (`LICENSE-MIT`)
- Apache License 2.0 (`LICENSE-APACHE`)

at your option.
