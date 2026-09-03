//! Domain models: Function, Revision, Build, TriggerBinding, and related types.

pub mod binding;
pub mod build;
pub mod function;
pub mod revision;
pub mod runtime;
pub mod validate;

pub use binding::TriggerBinding;
pub use build::Build;
pub use function::Function;
pub use revision::Revision;
pub use runtime::{DetectRuntimeError, Runtime, RuntimeLabel, detect_runtime};

#[cfg(test)]
mod tests;
