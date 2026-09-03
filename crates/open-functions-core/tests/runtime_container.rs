//! Integration tests for `ContainerDriver` (T073) against a real Docker
//! daemon, per plan.md's "Runtime drivers" Design Notes and T069's task
//! line: network creation, create/start/inspect IP, HTTP reachability,
//! stop/remove, and label-based stale-container cleanup.
//!
//! Opt-in and skipped by default (needs a real, reachable Docker daemon):
//! set `OPEN_FUNCTIONS_TEST_DOCKER=1` to run, matching this workspace's existing
//! opt-in-external-dependency convention (see `crates/open-functions/tests/e2e_pubsub.rs`'s
//! `OPEN_FUNCTIONS_TEST_OPEN_PUBUSB_URL` gate). `.github/workflows/ci.yml`'s `docker-tests`
//! job sets this and runs the whole workspace under `cargo nextest run`.
//!
//! The test image is built from a throwaway Dockerfile written to a tempdir
//! that just copies in `examples/hello-http`'s already-built release binary
//! (host `cargo build --release`, same lazy-build-if-missing helper as
//! `runtime_process.rs`'s `hello_http_binary()`) onto a `debian:bookworm-slim`
//! base and runs it directly -- much faster than a full `rust:1-bookworm`
//! multi-stage build (which examples/hello-http's own `Dockerfile` uses for
//! the *container build mode* being covered separately by T070/T074), and
//! confirmed compatible here: the host's glibc (Ubuntu 24.04, glibc 2.39) is
//! ABI-compatible with debian:bookworm-slim's glibc 2.36 runtime for this
//! binary (verified manually before writing this test: running the host
//! binary under a bookworm-slim container failed only on a missing
//! `FUNCTION_TARGET` env var, not a GLIBC version error).
//!
//! Panicking via `unwrap`/`expect` on setup failures is the desired behavior
//! in tests (it fails the test with a clear message), so the crate-wide
//! `unwrap_used`/`expect_used` lints are relaxed here, matching
//! `runtime_process.rs`.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::process::Command;
use std::sync::OnceLock;
use std::time::Duration;

use bollard::Docker;
use bollard::models::ContainerCreateBody;
use bollard::query_parameters::{
    CreateContainerOptionsBuilder, ListNetworksOptionsBuilder, RemoveContainerOptionsBuilder,
};
use open_functions_core::runtime::container::{ContainerDriver, sweep_stale_containers};
use open_functions_core::runtime::docker::{LABEL_FUNCTION, NETWORK_NAME, connect};
use open_functions_core::runtime::{Driver, InstanceExit, InstanceSpec};

/// The throwaway image tag this test suite builds and reuses across tests.
const TEST_IMAGE_TAG: &str = "open-functions-test-hello-http:latest";

/// Skips the calling test unless `OPEN_FUNCTIONS_TEST_DOCKER=1`, per this workspace's
/// opt-in-external-dependency convention.
macro_rules! require_docker_test {
    () => {
        if std::env::var("OPEN_FUNCTIONS_TEST_DOCKER").is_err() {
            eprintln!(
                "skipping {}: set OPEN_FUNCTIONS_TEST_DOCKER=1 to run (needs a real Docker daemon)",
                module_path!()
            );
            return;
        }
    };
}

fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("crates/")
        .parent()
        .expect("workspace root")
        .to_path_buf()
}

/// Builds `examples/hello-http` in release mode if the binary isn't already
/// there. Adapted from `runtime_process.rs`'s helper of the same name.
fn hello_http_binary() -> PathBuf {
    let example_dir = workspace_root().join("examples/hello-http");
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

/// Builds [`TEST_IMAGE_TAG`] from a throwaway Dockerfile in a tempdir, once
/// per test-binary process (the tests below all run in the same process
/// under `cargo test`, so a `OnceLock` is enough; each call after the first
/// is a no-op). Returns the tag.
fn ensure_test_image() -> &'static str {
    static BUILT: OnceLock<()> = OnceLock::new();
    BUILT.get_or_init(|| {
        let binary = hello_http_binary();
        let dir = tempfile::tempdir().expect("failed to create tempdir for test image build");

        std::fs::copy(&binary, dir.path().join("hello-http"))
            .expect("failed to copy hello-http binary into build context");
        std::fs::write(
            dir.path().join("Dockerfile"),
            "FROM debian:bookworm-slim\n\
             COPY hello-http /function\n\
             ENTRYPOINT [\"/function\"]\n",
        )
        .expect("failed to write throwaway Dockerfile");

        let status = Command::new("docker")
            .args(["build", "-t", TEST_IMAGE_TAG, "."])
            .current_dir(dir.path())
            .status()
            .expect("failed to invoke `docker build`");
        assert!(status.success(), "`docker build` failed for the test image");
    });
    TEST_IMAGE_TAG
}

fn docker_client() -> Docker {
    connect("").expect("connect() should always succeed (it doesn't touch the network)")
}

fn base_spec() -> InstanceSpec {
    InstanceSpec {
        function_name: "hello".to_string(),
        revision: 1,
        entry_point: "hello".to_string(),
        signature_type: "http",
        env: BTreeMap::new(),
        memory_mib: 128,
        start_timeout: Duration::from_secs(30),
        artifact_path: PathBuf::new(),
        image_ref: Some(ensure_test_image().to_string()),
    }
}

