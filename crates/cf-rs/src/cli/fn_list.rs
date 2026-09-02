//! `cf-rs fn list` (T083): fetches and prints `GET /v1/functions`.

use std::process::ExitCode;

use super::client::AdminClient;
use super::output::{OutputFormat, print_json, resolve_output};

#[derive(clap::Args)]
pub struct ListArgs {
    #[arg(long, value_enum)]
    output: Option<OutputFormat>,
}

pub fn run(client: &AdminClient, args: ListArgs) -> ExitCode {
    let rt = match tokio::runtime::Runtime::new() {
        Ok(rt) => rt,
        Err(err) => {
            eprintln!("error: failed to start async runtime: {err}");
            return ExitCode::from(1);
        }
    };
    rt.block_on(async {
        match client.list().await {
            Ok(body) => {
                print_list(&body, resolve_output(args.output));
                ExitCode::from(0)
            }
            Err(err) => {
                eprintln!("error: {err}");
                ExitCode::from(err.suggested_exit_code())
            }
        }
    })
}

fn print_list(body: &serde_json::Value, format: OutputFormat) {
    if format == OutputFormat::Json {
        print_json(body);
        return;
    }

    let empty = Vec::new();
    let functions = body
        .get("functions")
        .and_then(|v| v.as_array())
        .unwrap_or(&empty);

    println!(
        "{:<30} {:<8} {:<6} {:<9} REVISION",
        "NAME", "TRIGGER", "SOURCE", "STATE"
    );
    for f in functions {
        let name = f.get("name").and_then(|v| v.as_str()).unwrap_or("-");
        let trigger = f
            .get("trigger")
            .and_then(|t| t.get("type"))
            .and_then(|v| v.as_str())
            .unwrap_or("-");
        let source = f.get("source_kind").and_then(|v| v.as_str()).unwrap_or("-");
        let state = f.get("state").and_then(|v| v.as_str()).unwrap_or("-");
        let revision = f
            .get("current_revision")
            .filter(|v| !v.is_null())
            .map(|v| v.to_string())
            .unwrap_or_else(|| "-".to_string());
        println!("{name:<30} {trigger:<8} {source:<6} {state:<9} {revision}");
    }
}
