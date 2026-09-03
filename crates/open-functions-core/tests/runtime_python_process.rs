//! Integration test (T021, 002-python-runtime): `Launch::python_host` run
//! through the real `ProcessDriver`, against a real `examples/hello-python-http`
//! venv built by `HostPythonBuilder` (same pipeline as T020's
//! `build_python_host.rs`). Requires a real `python3.14` + `uv` on `PATH`.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use open_functions_core::build::python::env::passthrough_env;
use open_functions_core::build::python::host::HostPythonBuilder;
use open_functions_core::build::python::{Installer, PythonBuildRequest, PythonBuilder};
use open_functions_core::logs::ring::LogStore;
use open_functions_core::model::function::{Function, FunctionState, QueuePolicy, Source, Trigger};
use open_functions_core::model::runtime::Runtime;
use open_functions_core::runtime::cgroup::CgroupLimiter;
use open_functions_core::runtime::launch::{Launch, PythonLaunchMode, python_instance_env};
use open_functions_core::runtime::process::ProcessDriver;
use open_functions_core::runtime::{Driver, InstanceSpec};

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

/// Builds a fresh venv for `examples/hello-python-http` into a per-call
/// tempdir (mirroring `build_python_host.rs`'s T020 tests). Each of this
/// file's 4 tests calls this independently rather than sharing one cached
/// artifact: a venv's console-script shebangs bake in its creation-time
/// absolute path, so a directory built once and reused/renamed across
/// `cargo nextest`'s per-test *processes* (nextest, unlike plain `cargo
/// test`, gives every test its own process, so there is no same-process
/// cache to share anyway) would break the moment its path changed. The
/// returned `TempDir` must be kept alive for as long as the artifact is
/// used -- its `Drop` removes the directory.
async fn build_hello_python_http() -> tempfile::TempDir {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let example_dir = manifest_dir
        .join("../../examples/hello-python-http")
        .canonicalize()
        .expect("examples/hello-python-http should exist relative to open-functions-core");
    let artifact_dir = tempfile::tempdir().expect("tempdir");
    let cache_root = manifest_dir.join("target/python-runtime-test-cache");
    let host_env: BTreeMap<String, String> = std::env::vars().collect();
    let request = PythonBuildRequest {
        function_name: "hello-py".to_string(),
        revision: 1,
        source_dir: example_dir,
        artifact_dir: artifact_dir.path().to_path_buf(),
        entry_point: "hello".to_string(),
        timeout: Duration::from_secs(180),
        cache_root: cache_root.clone(),
        functions_framework_spec: "functions-framework==3.10.2".to_string(),
        installer: Installer::Auto,
        python_bin: None,
        uv_bin: "uv".to_string(),
        container_image: "unused".to_string(),
        passthrough_env: passthrough_env(&host_env, &cache_root),
    };
    let builder = HostPythonBuilder {
        python_bin_override: String::new(),
        uv_bin: "uv".to_string(),
    };
    builder
        .build(&request)
        .await
        .expect("build examples/hello-python-http for runtime_python_process tests");
    artifact_dir
}

fn sample_function(concurrency: u32, timeout_secs: u32) -> Function {
    let now = chrono::Utc::now();
    Function {
        name: "hello-py".to_string(),
        trigger: Trigger::Http,
        runtime: Some(Runtime::Python314),
        source: Source::Dir {
            path: "unused-for-this-test".to_string(),
            bin: None,
        },
        env: BTreeMap::new(),
        entry_point: "hello".to_string(),
        timeout_secs,
        concurrency,
        memory_mib: 256,
        min_instances: 0,
        max_instances: 1,
        idle_timeout_secs: 900,
        queue_policy: QueuePolicy::Wait,
        queue_max_wait_secs: 30,
        state: FunctionState::Ready,
        current_revision: Some(1),
        last_error: None,
        created_at: now,
        updated_at: now,
    }
}

fn driver_with(log_store: Arc<LogStore>) -> ProcessDriver {
    ProcessDriver {
        limiter: Arc::new(CgroupLimiter::probe()),
        log_store,
    }
}

