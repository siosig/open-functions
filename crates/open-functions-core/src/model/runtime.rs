//! Function runtime kind (002-python-runtime): which language a `Source::Dir`
//! registration is written in, and how to tell from the directory contents
//! alone. `Source::Image` registrations carry `runtime` only as a display
//! hint (see [`crate::model::validate::validate_function`]) since the
//! existing image-mode contract is language-agnostic (`function-contract.md`).

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Which language a function's source is written in.
///
/// `#[serde(rename_all = "lowercase")]` makes `Python314` round-trip as the
/// string `"python314"` in both the admin API JSON and the redb-persisted
/// `Function` record (matching the `runtime` field values documented in
/// admin-api.md and data-model.md).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Runtime {
    Rust,
    Python314,
}

impl Runtime {
    /// The label value this runtime uses on Prometheus metrics and in the
    /// admin API's `runtime` field: `"rust"` / `"python314"`.
    pub fn label(self) -> &'static str {
        match self {
            Runtime::Rust => "rust",
            Runtime::Python314 => "python314",
        }
    }
}

/// The metrics `runtime` label value for every instance kind, including
/// image-mode functions that declared no `runtime` (labeled `"image"` rather
/// than left blank, so every metric series has a non-empty label — see
/// ops-config.md's metrics table for 002).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuntimeLabel {
    Rust,
    Python314,
    /// Image-mode function with no declared `runtime`.
    Image,
}

impl RuntimeLabel {
    pub fn as_str(self) -> &'static str {
        match self {
            RuntimeLabel::Rust => "rust",
            RuntimeLabel::Python314 => "python314",
            RuntimeLabel::Image => "image",
        }
    }

    /// Derives the metrics label from a function's declared runtime and
    /// whether it's image-mode (image-mode functions with no `runtime`
    /// declared fall back to `"image"`; source-mode functions always have a
    /// `runtime` by the time this is called, since [`detect_runtime`] or an
    /// explicit `runtime` is required at registration).
    pub fn from_declared(runtime: Option<Runtime>, is_image_mode: bool) -> Self {
        match (runtime, is_image_mode) {
            (Some(Runtime::Rust), _) => RuntimeLabel::Rust,
            (Some(Runtime::Python314), _) => RuntimeLabel::Python314,
            (None, true) => RuntimeLabel::Image,
            // A source-mode function must have runtime = Some(..) by
            // registration time; None here means a caller skipped that
            // invariant. Fall back to "image" rather than panicking, since
            // this is a metrics label, not a correctness-critical path.
            (None, false) => RuntimeLabel::Image,
        }
    }
}

/// Errors from [`detect_runtime`].
#[derive(Debug, thiserror::Error)]
pub enum DetectRuntimeError {
    #[error(
        "ambiguous runtime: both Cargo.toml and main.py are present in {0} \
         (pass an explicit runtime to resolve this)"
    )]
    Ambiguous(PathBuf),

    #[error("no runtime detected in {0}: expected a Cargo.toml (Rust) or main.py (Python 3.14)")]
    NotFound(PathBuf),

    #[error("failed to inspect {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

/// Auto-detects a `Source::Dir` registration's runtime from its top-level
/// contents, per spec.md's Edge Cases and FR-102: a `Cargo.toml` alone means
/// Rust, a `main.py` alone means Python 3.14, both present is ambiguous
/// (reject, don't guess), and neither present means no runtime was found.
///
/// Only checks file *presence* (`Path::is_file`), not contents -- deeper
/// validation (e.g. that `main.py` actually defines the requested entry
/// point) happens later, during the build/dependency-resolution step.
pub fn detect_runtime(dir: &Path) -> Result<Runtime, DetectRuntimeError> {
    let cargo_toml = dir.join("Cargo.toml");
    let main_py = dir.join("main.py");

    let has_cargo = path_is_file(&cargo_toml)?;
    let has_main_py = path_is_file(&main_py)?;

    match (has_cargo, has_main_py) {
        (true, true) => Err(DetectRuntimeError::Ambiguous(dir.to_path_buf())),
        (true, false) => Ok(Runtime::Rust),
        (false, true) => Ok(Runtime::Python314),
        (false, false) => Err(DetectRuntimeError::NotFound(dir.to_path_buf())),
    }
}

/// `Path::is_file`, but distinguishing "genuinely absent" (`NotFound`,
/// treated as `false`) from a real I/O error (permission denied, etc., which
/// must not be silently swallowed as "file doesn't exist").
fn path_is_file(path: &Path) -> Result<bool, DetectRuntimeError> {
    match std::fs::metadata(path) {
        Ok(meta) => Ok(meta.is_file()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(source) => Err(DetectRuntimeError::Io {
            path: path.to_path_buf(),
            source,
        }),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn write(dir: &Path, name: &str, contents: &str) {
        std::fs::write(dir.join(name), contents).expect("write fixture file");
    }

    #[test]
    fn detects_rust_from_cargo_toml_alone() {
        let dir = tempfile::tempdir().expect("tempdir");
        write(dir.path(), "Cargo.toml", "[package]\nname=\"x\"\n");
        assert_eq!(detect_runtime(dir.path()).expect("detect"), Runtime::Rust);
    }

    #[test]
    fn detects_python314_from_main_py_alone() {
        let dir = tempfile::tempdir().expect("tempdir");
        write(dir.path(), "main.py", "def hello(request): ...\n");
        assert_eq!(
            detect_runtime(dir.path()).expect("detect"),
            Runtime::Python314
        );
    }

    #[test]
    fn both_files_present_is_ambiguous() {
        let dir = tempfile::tempdir().expect("tempdir");
        write(dir.path(), "Cargo.toml", "[package]\nname=\"x\"\n");
        write(dir.path(), "main.py", "def hello(request): ...\n");
        assert!(matches!(
            detect_runtime(dir.path()),
            Err(DetectRuntimeError::Ambiguous(_))
        ));
    }

    #[test]
    fn neither_file_present_is_not_found() {
        let dir = tempfile::tempdir().expect("tempdir");
        assert!(matches!(
            detect_runtime(dir.path()),
            Err(DetectRuntimeError::NotFound(_))
        ));
    }

    #[test]
    fn runtime_serde_round_trips_as_lowercase_strings() {
        assert_eq!(
            serde_json::to_string(&Runtime::Rust).expect("serialize"),
            "\"rust\""
        );
        assert_eq!(
            serde_json::to_string(&Runtime::Python314).expect("serialize"),
            "\"python314\""
        );
        assert_eq!(
            serde_json::from_str::<Runtime>("\"rust\"").expect("deserialize"),
            Runtime::Rust
        );
        assert_eq!(
            serde_json::from_str::<Runtime>("\"python314\"").expect("deserialize"),
            Runtime::Python314
        );
        assert!(serde_json::from_str::<Runtime>("\"go\"").is_err());
    }

    #[test]
    fn runtime_label_matches_metrics_convention() {
        assert_eq!(Runtime::Rust.label(), "rust");
        assert_eq!(Runtime::Python314.label(), "python314");
    }

    #[test]
    fn runtime_label_from_declared_falls_back_to_image() {
        assert_eq!(
            RuntimeLabel::from_declared(Some(Runtime::Rust), true),
            RuntimeLabel::Rust
        );
        assert_eq!(RuntimeLabel::from_declared(None, true), RuntimeLabel::Image);
        assert_eq!(
            RuntimeLabel::from_declared(Some(Runtime::Python314), false),
            RuntimeLabel::Python314
        );
    }
}
