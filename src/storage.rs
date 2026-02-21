use std::cmp::Ordering;
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::Path;

use anyhow::{Context, Result};
use rusqlite::{params, Connection, OptionalExtension};
use serde_json::json;

use crate::model::{
    CloneHotspot, CloneMatch, DependencyPath, Entity, FileExtraction, PathHop, ReferenceLocation,
    RelatedEdge, SelectorSuggestion, SliceResult, SymbolLocation, TopFileSummary,
};

pub struct GraphStore {
    conn: Connection,
}

#[derive(Debug, Clone)]
pub struct UpsertOutcome {
    pub updated: usize,
    pub removed: usize,
    pub skipped: usize,
}

impl UpsertOutcome {
    pub fn new() -> Self {
        Self {
            updated: 0,
            removed: 0,
            skipped: 0,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SortOrder {
    ScoreDesc,
    LineAsc,
    LineDesc,
}

#[derive(Debug, Clone)]
pub struct ReferenceQueryOptions {
    pub edge_type_filter: Option<String>,
    pub file_glob: Option<String>,
    pub language: Option<String>,
    pub max_age_hours: Option<u64>,
    pub limit: usize,
    pub offset: usize,
    pub dedup: bool,
    pub order: SortOrder,
}

impl Default for ReferenceQueryOptions {
    fn default() -> Self {
        Self {
            edge_type_filter: None,
            file_glob: None,
            language: None,
            max_age_hours: None,
            limit: 200,
            offset: 0,
            dedup: true,
            order: SortOrder::ScoreDesc,
        }
    }
}

#[derive(Debug, Clone)]
pub struct SliceQueryOptions {
    pub max_neighbors: usize,
    pub dedup: bool,
    pub suppress_low_signal_repeats: bool,
    pub low_signal_name_cap: usize,
    pub prefer_project_symbols: bool,
}

impl Default for SliceQueryOptions {
    fn default() -> Self {
        Self {
            max_neighbors: 40,
            dedup: true,
            suppress_low_signal_repeats: true,
            low_signal_name_cap: 1,
            prefer_project_symbols: true,
        }
    }
}

#[derive(Debug, Clone)]
pub struct CloneQueryOptions {
    pub min_similarity: f64,
    pub limit: usize,
    pub offset: usize,
}

impl Default for CloneQueryOptions {
    fn default() -> Self {
        Self {
            min_similarity: 0.02,
            limit: 50,
            offset: 0,
        }
    }
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct PaginationInfo {
    pub total: usize,
    pub offset: usize,
    pub limit: usize,
    pub returned: usize,
    pub has_more: bool,
    pub next_offset: Option<usize>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct CloneAnalysis {
    pub self_fingerprint_count: i64,
    pub candidate_files: usize,
    pub surviving_candidates: usize,
    pub filtered_by_threshold: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_candidate_similarity: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub suggested_min_similarity: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub empty_reason: Option<String>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct FreshnessInfo {
    pub file_count: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub latest_indexed_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub schema_version: Option<String>,
    pub stale_after_hours: u64,
    pub is_stale: bool,
}

#[derive(Debug, Clone, Default)]
pub struct SelectorSuggestOptions {
    pub query: Option<String>,
    pub file_glob: Option<String>,
    pub entity_type: Option<String>,
    pub limit: usize,
    pub fuzzy: bool,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct SelectorResolution {
    pub parsed_as: String,
    pub matched: usize,
    pub selected_key: Option<String>,
}

#[derive(Debug, Clone)]
struct SelectorLookup {
    parsed_as: String,
    candidates: Vec<Entity>,
    entity: Option<Entity>,
}

impl GraphStore {
    pub fn open(db_path: &Path) -> Result<Self> {
        let conn = Connection::open(db_path)
            .with_context(|| format!("failed to open sqlite db at {}", db_path.display()))?;

        conn.execute_batch(
            "
            PRAGMA journal_mode = WAL;
            PRAGMA synchronous = NORMAL;
            PRAGMA foreign_keys = ON;

            CREATE TABLE IF NOT EXISTS files (
                path TEXT PRIMARY KEY,
                lang TEXT NOT NULL,
                content_hash TEXT NOT NULL,
                size_bytes INTEGER NOT NULL,
                indexed_at TEXT NOT NULL
            );

            CREATE TABLE IF NOT EXISTS entities (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                entity_type TEXT NOT NULL,
                key TEXT NOT NULL UNIQUE,
                name TEXT NOT NULL,
                lang TEXT,
                file_path TEXT,
                line INTEGER,
                col INTEGER,
                end_line INTEGER,
                end_col INTEGER,
                meta_json TEXT
            );

            CREATE TABLE IF NOT EXISTS edges (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                src_entity_id INTEGER NOT NULL,
                dst_entity_id INTEGER NOT NULL,
                edge_type TEXT NOT NULL,
                file_path TEXT,
                line INTEGER,
                col INTEGER,
                meta_json TEXT,
                FOREIGN KEY(src_entity_id) REFERENCES entities(id) ON DELETE CASCADE,
                FOREIGN KEY(dst_entity_id) REFERENCES entities(id) ON DELETE CASCADE
            );

            CREATE TABLE IF NOT EXISTS fingerprints (
                file_path TEXT NOT NULL,
                fp_hash INTEGER NOT NULL,
                span_start INTEGER NOT NULL,
                span_end INTEGER NOT NULL
            );

            CREATE TABLE IF NOT EXISTS meta (
                key TEXT PRIMARY KEY,
                value TEXT NOT NULL
            );

            CREATE INDEX IF NOT EXISTS idx_files_hash ON files(content_hash);
            CREATE INDEX IF NOT EXISTS idx_entities_name_type ON entities(name, entity_type);
            CREATE INDEX IF NOT EXISTS idx_entities_file_type ON entities(file_path, entity_type);
            CREATE INDEX IF NOT EXISTS idx_edges_src_type ON edges(src_entity_id, edge_type);
            CREATE INDEX IF NOT EXISTS idx_edges_dst_type ON edges(dst_entity_id, edge_type);
            CREATE INDEX IF NOT EXISTS idx_edges_file ON edges(file_path);
            CREATE INDEX IF NOT EXISTS idx_fingerprints_hash ON fingerprints(fp_hash, file_path);
            CREATE INDEX IF NOT EXISTS idx_fingerprints_file ON fingerprints(file_path);
            ",
        )?;

        conn.execute(
            "INSERT INTO meta(key, value) VALUES('schema_version', '1')
             ON CONFLICT(key) DO UPDATE SET value=excluded.value",
            [],
        )?;

        Ok(Self { conn })
    }

    pub fn tracked_file_hash(&self, path: &str) -> Result<Option<String>> {
        let hash = self
            .conn
            .query_row(
                "SELECT content_hash FROM files WHERE path = ?1",
                [path],
                |row| row.get(0),
            )
            .optional()?;
        Ok(hash)
    }

    pub fn tracked_files(&self) -> Result<HashSet<String>> {
        let mut stmt = self.conn.prepare("SELECT path FROM files")?;
        let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;
        let mut out = HashSet::new();
        for row in rows {
            out.insert(row?);
        }
        Ok(out)
    }

    pub fn remove_files(
        &mut self,
        removed_paths: &[String],
        outcome: &mut UpsertOutcome,
    ) -> Result<()> {
        let tx = self.conn.transaction()?;
        for file_path in removed_paths {
            tx.execute("DELETE FROM fingerprints WHERE file_path = ?1", [file_path])?;
            tx.execute("DELETE FROM edges WHERE file_path = ?1", [file_path])?;
            tx.execute(
                "DELETE FROM entities WHERE file_path = ?1 OR key = ?2",
                params![file_path, file_key(file_path)],
            )?;
            tx.execute("DELETE FROM files WHERE path = ?1", [file_path])?;
            outcome.removed += 1;
        }
        tx.commit()?;
        self.cleanup_orphan_nodes()?;
        Ok(())
    }

    pub fn index_file(
        &mut self,
        file_path: &str,
        language: &str,
        content_hash: &str,
        size_bytes: u64,
        extraction: &FileExtraction,
        fingerprints: &[(i64, i64, i64)],
        resolved_imports: &[(String, String)],
        outcome: &mut UpsertOutcome,
    ) -> Result<()> {
        let tx = self.conn.transaction()?;

        tx.execute("DELETE FROM fingerprints WHERE file_path = ?1", [file_path])?;
        tx.execute("DELETE FROM edges WHERE file_path = ?1", [file_path])?;
        tx.execute(
            "DELETE FROM entities WHERE file_path = ?1 AND entity_type != 'file'",
            [file_path],
        )?;

        tx.execute(
            "INSERT INTO files(path, lang, content_hash, size_bytes, indexed_at)
             VALUES(?1, ?2, ?3, ?4, datetime('now'))
             ON CONFLICT(path) DO UPDATE SET
                lang=excluded.lang,
                content_hash=excluded.content_hash,
                size_bytes=excluded.size_bytes,
                indexed_at=excluded.indexed_at",
            params![file_path, language, content_hash, size_bytes as i64],
        )?;

        let file_entity_id = ensure_entity_with_tx(
            &tx,
            "file",
            &file_key(file_path),
            file_path,
            Some(language),
            Some(file_path),
            None,
            None,
            None,
            None,
            Some(json!({"kind": "source"}).to_string()),
        )?;

        let mut symbol_name_entities: HashMap<String, i64> = HashMap::new();
        for definition in &extraction.definitions {
            let symbol_key = format!(
                "symbol:{}:{}:{}:{}:{}",
                file_path, definition.qualname, definition.kind, definition.line, definition.col
            );
            let symbol_meta = json!({
                "qualname": definition.qualname,
                "kind": definition.kind,
                "is_definition": true,
            })
            .to_string();

            let symbol_entity_id = ensure_entity_with_tx(
                &tx,
                "symbol",
                &symbol_key,
                &definition.name,
                Some(language),
                Some(file_path),
                Some(definition.line),
                Some(definition.col),
                Some(definition.end_line),
                Some(definition.end_col),
                Some(symbol_meta),
            )?;

            insert_edge_with_tx(
                &tx,
                file_entity_id,
                symbol_entity_id,
                "defines",
                Some(file_path),
                Some(definition.line),
                Some(definition.col),
                None,
            )?;

            let name_entity_id = if let Some(existing) = symbol_name_entities.get(&definition.name)
            {
                *existing
            } else {
                let key = symbol_name_key(language, &definition.name);
                let entity_id = ensure_entity_with_tx(
                    &tx,
                    "symbol_name",
                    &key,
                    &definition.name,
                    Some(language),
                    None,
                    None,
                    None,
                    None,
                    None,
                    None,
                )?;
                symbol_name_entities.insert(definition.name.clone(), entity_id);
                entity_id
            };

            insert_edge_with_tx(
                &tx,
                symbol_entity_id,
                name_entity_id,
                "names",
                Some(file_path),
                Some(definition.line),
                Some(definition.col),
                None,
            )?;
        }

        for reference in &extraction.references {
            let name_entity_id = if let Some(existing) = symbol_name_entities.get(&reference.name) {
                *existing
            } else {
                let key = symbol_name_key(language, &reference.name);
                let entity_id = ensure_entity_with_tx(
                    &tx,
                    "symbol_name",
                    &key,
                    &reference.name,
                    Some(language),
                    None,
                    None,
                    None,
                    None,
                    None,
                    None,
                )?;
                symbol_name_entities.insert(reference.name.clone(), entity_id);
                entity_id
            };

            let meta = json!({
                "end_line": reference.end_line,
                "end_col": reference.end_col
            })
            .to_string();

            insert_edge_with_tx(
                &tx,
                file_entity_id,
                name_entity_id,
                reference.kind.as_edge_type(),
                Some(file_path),
                Some(reference.line),
                Some(reference.col),
                Some(meta),
            )?;
        }

        for import_item in &extraction.imports {
            let module_entity_id = ensure_entity_with_tx(
                &tx,
                "module",
                &module_key(language, &import_item.module),
                &import_item.module,
                Some(language),
                None,
                None,
                None,
                None,
                None,
                None,
            )?;

            insert_edge_with_tx(
                &tx,
                file_entity_id,
                module_entity_id,
                "imports",
                Some(file_path),
                Some(import_item.line),
                Some(import_item.col),
                None,
            )?;
        }

        for (module_name, resolved_file) in resolved_imports {
            let module_entity_id = ensure_entity_with_tx(
                &tx,
                "module",
                &module_key(language, module_name),
                module_name,
                Some(language),
                None,
                None,
                None,
                None,
                None,
                None,
            )?;
            let resolved_file_id = ensure_entity_with_tx(
                &tx,
                "file",
                &file_key(resolved_file),
                resolved_file,
                Some(language),
                Some(resolved_file),
                None,
                None,
                None,
                None,
                Some(json!({"kind": "source"}).to_string()),
            )?;

            insert_edge_with_tx(
                &tx,
                module_entity_id,
                resolved_file_id,
                "resolves_to",
                Some(file_path),
                None,
                None,
                None,
            )?;
            insert_edge_with_tx(
                &tx,
                file_entity_id,
                resolved_file_id,
                "depends_on",
                Some(file_path),
                None,
                None,
                Some(json!({"via": module_name}).to_string()),
            )?;
        }

        let config_or_entry = classify_special_file(file_path);
        if let Some(entity_type) = config_or_entry {
            let special_id = ensure_entity_with_tx(
                &tx,
                entity_type,
                &format!("{}:{}", entity_type, file_path),
                file_path,
                Some(language),
                Some(file_path),
                None,
                None,
                None,
                None,
                None,
            )?;
            insert_edge_with_tx(
                &tx,
                file_entity_id,
                special_id,
                "contains",
                Some(file_path),
                None,
                None,
                None,
            )?;
        }

        for (fp_hash, span_start, span_end) in fingerprints {
            tx.execute(
                "INSERT INTO fingerprints(file_path, fp_hash, span_start, span_end)
                 VALUES(?1, ?2, ?3, ?4)",
                params![file_path, fp_hash, span_start, span_end],
            )?;
        }

        tx.commit()?;
        self.cleanup_orphan_nodes()?;
        outcome.updated += 1;
        Ok(())
    }

    pub fn symbol_definitions(&self, symbol_name: &str) -> Result<Vec<SymbolLocation>> {
        let mut stmt = self.conn.prepare(
            "
            SELECT s.name, s.file_path, s.line, s.col,
                   json_extract(s.meta_json, '$.kind') as kind,
                   json_extract(s.meta_json, '$.qualname') as qualname
            FROM entities sn
            JOIN edges en ON en.dst_entity_id = sn.id AND en.edge_type = 'names'
            JOIN entities s ON s.id = en.src_entity_id AND s.entity_type = 'symbol'
            WHERE sn.entity_type = 'symbol_name' AND sn.name = ?1
            ORDER BY s.file_path, s.line
            ",
        )?;

        let rows = stmt.query_map([symbol_name], |row| {
            Ok(SymbolLocation {
                symbol_name: row.get(0)?,
                file_path: row.get::<_, Option<String>>(1)?.unwrap_or_default(),
                line: row.get::<_, Option<i64>>(2)?.unwrap_or_default(),
                col: row.get::<_, Option<i64>>(3)?.unwrap_or_default(),
                kind: row
                    .get::<_, Option<String>>(4)?
                    .unwrap_or_else(|| "unknown".to_string()),
                qualname: row
                    .get::<_, Option<String>>(5)?
                    .unwrap_or_else(|| symbol_name.to_string()),
            })
        })?;

        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(Into::into)
    }

    pub fn symbol_references_page(
        &self,
        symbol_name: &str,
        options: &ReferenceQueryOptions,
    ) -> Result<(Vec<ReferenceLocation>, PaginationInfo)> {
        let mut out = self.symbol_references_unpaged(symbol_name, options)?;
        let total = out.len();
        let start = options.offset.min(total);
        let end = start.saturating_add(options.limit).min(total);
        let rows = out.drain(start..end).collect::<Vec<_>>();
        let pagination = build_pagination(total, options.offset, options.limit, rows.len());
        Ok((rows, pagination))
    }

    fn symbol_references_unpaged(
        &self,
        symbol_name: &str,
        options: &ReferenceQueryOptions,
    ) -> Result<Vec<ReferenceLocation>> {
        let mut params: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();
        let mut where_clauses = vec![
            "sn.entity_type = 'symbol_name'".to_string(),
            "sn.name = ?".to_string(),
        ];
        params.push(Box::new(symbol_name.to_string()));

        match options.edge_type_filter.as_deref() {
            Some(edge_type) => {
                where_clauses.push("e.edge_type = ?".to_string());
                params.push(Box::new(edge_type.to_string()));
            }
            None => where_clauses.push("e.edge_type IN ('references', 'calls')".to_string()),
        }

        if let Some(glob) = options.file_glob.as_deref() {
            where_clauses.push("e.file_path GLOB ?".to_string());
            params.push(Box::new(glob.replace('\\', "/")));
        }

        if let Some(language) = options.language.as_deref() {
            where_clauses.push("f.lang = ?".to_string());
            params.push(Box::new(language.to_string()));
        }

        if let Some(max_age_hours) = options.max_age_hours {
            where_clauses.push("f.indexed_at >= datetime('now', ?)".to_string());
            params.push(Box::new(format!("-{max_age_hours} hours")));
        }

        let sql = format!(
            "
            SELECT sn.name, e.file_path, e.line, e.col, e.edge_type
            FROM entities sn
            JOIN edges e ON e.dst_entity_id = sn.id
            LEFT JOIN files f ON f.path = e.file_path
            WHERE {}
            ORDER BY e.file_path ASC, e.line ASC, e.col ASC
            ",
            where_clauses.join(" AND ")
        );

        let mut stmt = self.conn.prepare(&sql)?;
        let bind_params = rusqlite::params_from_iter(params.iter().map(|p| &**p));
        let rows = stmt.query_map(bind_params, |row| {
            Ok(ReferenceLocation {
                symbol_name: row.get(0)?,
                file_path: row.get::<_, Option<String>>(1)?.unwrap_or_default(),
                line: row.get::<_, Option<i64>>(2)?.unwrap_or_default(),
                col: row.get::<_, Option<i64>>(3)?.unwrap_or_default(),
                edge_type: row.get(4)?,
                score: None,
                why: None,
            })
        })?;

        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }

        if options.dedup {
            let mut seen = HashSet::new();
            out.retain(|item| {
                seen.insert((
                    item.file_path.clone(),
                    item.line,
                    item.col,
                    item.edge_type.clone(),
                ))
            });
        }

        let def_files = self.definition_files_for_symbol(symbol_name)?;
        for item in &mut out {
            let mut score = if item.edge_type == "calls" { 2.0 } else { 1.0 };
            let mut why = vec![format!("edge_type={}", item.edge_type)];
            if def_files.contains(&item.file_path) {
                score += 0.35;
                why.push("same_file_as_definition".to_string());
            }
            item.score = Some(score);
            item.why = Some(why.join(","));
        }

        out.sort_by(reference_sorter(options.order));
        Ok(out)
    }

    pub fn dependency_path(
        &self,
        from_selector: &str,
        to_selector: &str,
        max_depth: usize,
    ) -> Result<DependencyPath> {
        let from_resolution = self.resolve_selector(from_selector)?;
        let to_resolution = self.resolve_selector(to_selector)?;

        let Some(from) = from_resolution.entity else {
            return Ok(DependencyPath {
                found: false,
                hops: Vec::new(),
            });
        };
        let Some(to) = to_resolution.entity else {
            return Ok(DependencyPath {
                found: false,
                hops: Vec::new(),
            });
        };

        if from.id == to.id {
            return Ok(DependencyPath {
                found: true,
                hops: vec![PathHop {
                    entity_key: from.key,
                    entity_name: from.name,
                    entity_type: from.entity_type,
                }],
            });
        }

        let mut queue: VecDeque<(i64, usize)> = VecDeque::new();
        let mut seen: HashSet<i64> = HashSet::new();
        let mut prev: HashMap<i64, i64> = HashMap::new();

        queue.push_back((from.id, 0));
        seen.insert(from.id);

        while let Some((current, depth)) = queue.pop_front() {
            if depth >= max_depth {
                continue;
            }
            for neighbor in self.outgoing_neighbors(current)? {
                if seen.insert(neighbor) {
                    prev.insert(neighbor, current);
                    if neighbor == to.id {
                        let mut chain = vec![to.id];
                        let mut cursor = to.id;
                        while let Some(parent) = prev.get(&cursor) {
                            chain.push(*parent);
                            if *parent == from.id {
                                break;
                            }
                            cursor = *parent;
                        }
                        chain.reverse();

                        let mut hops = Vec::with_capacity(chain.len());
                        for entity_id in chain {
                            let entity = self.entity_by_id(entity_id)?;
                            hops.push(PathHop {
                                entity_key: entity.key,
                                entity_name: entity.name,
                                entity_type: entity.entity_type,
                            });
                        }

                        return Ok(DependencyPath { found: true, hops });
                    }
                    queue.push_back((neighbor, depth + 1));
                }
            }
        }

        Ok(DependencyPath {
            found: false,
            hops: Vec::new(),
        })
    }

    pub fn dependency_path_with_diagnostics(
        &self,
        from_selector: &str,
        to_selector: &str,
        max_depth: usize,
    ) -> Result<(DependencyPath, SelectorResolution, SelectorResolution)> {
        let from_resolution = self.resolve_selector(from_selector)?;
        let to_resolution = self.resolve_selector(to_selector)?;

        let from_diag = SelectorResolution {
            parsed_as: from_resolution.parsed_as.clone(),
            matched: from_resolution.candidates.len(),
            selected_key: from_resolution.entity.as_ref().map(|item| item.key.clone()),
        };
        let to_diag = SelectorResolution {
            parsed_as: to_resolution.parsed_as.clone(),
            matched: to_resolution.candidates.len(),
            selected_key: to_resolution.entity.as_ref().map(|item| item.key.clone()),
        };

        let path = self.dependency_path(from_selector, to_selector, max_depth)?;
        Ok((path, from_diag, to_diag))
    }

    pub fn minimal_slice_with_options(
        &self,
        file_path: &str,
        line: Option<i64>,
        depth: usize,
        options: &SliceQueryOptions,
    ) -> Result<Option<SliceResult>> {
        let anchor = if let Some(line_no) = line {
            self.anchor_symbol_for_line(file_path, line_no)?
                .or_else(|| self.find_entity_by_key(&file_key(file_path)).ok().flatten())
        } else {
            self.find_entity_by_key(&file_key(file_path))?
        };

        let Some(anchor) = anchor else {
            return Ok(None);
        };

        let mut neighbors = Vec::new();
        let mut frontier = vec![(anchor.id, 0usize)];
        let mut seen: HashSet<i64> = HashSet::new();
        seen.insert(anchor.id);
        let mut seen_edges: HashSet<(String, String, i64, String)> = HashSet::new();

        for _ in 0..depth.max(1) {
            let mut next = Vec::new();
            for (node_id, level) in frontier {
                for mut related in self.neighbor_edges(node_id)? {
                    if seen.insert(related.entity.id) {
                        next.push((related.entity.id, level + 1));
                    }
                    if options.dedup
                        && !seen_edges.insert((
                            related.direction.clone(),
                            related.edge_type.clone(),
                            related.entity.id,
                            related.entity.key.clone(),
                        ))
                    {
                        continue;
                    }

                    let score =
                        score_related_edge(&related, level + 1, options.prefer_project_symbols);
                    related.depth = Some((level + 1) as i64);
                    related.score = Some(score);
                    related.why = Some(format!(
                        "edge_type={},direction={},depth={}",
                        related.edge_type,
                        related.direction,
                        level + 1
                    ));
                    neighbors.push(related);
                }
            }
            if next.is_empty() {
                break;
            }
            frontier = next;

            if options.max_neighbors > 0 && neighbors.len() >= options.max_neighbors {
                break;
            }
        }

        if options.max_neighbors > 0 {
            neighbors.sort_by(related_edge_sorter);
            if neighbors.len() > options.max_neighbors {
                neighbors.truncate(options.max_neighbors);
            }
        }

        if options.suppress_low_signal_repeats {
            let cap = options.low_signal_name_cap.max(1);
            let mut seen_symbol_names: HashMap<String, usize> = HashMap::new();
            neighbors.retain(|edge| {
                if edge.entity.entity_type != "symbol_name" {
                    return true;
                }
                let per_name_cap = cap;
                let count = seen_symbol_names
                    .entry(edge.entity.name.clone())
                    .or_insert(0);
                if *count >= per_name_cap {
                    return false;
                }
                *count += 1;
                true
            });
        }

        Ok(Some(SliceResult { anchor, neighbors }))
    }

    pub fn clone_matches_with_options(
        &self,
        file_path: &str,
        options: &CloneQueryOptions,
    ) -> Result<Vec<CloneMatch>> {
        let (rows, _, _) = self.clone_matches_page(file_path, options)?;
        Ok(rows)
    }

    pub fn clone_matches_page(
        &self,
        file_path: &str,
        options: &CloneQueryOptions,
    ) -> Result<(Vec<CloneMatch>, PaginationInfo, CloneAnalysis)> {
        let self_count: i64 = self.conn.query_row(
            "SELECT COUNT(DISTINCT fp_hash) FROM fingerprints WHERE file_path = ?1",
            [file_path],
            |row| row.get(0),
        )?;

        if self_count == 0 {
            let pagination = build_pagination(0, options.offset, options.limit, 0);
            let analysis = CloneAnalysis {
                self_fingerprint_count: 0,
                candidate_files: 0,
                surviving_candidates: 0,
                filtered_by_threshold: 0,
                max_candidate_similarity: None,
                suggested_min_similarity: Some(0.0),
                empty_reason: Some(
                    "source file has no fingerprints; file may be too small or not yet indexed"
                        .to_string(),
                ),
            };
            return Ok((Vec::new(), pagination, analysis));
        }

        let mut shared_stmt = self.conn.prepare(
            "
            SELECT f2.file_path, COUNT(DISTINCT f1.fp_hash) AS shared_count
            FROM fingerprints f1
            JOIN fingerprints f2 ON f1.fp_hash = f2.fp_hash
            WHERE f1.file_path = ?1
              AND f2.file_path != ?1
            GROUP BY f2.file_path
            ORDER BY shared_count DESC
            ",
        )?;

        let shared_rows = shared_stmt.query_map([file_path], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
        })?;

        let mut counts_stmt = self.conn.prepare(
            "SELECT file_path, COUNT(DISTINCT fp_hash) FROM fingerprints GROUP BY file_path",
        )?;
        let counts_rows = counts_stmt.query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
        })?;

        let mut totals: HashMap<String, i64> = HashMap::new();
        for row in counts_rows {
            let (path, cnt) = row?;
            totals.insert(path, cnt);
        }

        let mut all_candidates = Vec::new();
        for row in shared_rows {
            let (other_file, shared_count) = row?;
            let other_total = totals.get(&other_file).copied().unwrap_or(1);
            let denom = self_count.max(other_total) as f64;
            let similarity = shared_count as f64 / denom;
            all_candidates.push(CloneMatch {
                other_file,
                shared_fingerprints: shared_count,
                similarity,
            });
        }

        let candidate_files = all_candidates.len();
        let max_candidate_similarity = all_candidates
            .iter()
            .map(|item| item.similarity)
            .max_by(|left, right| left.partial_cmp(right).unwrap_or(Ordering::Equal));
        let mut surviving = all_candidates
            .into_iter()
            .filter(|row| row.similarity >= options.min_similarity)
            .collect::<Vec<_>>();
        let surviving_count = surviving.len();
        let filtered_by_threshold = candidate_files.saturating_sub(surviving_count);

        surviving.sort_by(|left, right| {
            right
                .similarity
                .partial_cmp(&left.similarity)
                .unwrap_or(Ordering::Equal)
                .then_with(|| right.shared_fingerprints.cmp(&left.shared_fingerprints))
                .then_with(|| left.other_file.cmp(&right.other_file))
        });

        let total = surviving.len();
        let start = options.offset.min(total);
        let end = start.saturating_add(options.limit).min(total);
        let rows = surviving[start..end].to_vec();
        let pagination = build_pagination(total, options.offset, options.limit, rows.len());

        let empty_reason = if total > 0 {
            None
        } else if candidate_files == 0 {
            Some("no overlapping fingerprints with other files".to_string())
        } else {
            Some(format!(
                "all clone candidates were filtered by min_similarity={:.3}; try lowering the threshold",
                options.min_similarity
            ))
        };
        let analysis = CloneAnalysis {
            self_fingerprint_count: self_count,
            candidate_files,
            surviving_candidates: surviving_count,
            filtered_by_threshold,
            max_candidate_similarity,
            suggested_min_similarity: max_candidate_similarity.map(|value| (value * 0.9).max(0.0)),
            empty_reason,
        };

        Ok((rows, pagination, analysis))
    }

    pub fn clone_hotspots(
        &self,
        file_path: &str,
        options: &CloneQueryOptions,
    ) -> Result<Vec<CloneHotspot>> {
        let (rows, _, _) = self.clone_hotspots_page(file_path, options)?;
        Ok(rows)
    }

    pub fn clone_hotspots_page(
        &self,
        file_path: &str,
        options: &CloneQueryOptions,
    ) -> Result<(Vec<CloneHotspot>, PaginationInfo, CloneAnalysis)> {
        let (rows, _, analysis) = self.clone_matches_page(
            file_path,
            &CloneQueryOptions {
                min_similarity: options.min_similarity,
                limit: usize::MAX,
                offset: 0,
            },
        )?;
        let mut buckets: HashMap<String, (i64, f64, f64)> = HashMap::new();
        for row in rows {
            let dir = Path::new(&row.other_file)
                .parent()
                .map(|value| value.to_string_lossy().replace('\\', "/"))
                .unwrap_or_else(|| ".".to_string());
            let entry = buckets.entry(dir).or_insert((0, 0.0, 0.0));
            entry.0 += 1;
            entry.1 += row.similarity;
            if row.similarity > entry.2 {
                entry.2 = row.similarity;
            }
        }

        let mut out = Vec::new();
        for (directory, (files, sum_similarity, max_similarity)) in buckets {
            out.push(CloneHotspot {
                directory,
                files,
                avg_similarity: if files == 0 {
                    0.0
                } else {
                    sum_similarity / files as f64
                },
                max_similarity,
            });
        }

        out.sort_by(|left, right| {
            right
                .avg_similarity
                .partial_cmp(&left.avg_similarity)
                .unwrap_or(Ordering::Equal)
                .then_with(|| right.files.cmp(&left.files))
                .then_with(|| left.directory.cmp(&right.directory))
        });
        let total = out.len();
        let start = options.offset.min(total);
        let end = start.saturating_add(options.limit).min(total);
        let rows = out[start..end].to_vec();
        let pagination = build_pagination(total, options.offset, options.limit, rows.len());
        Ok((rows, pagination, analysis))
    }

    pub fn top_reference_files(
        &self,
        rows: &[ReferenceLocation],
        limit: usize,
    ) -> Vec<TopFileSummary> {
        let mut counts: HashMap<String, i64> = HashMap::new();
        for row in rows {
            *counts.entry(row.file_path.clone()).or_insert(0) += 1;
        }

        let mut out: Vec<TopFileSummary> = counts
            .into_iter()
            .map(|(file_path, count)| TopFileSummary { file_path, count })
            .collect();
        out.sort_by(|left, right| {
            right
                .count
                .cmp(&left.count)
                .then_with(|| left.file_path.cmp(&right.file_path))
        });
        if limit > 0 && out.len() > limit {
            out.truncate(limit);
        }
        out
    }

    pub fn selector_suggestions_advanced(
        &self,
        options: &SelectorSuggestOptions,
    ) -> Result<Vec<SelectorSuggestion>> {
        let query_tokens = tokenize_discovery_query(options.query.as_deref().unwrap_or_default());
        let query_lower = options
            .query
            .as_deref()
            .map(str::trim)
            .unwrap_or_default()
            .to_ascii_lowercase();
        let base_fetch = (options.limit.max(1) * 8).min(2000) as i64;

        let mut out = self.selector_suggestions_fetch(
            options,
            &query_lower,
            &query_tokens,
            options.fuzzy,
            base_fetch,
        )?;
        if out.is_empty() && options.fuzzy && !query_lower.is_empty() {
            // Fuzzy fallback: broaden to scope-only fetch, then rank in-memory.
            let widened_fetch = (options.limit.max(1) * 200).min(20000) as i64;
            out = self.selector_suggestions_fetch(
                options,
                &query_lower,
                &query_tokens,
                false,
                widened_fetch,
            )?;
        }

        for suggestion in &mut out {
            let (score, why) =
                discovery_score(suggestion, &query_lower, &query_tokens, options.fuzzy);
            suggestion.score = Some(score);
            suggestion.why = Some(why);
        }

        out.sort_by(|left, right| {
            right
                .score
                .partial_cmp(&left.score)
                .unwrap_or(Ordering::Equal)
                .then_with(|| entity_rank(&left.entity_type).cmp(&entity_rank(&right.entity_type)))
                .then_with(|| left.key.cmp(&right.key))
        });

        let limit = options.limit.max(1);
        if out.len() > limit {
            out.truncate(limit);
        }
        Ok(out)
    }

    fn selector_suggestions_fetch(
        &self,
        options: &SelectorSuggestOptions,
        query_lower: &str,
        query_tokens: &[String],
        include_query_filter: bool,
        fetch_limit: i64,
    ) -> Result<Vec<SelectorSuggestion>> {
        let mut where_clauses = Vec::new();
        let mut params: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();

        if include_query_filter && !query_lower.is_empty() {
            if options.fuzzy && !query_tokens.is_empty() {
                let mut token_parts = Vec::new();
                for token in query_tokens {
                    token_parts.push(
                        "(key LIKE ? OR name LIKE ? OR COALESCE(file_path, '') LIKE ?)".to_string(),
                    );
                    let wildcard = format!("%{}%", token.to_ascii_lowercase());
                    params.push(Box::new(wildcard.clone()));
                    params.push(Box::new(wildcard.clone()));
                    params.push(Box::new(wildcard));
                }
                where_clauses.push(format!("({})", token_parts.join(" OR ")));
            } else {
                where_clauses.push(
                    "(key LIKE ? OR name LIKE ? OR COALESCE(file_path, '') LIKE ?)".to_string(),
                );
                let wildcard = format!("%{}%", query_lower);
                params.push(Box::new(wildcard.clone()));
                params.push(Box::new(wildcard.clone()));
                params.push(Box::new(wildcard));
            }
        }

        if let Some(entity_type) = options.entity_type.as_deref() {
            where_clauses.push("entity_type = ?".to_string());
            params.push(Box::new(entity_type.to_string()));
        }

        if let Some(file_glob) = options.file_glob.as_deref() {
            // Scope hint: constrain file-backed entities while keeping global entities discoverable.
            where_clauses.push("(file_path IS NULL OR COALESCE(file_path, '') GLOB ?)".to_string());
            params.push(Box::new(file_glob.replace('\\', "/")));
        }

        let where_sql = if where_clauses.is_empty() {
            "1=1".to_string()
        } else {
            where_clauses.join(" AND ")
        };

        let sql = format!(
            "
            SELECT entity_type, key, name, file_path, line
            FROM entities
            WHERE {where_sql}
            ORDER BY
                CASE entity_type
                    WHEN 'file' THEN 0
                    WHEN 'symbol_name' THEN 1
                    WHEN 'symbol' THEN 2
                    WHEN 'module' THEN 3
                    ELSE 9
                END,
                key
            LIMIT ?
            "
        );
        params.push(Box::new(fetch_limit.max(1)));

        let bind_params = rusqlite::params_from_iter(params.iter().map(|p| &**p));
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt.query_map(bind_params, |row| {
            Ok(SelectorSuggestion {
                entity_type: row.get(0)?,
                key: row.get(1)?,
                name: row.get(2)?,
                file_path: row.get(3)?,
                line: row.get(4)?,
                score: None,
                why: None,
            })
        })?;

        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    pub fn index_warning(&self, stale_after_hours: u64) -> Result<Option<String>> {
        let file_count: i64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM files", [], |row| row.get(0))?;
        if file_count == 0 {
            return Ok(Some(
                "index is empty; run lumora.index_repository before querying".to_string(),
            ));
        }

        let latest: Option<String> = self
            .conn
            .query_row("SELECT MAX(indexed_at) FROM files", [], |row| row.get(0))
            .optional()?
            .flatten();
        let Some(latest) = latest else {
            return Ok(Some(
                "index timestamp unavailable; results may be partial".to_string(),
            ));
        };

        let stale_cutoff = format!("-{stale_after_hours} hours");
        let stale: i64 = self.conn.query_row(
            "SELECT CASE WHEN ?1 < datetime('now', ?2) THEN 1 ELSE 0 END",
            params![latest, stale_cutoff],
            |row| row.get(0),
        )?;
        if stale > 0 {
            return Ok(Some(format!(
                "index appears stale (latest indexed_at={latest}); consider re-indexing"
            )));
        }
        Ok(None)
    }

    pub fn freshness_info(&self, stale_after_hours: u64) -> Result<FreshnessInfo> {
        let file_count: i64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM files", [], |row| row.get(0))?;
        let latest_indexed_at: Option<String> = self
            .conn
            .query_row("SELECT MAX(indexed_at) FROM files", [], |row| row.get(0))
            .optional()?
            .flatten();
        let schema_version: Option<String> = self
            .conn
            .query_row(
                "SELECT value FROM meta WHERE key = 'schema_version'",
                [],
                |row| row.get(0),
            )
            .optional()?;

        let is_stale = if let Some(latest) = latest_indexed_at.as_deref() {
            let stale_cutoff = format!("-{stale_after_hours} hours");
            let stale: i64 = self.conn.query_row(
                "SELECT CASE WHEN ?1 < datetime('now', ?2) THEN 1 ELSE 0 END",
                params![latest, stale_cutoff],
                |row| row.get(0),
            )?;
            stale > 0
        } else {
            true
        };

        Ok(FreshnessInfo {
            file_count,
            latest_indexed_at,
            schema_version,
            stale_after_hours,
            is_stale,
        })
    }

    fn definition_files_for_symbol(&self, symbol_name: &str) -> Result<HashSet<String>> {
        let mut stmt = self.conn.prepare(
            "
            SELECT DISTINCT s.file_path
            FROM entities sn
            JOIN edges en ON en.dst_entity_id = sn.id AND en.edge_type = 'names'
            JOIN entities s ON s.id = en.src_entity_id AND s.entity_type = 'symbol'
            WHERE sn.entity_type = 'symbol_name' AND sn.name = ?1
            ",
        )?;
        let rows = stmt.query_map([symbol_name], |row| row.get::<_, Option<String>>(0))?;
        let mut out = HashSet::new();
        for row in rows {
            if let Some(file) = row? {
                out.insert(file);
            }
        }
        Ok(out)
    }

    fn resolve_selector(&self, selector: &str) -> Result<SelectorLookup> {
        let parsed = parse_selector(selector)?;
        match parsed {
            ParsedSelector::Key(key) => {
                let candidate = self.find_entity_by_key(&key)?;
                let candidates = candidate.clone().into_iter().collect::<Vec<_>>();
                Ok(SelectorLookup {
                    parsed_as: "key".to_string(),
                    entity: candidate,
                    candidates,
                })
            }
            ParsedSelector::File(path) => {
                let normalized = normalize_selector_path(&path);
                let key = file_key(&normalized);
                let candidate = self.find_entity_by_key(&key)?;
                let candidates = candidate.clone().into_iter().collect::<Vec<_>>();
                Ok(SelectorLookup {
                    parsed_as: "file".to_string(),
                    entity: candidate,
                    candidates,
                })
            }
            ParsedSelector::SymbolName { lang, name } => {
                let key = symbol_name_key(&lang, &name);
                let candidate = self.find_entity_by_key(&key)?;
                let candidates = candidate.clone().into_iter().collect::<Vec<_>>();
                Ok(SelectorLookup {
                    parsed_as: "symbol_name".to_string(),
                    entity: candidate,
                    candidates,
                })
            }
            ParsedSelector::Name(name) => {
                let candidates = self.entities_by_name(&name)?;
                let entity = candidates.first().cloned();
                Ok(SelectorLookup {
                    parsed_as: "name".to_string(),
                    candidates,
                    entity,
                })
            }
            ParsedSelector::Auto(raw) => {
                let normalized = normalize_selector_path(&raw);
                let mut candidates = Vec::new();
                if let Some(by_key) = self.find_entity_by_key(&normalized)? {
                    candidates.push(by_key);
                }
                if let Some(file_match) = self.find_entity_by_key(&file_key(&normalized))? {
                    candidates.push(file_match);
                }
                for by_name in self.entities_by_name(&raw)? {
                    candidates.push(by_name);
                }
                dedup_entities_by_id(&mut candidates);
                candidates.sort_by(|left, right| {
                    entity_rank(&left.entity_type)
                        .cmp(&entity_rank(&right.entity_type))
                        .then_with(|| left.key.cmp(&right.key))
                });
                let entity = candidates.first().cloned();
                Ok(SelectorLookup {
                    parsed_as: "auto".to_string(),
                    candidates,
                    entity,
                })
            }
        }
    }

    fn outgoing_neighbors(&self, entity_id: i64) -> Result<Vec<i64>> {
        let mut stmt = self
            .conn
            .prepare("SELECT dst_entity_id FROM edges WHERE src_entity_id = ?1")?;
        let rows = stmt.query_map([entity_id], |row| row.get::<_, i64>(0))?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(Into::into)
    }

    fn entity_by_id(&self, id: i64) -> Result<Entity> {
        self.conn.query_row(
            "
            SELECT id, entity_type, key, name, lang, file_path, line, col, end_line, end_col, meta_json
            FROM entities
            WHERE id = ?1
            ",
            [id],
            map_entity,
        ).map_err(Into::into)
    }

    fn entities_by_name(&self, name: &str) -> Result<Vec<Entity>> {
        let mut stmt = self.conn.prepare(
            "
            SELECT id, entity_type, key, name, lang, file_path, line, col, end_line, end_col, meta_json
            FROM entities
            WHERE name = ?1
            ORDER BY
                CASE entity_type
                    WHEN 'symbol' THEN 0
                    WHEN 'symbol_name' THEN 1
                    WHEN 'file' THEN 2
                    ELSE 3
                END,
                file_path,
                line
            LIMIT 32
            ",
        )?;

        let rows = stmt.query_map([name], map_entity)?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(Into::into)
    }

    fn find_entity_by_key(&self, key: &str) -> Result<Option<Entity>> {
        let mut stmt = self.conn.prepare(
            "
            SELECT id, entity_type, key, name, lang, file_path, line, col, end_line, end_col, meta_json
            FROM entities
            WHERE key = ?1
            LIMIT 1
            ",
        )?;

        stmt.query_row([key], map_entity)
            .optional()
            .map_err(Into::into)
    }

    fn anchor_symbol_for_line(&self, file_path: &str, line: i64) -> Result<Option<Entity>> {
        let mut stmt = self.conn.prepare(
            "
            SELECT id, entity_type, key, name, lang, file_path, line, col, end_line, end_col, meta_json
            FROM entities
            WHERE entity_type = 'symbol'
              AND file_path = ?1
              AND line <= ?2
              AND end_line >= ?2
            ORDER BY (end_line - line) ASC
            LIMIT 1
            ",
        )?;
        stmt.query_row(params![file_path, line], map_entity)
            .optional()
            .map_err(Into::into)
    }

    fn neighbor_edges(&self, entity_id: i64) -> Result<Vec<RelatedEdge>> {
        let mut out = Vec::new();

        let mut outgoing = self.conn.prepare(
            "
            SELECT e.edge_type,
                   dst.id, dst.entity_type, dst.key, dst.name, dst.lang, dst.file_path,
                   dst.line, dst.col, dst.end_line, dst.end_col, dst.meta_json
            FROM edges e
            JOIN entities dst ON dst.id = e.dst_entity_id
            WHERE e.src_entity_id = ?1
            ",
        )?;

        let outgoing_rows = outgoing.query_map([entity_id], |row| {
            Ok(RelatedEdge {
                edge_type: row.get(0)?,
                direction: "outgoing".to_string(),
                entity: Entity {
                    id: row.get(1)?,
                    entity_type: row.get(2)?,
                    key: row.get(3)?,
                    name: row.get(4)?,
                    lang: row.get(5)?,
                    file_path: row.get(6)?,
                    line: row.get(7)?,
                    col: row.get(8)?,
                    end_line: row.get(9)?,
                    end_col: row.get(10)?,
                    meta_json: row.get(11)?,
                },
                depth: None,
                score: None,
                why: None,
            })
        })?;

        for row in outgoing_rows {
            out.push(row?);
        }

        let mut incoming = self.conn.prepare(
            "
            SELECT e.edge_type,
                   src.id, src.entity_type, src.key, src.name, src.lang, src.file_path,
                   src.line, src.col, src.end_line, src.end_col, src.meta_json
            FROM edges e
            JOIN entities src ON src.id = e.src_entity_id
            WHERE e.dst_entity_id = ?1
            ",
        )?;

        let incoming_rows = incoming.query_map([entity_id], |row| {
            Ok(RelatedEdge {
                edge_type: row.get(0)?,
                direction: "incoming".to_string(),
                entity: Entity {
                    id: row.get(1)?,
                    entity_type: row.get(2)?,
                    key: row.get(3)?,
                    name: row.get(4)?,
                    lang: row.get(5)?,
                    file_path: row.get(6)?,
                    line: row.get(7)?,
                    col: row.get(8)?,
                    end_line: row.get(9)?,
                    end_col: row.get(10)?,
                    meta_json: row.get(11)?,
                },
                depth: None,
                score: None,
                why: None,
            })
        })?;

        for row in incoming_rows {
            out.push(row?);
        }

        Ok(out)
    }

    fn cleanup_orphan_nodes(&mut self) -> Result<()> {
        self.conn.execute(
            "
            DELETE FROM entities
            WHERE entity_type IN ('symbol_name', 'module')
              AND id NOT IN (SELECT src_entity_id FROM edges)
              AND id NOT IN (SELECT dst_entity_id FROM edges)
            ",
            [],
        )?;
        Ok(())
    }
}

#[derive(Debug, Clone)]
enum ParsedSelector {
    Key(String),
    File(String),
    SymbolName { lang: String, name: String },
    Name(String),
    Auto(String),
}

fn parse_selector(selector: &str) -> Result<ParsedSelector> {
    let value = selector.trim();
    if value.is_empty() {
        anyhow::bail!(
            "selector is empty. Examples: file:src/main.rs, symbol_name:rust:run_mcp_stdio, main"
        );
    }

    if let Some(rest) = value.strip_prefix("file:") {
        let file = rest.trim();
        if file.is_empty() {
            anyhow::bail!("invalid `file:` selector: missing path. Example: file:src/main.rs");
        }
        return Ok(ParsedSelector::File(file.to_string()));
    }

    if let Some(rest) = value.strip_prefix("symbol_name:") {
        let mut parts = rest.splitn(2, ':');
        let lang = parts.next().unwrap_or_default().trim();
        let name = parts.next().unwrap_or_default().trim();
        if lang.is_empty() || name.is_empty() {
            anyhow::bail!(
                "invalid `symbol_name:` selector. Expected symbol_name:<lang>:<name>, e.g. symbol_name:rust:run_mcp_stdio"
            );
        }
        return Ok(ParsedSelector::SymbolName {
            lang: lang.to_string(),
            name: name.to_string(),
        });
    }

    if let Some(rest) = value.strip_prefix("symbol:") {
        let symbol = rest.trim();
        if symbol.is_empty() {
            anyhow::bail!("invalid `symbol:` selector: missing name. Example: symbol:main");
        }
        return Ok(ParsedSelector::Name(symbol.to_string()));
    }

    if value.starts_with("key:") {
        let raw = value.trim_start_matches("key:").trim();
        if raw.is_empty() {
            anyhow::bail!("invalid `key:` selector: missing key value");
        }
        return Ok(ParsedSelector::Key(raw.to_string()));
    }

    if value.starts_with("file:")
        || value.starts_with("symbol_name:")
        || value.starts_with("symbol:")
        || value.starts_with("key:")
    {
        anyhow::bail!(
            "unsupported selector form `{value}`. Examples: file:src/main.rs, symbol_name:rust:main, symbol:main"
        );
    }

    if value.starts_with("module:") || value.starts_with("symbol_name:") {
        return Ok(ParsedSelector::Key(value.to_string()));
    }

    Ok(ParsedSelector::Auto(value.to_string()))
}

fn normalize_selector_path(path: &str) -> String {
    path.trim().replace('\\', "/")
}

fn dedup_entities_by_id(items: &mut Vec<Entity>) {
    let mut seen = HashSet::new();
    items.retain(|item| seen.insert(item.id));
}

fn entity_rank(entity_type: &str) -> i64 {
    match entity_type {
        "symbol" => 0,
        "symbol_name" => 1,
        "file" => 2,
        "module" => 3,
        _ => 9,
    }
}

fn reference_sorter(
    order: SortOrder,
) -> impl FnMut(&ReferenceLocation, &ReferenceLocation) -> Ordering + Copy {
    move |left, right| {
        let score_cmp = right
            .score
            .partial_cmp(&left.score)
            .unwrap_or(Ordering::Equal);
        let path_cmp = left.file_path.cmp(&right.file_path);
        let line_cmp = left.line.cmp(&right.line);
        let col_cmp = left.col.cmp(&right.col);

        match order {
            SortOrder::ScoreDesc => score_cmp
                .then_with(|| path_cmp)
                .then_with(|| line_cmp)
                .then_with(|| col_cmp),
            SortOrder::LineAsc => path_cmp.then_with(|| line_cmp).then_with(|| col_cmp),
            SortOrder::LineDesc => path_cmp
                .reverse()
                .then_with(|| line_cmp.reverse())
                .then_with(|| col_cmp.reverse()),
        }
    }
}

fn score_related_edge(edge: &RelatedEdge, depth: usize, prefer_project_symbols: bool) -> f64 {
    let edge_weight = match edge.edge_type.as_str() {
        "calls" => 2.5,
        "depends_on" => 2.2,
        "imports" => 2.0,
        "defines" => 1.8,
        "references" => 1.2,
        "names" => 0.8,
        "contains" => 0.6,
        _ => 1.0,
    };
    let direction_boost = if edge.direction == "outgoing" {
        0.2
    } else {
        0.0
    };
    let depth_penalty = (depth as f64 - 1.0) * 0.25;
    let mut score = edge_weight + direction_boost - depth_penalty;

    if edge.entity.entity_type == "symbol_name" {
        if is_low_signal_symbol_name(&edge.entity.name) {
            score -= 1.3;
        } else if prefer_project_symbols && is_project_local_symbol_name(&edge.entity.name) {
            score += 0.35;
        }
    }

    score.max(0.0)
}

fn related_edge_sorter(left: &RelatedEdge, right: &RelatedEdge) -> Ordering {
    right
        .score
        .partial_cmp(&left.score)
        .unwrap_or(Ordering::Equal)
        .then_with(|| left.edge_type.cmp(&right.edge_type))
        .then_with(|| left.direction.cmp(&right.direction))
        .then_with(|| left.entity.key.cmp(&right.entity.key))
}

fn build_pagination(total: usize, offset: usize, limit: usize, returned: usize) -> PaginationInfo {
    let safe_limit = limit.max(1);
    let safe_offset = offset.min(total);
    let has_more = safe_offset + returned < total;
    let next_offset = if has_more {
        Some(safe_offset + returned)
    } else {
        None
    };

    PaginationInfo {
        total,
        offset: safe_offset,
        limit: safe_limit,
        returned,
        has_more,
        next_offset,
    }
}

fn tokenize_discovery_query(input: &str) -> Vec<String> {
    input
        .split(|ch: char| !(ch.is_ascii_alphanumeric() || ch == ':' || ch == '/'))
        .filter(|part| !part.is_empty())
        .map(|part| part.to_ascii_lowercase())
        .collect()
}

fn discovery_score(
    suggestion: &SelectorSuggestion,
    query_lower: &str,
    query_tokens: &[String],
    fuzzy: bool,
) -> (f64, String) {
    let key = suggestion.key.to_ascii_lowercase();
    let name = suggestion.name.to_ascii_lowercase();
    let path = suggestion
        .file_path
        .as_deref()
        .unwrap_or_default()
        .replace('\\', "/")
        .to_ascii_lowercase();

    let mut score = 0.0;
    let mut reasons = Vec::new();

    if query_lower.is_empty() {
        score = 10.0 - entity_rank(&suggestion.entity_type) as f64;
        reasons.push("no_query_default_ranking".to_string());
        return (score, reasons.join(","));
    }

    if name == query_lower {
        score += 120.0;
        reasons.push("exact_name".to_string());
    }
    if key == query_lower {
        score += 120.0;
        reasons.push("exact_key".to_string());
    }
    if name.starts_with(query_lower) {
        score += 70.0;
        reasons.push("name_prefix".to_string());
    }
    if key.starts_with(query_lower) {
        score += 60.0;
        reasons.push("key_prefix".to_string());
    }
    if name.contains(query_lower) {
        score += 50.0;
        reasons.push("name_contains".to_string());
    }
    if key.contains(query_lower) {
        score += 40.0;
        reasons.push("key_contains".to_string());
    }
    if path.contains(query_lower) {
        score += 32.0;
        reasons.push("path_contains".to_string());
    }

    let mut token_match_count = 0usize;
    for token in query_tokens {
        if token.is_empty() {
            continue;
        }
        if name.contains(token) {
            score += 10.0;
            token_match_count += 1;
        }
        if key.contains(token) {
            score += 8.0;
            token_match_count += 1;
        }
        if path.contains(token) {
            score += 6.0;
            token_match_count += 1;
        }
    }
    if token_match_count > 0 {
        reasons.push(format!("token_matches={token_match_count}"));
    }

    if fuzzy {
        let name_ratio = fuzzy_subsequence_ratio(query_lower, &name);
        let key_ratio = fuzzy_subsequence_ratio(query_lower, &key);
        let path_ratio = fuzzy_subsequence_ratio(query_lower, &path);
        let best = name_ratio.max(key_ratio).max(path_ratio);
        if best > 0.0 {
            score += best * 25.0;
            reasons.push(format!("fuzzy={best:.2}"));
        }
    }

    score += (10 - entity_rank(&suggestion.entity_type)).max(0) as f64 * 0.2;
    if reasons.is_empty() {
        reasons.push("fallback_rank".to_string());
    }
    (score, reasons.join(","))
}

fn fuzzy_subsequence_ratio(query: &str, text: &str) -> f64 {
    if query.is_empty() || text.is_empty() {
        return 0.0;
    }
    let normalize = |input: &str| {
        input
            .chars()
            .filter(|ch| ch.is_ascii_alphanumeric())
            .collect::<String>()
            .to_ascii_lowercase()
    };
    let norm_query = normalize(query);
    let norm_text = normalize(text);
    if norm_query.is_empty() || norm_text.is_empty() {
        return 0.0;
    }
    if norm_text.contains(&norm_query) {
        return 1.0;
    }

    let qchars: Vec<char> = norm_query.chars().collect();
    let mut matched = 0usize;
    let mut qidx = 0usize;
    for ch in norm_text.chars() {
        if qidx < qchars.len() && ch == qchars[qidx] {
            matched += 1;
            qidx += 1;
        }
    }
    matched as f64 / qchars.len() as f64
}

fn is_low_signal_symbol_name(name: &str) -> bool {
    matches!(
        name,
        "Ok" | "Err"
            | "Some"
            | "None"
            | "Result"
            | "Option"
            | "String"
            | "Vec"
            | "Box"
            | "Self"
            | "self"
    )
}

fn is_project_local_symbol_name(name: &str) -> bool {
    if is_low_signal_symbol_name(name) {
        return false;
    }

    if name.len() <= 2 {
        return false;
    }

    let lower = name.to_ascii_lowercase();
    !matches!(
        lower.as_str(),
        "string"
            | "str"
            | "vec"
            | "box"
            | "result"
            | "option"
            | "path"
            | "pathbuf"
            | "hashmap"
            | "hashset"
            | "usize"
            | "u64"
            | "i64"
            | "bool"
    )
}

fn ensure_entity_with_tx(
    tx: &rusqlite::Transaction<'_>,
    entity_type: &str,
    key: &str,
    name: &str,
    lang: Option<&str>,
    file_path: Option<&str>,
    line: Option<i64>,
    col: Option<i64>,
    end_line: Option<i64>,
    end_col: Option<i64>,
    meta_json: Option<String>,
) -> Result<i64> {
    tx.execute(
        "
        INSERT INTO entities(entity_type, key, name, lang, file_path, line, col, end_line, end_col, meta_json)
        VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
        ON CONFLICT(key) DO UPDATE SET
            entity_type=excluded.entity_type,
            name=excluded.name,
            lang=COALESCE(excluded.lang, entities.lang),
            file_path=COALESCE(excluded.file_path, entities.file_path),
            line=COALESCE(excluded.line, entities.line),
            col=COALESCE(excluded.col, entities.col),
            end_line=COALESCE(excluded.end_line, entities.end_line),
            end_col=COALESCE(excluded.end_col, entities.end_col),
            meta_json=COALESCE(excluded.meta_json, entities.meta_json)
        ",
        params![
            entity_type,
            key,
            name,
            lang,
            file_path,
            line,
            col,
            end_line,
            end_col,
            meta_json
        ],
    )?;

    tx.query_row("SELECT id FROM entities WHERE key = ?1", [key], |row| {
        row.get(0)
    })
    .map_err(Into::into)
}

fn insert_edge_with_tx(
    tx: &rusqlite::Transaction<'_>,
    src_entity_id: i64,
    dst_entity_id: i64,
    edge_type: &str,
    file_path: Option<&str>,
    line: Option<i64>,
    col: Option<i64>,
    meta_json: Option<String>,
) -> Result<()> {
    tx.execute(
        "
        INSERT INTO edges(src_entity_id, dst_entity_id, edge_type, file_path, line, col, meta_json)
        VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7)
        ",
        params![
            src_entity_id,
            dst_entity_id,
            edge_type,
            file_path,
            line,
            col,
            meta_json
        ],
    )?;
    Ok(())
}

fn map_entity(row: &rusqlite::Row<'_>) -> rusqlite::Result<Entity> {
    Ok(Entity {
        id: row.get(0)?,
        entity_type: row.get(1)?,
        key: row.get(2)?,
        name: row.get(3)?,
        lang: row.get(4)?,
        file_path: row.get(5)?,
        line: row.get(6)?,
        col: row.get(7)?,
        end_line: row.get(8)?,
        end_col: row.get(9)?,
        meta_json: row.get(10)?,
    })
}

pub fn file_key(path: &str) -> String {
    format!("file:{path}")
}

pub fn symbol_name_key(lang: &str, symbol_name: &str) -> String {
    format!("symbol_name:{lang}:{symbol_name}")
}

pub fn module_key(lang: &str, module_name: &str) -> String {
    format!("module:{lang}:{module_name}")
}

fn classify_special_file(path: &str) -> Option<&'static str> {
    let lower = path.replace('\\', "/").to_lowercase();
    if lower.ends_with("cargo.toml")
        || lower.ends_with("pyproject.toml")
        || lower.ends_with("setup.cfg")
        || lower.ends_with("package.json")
    {
        return Some("config");
    }

    if lower.ends_with("/src/main.rs")
        || lower.ends_with("/src/lib.rs")
        || lower.ends_with("/__main__.py")
    {
        return Some("entrypoint");
    }

    None
}