fn base_spec(
    artifact_dir: &std::path::Path,
    function: &Function,
    extra_env: &[(&str, &str)],
) -> InstanceSpec {
    let mut env = python_instance_env(
        function,
        PythonLaunchMode::Process,
        &artifact_dir.join("venv"),
    );
    for (k, v) in extra_env {
        env.insert((*k).to_string(), (*v).to_string());
    }
    InstanceSpec {
        function_name: function.name.clone(),
        revision: 1,
        entry_point: function.entry_point.clone(),
        signature_type: "http",
        env,
        memory_mib: function.memory_mib,
        start_timeout: Duration::from_secs(15),
        launch: Launch::python_host(artifact_dir),
        runtime_label: open_functions_core::model::runtime::RuntimeLabel::Python314,
    }
}

#[tokio::test]
async fn get_with_path_and_query_returns_the_same_body_as_the_rust_reference() {
    require_python314!();
    let artifact_dir_guard = build_hello_python_http().await;
    let artifact_dir = artifact_dir_guard.path();
    let function = sample_function(1, 60);
    let driver = driver_with(Arc::new(LogStore::default()));
    let spec = base_spec(artifact_dir, &function, &[]);

    let handle = driver
        .spawn(&spec)
        .await
        .expect("spawn should succeed for the built hello-python-http venv");

    let resp = reqwest::get(format!("http://{}/x?y=1", handle.addr))
        .await
        .expect("HTTP GET to the instance should succeed");
    assert_eq!(resp.status(), reqwest::StatusCode::OK);
    let body = resp.text().await.expect("response body should be text");
    assert_eq!(body, "Hello /x?y=1");

    let _ = handle.stop(Duration::from_secs(5)).await;
}

#[tokio::test]
async fn threads_2_serves_two_concurrent_sleep_requests_in_parallel() {
    require_python314!();
    let artifact_dir_guard = build_hello_python_http().await;
    let artifact_dir = artifact_dir_guard.path();
    let function = sample_function(2, 60);
    let driver = driver_with(Arc::new(LogStore::default()));
    let spec = base_spec(artifact_dir, &function, &[("SLEEP_MS", "500")]);

    let handle = driver
        .spawn(&spec)
        .await
        .expect("spawn should succeed for the built hello-python-http venv");
    let addr = handle.addr;

    let start = Instant::now();
    let (r1, r2) = tokio::join!(
        reqwest::get(format!("http://{addr}/a")),
        reqwest::get(format!("http://{addr}/b")),
    );
    let elapsed = start.elapsed();
    assert_eq!(
        r1.expect("request 1 should succeed").status(),
        reqwest::StatusCode::OK
    );
    assert_eq!(
        r2.expect("request 2 should succeed").status(),
        reqwest::StatusCode::OK
    );

    assert!(
        elapsed < Duration::from_millis(900),
        "THREADS=2 should serve two SLEEP_MS=500 requests concurrently (~0.5s), took {elapsed:?}"
    );

    let _ = handle.stop(Duration::from_secs(5)).await;
}

