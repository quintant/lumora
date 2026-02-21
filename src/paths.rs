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
