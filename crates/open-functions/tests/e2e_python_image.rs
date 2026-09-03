//! End-to-end test for User Story 3 (T040, 002-python-runtime): builds
//! `examples/hello-python-http`'s own `Dockerfile` for real, registers it
//! image-mode with an explicit `runtime = "python314"` declaration, and
//! confirms it serves through the existing (language-agnostic) image-mode
//! contract unchanged -- plus that a second image-mode registration with
//! `runtime` omitted reports `null` (display-only hint, never inferred for
//! image-mode). Mirrors `e2e_http.rs`'s `ServeProcess` shape.
//!
//! Opt-in and skipped by default (needs a real, reachable Docker daemon):
//! set `OPEN_FUNCTIONS_TEST_DOCKER=1` to run, matching this workspace's existing
//! opt-in-external-dependency convention.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use serde_json::{Value, json};

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

fn docker_build(context_dir: &std::path::Path, tag: &str) {
    let status = Command::new("docker")
        .args(["build", "-t", tag, &context_dir.to_string_lossy()])
        .status()
        .expect("invoke docker build");
    assert!(
        status.success(),
        "docker build -t {tag} {context_dir:?} failed"
    );
}

async fn wait_for_ready(client: &reqwest::Client, admin_url: &str, name: &str) -> Value {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
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
        assert!(
            tokio::time::Instant::now() < deadline,
            "{name} did not become ready within 30s: {describe}"
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

#[tokio::test]
async fn python_image_mode_serves_unchanged_and_runtime_is_display_only() {
    require_docker_test!();

    let image_tag = format!(
        "open-functions-test-hello-python-http-img:{}",
        uuid::Uuid::new_v4().simple()
    );
    docker_build(
        &workspace_root().join("examples/hello-python-http"),
        &image_tag,
    );

    let data_dir = tempfile::tempdir().expect("tempdir");
    let server = spawn_serve(data_dir.path(), 28380, 28381).await;
    let client = reqwest::Client::new();

    // Explicit runtime = python314 (display-only, never validated for image-mode).
    let deploy_resp = client
        .put(format!("{}/v1/functions/hello-py-img", server.admin_url))
        .json(&json!({
            "trigger": {"type": "http"},
            "source": {"kind": "image", "ref": image_tag},
            "runtime": "python314",
            "entry_point": "hello",
        }))
        .timeout(Duration::from_secs(60))
        .send()
        .await
        .expect("PUT /v1/functions/hello-py-img");
    assert_eq!(deploy_resp.status(), 202);

    let describe = wait_for_ready(&client, &server.admin_url, "hello-py-img").await;
    assert_eq!(describe["runtime"], "python314");

    let resp = client
        .get(format!("{}/hello-py-img/x", server.invoke_url))
        .send()
        .await
        .expect("GET /hello-py-img/x");
    assert_eq!(resp.status(), 200);
    assert!(resp.headers().contains_key("function-execution-id"));
    let body = resp.text().await.expect("body");
    assert_eq!(body, "Hello /x");

    // A second image-mode registration with `runtime` omitted: display-only,
    // never inferred -- describe reports it as `null`, not auto-detected.
    let deploy_resp2 = client
        .put(format!(
            "{}/v1/functions/hello-img-no-runtime",
            server.admin_url
        ))
        .json(&json!({
            "trigger": {"type": "http"},
            "source": {"kind": "image", "ref": image_tag},
            "entry_point": "hello",
        }))
        .timeout(Duration::from_secs(60))
        .send()
        .await
        .expect("PUT /v1/functions/hello-img-no-runtime");
    assert_eq!(deploy_resp2.status(), 202);

    let describe2 = wait_for_ready(&client, &server.admin_url, "hello-img-no-runtime").await;
    assert!(
        describe2["runtime"].is_null(),
        "expected runtime = null when omitted for an image-mode registration, got {}",
        describe2["runtime"]
    );

    let _ = Command::new("docker")
        .args(["rmi", "-f", &image_tag])
        .status();
}
