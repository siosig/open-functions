//! End-to-end test for User Story 5 (T050, 002-python-runtime): Rust and
//! Python functions coexisting -- `GET /v1/functions` distinguishes them by
//! `runtime`, a crashing Python function doesn't affect an unrelated Rust
//! function's success rate, and `/metrics` carries the `runtime` (and, for
//! builds, `tool`) label ops-config.md's 002 delta adds to every metric
//! listed there. Requires a real `python3.14` + `uv` on `PATH` (skips
//! otherwise).
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

/// Finds a metric sample line in `/metrics`' Prometheus text exposition
/// format whose metric name matches `metric` and whose label set contains
/// every `(key, value)` pair in `must_contain` -- a loose substring-based
/// check (good enough for text exposition format, avoids pulling in a full
/// Prometheus parser for one test).
fn metrics_has_series(body: &str, metric: &str, must_contain: &[(&str, &str)]) -> bool {
    body.lines().any(|line| {
        if !line.starts_with(metric) {
            return false;
        }
        must_contain
            .iter()
            .all(|(k, v)| line.contains(&format!("{k}=\"{v}\"")))
    })
}

#[tokio::test]
async fn rust_and_python_functions_coexist_with_isolated_crashes_and_labeled_metrics() {
    require_python314!();
    let data_dir = tempfile::tempdir().expect("tempdir");
    let server = spawn_serve(data_dir.path(), 28580, 28581).await;
    let client = reqwest::Client::new();

    // Rust function.
    let rust_deploy = client
        .put(format!("{}/v1/functions/hello", server.admin_url))
        .json(&json!({
            "trigger": {"type": "http"},
            "source": {"kind": "dir", "path": workspace_root().join("examples/hello-http").to_string_lossy()},
            "entry_point": "hello",
        }))
        .timeout(Duration::from_secs(300))
        .send()
        .await
        .expect("PUT /v1/functions/hello");
    assert_eq!(rust_deploy.status(), 202);
    wait_for_ready(&client, &server.admin_url, "hello").await;

    // Python function, deployed with CRASH=1 (crashes on every request, per
    // examples/hello-python-http's own doc comment).
    let py_deploy = client
        .put(format!("{}/v1/functions/hello-py", server.admin_url))
        .json(&json!({
            "trigger": {"type": "http"},
            "source": {"kind": "dir", "path": workspace_root().join("examples/hello-python-http").to_string_lossy()},
            "entry_point": "hello",
            "env": {"CRASH": "1"},
        }))
        .timeout(Duration::from_secs(300))
        .send()
        .await
        .expect("PUT /v1/functions/hello-py");
    assert_eq!(py_deploy.status(), 202);
    wait_for_ready(&client, &server.admin_url, "hello-py").await;

    // GET /v1/functions distinguishes them by runtime.
    let list: Value = client
        .get(format!("{}/v1/functions", server.admin_url))
        .send()
        .await
        .expect("GET /v1/functions")
        .json()
        .await
        .expect("list JSON");
    let functions = list["functions"].as_array().expect("functions array");
    let rust_entry = functions
        .iter()
        .find(|f| f["name"] == "hello")
        .expect("hello should be listed");
    assert_eq!(rust_entry["runtime"], "rust");
    let py_entry = functions
        .iter()
        .find(|f| f["name"] == "hello-py")
        .expect("hello-py should be listed");
    assert_eq!(py_entry["runtime"], "python314");

    // Crash the Python function once (CRASH=1 crashes the whole request,
    // giving a non-2xx status one way or another -- the exact status/timing
    // is unreliable, per runtime_python_process.rs's own documented finding
    // about gunicorn's worker-respawn behavior, so this just fires the
    // request with a bounded timeout and doesn't assert its own outcome).
    let _ = client
        .get(format!("{}/hello-py/crash-me", server.invoke_url))
        .timeout(Duration::from_secs(5))
        .send()
        .await;

    // The unrelated Rust function must stay 100% healthy: 20/20 successes.
    for i in 1..=20 {
        let resp = client
            .get(format!("{}/hello/world", server.invoke_url))
            .send()
            .await
            .unwrap_or_else(|err| panic!("call #{i} to hello failed to complete: {err}"));
        assert_eq!(resp.status(), 200, "call #{i} to hello should succeed");
    }

    let metrics_body = client
        .get(format!("{}/metrics", server.admin_url))
        .send()
        .await
        .expect("GET /metrics")
        .text()
        .await
        .expect("metrics body");

    assert!(
        metrics_has_series(
            &metrics_body,
            "open_functions_invocations_total",
            &[("function", "hello"), ("runtime", "rust")],
        ),
        "expected open_functions_invocations_total{{...,runtime=\"rust\"}} series"
    );
    assert!(
        metrics_has_series(
            &metrics_body,
            "open_functions_invocations_total",
            &[("function", "hello-py"), ("runtime", "python314")],
        ),
        "expected open_functions_invocations_total{{...,runtime=\"python314\"}} series"
    );
    assert!(
        metrics_has_series(
            &metrics_body,
            "open_functions_instances",
            &[("function", "hello-py"), ("runtime", "python314")],
        ),
        "expected open_functions_instances{{...,runtime=\"python314\"}} series"
    );
    assert!(
        metrics_has_series(
            &metrics_body,
            "open_functions_builds_total",
            &[
                ("function", "hello-py"),
                ("runtime", "python314"),
                ("tool", "uv"),
            ],
        ),
        "expected open_functions_builds_total{{...,runtime=\"python314\",tool=\"uv\"}} series"
    );
    assert!(
        metrics_has_series(
            &metrics_body,
            "open_functions_builds_total",
            &[
                ("function", "hello"),
                ("runtime", "rust"),
                ("tool", "cargo"),
            ],
        ),
        "expected open_functions_builds_total{{...,runtime=\"rust\",tool=\"cargo\"}} series"
    );
}
