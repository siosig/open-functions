pub mod container;
pub mod host_cargo;
pub mod metadata;

#[cfg(test)]
mod tests;

/// A request to build a single function revision's artifact from source.
#[derive(Debug, Clone)]
pub struct BuildRequest {
    pub function_name: String,
    pub revision: u32,
    /// Absolute path to the source directory (contains Cargo.toml).
    pub source_dir: std::path::PathBuf,
    /// Explicit bin target name, if the source has more than one.
    pub bin: Option<String>,
    /// Where to write the final copied artifact (a single executable file).
    pub artifact_path: std::path::PathBuf,
    /// Where to write the combined stdout+stderr build log.
    pub log_path: std::path::PathBuf,
    /// `CARGO_TARGET_DIR` to use (shared across revisions of the same function
    /// so incremental compilation caches are reused).
    pub cargo_target_dir: std::path::PathBuf,
    pub timeout: std::time::Duration,
}

#[derive(Debug, thiserror::Error)]
pub enum BuildError {
    #[error(transparent)]
    Metadata(#[from] metadata::MetadataError),
    #[error("failed to spawn build process: {0}")]
    Spawn(#[source] std::io::Error),
    #[error("build timed out after {0:?}")]
    Timeout(std::time::Duration),
    #[error("build exited with non-zero status {0}; see build log at {1}")]
    NonZeroExit(i32, std::path::PathBuf),
    #[error("failed to copy artifact from {from} to {to}: {source}")]
    CopyArtifact {
        from: std::path::PathBuf,
        to: std::path::PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
}

/// Builds a function revision's artifact from source. Implementation-agnostic:
/// `host_cargo::HostCargoBuilder` runs `cargo build` on the host; a future
/// `container::ContainerBuilder` (US4) will build inside a `rust:1-bookworm`
/// container.
#[async_trait::async_trait]
pub trait Builder: Send + Sync {
    async fn build(&self, request: &BuildRequest) -> Result<(), BuildError>;
}
