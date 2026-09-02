//! End-to-end test for User Story 3's graceful shutdown (T058), per
//! spec.md's acceptance scenario 4 and ops-config.md's signal table: an
//! in-flight request gets to finish within `shutdown_grace_secs`, a new
//! connection attempted after the signal is refused, and the process exits
//! 0. Modeled on `e2e_http.rs`'s `spawn_serve` pattern.
//!
//! The instance-level SIGTERM → `stop_grace_secs` → SIGKILL mechanism (the
//! other half of T058's acceptance criteria) already has a dedicated,
//! faster unit-level test —
//! `crates/open-functions-core/tests/runtime_process.rs`'s
//! `graceful_stop_returns_promptly_within_grace` — so it isn't re-verified
//! here at the full-process level; `hello-http` has no `SIGTERM` handler of
//! its own, so it can't demonstrate the SIGKILL fallback via this binary
//! without a purpose-built slow-to-exit fixture.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::json;

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

#[tokio::test]
async fn in_flight_request_completes_new_connections_refused_and_exit_is_clean() {
    let data_dir = tempfile::tempdir().expect("tempdir");
    let mut server = spawn_serve(data_dir.path(), 28480, 28481).await;
    let client = reqwest::Client::new();

    let deploy_resp = client
        .put(format!("{}/v1/functions/slow", server.admin_url))
        .json(&json!({
            "trigger": {"type": "http"},
            "source": {"kind": "dir", "path": hello_http_dir().to_string_lossy()},
            "entry_point": "hello",
            "env": {"SLEEP_MS": "2000"},
        }))
        .timeout(Duration::from_secs(300))
        .send()
        .await
        .expect("PUT /v1/functions/slow");
    assert_eq!(deploy_resp.status(), 202);

    // Poll until ready (the build itself is fast; deploy_build_and_invoke_round_trip
    // in e2e_http.rs documents why a *cold* build can still take minutes on a
    // loaded machine, so this uses the same generous bound).
    let ready_deadline = Instant::now() + Duration::from_secs(300);
    loop {
        let describe: serde_json::Value = client
            .get(format!("{}/v1/functions/slow", server.admin_url))
            .send()
            .await
            .expect("describe")
            .json()
            .await
            .expect("describe JSON");
        if describe["state"] == "ready" {
            break;
        }
        assert!(
            Instant::now() < ready_deadline,
            "function never became ready: {describe}"
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }

    // Start the slow (2s) in-flight request in the background.
    let invoke_url = server.invoke_url.clone();
    let in_flight = tokio::spawn(async move {
        reqwest::Client::new()
            .get(format!("{invoke_url}/slow"))
            .timeout(Duration::from_secs(10))
            .send()
            .await
    });

    // Give it time to actually be accepted and dispatched to the instance
    // before sending the signal, so this genuinely exercises "in-flight
    // during shutdown" rather than racing the connection itself.
    tokio::time::sleep(Duration::from_millis(300)).await;

    let pid = server.child.id();
    let kill_status = Command::new("kill")
        .args(["-TERM", &pid.to_string()])
        .status()
        .expect("invoke `kill -TERM`");
    assert!(kill_status.success(), "kill -TERM {pid} failed to run");

    // A new connection attempted shortly after the signal must be refused
    // (or at least not succeed): the listener stops accepting once graceful
    // shutdown begins. Give the signal a brief moment to be handled first —
    // this isn't asserting sub-millisecond behavior, just that new work is
    // rejected well before the in-flight request's 2s completes.
    tokio::time::sleep(Duration::from_millis(200)).await;
    let new_conn_result = reqwest::Client::new()
        .get(format!("{}/slow", server.invoke_url))
        .timeout(Duration::from_secs(2))
        .send()
        .await;
    assert!(
        new_conn_result.is_err(),
        "a new connection attempted after SIGTERM should be refused, got: {new_conn_result:?}"
    );

    // The in-flight request must still complete successfully.
    let in_flight_resp = in_flight
        .await
        .expect("in-flight request task")
        .expect("in-flight request should complete, not be cut off");
    assert_eq!(in_flight_resp.status(), 200);
    let body = in_flight_resp.text().await.expect("body");
    // Path-prefix resolution strips `/slow` (the function name) before
    // forwarding, per function-contract.md — the function sees `/`, not
    // `/slow` (see `e2e_http.rs`'s identical `GET /hello/world` -> "Hello
    // /world" pattern).
    assert_eq!(body, "Hello /");

    // The process must exit cleanly (code 0) within a bounded time — well
    // under `shutdown_grace_secs`'s 30s default, since the only in-flight
    // work was the 2s request already awaited above.
    let exit_deadline = Duration::from_secs(15);
    let start = Instant::now();
    let status = loop {
        if let Some(status) = server.child.try_wait().expect("try_wait") {
            break status;
        }
        assert!(
            start.elapsed() < exit_deadline,
            "open-functions serve did not exit within {exit_deadline:?} of SIGTERM"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    };
    assert_eq!(
        status.code(),
        Some(0),
        "expected clean exit 0, got {status:?}"
    );
}
