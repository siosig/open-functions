//! Integration tests for `InstancePool` (T038) using the real `ProcessDriver`
//! and the real `examples/hello-http` fixture binary — same pattern as
//! `tests/runtime_process.rs`. Exercises the acquire-order/single-flight,
//! queue_policy wait/reject, idle reaper (min_instances-respecting), and
//! reactive crash-detection behavior described in plan.md's "InstancePool"
//! Design Notes and spec.md's FR-016/FR-017/FR-019.
//!
//! Panicking via `unwrap`/`expect` on setup/assertion failures is the
//! desired behavior in tests, matching `tests/runtime_process.rs`.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::process::Command;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::Semaphore;

use cf_rs_core::logs::ring::LogStore;
use cf_rs_core::pool::{AcquireError, InstancePool, PoolConfig, QueuePolicy};
use cf_rs_core::runtime::InstanceSpec;
use cf_rs_core::runtime::cgroup::CgroupLimiter;
use cf_rs_core::runtime::process::ProcessDriver;

/// Builds `examples/hello-http` in release mode if the binary isn't already
/// there. Mirrors `tests/runtime_process.rs::hello_http_binary` — duplicated
/// rather than shared because each integration test file compiles as its own
/// crate.
fn hello_http_binary() -> PathBuf {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let example_dir = manifest_dir
        .join("../../examples/hello-http")
        .canonicalize()
        .expect("examples/hello-http should exist relative to cf-rs-core");
    let binary = example_dir.join("target/release/hello-http");

    if !binary.exists() {
        let status = Command::new("cargo")
            .args(["build", "--release"])
            .current_dir(&example_dir)
            .env_remove("CARGO_TARGET_DIR")
            .status()
            .expect("failed to invoke cargo to build examples/hello-http");
        assert!(
            status.success(),
            "cargo build --release failed for examples/hello-http"
        );
    }

    assert!(
        binary.exists(),
        "hello-http binary missing at {binary:?} even after building"
    );
    binary
}

fn base_spec(artifact_path: PathBuf) -> InstanceSpec {
    InstanceSpec {
        function_name: "hello".to_string(),
        revision: 1,
        entry_point: "hello".to_string(),
        signature_type: "http",
        env: BTreeMap::new(),
        memory_mib: 128,
        start_timeout: Duration::from_secs(10),
        artifact_path,
        image_ref: None,
    }
}

fn driver() -> Arc<ProcessDriver> {
    Arc::new(ProcessDriver {
        limiter: Arc::new(CgroupLimiter::probe()),
        log_store: Arc::new(LogStore::default()),
    })
}

fn global_limit() -> Arc<Semaphore> {
    Arc::new(Semaphore::new(32))
}

fn make_pool(spec: InstanceSpec, config: PoolConfig) -> InstancePool {
    InstancePool::new(
        spec.function_name.clone(),
        driver(),
        spec,
        config,
        global_limit(),
    )
}

/// Criterion 1: concurrency=1, two concurrent `acquire()` calls start two
/// distinct instances (single-flight prevents a thundering herd, but must
/// not serialize callers onto the same, already-full, instance).
#[tokio::test]
async fn two_concurrent_acquires_start_two_distinct_instances() {
    let binary = hello_http_binary();
    let pool = make_pool(
        base_spec(binary),
        PoolConfig {
            concurrency: 1,
            min_instances: 0,
            max_instances: 4,
            idle_timeout: Duration::from_secs(900),
            queue_policy: QueuePolicy::Wait,
            queue_max_wait: Duration::from_secs(10),
            start_timeout: Duration::from_secs(10),
            stop_grace: Duration::from_secs(5),
        },
    );

    let (r1, r2) = tokio::join!(pool.acquire(), pool.acquire());
    let a1 = r1.expect("first acquire should succeed");
    let a2 = r2.expect("second acquire should succeed");

    assert_ne!(
        a1.addr, a2.addr,
        "two concurrent callers with concurrency=1 must land on two different instances"
    );
    assert!(std::net::TcpStream::connect(a1.addr).is_ok());
    assert!(std::net::TcpStream::connect(a2.addr).is_ok());
    assert_eq!(pool.instance_count().await, 2);
}

