use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context, Result};
use regex::Regex;
use serde::Deserialize;
use serde_json::{json, Value};
use walkdir::{DirEntry, WalkDir};

use crate::parser::{detect_language, parse_file};

const IGNORE_DIRS: &[&str] = &[
    ".git",
    "target",
    "node_modules",
    "dist",
    "build",
    "venv",
    ".venv",
    "__pycache__",
    ".mypy_cache",
    ".pytest_cache",
    ".lumora",
];

#[derive(Debug, Clone, Deserialize)]
pub struct MultiReadRequest {
    pub path: String,
    pub start_line: Option<u64>,
    pub end_line: Option<u64>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct MultiOutlineRequest {
    pub path: String,
    pub max_depth: Option<usize>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct BatchEditRequest {
    pub path: String,
    pub old_text: String,
    pub new_text: String,
    pub replace_all: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub struct PatchHunkRequest {
    pub start_line: u64,
    pub old_lines: Vec<String>,
    pub new_lines: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct FilePatchRequest {
    pub path: String,
    pub hunks: Vec<PatchHunkRequest>,
}

pub fn safe_resolve_path(base: &Path, user_path: &str) -> Result<PathBuf> {
    let base_canonical = fs::canonicalize(base)
        .with_context(|| format!("failed to canonicalize base path {}", base.display()))?;
    let joined = base.join(user_path);

    if joined.exists() {
        let resolved = fs::canonicalize(&joined)
            .with_context(|| format!("failed to canonicalize path {}", joined.display()))?;
        if !resolved.starts_with(&base_canonical) {
            return Err(anyhow!("path escapes repository root"));
        }
        return Ok(resolved);
    }

    let parent = joined
        .parent()
        .ok_or_else(|| anyhow!("invalid path: missing parent"))?;
    if !parent.exists() {
        return Err(anyhow!("parent directory does not exist"));
    }

    let parent_canonical = fs::canonicalize(parent)
        .with_context(|| format!("failed to canonicalize parent {}", parent.display()))?;
    if !parent_canonical.starts_with(&base_canonical) {
        return Err(anyhow!("path escapes repository root"));
    }

    let file_name = joined
        .file_name()
        .ok_or_else(|| anyhow!("invalid path: missing file name"))?;
    Ok(parent_canonical.join(file_name))
}

pub fn read_file_contents(
    repo_root: &Path,
    path: &str,
    start_line: Option<u64>,
    end_line: Option<u64>,
    max_lines: u64,
) -> Result<Value> {
    let resolved = safe_resolve_path(repo_root, path)?;
    let source = fs::read_to_string(&resolved)
        .with_context(|| format!("failed to read {}", resolved.display()))?;
    let language = detect_language(&resolved)
        .map(|lang| lang.as_str().to_string())
        .or_else(|| {
            resolved
                .extension()
                .and_then(|ext| ext.to_str())
                .map(str::to_string)
        });

    build_read_response(
        repo_root, &resolved, &source, start_line, end_line, max_lines, language,
    )
}

pub fn file_outline(repo_root: &Path, path: &str, max_depth: Option<usize>) -> Result<Value> {
    let resolved = safe_resolve_path(repo_root, path)?;
    let source = fs::read_to_string(&resolved)
        .with_context(|| format!("failed to read {}", resolved.display()))?;
    let rel_path = to_rel_path(repo_root, &resolved)?;

    let Some(parsed) = parse_file(&resolved, &source)? else {
        return Ok(json!({
            "path": rel_path,
            "language": Value::Null,
            "entries": [],
            "note": "unsupported language for AST outline"
        }));
    };

    let entries: Vec<Value> = parsed
        .definitions
        .into_iter()
        .filter(|definition| {
            if let Some(max_depth) = max_depth {
                definition.qualname.matches("::").count() <= max_depth
            } else {
                true
            }
        })
        .map(|definition| {
            json!({
                "name": definition.name,
                "kind": definition.kind,
                "qualname": definition.qualname,
                "line": definition.line,
                "end_line": definition.end_line
            })
        })
        .collect();

    Ok(json!({
        "path": rel_path,
        "language": parsed.language.as_str(),
        "entries": entries
    }))
}

pub fn multi_outline(repo_root: &Path, outlines: &[MultiOutlineRequest]) -> Result<Value> {
    let mut results = Vec::with_capacity(outlines.len());
    let mut total_entries = 0_u64;
    let mut unsupported_files = 0_u64;

    for request in outlines {
        let outline = file_outline(repo_root, &request.path, request.max_depth)?;
        total_entries += outline
            .get("entries")
            .and_then(Value::as_array)
            .map(|entries| entries.len() as u64)
            .unwrap_or(0);
        if outline.get("note").and_then(Value::as_str)
            == Some("unsupported language for AST outline")
        {
            unsupported_files += 1;
        }
        results.push(outline);
    }

    Ok(json!({
        "results": results,
        "total_entries": total_entries,
        "unsupported_files": unsupported_files
    }))
}

pub fn search_in_files(
    repo_root: &Path,
    pattern: &str,
    file_glob: Option<&str>,
    context_lines: u64,
    max_results: u64,
    is_regex: bool,
) -> Result<Value> {
    let regex = if is_regex {
        Regex::new(pattern).with_context(|| format!("invalid regex pattern `{pattern}`"))?
    } else {
        Regex::new(&regex::escape(pattern)).expect("escaped literal regex should compile")
    };
    let file_glob_regex = file_glob.map(glob_to_regex).transpose()?;

    let mut matches = Vec::new();
    let mut truncated = false;

    let walker = WalkDir::new(repo_root).into_iter().filter_entry(|entry| {
        if entry.depth() == 0 {
            return true;
        }
        should_descend(entry)
    });

    for entry in walker {
        let entry = entry?;
        if !entry.file_type().is_file() {
            continue;
        }

        let rel_path = to_rel_path(repo_root, entry.path())?;
        if let Some(glob_regex) = file_glob_regex.as_ref() {
            if !glob_regex.is_match(&rel_path) {
                continue;
            }
        }

        let content = match fs::read_to_string(entry.path()) {
            Ok(value) => value,
            Err(_) => continue,
        };
        let lines: Vec<&str> = content.lines().collect();
        let context = context_lines as usize;

        for (idx, line) in lines.iter().enumerate() {
            if !regex.is_match(line) {
                continue;
            }

            let before_start = idx.saturating_sub(context);
            let after_end = (idx + context + 1).min(lines.len());
            let context_before = lines[before_start..idx]
                .iter()
                .map(|v| (*v).to_string())
                .collect::<Vec<_>>();
            let context_after = lines[idx + 1..after_end]
                .iter()
                .map(|v| (*v).to_string())
                .collect::<Vec<_>>();

            matches.push(json!({
                "file": rel_path,
                "line": idx + 1,
                "content": line,
                "context_before": context_before,
                "context_after": context_after
            }));

            if matches.len() as u64 >= max_results {
                truncated = true;
                break;
            }
        }

        if truncated {
            break;
        }
    }

    let total_matches = matches.len();
    Ok(json!({
        "matches": matches,
        "total_matches": total_matches,
        "truncated": truncated
    }))
}

pub fn list_dir(
    repo_root: &Path,
    path: &str,
    recursive: bool,
    max_depth: u64,
    file_glob: Option<&str>,
) -> Result<Value> {
    let resolved = safe_resolve_path(repo_root, path)?;
    if !resolved.is_dir() {
        return Err(anyhow!("path is not a directory"));
    }

    let file_glob_regex = file_glob.map(glob_to_regex).transpose()?;
    let mut entries = Vec::new();

    if recursive {
        let depth = max_depth.max(1) as usize;
        let walker = WalkDir::new(&resolved)
            .min_depth(1)
            .max_depth(depth)
            .into_iter()
            .filter_entry(|entry| {
                if entry.path() == resolved {
                    return true;
                }
                should_descend(entry)
            });

        for entry in walker {
            let entry = entry?;
            push_dir_entry(repo_root, &entry, file_glob_regex.as_ref(), &mut entries)?;
        }
    } else {
        for entry in fs::read_dir(&resolved)? {
            let entry = entry?;
            let file_type = entry.file_type()?;
            if file_type.is_dir() {
                let name = entry.file_name();
                if let Some(name) = name.to_str() {
                    if IGNORE_DIRS.contains(&name) {
                        continue;
                    }
                }
            }

            let path = entry.path();
            let rel_path = to_rel_path(repo_root, &path)?;
            if let Some(glob_regex) = file_glob_regex.as_ref() {
                if file_type.is_file() && !glob_regex.is_match(&rel_path) {
                    continue;
                }
            }

            let metadata = entry.metadata()?;
            let entry_type = if file_type.is_dir() { "dir" } else { "file" };
            let size = if file_type.is_file() {
                Some(metadata.len())
            } else {
                None
            };
            entries.push(json!({
                "name": entry.file_name().to_string_lossy().to_string(),
                "path": rel_path,
                "type": entry_type,
                "size": size
            }));
        }
    }

    entries.sort_by(|left, right| {
        let left_path = left.get("path").and_then(Value::as_str).unwrap_or_default();
        let right_path = right
            .get("path")
            .and_then(Value::as_str)
            .unwrap_or_default();
        left_path.cmp(right_path)
    });

    Ok(json!({
        "path": to_rel_path(repo_root, &resolved)?,
        "entries": entries
    }))
}

pub fn write_file_contents(
    repo_root: &Path,
    path: &str,
    content: &str,
    create_dirs: bool,
) -> Result<Value> {
    let resolved = match safe_resolve_path(repo_root, path) {
        Ok(path) => path,
        Err(err) if create_dirs => {
            prepare_parent_dirs(repo_root, path)?;
            safe_resolve_path(repo_root, path).with_context(|| {
                format!("failed to resolve path after creating directories: {err}")
            })?
        }
        Err(err) => return Err(err),
    };

    if create_dirs {
        if let Some(parent) = resolved.parent() {
            fs::create_dir_all(parent).with_context(|| {
                format!("failed to create parent dirs for {}", resolved.display())
            })?;
        }
    }

    fs::write(&resolved, content)
        .with_context(|| format!("failed to write {}", resolved.display()))?;

    Ok(json!({
        "path": to_rel_path(repo_root, &resolved)?,
        "bytes_written": content.len()
    }))
}

pub fn move_file_op(repo_root: &Path, source: &str, destination: &str) -> Result<Value> {
    let source_path = safe_resolve_path(repo_root, source)?;
    let destination_path = safe_resolve_path(repo_root, destination)?;
    fs::rename(&source_path, &destination_path).with_context(|| {
        format!(
            "failed to move {} to {}",
            source_path.display(),
            destination_path.display()
        )
    })?;

    Ok(json!({
        "source": to_rel_path(repo_root, &source_path)?,
        "destination": to_rel_path(repo_root, &destination_path)?
    }))
}

pub fn delete_file_op(repo_root: &Path, path: &str) -> Result<Value> {
    let resolved = safe_resolve_path(repo_root, path)?;
    if !resolved.exists() {
        return Err(anyhow!("file does not exist"));
    }
    if !resolved.is_file() {
        return Err(anyhow!("path is not a file"));
    }

    fs::remove_file(&resolved)
        .with_context(|| format!("failed to delete {}", resolved.display()))?;

    Ok(json!({
        "path": to_rel_path(repo_root, &resolved)?,
        "deleted": true
    }))
}

pub fn edit_file_contents(
    repo_root: &Path,
    path: &str,
    old_text: &str,
    new_text: &str,
    dry_run: bool,
) -> Result<Value> {
    let resolved = safe_resolve_path(repo_root, path)?;
    let original = fs::read_to_string(&resolved)
        .with_context(|| format!("failed to read {}", resolved.display()))?;
    let applied_edit = apply_text_edit(&original, old_text, new_text, false)?;
    let updated = applied_edit.updated;
    if !dry_run {
        fs::write(&resolved, updated.as_bytes())
            .with_context(|| format!("failed to write {}", resolved.display()))?;
    }

    Ok(json!({
        "path": to_rel_path(repo_root, &resolved)?,
        "applied": !dry_run,
        "occurrences_found": applied_edit.occurrences_found,
        "replacements_applied": applied_edit.replacements_applied,
        "diff_preview": applied_edit.diff_preview
    }))
}

pub fn batch_edit_file_contents(
    repo_root: &Path,
    edits: &[BatchEditRequest],
    dry_run: bool,
) -> Result<Value> {
    let mut pending_files = Vec::<PendingFileEdit>::new();
    let mut results = Vec::with_capacity(edits.len());
    let mut total_replacements_applied = 0_u64;

    for edit in edits {
        let resolved = safe_resolve_path(repo_root, &edit.path)?;
        let rel_path = to_rel_path(repo_root, &resolved)?;

        let pending_idx = if let Some(idx) = pending_files
            .iter()
            .position(|item| item.resolved == resolved)
        {
            idx
        } else {
            let original = fs::read_to_string(&resolved)
                .with_context(|| format!("failed to read {}", resolved.display()))?;
            pending_files.push(PendingFileEdit {
                resolved: resolved.clone(),
                original: original.clone(),
                current: original,
            });
            pending_files.len() - 1
        };

        let pending = &mut pending_files[pending_idx];
        let applied_edit = apply_text_edit(
            &pending.current,
            &edit.old_text,
            &edit.new_text,
            edit.replace_all,
        )?;
        pending.current = applied_edit.updated;
        total_replacements_applied += applied_edit.replacements_applied;

        results.push(json!({
            "path": rel_path,
            "replace_all": edit.replace_all,
            "occurrences_found": applied_edit.occurrences_found,
            "replacements_applied": applied_edit.replacements_applied,
            "diff_preview": applied_edit.diff_preview
        }));
    }

    let changed_files = pending_files
        .iter()
        .filter(|item| item.current != item.original)
        .count() as u64;

    if !dry_run {
        for item in &pending_files {
            if item.current != item.original {
                fs::write(&item.resolved, item.current.as_bytes())
                    .with_context(|| format!("failed to write {}", item.resolved.display()))?;
            }
        }
    }

    Ok(json!({
        "results": results,
        "applied": !dry_run,
        "changed_files": changed_files,
        "total_replacements_applied": total_replacements_applied
    }))
}

pub fn apply_patch_file_contents(
    repo_root: &Path,
    patches: &[FilePatchRequest],
    dry_run: bool,
) -> Result<Value> {
    let mut pending_files = Vec::<PendingPatchedFile>::new();
    let mut results = Vec::with_capacity(patches.len());
    let mut total_hunks_applied = 0_u64;

    for patch in patches {
        let resolved = safe_resolve_path(repo_root, &patch.path)?;
        let rel_path = to_rel_path(repo_root, &resolved)?;
        let pending_idx = if let Some(idx) = pending_files
            .iter()
            .position(|item| item.resolved == resolved)
        {
            idx
        } else {
            let original_source = fs::read_to_string(&resolved)
                .with_context(|| format!("failed to read {}", resolved.display()))?;
            pending_files.push(PendingPatchedFile {
                resolved: resolved.clone(),
                original: LineBuffer::from_source(&original_source),
                current: LineBuffer::from_source(&original_source),
            });
            pending_files.len() - 1
        };

        let pending = &mut pending_files[pending_idx];
        let mut hunk_results = Vec::with_capacity(patch.hunks.len());
        let mut cumulative_line_delta = 0_i64;
        let mut last_start_line = 0_u64;

        for hunk in &patch.hunks {
            if hunk.start_line == 0 {
                return Err(anyhow!("patch hunks use 1-indexed line numbers"));
            }
            if hunk.start_line < last_start_line {
                return Err(anyhow!(
                    "patch hunks for `{}` must be sorted by start_line",
                    rel_path
                ));
            }
            last_start_line = hunk.start_line;

            let applied_hunk = apply_patch_hunk(&mut pending.current, hunk, cumulative_line_delta)?;
            cumulative_line_delta += applied_hunk.line_delta;
            total_hunks_applied += 1;
            hunk_results.push(json!({
                "start_line": applied_hunk.start_line,
                "applied_start_line": applied_hunk.applied_start_line,
                "old_line_count": applied_hunk.old_line_count,
                "new_line_count": applied_hunk.new_line_count,
                "line_delta": applied_hunk.line_delta,
                "diff_preview": applied_hunk.diff_preview
            }));
        }

        results.push(json!({
            "path": rel_path,
            "hunks_applied": hunk_results.len(),
            "line_delta": cumulative_line_delta,
            "hunks": hunk_results
        }));
    }

    let changed_files = pending_files
        .iter()
        .filter(|item| item.current != item.original)
        .count() as u64;

    if !dry_run {
        for item in &pending_files {
            if item.current != item.original {
                let updated = item.current.to_source();
                fs::write(&item.resolved, updated.as_bytes())
                    .with_context(|| format!("failed to write {}", item.resolved.display()))?;
            }
        }
    }

    Ok(json!({
        "results": results,
        "applied": !dry_run,
        "changed_files": changed_files,
        "total_hunks_applied": total_hunks_applied
    }))
}

pub fn multi_read(
    repo_root: &Path,
    reads: &[MultiReadRequest],
    max_total_lines: u64,
) -> Result<Value> {
    let mut prepared = Vec::new();
    for request in reads {
        let resolved = safe_resolve_path(repo_root, &request.path)?;
        let source = fs::read_to_string(&resolved)
            .with_context(|| format!("failed to read {}", resolved.display()))?;
        let lines: Vec<&str> = source.lines().collect();
        let total = lines.len() as u64;
        let range = compute_range(total, request.start_line, request.end_line);
        prepared.push(PreparedRead {
            resolved,
            source,
            start_line: range.0,
            end_line: range.1,
            requested_lines: range.2,
        });
    }

    let requested_total: u64 = prepared.iter().map(|item| item.requested_lines).sum();
    let budgets = allocate_budgets(&prepared, requested_total, max_total_lines);

    let mut results = Vec::new();
    let mut total_lines_returned = 0_u64;
    for (idx, item) in prepared.iter().enumerate() {
        let language = detect_language(&item.resolved).map(|lang| lang.as_str().to_string());
        let response = build_read_response(
            repo_root,
            &item.resolved,
            &item.source,
            Some(item.start_line),
            Some(item.end_line),
            budgets[idx],
            language,
        )?;
        total_lines_returned += response
            .get("end_line")
            .and_then(Value::as_u64)
            .unwrap_or(0)
            .saturating_sub(
                response
                    .get("start_line")
                    .and_then(Value::as_u64)
                    .unwrap_or(0),
            )
            + if response
                .get("start_line")
                .and_then(Value::as_u64)
                .unwrap_or(0)
                > 0
                && response
                    .get("end_line")
                    .and_then(Value::as_u64)
                    .unwrap_or(0)
                    >= response
                        .get("start_line")
                        .and_then(Value::as_u64)
                        .unwrap_or(0)
            {
                1
            } else {
                0
            };
        results.push(response);
    }

    Ok(json!({
        "results": results,
        "total_lines_returned": total_lines_returned
    }))
}

fn compute_range(
    total_lines: u64,
    start_line: Option<u64>,
    end_line: Option<u64>,
) -> (u64, u64, u64) {
    if total_lines == 0 {
        return (0, 0, 0);
    }

    let start = start_line.unwrap_or(1).max(1).min(total_lines);
    let end_requested = end_line.unwrap_or(total_lines).max(start);
    let end = end_requested.min(total_lines);
    let requested = end.saturating_sub(start) + 1;
    (start, end, requested)
}

fn build_read_response(
    repo_root: &Path,
    resolved: &Path,
    source: &str,
    start_line: Option<u64>,
    end_line: Option<u64>,
    max_lines: u64,
    language: Option<String>,
) -> Result<Value> {
    let lines: Vec<&str> = source.lines().collect();
    let total_lines = lines.len() as u64;

    if total_lines == 0 {
        return Ok(json!({
            "path": to_rel_path(repo_root, resolved)?,
            "content": "",
            "total_lines": 0,
            "start_line": 0,
            "end_line": 0,
            "returned_lines": 0,
            "truncated": false,
            "language": language
        }));
    }

    let (slice_start, _slice_end, requested_count) =
        compute_range(total_lines, start_line, end_line);
    let taken = requested_count.min(max_lines);
    let truncated = taken < requested_count;
    let final_end = if taken == 0 {
        slice_start.saturating_sub(1)
    } else {
        slice_start + taken - 1
    };

    let content = if taken == 0 {
        String::new()
    } else {
        lines[(slice_start - 1) as usize..final_end as usize]
            .join("\n")
            .to_string()
    };

    Ok(json!({
        "path": to_rel_path(repo_root, resolved)?,
        "content": content,
        "total_lines": total_lines,
        "start_line": slice_start,
        "end_line": final_end,
        "returned_lines": taken,
        "truncated": truncated,
        "language": language
    }))
}

fn allocate_budgets(
    prepared: &[PreparedRead],
    requested_total: u64,
    max_total_lines: u64,
) -> Vec<u64> {
    if requested_total == 0 {
        return vec![0; prepared.len()];
    }
    if requested_total <= max_total_lines {
        return prepared.iter().map(|item| item.requested_lines).collect();
    }

    let mut budgets = vec![0_u64; prepared.len()];
    let mut assigned = 0_u64;

    for (idx, item) in prepared.iter().enumerate() {
        if item.requested_lines == 0 {
            continue;
        }
        let proportional = (item.requested_lines * max_total_lines) / requested_total;
        let minimum = if max_total_lines > assigned { 1 } else { 0 };
        let budget = proportional.max(minimum).min(item.requested_lines);
        budgets[idx] = budget;
        assigned += budget;
    }

    if assigned > max_total_lines {
        let mut overflow = assigned - max_total_lines;
        for budget in &mut budgets {
            if overflow == 0 {
                break;
            }
            if *budget > 0 {
                *budget -= 1;
                overflow -= 1;
            }
        }
    }

    if assigned < max_total_lines {
        let mut remaining = max_total_lines - assigned;
        for (idx, item) in prepared.iter().enumerate() {
            if remaining == 0 {
                break;
            }
            let room = item.requested_lines.saturating_sub(budgets[idx]);
            if room == 0 {
                continue;
            }
            let add = room.min(remaining);
            budgets[idx] += add;
            remaining -= add;
        }
    }

    budgets
}

fn should_descend(entry: &DirEntry) -> bool {
    if !entry.file_type().is_dir() {
        return true;
    }
    let name = entry.file_name().to_str().unwrap_or_default().to_string();
    !IGNORE_DIRS.contains(&name.as_str())
}

fn glob_to_regex(glob: &str) -> Result<Regex> {
    let mut pattern = String::from("^");
    for ch in glob.replace('\\', "/").chars() {
        match ch {
            '*' => pattern.push_str(".*"),
            '?' => pattern.push('.'),
            '.' | '+' | '(' | ')' | '[' | ']' | '{' | '}' | '^' | '$' | '|' => {
                pattern.push('\\');
                pattern.push(ch);
            }
            _ => pattern.push(ch),
        }
    }
    pattern.push('$');
    Regex::new(&pattern).with_context(|| format!("invalid file_glob `{glob}`"))
}

fn push_dir_entry(
    repo_root: &Path,
    entry: &DirEntry,
    file_glob_regex: Option<&Regex>,
    entries: &mut Vec<Value>,
) -> Result<()> {
    let rel_path = to_rel_path(repo_root, entry.path())?;
    if let Some(glob_regex) = file_glob_regex {
        if entry.file_type().is_file() && !glob_regex.is_match(&rel_path) {
            return Ok(());
        }
    }

    let metadata = entry.metadata()?;
    let entry_type = if entry.file_type().is_dir() {
        "dir"
    } else {
        "file"
    };
    let size = if entry.file_type().is_file() {
        Some(metadata.len())
    } else {
        None
    };

    entries.push(json!({
        "name": entry.file_name().to_string_lossy().to_string(),
        "path": rel_path,
        "type": entry_type,
        "size": size
    }));
    Ok(())
}

fn prepare_parent_dirs(repo_root: &Path, user_path: &str) -> Result<()> {
    let joined = repo_root.join(user_path);
    let parent = joined
        .parent()
        .ok_or_else(|| anyhow!("invalid path: missing parent"))?;

    let base_canonical = fs::canonicalize(repo_root)?;
    let mut cursor = parent.to_path_buf();
    while !cursor.exists() {
        let Some(next) = cursor.parent() else {
            return Err(anyhow!("parent directory does not exist"));
        };
        cursor = next.to_path_buf();
    }

    let existing_canonical = fs::canonicalize(&cursor)?;
    if !existing_canonical.starts_with(&base_canonical) {
        return Err(anyhow!("path escapes repository root"));
    }

    fs::create_dir_all(parent)
        .with_context(|| format!("failed to create directory {}", parent.display()))
}

fn build_diff_preview(original: &str, updated: &str, old_text: &str) -> String {
    let Some(idx) = original.find(old_text) else {
        return String::new();
    };

    let line_num = original[..idx].bytes().filter(|b| *b == b'\n').count() + 1;
    let before_start = line_num.saturating_sub(2).max(1);
    let after_end = line_num + 2;

    let old_lines: Vec<&str> = original.lines().collect();
    let new_lines: Vec<&str> = updated.lines().collect();
    let old_snippet = old_lines
        .iter()
        .enumerate()
        .filter(|(idx, _)| {
            let n = idx + 1;
            n >= before_start && n <= after_end
        })
        .map(|(_, line)| *line)
        .collect::<Vec<_>>()
        .join("\n");
    let new_snippet = new_lines
        .iter()
        .enumerate()
        .filter(|(idx, _)| {
            let n = idx + 1;
            n >= before_start && n <= after_end
        })
        .map(|(_, line)| *line)
        .collect::<Vec<_>>()
        .join("\n");

    format!("--- old\n{old_snippet}\n+++ new\n{new_snippet}")
}

fn apply_text_edit(
    original: &str,
    old_text: &str,
    new_text: &str,
    replace_all: bool,
) -> Result<AppliedTextEdit> {
    if old_text.is_empty() {
        return Err(anyhow!("old_text must not be empty"));
    }

    let occurrences_found = original.matches(old_text).count() as u64;
    if occurrences_found == 0 {
        return Err(anyhow!("old_text not found in file"));
    }
    if !replace_all && occurrences_found > 1 {
        return Err(anyhow!(
            "old_text matches {} times; must match exactly once",
            occurrences_found
        ));
    }

    let replacements_applied = if replace_all { occurrences_found } else { 1 };
    let updated = if replace_all {
        original.replace(old_text, new_text)
    } else {
        original.replacen(old_text, new_text, 1)
    };

    Ok(AppliedTextEdit {
        updated: updated.clone(),
        occurrences_found,
        replacements_applied,
        diff_preview: build_diff_preview(original, &updated, old_text),
    })
}

fn apply_patch_hunk(
    buffer: &mut LineBuffer,
    hunk: &PatchHunkRequest,
    cumulative_line_delta: i64,
) -> Result<AppliedPatchHunk> {
    let adjusted_start = hunk.start_line as i64 - 1 + cumulative_line_delta;
    if adjusted_start < 0 {
        return Err(anyhow!(
            "patch hunk at line {} shifted before the start of the file",
            hunk.start_line
        ));
    }
    let adjusted_start = adjusted_start as usize;

    if hunk.old_lines.is_empty() {
        if adjusted_start > buffer.lines.len() {
            return Err(anyhow!(
                "patch hunk insertion at line {} is beyond the end of the file",
                hunk.start_line
            ));
        }
    } else {
        let adjusted_end = adjusted_start + hunk.old_lines.len();
        if adjusted_end > buffer.lines.len() {
            return Err(anyhow!(
                "patch hunk at line {} extends beyond the end of the file",
                hunk.start_line
            ));
        }

        let actual = &buffer.lines[adjusted_start..adjusted_end];
        if actual != hunk.old_lines.as_slice() {
            return Err(anyhow!(
                "patch hunk at line {} did not match file contents\nexpected:\n{}\nfound:\n{}",
                hunk.start_line,
                format_patch_lines(&hunk.old_lines),
                format_patch_lines(actual)
            ));
        }
    }

    buffer.lines.splice(
        adjusted_start..adjusted_start + hunk.old_lines.len(),
        hunk.new_lines.iter().cloned(),
    );

    Ok(AppliedPatchHunk {
        start_line: hunk.start_line,
        applied_start_line: adjusted_start as u64 + 1,
        old_line_count: hunk.old_lines.len() as u64,
        new_line_count: hunk.new_lines.len() as u64,
        line_delta: hunk.new_lines.len() as i64 - hunk.old_lines.len() as i64,
        diff_preview: build_patch_hunk_preview(hunk.start_line, &hunk.old_lines, &hunk.new_lines),
    })
}

fn build_patch_hunk_preview(start_line: u64, old_lines: &[String], new_lines: &[String]) -> String {
    format!(
        "@@ line {} @@\n--- old\n{}\n+++ new\n{}",
        start_line,
        format_patch_lines(old_lines),
        format_patch_lines(new_lines)
    )
}

fn format_patch_lines(lines: &[impl AsRef<str>]) -> String {
    if lines.is_empty() {
        "(empty)".to_string()
    } else {
        lines
            .iter()
            .map(|line| line.as_ref())
            .collect::<Vec<_>>()
            .join("\n")
    }
}

fn to_rel_path(repo_root: &Path, path: &Path) -> Result<String> {
    let rel = path.strip_prefix(repo_root).with_context(|| {
        format!(
            "failed to make path relative to repo root: {}",
            path.display()
        )
    })?;
    Ok(rel.to_string_lossy().replace('\\', "/"))
}

#[derive(Debug)]
struct PreparedRead {
    resolved: PathBuf,
    source: String,
    start_line: u64,
    end_line: u64,
    requested_lines: u64,
}

#[derive(Debug)]
struct PendingFileEdit {
    resolved: PathBuf,
    original: String,
    current: String,
}

#[derive(Debug)]
struct AppliedTextEdit {
    updated: String,
    occurrences_found: u64,
    replacements_applied: u64,
    diff_preview: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct LineBuffer {
    lines: Vec<String>,
    has_trailing_newline: bool,
}

impl LineBuffer {
    fn from_source(source: &str) -> Self {
        if source.is_empty() {
            return Self {
                lines: Vec::new(),
                has_trailing_newline: false,
            };
        }

        let has_trailing_newline = source.ends_with('\n');
        let mut lines = source.split('\n').map(str::to_string).collect::<Vec<_>>();
        if has_trailing_newline {
            lines.pop();
        }

        Self {
            lines,
            has_trailing_newline,
        }
    }

    fn to_source(&self) -> String {
        let mut source = self.lines.join("\n");
        if self.has_trailing_newline && !self.lines.is_empty() {
            source.push('\n');
        }
        source
    }
}

#[derive(Debug)]
struct PendingPatchedFile {
    resolved: PathBuf,
    original: LineBuffer,
    current: LineBuffer,
}

#[derive(Debug)]
struct AppliedPatchHunk {
    start_line: u64,
    applied_start_line: u64,
    old_line_count: u64,
    new_line_count: u64,
    line_delta: i64,
    diff_preview: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn setup_repo() -> TempDir {
        let dir = TempDir::new().expect("temp dir should be created");
        fs::create_dir_all(dir.path().join("src")).expect("src should be created");
        dir
    }

    #[test]
    fn test_safe_resolve_path_valid() {
        let dir = setup_repo();
        fs::write(dir.path().join("src/main.rs"), "fn main() {}\n")
            .expect("seed file should be written");
        let resolved = safe_resolve_path(dir.path(), "src/main.rs").expect("path should resolve");
        assert!(resolved.ends_with("src/main.rs"));
    }

    #[test]
    fn test_safe_resolve_path_traversal_blocked() {
        let dir = setup_repo();
        let outside = safe_resolve_path(dir.path(), "../outside.txt");
        assert!(outside.is_err(), "path traversal should fail");
    }

    #[test]
    fn test_safe_resolve_path_non_existing_path() {
        let dir = setup_repo();
        let resolved = safe_resolve_path(dir.path(), "src/new.rs")
            .expect("non-existing path with existing parent should resolve");
        assert!(resolved.ends_with("src/new.rs"));
    }

    #[test]
    fn test_read_file_contents_basic() {
        let dir = setup_repo();
        fs::write(dir.path().join("src/lib.rs"), "a\nb\nc\n").expect("file should be written");
        let value = read_file_contents(dir.path(), "src/lib.rs", None, None, 500)
            .expect("read should succeed");
        assert_eq!(value["total_lines"], 3);
        assert_eq!(value["content"], "a\nb\nc");
    }

    #[test]
    fn test_read_file_contents_line_range() {
        let dir = setup_repo();
        fs::write(dir.path().join("src/lib.rs"), "a\nb\nc\nd\n").expect("file should be written");
        let value = read_file_contents(dir.path(), "src/lib.rs", Some(2), Some(3), 500)
            .expect("read should succeed");
        assert_eq!(value["start_line"], 2);
        assert_eq!(value["end_line"], 3);
        assert_eq!(value["content"], "b\nc");
    }

    #[test]
    fn test_read_file_contents_truncation() {
        let dir = setup_repo();
        fs::write(dir.path().join("src/lib.rs"), "1\n2\n3\n4\n").expect("file should be written");
        let value = read_file_contents(dir.path(), "src/lib.rs", None, None, 2)
            .expect("read should succeed");
        assert_eq!(value["truncated"], true);
        assert_eq!(value["end_line"], 2);
    }

    #[test]
    fn test_file_outline_rust_file() {
        let dir = setup_repo();
        fs::write(
            dir.path().join("src/lib.rs"),
            "fn alpha() {}\nstruct Beta;\n",
        )
        .expect("file should be written");
        let value = file_outline(dir.path(), "src/lib.rs", None).expect("outline should succeed");
        let entries = value["entries"]
            .as_array()
            .expect("entries should be array");
        assert!(entries.len() >= 2, "expected at least two definitions");
    }

    #[test]
    fn test_file_outline_unsupported_file() {
        let dir = setup_repo();
        fs::write(dir.path().join("src/file.txt"), "hello").expect("file should be written");
        let value = file_outline(dir.path(), "src/file.txt", None).expect("outline should succeed");
        assert_eq!(value["path"], "src/file.txt");
        assert!(value["language"].is_null());
        assert_eq!(value["entries"].as_array().unwrap().len(), 0);
        assert_eq!(
            value["note"], "unsupported language for AST outline",
            "unsupported file should have note"
        );
    }

    #[test]
    fn test_search_in_files_literal() {
        let dir = setup_repo();
        fs::write(dir.path().join("src/a.rs"), "hello world\n").expect("file should be written");
        let value = search_in_files(dir.path(), "world", Some("*.rs"), 1, 10, false)
            .expect("search should succeed");
        assert_eq!(value["total_matches"], 1);
    }

    #[test]
    fn test_search_in_files_regex() {
        let dir = setup_repo();
        fs::write(dir.path().join("src/a.rs"), "foo123\n").expect("file should be written");
        let value = search_in_files(dir.path(), "foo\\d+", Some("*.rs"), 1, 10, true)
            .expect("search should succeed");
        assert_eq!(value["total_matches"], 1);
    }

    #[test]
    fn test_search_in_files_no_matches() {
        let dir = setup_repo();
        fs::write(dir.path().join("src/a.rs"), "abc\n").expect("file should be written");
        let value =
            search_in_files(dir.path(), "zzz", None, 1, 10, false).expect("search should succeed");
        assert_eq!(value["total_matches"], 0);
    }

    #[test]
    fn test_list_dir_non_recursive() {
        let dir = setup_repo();
        fs::write(dir.path().join("src/a.rs"), "a").expect("file should be written");
        fs::create_dir_all(dir.path().join("src/nested")).expect("nested dir should be created");
        let value = list_dir(dir.path(), "src", false, 3, None).expect("list should succeed");
        let entries = value["entries"]
            .as_array()
            .expect("entries should be array");
        assert_eq!(entries.len(), 2);
    }

    #[test]
    fn test_list_dir_recursive() {
        let dir = setup_repo();
        fs::create_dir_all(dir.path().join("src/nested")).expect("nested dir should be created");
        fs::write(dir.path().join("src/nested/a.rs"), "x").expect("file should be written");
        let value =
            list_dir(dir.path(), "src", true, 3, Some("*.rs")).expect("list should succeed");
        let entries = value["entries"]
            .as_array()
            .expect("entries should be array");
        assert!(
            entries.iter().any(|item| item["path"] == "src/nested/a.rs"),
            "recursive list should include nested file"
        );
    }

    #[test]
    fn test_write_file_contents_create_new() {
        let dir = setup_repo();
        let value = write_file_contents(dir.path(), "src/new.rs", "fn a() {}", false)
            .expect("write should succeed");
        assert_eq!(value["bytes_written"], 9);
        assert!(dir.path().join("src/new.rs").exists());
    }

    #[test]
    fn test_write_file_contents_create_with_dirs() {
        let dir = setup_repo();
        let value = write_file_contents(dir.path(), "nested/deep/file.txt", "ok", true)
            .expect("write with dirs should succeed");
        assert_eq!(value["bytes_written"], 2);
        assert!(dir.path().join("nested/deep/file.txt").exists());
    }

    #[test]
    fn test_edit_file_contents_successful_edit() {
        let dir = setup_repo();
        fs::write(dir.path().join("src/edit.rs"), "let a = 1;\n").expect("file should be written");
        let value = edit_file_contents(dir.path(), "src/edit.rs", "1", "2", false)
            .expect("edit should succeed");
        assert_eq!(value["applied"], true);
        assert_eq!(value["replacements_applied"], 1);
        assert_eq!(
            fs::read_to_string(dir.path().join("src/edit.rs")).expect("file should be readable"),
            "let a = 2;\n"
        );
    }

    #[test]
    fn test_edit_file_contents_zero_matches_error() {
        let dir = setup_repo();
        fs::write(dir.path().join("src/edit.rs"), "abc\n").expect("file should be written");
        let result = edit_file_contents(dir.path(), "src/edit.rs", "missing", "x", false);
        assert!(result.is_err(), "zero matches should fail");
    }

    #[test]
    fn test_edit_file_contents_multiple_matches_error() {
        let dir = setup_repo();
        fs::write(dir.path().join("src/edit.rs"), "x x\n").expect("file should be written");
        let result = edit_file_contents(dir.path(), "src/edit.rs", "x", "y", false);
        assert!(result.is_err(), "multiple matches should fail");
    }

    #[test]
    fn test_edit_file_contents_dry_run() {
        let dir = setup_repo();
        fs::write(dir.path().join("src/edit.rs"), "hello\n").expect("file should be written");
        let value = edit_file_contents(dir.path(), "src/edit.rs", "hello", "bye", true)
            .expect("dry run should succeed");
        assert_eq!(value["applied"], false);
        assert_eq!(value["replacements_applied"], 1);
        assert_eq!(
            fs::read_to_string(dir.path().join("src/edit.rs")).expect("file should be readable"),
            "hello\n"
        );
    }

    #[test]
    fn test_batch_edit_file_contents_across_files() {
        let dir = setup_repo();
        fs::write(dir.path().join("src/a.rs"), "let a = 1;\n").expect("file should be written");
        fs::write(dir.path().join("src/b.rs"), "let b = 2;\n").expect("file should be written");
        let edits = vec![
            BatchEditRequest {
                path: "src/a.rs".to_string(),
                old_text: "1".to_string(),
                new_text: "10".to_string(),
                replace_all: false,
            },
            BatchEditRequest {
                path: "src/b.rs".to_string(),
                old_text: "2".to_string(),
                new_text: "20".to_string(),
                replace_all: false,
            },
        ];

        let value =
            batch_edit_file_contents(dir.path(), &edits, false).expect("batch edit should succeed");
        assert_eq!(value["applied"], true);
        assert_eq!(value["changed_files"], 2);
        assert_eq!(value["total_replacements_applied"], 2);
        assert_eq!(
            fs::read_to_string(dir.path().join("src/a.rs")).expect("file should be readable"),
            "let a = 10;\n"
        );
        assert_eq!(
            fs::read_to_string(dir.path().join("src/b.rs")).expect("file should be readable"),
            "let b = 20;\n"
        );
    }

    #[test]
    fn test_batch_edit_file_contents_same_file_ordered() {
        let dir = setup_repo();
        fs::write(dir.path().join("src/edit.rs"), "alpha beta\n").expect("file should be written");
        let edits = vec![
            BatchEditRequest {
                path: "src/edit.rs".to_string(),
                old_text: "alpha".to_string(),
                new_text: "gamma".to_string(),
                replace_all: false,
            },
            BatchEditRequest {
                path: "src/edit.rs".to_string(),
                old_text: "gamma beta".to_string(),
                new_text: "delta".to_string(),
                replace_all: false,
            },
        ];

        let value =
            batch_edit_file_contents(dir.path(), &edits, false).expect("batch edit should succeed");
        assert_eq!(value["changed_files"], 1);
        assert_eq!(value["total_replacements_applied"], 2);
        assert_eq!(
            fs::read_to_string(dir.path().join("src/edit.rs")).expect("file should be readable"),
            "delta\n"
        );
    }

    #[test]
    fn test_batch_edit_file_contents_replace_all() {
        let dir = setup_repo();
        fs::write(dir.path().join("src/edit.rs"), "x x x\n").expect("file should be written");
        let edits = vec![BatchEditRequest {
            path: "src/edit.rs".to_string(),
            old_text: "x".to_string(),
            new_text: "y".to_string(),
            replace_all: true,
        }];

        let value =
            batch_edit_file_contents(dir.path(), &edits, false).expect("batch edit should succeed");
        assert_eq!(value["total_replacements_applied"], 3);
        assert_eq!(
            fs::read_to_string(dir.path().join("src/edit.rs")).expect("file should be readable"),
            "y y y\n"
        );
    }

    #[test]
    fn test_batch_edit_file_contents_is_atomic_on_failure() {
        let dir = setup_repo();
        fs::write(dir.path().join("src/a.rs"), "let a = 1;\n").expect("file should be written");
        fs::write(dir.path().join("src/b.rs"), "let b = 2;\n").expect("file should be written");
        let edits = vec![
            BatchEditRequest {
                path: "src/a.rs".to_string(),
                old_text: "1".to_string(),
                new_text: "10".to_string(),
                replace_all: false,
            },
            BatchEditRequest {
                path: "src/b.rs".to_string(),
                old_text: "missing".to_string(),
                new_text: "20".to_string(),
                replace_all: false,
            },
        ];

        let result = batch_edit_file_contents(dir.path(), &edits, false);
        assert!(result.is_err(), "batch edit should fail");
        assert_eq!(
            fs::read_to_string(dir.path().join("src/a.rs")).expect("file should be readable"),
            "let a = 1;\n"
        );
        assert_eq!(
            fs::read_to_string(dir.path().join("src/b.rs")).expect("file should be readable"),
            "let b = 2;\n"
        );
    }

    #[test]
    fn test_apply_patch_file_contents_replace_insert_and_delete() {
        let dir = setup_repo();
        fs::write(dir.path().join("src/edit.rs"), "one\ntwo\nthree\n")
            .expect("file should be written");
        let patches = vec![FilePatchRequest {
            path: "src/edit.rs".to_string(),
            hunks: vec![
                PatchHunkRequest {
                    start_line: 1,
                    old_lines: vec![],
                    new_lines: vec!["zero".to_string()],
                },
                PatchHunkRequest {
                    start_line: 2,
                    old_lines: vec!["two".to_string()],
                    new_lines: vec!["TWO".to_string()],
                },
                PatchHunkRequest {
                    start_line: 3,
                    old_lines: vec!["three".to_string()],
                    new_lines: vec![],
                },
            ],
        }];

        let value =
            apply_patch_file_contents(dir.path(), &patches, false).expect("patch should succeed");
        assert_eq!(value["applied"], true);
        assert_eq!(value["changed_files"], 1);
        assert_eq!(value["total_hunks_applied"], 3);
        assert_eq!(
            fs::read_to_string(dir.path().join("src/edit.rs")).expect("file should be readable"),
            "zero\none\nTWO\n"
        );
    }

    #[test]
    fn test_apply_patch_file_contents_across_files() {
        let dir = setup_repo();
        fs::write(dir.path().join("src/a.rs"), "alpha\n").expect("file should be written");
        fs::write(dir.path().join("src/b.rs"), "beta\n").expect("file should be written");
        let patches = vec![
            FilePatchRequest {
                path: "src/a.rs".to_string(),
                hunks: vec![PatchHunkRequest {
                    start_line: 1,
                    old_lines: vec!["alpha".to_string()],
                    new_lines: vec!["ALPHA".to_string()],
                }],
            },
            FilePatchRequest {
                path: "src/b.rs".to_string(),
                hunks: vec![PatchHunkRequest {
                    start_line: 1,
                    old_lines: vec!["beta".to_string()],
                    new_lines: vec!["BETA".to_string()],
                }],
            },
        ];

        let value =
            apply_patch_file_contents(dir.path(), &patches, false).expect("patch should succeed");
        assert_eq!(value["changed_files"], 2);
        assert_eq!(value["total_hunks_applied"], 2);
    }

    #[test]
    fn test_apply_patch_file_contents_dry_run() {
        let dir = setup_repo();
        fs::write(dir.path().join("src/edit.rs"), "hello\n").expect("file should be written");
        let patches = vec![FilePatchRequest {
            path: "src/edit.rs".to_string(),
            hunks: vec![PatchHunkRequest {
                start_line: 1,
                old_lines: vec!["hello".to_string()],
                new_lines: vec!["bye".to_string()],
            }],
        }];

        let value =
            apply_patch_file_contents(dir.path(), &patches, true).expect("dry run should succeed");
        assert_eq!(value["applied"], false);
        assert_eq!(
            fs::read_to_string(dir.path().join("src/edit.rs")).expect("file should be readable"),
            "hello\n"
        );
    }

    #[test]
    fn test_apply_patch_file_contents_is_atomic_on_failure() {
        let dir = setup_repo();
        fs::write(dir.path().join("src/a.rs"), "alpha\n").expect("file should be written");
        fs::write(dir.path().join("src/b.rs"), "beta\n").expect("file should be written");
        let patches = vec![
            FilePatchRequest {
                path: "src/a.rs".to_string(),
                hunks: vec![PatchHunkRequest {
                    start_line: 1,
                    old_lines: vec!["alpha".to_string()],
                    new_lines: vec!["ALPHA".to_string()],
                }],
            },
            FilePatchRequest {
                path: "src/b.rs".to_string(),
                hunks: vec![PatchHunkRequest {
                    start_line: 1,
                    old_lines: vec!["wrong".to_string()],
                    new_lines: vec!["BETA".to_string()],
                }],
            },
        ];

        let result = apply_patch_file_contents(dir.path(), &patches, false);
        assert!(result.is_err(), "patch should fail");
        assert_eq!(
            fs::read_to_string(dir.path().join("src/a.rs")).expect("file should be readable"),
            "alpha\n"
        );
        assert_eq!(
            fs::read_to_string(dir.path().join("src/b.rs")).expect("file should be readable"),
            "beta\n"
        );
    }

    #[test]
    fn test_apply_patch_file_contents_unsorted_hunks_error() {
        let dir = setup_repo();
        fs::write(dir.path().join("src/edit.rs"), "one\ntwo\n").expect("file should be written");
        let patches = vec![FilePatchRequest {
            path: "src/edit.rs".to_string(),
            hunks: vec![
                PatchHunkRequest {
                    start_line: 2,
                    old_lines: vec!["two".to_string()],
                    new_lines: vec!["TWO".to_string()],
                },
                PatchHunkRequest {
                    start_line: 1,
                    old_lines: vec!["one".to_string()],
                    new_lines: vec!["ONE".to_string()],
                },
            ],
        }];

        let result = apply_patch_file_contents(dir.path(), &patches, false);
        assert!(result.is_err(), "unsorted hunks should fail");
    }

    #[test]
    fn test_multi_read_basic() {
        let dir = setup_repo();
        fs::write(dir.path().join("src/a.rs"), "a1\na2\n").expect("file should be written");
        fs::write(dir.path().join("src/b.rs"), "b1\nb2\n").expect("file should be written");
        let requests = vec![
            MultiReadRequest {
                path: "src/a.rs".to_string(),
                start_line: None,
                end_line: None,
            },
            MultiReadRequest {
                path: "src/b.rs".to_string(),
                start_line: Some(1),
                end_line: Some(1),
            },
        ];
        let value = multi_read(dir.path(), &requests, 10).expect("multi read should succeed");
        assert_eq!(value["results"].as_array().unwrap().len(), 2);
        assert!(
            value["total_lines_returned"].as_u64().unwrap() >= 3,
            "should return expected lines"
        );
    }

    #[test]
    fn test_multi_outline_basic() {
        let dir = setup_repo();
        fs::write(
            dir.path().join("src/lib.rs"),
            "fn alpha() {}\nstruct Beta;\n",
        )
        .expect("file should be written");
        fs::write(dir.path().join("src/file.txt"), "hello\n").expect("file should be written");
        let requests = vec![
            MultiOutlineRequest {
                path: "src/lib.rs".to_string(),
                max_depth: None,
            },
            MultiOutlineRequest {
                path: "src/file.txt".to_string(),
                max_depth: None,
            },
        ];

        let value = multi_outline(dir.path(), &requests).expect("multi outline should succeed");
        let results = value["results"]
            .as_array()
            .expect("results should be array");
        assert_eq!(results.len(), 2);
        assert_eq!(value["unsupported_files"], 1);
        assert!(
            value["total_entries"].as_u64().unwrap() >= 2,
            "should count outlined definitions"
        );
        assert_eq!(results[0]["path"], "src/lib.rs");
        assert_eq!(results[1]["path"], "src/file.txt");
    }

    #[test]
    fn test_move_and_delete_file_ops() {
        let dir = setup_repo();
        fs::write(dir.path().join("src/from.rs"), "x").expect("file should be written");
        let moved =
            move_file_op(dir.path(), "src/from.rs", "src/to.rs").expect("move should succeed");
        assert_eq!(moved["destination"], "src/to.rs");
        assert!(dir.path().join("src/to.rs").exists());

        let deleted = delete_file_op(dir.path(), "src/to.rs").expect("delete should succeed");
        assert_eq!(deleted["deleted"], true);
        assert!(!dir.path().join("src/to.rs").exists());
    }
}
