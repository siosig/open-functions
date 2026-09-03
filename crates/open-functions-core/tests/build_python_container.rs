//! Integration test (T033, 002-python-runtime) for `ContainerPythonBuilder`
//! against a real Docker daemon and a real pull of
//! `ghcr.io/astral-sh/uv:python3.14-trixie-slim`. Opt-in: set
//! `OPEN_FUNCTIONS_TEST_DOCKER=1`, matching this workspace's existing
//! opt-in-external-dependency convention (`runtime_container.rs`).
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeMap;
use std::os::unix::fs::MetadataExt;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use bollard::Docker;
use bollard::query_parameters::ListContainersOptionsBuilder;
use open_functions_core::build::python::container::ContainerPythonBuilder;
use open_functions_core::build::python::env::passthrough_env;
use open_functions_core::build::python::{Installer, PythonBuildRequest, PythonBuilder};
use open_functions_core::runtime::container::ContainerDriver;
use open_functions_core::runtime::docker::{LABEL_FUNCTION, connect};
use open_functions_core::runtime::{Driver, InstanceSpec, Launch};

const CONTAINER_IMAGE: &str = "ghcr.io/astral-sh/uv:python3.14-trixie-slim";

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

fn hello_python_http_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../examples/hello-python-http")
        .canonicalize()
        .expect("examples/hello-python-http should exist relative to open-functions-core")
}

fn docker_client() -> Docker {
    connect("").expect("connect() should always succeed (it doesn't touch the network)")
}

fn base_request(
    artifact_dir: &std::path::Path,
    cache_root: &std::path::Path,
) -> PythonBuildRequest {
    let host_env: BTreeMap<String, String> = std::env::vars().collect();
    PythonBuildRequest {
        function_name: "hello-py-container".to_string(),
        revision: 1,
        source_dir: hello_python_http_dir(),
        artifact_dir: artifact_dir.to_path_buf(),
        entry_point: "hello".to_string(),
        timeout: Duration::from_secs(180),
        cache_root: cache_root.to_path_buf(),
        functions_framework_spec: "functions-framework==3.10.2".to_string(),
        installer: Installer::Auto,
        python_bin: None,
        uv_bin: "uv".to_string(),
        container_image: CONTAINER_IMAGE.to_string(),
        passthrough_env: passthrough_env(&host_env, cache_root),
    }
}

#[tokio::test]
async fn container_build_produces_a_host_owned_venv_and_launches_via_container_driver() {
    require_docker_test!();
    let artifact_dir = tempfile::tempdir().expect("tempdir");
    let cache_root = tempfile::tempdir().expect("tempdir");
    let request = base_request(artifact_dir.path(), cache_root.path());
    let builder = ContainerPythonBuilder {
        docker: docker_client(),
    };

    let outcome = builder
        .build(&request)
        .await
        .expect("container build should succeed for examples/hello-python-http");
    assert_eq!(outcome.tool, "uv");

    // The venv (bind-mounted at /function inside the build container, owned
    // by the host uid via --user) must be visible and host-owned on the
    // host side too, at <artifact>/venv. `uv venv` links rather than copies
    // the interpreter, so `venv/bin/python3.14` is a symlink to
    // `/usr/local/bin/python3.14` -- only resolvable inside the build
    // image's own filesystem, not the host's (confirmed manually: `exists()`
    // on it from the host is a false negative even on a fully successful
    // build). `pyvenv.cfg` is always a real file, so check that instead.
    let venv_dir = artifact_dir.path().join("venv");
    assert!(
        venv_dir.join("pyvenv.cfg").exists(),
        "expected {venv_dir:?}/pyvenv.cfg to exist after a successful venv creation"
    );
    assert!(
        std::fs::symlink_metadata(venv_dir.join("bin/python3.14")).is_ok(),
        "expected venv/bin/python3.14 to exist as a symlink (even if its target only \
         resolves inside the build image)"
    );
    let meta = std::fs::metadata(&venv_dir).expect("stat venv dir");
    let host_uid = unsafe { libc_getuid() };
    assert_eq!(
        meta.uid(),
        host_uid,
        "venv directory should be owned by the host uid (--user was honored)"
    );

    let requirements =
        std::fs::read_to_string(artifact_dir.path().join("requirements.open-functions.txt"))
            .expect("read requirements.open-functions.txt");
    assert!(requirements.contains("functions-framework==3.10.2"));

    let log =
        std::fs::read_to_string(artifact_dir.path().join("build.log")).expect("read build.log");
    assert!(log.contains("== step: container-build =="), "log: {log}");

    // Launch::python_container against the same artifact dir, via a real
    // ContainerDriver -- the venv's own absolute paths (baked in as
    // /function/venv above) must still resolve once bind-mounted again here.
    let driver = ContainerDriver::new(docker_client());
    let spec = InstanceSpec {
        function_name: "hello-py-container".to_string(),
        revision: 1,
        entry_point: "hello".to_string(),
        signature_type: "http",
        env: open_functions_core::runtime::launch::python_instance_env(
            &sample_function(),
            open_functions_core::runtime::launch::PythonLaunchMode::Container,
            &PathBuf::from(open_functions_core::runtime::launch::CONTAINER_ARTIFACT_DIR)
                .join("venv"),
        ),
        memory_mib: 256,
        start_timeout: Duration::from_secs(20),
        launch: Launch::python_container(artifact_dir.path(), CONTAINER_IMAGE.to_string()),
        runtime_label: open_functions_core::model::runtime::RuntimeLabel::Python314,
    };

    let handle = driver
        .spawn(&spec)
        .await
        .expect("spawn should succeed for the container-built venv");

    // `docker ps` labels this container with LABEL_FUNCTION = function_name.
    let docker = docker_client();
    let mut filters = std::collections::HashMap::new();
    filters.insert(
        "label".to_string(),
        vec![format!("{LABEL_FUNCTION}=hello-py-container")],
    );
    let containers = docker
        .list_containers(Some(
            ListContainersOptionsBuilder::default()
                .filters(&filters)
                .build(),
        ))
        .await
        .expect("list_containers");
    assert!(
        !containers.is_empty(),
        "expected a running container labeled {LABEL_FUNCTION}=hello-py-container"
    );

    let resp = reqwest::get(format!("http://{}/x?y=1", handle.addr))
        .await
        .expect("HTTP GET to the container instance should succeed");
    assert_eq!(resp.status(), reqwest::StatusCode::OK);
    let body = resp.text().await.expect("body");
    assert_eq!(body, "Hello /x?y=1");

    let _ = handle.stop(Duration::from_secs(5)).await;
}

