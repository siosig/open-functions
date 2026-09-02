//! Fast unit tests for the parts of `pool::instance` that don't require a
//! real `Driver`/process. Behavior that does need a real instance (acquire
//! ordering, idle reaper, crash reporting, queueing) is covered by the
//! integration tests in `crates/cf-rs-core/tests/pool.rs`, which use the
//! real `ProcessDriver` + `examples/hello-http`.

use std::time::Duration;

use super::instance::{AcquireError, PoolConfig, QueuePolicy};
use crate::runtime::DriverError;

fn sample_config() -> PoolConfig {
    PoolConfig {
        concurrency: 1,
        min_instances: 0,
        max_instances: 4,
        idle_timeout: Duration::from_secs(900),
        queue_policy: QueuePolicy::Wait,
        queue_max_wait: Duration::from_secs(30),
        start_timeout: Duration::from_secs(10),
        stop_grace: Duration::from_secs(5),
    }
}

#[test]
fn pool_config_holds_the_values_it_was_built_with() {
    let config = sample_config();
    assert_eq!(config.concurrency, 1);
    assert_eq!(config.min_instances, 0);
    assert_eq!(config.max_instances, 4);
    assert_eq!(config.queue_policy, QueuePolicy::Wait);
}

#[test]
fn queue_policy_variants_are_distinguishable() {
    assert_ne!(QueuePolicy::Wait, QueuePolicy::Reject);
    assert_eq!(QueuePolicy::Reject, QueuePolicy::Reject);
}

#[test]
fn acquire_error_messages_are_descriptive() {
    let rejected = AcquireError::Rejected;
    assert_eq!(
        rejected.to_string(),
        "all instances at capacity and queue_policy is reject"
    );

    let timeout = AcquireError::QueueTimeout(Duration::from_millis(500));
    assert!(timeout.to_string().contains("500ms"));

    let draining = AcquireError::Draining;
    assert_eq!(
        draining.to_string(),
        "pool is draining and cannot accept new work"
    );

    let spawn: AcquireError = DriverError::ReadyTimeout(Duration::from_secs(10)).into();
    assert!(
        spawn
            .to_string()
            .starts_with("failed to start a new instance")
    );
}
