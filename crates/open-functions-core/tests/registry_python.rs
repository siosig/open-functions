//! Integration test (T022, 002-python-runtime) for the registry's Python
//! registration flow (`RegistryService::register` -> `register_source` ->
//! `register_source_python`), against an in-memory store, a fake
//! `PythonBuilder` (deterministic, no real `python3.14`/`uv` needed), and a
//! fake `Driver` (never actually invoked -- `min_instances = 0` in every
//! `RegisterRequest` here, so `activate_revision` only sets up a pool
//! template, it never spawns an instance).
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use tokio::sync::Mutex;

use open_functions_core::build::container::ContainerBuilder;
use open_functions_core::build::host_cargo::HostCargoBuilder;
use open_functions_core::build::python::{
    Installer, PythonBuildError, PythonBuildOutcome, PythonBuildRequest, PythonBuilder,
};
use open_functions_core::model::function::{
    FunctionState, QueuePolicy as ModelQueuePolicy, Source, Trigger,
};
use open_functions_core::model::runtime::Runtime;
use open_functions_core::registry::memory::MemoryStore;
use open_functions_core::registry::service::{
    BuildModeSetting, PythonModeSetting, PythonSettings, RegisterError, RegisterRequest,
    RegistrationDefaults, RegistryService,
};
use open_functions_core::runtime::{Driver, DriverError, InstanceHandle, InstanceSpec};

/// Scripted outcome for one call to [`FakePythonBuilder::build`].
enum FakeOutcome {
    Succeed { tool: &'static str },
    FailInstall,
}

/// A `PythonBuilder` whose `is_available`/`build` results are scripted, so
/// this test never needs a real `python3.14`/`uv`. `outcomes` is consumed
/// front-to-back, one entry per `build()` call; the last entry repeats once
/// the queue is exhausted (so tests that only care about the first call
/// don't have to queue more than they need).
struct FakePythonBuilder {
    available: AtomicBool,
    outcomes: Mutex<Vec<FakeOutcome>>,
}

impl FakePythonBuilder {
    fn new(available: bool, outcomes: Vec<FakeOutcome>) -> Self {
        Self {
            available: AtomicBool::new(available),
            outcomes: Mutex::new(outcomes),
        }
    }
}

#[async_trait::async_trait]
impl PythonBuilder for FakePythonBuilder {
    async fn is_available(&self) -> bool {
        self.available.load(Ordering::SeqCst)
    }

