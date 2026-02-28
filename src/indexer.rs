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

const INDEXABLE_CONFIG_FILES: &[&str] = &[
    "Cargo.toml",
    "pyproject.toml",
    "setup.cfg",
    "package.json",
    "tsconfig.json",
    "go.mod",
    "build.gradle",
    "build.gradle.kts",
    "pom.xml",
    "composer.json",
    "Gemfile",
    "renv.lock",
    "requirements.txt",
    "Pipfile",
];
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
            continue;
        }

        if let Some(lang) = detect_language(&abs_path) {
            files.push(CandidateFile {
                abs_path,
                rel_path,
                kind: FileKind::Source(lang),
            });
        }
    }

    files.sort_by(|left, right| left.rel_path.cmp(&right.rel_path));
    Ok(files)
}

fn config_language_hint(file_name: &str) -> LanguageKind {
    match file_name {
        "Cargo.toml" => LanguageKind::Rust,
        "pyproject.toml" | "setup.cfg" | "requirements.txt" | "Pipfile" => LanguageKind::Python,
        "package.json" => LanguageKind::JavaScript,
        "tsconfig.json" => LanguageKind::TypeScript,
        "go.mod" => LanguageKind::Go,
        "build.gradle" | "build.gradle.kts" => LanguageKind::Kotlin,
        "pom.xml" => LanguageKind::Java,
        "Gemfile" => LanguageKind::Ruby,
        "composer.json" => LanguageKind::Json,
        _ => LanguageKind::Json,
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
        LanguageKind::Rust => rust_import_candidates(normalized_module),
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
        _ => return None,
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

fn rust_import_candidates(raw_module: &str) -> Vec<PathBuf> {
    // Keep only the stable module prefix from `use` shapes like:
    // - crate::mcp::run_mcp_stdio
    // - crate::indexer::{index_repository, IndexOptions}
    // - crate::storage::GraphStore as Store
    let mut module = raw_module.trim();

    if let Some((prefix, _)) = module.split_once('{') {
        module = prefix;
    }
    if let Some((prefix, _)) = module.split_once(" as ") {
        module = prefix;
    }
    module = module.trim().trim_end_matches("::");

    let mut parts: Vec<&str> = module
        .split("::")
        .map(str::trim)
        .filter(|part| !part.is_empty())
        .collect();
    while matches!(parts.first().copied(), Some("crate" | "self" | "super")) {
        parts.remove(0);
    }

    if parts.is_empty() {
        return Vec::new();
    }

    let mut out = Vec::new();
    let mut seen = HashSet::new();

    // Try increasingly broader prefixes so symbol imports still resolve to module files.
    for len in (1..=parts.len()).rev() {
        let prefix = parts[..len].join("/");
        let file_candidate = PathBuf::from("src").join(format!("{prefix}.rs"));
        let mod_candidate = PathBuf::from("src").join(prefix).join("mod.rs");

        if seen.insert(file_candidate.clone()) {
            out.push(file_candidate);
        }
        if seen.insert(mod_candidate.clone()) {
            out.push(mod_candidate);
        }
    }

    out
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

#[cfg(test)]
mod tests {
    use super::*;

    use std::collections::BTreeSet;

    use crate::model::Import;
    use crate::storage::GraphStore;

    fn setup_test_repo() -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::TempDir::new().unwrap();
        let repo = dir.path().to_path_buf();
        // Create .git dir so discover_repo_root works
        std::fs::create_dir(repo.join(".git")).unwrap();
        std::fs::create_dir_all(repo.join("src")).unwrap();
        (dir, repo)
    }

    fn write_file(path: &Path, content: &str) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(path, content).unwrap();
    }

    fn open_test_store(repo: &Path) -> GraphStore {
        GraphStore::open(&repo.join("graph.db")).unwrap()
    }

    #[test]
    fn index_repository_basic_indexes_one_file() {
        let (_dir, repo) = setup_test_repo();
        write_file(&repo.join("src/lib.rs"), "pub fn greet() {}\n");

        let mut store = open_test_store(&repo);
        let report = index_repository(&mut store, &repo, IndexOptions { full: false }).unwrap();

        assert_eq!(report.indexed_files, 1);
        assert_eq!(report.skipped_files, 0);
        assert_eq!(report.removed_files, 0);
    }

    #[test]
    fn index_repository_incremental_skips_unchanged_file() {
        let (_dir, repo) = setup_test_repo();
        write_file(&repo.join("src/lib.rs"), "pub fn greet() {}\n");

        let mut store = open_test_store(&repo);
        let first = index_repository(&mut store, &repo, IndexOptions { full: false }).unwrap();
        let second = index_repository(&mut store, &repo, IndexOptions { full: false }).unwrap();

        assert_eq!(first.indexed_files, 1);
        assert_eq!(second.indexed_files, 0);
        assert_eq!(second.skipped_files, 1);
        assert_eq!(second.removed_files, 0);
    }

    #[test]
    fn index_repository_full_rebuild_reindexes_without_skips() {
        let (_dir, repo) = setup_test_repo();
        write_file(&repo.join("src/lib.rs"), "pub fn greet() {}\n");

        let mut store = open_test_store(&repo);
        let _ = index_repository(&mut store, &repo, IndexOptions { full: false }).unwrap();
        let rebuild = index_repository(&mut store, &repo, IndexOptions { full: true }).unwrap();

        assert_eq!(rebuild.indexed_files, 1);
        assert_eq!(rebuild.skipped_files, 0);
    }

    #[test]
    fn index_repository_removes_stale_files() {
        let (_dir, repo) = setup_test_repo();
        let file = repo.join("src/lib.rs");
        write_file(&file, "pub fn greet() {}\n");

        let mut store = open_test_store(&repo);
        let _ = index_repository(&mut store, &repo, IndexOptions { full: false }).unwrap();

        std::fs::remove_file(&file).unwrap();
        let report = index_repository(&mut store, &repo, IndexOptions { full: false }).unwrap();

        assert_eq!(report.removed_files, 1);
    }

    #[test]
    fn file_discovery_respects_ignore_dirs() {
        let (_dir, repo) = setup_test_repo();
        write_file(&repo.join("target/foo.rs"), "pub fn ignored() {}\n");
        write_file(&repo.join("node_modules/bar.py"), "print('ignored')\n");
        write_file(&repo.join(".git/thing.rs"), "pub fn ignored() {}\n");

        let files = discover_files(&repo).unwrap();
        assert!(files.is_empty());
    }

    #[test]
    fn file_discovery_finds_config_files() {
        let (_dir, repo) = setup_test_repo();
        write_file(
            &repo.join("Cargo.toml"),
            "[package]\nname = \"x\"\nversion = \"0.1.0\"\n",
        );
        write_file(&repo.join("pyproject.toml"), "[project]\nname = \"x\"\n");
        write_file(&repo.join("package.json"), "{\"name\":\"x\"}\n");

        let files = discover_files(&repo).unwrap();
        let rel_paths: BTreeSet<String> = files.iter().map(|item| item.rel_path.clone()).collect();
        assert_eq!(
            rel_paths,
            BTreeSet::from([
                "Cargo.toml".to_string(),
                "package.json".to_string(),
                "pyproject.toml".to_string(),
            ])
        );

        let cargo = files
            .iter()
            .find(|item| item.rel_path == "Cargo.toml")
            .unwrap();
        let pyproject = files
            .iter()
            .find(|item| item.rel_path == "pyproject.toml")
            .unwrap();
        let package_json = files
            .iter()
            .find(|item| item.rel_path == "package.json")
            .unwrap();

        assert!(matches!(cargo.kind, FileKind::Config(LanguageKind::Rust)));
        assert!(matches!(
            pyproject.kind,
            FileKind::Config(LanguageKind::Python)
        ));
        assert!(matches!(
            package_json.kind,
            FileKind::Config(LanguageKind::JavaScript)
        ));
    }

    #[test]
    fn file_discovery_finds_rs_and_py_sources() {
        let (_dir, repo) = setup_test_repo();
        write_file(&repo.join("src/lib.rs"), "pub fn r() {}\n");
        write_file(&repo.join("src/mod.py"), "def p():\n    return 1\n");

        let files = discover_files(&repo).unwrap();
        let rel_paths: BTreeSet<String> = files.iter().map(|item| item.rel_path.clone()).collect();

        assert_eq!(
            rel_paths,
            BTreeSet::from(["src/lib.rs".to_string(), "src/mod.py".to_string()])
        );
    }

    #[test]
    fn build_winnowed_fingerprints_produces_non_empty_tuples() {
        let content = "fn main() { let alpha = 1; let beta = alpha + 2; println!(\"{}\", beta); }";
        let fps = build_winnowed_fingerprints(content, 5, 4);
        let token_count = tokenize(content).len() as i64;

        assert!(!fps.is_empty());
        for (hash, start, end) in fps {
            assert_ne!(hash, 0);
            assert!(start >= 0);
            assert!(end > start);
            assert!(end <= token_count);
        }
    }

    #[test]
    fn build_winnowed_fingerprints_empty_content_returns_empty_vec() {
        let fps = build_winnowed_fingerprints("", 5, 4);
        assert!(fps.is_empty());
    }

    #[test]
    fn build_winnowed_fingerprints_short_content_returns_empty_vec() {
        let fps = build_winnowed_fingerprints("short tokens", 5, 4);
        assert!(fps.is_empty());
    }

    #[test]
    fn import_resolution_for_rust_resolves_to_src_storage_rs() {
        let (_dir, repo) = setup_test_repo();
        write_file(
            &repo.join("src/main.rs"),
            "use crate::storage::GraphStore;\nfn main() {}\n",
        );
        write_file(&repo.join("src/storage.rs"), "pub struct GraphStore;\n");

        let imports = vec![Import {
            module: "crate::storage::GraphStore".to_string(),
            line: 1,
            col: 1,
        }];
        let resolved = resolve_imports(&repo, "src/main.rs", LanguageKind::Rust, &imports);

        assert_eq!(
            resolve_single_import(
                &repo,
                "src/main.rs",
                LanguageKind::Rust,
                "crate::storage::GraphStore"
            ),
            Some("src/storage.rs".to_string())
        );
        assert!(resolved.contains(&(
            "crate::storage::GraphStore".to_string(),
            "src/storage.rs".to_string()
        )));
    }

    #[test]
    fn import_resolution_for_python_resolves_module_file() {
        let (_dir, repo) = setup_test_repo();
        write_file(&repo.join("main.py"), "import foo\nfoo.run()\n");
        write_file(&repo.join("foo.py"), "def run():\n    return 1\n");

        let imports = vec![Import {
            module: "foo".to_string(),
            line: 1,
            col: 1,
        }];
        let resolved = resolve_imports(&repo, "main.py", LanguageKind::Python, &imports);

        assert_eq!(
            resolve_single_import(&repo, "main.py", LanguageKind::Python, "foo"),
            Some("foo.py".to_string())
        );
        assert!(resolved.contains(&("foo".to_string(), "foo.py".to_string())));
    }

    #[test]
    fn parse_failures_stay_zero_for_valid_rust_content() {
        let (_dir, repo) = setup_test_repo();
        write_file(
            &repo.join("src/main.rs"),
            "fn main() { println!(\"ok\"); }\n",
        );

        let mut store = open_test_store(&repo);
        let report = index_repository(&mut store, &repo, IndexOptions { full: false }).unwrap();

        assert_eq!(report.parse_failures, 0);
        assert!(report.errors.is_empty());
    }

    #[test]
    fn private_helpers_cover_hashes_paths_and_candidates() {
        assert_eq!(config_language_hint("Cargo.toml"), LanguageKind::Rust);
        assert_eq!(
            config_language_hint("package.json"),
            LanguageKind::JavaScript
        );

        assert_eq!(
            normalize_rel_path(std::path::Path::new("src/lib.rs")),
            "src/lib.rs"
        );

        assert_eq!(tokenize("Hello_World! 123"), vec!["hello_world", "123"]);

        let first_hash = stable_i64_hash(b"alpha");
        let same_hash = stable_i64_hash(b"alpha");
        let different_hash = stable_i64_hash(b"beta");
        assert_eq!(first_hash, same_hash);
        assert_ne!(first_hash, different_hash);

        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );

        let rust_candidates = rust_import_candidates("crate::storage::GraphStore as Store");
        assert!(rust_candidates.contains(&PathBuf::from("src/storage.rs")));
    }
}
