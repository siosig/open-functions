use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BuildMode {
    Host,
    Container,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BuildStatus {
    Running,
    Succeeded,
    Failed,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Build {
    pub id: String,
    pub function_name: String,
    pub revision: u32,
    pub mode: BuildMode,
    pub status: BuildStatus,
    pub log_path: String,
    pub exit_code: Option<i32>,
    pub started_at: chrono::DateTime<chrono::Utc>,
    pub finished_at: Option<chrono::DateTime<chrono::Utc>>,
}
