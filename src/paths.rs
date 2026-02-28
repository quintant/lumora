use std::env;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

pub const STATE_DIR_NAME: &str = ".lumora";
pub const DEFAULT_DB_FILE: &str = "graph.db";

#[derive(Debug, Clone)]
pub struct RuntimePaths {
    pub repo_root: PathBuf,
    pub state_dir: PathBuf,
    pub db_path: PathBuf,
}

pub fn resolve_runtime_paths(
    repo_hint: &Path,
    state_dir_override: Option<&Path>,
    db_override: Option<&Path>,
) -> Result<RuntimePaths> {
    let repo_root = discover_repo_root(repo_hint)?;
    let state_dir = match state_dir_override {
        Some(explicit) => absolutize_path(explicit)?,
        None => repo_root.join(STATE_DIR_NAME),
    };

    let db_path = match db_override {
        Some(explicit) => absolutize_path(explicit)?,
        None => state_dir.join(DEFAULT_DB_FILE),
    };

    Ok(RuntimePaths {
        repo_root,
        state_dir,
        db_path,
    })
}

pub fn ensure_state_layout(paths: &RuntimePaths) -> Result<()> {
    fs::create_dir_all(&paths.state_dir)
        .with_context(|| format!("failed to create state dir {}", paths.state_dir.display()))?;

    if let Some(parent) = paths.db_path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create db parent {}", parent.display()))?;
    }

    Ok(())
}

pub fn discover_repo_root(repo_hint: &Path) -> Result<PathBuf> {
    let start = absolutize_path(repo_hint)?;
    let mut cursor = if start.is_file() {
        start
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| start.clone())
    } else {
        start.clone()
    };

    loop {
        let dot_git = cursor.join(".git");
        if dot_git.exists() {
            return Ok(cursor);
        }
        let Some(parent) = cursor.parent() else {
            return Ok(start);
        };
        cursor = parent.to_path_buf();
    }
}

fn absolutize_path(path: &Path) -> Result<PathBuf> {
    let candidate = if path.is_absolute() {
        path.to_path_buf()
    } else {
        let cwd = env::current_dir().context("failed to read current working directory")?;
        cwd.join(path)
    };

    if candidate.exists() {
        let canonical = match fs::canonicalize(&candidate) {
            Ok(path) => path,
            Err(_) => candidate,
        };
        Ok(strip_windows_verbatim_prefix(canonical))
    } else {
        Ok(candidate)
    }
}

