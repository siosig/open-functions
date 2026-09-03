//! Integration test for startup restore (T057), covering
//! `RegistryService::restore()`'s behavior per data-model.md's Build entity
//! note and T060's task description: functions are recovered from redb
//! (not `register()`'s normal build pipeline — this test seeds the store
//! directly, as if a prior process had already registered/built these
//! functions before exiting), a build left `running` by an unclean shutdown
//! is reconciled to `failed`, a `ready` function with a missing artifact is
//! flagged `broken`, and `min_instances` is pre-warmed.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::Duration;

use open_functions_core::build::container::ContainerBuilder;
use open_functions_core::build::host_cargo::HostCargoBuilder;
use open_functions_core::build::python::Installer as PythonInstaller;
use open_functions_core::build::python::env::passthrough_env;
use open_functions_core::build::python::host::HostPythonBuilder;
use open_functions_core::logs::ring::LogStore;
use open_functions_core::model::build::{Build, BuildMode, BuildStatus};
use open_functions_core::model::function::{
    Function, FunctionState, QueuePolicy as ModelQueuePolicy, Source, Trigger,
};
use open_functions_core::model::revision::Revision;
use open_functions_core::model::runtime::Runtime;
use open_functions_core::registry::redb_store::RedbStore;
use open_functions_core::registry::service::{
    BuildModeSetting, PythonModeSetting, PythonSettings, RegistrationDefaults, RegistryService,
};
use open_functions_core::registry::store::Store;
use open_functions_core::runtime::cgroup::CgroupLimiter;
use open_functions_core::runtime::container::ContainerDriver;
use open_functions_core::runtime::docker;
use open_functions_core::runtime::process::ProcessDriver;

