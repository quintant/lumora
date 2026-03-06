# Changelog

All notable changes to this project will be documented in this file.

The format is based on Keep a Changelog and this project follows Semantic Versioning.

## [0.4.0] - 2026-03-06

### Added
- New `multi_outline` MCP tool for batching AST-derived structure outlines across multiple files in one call.
- New `batch_edit` MCP tool for applying multiple validated text edits across one or more files atomically.
- New `symbol_source` MCP tool for reading symbol definition source spans directly with shared line budgets.
- New `apply_patch` MCP tool for exact line-based hunk application across existing files with atomic validation.

### Changed
- `file_outline` now returns the file path even for unsupported languages, which makes batched structural inspection easier to consume consistently.
- `symbol_definitions` responses now include stored symbol end positions when available.

### Fixed
- Removed an unused internal `Edge` model that was generating a warning on every build/test run.

## [0.3.0] - 2026-03-02

### Added
- 9 new file-operation MCP tools: `read_file`, `file_outline`, `search_files`, `list_directory`, `write_file`, `edit_file`, `multi_read`, `move_file`, `delete_file` — all sandboxed to the repository root.
- Support for 21 additional languages via tree-sitter grammars: JavaScript, TypeScript, TSX, Go, Java, C, C++, C#, Ruby, Bash, CSS, HTML, JSON, TOML, YAML, Scala, Kotlin, Lua, Elixir, Haskell, Swift.
- New `src/fileops.rs` module implementing all file-operation tool logic with path-traversal protection.
- New `src/storage.rs` module with a cleaner query/storage abstraction layer.
- New `src/paths.rs` module for repository-root detection and path sandboxing utilities.
- New `src/languages.rs` module centralising language detection and grammar dispatch.
- `regex` dependency for text search in `search_files`.
- Comprehensive unit tests across `mcp.rs` and the new modules.

### Changed
- MCP server now exposes 17 tools (up from 8).
- README updated to document all 17 tools and 23 supported languages.


## [0.2.0] - 2026-02-21

### Added
- Ranked and paged query responses for references/callers with `score_desc` and line-order modes.
- Richer diagnostics and optional freshness metadata for MCP query responses.
- Stronger selector discovery with fuzzy ranking, scope hints (`file_glob`, `entity_type`), and score/explanation fields.
- Clone query analysis metadata (`candidate_files`, `filtered_by_threshold`, `suggested_min_similarity`) plus hotspots mode pagination.
- Additional minimal-slice controls (`suppress_low_signal_repeats`, `low_signal_name_cap`, `prefer_project_symbols`).

### Changed
- Improved default context signal quality for `minimal_slice` by reducing default fanout.
- Improved default clone threshold behavior for better practical results on typical repositories.
- Updated documentation for dependency-path selector semantics and AI-agent-focused workflows.

### Fixed
- MCP query ergonomics and selector UX issues discovered during cross-agent testing.
- Multiple noise and relevance regressions in slice/discovery/clone outputs.

## [0.1.0] - 2026-02-21

### Added
- Offline semantic code graph CLI with incremental indexing.
- Event-driven watcher daemon using `notify`.
- SQLite-backed graph storage and query commands.
- MCP stdio server mode with tool-based graph access.
- `setup-codex` helper and generic MCP config output command.
- Cross-platform CI workflow for Linux, macOS, and Windows.
