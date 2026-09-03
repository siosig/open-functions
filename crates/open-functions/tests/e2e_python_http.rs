//! End-to-end test for User Story 1's Python host-mode path (T023,
//! 002-python-runtime): spawns the real `open-functions` binary, deploys the
//! real `examples/hello-python-http` function against it via the admin API
//! (mirroring `e2e_http.rs`'s shape for the Rust path), and exercises the
//! full request/logs/redeploy/failure path through both listeners. Requires
//! a real `python3.14` + `uv` on `PATH` (skips otherwise).
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use serde_json::{Value, json};

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

/// A running `open-functions serve` subprocess, killed on drop so a test
/// failure (panic) never leaves an orphaned server behind. Mirrors
/// `e2e_http.rs`'s identically named helper (each integration-test binary
/// is its own compilation unit).
struct ServeProcess {
    child: Child,
    admin_url: String,
    invoke_url: String,
}

impl Drop for ServeProcess {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("crates/")
        .parent()
        .expect("workspace root")
        .to_path_buf()
}

async fn spawn_serve(
    data_dir: &std::path::Path,
    invoke_port: u16,
    admin_port: u16,
) -> ServeProcess {
    let bin = assert_cmd::cargo::cargo_bin("open-functions");
    let child = Command::new(bin)
        .args([
            "serve",
            "--data-dir",
            &data_dir.to_string_lossy(),
            "--invoke-listen",
            &format!("127.0.0.1:{invoke_port}"),
            "--admin-listen",
            &format!("127.0.0.1:{admin_port}"),
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn open-functions serve");

    let admin_url = format!("http://127.0.0.1:{admin_port}");
    let invoke_url = format!("http://127.0.0.1:{invoke_port}");

    let client = reqwest::Client::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        if let Ok(resp) = client.get(format!("{admin_url}/readyz")).send().await
            && resp.status().is_success()
        {
            break;
        }
        if tokio::time::Instant::now() >= deadline {
            panic!("open-functions serve did not become ready within 10s");
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    ServeProcess {
        child,
        admin_url,
        invoke_url,
    }
}

fn hello_http_dir() -> PathBuf {
    workspace_root().join("examples/hello-http")
}

fn hello_python_http_dir() -> PathBuf {
    workspace_root().join("examples/hello-python-http")
}

async fn wait_for_ready(client: &reqwest::Client, admin_url: &str, name: &str) -> Value {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(180);
    loop {
        let describe: Value = client
            .get(format!("{admin_url}/v1/functions/{name}"))
            .send()
            .await
            .expect("GET function")
            .json()
            .await
            .expect("describe JSON");
        if describe["state"] == "ready" {
            return describe;
        }
        if tokio::time::Instant::now() >= deadline {
            panic!("{name} did not become ready within 180s: {describe}");
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

#[tokio::test]
async fn python_host_function_matches_rust_reference_and_supports_the_full_lifecycle() {
    require_python314!();
    let data_dir = tempfile::tempdir().expect("tempdir");
    let server = spawn_serve(data_dir.path(), 28280, 28281).await;
    let client = reqwest::Client::new();

    // Rust reference, for the byte-identity comparison below.
    let rust_deploy = client
        .put(format!("{}/v1/functions/hello", server.admin_url))
        .json(&json!({
            "trigger": {"type": "http"},
            "source": {"kind": "dir", "path": hello_http_dir().to_string_lossy()},
            "entry_point": "hello",
        }))
        .timeout(Duration::from_secs(300))
        .send()
        .await
        .expect("PUT /v1/functions/hello");
    assert_eq!(rust_deploy.status(), 202);
    wait_for_ready(&client, &server.admin_url, "hello").await;

    // Python function, `runtime` omitted (auto-detected from `main.py`).
    let py_deploy = client
        .put(format!("{}/v1/functions/hello-py", server.admin_url))
        .json(&json!({
            "trigger": {"type": "http"},
            "source": {"kind": "dir", "path": hello_python_http_dir().to_string_lossy()},
            "entry_point": "hello",
        }))
        .timeout(Duration::from_secs(300))
        .send()
        .await
        .expect("PUT /v1/functions/hello-py");
    assert_eq!(py_deploy.status(), 202);
    let py_deploy_body: Value = py_deploy.json().await.expect("deploy response JSON");
    let first_build_id = py_deploy_body["build_id"]
        .as_str()
        .expect("build_id")
        .to_string();
    let describe_after_first_deploy = wait_for_ready(&client, &server.admin_url, "hello-py").await;
    assert_eq!(describe_after_first_deploy["runtime"], "python314");

    // Byte-identical body against the Rust reference.
    let rust_resp = client
        .get(format!("{}/hello/world?x=1", server.invoke_url))
        .send()
        .await
        .expect("GET /hello/world");
    assert_eq!(rust_resp.status(), 200);
    let rust_body = rust_resp.text().await.expect("rust body");

    let py_resp = client
        .get(format!("{}/hello-py/world?x=1", server.invoke_url))
        .send()
        .await
        .expect("GET /hello-py/world");
    assert_eq!(py_resp.status(), 200);
    let execution_id = py_resp
        .headers()
        .get("function-execution-id")
        .expect("Function-Execution-Id header present")
        .to_str()
        .expect("header is ASCII")
        .to_string();
    let py_body = py_resp.text().await.expect("python body");
    assert_eq!(
        py_body, rust_body,
        "Python and Rust hello functions must return byte-identical bodies for the same path/query"
    );
    assert_eq!(py_body, "Hello /world?x=1");

    // The request's execution_id reaches the function log ring buffer.
    let logs_body = client
        .get(format!(
            "{}/v1/functions/hello-py/logs?tail=20",
            server.admin_url
        ))
        .send()
        .await
        .expect("GET logs")
        .text()
        .await
        .expect("logs body");
    assert!(
        logs_body.contains(&execution_id),
        "expected the function log tail to contain execution_id {execution_id:?}: {logs_body}"
    );

    // Redeploy to a second revision. Note: `invoke.rs` gates every
    // invocation on `Function.state == Ready` (returning 503 otherwise),
    // independent of whether the prior revision's instance is still alive
    // and technically able to serve -- confirmed empirically (a busy-poll
    // across this same redeploy consistently saw 503s while `state =
    // building`, for reasons unrelated to Python: this gate is pre-existing
    // 001 behavior, not something 002 changes). So a redeploy has a real,
    // brief unavailability window; what this test asserts instead is that
    // invocation works again immediately once the redeploy completes.
    let redeploy = client
        .put(format!("{}/v1/functions/hello-py", server.admin_url))
        .json(&json!({
            "trigger": {"type": "http"},
            "source": {"kind": "dir", "path": hello_python_http_dir().to_string_lossy()},
            "entry_point": "hello",
            "env": {"REDEPLOY_MARKER": "1"},
        }))
        .timeout(Duration::from_secs(300))
        .send()
        .await
        .expect("redeploy PUT");
    assert_eq!(redeploy.status(), 202);
    let describe_after_redeploy = wait_for_ready(&client, &server.admin_url, "hello-py").await;
    assert_eq!(describe_after_redeploy["current_revision"], 2);

    let after_redeploy_resp = client
        .get(format!("{}/hello-py/after-redeploy", server.invoke_url))
        .send()
        .await
        .expect("GET /hello-py/after-redeploy");
    assert_eq!(after_redeploy_resp.status(), 200);

    // A broken entry_point fails the build (AttributeError, since main.py
    // has no `nope` attribute) but leaves the prior ready revision serving.
    let broken_deploy = client
        .put(format!("{}/v1/functions/hello-py", server.admin_url))
        .json(&json!({
            "trigger": {"type": "http"},
            "source": {"kind": "dir", "path": hello_python_http_dir().to_string_lossy()},
            "entry_point": "nope",
        }))
        .timeout(Duration::from_secs(300))
        .send()
        .await
        .expect("broken-entry_point PUT");
    assert_eq!(broken_deploy.status(), 202);
    let broken_deploy_body: Value = broken_deploy.json().await.expect("deploy response JSON");
    let broken_build_id = broken_deploy_body["build_id"]
        .as_str()
        .expect("build_id")
        .to_string();

    let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
    let describe_after_broken = loop {
        let build: Value = client
            .get(format!(
                "{}/v1/functions/hello-py/builds/{broken_build_id}",
                server.admin_url
            ))
            .send()
            .await
            .expect("GET build")
            .json()
            .await
            .expect("build JSON");
        if build["status"] == "failed" {
            break client
                .get(format!("{}/v1/functions/hello-py", server.admin_url))
                .send()
                .await
                .expect("describe after broken deploy")
                .json::<Value>()
                .await
                .expect("describe JSON");
        }
        if tokio::time::Instant::now() >= deadline {
            panic!("broken-entry_point build did not finish within 60s: {build}");
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    };
    assert_eq!(
        describe_after_broken["state"], "ready",
        "a failed redeploy must leave the prior ready revision serving (FR-007)"
    );
    assert_eq!(describe_after_broken["current_revision"], 2);
    // `PythonBuildError::EntryPoint`'s Display text names the failed step,
    // not the traceback itself (that's in the build log, checked below via
    // `builds/{id}/log`).
    assert!(
        describe_after_broken["last_error"]
            .as_str()
            .expect("last_error should be a string")
            .contains("entry point verification failed"),
        "last_error should mention the entry-point verification failure: {}",
        describe_after_broken["last_error"]
    );

    let broken_log = client
        .get(format!(
            "{}/v1/functions/hello-py/builds/{broken_build_id}/log",
            server.admin_url
        ))
        .send()
        .await
        .expect("GET build log")
        .text()
        .await
        .expect("build log body");
    assert!(
        broken_log.contains("Traceback") && broken_log.contains("AttributeError"),
        "build log should contain the entry-point verification traceback: {broken_log}"
    );

    // describe's runtime / revisions[].build_mode / builds[].tool.
    assert_eq!(describe_after_broken["runtime"], "python314");
    let revisions = describe_after_broken["revisions"]
        .as_array()
        .expect("revisions should be an array");
    let current_revision_entry = revisions
        .iter()
        .find(|r| r["number"] == 2)
        .expect("revision 2 should be present in revisions[]");
    assert_eq!(current_revision_entry["build_mode"], "host");

    let builds = describe_after_broken["builds"]
        .as_array()
        .expect("builds should be an array");
    let first_build_entry = builds
        .iter()
        .find(|b| b["id"] == first_build_id)
        .expect("the first successful build should be present in builds[]");
    let tool = first_build_entry["tool"]
        .as_str()
        .expect("tool should be a string for a succeeded build");
    assert!(
        tool == "uv" || tool == "pip",
        "expected tool to be uv or pip, got {tool:?}"
    );
}
