//! Unit tests for image-kind (`Source::Image`) registration (T071), covering
//! T075's `RegistryService::register`/`register_image` behavior: Docker
//! unavailable -> `412 FAILED_PRECONDITION` (`RegisterError::Unsupported`),
//! and a successful registration records the image's digest on the
//! resulting `Revision`.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::Semaphore;

use crate::build::container::ContainerBuilder;
use crate::build::host_cargo::HostCargoBuilder;
use crate::build::python::Installer as PythonInstaller;
use crate::build::python::env::passthrough_env;
use crate::build::python::host::HostPythonBuilder;
use crate::model::function::{QueuePolicy as ModelQueuePolicy, Source, Trigger};
use crate::registry::memory::MemoryStore;
use crate::registry::service::{
    BuildModeSetting, PythonModeSetting, PythonSettings, RegisterError, RegisterRequest,
    RegistrationDefaults, RegistryService,
};
use crate::runtime::cgroup::CgroupLimiter;
use crate::runtime::container::ContainerDriver;
use crate::runtime::docker;
use crate::runtime::process::ProcessDriver;

// This crate configures `clippy::unwrap_used`/`clippy::expect_used` as
// warnings (promoted to errors under `-D warnings`), applying to every
// target including this test module; `ok`/`err` stand in for
// `.unwrap()`/`.expect()` without tripping those lints (matches
// `build::tests`'s established pattern in this same crate).
fn ok<T, E: std::fmt::Debug>(result: Result<T, E>, context: &str) -> T {
    result.unwrap_or_else(|e| panic!("{context}: {e:?}"))
}

fn defaults() -> RegistrationDefaults {
    RegistrationDefaults {
        timeout_secs: 60,
        concurrency: 1,
        memory_mib: 256,
        min_instances: 0,
        max_instances: 10,
        idle_timeout_secs: 900,
        queue_policy: ModelQueuePolicy::Wait,
        queue_max_wait_secs: 30,
    }
}

/// Builds a `RegistryService` pointed at `docker_socket` (a bogus path
/// yields a `Docker` client that will never actually reach a daemon,
/// deterministically, without needing Docker uninstalled/unreachable on the
/// machine actually running this test suite -- see `runtime::docker`'s own
/// doc comments on why constructing a client this way is always infallible).
fn registry_with_docker_socket(data_dir: &std::path::Path, docker_socket: &str) -> RegistryService {
    let store = Arc::new(MemoryStore::new());
    let host_builder = Arc::new(HostCargoBuilder {
        cargo_bin: "cargo".to_string(),
    });
    let container_builder = Arc::new(ContainerBuilder {
        docker_socket: docker_socket.to_string(),
    });
    let process_driver = Arc::new(ProcessDriver {
        limiter: Arc::new(CgroupLimiter::probe()),
        log_store: Arc::new(crate::logs::ring::LogStore::default()),
    });
    let container_driver = Arc::new(ContainerDriver {
        docker: ok(docker::connect(docker_socket), "connect"),
        log_store: Arc::new(crate::logs::ring::LogStore::default()),
    });
    let global_limit = Arc::new(Semaphore::new(8));
    let cache_root = data_dir.join("cache");
    let python = PythonSettings {
        mode: PythonModeSetting::Host,
        host_builder: Arc::new(HostPythonBuilder {
            python_bin_override: String::new(),
            uv_bin: "uv".to_string(),
        }),
        container_builder: None,
        installer: PythonInstaller::Auto,
        python_bin: String::new(),
        uv_bin: "uv".to_string(),
        container_image: "ghcr.io/astral-sh/uv:python3.14-trixie-slim".to_string(),
        functions_framework_spec: "functions-framework==3.10.2".to_string(),
        cache_root: cache_root.clone(),
        passthrough_env: passthrough_env(&std::env::vars().collect(), &cache_root),
    };

    RegistryService::new(
        store,
        host_builder,
        container_builder,
        process_driver,
        container_driver,
        BuildModeSetting::Auto,
        docker_socket.to_string(),
        global_limit,
        data_dir,
        Duration::from_secs(300),
        defaults(),
        None,
        Arc::new(crate::logs::ring::LogStore::default()),
        python,
    )
}

