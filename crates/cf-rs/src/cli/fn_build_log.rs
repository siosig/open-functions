//! `cf-rs fn build-log` (T083): `GET /v1/functions/{name}/builds/{id}/log[?follow]`.
//! `--build` defaults to the function's current revision's build (via
//! `GET /v1/functions/{name}`'s `current_build_id` field, T081) when omitted.

use std::io::Write;
use std::process::ExitCode;

use futures_util::StreamExt;

use super::client::AdminClient;

#[derive(clap::Args)]
pub struct BuildLogArgs {
    name: String,
    /// Build id to show; defaults to the function's current build.
    #[arg(long)]
    build: Option<String>,
    #[arg(long)]
    follow: bool,
}

pub fn run(client: &AdminClient, args: BuildLogArgs) -> ExitCode {
    let rt = match tokio::runtime::Runtime::new() {
        Ok(rt) => rt,
        Err(err) => {
            eprintln!("error: failed to start async runtime: {err}");
            return ExitCode::from(1);
        }
    };
    rt.block_on(async {
        let build_id = match resolve_build_id(client, &args).await {
            Ok(id) => id,
            Err(code) => return code,
        };

        if !args.follow {
            return match client.get_build_log(&args.name, &build_id).await {
                Ok(text) => {
                    print!("{text}");
                    ExitCode::from(0)
                }
                Err(err) => {
                    eprintln!("error: {err}");
                    ExitCode::from(err.suggested_exit_code())
                }
            };
        }

        let resp = match client.follow_build_log(&args.name, &build_id).await {
            Ok(resp) => resp,
            Err(err) => {
                eprintln!("error: {err}");
                return ExitCode::from(err.suggested_exit_code());
            }
        };
        let mut stream = resp.bytes_stream();
        while let Some(chunk) = stream.next().await {
            match chunk {
                Ok(bytes) => {
                    let _ = std::io::stdout().write_all(&bytes);
                    let _ = std::io::stdout().flush();
                }
                Err(err) => {
                    eprintln!("error reading build log stream: {err}");
                    return ExitCode::from(1);
                }
            }
        }
        ExitCode::from(0)
    })
}

async fn resolve_build_id(client: &AdminClient, args: &BuildLogArgs) -> Result<String, ExitCode> {
    if let Some(id) = &args.build {
        return Ok(id.clone());
    }
    match client.describe(&args.name).await {
        Ok(detail) => match detail.get("current_build_id").and_then(|v| v.as_str()) {
            Some(id) => Ok(id.to_string()),
            None => {
                eprintln!(
                    "error: {:?} has no build yet (image-mode functions never build; \
                     pass --build to see a specific past build)",
                    args.name
                );
                Err(ExitCode::from(2))
            }
        },
        Err(err) => {
            eprintln!("error: {err}");
            Err(ExitCode::from(err.suggested_exit_code()))
        }
    }
}