/// Criterion 2: at `max_instances` with no free slot and `queue_policy =
/// Reject`, a further `acquire()` fails immediately.
#[tokio::test]
async fn reject_policy_fails_fast_at_capacity() {
    let binary = hello_http_binary();
    let pool = make_pool(
        base_spec(binary),
        PoolConfig {
            concurrency: 1,
            min_instances: 0,
            max_instances: 1,
            idle_timeout: Duration::from_secs(900),
            queue_policy: QueuePolicy::Reject,
            queue_max_wait: Duration::from_secs(30),
            start_timeout: Duration::from_secs(10),
            stop_grace: Duration::from_secs(5),
        },
    );

    let _held = pool.acquire().await.expect("first acquire should succeed");

    let start = Instant::now();
    let result = pool.acquire().await;
    let elapsed = start.elapsed();

    match result {
        Err(AcquireError::Rejected) => {}
        Ok(a) => panic!("expected Rejected, got Ok(addr={})", a.addr),
        Err(other) => panic!("expected Rejected, got Err({other})"),
    }
    assert!(
        elapsed < Duration::from_millis(200),
        "Reject should fail fast, took {elapsed:?}"
    );
}

/// Criterion 3: at `max_instances` with `queue_policy = Wait` and a short
/// `queue_max_wait`, a further `acquire()` fails with `QueueTimeout` after
/// roughly that wait.
#[tokio::test]
async fn wait_policy_times_out_after_queue_max_wait() {
    let binary = hello_http_binary();
    let queue_max_wait = Duration::from_millis(500);
    let pool = make_pool(
        base_spec(binary),
        PoolConfig {
            concurrency: 1,
            min_instances: 0,
            max_instances: 1,
            idle_timeout: Duration::from_secs(900),
            queue_policy: QueuePolicy::Wait,
            queue_max_wait,
            start_timeout: Duration::from_secs(10),
            stop_grace: Duration::from_secs(5),
        },
    );

    let _held = pool.acquire().await.expect("first acquire should succeed");

    let start = Instant::now();
    let result = pool.acquire().await;
    let elapsed = start.elapsed();

    match result {
        Err(AcquireError::QueueTimeout(reported)) => assert_eq!(reported, queue_max_wait),
        Ok(a) => panic!("expected QueueTimeout, got Ok(addr={})", a.addr),
        Err(other) => panic!("expected QueueTimeout, got Err({other})"),
    }
    assert!(
        elapsed >= queue_max_wait,
        "should not time out before queue_max_wait ({queue_max_wait:?}), took {elapsed:?}"
    );
    assert!(
        elapsed < queue_max_wait * 3,
        "should not wait much longer than queue_max_wait ({queue_max_wait:?}), took {elapsed:?}"
    );
}

/// Criterion 4: the idle reaper stops an instance idle past `idle_timeout`,
/// so a subsequent `acquire()` starts a fresh instance. Drives
/// `reap_idle_once()` directly rather than `spawn_idle_reaper()`'s real 30s
/// tick, which would make this test take 30+ seconds for no additional
/// coverage (the tick-loop shape itself is covered separately below).
#[tokio::test]
async fn idle_reaper_stops_instances_past_idle_timeout() {
    let binary = hello_http_binary();
    let pool = make_pool(
        base_spec(binary),
        PoolConfig {
            concurrency: 1,
            min_instances: 0,
            max_instances: 4,
            idle_timeout: Duration::from_millis(200),
            queue_policy: QueuePolicy::Reject,
            queue_max_wait: Duration::from_secs(1),
            start_timeout: Duration::from_secs(10),
            stop_grace: Duration::from_secs(5),
        },
    );

    let first = pool.acquire().await.expect("acquire should succeed");
    let first_addr = first.addr;
    drop(first); // release the permit so the instance is fully idle

    tokio::time::sleep(Duration::from_millis(300)).await;
    pool.reap_idle_once().await;

    assert_eq!(
        pool.instance_count().await,
        0,
        "idle instance should have been reaped"
    );

    let second = pool.acquire().await.expect("acquire should succeed");
    assert_ne!(
        second.addr, first_addr,
        "a fresh instance should have been started, not the reaped one"
    );
}

