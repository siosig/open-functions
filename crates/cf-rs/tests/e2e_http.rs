//! End-to-end test for User Story 1 (T031): spawns the real `cf-rs` binary,
//! deploys the real `examples/hello-http` function against it via the admin
//! API, and exercises the full request path through the invoke listener.
//!
//! This is slow (a real, COLD `cargo build --release` of `hello-http` per
//! deploy, into a fresh `CARGO_TARGET_DIR` under a throwaway `tempfile`
//! `data_dir` each test run — none of the host's incremental cache carries
//! over) and serial by nature (one `cf-rs serve` process, fixed ports chosen
//! to avoid colliding with other tests) — run with `--test-threads=1` if run
//! alongside other slow integration tests in the same crate. Deploy PUTs use
//! a 300s client timeout: a cold build of `hello-http` (which pulls in
//! `cf-rs-sdk`'s full dependency tree — axum, cloudevents-sdk, tokio, ...)
//! has been observed to take up to ~3 minutes depending on machine load.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use serde_json::{Value, json};

/// A running `cf-rs serve` subprocess, killed on drop so a test failure
/// (panic) never leaves an orphaned server behind.
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
    let bin = assert_cmd::cargo::cargo_bin("cf-rs");
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
        .expect("spawn cf-rs serve");

    let admin_url = format!("http://127.0.0.1:{admin_port}");
    let invoke_url = format!("http://127.0.0.1:{invoke_port}");

    // Poll /readyz instead of a fixed sleep, bounded so a genuine startup
    // failure fails the test promptly rather than hanging.
    let client = reqwest::Client::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        if let Ok(resp) = client.get(format!("{admin_url}/readyz")).send().await
            && resp.status().is_success()
        {
            break;
        }
        if tokio::time::Instant::now() >= deadline {
            panic!("cf-rs serve did not become ready within 10s");
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

#[tokio::test]
async fn deploy_build_and_invoke_round_trip() {
    let data_dir = tempfile::tempdir().expect("tempdir");
    let server = spawn_serve(data_dir.path(), 28180, 28181).await;
    let client = reqwest::Client::new();

    let deploy_resp = client
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
    assert_eq!(deploy_resp.status(), 202);
    let deploy_body: Value = deploy_resp.json().await.expect("deploy response JSON");
    assert_eq!(deploy_body["revision"], 1);

    let describe: Value = client
        .get(format!("{}/v1/functions/hello", server.admin_url))
        .send()
        .await
        .expect("GET function")
        .json()
        .await
        .expect("describe JSON");
    assert_eq!(describe["state"], "ready");
    assert_eq!(describe["current_revision"], 1);

    let invoke_resp = client
        .get(format!("{}/hello/world?x=1", server.invoke_url))
        .send()
        .await
        .expect("GET /hello/world");
    assert_eq!(invoke_resp.status(), 200);
    assert!(invoke_resp.headers().contains_key("function-execution-id"));
    let body = invoke_resp.text().await.expect("body");
    // Path-prefix resolution must strip "/hello" before forwarding.
    assert_eq!(body, "Hello /world?x=1");

    let unknown_resp = client
        .get(format!("{}/does-not-exist", server.invoke_url))
        .send()
        .await
        .expect("GET unknown function");
    assert_eq!(unknown_resp.status(), 404);
}

#[tokio::test]
async fn failed_redeploy_keeps_previous_revision_serving() {
    let data_dir = tempfile::tempdir().expect("tempdir");
    let server = spawn_serve(data_dir.path(), 28182, 28183).await;
    let client = reqwest::Client::new();

    // Copy hello-http so we can safely mutate it for the broken-source
    // deploy without touching the real examples/ tree; keep it as a sibling
    // under examples/ so its relative `path = "../../crates/cf-rs-sdk"`
    // dependency still resolves (see build_host.rs's integration test for
    // the same requirement).
    let examples_dir = workspace_root().join("examples");
    let broken_dir = tempfile::Builder::new()
        .prefix("hello-http-e2e-broken-")
        .tempdir_in(&examples_dir)
        .expect("tempdir_in examples/");
    copy_dir(&hello_http_dir(), broken_dir.path());

    // First deploy succeeds from the (still-working) copy.
    let deploy1 = client
        .put(format!("{}/v1/functions/flaky", server.admin_url))
        .json(&json!({
            "trigger": {"type": "http"},
            "source": {"kind": "dir", "path": broken_dir.path().to_string_lossy()},
            "entry_point": "hello",
        }))
        .timeout(Duration::from_secs(300))
        .send()
        .await
        .expect("first deploy");
    assert_eq!(deploy1.status(), 202);

    let ready: Value = client
        .get(format!("{}/v1/functions/flaky", server.admin_url))
        .send()
        .await
        .expect("describe after first deploy")
        .json()
        .await
        .expect("JSON");
    assert_eq!(ready["state"], "ready");
    assert_eq!(ready["current_revision"], 1);

    // Now break the same source directory in place and redeploy.
    let main_rs = broken_dir.path().join("src/main.rs");
    let original = std::fs::read_to_string(&main_rs).expect("read main.rs");
    std::fs::write(&main_rs, format!("this is not valid rust\n{original}"))
        .expect("write broken main.rs");

    let deploy2 = client
        .put(format!("{}/v1/functions/flaky", server.admin_url))
        .json(&json!({
            "trigger": {"type": "http"},
            "source": {"kind": "dir", "path": broken_dir.path().to_string_lossy()},
            "entry_point": "hello",
        }))
        .timeout(Duration::from_secs(300))
        .send()
        .await
        .expect("second (broken) deploy");
    assert_eq!(deploy2.status(), 202);

    let after_failed: Value = client
        .get(format!("{}/v1/functions/flaky", server.admin_url))
        .send()
        .await
        .expect("describe after failed deploy")
        .json()
        .await
        .expect("JSON");
    // FR-007: a failed re-deploy must leave the prior ready revision serving.
    assert_eq!(after_failed["state"], "ready");
    assert_eq!(after_failed["current_revision"], 1);
    assert!(after_failed["last_error"].is_string());

    let invoke_resp = client
        .get(format!("{}/flaky", server.invoke_url))
        .send()
        .await
        .expect("invoke after failed redeploy");
    assert_eq!(invoke_resp.status(), 200);
}

/// Minimal recursive copy sufficient for a small Rust source tree
/// (`Cargo.toml`, `src/`, `Dockerfile`) — not a general-purpose utility.
fn copy_dir(from: &std::path::Path, to: &std::path::Path) {
    for entry in std::fs::read_dir(from).expect("read_dir source") {
        let entry = entry.expect("dir entry");
        let dest = to.join(entry.file_name());
        let file_type = entry.file_type().expect("file_type");
        if file_type.is_dir() {
            std::fs::create_dir_all(&dest).expect("create_dir_all");
            copy_dir(&entry.path(), &dest);
        } else {
            std::fs::copy(entry.path(), &dest).expect("copy file");
        }
    }
}
