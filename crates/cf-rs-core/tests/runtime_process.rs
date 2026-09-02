//! Integration tests for `ProcessDriver` (T035) using the real
//! `examples/hello-http` fixture binary, per plan.md's "Runtime drivers"
//! Design Notes and contracts/function-contract.md's "Startup and environment variables" table.
//!
//! Panicking via `unwrap`/`expect` on setup failures is the desired behavior in
//! tests (it fails the test with a clear message), so the crate-wide
//! `unwrap_used`/`expect_used` lints are relaxed here, matching
//! `crates/cf-rs-sdk/tests/http_contract.rs` and `crates/cf-rs/tests/config.rs`.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::{Duration, Instant};

use cf_rs_core::runtime::cgroup::CgroupLimiter;
use cf_rs_core::runtime::process::ProcessDriver;
use cf_rs_core::runtime::{Driver, DriverError, InstanceExit, InstanceSpec};

/// Builds `examples/hello-http` in release mode if the binary isn't already
/// there, so this test is self-contained (doesn't silently no-op in CI).
/// `CARGO_TARGET_DIR` is explicitly cleared for the child `cargo` invocation:
/// this workspace's own agent-isolation convention sets that env var for
/// *our* build, and `examples/hello-http` is deliberately not a workspace
/// member (see its `Cargo.toml`) — it must build to its own
/// `target/release/`, matching what plan.md's Builder does for real deploys.
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
    }
}

fn driver() -> ProcessDriver {
    ProcessDriver {
        limiter: Arc::new(CgroupLimiter::probe()),
    }
}

#[tokio::test]
async fn spawn_serves_http_and_readiness_holds() {
    let binary = hello_http_binary();
    let driver = driver();
    let spec = base_spec(binary);

    let handle = driver
        .spawn(&spec)
        .await
        .expect("spawn should succeed for a valid hello-http artifact");
    let addr = handle.addr;

    // Re-verify with a raw TCP connect, even though `spawn()`'s readiness
    // polling already proved this.
    assert!(
        std::net::TcpStream::connect(addr).is_ok(),
        "instance should accept a raw TCP connection at {addr}"
    );

    // A crash-free successful GET indirectly proves FUNCTION_TARGET=hello,
    // PORT, and FUNCTION_SIGNATURE_TYPE=http all reached the child correctly:
    // if FUNCTION_TARGET were wrong, `Functions::router()` inside the SDK
    // would return `Error::MissingTarget` and `serve()` would exit(1) before
    // ever binding PORT, so `spawn()` would have failed readiness above.
    let resp = reqwest::get(format!("http://{addr}/some/path"))
        .await
        .expect("HTTP GET to the instance should succeed");
    assert_eq!(resp.status(), reqwest::StatusCode::OK);
    let body = resp.text().await.expect("response body should be text");
    assert_eq!(body, "Hello /some/path");

    let exit = handle.stop(Duration::from_secs(5)).await;
    assert_eq!(exit, InstanceExit::Stopped);
}

#[tokio::test]
async fn crash_during_request_is_reported_as_crashed() {
    let binary = hello_http_binary();
    let driver = driver();
    let mut spec = base_spec(binary);
    // hello-http's CRASH=1 only exits inside the request handler, not at
    // startup, so `spawn()` (readiness = the process binding PORT) succeeds
    // normally; the crash only happens once a request actually arrives.
    spec.env.insert("CRASH".to_string(), "1".to_string());

    let handle = driver
        .spawn(&spec)
        .await
        .expect("spawn should succeed even though the handler will crash on request");
    let addr = handle.addr;

    // Trigger the crash path. The instance exits mid-request, so the response
    // may come back as a connection error or a partial response depending on
    // timing; either is fine, only the resulting exit reason matters.
    let _ = reqwest::get(format!("http://{addr}/")).await;

    let exit = handle.wait().await;
    assert!(
        matches!(exit, InstanceExit::Crashed(_)),
        "expected Crashed, got {exit:?}"
    );
}

#[tokio::test]
async fn graceful_stop_returns_promptly_within_grace() {
    let binary = hello_http_binary();
    let driver = driver();
    let spec = base_spec(binary);

    let handle = driver
        .spawn(&spec)
        .await
        .expect("spawn should succeed for a valid hello-http artifact");

    let grace = Duration::from_secs(5);
    let start = Instant::now();
    let exit = handle.stop(grace).await;
    let elapsed = start.elapsed();

    assert_eq!(exit, InstanceExit::Stopped);
    assert!(
        elapsed < grace,
        "stop() took {elapsed:?}, expected well under the {grace:?} grace period \
         (hello-http has no SIGTERM handler, so the OS default should terminate it immediately)"
    );
}

#[tokio::test]
async fn missing_artifact_fails_with_spawn_error_not_ready_timeout() {
    let driver = driver();
    let mut spec = base_spec(PathBuf::from("/nonexistent/path/cf-rs-test-missing-binary"));
    // Keep this short in case the exec failure were ever *not* immediate;
    // the assertion below still requires `DriverError::Spawn`, not a timeout.
    spec.start_timeout = Duration::from_secs(2);

    match driver.spawn(&spec).await {
        Err(DriverError::Spawn(_)) => {}
        Err(other) => panic!("expected DriverError::Spawn(_), got Err({other})"),
        Ok(handle) => panic!(
            "expected spawn to fail for a nonexistent artifact, but it succeeded with addr {}",
            handle.addr
        ),
    }
}

/// Sanity check that `hello_http_binary()`'s path actually resolves inside
/// `examples/hello-http/target/release/`, per the task's fixture contract.
#[test]
fn fixture_binary_path_is_examples_hello_http_release() {
    let binary = hello_http_binary();
    assert!(binary.ends_with(Path::new("examples/hello-http/target/release/hello-http")));
}
