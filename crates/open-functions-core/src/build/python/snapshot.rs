//! Source snapshotting (T026): `contracts/python-function-contract.md`'s
//! "Dependency resolution and artifacts" step 1 -- copy the source directory into
//! `<artifact_dir>/src/` so later starts see a frozen copy, unaffected by
//! edits/deletes of the original directory (FR-105a). std-only, no
//! directory-walking crate needed for this project's scale.

use std::path::Path;

/// Directory/file basenames excluded from the snapshot at any depth.
const EXCLUDED_NAMES: &[&str] = &[
    ".venv",
    "venv",
    "__pycache__",
    ".git",
    ".mypy_cache",
    ".pytest_cache",
    ".ruff_cache",
];

/// Recursively copies `source_dir`'s contents into `dest_dir` (created if
/// missing), excluding [`EXCLUDED_NAMES`] and any `*.pyc` file. Symlinks are
/// recreated as symlinks (not followed / not dereferenced into a copy).
pub fn snapshot_source(source_dir: &Path, dest_dir: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dest_dir)?;
    copy_dir_contents(source_dir, dest_dir)
}

fn copy_dir_contents(src: &Path, dst: &Path) -> std::io::Result<()> {
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let file_name = entry.file_name();
        let name = file_name.to_string_lossy();
        if EXCLUDED_NAMES.contains(&name.as_ref()) || name.ends_with(".pyc") {
            continue;
        }

        let dst_path = dst.join(&file_name);
        let file_type = entry.file_type()?;
        if file_type.is_symlink() {
            copy_symlink(&entry.path(), &dst_path)?;
        } else if file_type.is_dir() {
            std::fs::create_dir_all(&dst_path)?;
            copy_dir_contents(&entry.path(), &dst_path)?;
        } else {
            std::fs::copy(entry.path(), &dst_path)?;
        }
    }
    Ok(())
}

#[cfg(unix)]
fn copy_symlink(src: &Path, dst: &Path) -> std::io::Result<()> {
    let target = std::fs::read_link(src)?;
    std::os::unix::fs::symlink(target, dst)
}

#[cfg(not(unix))]
fn copy_symlink(src: &Path, dst: &Path) -> std::io::Result<()> {
    std::fs::copy(src, dst).map(|_| ())
}