/// Criterion 5: a crash mid-request, reported via `report_dead`, removes the
/// instance from rotation so the next `acquire()` starts a new one.
#[tokio::test]
async fn crash_then_report_dead_prevents_reuse() {
    let binary = hello_http_binary();
    let mut spec = base_spec(binary);
    spec.env.insert("CRASH".to_string(), "1".to_string());

    let pool = make_pool(
        spec,
        PoolConfig {
            concurrency: 4,
            min_instances: 0,
            max_instances: 4,
            idle_timeout: Duration::from_secs(900),
            queue_policy: QueuePolicy::Reject,
            queue_max_wait: Duration::from_secs(1),
            start_timeout: Duration::from_secs(10),
            stop_grace: Duration::from_secs(5),
        },
    );

    let acquired = pool.acquire().await.expect("acquire should succeed");
    let dead_addr = acquired.addr;

    // Trigger the crash. The response may come back as a connection error or
    // a partial response depending on timing; only the resulting removal
    // matters here.
    let _ = reqwest::get(format!("http://{dead_addr}/")).await;
    // Give the process a moment to actually exit before we report it.
    tokio::time::sleep(Duration::from_millis(200)).await;

    pool.report_dead(dead_addr).await;
    assert_eq!(pool.instance_count().await, 0);

    // Idempotent: reporting the same (already-removed) address again is a
    // no-op, not a panic.
    pool.report_dead(dead_addr).await;

    let fresh = pool.acquire().await.expect("acquire should succeed");
    assert_ne!(
        fresh.addr, dead_addr,
        "the dead instance must never be handed out again"
    );
    assert_eq!(pool.instance_count().await, 1);
}

/// Criterion 6: the idle reaper must not stop the last instance below
/// `min_instances`, even once it's idle past `idle_timeout`.
#[tokio::test]
async fn idle_reaper_respects_min_instances() {
    let binary = hello_http_binary();
    let pool = make_pool(
        base_spec(binary),
        PoolConfig {
            concurrency: 1,
            min_instances: 1,
            max_instances: 4,
            idle_timeout: Duration::from_millis(200),
            queue_policy: QueuePolicy::Reject,
            queue_max_wait: Duration::from_secs(1),
            start_timeout: Duration::from_secs(10),
            stop_grace: Duration::from_secs(5),
        },
    );

    let acquired = pool.acquire().await.expect("acquire should succeed");
    drop(acquired); // idle, but min_instances=1 should protect it

    tokio::time::sleep(Duration::from_millis(300)).await;
    pool.reap_idle_once().await;

    assert_eq!(
        pool.instance_count().await,
        1,
        "the only instance must survive the reaper when min_instances=1"
    );
}

/// `spawn_idle_reaper` returns a live background task on a 30s tick; this
/// only checks the task shape (spawns, is abortable) without waiting out a
/// real tick, which the criterion-4 test above already covers via
/// `reap_idle_once()`.
#[tokio::test]
async fn spawn_idle_reaper_returns_an_abortable_background_task() {
    let binary = hello_http_binary();
    let pool = Arc::new(make_pool(
        base_spec(binary),
        PoolConfig {
            concurrency: 1,
            min_instances: 0,
            max_instances: 4,
            idle_timeout: Duration::from_secs(900),
            queue_policy: QueuePolicy::Reject,
            queue_max_wait: Duration::from_secs(1),
            start_timeout: Duration::from_secs(10),
            stop_grace: Duration::from_secs(5),
        },
    ));

    let handle = pool.spawn_idle_reaper();
    assert!(!handle.is_finished());
    handle.abort();
}
