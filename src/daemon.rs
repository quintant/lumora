use std::path::Path;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use anyhow::Result;
use notify::{recommended_watcher, Event, RecursiveMode, Watcher};

use crate::indexer::{index_repository, IndexOptions, IndexReport};
use crate::paths::{RuntimePaths, STATE_DIR_NAME};
use crate::storage::GraphStore;

const WATCH_IGNORE_DIRS: &[&str] = &[
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

pub fn run_watcher_daemon(
    paths: &RuntimePaths,
    full_first: bool,
    debounce_ms: u64,
    json: bool,
) -> Result<()> {
    let mut store = GraphStore::open(&paths.db_path)?;
    let initial_report = index_repository(
        &mut store,
        &paths.repo_root,
        IndexOptions { full: full_first },
    )?;
    emit_report(&initial_report, json)?;

    let (tx, rx) = mpsc::channel();
    let mut watcher = recommended_watcher(move |event| {
        let _ = tx.send(event);
    })?;
    watcher.watch(&paths.repo_root, RecursiveMode::Recursive)?;

    eprintln!(
        "watching {} (state: {})",
        paths.repo_root.display(),
        paths.state_dir.display()
    );

    loop {
        let first = match rx.recv() {
            Ok(event) => event,
            Err(_) => continue,
        };

        let mut saw_relevant_change = false;
        let mut force_full_rescan = false;
        consume_event(
            first,
            &paths.repo_root,
            &paths.state_dir,
            &mut saw_relevant_change,
            &mut force_full_rescan,
        );

        let quiet_for = Duration::from_millis(debounce_ms.max(50));
        let flush_deadline = Instant::now() + quiet_for;
        loop {
            let now = Instant::now();
            if now >= flush_deadline {
                break;
            }

            match rx.recv_timeout(flush_deadline.saturating_duration_since(now)) {
                Ok(event) => consume_event(
                    event,
                    &paths.repo_root,
                    &paths.state_dir,
                    &mut saw_relevant_change,
                    &mut force_full_rescan,
                ),
                Err(mpsc::RecvTimeoutError::Timeout) => break,
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }
        }

        if !saw_relevant_change && !force_full_rescan {
            continue;
        }

        let report = index_repository(
            &mut store,
            &paths.repo_root,
            IndexOptions {
                full: force_full_rescan,
            },
        )?;
        emit_report(&report, json)?;
    }
}

fn consume_event(
    event: notify::Result<Event>,
    repo_root: &Path,
    state_dir: &Path,
    saw_relevant_change: &mut bool,
    force_full_rescan: &mut bool,
) {
    match event {
        Ok(event) => {
            if event.paths.is_empty() {
                *saw_relevant_change = true;
                return;
            }
            for path in event.paths {
                if is_relevant_path(&path, repo_root, state_dir) {
                    *saw_relevant_change = true;
                    return;
                }
            }
        }
        Err(err) => {
            eprintln!("watch error: {err}");
            *force_full_rescan = true;
        }
    }
}

fn is_relevant_path(path: &Path, repo_root: &Path, state_dir: &Path) -> bool {
    if path.starts_with(state_dir) {
        return false;
    }
    if !path.starts_with(repo_root) {
        return false;
    }

    let rel = match path.strip_prefix(repo_root) {
        Ok(rel) => rel,
        Err(_) => return false,
    };

    let mut components = rel.components();
    if let Some(first) = components.next() {
        let first = first.as_os_str().to_string_lossy().to_string();
        if WATCH_IGNORE_DIRS.contains(&first.as_str()) {
            return false;
        }
    }

    true
}

fn emit_report(report: &IndexReport, json: bool) -> Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(report)?);
    } else {
        println!(
            "indexed={} skipped={} removed={} parse_failures={} errors={}",
            report.indexed_files,
            report.skipped_files,
            report.removed_files,
            report.parse_failures,
            report.errors.len()
        );
        for error in &report.errors {
            eprintln!("index warning: {error}");
        }
    }

    Ok(())
}
