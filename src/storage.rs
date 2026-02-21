use std::collections::{HashMap, HashSet, VecDeque};
use std::path::Path;

use anyhow::{Context, Result};
use rusqlite::{params, Connection, OptionalExtension};
use serde_json::json;

use crate::model::{
    CloneMatch, DependencyPath, Entity, FileExtraction, PathHop, ReferenceLocation, RelatedEdge,
    SliceResult, SymbolLocation,
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

    pub fn symbol_references(
        &self,
        symbol_name: &str,
        edge_type_filter: Option<&str>,
    ) -> Result<Vec<ReferenceLocation>> {
        let sql = match edge_type_filter {
            Some(_) => {
                "
                SELECT sn.name, e.file_path, e.line, e.col, e.edge_type
                FROM entities sn
                JOIN edges e ON e.dst_entity_id = sn.id
                WHERE sn.entity_type = 'symbol_name'
                  AND sn.name = ?1
                  AND e.edge_type = ?2
                ORDER BY e.file_path, e.line
                "
            }
            None => {
                "
                SELECT sn.name, e.file_path, e.line, e.col, e.edge_type
                FROM entities sn
                JOIN edges e ON e.dst_entity_id = sn.id
                WHERE sn.entity_type = 'symbol_name'
                  AND sn.name = ?1
                  AND e.edge_type IN ('references', 'calls')
                ORDER BY e.file_path, e.line
                "
            }
        };

        let mut stmt = self.conn.prepare(sql)?;
        let mapper = |row: &rusqlite::Row<'_>| {
            Ok(ReferenceLocation {
                symbol_name: row.get(0)?,
                file_path: row.get::<_, Option<String>>(1)?.unwrap_or_default(),
                line: row.get::<_, Option<i64>>(2)?.unwrap_or_default(),
                col: row.get::<_, Option<i64>>(3)?.unwrap_or_default(),
                edge_type: row.get(4)?,
            })
        };

        let rows = match edge_type_filter {
            Some(filter) => stmt.query_map(params![symbol_name, filter], mapper)?,
            None => stmt.query_map([symbol_name], mapper)?,
        };

        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(Into::into)
    }

    pub fn dependency_path(
        &self,
        from_selector: &str,
        to_selector: &str,
        max_depth: usize,
    ) -> Result<DependencyPath> {
        let Some(from) = self.find_entity(from_selector)? else {
            return Ok(DependencyPath {
                found: false,
                hops: Vec::new(),
            });
        };
        let Some(to) = self.find_entity(to_selector)? else {
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

    pub fn minimal_slice(
        &self,
        file_path: &str,
        line: Option<i64>,
        depth: usize,
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
        let mut frontier = vec![anchor.id];
        let mut seen: HashSet<i64> = HashSet::new();
        seen.insert(anchor.id);

        for _ in 0..depth.max(1) {
            let mut next = Vec::new();
            for node_id in frontier {
                for related in self.neighbor_edges(node_id)? {
                    if seen.insert(related.entity.id) {
                        next.push(related.entity.id);
                    }
                    neighbors.push(related);
                }
            }
            if next.is_empty() {
                break;
            }
            frontier = next;
        }

        Ok(Some(SliceResult { anchor, neighbors }))
    }

    pub fn clone_matches(&self, file_path: &str, min_similarity: f64) -> Result<Vec<CloneMatch>> {
        let self_count: i64 = self.conn.query_row(
            "SELECT COUNT(DISTINCT fp_hash) FROM fingerprints WHERE file_path = ?1",
            [file_path],
            |row| row.get(0),
        )?;

        if self_count == 0 {
            return Ok(Vec::new());
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

        let mut out = Vec::new();
        for row in shared_rows {
            let (other_file, shared_count) = row?;
            let other_total = totals.get(&other_file).copied().unwrap_or(1);
            let denom = self_count.max(other_total) as f64;
            let similarity = shared_count as f64 / denom;
            if similarity >= min_similarity {
                out.push(CloneMatch {
                    other_file,
                    shared_fingerprints: shared_count,
                    similarity,
                });
            }
        }

        Ok(out)
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

    fn find_entity(&self, selector: &str) -> Result<Option<Entity>> {
        if let Some(by_key) = self.find_entity_by_key(selector)? {
            return Ok(Some(by_key));
        }

        if let Some(file) = self.find_entity_by_key(&file_key(selector))? {
            return Ok(Some(file));
        }

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
            LIMIT 1
            ",
        )?;

        let found = stmt.query_row([selector], map_entity).optional()?;
        Ok(found)
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