/// Builds `examples/hello-http` in release mode if not already built, so
/// this test is self-contained. Mirrors `runtime_process.rs`'s identically
/// named helper (each integration-test binary is its own compilation unit,
/// so this small duplication is the simplest option — see that file for why
/// `CARGO_TARGET_DIR` is explicitly cleared).
fn hello_http_binary() -> PathBuf {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let example_dir = manifest_dir
        .join("../../examples/hello-http")
        .canonicalize()
        .expect("examples/hello-http should exist relative to open-functions-core");
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
    assert!(binary.exists(), "hello-http binary missing at {binary:?}");
    binary
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

/// Returns a `RegistryService` alongside the same `Arc<dyn Store>` it was
/// built with, so the test can seed/inspect persisted state directly
/// (`RegistryService::store` is only `pub(crate)` — this integration test,
/// like any `tests/*.rs` file, compiles as a separate crate that only sees
/// `open-functions-core`'s public API).
fn new_registry(data_dir: &Path) -> (RegistryService, Arc<dyn Store>) {
    let db_path = data_dir.join("meta.redb");
    let store: Arc<dyn Store> = Arc::new(RedbStore::open(&db_path).expect("open redb store"));
    let host_builder = Arc::new(HostCargoBuilder {
        cargo_bin: "cargo".to_string(),
    });
    // Not exercised by any test in this file (none register an image-mode
    // function) — constructed anyway since `RegistryService::new` always
    // needs one; `docker::connect` is infallible even with no daemon
    // running (see `runtime::docker`'s own doc comments).
    let container_builder = Arc::new(ContainerBuilder {
        docker_socket: String::new(),
    });
    let process_driver = Arc::new(ProcessDriver {
        limiter: Arc::new(CgroupLimiter::probe()),
        log_store: Arc::new(LogStore::default()),
    });
    let container_driver = Arc::new(ContainerDriver {
        docker: docker::connect("").expect("connect (no daemon required to construct)"),
        log_store: Arc::new(LogStore::default()),
    });
    let global_limit = Arc::new(tokio::sync::Semaphore::new(32));
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

    let registry = RegistryService::new(
        Arc::clone(&store),
        host_builder,
        container_builder,
        process_driver,
        container_driver,
        BuildModeSetting::Host,
        String::new(),
        global_limit,
        data_dir,
        Duration::from_secs(1800),
        defaults(),
        None,
        Arc::new(LogStore::default()),
        python,
    );
    (registry, store)
}

/// A minimal, otherwise-valid `Function` in the given `state`, with
/// `min_instances` left at 0 (tests that need pre-warming override it).
fn base_function(name: &str, state: FunctionState, current_revision: Option<u32>) -> Function {
    let now = chrono::Utc::now();
    Function {
        name: name.to_string(),
        trigger: Trigger::Http,
        runtime: Some(Runtime::Rust),
        source: Source::Dir {
            path: "unused-for-restore".to_string(),
            bin: None,
        },
        env: BTreeMap::new(),
        entry_point: "hello".to_string(),
        timeout_secs: 60,
        concurrency: 1,
        memory_mib: 256,
        min_instances: 0,
        max_instances: 10,
        idle_timeout_secs: 900,
        queue_policy: ModelQueuePolicy::Wait,
        queue_max_wait_secs: 30,
        state,
        current_revision,
        last_error: None,
        created_at: now,
        updated_at: now,
    }
}

fn revision_for(function: &Function, number: u32, artifact_path: &Path) -> Revision {
    let mut snapshot = function.clone();
    snapshot.state = FunctionState::Ready;
    snapshot.current_revision = Some(number);
    Revision {
        function_name: function.name.clone(),
        number,
        artifact_path: Some(artifact_path.to_string_lossy().to_string()),
        image_digest: None,
        build_id: None,
        snapshot,
        build_mode: Some(BuildMode::Host),
        container_image: None,
        artifact_pruned: false,
        created_at: chrono::Utc::now(),
    }
}

#[tokio::test]
async fn ready_function_pool_is_recreated_from_persisted_revision() {
    let data_dir = tempfile::tempdir().expect("tempdir");
    let (registry, store) = new_registry(data_dir.path());
    let binary = hello_http_binary();

    let function = base_function("recreated", FunctionState::Ready, Some(1));
    let revision = revision_for(&function, 1, &binary);
    store.put_function(&function).expect("put_function");
    store.put_revision(&revision).expect("put_revision");

    assert!(
        registry.pool_for("recreated").await.is_none(),
        "pool should not exist before restore"
    );

    let report = registry.restore().await.expect("restore");
    assert_eq!(report.functions_restored, 1);
    assert!(report.broken_functions.is_empty());

    assert!(
        registry.pool_for("recreated").await.is_some(),
        "restore() should have recreated the InstancePool for a ready function"
    );
}

#[tokio::test]
async fn interrupted_build_without_prior_revision_marks_function_failed() {
    let data_dir = tempfile::tempdir().expect("tempdir");
    let (registry, store) = new_registry(data_dir.path());

    let function = base_function("brand-new", FunctionState::Building, None);
    store.put_function(&function).expect("put_function");
    store
        .put_build(&Build {
            id: "build-1".to_string(),
            function_name: "brand-new".to_string(),
            revision: 1,
            mode: BuildMode::Host,
            status: BuildStatus::Running,
            log_path: "unused".to_string(),
            exit_code: None,
            started_at: chrono::Utc::now(),
            finished_at: None,
            tool: Some("cargo".to_string()),
        })
        .expect("put_build");

    let report = registry.restore().await.expect("restore");
    assert_eq!(report.builds_marked_interrupted, 1);

    let stored_build = store
        .get_build("build-1")
        .expect("get_build")
        .expect("build should still exist");
    assert_eq!(stored_build.status, BuildStatus::Failed);

    let stored_function = store
        .get_function("brand-new")
        .expect("get_function")
        .expect("function should still exist");
    assert_eq!(stored_function.state, FunctionState::Failed);
    assert_eq!(
        stored_function.last_error.as_deref(),
        Some("interrupted by restart")
    );
}

#[tokio::test]
async fn interrupted_redeploy_build_keeps_previous_revision_ready() {
    let data_dir = tempfile::tempdir().expect("tempdir");
    let (registry, store) = new_registry(data_dir.path());
    let binary = hello_http_binary();

    // Revision 1 already `ready`; a redeploy to revision 2 was interrupted
    // mid-build (state left `building`, current_revision still 1).
    let mut function = base_function("redeploying", FunctionState::Building, Some(1));
    function.last_error = None;
    let revision1 = revision_for(&function, 1, &binary);
    store.put_function(&function).expect("put_function");
    store.put_revision(&revision1).expect("put_revision");
    store
        .put_build(&Build {
            id: "build-2".to_string(),
            function_name: "redeploying".to_string(),
            revision: 2,
            mode: BuildMode::Host,
            status: BuildStatus::Running,
            log_path: "unused".to_string(),
            exit_code: None,
            started_at: chrono::Utc::now(),
            finished_at: None,
            tool: Some("cargo".to_string()),
        })
        .expect("put_build");

    let report = registry.restore().await.expect("restore");
    assert_eq!(report.builds_marked_interrupted, 1);

    let stored_function = store
        .get_function("redeploying")
        .expect("get_function")
        .expect("function should still exist");
    // FR-007 / data-model.md: a failed (here: interrupted) re-deploy leaves
    // the prior ready revision serving, not `failed`.
    assert_eq!(stored_function.state, FunctionState::Ready);
    assert_eq!(stored_function.current_revision, Some(1));
    assert_eq!(
        stored_function.last_error.as_deref(),
        Some("interrupted by restart")
    );

    // The still-valid revision 1 pool should also have been recreated.
    assert!(registry.pool_for("redeploying").await.is_some());
}

#[tokio::test]
async fn ready_function_with_missing_artifact_is_reported_broken() {
    let data_dir = tempfile::tempdir().expect("tempdir");
    let (registry, store) = new_registry(data_dir.path());

    let function = base_function("broken-fn", FunctionState::Ready, Some(1));
    let missing = data_dir.path().join("artifacts/broken-fn/1/function");
    let revision = revision_for(&function, 1, &missing);
    store.put_function(&function).expect("put_function");
    store.put_revision(&revision).expect("put_revision");

    let report = registry.restore().await.expect("restore");
    assert_eq!(report.broken_functions, vec!["broken-fn".to_string()]);
}

#[tokio::test]
async fn restore_backfills_runtime_for_dir_sources_and_leaves_image_sources_none() {
    let data_dir = tempfile::tempdir().expect("tempdir");
    let (registry, store) = new_registry(data_dir.path());

    // A pre-002 Rust source-mode record persisted before `Function.runtime`
    // existed: `runtime: None`, exactly as `#[serde(default)]` would decode
    // it from an old JSON payload that never had the field.
    let mut dir_function = base_function("legacy-dir", FunctionState::Building, None);
    dir_function.runtime = None;
    store.put_function(&dir_function).expect("put_function");

    // A pre-002 image-mode record, also `runtime: None` — must stay `None`
    // (data-model.md: only `Source::Dir` gets backfilled to `Rust`).
    let mut image_function = base_function("legacy-image", FunctionState::Building, None);
    image_function.runtime = None;
    image_function.source = Source::Image {
        image_ref: "example.com/legacy-image:v1".to_string(),
    };
    store.put_function(&image_function).expect("put_function");

    registry.restore().await.expect("restore");

    let stored_dir = store
        .get_function("legacy-dir")
        .expect("get_function")
        .expect("function should still exist");
    assert_eq!(
        stored_dir.runtime,
        Some(Runtime::Rust),
        "a runtime-less Source::Dir record must be backfilled to Rust and persisted"
    );

    let stored_image = store
        .get_function("legacy-image")
        .expect("get_function")
        .expect("function should still exist");
    assert_eq!(
        stored_image.runtime, None,
        "a runtime-less Source::Image record must stay None"
    );
}

#[tokio::test]
async fn min_instances_are_prewarmed_on_restore() {
    let data_dir = tempfile::tempdir().expect("tempdir");
    let (registry, store) = new_registry(data_dir.path());
    let binary = hello_http_binary();

    let mut function = base_function("warm", FunctionState::Ready, Some(1));
    function.min_instances = 2;
    let mut revision = revision_for(&function, 1, &binary);
    revision.snapshot.min_instances = 2;
    store.put_function(&function).expect("put_function");
    store.put_revision(&revision).expect("put_revision");

    registry.restore().await.expect("restore");

    let pool = registry
        .pool_for("warm")
        .await
        .expect("pool should exist after restore");
    assert_eq!(
        pool.instance_count().await,
        2,
        "restore should have pre-warmed min_instances=2"
    );
}

// ---- T045 (002-python-runtime, US4) ----

fn python314_available() -> bool {
    std::process::Command::new("python3.14")
        .args(["-c", "import sys; print(sys.version_info[:2])"])
        .output()
        .map(|o| o.status.success() && String::from_utf8_lossy(&o.stdout).trim() == "(3, 14)")
        .unwrap_or(false)
}

macro_rules! require_python314 {
    () => {
        if !python314_available() {
            eprintln!("skipping {}: python3.14 not found on PATH", module_path!());
            return;
        }
    };
}

fn hello_python_http_dir() -> PathBuf {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    manifest_dir
        .join("../../examples/hello-python-http")
        .canonicalize()
        .expect("examples/hello-python-http should exist relative to open-functions-core")
}

/// A Python function's revision (`build_mode = host`) must be restored via
/// `Launch::python_host`, exactly as `activate_revision` would build it for
/// a fresh registration -- `restore_ready_function_pools` calls the very
/// same `activate_revision`, so this is really confirming no Rust-only
/// assumption snuck into that shared path, proven end-to-end with a real
/// build + a real post-restore invocation (not just "a pool object exists").
#[tokio::test]
async fn python_function_revision_is_restored_via_launch_python_host_and_actually_serves() {
    require_python314!();
    let data_dir = tempfile::tempdir().expect("tempdir");

    let (registry, store) = new_registry(data_dir.path());
    let req = open_functions_core::registry::service::RegisterRequest {
        name: "hello-py-restore".to_string(),
        trigger: Trigger::Http,
        source: Source::Dir {
            path: hello_python_http_dir().to_string_lossy().to_string(),
            bin: None,
        },
        runtime: None,
        entry_point: Some("hello".to_string()),
        ..Default::default()
    };
    registry
        .register(req)
        .await
        .expect("registering examples/hello-python-http should succeed");
    let function = registry
        .get("hello-py-restore")
        .expect("get")
        .expect("function should exist");
    assert_eq!(function.state, FunctionState::Ready);
    drop(registry);
    drop(store);

    // A fresh `RegistryService` against the same `data_dir`, simulating a
    // process restart: `pools` always starts empty (T060's own design), so
    // whatever comes back from `pool_for` below only exists because
    // `restore()` rebuilt it from the persisted `Revision`.
    let (restored_registry, _store2) = new_registry(data_dir.path());
    let report = restored_registry.restore().await.expect("restore");
    assert!(report.broken_functions.is_empty());

    let pool = restored_registry
        .pool_for("hello-py-restore")
        .await
        .expect("pool should have been recreated by restore()");
    let acquired = pool
        .acquire()
        .await
        .expect("acquire should spawn a fresh Launch::python_host instance");
    let resp = reqwest::get(format!("http://{}/x?y=1", acquired.addr))
        .await
        .expect("HTTP GET to the restored instance should succeed");
    assert_eq!(resp.status(), reqwest::StatusCode::OK);
    let body = resp.text().await.expect("body");
    assert_eq!(
        body, "Hello /x?y=1",
        "the restored instance must actually be running via Launch::python_host"
    );
}
