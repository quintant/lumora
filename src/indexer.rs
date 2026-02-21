use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::Serialize;
use sha2::{Digest, Sha256};
use walkdir::WalkDir;

use crate::model::{FileExtraction, LanguageKind};
use crate::parser::{detect_language, parse_file};
use crate::paths::STATE_DIR_NAME;
use crate::storage::{GraphStore, UpsertOutcome};

const INDEXABLE_CONFIG_FILES: &[&str] =
    &["Cargo.toml", "pyproject.toml", "setup.cfg", "package.json"];
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
    STATE_DIR_NAME,
];

#[derive(Debug, Clone)]
pub struct IndexOptions {
    pub full: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct IndexReport {
    pub repo_root: String,
    pub indexed_files: usize,
    pub skipped_files: usize,
    pub removed_files: usize,
    pub parse_failures: usize,
    pub errors: Vec<String>,
}

pub fn index_repository(
    store: &mut GraphStore,
    repo_root: &Path,
    options: IndexOptions,
) -> Result<IndexReport> {
    let mut outcome = UpsertOutcome::new();
    let mut errors = Vec::new();

    let files = discover_files(repo_root)?;
    let current_paths: HashSet<String> = files.iter().map(|item| item.rel_path.clone()).collect();

    let tracked = store.tracked_files()?;
    let mut removed: Vec<String> = if options.full {
        tracked.iter().cloned().collect()
    } else {
        tracked
            .iter()
            .filter(|old_path| !current_paths.contains(*old_path))
            .cloned()
            .collect()
    };
    removed.sort();

    if !removed.is_empty() {
        store.remove_files(&removed, &mut outcome)?;
    }

    for file in files {
        let content = match fs::read_to_string(&file.abs_path) {
            Ok(content) => content,
            Err(err) => {
                errors.push(format!("{}: failed to read file: {err}", file.rel_path));
                continue;
            }
        };

        let hash = sha256_hex(content.as_bytes());
        if !options.full {
            if let Some(existing_hash) = store.tracked_file_hash(&file.rel_path)? {
                if existing_hash == hash {
                    outcome.skipped += 1;
                    continue;
                }
            }
        }

        let extraction = match file.kind {
            FileKind::Source(_language) => match parse_file(&file.abs_path, &content) {
                Ok(Some(extraction)) => extraction,
                Ok(None) => {
                    outcome.skipped += 1;
                    continue;
                }
                Err(err) => {
                    errors.push(format!("{}: parse failed: {err}", file.rel_path));
                    continue;
                }
            },
            FileKind::Config(language) => FileExtraction {
                language,
                definitions: Vec::new(),
                references: Vec::new(),
                imports: Vec::new(),
            },
        };

        let resolved_imports = resolve_imports(
            repo_root,
            &file.rel_path,
            extraction.language,
            &extraction.imports,
        );
        let fingerprints = build_winnowed_fingerprints(&content, 5, 4);

        if let Err(err) = store.index_file(
            &file.rel_path,
            extraction.language.as_str(),
            &hash,
            content.len() as u64,
            &extraction,
            &fingerprints,
            &resolved_imports,
            &mut outcome,
        ) {
            errors.push(format!("{}: index write failed: {err}", file.rel_path));
        }
    }

    Ok(IndexReport {
        repo_root: normalize_rel_path(repo_root),
        indexed_files: outcome.updated,
        skipped_files: outcome.skipped,
        removed_files: outcome.removed,
        parse_failures: errors
            .iter()
            .filter(|msg| msg.contains("parse failed"))
            .count(),
        errors,
    })
}

#[derive(Debug, Clone)]
struct CandidateFile {
    abs_path: PathBuf,
    rel_path: String,
    kind: FileKind,
}

#[derive(Debug, Clone, Copy)]
enum FileKind {
    Source(LanguageKind),
    Config(LanguageKind),
}

fn discover_files(repo_root: &Path) -> Result<Vec<CandidateFile>> {
    let mut files = Vec::new();

    let walker = WalkDir::new(repo_root).into_iter().filter_entry(|entry| {
        let path = entry.path();
        let name = path
            .file_name()
            .and_then(|part| part.to_str())
            .unwrap_or_default();
        if path.is_dir() && IGNORE_DIRS.contains(&name) {
            return false;
        }
        true
    });

    for entry in walker {
        let entry = entry?;
        if !entry.file_type().is_file() {
            continue;
        }
        let abs_path = entry.into_path();
        let rel = abs_path
            .strip_prefix(repo_root)
            .with_context(|| format!("failed to strip repo prefix for {}", abs_path.display()))?;
        let rel_path = normalize_rel_path(rel);

        if let Some(lang) = detect_language(&abs_path) {
            files.push(CandidateFile {
                abs_path,
                rel_path,
                kind: FileKind::Source(lang),
            });
            continue;
        }

        let file_name = abs_path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or_default()
            .to_string();
        if INDEXABLE_CONFIG_FILES.contains(&file_name.as_str()) {
            files.push(CandidateFile {
                abs_path,
                rel_path,
                kind: FileKind::Config(config_language_hint(&file_name)),
            });
        }
    }

    files.sort_by(|left, right| left.rel_path.cmp(&right.rel_path));
    Ok(files)
}

fn config_language_hint(file_name: &str) -> LanguageKind {
    match file_name {
        "Cargo.toml" => LanguageKind::Rust,
        _ => LanguageKind::Python,
    }
}

fn resolve_imports(
    repo_root: &Path,
    rel_path: &str,
    language: LanguageKind,
    imports: &[crate::model::Import],
) -> Vec<(String, String)> {
    let mut out = Vec::new();

    for import_item in imports {
        if let Some(resolved) =
            resolve_single_import(repo_root, rel_path, language, &import_item.module)
        {
            out.push((import_item.module.clone(), resolved));
        }
    }

    out
}

fn resolve_single_import(
    repo_root: &Path,
    rel_path: &str,
    language: LanguageKind,
    module: &str,
) -> Option<String> {
    let normalized_module = module.trim().trim_matches('{').trim_matches('}');
    if normalized_module.is_empty() {
        return None;
    }

    let candidates: Vec<PathBuf> = match language {
        LanguageKind::Rust => {
            let module_path = normalized_module
                .trim_start_matches("crate::")
                .trim_start_matches("self::")
                .trim_start_matches("super::")
                .replace("::", "/");
            if module_path.is_empty() {
                return None;
            }
            vec![
                PathBuf::from("src").join(format!("{module_path}.rs")),
                PathBuf::from("src").join(module_path).join("mod.rs"),
            ]
        }
        LanguageKind::Python => {
            let module_path = normalized_module.replace('.', "/");
            if module_path.is_empty() {
                return None;
            }
            vec![
                PathBuf::from(format!("{module_path}.py")),
                PathBuf::from(module_path).join("__init__.py"),
            ]
        }
    };

    for candidate in candidates {
        let absolute = repo_root.join(&candidate);
        if absolute.exists() {
            return Some(normalize_rel_path(candidate));
        }
    }

    // Rust relative modules often map to sibling files.
    if matches!(language, LanguageKind::Rust) {
        let base_dir = Path::new(rel_path)
            .parent()
            .unwrap_or_else(|| Path::new(""));
        let tail = normalized_module.rsplit("::").next().unwrap_or_default();
        let sibling = base_dir.join(format!("{tail}.rs"));
        let absolute = repo_root.join(&sibling);
        if absolute.exists() {
            return Some(normalize_rel_path(sibling));
        }
    }

    None
}

fn build_winnowed_fingerprints(content: &str, k: usize, window: usize) -> Vec<(i64, i64, i64)> {
    let tokens = tokenize(content);
    if tokens.len() < k || k == 0 || window == 0 {
        return Vec::new();
    }

    let mut kgrams = Vec::new();
    for i in 0..=(tokens.len() - k) {
        let gram = tokens[i..i + k].join(" ");
        let hash = stable_i64_hash(gram.as_bytes());
        kgrams.push((hash, i as i64, (i + k) as i64));
    }

    if kgrams.len() <= window {
        return kgrams;
    }

    let mut selected = Vec::new();
    let mut selected_set = HashSet::new();
    for i in 0..=(kgrams.len() - window) {
        let mut min_item = kgrams[i];
        for item in &kgrams[i..i + window] {
            if item.0 < min_item.0 {
                min_item = *item;
            }
        }
        if selected_set.insert((min_item.0, min_item.1)) {
            selected.push(min_item);
        }
    }

    selected
}

fn tokenize(content: &str) -> Vec<String> {
    content
        .split(|ch: char| !(ch.is_ascii_alphanumeric() || ch == '_'))
        .filter(|part| !part.is_empty())
        .map(|part| part.to_ascii_lowercase())
        .collect()
}

fn stable_i64_hash(bytes: &[u8]) -> i64 {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    let digest = hasher.finalize();
    i64::from_be_bytes([
        digest[0], digest[1], digest[2], digest[3], digest[4], digest[5], digest[6], digest[7],
    ])
}

fn sha256_hex(content: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(content);
    let digest = hasher.finalize();
    format!("{digest:x}")
}

fn normalize_rel_path(path: impl AsRef<Path>) -> String {
    path.as_ref().to_string_lossy().replace('\\', "/")
}
