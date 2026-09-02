pub mod instance;

pub use instance::{AcquireError, AcquiredInstance, InstancePool, PoolConfig, QueuePolicy};

#[cfg(test)]
mod tests;
