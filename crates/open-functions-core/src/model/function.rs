use serde::{Deserialize, Serialize};

use super::runtime::Runtime;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FunctionState {
    Building,
    Ready,
    Failed,
    Deleting,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Trigger {
    #[default]
    Http,
    Pubsub {
        topic: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Source {
    Dir {
        path: String,
        #[serde(default)]
        bin: Option<String>,
    },
    Image {
        #[serde(rename = "ref")]
        image_ref: String,
    },
}

impl Default for Source {
    fn default() -> Self {
        Source::Dir {
            path: String::new(),
            bin: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QueuePolicy {
    Wait,
    Reject,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Function {
    pub name: String,
    pub trigger: Trigger,
    pub source: Source,
    /// Which language `source` is written in (002-python-runtime). `None`
    /// for records persisted before this field existed (restore backfills
    /// `Some(Runtime::Rust)` for `Source::Dir` -- see registry::restore) or
    /// for `Source::Image` registrations that declared no `runtime` (kept
    /// only as a display hint there; the image-mode contract itself is
    /// language-agnostic, see function-contract.md).
    #[serde(default)]
    pub runtime: Option<Runtime>,
    #[serde(default)]
    pub env: std::collections::BTreeMap<String, String>,
    #[serde(default = "default_entry_point")]
    pub entry_point: String,
    pub timeout_secs: u32,
    pub concurrency: u32,
    pub memory_mib: u32,
    pub min_instances: u32,
    pub max_instances: u32,
    pub idle_timeout_secs: u32,
    pub queue_policy: QueuePolicy,
    pub queue_max_wait_secs: u32,
    pub state: FunctionState,
    pub current_revision: Option<u32>,
    pub last_error: Option<String>,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub updated_at: chrono::DateTime<chrono::Utc>,
}

fn default_entry_point() -> String {
    "function".to_string()
}
