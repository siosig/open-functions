//! Domain models: Function, Revision, Build, TriggerBinding, and related types.

pub mod binding;
pub mod build;
pub mod function;
pub mod revision;
pub mod validate;

pub use binding::TriggerBinding;
pub use build::Build;
pub use function::Function;
pub use revision::Revision;

#[cfg(test)]
mod tests;
