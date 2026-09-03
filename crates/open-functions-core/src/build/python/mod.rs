//! Python 3.14 build pipeline (002-python-runtime, T024): dependency
//! resolution for a `Source::Dir` function whose `runtime` is
//! `Python314` -- as opposed to `crate::build::Builder`, which compiles Rust
//! sources with `cargo`, a [`PythonBuilder`] snapshots the source, resolves
//! `requirements.txt` into a venv, and verifies the entry point, per
//! `contracts/python-function-contract.md`'s "Dependency resolution and artifacts" steps.
//!
//! `HostPythonBuilder` (T028, [`host`]) runs these steps directly on the
//! host; `ContainerPythonBuilder` (T034, container-mode) runs steps 3-4
//! inside `python.container_image`. Both share [`requirements`],
//! [`snapshot`], and [`env`] (steps 1-2 are always run on the host, even for
//! container-mode, per the contract's step 5).

pub mod container;
pub mod env;
pub mod host;
pub mod requirements;
pub mod snapshot;

#[cfg(test)]
mod tests;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use tokio::io::AsyncWriteExt;

/// Which tool resolves `python.installer = "auto"` into: `auto` prefers `uv`
/// when it's usable, falling back to the interpreter's own `venv` + `pip`
/// (research.md R-installer).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Installer {
    Auto,
    Uv,
    Pip,
}

/// Input to [`PythonBuilder::build`] -- one function revision's Python
/// dependency-resolution job. Mirrors `crate::build::BuildRequest`'s role
/// for the Rust pipeline; see data-model.md's `PythonBuildRequest` entry.
#[derive(Debug, Clone)]
pub struct PythonBuildRequest {
    pub function_name: String,
    pub revision: u32,
    /// Absolute path to the source directory (contains `main.py`).
    pub source_dir: PathBuf,
    /// Absolute path to `<data_dir>/artifacts/<name>/<rev>/`, where `src/`,
    /// `venv/`, `requirements.open-functions.txt`, and `build.log` are
    /// written.
    pub artifact_dir: PathBuf,
    pub entry_point: String,
    /// Overall wall-clock budget for the whole build (snapshot through
    /// entry-point verification) -- `Function.timeout_secs` is a per-request
    /// invoke timeout and is unrelated; this comes from the registration
    /// path's own build timeout setting (mirrors `BuildRequest::timeout`).
    pub timeout: Duration,
    /// `<data_dir>/cache`, the parent of the `uv/`/`pip/` cache
    /// subdirectories shared across every function's builds.
    pub cache_root: PathBuf,
    /// `python.functions_framework` (e.g. `"functions-framework==3.10.2"`),
    /// appended to `requirements.open-functions.txt` only when the user's
    /// own `requirements.txt` doesn't already declare it (FR-104).
    pub functions_framework_spec: String,
    pub installer: Installer,
    /// Explicit interpreter override (`python.python_bin`), if configured.
    /// `None` means autodetect (`python3.14` -> `python3` -> `python`).
    pub python_bin: Option<String>,
    pub uv_bin: String,
    /// `python.container_image` -- carried here only for
    /// `ContainerPythonBuilder` (T034); `HostPythonBuilder` ignores it.
    pub container_image: String,
    /// The allowlisted host env vars to pass through to `uv`/`pip`, plus the
    /// host's own cache-dir/proxy overrides -- see [`env::passthrough_env`].
    pub passthrough_env: BTreeMap<String, String>,
}

/// Result of a successful [`PythonBuilder::build`]. `tool` is `"uv"` or
/// `"pip"` (whichever `installer` actually resolved to), recorded on
/// `Build.tool` the same way the Rust pipeline records `"cargo"`.
#[derive(Debug, Clone)]
pub struct PythonBuildOutcome {
    pub tool: String,
}

#[derive(Debug, thiserror::Error)]
pub enum PythonBuildError {
    /// No interpreter resolving to Python 3.14.x was found (autodetect
    /// exhausted `python3.14`/`python3`/`python`), or `python.python_bin`
    /// was set but doesn't run / isn't 3.14.
    #[error("no usable Python 3.14 interpreter found (tried {tried:?})")]
    UnsupportedPython { tried: Vec<String> },
    #[error("failed to snapshot source directory: {0}")]
    SnapshotFailed(#[source] std::io::Error),
    #[error("failed to create the virtual environment; see build log at {0}")]
    VenvFailed(PathBuf),
    #[error("dependency installation failed; see build log at {0}")]
    Install(PathBuf),
    #[error("entry point verification failed; see build log at {0}")]
    EntryPoint(PathBuf),
    #[error("build timed out after {0:?}")]
    Timeout(Duration),
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
}

/// Resolves a `Source::Dir` Python function's dependencies into a venv.
/// `host::HostPythonBuilder` runs on the host; a future
/// `ContainerPythonBuilder` (T034) runs steps 3-4 inside
/// `python.container_image`.
#[async_trait::async_trait]
pub trait PythonBuilder: Send + Sync {
    async fn build(
        &self,
        request: &PythonBuildRequest,
    ) -> Result<PythonBuildOutcome, PythonBuildError>;

    /// Whether this builder's prerequisite tooling is usable right now
    /// (a Python 3.14 interpreter on `PATH`/`python_bin`, for
    /// `HostPythonBuilder`; the Docker daemon, for `ContainerPythonBuilder`).
    /// Used by `python.mode = auto`/`host`/`container` selection (T029) to
    /// pick a usable builder or reject with `FAILED_PRECONDITION`.
    async fn is_available(&self) -> bool {
        true
    }
}

/// Append-only `build.log` writer shared by every `PythonBuilder`
/// implementation: `contracts/python-function-contract.md` describes the log
/// as "chronological text interleaved with step headers (`== step: snapshot ==` etc.)".
/// Write errors are swallowed (best-effort logging must never fail an
/// otherwise-successful build), matching `build::host_cargo`'s convention.
pub(crate) struct BuildLog {
    file: tokio::fs::File,
}

impl BuildLog {
    pub(crate) async fn create(path: &Path) -> std::io::Result<Self> {
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        let file = tokio::fs::File::create(path).await?;
        Ok(Self { file })
    }

    pub(crate) async fn step(&mut self, name: &str) {
        let _ = self
            .file
            .write_all(format!("== step: {name} ==\n").as_bytes())
            .await;
        let _ = self.file.flush().await;
    }

    pub(crate) async fn write_output(&mut self, text: &[u8]) {
        if text.is_empty() {
            return;
        }
        let _ = self.file.write_all(text).await;
        if !text.ends_with(b"\n") {
            let _ = self.file.write_all(b"\n").await;
        }
        let _ = self.file.flush().await;
    }
}