fn strip_windows_verbatim_prefix(path: PathBuf) -> PathBuf {
    #[cfg(windows)]
    {
        let raw = path.to_string_lossy();
        if let Some(rest) = raw.strip_prefix(r"\\?\") {
            return PathBuf::from(rest);
        }
    }
    path
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::env;
    use std::fs;
    use std::path::{Path, PathBuf};
    use tempfile::TempDir;

    struct CwdGuard {
        original: PathBuf,
    }

    impl CwdGuard {
        fn set_to(path: &Path) -> Self {
            let original = env::current_dir().expect("failed to read current dir before test");
            env::set_current_dir(path).expect("failed to set current dir for test");
            Self { original }
        }
    }

    impl Drop for CwdGuard {
        fn drop(&mut self) {
            let _ = env::set_current_dir(&self.original);
        }
    }

    #[test]
    fn discover_repo_root_finds_nearest_git_parent() {
        let temp = TempDir::new().expect("failed to create temp dir");
        let repo_root = temp.path().join("repo");
        let nested = repo_root.join("src").join("deep");
        fs::create_dir_all(repo_root.join(".git")).expect("failed to create .git");
        fs::create_dir_all(&nested).expect("failed to create nested directories");

        let found = discover_repo_root(&nested).expect("discover_repo_root failed");
        assert_eq!(
            found,
            fs::canonicalize(&repo_root).expect("canonicalize failed")
        );
    }

    #[test]
    fn discover_repo_root_returns_start_when_no_git_found() {
        let temp = TempDir::new().expect("failed to create temp dir");
        let start = temp.path().join("workspace");
        fs::create_dir_all(&start).expect("failed to create start directory");

        let found = discover_repo_root(&start).expect("discover_repo_root failed");
        assert_eq!(
            found,
            fs::canonicalize(&start).expect("canonicalize failed")
        );
    }

    #[test]
    fn resolve_runtime_paths_uses_default_state_and_db_locations() {
        let temp = TempDir::new().expect("failed to create temp dir");
        let repo_root = temp.path().join("repo");
        fs::create_dir_all(repo_root.join(".git")).expect("failed to create .git");

        let paths =
            resolve_runtime_paths(&repo_root, None, None).expect("resolve_runtime_paths failed");

        let expected_repo_root = fs::canonicalize(&repo_root).expect("canonicalize failed");
        let expected_state_dir = expected_repo_root.join(STATE_DIR_NAME);
        let expected_db_path = expected_state_dir.join(DEFAULT_DB_FILE);

        assert_eq!(paths.repo_root, expected_repo_root);
        assert_eq!(paths.state_dir, expected_state_dir);
        assert_eq!(paths.db_path, expected_db_path);
    }

    #[test]
    fn resolve_runtime_paths_uses_state_override_and_default_db_name() {
        let temp = TempDir::new().expect("failed to create temp dir");
        let repo_root = temp.path().join("repo");
        let custom_state = temp.path().join("custom-state");
        fs::create_dir_all(repo_root.join(".git")).expect("failed to create .git");
        fs::create_dir_all(&custom_state).expect("failed to create custom state dir");

        let paths = resolve_runtime_paths(&repo_root, Some(&custom_state), None)
            .expect("resolve_runtime_paths failed");

        assert_eq!(
            paths.state_dir,
            fs::canonicalize(&custom_state).expect("canonicalize failed")
        );
        assert_eq!(paths.db_path, paths.state_dir.join(DEFAULT_DB_FILE));
    }

    #[test]
    fn resolve_runtime_paths_uses_db_override_with_default_state_dir() {
        let temp = TempDir::new().expect("failed to create temp dir");
        let repo_root = temp.path().join("repo");
        let custom_db_parent = temp.path().join("custom-db");
        let custom_db = custom_db_parent.join("alt.db");
        fs::create_dir_all(repo_root.join(".git")).expect("failed to create .git");
        fs::create_dir_all(&custom_db_parent).expect("failed to create custom db parent");
        fs::write(&custom_db, b"").expect("failed to create custom db file");

        let paths = resolve_runtime_paths(&repo_root, None, Some(&custom_db))
            .expect("resolve_runtime_paths failed");

        let expected_repo_root = fs::canonicalize(&repo_root).expect("canonicalize failed");
        assert_eq!(paths.state_dir, expected_repo_root.join(STATE_DIR_NAME));
        assert_eq!(
            paths.db_path,
            fs::canonicalize(&custom_db).expect("canonicalize failed")
        );
    }

    #[test]
    fn resolve_runtime_paths_uses_both_state_and_db_overrides() {
        let temp = TempDir::new().expect("failed to create temp dir");
        let repo_root = temp.path().join("repo");
        let custom_state = temp.path().join("state-override");
        let custom_db_parent = temp.path().join("db-override");
        let custom_db = custom_db_parent.join("graph-custom.db");
        fs::create_dir_all(repo_root.join(".git")).expect("failed to create .git");
        fs::create_dir_all(&custom_state).expect("failed to create custom state dir");
        fs::create_dir_all(&custom_db_parent).expect("failed to create custom db parent");

        let paths = resolve_runtime_paths(&repo_root, Some(&custom_state), Some(&custom_db))
            .expect("resolve_runtime_paths failed");

        assert_eq!(
            paths.state_dir,
            fs::canonicalize(&custom_state).expect("canonicalize failed")
        );
        assert_eq!(paths.db_path, custom_db);
    }

    #[test]
    fn ensure_state_layout_creates_state_and_db_parent_directories() {
        let temp = TempDir::new().expect("failed to create temp dir");
        let repo_root = temp.path().join("repo");
        let state_dir = temp.path().join("nested").join("state");
        let db_path = temp.path().join("nested").join("db").join("graph.db");
        fs::create_dir_all(repo_root.join(".git")).expect("failed to create .git");

        let paths = RuntimePaths {
            repo_root,
            state_dir: state_dir.clone(),
            db_path: db_path.clone(),
        };

        ensure_state_layout(&paths).expect("ensure_state_layout failed");

        assert!(state_dir.is_dir());
        assert!(db_path.parent().expect("db has parent").is_dir());
    }

    #[test]
    fn resolve_runtime_paths_absolutizes_relative_overrides() {
        let temp = TempDir::new().expect("failed to create temp dir");
        let _cwd_guard = CwdGuard::set_to(temp.path());
        let repo_hint = temp.path().join("repo");
        fs::create_dir_all(&repo_hint).expect("failed to create repo hint directory");

        let relative_state = Path::new("relative/state");
        let relative_db = Path::new("relative/db/custom.db");
        let paths = resolve_runtime_paths(&repo_hint, Some(relative_state), Some(relative_db))
            .expect("resolve_runtime_paths failed");

        assert_eq!(paths.state_dir, temp.path().join(relative_state));
        assert_eq!(paths.db_path, temp.path().join(relative_db));
    }

    #[test]
    fn resolve_runtime_paths_preserves_nonexistent_absolute_override_paths() {
        let temp = TempDir::new().expect("failed to create temp dir");
        let repo_root = temp.path().join("repo");
        let missing_state = temp.path().join("missing").join("state");
        let missing_db = temp.path().join("missing").join("db").join("graph.db");
        fs::create_dir_all(repo_root.join(".git")).expect("failed to create .git");

        let paths = resolve_runtime_paths(&repo_root, Some(&missing_state), Some(&missing_db))
            .expect("resolve_runtime_paths failed");

        assert_eq!(paths.state_dir, missing_state);
        assert_eq!(paths.db_path, missing_db);
    }

    #[cfg(not(windows))]
    #[test]
    fn resolve_runtime_paths_non_windows_verbatim_strip_is_passthrough() {
        let temp = TempDir::new().expect("failed to create temp dir");
        let repo_root = temp.path().join("repo");
        let existing_state = temp.path().join("existing-state");
        fs::create_dir_all(repo_root.join(".git")).expect("failed to create .git");
        fs::create_dir_all(&existing_state).expect("failed to create existing state dir");

        let expected = fs::canonicalize(&existing_state).expect("canonicalize failed");
        let paths = resolve_runtime_paths(&repo_root, Some(&existing_state), None)
            .expect("resolve_runtime_paths failed");

        assert_eq!(paths.state_dir, expected);
        assert!(!paths.state_dir.to_string_lossy().starts_with(r"\\?\"));
    }
}
