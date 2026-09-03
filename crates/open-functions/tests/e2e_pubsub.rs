//! End-to-end test for User Story 2 (T048, parametrized for 002-python-runtime's
//! T037): against a *real* open-pubusb instance, creates a topic, deploys a
//! Pub/Sub-triggered function with `--trigger-topic`, publishes a message,
//! confirms it reaches the function (via its structured stdout log line,
//! captured through open-functions's own process -- see
//! `runtime::process::spawn_log_drain`, which re-emits a function instance's
//! captured stdout as a `tracing` event on `open-functions serve`'s own
//! stdout), then deletes the function and confirms the open-pubusb
//! subscription is torn down. `run_pubsub_e2e_case` is the shared helper;
//! the Rust case (`examples/hello-pubsub`) and the Python case
//! (`examples/hello-python-pubsub`) both run it.
//!
//! Opt-in and skipped by default: it needs a real, running open-pubusb (this
//! workspace has no way to spin one up itself -- open-pubusb is a sibling
//! project). Set `OPEN_FUNCTIONS_TEST_OPEN_PUBUSB_URL` (e.g. `http://127.0.0.1:8085`) to run it.
//! When that open-pubusb runs in a container, also set
//! `OPEN_FUNCTIONS_TEST_INVOKE_BIND` / `OPEN_FUNCTIONS_TEST_PUSH_BASE_URL` so its
//! Push deliveries can route back to this process (see `spawn_serve`).
//!
//! Requires an open-pubusb build that honors `pushConfig` over REST (its
//! commit 7c29c98, "feat(rest): accept the full Subscription message on
//! create"). Against an earlier build, subscription creation succeeds but
//! silently drops the push config, so no delivery ever arrives and this test
//! fails on the marker-not-seen assertion.
//!
//! Topic/publish calls below follow the standard Pub/Sub REST shape (`PUT
//! .../topics/{t}` to create, `POST .../topics/{t}:publish` to publish) by
//! analogy with `OpenPubusbClient`'s own subscriptions REST subset
//! (`crates/open-functions-core/src/pubsub/client.rs`); adjust here if open-pubusb's actual
//! topics API differs.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdout, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use base64::Engine;
use serde_json::{Value, json};

