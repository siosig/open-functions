//! `cargo metadata` bin-target discovery. Implemented in US1 (T032).
//!
//! This module shells out to `cargo metadata --no-deps` synchronously. It is
//! intentionally NOT `async` — callers running inside an async context should
//! wrap calls in `tokio::task::spawn_blocking`.

use std::path::{Path, PathBuf};
use std::process::Command;

#[derive(Debug, thiserror::Error)]
pub enum MetadataError {
    #[error("failed to run `cargo metadata` in {dir}: {source}")]
    Spawn {
        dir: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("`cargo metadata` in {dir} exited with status {status}: {stderr}")]
    NonZeroExit {
        dir: PathBuf,
        status: i32,
        stderr: String,
    },
    #[error("failed to parse `cargo metadata` output for {dir}: {source}")]
    Parse {
        dir: PathBuf,
        #[source]
        source: serde_json::Error,
    },
    #[error("no [[bin]] target found in {dir}")]
    NoBinTarget { dir: PathBuf },
    #[error(
        "multiple [[bin]] targets found in {dir} ({names:?}); registration must specify `source.bin`"
    )]
    AmbiguousBinTarget { dir: PathBuf, names: Vec<String> },
    #[error("bin target {name:?} not found in {dir}; available: {available:?}")]
    BinNotFound {
        dir: PathBuf,
        name: String,
        available: Vec<String>,
    },
}

/// Resolves which `[[bin]]` target to build for a source directory. If `requested_bin`
/// is `Some`, that exact name must exist. If `None`, there must be exactly one bin
/// target in the *root* package found by `cargo metadata --no-deps` run in `dir`
/// (only the package whose manifest_path is under `dir`, not its dependencies).
pub fn resolve_bin_target(
    dir: &Path,
    requested_bin: Option<&str>,
) -> Result<String, MetadataError> {
    let output = Command::new("cargo")
        .args([
            "metadata",
            "--no-deps",
            "--format-version",
            "1",
            "--offline",
        ])
        .current_dir(dir)
        .output()
        .map_err(|source| MetadataError::Spawn {
            dir: dir.to_path_buf(),
            source,
        })?;

    if !output.status.success() {
        return Err(MetadataError::NonZeroExit {
            dir: dir.to_path_buf(),
            status: output.status.code().unwrap_or(-1),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        });
    }

    let value: serde_json::Value =
        serde_json::from_slice(&output.stdout).map_err(|source| MetadataError::Parse {
            dir: dir.to_path_buf(),
            source,
        })?;

    let bin_names = collect_root_bin_names(&value, dir);

    match requested_bin {
        Some(name) => {
            if bin_names.iter().any(|n| n == name) {
                Ok(name.to_string())
            } else {
                Err(MetadataError::BinNotFound {
                    dir: dir.to_path_buf(),
                    name: name.to_string(),
                    available: bin_names,
                })
            }
        }
        None => match bin_names.len() {
            0 => Err(MetadataError::NoBinTarget {
                dir: dir.to_path_buf(),
            }),
            1 => Ok(bin_names[0].clone()),
            _ => Err(MetadataError::AmbiguousBinTarget {
                dir: dir.to_path_buf(),
                names: bin_names,
            }),
        },
    }
}

/// Collects bin-target names from packages whose `manifest_path` lives under `dir`
/// (i.e. the root package(s) being built, not their dependencies).
fn collect_root_bin_names(metadata: &serde_json::Value, dir: &Path) -> Vec<String> {
    let dir = dir.canonicalize().unwrap_or_else(|_| dir.to_path_buf());

    let mut names = Vec::new();
    let Some(packages) = metadata.get("packages").and_then(|p| p.as_array()) else {
        return names;
    };

    for package in packages {
        let Some(manifest_path) = package.get("manifest_path").and_then(|m| m.as_str()) else {
            continue;
        };
        let manifest_path = PathBuf::from(manifest_path);
        let manifest_dir = manifest_path
            .parent()
            .map(|p| p.canonicalize().unwrap_or_else(|_| p.to_path_buf()));
        if manifest_dir != Some(dir.clone()) {
            continue;
        }

        let Some(targets) = package.get("targets").and_then(|t| t.as_array()) else {
            continue;
        };
        for target in targets {
            let is_bin = target
                .get("kind")
                .and_then(|k| k.as_array())
                .map(|kinds| kinds.iter().any(|k| k.as_str() == Some("bin")))
                .unwrap_or(false);
            if !is_bin {
                continue;
            }
            if let Some(name) = target.get("name").and_then(|n| n.as_str()) {
                names.push(name.to_string());
            }
        }
    }

    names
}
