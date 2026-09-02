//! `cf-rs fn deploy` (T043): registers a function via the admin API and
//! follows the build to completion (polling `GET .../builds/{id}` and
//! printing the log — see `crates/cf-rs/src/server/admin.rs` for why this is
//! a poll-and-print loop rather than a true chunked `follow` stream, an MVP
//! simplification for User Story 1; `--no-wait` skips this and returns as
//! soon as the build is accepted).

use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use serde_json::{Value, json};

use super::client::AdminClient;

#[derive(clap::Args)]
pub struct DeployArgs {
    /// Function name.
    name: String,
    /// Rust source directory (mutually exclusive with `--image`).
    #[arg(long)]
    source: Option<PathBuf>,
    /// Container image reference (mutually exclusive with `--source`).
    #[arg(long)]
    image: Option<String>,
    #[arg(long)]
    trigger_http: bool,
    #[arg(long)]
    trigger_topic: Option<String>,
    #[arg(long, default_value = "function")]
    entry_point: String,
    #[arg(long = "set-env", value_parser = parse_env_pair)]
    set_env: Vec<(String, String)>,
    #[arg(long)]
    timeout: Option<u32>,
    #[arg(long)]
    concurrency: Option<u32>,
    #[arg(long)]
    memory: Option<u32>,
    #[arg(long)]
    min_instances: Option<u32>,
    #[arg(long)]
    max_instances: Option<u32>,
    #[arg(long)]
    bin: Option<String>,
    /// Return as soon as the build is accepted instead of following it to
    /// completion.
    #[arg(long)]
    no_wait: bool,
}

fn parse_env_pair(s: &str) -> Result<(String, String), String> {
    match s.split_once('=') {
        Some((k, v)) => Ok((k.to_string(), v.to_string())),
        None => Err(format!("expected K=V, got {s:?}")),
    }
}

pub fn run(client: &AdminClient, args: DeployArgs) -> ExitCode {
    let rt = match tokio::runtime::Runtime::new() {
        Ok(rt) => rt,
        Err(err) => {
            eprintln!("error: failed to start async runtime: {err}");
            return ExitCode::from(1);
        }
    };
    rt.block_on(run_async(client, args))
}

async fn run_async(client: &AdminClient, args: DeployArgs) -> ExitCode {
    let source = match build_source_json(&args) {
        Ok(source) => source,
        Err(code) => return code,
    };

    let mut fields = serde_json::Map::new();
    fields.insert("trigger".into(), build_trigger_json(&args));
    fields.insert("source".into(), source);
    fields.insert("entry_point".into(), json!(args.entry_point));
    fields.insert(
        "env".into(),
        json!(
            args.set_env
                .into_iter()
                .collect::<std::collections::BTreeMap<_, _>>()
        ),
    );
    if let Some(v) = args.timeout {
        fields.insert("timeout_secs".into(), json!(v));
    }
    if let Some(v) = args.concurrency {
        fields.insert("concurrency".into(), json!(v));
    }
    if let Some(v) = args.memory {
        fields.insert("memory_mib".into(), json!(v));
    }
    if let Some(v) = args.min_instances {
        fields.insert("min_instances".into(), json!(v));
    }
    if let Some(v) = args.max_instances {
        fields.insert("max_instances".into(), json!(v));
    }
    let body = Value::Object(fields);

    let accepted = match client.deploy(&args.name, body).await {
        Ok(resp) => resp,
        Err(err) => {
            eprintln!("error: {err}");
            return ExitCode::from(err.suggested_exit_code());
        }
    };

    let build_id = match accepted.get("build_id").and_then(Value::as_str) {
        Some(id) => id.to_string(),
        None => {
            eprintln!("error: admin API response missing build_id: {accepted}");
            return ExitCode::from(1);
        }
    };
    let revision = accepted.get("revision").and_then(Value::as_u64);

    // Image-mode registrations have no build step at all (the digest is
    // resolved synchronously during `register`, per T075) -- the admin API
    // signals this with an empty `build_id` rather than a real one, since
    // there's no `Build` record to poll. `--source` deploys always get a
    // real (non-empty) UUID here.
    if build_id.is_empty() {
        let rev = revision
            .map(|r| r.to_string())
            .unwrap_or_else(|| "?".to_string());
        println!("Deployed {:?} (revision {rev})", args.name);
        return ExitCode::from(0);
    }

    if args.no_wait {
        println!("Deploy accepted for {:?} (build {build_id})", args.name);
        return ExitCode::from(0);
    }

    println!("Building {:?} (build {build_id})...", args.name);
    follow_build(client, &args.name, &build_id, revision).await
}

fn build_trigger_json(args: &DeployArgs) -> Value {
    match &args.trigger_topic {
        Some(topic) => json!({"type": "pubsub", "topic": topic}),
        None => json!({"type": "http"}),
    }
}

fn build_source_json(args: &DeployArgs) -> Result<Value, ExitCode> {
    match (&args.source, &args.image) {
        (Some(_), Some(_)) => {
            eprintln!("error: --source and --image are mutually exclusive");
            Err(ExitCode::from(2))
        }
        (None, None) => {
            eprintln!("error: one of --source or --image is required");
            Err(ExitCode::from(2))
        }
        (Some(path), None) => {
            let abs = match std::fs::canonicalize(path) {
                Ok(abs) => abs,
                Err(err) => {
                    eprintln!("error: --source {path:?}: {err}");
                    return Err(ExitCode::from(2));
                }
            };
            let mut source = json!({"kind": "dir", "path": abs.to_string_lossy()});
            if let Some(bin) = &args.bin {
                source["bin"] = json!(bin);
            }
            Ok(source)
        }
        (None, Some(image_ref)) => Ok(json!({"kind": "image", "ref": image_ref})),
    }
}

async fn follow_build(
    client: &AdminClient,
    name: &str,
    build_id: &str,
    revision: Option<u64>,
) -> ExitCode {
    let mut last_len = 0usize;
    loop {
        if let Ok(log) = client.get_build_log(name, build_id).await
            && log.len() > last_len
        {
            print!("{}", &log[last_len..]);
            use std::io::Write;
            let _ = std::io::stdout().flush();
            last_len = log.len();
        }

        let build = match client.get_build(name, build_id).await {
            Ok(build) => build,
            Err(err) => {
                eprintln!("error polling build status: {err}");
                return ExitCode::from(err.suggested_exit_code());
            }
        };
        match build.get("status").and_then(Value::as_str) {
            Some("succeeded") => {
                let rev = revision
                    .map(|r| r.to_string())
                    .unwrap_or_else(|| "?".to_string());
                println!("Deployed {name:?} (revision {rev})");
                return ExitCode::from(0);
            }
            Some("failed") => {
                eprintln!("error: build failed for {name:?}; see log above");
                return ExitCode::from(1);
            }
            _ => {
                tokio::time::sleep(Duration::from_millis(300)).await;
            }
        }
    }
}
