use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BindingState {
    Pending,
    Bound,
    Unbinding,
    Error,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TriggerBinding {
    pub function_name: String,
    pub subscription: String,
    pub topic: String,
    pub push_endpoint: String,
    pub state: BindingState,
    pub last_error: Option<String>,
    pub next_retry_at: Option<chrono::DateTime<chrono::Utc>>,
}
