//! `Store` trait: persistence abstraction implemented by redb and in-memory backends.

use crate::model::{Build, Function, Revision, TriggerBinding};

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("not found: {0}")]
    NotFound(String),
    #[error("storage error: {0}")]
    Backend(String),
}

pub trait Store: Send + Sync {
    fn get_function(&self, name: &str) -> Result<Option<Function>, StoreError>;
    fn list_functions(&self) -> Result<Vec<Function>, StoreError>;
    fn put_function(&self, function: &Function) -> Result<(), StoreError>;
    fn delete_function(&self, name: &str) -> Result<(), StoreError>;

    fn put_revision(&self, revision: &Revision) -> Result<(), StoreError>;
    fn get_revision(&self, name: &str, number: u32) -> Result<Option<Revision>, StoreError>;

    fn put_build(&self, build: &Build) -> Result<(), StoreError>;
    fn get_build(&self, id: &str) -> Result<Option<Build>, StoreError>;

    fn get_binding(&self, name: &str) -> Result<Option<TriggerBinding>, StoreError>;
    fn put_binding(&self, binding: &TriggerBinding) -> Result<(), StoreError>;
    fn delete_binding(&self, name: &str) -> Result<(), StoreError>;
    /// All tracked bindings, in no particular order. Used by the Pub/Sub
    /// binding reconciler's periodic retry sweep to find bindings whose
    /// `next_retry_at` is due.
    fn list_bindings(&self) -> Result<Vec<TriggerBinding>, StoreError>;
}
