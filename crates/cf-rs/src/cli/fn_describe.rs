//! `cf-rs fn describe` (T043): fetches and prints `GET /v1/functions/{name}`.

use std::process::ExitCode;

use super::client::AdminClient;

#[derive(clap::Args)]
pub struct DescribeArgs {
    name: String,
}

pub fn run(client: &AdminClient, args: DescribeArgs) -> ExitCode {
    let rt = match tokio::runtime::Runtime::new() {
        Ok(rt) => rt,
        Err(err) => {
            eprintln!("error: failed to start async runtime: {err}");
            return ExitCode::from(1);
        }
    };
    rt.block_on(async {
        match client.describe(&args.name).await {
            Ok(detail) => {
                match serde_json::to_string_pretty(&detail) {
                    Ok(pretty) => println!("{pretty}"),
                    Err(_) => println!("{detail}"),
                }
                ExitCode::from(0)
            }
            Err(err) => {
                eprintln!("error: {err}");
                ExitCode::from(err.suggested_exit_code())
            }
        }
    })
}
