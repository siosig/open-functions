//! Domain layer for cf-rs. Does not depend on `axum`; the `cf-rs` binary wires this
//! into HTTP listeners.

pub mod build;
pub mod forward;
pub mod logs;
pub mod metrics;
pub mod model;
pub mod pool;
pub mod pubsub;
pub mod registry;
pub mod resolve;
pub mod runtime;
