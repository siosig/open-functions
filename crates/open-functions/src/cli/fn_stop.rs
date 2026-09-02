//! `open-functions fn stop` (T083): `POST /v1/functions/{name}:stop` -- forces every
//! running instance to stop (scale to zero) without touching the
//! function's registration.

use std::process::ExitCode;

use super::client::AdminClient;

#[derive(clap::Args)]
pub struct StopArgs {
    name: String,
}

pub fn run(client: &AdminClient, args: StopArgs) -> ExitCode {
    let rt = match tokio::runtime::Runtime::new() {
        Ok(rt) => rt,
        Err(err) => {
            eprintln!("error: failed to start async runtime: {err}");
            return ExitCode::from(1);
        }
    };
    rt.block_on(async {
        match client.stop(&args.name).await {
            Ok(_) => {
                println!("Stopped {:?}", args.name);
                ExitCode::from(0)
            }
            Err(err) => {
                eprintln!("error: {err}");
                ExitCode::from(err.suggested_exit_code())
            }
        }
    })
}
