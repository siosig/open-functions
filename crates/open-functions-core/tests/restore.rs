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
use open_functions_core::logs::ring::LogStore;
use open_functions_core::model::build::{Build, BuildMode, BuildStatus};
use open_functions_core::model::function::{
    Function, FunctionState, QueuePolicy as ModelQueuePolicy, Source, Trigger,
};
use open_functions_core::model::revision::Revision;
use open_functions_core::registry::redb_store::RedbStore;
use open_functions_core::registry::service::{
    BuildModeSetting, RegistrationDefaults, RegistryService,
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
