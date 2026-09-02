//! End-to-end test for User Story 2 (T048): against a *real* ps-rs instance,
//! creates a topic, deploys `examples/hello-pubsub` with `--trigger-topic`,
//! publishes a message, confirms it reaches the function (via its structured
//! stdout log line, captured through open-functions's own process — see
//! `runtime::process::spawn_log_drain`, which re-emits a function instance's
//! captured stdout as a `tracing` event on `open-functions serve`'s own stdout), then
//! deletes the function and confirms the ps-rs subscription is torn down.
//!
//! Opt-in and skipped by default: it needs a real, running ps-rs (this
//! workspace has no way to spin one up itself — ps-rs is a sibling project).
//! Set `OPEN_FUNCTIONS_TEST_PSRS_URL` (e.g. `http://127.0.0.1:8085`) to run it.
//!
//! Topic/publish calls below follow the standard Pub/Sub REST shape (`PUT
//! .../topics/{t}` to create, `POST .../topics/{t}:publish` to publish) by
//! analogy with `PsRsClient`'s own subscriptions REST subset
//! (`crates/open-functions-core/src/pubsub/client.rs`); adjust here if ps-rs's actual
//! topics API differs.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::process::{Child, ChildStdout, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use base64::Engine;
use serde_json::{Value, json};

/// A running `open-functions serve` subprocess with its stdout captured line-by-line
/// into `log_lines` (rather than discarded, as `e2e_http.rs`'s does) so the
/// test can poll for the pubsub-triggered function's log output, which
/// `open-functions serve` re-emits as its own `tracing` events.
struct ServeProcess {
    child: Child,
    admin_url: String,
    log_lines: Arc<Mutex<Vec<String>>>,
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

fn hello_pubsub_dir() -> PathBuf {
    workspace_root().join("examples/hello-pubsub")
}

fn spawn_stdout_collector(stdout: ChildStdout) -> Arc<Mutex<Vec<String>>> {
    let lines = Arc::new(Mutex::new(Vec::new()));
    let collector = Arc::clone(&lines);
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            collector
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(line);
        }
    });
    lines
}

async fn spawn_serve(
    data_dir: &std::path::Path,
    invoke_port: u16,
    admin_port: u16,
    psrs_url: &str,
    project: &str,
) -> ServeProcess {
    let bin = assert_cmd::cargo::cargo_bin("open-functions");
    let mut child = Command::new(bin)
        .args([
            "serve",
            "--data-dir",
            &data_dir.to_string_lossy(),
            "--invoke-listen",
            &format!("127.0.0.1:{invoke_port}"),
            "--admin-listen",
            &format!("127.0.0.1:{admin_port}"),
        ])
        .env("OPEN_FUNCTIONS__PUBSUB__ENABLED", "true")
        .env("OPEN_FUNCTIONS__PUBSUB__BASE_URL", psrs_url)
        .env("OPEN_FUNCTIONS__PUBSUB__PROJECT", project)
        .env("OPEN_FUNCTIONS__PUBSUB__RETRY_INITIAL_SECS", "1")
        .env("OPEN_FUNCTIONS__PUBSUB__RETRY_MAX_SECS", "5")
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn open-functions serve");

    let stdout = child.stdout.take().expect("child stdout");
    let log_lines = spawn_stdout_collector(stdout);

    let admin_url = format!("http://127.0.0.1:{admin_port}");
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
        log_lines,
    }
}

async fn create_topic(client: &reqwest::Client, psrs_url: &str, project: &str, topic: &str) {
    let resp = client
        .put(format!("{psrs_url}/v1/projects/{project}/topics/{topic}"))
        .json(&json!({}))
        .send()
        .await
        .expect("PUT topic");
    assert!(
        resp.status().is_success() || resp.status().as_u16() == 409,
        "unexpected status creating topic: {}",
        resp.status()
    );
}

async fn publish(client: &reqwest::Client, psrs_url: &str, project: &str, topic: &str, data: &str) {
    let encoded = base64::engine::general_purpose::STANDARD.encode(data);
    let resp = client
        .post(format!(
            "{psrs_url}/v1/projects/{project}/topics/{topic}:publish"
        ))
        .json(&json!({"messages": [{"data": encoded}]}))
        .send()
        .await
        .expect("publish");
    assert!(
        resp.status().is_success(),
        "publish failed: {}",
        resp.status()
    );
}

#[tokio::test]
async fn topic_publish_reaches_function_and_delete_removes_subscription() {
    let Ok(psrs_url) = std::env::var("OPEN_FUNCTIONS_TEST_PSRS_URL") else {
        eprintln!(
            "skipping topic_publish_reaches_function_and_delete_removes_subscription: \
             set OPEN_FUNCTIONS_TEST_PSRS_URL (e.g. http://127.0.0.1:8085) to run against a real ps-rs"
        );
        return;
    };
    let psrs_url = psrs_url.trim_end_matches('/').to_string();
    let project = "local";
    let topic = format!("open-functions-e2e-{}", uuid::Uuid::new_v4().simple());

    let client = reqwest::Client::new();
    create_topic(&client, &psrs_url, project, &topic).await;

    let data_dir = tempfile::tempdir().expect("tempdir");
    let server = spawn_serve(data_dir.path(), 28280, 28281, &psrs_url, project).await;

    let deploy_resp = client
        .put(format!("{}/v1/functions/on-e2e", server.admin_url))
        .json(&json!({
            "trigger": {"type": "pubsub", "topic": topic},
            "source": {"kind": "dir", "path": hello_pubsub_dir().to_string_lossy()},
            "entry_point": "on_msg",
        }))
        .timeout(Duration::from_secs(300))
        .send()
        .await
        .expect("PUT /v1/functions/on-e2e");
    assert_eq!(deploy_resp.status(), 202);

    // Poll until the build finishes (ready) before publishing, so the first
    // push delivery doesn't race a not-yet-started instance.
    let ready_deadline = tokio::time::Instant::now() + Duration::from_secs(300);
    loop {
        let describe: Value = client
            .get(format!("{}/v1/functions/on-e2e", server.admin_url))
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
            tokio::time::Instant::now() < ready_deadline,
            "function did not become ready in time: {describe}"
        );
        tokio::time::sleep(Duration::from_millis(300)).await;
    }

    let marker = format!("e2e-marker-{}", uuid::Uuid::new_v4().simple());
    publish(&client, &psrs_url, project, &topic, &marker).await;

    let seen_deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    let seen = loop {
        let found = server
            .log_lines
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .any(|line| line.contains(&marker));
        if found {
            break true;
        }
        if tokio::time::Instant::now() >= seen_deadline {
            break false;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    };
    assert!(
        seen,
        "published message marker {marker:?} did not reach the function's log within 30s"
    );

    let delete_resp = client
        .delete(format!("{}/v1/functions/on-e2e", server.admin_url))
        .send()
        .await
        .expect("DELETE function");
    assert_eq!(delete_resp.status(), 202);

    let sub_gone_deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    let sub_name = "open-functions-on-e2e";
    loop {
        let resp = client
            .get(format!(
                "{psrs_url}/v1/projects/{project}/subscriptions/{sub_name}"
            ))
            .send()
            .await
            .expect("GET subscription");
        if resp.status().as_u16() == 404 {
            break;
        }
        assert!(
            tokio::time::Instant::now() < sub_gone_deadline,
            "subscription {sub_name:?} was not removed within 15s of delete"
        );
        tokio::time::sleep(Duration::from_millis(300)).await;
    }
}
