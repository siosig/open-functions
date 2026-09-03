use super::build::BuildMode;
use super::function::Function;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Revision {
    pub function_name: String,
    pub number: u32,
    pub artifact_path: Option<String>,
    pub image_digest: Option<String>,
    pub build_id: Option<String>,
    pub snapshot: Function,
    /// Which build/dependency-resolution method produced this revision's
    /// artifact (002-python-runtime): `host` or `container`. `None` for
    /// image-mode revisions (no build step) and for records persisted
    /// before this field existed. Fixes the *launch* method for this
    /// revision's instances regardless of later config changes -- see
    /// runtime::launch and data-model.md's "launch method is immutable" note.
    #[serde(default)]
    pub build_mode: Option<BuildMode>,
    /// The container image used to resolve dependencies and launch this
    /// revision's instances, when `build_mode == Some(Container)` for a
    /// Python source-mode revision. `None` otherwise.
    #[serde(default)]
    pub container_image: Option<String>,
    /// Whether this revision's artifact directory has been deleted by the
    /// retention policy (FR-108a: only the current and previous revision's
    /// artifacts are kept). `artifact_path` is set to `None` at the same
    /// time this flips to `true`.
    #[serde(default)]
    pub artifact_pruned: bool,
    pub created_at: chrono::DateTime<chrono::Utc>,
}
