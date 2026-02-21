# Changelog

All notable changes to this project will be documented in this file.

The format is based on Keep a Changelog and this project follows Semantic Versioning.

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
