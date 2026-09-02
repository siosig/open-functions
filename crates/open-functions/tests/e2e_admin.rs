//! End-to-end test for User Story 5 (T078): spawns the real `open-functions` binary,
//! deploys real functions via the admin API, and exercises the management
//! surface T081's admin endpoints provide -- `GET /v1/functions` (list),
//! `GET .../logs?tail` (ring buffer, T079), `GET .../builds/{id}/log[?follow]`,
//! `POST .../stop`, and `DELETE` (T080's complete delete flow: 404 after,
//! artifacts removed).
//!
//! Slow for the same reason `e2e_http.rs` is (a real `cargo build --release`
//! of `hello-http` per deploy, into a fresh `CARGO_TARGET_DIR`) -- this test
//! deploys three functions to exercise `list`.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use serde_json::{Value, json};

/// A running `open-functions serve` subprocess, killed on drop so a test failure
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

fn hello_http_dir() -> PathBuf {
    workspace_root().join("examples/hello-http")
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

async fn deploy(client: &reqwest::Client, server: &ServeProcess, name: &str) {
    let resp = client
        .put(format!("{}/v1/functions/{name}", server.admin_url))
        .json(&json!({
            "trigger": {"type": "http"},
            "source": {"kind": "dir", "path": hello_http_dir().to_string_lossy()},
            "entry_point": "hello",
        }))
        .timeout(Duration::from_secs(300))
        .send()
        .await
        .unwrap_or_else(|err| panic!("PUT /v1/functions/{name}: {err}"));
    assert_eq!(resp.status(), 202, "deploy {name:?} should be accepted");
}

#[tokio::test]
async fn management_lifecycle() {
    let data_dir = tempfile::tempdir().expect("tempdir");
    let server = spawn_serve(data_dir.path(), 28190, 28191).await;
    let client = reqwest::Client::new();

    for name in ["mgmt-a", "mgmt-b", "mgmt-c"] {
        deploy(&client, &server, name).await;
    }

    // `GET /v1/functions` lists all three.
    let list: Value = client
        .get(format!("{}/v1/functions", server.admin_url))
        .send()
        .await
        .expect("list")
        .json()
        .await
        .expect("list JSON");
    let functions = list["functions"].as_array().expect("functions array");
    assert_eq!(
        functions.len(),
        3,
        "expected 3 functions, got {functions:?}"
    );
    let names: Vec<&str> = functions
        .iter()
        .filter_map(|f| f["name"].as_str())
        .collect();
    for name in ["mgmt-a", "mgmt-b", "mgmt-c"] {
        assert!(
            names.contains(&name),
            "list should include {name:?}, got {names:?}"
        );
    }

    // `GET /v1/functions/{name}` field consistency.
    let describe: Value = client
        .get(format!("{}/v1/functions/mgmt-a", server.admin_url))
        .send()
        .await
        .expect("describe")
        .json()
        .await
        .expect("describe JSON");
    assert_eq!(describe["name"], "mgmt-a");
    assert_eq!(describe["state"], "ready");
    assert_eq!(describe["current_revision"], 1);
    let build_id = describe["current_build_id"]
        .as_str()
        .expect("current_build_id should be present for a source-mode deploy")
        .to_string();

    // `GET .../builds/{id}/log` (no follow): the full completed build log.
    let build_log = client
        .get(format!(
            "{}/v1/functions/mgmt-a/builds/{build_id}/log",
            server.admin_url
        ))
        .send()
        .await
        .expect("build log")
        .text()
        .await
        .expect("build log text");
    assert!(
        build_log.contains("Finished"),
        "expected a successful cargo build log, got: {build_log}"
    );

    // `GET .../builds/{id}/log?follow=true`: the build already finished by
    // the time this request is made (`register` runs synchronously to
    // completion, see registry::service's top doc comment), so the stream
    // must still deliver the full log and then end promptly rather than
    // hang waiting for more.
    let follow_resp = client
        .get(format!(
            "{}/v1/functions/mgmt-a/builds/{build_id}/log?follow=true",
            server.admin_url
        ))
        .timeout(Duration::from_secs(10))
        .send()
        .await
        .expect("follow build log");
    assert_eq!(follow_resp.status(), 200);
    let follow_text = follow_resp.text().await.expect("follow build log text");
    assert!(
        follow_text.contains("Finished"),
        "expected the full build log via follow, got: {follow_text}"
    );

    // Invoke mgmt-a to generate a real log line carrying an execution id.
    let invoke_resp = client
        .get(format!("{}/mgmt-a/logcheck", server.invoke_url))
        .send()
        .await
        .expect("invoke");
    assert_eq!(invoke_resp.status(), 200);

    // `GET .../logs?tail=10` (ring buffer, T079) contains that line.
    let mut found_execution_id = false;
    for _ in 0..30 {
        let logs_text = client
            .get(format!(
                "{}/v1/functions/mgmt-a/logs?tail=10",
                server.admin_url
            ))
            .send()
            .await
            .expect("logs")
            .text()
            .await
            .expect("logs text");
        let has_execution_id = logs_text.lines().any(|line| {
            serde_json::from_str::<Value>(line)
                .ok()
                .is_some_and(|record| record["execution_id"].is_string())
        });
        if has_execution_id {
            found_execution_id = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(
        found_execution_id,
        "expected a log line with a non-null execution_id after invoking mgmt-a"
    );

    // `POST .../stop` scales mgmt-a to zero without touching its registration.
    let stop_resp = client
        .post(format!("{}/v1/functions/mgmt-a/stop", server.admin_url))
        .send()
        .await
        .expect("stop");
    assert_eq!(stop_resp.status(), 200);
    let after_stop: Value = client
        .get(format!("{}/v1/functions/mgmt-a", server.admin_url))
        .send()
        .await
        .expect("describe after stop")
        .json()
        .await
        .expect("JSON");
    assert_eq!(after_stop["instances_running"], 0);
    assert_eq!(
        after_stop["state"], "ready",
        "stop must not change the function's own registration state"
    );

    // `DELETE /v1/functions/mgmt-b`: 202, then 404, then its artifacts
    // directory is gone (T080's complete delete flow).
    let delete_resp = client
        .delete(format!("{}/v1/functions/mgmt-b", server.admin_url))
        .send()
        .await
        .expect("delete");
    assert_eq!(delete_resp.status(), 202);

    let get_after_delete = client
        .get(format!("{}/v1/functions/mgmt-b", server.admin_url))
        .send()
        .await
        .expect("get after delete");
    assert_eq!(get_after_delete.status(), 404);

    let artifacts_dir = data_dir.path().join("artifacts").join("mgmt-b");
    assert!(
        !artifacts_dir.exists(),
        "artifacts dir for a deleted function should be removed, found {artifacts_dir:?}"
    );
}
