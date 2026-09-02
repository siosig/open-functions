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
    pub created_at: chrono::DateTime<chrono::Utc>,
}
