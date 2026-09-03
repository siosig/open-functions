//! Integration test (T044, 002-python-runtime US4) for
//! `RegistryService::prune_old_artifacts` (FR-108a): registering rev1..rev3
//! of a function must delete rev1's artifact directory once rev3 activates
//! (keeping only current and current-1), mark `Revision.artifact_pruned`,
//! leave the `Build` record intact, and never fail the registration itself
//! if the deletion fails. In-memory store + fake `Builder`/`PythonBuilder`
//! that actually write real files under the real artifacts directory (so
//! pruning has something real to delete and verify), + a fake `Driver`
//! (never actually invoked -- `min_instances = 0` throughout).
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;
use std::time::Duration;

use open_functions_core::build::python::env::passthrough_env;
use open_functions_core::build::python::{
    Installer, PythonBuildError, PythonBuildOutcome, PythonBuildRequest, PythonBuilder,
};
use open_functions_core::build::{BuildError, BuildRequest, Builder};
use open_functions_core::model::function::{QueuePolicy as ModelQueuePolicy, Source, Trigger};
use open_functions_core::model::runtime::Runtime;
use open_functions_core::registry::memory::MemoryStore;
use open_functions_core::registry::service::{
    BuildModeSetting, PythonModeSetting, PythonSettings, RegisterRequest, RegistrationDefaults,
    RegistryService,
};
use open_functions_core::runtime::{Driver, DriverError, InstanceHandle, InstanceSpec};

/// Writes a dummy artifact + log file wherever the real `HostCargoBuilder`
/// would, so `prune_old_artifacts` has something real to delete.
struct FakeRustBuilder;

#[async_trait::async_trait]
impl Builder for FakeRustBuilder {
    async fn build(&self, request: &BuildRequest) -> Result<(), BuildError> {
        if let Some(parent) = request.artifact_path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        tokio::fs::write(&request.artifact_path, b"fake-executable").await?;
        tokio::fs::write(&request.log_path, b"fake build log").await?;
        Ok(())
    }
}

/// Writes a dummy file into `artifact_dir`, so `prune_old_artifacts` has
/// something real to delete -- mirrors what the real `HostPythonBuilder`'s
/// `snapshot`/`resolve_requirements` steps leave behind.
struct FakePythonBuilder;

#[async_trait::async_trait]
impl PythonBuilder for FakePythonBuilder {
    async fn build(
        &self,
        request: &PythonBuildRequest,
    ) -> Result<PythonBuildOutcome, PythonBuildError> {
        tokio::fs::create_dir_all(&request.artifact_dir).await?;
        tokio::fs::write(request.artifact_dir.join("marker.txt"), b"fake-venv-marker").await?;
        Ok(PythonBuildOutcome {
            tool: "uv".to_string(),
        })
    }
}

/// Never actually called (`min_instances = 0` throughout this file).
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

/// Returns the registry alongside its `data_dir`, so the test can inspect
/// `<data_dir>/artifacts/<name>/<rev>/` directly.
fn registry_with(data_dir: &std::path::Path) -> RegistryService {
    let store = Arc::new(MemoryStore::new());
    let host_builder = Arc::new(FakeRustBuilder);
    let container_builder = Arc::new(FakeRustBuilder);
    let process_driver = Arc::new(FakeDriver);
    let container_driver = Arc::new(FakeDriver);
    let global_limit = Arc::new(tokio::sync::Semaphore::new(8));
    let cache_root = data_dir.join("cache");

    let python = PythonSettings {
        mode: PythonModeSetting::Host,
        host_builder: Arc::new(FakePythonBuilder),
        container_builder: None,
        installer: Installer::Auto,
        python_bin: String::new(),
        uv_bin: "uv".to_string(),
        container_image: "unused".to_string(),
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
        BuildModeSetting::Host,
        String::new(),
        global_limit,
        data_dir,
        Duration::from_secs(60),
        defaults(),
        None,
        Arc::new(open_functions_core::logs::ring::LogStore::default()),
        python,
    )
}

fn rust_source_dir() -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(
        dir.path().join("Cargo.toml"),
        "[package]\nname = \"unused\"\nversion = \"0.0.0\"\n",
    )
    .expect("write Cargo.toml");
    dir
}

fn python_source_dir() -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(
        dir.path().join("main.py"),
        "def hello(request):\n    return 'hi'\n",
    )
    .expect("write main.py");
    dir
}

fn register_request(
    name: &str,
    source_path: &std::path::Path,
    runtime: Runtime,
) -> RegisterRequest {
    RegisterRequest {
        name: name.to_string(),
        trigger: Trigger::Http,
        source: Source::Dir {
            path: source_path.to_string_lossy().to_string(),
            bin: None,
        },
        runtime: Some(runtime),
        entry_point: Some("hello".to_string()),
        ..Default::default()
    }
}