fn python314_available() -> bool {
    std::process::Command::new("python3.14")
        .args(["-c", "import sys; print(sys.version_info[:2])"])
        .output()
        .map(|o| o.status.success() && String::from_utf8_lossy(&o.stdout).trim() == "(3, 14)")
        .unwrap_or(false)
}

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
    open_pubusb_url: &str,
    project: &str,
) -> ServeProcess {
    // open-pubusb has to be able to *reach back* to this process's invoke
    // listener to deliver a Push. That works out of the box only when both
    // run on the same host (the default below). When open-pubusb runs in a
    // container, loopback inside it is the container itself, so the bind
    // address and the advertised push URL both have to name an address the
    // container can route to (e.g. the docker bridge gateway, 172.17.0.1):
    //
    //   OPEN_FUNCTIONS_TEST_INVOKE_BIND=0.0.0.0 \
    //   OPEN_FUNCTIONS_TEST_PUSH_BASE_URL=http://172.17.0.1:28280 \
    //   OPEN_FUNCTIONS_TEST_OPEN_PUBUSB_URL=http://127.0.0.1:8085 cargo nextest run ...
    let invoke_bind = std::env::var("OPEN_FUNCTIONS_TEST_INVOKE_BIND")
        .unwrap_or_else(|_| "127.0.0.1".to_string());
    let bin = assert_cmd::cargo::cargo_bin("open-functions");
    let mut command = Command::new(bin);
    command
        .args([
            "serve",
            "--data-dir",
            &data_dir.to_string_lossy(),
            "--invoke-listen",
            &format!("{invoke_bind}:{invoke_port}"),
            "--admin-listen",
            &format!("127.0.0.1:{admin_port}"),
        ])
        .env("OPEN_FUNCTIONS__PUBSUB__ENABLED", "true")
        .env("OPEN_FUNCTIONS__PUBSUB__BASE_URL", open_pubusb_url)
        .env("OPEN_FUNCTIONS__PUBSUB__PROJECT", project)
        .env("OPEN_FUNCTIONS__PUBSUB__RETRY_INITIAL_SECS", "1")
        .env("OPEN_FUNCTIONS__PUBSUB__RETRY_MAX_SECS", "5")
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    if let Ok(push_base_url) = std::env::var("OPEN_FUNCTIONS_TEST_PUSH_BASE_URL") {
        command.env("OPEN_FUNCTIONS__PUBSUB__PUSH_BASE_URL", push_base_url);
    }
    let mut child = command.spawn().expect("spawn open-functions serve");

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

async fn create_topic(client: &reqwest::Client, open_pubusb_url: &str, project: &str, topic: &str) {
    let resp = client
        .put(format!(
            "{open_pubusb_url}/v1/projects/{project}/topics/{topic}"
        ))
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

async fn publish(
    client: &reqwest::Client,
    open_pubusb_url: &str,
    project: &str,
    topic: &str,
    data: &str,
) {
    let encoded = base64::engine::general_purpose::STANDARD.encode(data);
    let resp = client
        .post(format!(
            "{open_pubusb_url}/v1/projects/{project}/topics/{topic}:publish"
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

/// Shared E2E case body (T037): deploys `source_dir`/`entry_point` as a
/// Pub/Sub-triggered function against a real open-pubusb, publishes a
/// message, confirms the marker reaches the function's log, deletes the
/// function, and confirms the subscription disappears. `function_name` must
/// be unique per call within a test run (each spawns its own `serve` on its
/// own ports, so no two cases actually race, but the subscription/topic
/// names on the shared open-pubusb instance must not collide).
async fn run_pubsub_e2e_case(
    function_name: &str,
    source_dir: &Path,
    entry_point: &str,
    invoke_port: u16,
    admin_port: u16,
) {
    let open_pubusb_url = std::env::var("OPEN_FUNCTIONS_TEST_OPEN_PUBUSB_URL")
        .expect("caller must check OPEN_FUNCTIONS_TEST_OPEN_PUBUSB_URL before calling")
        .trim_end_matches('/')
        .to_string();
    let project = "local";
    let topic = format!("open-functions-e2e-{}", uuid::Uuid::new_v4().simple());

    let client = reqwest::Client::new();
    create_topic(&client, &open_pubusb_url, project, &topic).await;

    let data_dir = tempfile::tempdir().expect("tempdir");
    let server = spawn_serve(
        data_dir.path(),
        invoke_port,
        admin_port,
        &open_pubusb_url,
        project,
    )
    .await;

    let deploy_resp = client
        .put(format!("{}/v1/functions/{function_name}", server.admin_url))
        .json(&json!({
            "trigger": {"type": "pubsub", "topic": topic},
            "source": {"kind": "dir", "path": source_dir.to_string_lossy()},
            "entry_point": entry_point,
        }))
        .timeout(Duration::from_secs(300))
        .send()
        .await
        .expect("PUT /v1/functions/{function_name}");
    assert_eq!(deploy_resp.status(), 202);

    // Poll until the build finishes (ready) before publishing, so the first
    // push delivery doesn't race a not-yet-started instance.
    let ready_deadline = tokio::time::Instant::now() + Duration::from_secs(300);
    loop {
        let describe: Value = client
            .get(format!("{}/v1/functions/{function_name}", server.admin_url))
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
    publish(&client, &open_pubusb_url, project, &topic, &marker).await;

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
        .delete(format!("{}/v1/functions/{function_name}", server.admin_url))
        .send()
        .await
        .expect("DELETE function");
    assert_eq!(delete_resp.status(), 202);

    let sub_gone_deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    let sub_name = format!("open-functions-{function_name}");
    loop {
        let resp = client
            .get(format!(
                "{open_pubusb_url}/v1/projects/{project}/subscriptions/{sub_name}"
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

    // The topic is this test's own (uuid-suffixed) resource, and open-pubusb
    // is a long-lived shared instance here -- without this, every run leaves
    // another `open-functions-e2e-<uuid>` topic behind forever. Best-effort:
    // a failure to clean up must not fail an otherwise-passing test.
    let _ = client
        .delete(format!(
            "{open_pubusb_url}/v1/projects/{project}/topics/{topic}"
        ))
        .send()
        .await;
}

#[tokio::test]
async fn rust_topic_publish_reaches_function_and_delete_removes_subscription() {
    if std::env::var("OPEN_FUNCTIONS_TEST_OPEN_PUBUSB_URL").is_err() {
        eprintln!(
            "skipping rust_topic_publish_reaches_function_and_delete_removes_subscription: \
             set OPEN_FUNCTIONS_TEST_OPEN_PUBUSB_URL (e.g. http://127.0.0.1:8085) to run against a real open-pubusb"
        );
        return;
    }
    let source_dir = workspace_root().join("examples/hello-pubsub");
    run_pubsub_e2e_case("on-e2e", &source_dir, "on_msg", 28280, 28281).await;
}

#[tokio::test]
async fn python_topic_publish_reaches_function_and_delete_removes_subscription() {
    if std::env::var("OPEN_FUNCTIONS_TEST_OPEN_PUBUSB_URL").is_err() {
        eprintln!(
            "skipping python_topic_publish_reaches_function_and_delete_removes_subscription: \
             set OPEN_FUNCTIONS_TEST_OPEN_PUBUSB_URL (e.g. http://127.0.0.1:8085) to run against a real open-pubusb"
        );
        return;
    }
    if !python314_available() {
        eprintln!(
            "skipping python_topic_publish_reaches_function_and_delete_removes_subscription: \
             python3.14 not found on PATH"
        );
        return;
    }
    let source_dir = workspace_root().join("examples/hello-python-pubsub");
    run_pubsub_e2e_case("on-e2e-py", &source_dir, "on_msg", 28282, 28283).await;
}