/// Covers T069's "network creation, create/start/inspect IP, HTTP reachability" and, at
/// the end, "stop/remove": spawning against a fresh (possibly missing)
/// `open-functions` network creates it, the resulting container is reachable over
/// HTTP through its assigned IP, and stopping tears the container down
/// completely.
#[tokio::test]
async fn spawn_creates_network_and_serves_http_then_stop_removes_container() {
    require_docker_test!();

    let docker = docker_client();

    // Best-effort: remove the `open-functions` network first (if it exists and has no
    // other endpoints) so this test actually exercises "created if missing"
    // rather than "already existed". A failure here (e.g. the network has
    // unrelated active endpoints, or simply doesn't exist yet) is not fatal
    // to the rest of the test -- `ensure_network` inside `spawn` is
    // idempotent either way.
    let _ = docker.remove_network(NETWORK_NAME).await;

    let driver = ContainerDriver::new(docker.clone());
    let spec = base_spec();

    let handle = driver
        .spawn(&spec)
        .await
        .expect("spawn should succeed for a valid image_ref");
    let addr = handle.addr;

    let networks = docker
        .list_networks(Some(
            ListNetworksOptionsBuilder::default()
                .filters(&std::collections::HashMap::from([(
                    "name".to_string(),
                    vec![NETWORK_NAME.to_string()],
                )]))
                .build(),
        ))
        .await
        .expect("list_networks should succeed");
    assert!(
        networks
            .iter()
            .any(|n| n.name.as_deref() == Some(NETWORK_NAME)),
        "expected the {NETWORK_NAME} network to exist after spawn, got {networks:?}"
    );

    // Re-verify with a raw TCP connect, even though `spawn()`'s readiness
    // polling already proved this.
    assert!(
        std::net::TcpStream::connect(addr).is_ok(),
        "instance should accept a raw TCP connection at {addr}"
    );

    let resp = reqwest::get(format!("http://{addr}/some/path"))
        .await
        .expect("HTTP GET to the instance should succeed");
    assert_eq!(resp.status(), reqwest::StatusCode::OK);
    let body = resp.text().await.expect("response body should be text");
    assert_eq!(body, "Hello /some/path");

    let container_name_prefix = "open-functions-hello-1-";
    let exit = handle.stop(Duration::from_secs(5)).await;
    assert_eq!(exit, InstanceExit::Stopped);

    // stop() consumed the handle, so re-list containers by name prefix
    // (the exact generated name included a random suffix we didn't keep)
    // and assert none remain -- proves `remove` actually happened, not just
    // `stop`.
    let remaining = docker
        .list_containers(Some(
            bollard::query_parameters::ListContainersOptionsBuilder::default()
                .all(true)
                .build(),
        ))
        .await
        .expect("list_containers should succeed");
    let leftover: Vec<_> = remaining
        .iter()
        .filter(|c| {
            c.names.as_ref().is_some_and(|names| {
                names
                    .iter()
                    .any(|n| n.trim_start_matches('/').starts_with(container_name_prefix))
            })
        })
        .collect();
    assert!(
        leftover.is_empty(),
        "expected no leftover containers named {container_name_prefix}*, found {leftover:?}"
    );
}

/// Covers T069's "sweep leftovers by label": a labeled container created directly
/// against the daemon (out-of-band, not started, not through the driver) is
/// removed by [`sweep_stale_containers`].
#[tokio::test]
async fn sweep_stale_containers_removes_labeled_leftovers() {
    require_docker_test!();

    let docker = docker_client();
    ensure_test_image();

    let stale_name = "open-functions-test-stale-leftover";
    // Clean up any leftover from a previous failed run before creating a
    // fresh one.
    let _ = docker
        .remove_container(
            stale_name,
            Some(RemoveContainerOptionsBuilder::default().force(true).build()),
        )
        .await;

    let body = ContainerCreateBody {
        image: Some(TEST_IMAGE_TAG.to_string()),
        labels: Some(std::collections::HashMap::from([(
            LABEL_FUNCTION.to_string(),
            "stale-test-function".to_string(),
        )])),
        ..Default::default()
    };
    docker
        .create_container(
            Some(
                CreateContainerOptionsBuilder::default()
                    .name(stale_name)
                    .build(),
            ),
            body,
        )
        .await
        .expect("create_container for the out-of-band stale container should succeed");
    // Deliberately not started: the sweep must remove labeled containers
    // regardless of run state ("leftover containers" includes never-started ones left
    // by a crash between create and start).

    let removed = sweep_stale_containers(&docker)
        .await
        .expect("sweep_stale_containers should succeed");
    assert!(
        removed >= 1,
        "expected sweep to remove at least the one stale container we created, removed {removed}"
    );

    let remaining = docker
        .list_containers(Some(
            bollard::query_parameters::ListContainersOptionsBuilder::default()
                .all(true)
                .filters(&std::collections::HashMap::from([(
                    "name".to_string(),
                    vec![stale_name.to_string()],
                )]))
                .build(),
        ))
        .await
        .expect("list_containers should succeed");
    assert!(
        remaining.is_empty(),
        "expected the stale container to be gone after sweep, found {remaining:?}"
    );
}
