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

    let Some(parsed) = parse_file(&resolved, &source)? else {
        return Ok(json!({
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
        "path": to_rel_path(repo_root, &resolved)?,
        "language": parsed.language.as_str(),
        "entries": entries
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
    if old_text.is_empty() {
        return Err(anyhow!("old_text must not be empty"));
    }

    let resolved = safe_resolve_path(repo_root, path)?;
    let original = fs::read_to_string(&resolved)
        .with_context(|| format!("failed to read {}", resolved.display()))?;

    let occurrences = original.matches(old_text).count();
    if occurrences == 0 {
        return Err(anyhow!("old_text not found in file"));
    }
    if occurrences > 1 {
        return Err(anyhow!(
            "old_text matches {} times; must match exactly once",
            occurrences
        ));
    }

    let updated = original.replacen(old_text, new_text, 1);
    if !dry_run {
        fs::write(&resolved, updated.as_bytes())
            .with_context(|| format!("failed to write {}", resolved.display()))?;
    }

    let preview = build_diff_preview(&original, &updated, old_text);
    Ok(json!({
        "path": to_rel_path(repo_root, &resolved)?,
        "applied": !dry_run,
        "occurrences_found": occurrences,
        "diff_preview": preview
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
        assert_eq!(
            fs::read_to_string(dir.path().join("src/edit.rs")).expect("file should be readable"),
            "hello\n"
        );
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