#[tokio::test]
async fn a_second_build_with_a_warm_uv_cache_is_not_slower() {
    require_docker_test!();
    let cache_root = tempfile::tempdir().expect("tempdir");
    let builder = ContainerPythonBuilder {
        docker: docker_client(),
    };

    let artifact1 = tempfile::tempdir().expect("tempdir");
    let request1 = base_request(artifact1.path(), cache_root.path());
    let start1 = Instant::now();
    builder
        .build(&request1)
        .await
        .expect("first build should succeed");
    let elapsed1 = start1.elapsed();

    let artifact2 = tempfile::tempdir().expect("tempdir");
    let mut request2 = base_request(artifact2.path(), cache_root.path());
    request2.function_name = "hello-py-container-2".to_string();
    let start2 = Instant::now();
    builder
        .build(&request2)
        .await
        .expect("second build should succeed");
    let elapsed2 = start2.elapsed();

    eprintln!("first build: {elapsed1:?}, second (warm UV_CACHE_DIR) build: {elapsed2:?}");
    assert!(
        elapsed2 < elapsed1 * 2,
        "a warm-cache second build should not be dramatically slower than the first \
         (first={elapsed1:?}, second={elapsed2:?})"
    );
}

fn sample_function() -> open_functions_core::model::function::Function {
    let now = chrono::Utc::now();
    open_functions_core::model::function::Function {
        name: "hello-py-container".to_string(),
        trigger: open_functions_core::model::function::Trigger::Http,
        runtime: Some(open_functions_core::model::runtime::Runtime::Python314),
        source: open_functions_core::model::function::Source::Dir {
            path: "unused".to_string(),
            bin: None,
        },
        env: BTreeMap::new(),
        entry_point: "hello".to_string(),
        timeout_secs: 60,
        concurrency: 1,
        memory_mib: 256,
        min_instances: 0,
        max_instances: 1,
        idle_timeout_secs: 900,
        queue_policy: open_functions_core::model::function::QueuePolicy::Wait,
        queue_max_wait_secs: 30,
        state: open_functions_core::model::function::FunctionState::Ready,
        current_revision: Some(1),
        last_error: None,
        created_at: now,
        updated_at: now,
    }
}

unsafe extern "C" {
    fn getuid() -> u32;
}
unsafe fn libc_getuid() -> u32 {
    unsafe { getuid() }
}