#[tokio::test]
async fn log_pipe_receives_a_line_whose_execution_id_matches_the_request_header() {
    require_python314!();
    let artifact_dir_guard = build_hello_python_http().await;
    let artifact_dir = artifact_dir_guard.path();
    let function = sample_function(1, 60);
    let log_store = Arc::new(LogStore::default());
    let driver = driver_with(Arc::clone(&log_store));
    let spec = base_spec(artifact_dir, &function, &[]);

    let handle = driver
        .spawn(&spec)
        .await
        .expect("spawn should succeed for the built hello-python-http venv");

    let execution_id = "abcd1234abcd1234abcd1234abcd1234";
    let client = reqwest::Client::new();
    let resp = client
        .get(format!("http://{}/exec-id-check", handle.addr))
        .header("Function-Execution-Id", execution_id)
        .send()
        .await
        .expect("HTTP GET to the instance should succeed");
    assert_eq!(resp.status(), reqwest::StatusCode::OK);

    let buffer = log_store.buffer_for(&function.name);
    let mut found = false;
    for _ in 0..50 {
        if buffer
            .tail(100)
            .iter()
            .any(|record| record.execution_id.as_deref() == Some(execution_id))
        {
            found = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(
        found,
        "expected a log line whose execution_id matches the Function-Execution-Id header, got: {:?}",
        buffer
            .tail(100)
            .iter()
            .map(|r| (&r.message, &r.execution_id))
            .collect::<Vec<_>>()
    );

    let _ = handle.stop(Duration::from_secs(5)).await;
}

/// `CRASH=1` here means something different for Python than for
/// `examples/hello-http`'s own crash test: Rust's `hello` binary calls
/// `std::process::exit(1)` on the single process `ProcessDriver` itself
/// launched, so that process's `child.wait()` resolves immediately
/// (`InstanceExit::Crashed`). `functions-framework`'s `WORKERS=1` instead
/// runs under a gunicorn *master* process that forks a worker subprocess;
/// `os._exit(1)` inside the worker kills only that worker, and gunicorn
/// immediately respawns a fresh one under the same still-running master --
/// confirmed empirically (`ps` shows a new worker pid within ~1s; gunicorn's
/// own stderr logs `Worker (pid:...) exited with code 1.`). Because `CRASH`
/// is an *instance-level* env var (not a one-shot trigger), every respawned
/// worker crashes again on its very next request too -- there is no actual
/// "recovery" to observe here, only a genuine crash-respawn loop. The
/// process `ProcessDriver` is watching (the master) never exits on its own,
/// so `InstanceExit::Crashed` is not the right assertion either. What this
/// test verifies instead: the crash is real at the OS-process level (the
/// request genuinely fails to complete normally), which is the meaningful
/// part of "crash detection" for a multi-worker runtime.
#[tokio::test]
async fn crash_env_causes_the_request_to_fail_rather_than_succeed() {
    require_python314!();
    let artifact_dir_guard = build_hello_python_http().await;
    let artifact_dir = artifact_dir_guard.path();
    let function = sample_function(1, 60);
    let driver = driver_with(Arc::new(LogStore::default()));
    let spec = base_spec(artifact_dir, &function, &[("CRASH", "1")]);

    let handle = driver
        .spawn(&spec)
        .await
        .expect("spawn should succeed even though the handler will crash on request");
    let addr = handle.addr;

    // The crashing worker's connection can hang rather than reset promptly
    // (confirmed empirically), so this needs a bounded client-side timeout;
    // either a client-side error/timeout or a non-200 response counts as
    // "the request did not complete normally", which is the only outcome
    // that's actually guaranteed here.
    let result = reqwest::Client::new()
        .get(format!("http://{addr}/crash-me"))
        .timeout(Duration::from_secs(5))
        .send()
        .await;
    let request_completed_normally =
        matches!(&result, Ok(resp) if resp.status() == reqwest::StatusCode::OK);
    assert!(
        !request_completed_normally,
        "expected the CRASH=1 request to fail (timeout/connection error/non-200), got {result:?}"
    );

    let _ = handle.stop(Duration::from_secs(5)).await;
}

// ---- T036 (002-python-runtime, US2): examples/hello-python-pubsub ----

/// `data:"aGVsbG8="` base64-decodes to `"hello"`, matching this file's
/// expected `data=hello` log line -- mirrors
/// `open-functions-sdk/tests/cloudevent_contract.rs`'s `PUBSUB_DATA_JSON`.
const PUBSUB_DATA_JSON: &str = r#"{"message":{"data":"aGVsbG8=","attributes":{"k":"v"},"messageId":"1234567890","publishTime":"2026-09-02T01:02:03.456Z","orderingKey":""},"subscription":"projects/local/subscriptions/open-functions-on-msg"}"#;

async fn build_hello_python_pubsub() -> tempfile::TempDir {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let example_dir = manifest_dir
        .join("../../examples/hello-python-pubsub")
        .canonicalize()
        .expect("examples/hello-python-pubsub should exist relative to open-functions-core");
    let artifact_dir = tempfile::tempdir().expect("tempdir");
    let cache_root = manifest_dir.join("target/python-runtime-test-cache");
    let host_env: BTreeMap<String, String> = std::env::vars().collect();
    let request = PythonBuildRequest {
        function_name: "on-orders-py".to_string(),
        revision: 1,
        source_dir: example_dir,
        artifact_dir: artifact_dir.path().to_path_buf(),
        entry_point: "on_msg".to_string(),
        timeout: Duration::from_secs(180),
        cache_root: cache_root.clone(),
        functions_framework_spec: "functions-framework==3.10.2".to_string(),
        installer: Installer::Auto,
        python_bin: None,
        uv_bin: "uv".to_string(),
        container_image: "unused".to_string(),
        passthrough_env: passthrough_env(&host_env, &cache_root),
    };
    let builder = HostPythonBuilder {
        python_bin_override: String::new(),
        uv_bin: "uv".to_string(),
    };
    builder
        .build(&request)
        .await
        .expect("build examples/hello-python-pubsub for runtime_python_process tests");
    artifact_dir
}

fn pubsub_spec(artifact_dir: &std::path::Path, extra_env: &[(&str, &str)]) -> InstanceSpec {
    let function = sample_function(1, 60);
    let mut env = python_instance_env(
        &function,
        PythonLaunchMode::Process,
        &artifact_dir.join("venv"),
    );
    for (k, v) in extra_env {
        env.insert((*k).to_string(), (*v).to_string());
    }
    InstanceSpec {
        function_name: "on-orders-py".to_string(),
        revision: 1,
        entry_point: "on_msg".to_string(),
        signature_type: "cloudevent",
        env,
        memory_mib: 256,
        start_timeout: Duration::from_secs(15),
        launch: Launch::python_host(artifact_dir),
        runtime_label: open_functions_core::model::runtime::RuntimeLabel::Python314,
    }
}

#[tokio::test]
async fn pubsub_binary_mode_cloudevent_returns_200_and_logs_the_decoded_message() {
    require_python314!();
    let artifact_dir_guard = build_hello_python_pubsub().await;
    let artifact_dir = artifact_dir_guard.path();
    let log_store = Arc::new(LogStore::default());
    let driver = driver_with(Arc::clone(&log_store));
    let spec = pubsub_spec(artifact_dir, &[]);

    let handle = driver
        .spawn(&spec)
        .await
        .expect("spawn should succeed for the built hello-python-pubsub venv");

    let resp = reqwest::Client::new()
        .post(format!("http://{}/", handle.addr))
        .header("ce-specversion", "1.0")
        .header("ce-id", "1234567890")
        .header(
            "ce-source",
            "//pubsub.googleapis.com/projects/local/topics/orders-py",
        )
        .header("ce-type", "google.cloud.pubsub.topic.v1.messagePublished")
        .header("ce-time", "2026-09-02T01:02:03.456Z")
        .header("content-type", "application/json")
        .body(PUBSUB_DATA_JSON)
        .send()
        .await
        .expect("binary-mode CloudEvent POST should succeed");
    assert_eq!(resp.status(), reqwest::StatusCode::OK);

    let buffer = log_store.buffer_for("on-orders-py");
    let mut found = false;
    for _ in 0..50 {
        if buffer.tail(100).iter().any(|record| {
            record
                .message
                .contains("type=google.cloud.pubsub.topic.v1.messagePublished")
                && record.message.contains("message_id=1234567890")
                && record.message.contains("data=hello")
        }) {
            found = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(
        found,
        "expected a log line with type=/message_id=/data=hello, got: {:?}",
        buffer
            .tail(100)
            .iter()
            .map(|r| &r.message)
            .collect::<Vec<_>>()
    );

    let _ = handle.stop(Duration::from_secs(5)).await;
}

#[tokio::test]
async fn pubsub_structured_mode_cloudevent_also_returns_200() {
    require_python314!();
    let artifact_dir_guard = build_hello_python_pubsub().await;
    let artifact_dir = artifact_dir_guard.path();
    let driver = driver_with(Arc::new(LogStore::default()));
    let spec = pubsub_spec(artifact_dir, &[]);

    let handle = driver
        .spawn(&spec)
        .await
        .expect("spawn should succeed for the built hello-python-pubsub venv");

    let envelope = serde_json::json!({
        "specversion": "1.0",
        "id": "1234567890",
        "source": "//pubsub.googleapis.com/projects/local/topics/orders-py",
        "type": "google.cloud.pubsub.topic.v1.messagePublished",
        "time": "2026-09-02T01:02:03.456Z",
        "datacontenttype": "application/json",
        "data": {
            "message": {
                "data": "aGVsbG8=",
                "attributes": {"k": "v"},
                "messageId": "1234567890",
                "publishTime": "2026-09-02T01:02:03.456Z",
                "orderingKey": ""
            },
            "subscription": "projects/local/subscriptions/open-functions-on-msg"
        }
    });

    let resp = reqwest::Client::new()
        .post(format!("http://{}/", handle.addr))
        .header("content-type", "application/cloudevents+json")
        .body(envelope.to_string())
        .send()
        .await
        .expect("structured-mode CloudEvent POST should succeed");
    assert_eq!(resp.status(), reqwest::StatusCode::OK);

    let _ = handle.stop(Duration::from_secs(5)).await;
}

#[tokio::test]
async fn pubsub_fail_env_returns_500() {
    require_python314!();
    let artifact_dir_guard = build_hello_python_pubsub().await;
    let artifact_dir = artifact_dir_guard.path();
    let log_store = Arc::new(LogStore::default());
    let driver = driver_with(Arc::clone(&log_store));
    let spec = pubsub_spec(artifact_dir, &[("FAIL", "1")]);

    let handle = driver
        .spawn(&spec)
        .await
        .expect("spawn should succeed for the built hello-python-pubsub venv");

    let execution_id = "1234abcd1234abcd1234abcd1234abcd";
    let resp = reqwest::Client::new()
        .post(format!("http://{}/", handle.addr))
        .header("ce-specversion", "1.0")
        .header("ce-id", "1234567890")
        .header(
            "ce-source",
            "//pubsub.googleapis.com/projects/local/topics/orders-py",
        )
        .header("ce-type", "google.cloud.pubsub.topic.v1.messagePublished")
        .header("ce-time", "2026-09-02T01:02:03.456Z")
        .header("content-type", "application/json")
        .header("Function-Execution-Id", execution_id)
        .body(PUBSUB_DATA_JSON)
        .send()
        .await
        .expect("POST should succeed even though the handler raises");
    assert_eq!(resp.status(), reqwest::StatusCode::INTERNAL_SERVER_ERROR);

    // T038: functions-framework wraps an uncaught exception's traceback as a
    // *single* `LOG_EXECUTION_ID` JSON line on stderr (embedded `\n`s inside
    // its "message" field, confirmed empirically -- not literal newlines on
    // the wire), with no "severity" key. `logs::pipe::parse_line`'s existing
    // fallback (`stream.default_severity()`, stderr -> ERROR) already
    // classifies this correctly with no changes needed in `pipe.rs`; this
    // asserts that finding stays true.
    let buffer = log_store.buffer_for("on-orders-py");
    let mut found = false;
    for _ in 0..50 {
        if buffer.tail(100).iter().any(|record| {
            record.severity == "ERROR"
                && record.execution_id.as_deref() == Some(execution_id)
                && record.message.contains("Traceback")
                && record.message.contains("RuntimeError: simulated failure")
        }) {
            found = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(
        found,
        "expected an ERROR-severity log record with the traceback and matching execution_id, got: {:?}",
        buffer
            .tail(100)
            .iter()
            .map(|r| (&r.severity, &r.execution_id, &r.message))
            .collect::<Vec<_>>()
    );

    let _ = handle.stop(Duration::from_secs(5)).await;
}