fn image_request(name: &str, image_ref: &str) -> RegisterRequest {
    RegisterRequest {
        name: name.to_string(),
        trigger: Trigger::Http,
        source: Source::Image {
            image_ref: image_ref.to_string(),
        },
        runtime: None,
        entry_point: Some("hello".to_string()),
        env: BTreeMap::new(),
        timeout_secs: None,
        concurrency: None,
        memory_mib: None,
        min_instances: None,
        max_instances: None,
        idle_timeout_secs: None,
        queue_policy: None,
        queue_max_wait_secs: None,
    }
}

#[tokio::test]
async fn image_registration_with_unreachable_docker_returns_precondition_failed() {
    let data_dir = ok(tempfile::tempdir(), "tempdir");
    // `bollard::Docker::connect_with_socket` checks the path *exists*
    // synchronously at construction time (a nonexistent path fails
    // immediately, before this test even gets to exercise the registration
    // flow's own error handling) -- `/dev/null` exists, so construction
    // succeeds, and the actual connection attempt only fails once something
    // tries to speak the Docker Engine API over it (`is_available`,
    // `inspect_image`), which is exactly the precondition this test
    // exercises.
    let registry = registry_with_docker_socket(data_dir.path(), "unix:///dev/null");

    let result = registry
        .register(image_request("hello-img", "hello-http:dev"))
        .await;

    match result {
        Err(RegisterError::Unsupported { reason, needed }) => {
            assert!(
                reason.to_lowercase().contains("docker"),
                "expected the precondition failure to mention Docker, got: {reason:?}"
            );
            assert_eq!(needed, vec!["docker".to_string()]);
        }
        other => panic!("expected RegisterError::Unsupported (412), got {other:?}"),
    }
}

/// Requires a real, reachable Docker daemon -- gated on `OPEN_FUNCTIONS_TEST_DOCKER`,
/// matching this crate's other Docker-dependent integration tests
/// (`tests/runtime_container.rs`, `tests/build_container.rs`).
#[tokio::test]
async fn successful_image_registration_records_the_digest() {
    let Ok(_) = std::env::var("OPEN_FUNCTIONS_TEST_DOCKER") else {
        eprintln!(
            "skipping successful_image_registration_records_the_digest: \
             set OPEN_FUNCTIONS_TEST_DOCKER=1 to run against the real local Docker daemon"
        );
        return;
    };

    let data_dir = ok(tempfile::tempdir(), "tempdir");
    // The real local daemon, via bollard's own default socket resolution.
    let registry = registry_with_docker_socket(data_dir.path(), "");

    // A small, near-universally-already-pulled image is enough to prove
    // digest resolution end-to-end; this test doesn't need the image to
    // actually behave like a open-functions function (no instance is started here,
    // only registration -- `activate_revision`'s pool creation doesn't spawn
    // anything until the pool is first acquired from).
    let image_ref = "debian:bookworm-slim";
    let result = registry
        .register(image_request("hello-img-digest-test", image_ref))
        .await;
    let accepted = ok(result, "register");

    let revision = ok(
        registry.get_revision("hello-img-digest-test", accepted.revision),
        "get_revision",
    )
    .expect("revision should exist after a successful registration");
    let digest = revision
        .image_digest
        .expect("image-mode revision should have image_digest set");
    assert!(
        digest.starts_with("sha256:"),
        "expected a sha256 content digest, got: {digest:?}"
    );
    assert!(
        revision.artifact_path.is_none(),
        "image-mode revisions should never have an artifact_path"
    );

    let function = ok(registry.get("hello-img-digest-test"), "get")
        .expect("function should exist after registration");
    assert_eq!(function.state, crate::model::function::FunctionState::Ready);
    assert_eq!(function.current_revision, Some(accepted.revision));
}