#[tokio::test]
async fn rust_rev1_is_pruned_once_rev3_activates() {
    let data_dir = tempfile::tempdir().expect("tempdir");
    let registry = registry_with(data_dir.path());
    let source = rust_source_dir();

    for _ in 0..3 {
        registry
            .register(register_request("hello", source.path(), Runtime::Rust))
            .await
            .expect("registration should succeed");
    }

    let rev1_dir = data_dir.path().join("artifacts/hello/1");
    let rev2_dir = data_dir.path().join("artifacts/hello/2");
    assert!(
        !rev1_dir.exists(),
        "rev1's artifact directory should have been pruned"
    );
    assert!(
        rev2_dir.exists(),
        "rev2's artifact directory must NOT be pruned (current - 1)"
    );

    let rev1 = registry
        .get_revision("hello", 1)
        .expect("get_revision")
        .expect("revision 1 record should still exist");
    assert!(rev1.artifact_pruned);
    assert!(rev1.artifact_path.is_none());

    let rev2 = registry
        .get_revision("hello", 2)
        .expect("get_revision")
        .expect("revision 2 record should still exist");
    assert!(!rev2.artifact_pruned);
    assert!(rev2.artifact_path.is_some());

    // Build records are never deleted, only the artifact directory.
    let builds = registry.list_builds_for("hello").expect("list_builds_for");
    assert_eq!(builds.len(), 3, "all 3 Build records must still exist");
}

#[tokio::test]
async fn python_rev1_is_pruned_once_rev3_activates() {
    let data_dir = tempfile::tempdir().expect("tempdir");
    let registry = registry_with(data_dir.path());
    let source = python_source_dir();

    for _ in 0..3 {
        registry
            .register(register_request(
                "hello-py",
                source.path(),
                Runtime::Python314,
            ))
            .await
            .expect("registration should succeed");
    }

    let rev1_dir = data_dir.path().join("artifacts/hello-py/1");
    let rev2_dir = data_dir.path().join("artifacts/hello-py/2");
    assert!(
        !rev1_dir.exists(),
        "rev1's artifact directory should have been pruned"
    );
    assert!(
        rev2_dir.exists(),
        "rev2's artifact directory must NOT be pruned (current - 1)"
    );

    let rev1 = registry
        .get_revision("hello-py", 1)
        .expect("get_revision")
        .expect("revision 1 record should still exist");
    assert!(rev1.artifact_pruned);
    assert!(rev1.artifact_path.is_none());

    let builds = registry
        .list_builds_for("hello-py")
        .expect("list_builds_for");
    assert_eq!(builds.len(), 3, "all 3 Build records must still exist");
}

#[cfg(unix)]
#[tokio::test]
async fn a_deletion_failure_does_not_fail_registration_and_leaves_artifact_pruned_false() {
    use std::os::unix::fs::PermissionsExt;

    let data_dir = tempfile::tempdir().expect("tempdir");
    let registry = registry_with(data_dir.path());
    let source = rust_source_dir();

    registry
        .register(register_request("hello", source.path(), Runtime::Rust))
        .await
        .expect("first registration should succeed");
    registry
        .register(register_request("hello", source.path(), Runtime::Rust))
        .await
        .expect("second registration should succeed");

    // Removing a directory *entry* needs write+exec on its *parent* --
    // strip that from artifacts/hello/ so rev1's directory can't be deleted,
    // simulating a real filesystem-permission prune failure.
    let hello_artifacts_dir = data_dir.path().join("artifacts/hello");
    let mut perms = std::fs::metadata(&hello_artifacts_dir)
        .expect("stat artifacts/hello")
        .permissions();
    perms.set_mode(0o555);
    std::fs::set_permissions(&hello_artifacts_dir, perms.clone())
        .expect("chmod artifacts/hello to read-only");

    let third = registry
        .register(register_request("hello", source.path(), Runtime::Rust))
        .await;

    // Restore permissions unconditionally before any assertion, so a
    // panic below doesn't leave a read-only directory behind for `tempfile`
    // to fail to clean up.
    perms.set_mode(0o755);
    let _ = std::fs::set_permissions(&hello_artifacts_dir, perms);

    third.expect("registration must succeed even though pruning rev1 fails");

    let rev1 = registry
        .get_revision("hello", 1)
        .expect("get_revision")
        .expect("revision 1 record should still exist");
    assert!(
        !rev1.artifact_pruned,
        "artifact_pruned must stay false when deletion actually failed, so a later \
         successful activation can retry"
    );
    assert!(
        data_dir.path().join("artifacts/hello/1").exists(),
        "rev1's artifact directory should still exist (deletion failed)"
    );
}