    async fn build(
        &self,
        _request: &PythonBuildRequest,
    ) -> Result<PythonBuildOutcome, PythonBuildError> {
        let mut outcomes = self.outcomes.lock().await;
        let outcome = if outcomes.len() > 1 {
            outcomes.remove(0)
        } else {
            match outcomes.first() {
                Some(FakeOutcome::Succeed { tool }) => FakeOutcome::Succeed { tool },
                Some(FakeOutcome::FailInstall) => FakeOutcome::FailInstall,
                None => FakeOutcome::FailInstall,
            }
        };
        match outcome {
            FakeOutcome::Succeed { tool } => Ok(PythonBuildOutcome {
                tool: tool.to_string(),
            }),
            FakeOutcome::FailInstall => Err(PythonBuildError::Install(
                Path::new("build.log").to_path_buf(),
            )),
        }
    }
}

/// Never actually called (`min_instances = 0` throughout this file), so any
/// invocation is a test bug -- panicking makes that loud rather than
/// silently spawning something unexpected.
struct FakeDriver;

#[async_trait::async_trait]
impl Driver for FakeDriver {
    async fn spawn(&self, _spec: &InstanceSpec) -> Result<InstanceHandle, DriverError> {
        panic!("FakeDriver::spawn should never be called in this test (min_instances = 0)");
    }
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

fn registry_with(python_available: bool, outcomes: Vec<FakeOutcome>) -> RegistryService {
    let store = Arc::new(MemoryStore::new());
    let host_builder = Arc::new(HostCargoBuilder {
        cargo_bin: "cargo".to_string(),
    });
    let container_builder = Arc::new(ContainerBuilder {
        docker_socket: String::new(),
    });
    let process_driver = Arc::new(FakeDriver);
    let container_driver = Arc::new(FakeDriver);
    let global_limit = Arc::new(tokio::sync::Semaphore::new(8));
    let data_dir = std::env::temp_dir().join(format!(
        "open-functions-registry-python-test-{}",
        uuid::Uuid::new_v4().simple()
    ));

    let python = PythonSettings {
        mode: PythonModeSetting::Host,
        host_builder: Arc::new(FakePythonBuilder::new(python_available, outcomes)),
        container_builder: None,
        installer: Installer::Auto,
        python_bin: String::new(),
        uv_bin: "uv".to_string(),
        container_image: "ghcr.io/astral-sh/uv:python3.14-trixie-slim".to_string(),
        functions_framework_spec: "functions-framework==3.10.2".to_string(),
        cache_root: data_dir.join("cache"),
        passthrough_env: BTreeMap::new(),
    };

    RegistryService::new(
        store,
        host_builder,
        container_builder,
        process_driver,
        container_driver,
        BuildModeSetting::Host,
        String::new(),
        global_limit,
        &data_dir,
        Duration::from_secs(60),
        defaults(),
        None,
        Arc::new(open_functions_core::logs::ring::LogStore::default()),
        python,
    )
}

/// Writes a minimal `main.py` (and, if `with_cargo_toml`, an empty
/// `Cargo.toml` alongside it) into a fresh tempdir, returning it.
fn source_dir(with_main_py: bool, with_cargo_toml: bool) -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("tempdir");
    if with_main_py {
        std::fs::write(
            dir.path().join("main.py"),
            "def hello(request):\n    return 'hi'\n",
        )
        .expect("write main.py");
    }
    if with_cargo_toml {
        std::fs::write(
            dir.path().join("Cargo.toml"),
            "[package]\nname = \"unused\"\nversion = \"0.0.0\"\n",
        )
        .expect("write Cargo.toml");
    }
    dir
}

fn register_request(name: &str, source_path: &Path, runtime: Option<Runtime>) -> RegisterRequest {
    RegisterRequest {
        name: name.to_string(),
        trigger: Trigger::Http,
        source: Source::Dir {
            path: source_path.to_string_lossy().to_string(),
            bin: None,
        },
        runtime,
        entry_point: Some("hello".to_string()),
        ..Default::default()
    }
}

#[tokio::test]
async fn main_py_only_directory_with_runtime_omitted_infers_python_and_records_tool_and_build_mode()
{
    let registry = registry_with(true, vec![FakeOutcome::Succeed { tool: "uv" }]);
    let dir = source_dir(true, false);

    let accepted = registry
        .register(register_request("hello-py", dir.path(), None))
        .await
        .expect("registration should succeed");

    let function = registry
        .get("hello-py")
        .expect("get")
        .expect("function should exist");
    assert_eq!(function.runtime, Some(Runtime::Python314));
    assert_eq!(function.state, FunctionState::Ready);
    assert_eq!(function.current_revision, Some(accepted.revision));

    let build = registry
        .get_build(&accepted.build_id)
        .expect("get_build")
        .expect("build should exist");
    assert_eq!(build.tool.as_deref(), Some("uv"));

    let revision = registry
        .get_revision("hello-py", accepted.revision)
        .expect("get_revision")
        .expect("revision should exist");
    assert_eq!(
        revision.build_mode,
        Some(open_functions_core::model::build::BuildMode::Host)
    );
}

#[tokio::test]
async fn explicit_runtime_rust_without_cargo_toml_is_rejected_as_invalid_argument() {
    let registry = registry_with(true, vec![FakeOutcome::Succeed { tool: "uv" }]);
    let dir = source_dir(true, false); // main.py only, no Cargo.toml

    let err = registry
        .register(register_request(
            "bad-rust",
            dir.path(),
            Some(Runtime::Rust),
        ))
        .await
        .expect_err("registration should be rejected");
    assert!(
        matches!(err, RegisterError::InvalidRuntime(_)),
        "expected InvalidRuntime, got {err:?}"
    );
}

#[tokio::test]
async fn both_cargo_toml_and_main_py_present_with_runtime_omitted_is_rejected_as_ambiguous() {
    let registry = registry_with(true, vec![FakeOutcome::Succeed { tool: "uv" }]);
    let dir = source_dir(true, true);

    let err = registry
        .register(register_request("ambiguous-fn", dir.path(), None))
        .await
        .expect_err("registration should be rejected");
    match &err {
        RegisterError::InvalidRuntime(reason) => {
            assert!(
                reason.to_lowercase().contains("ambiguous"),
                "expected an 'ambiguous' message, got {reason:?}"
            );
        }
        other => panic!("expected InvalidRuntime, got {other:?}"),
    }
}

#[tokio::test]
async fn python_mode_host_with_unavailable_builder_is_rejected_as_unsupported_naming_python314() {
    let registry = registry_with(false, vec![FakeOutcome::Succeed { tool: "uv" }]);
    let dir = source_dir(true, false);

    let err = registry
        .register(register_request("no-python", dir.path(), None))
        .await
        .expect_err("registration should be rejected");
    match &err {
        RegisterError::Unsupported { needed, .. } => {
            assert_eq!(
                needed,
                &vec!["python3.14".to_string()],
                "expected details.needed to name python3.14"
            );
        }
        other => panic!("expected Unsupported, got {other:?}"),
    }
}

#[tokio::test]
async fn a_failed_redeploy_build_keeps_the_previous_revision_ready_and_records_last_error() {
    let registry = registry_with(
        true,
        vec![
            FakeOutcome::Succeed { tool: "uv" },
            FakeOutcome::FailInstall,
        ],
    );
    let dir = source_dir(true, false);

    let first = registry
        .register(register_request("redeploy-me", dir.path(), None))
        .await
        .expect("first registration should succeed");

    let second = registry
        .register(register_request("redeploy-me", dir.path(), None))
        .await
        .expect("register() itself returns Ok even though the build fails in the background-equivalent flow");
    assert_eq!(
        second.revision,
        first.revision + 1,
        "a new revision number should still be allocated for the failed attempt"
    );

    let function = registry
        .get("redeploy-me")
        .expect("get")
        .expect("function should still exist");
    assert_eq!(
        function.state,
        FunctionState::Ready,
        "the prior ready revision must keep serving after a failed redeploy (FR-007)"
    );
    assert_eq!(
        function.current_revision,
        Some(first.revision),
        "current_revision must stay pinned to the last successful build"
    );
    assert!(
        function.last_error.is_some(),
        "a failed redeploy must record last_error"
    );
}

// ---- T041 (002-python-runtime, US3): image-mode doesn't validate runtime ----

/// `register_image` talks to a real Docker daemon (`resolve_image_digest`
/// bypasses the `Driver` abstraction entirely -- it constructs its own
/// `bollard::Docker` client from `docker_socket`), so this needs
/// `OPEN_FUNCTIONS_TEST_DOCKER=1`, matching this workspace's
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

fn image_register_request(
    name: &str,
    image_ref: &str,
    runtime: Option<Runtime>,
) -> RegisterRequest {
    RegisterRequest {
        name: name.to_string(),
        trigger: Trigger::Http,
        source: Source::Image {
            image_ref: image_ref.to_string(),
        },
        runtime,
        entry_point: Some("not-a-python-identifier".to_string()),
        ..Default::default()
    }
}

#[tokio::test]
async fn image_mode_accepts_either_declared_runtime_and_persists_it_unvalidated() {
    require_docker_test!();
    let registry = registry_with(true, vec![]);

    for runtime in [Runtime::Rust, Runtime::Python314] {
        let name = format!("image-fn-{}", runtime.label());
        let accepted = registry
            .register(image_register_request(
                &name,
                "python:3.14-slim",
                Some(runtime),
            ))
            .await
            .unwrap_or_else(|err| {
                panic!("image-mode registration with runtime = {runtime:?} should be accepted (not validated): {err:?}")
            });
        assert_eq!(accepted.build_id, "", "image-mode has no build step");

        let function = registry
            .get(&name)
            .expect("get")
            .expect("function should exist");
        assert_eq!(
            function.runtime,
            Some(runtime),
            "the declared runtime must be persisted as-is on Function.runtime"
        );
        assert_eq!(function.state, FunctionState::Ready);
    }
}
