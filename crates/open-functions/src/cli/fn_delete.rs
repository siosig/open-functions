//! `open-functions fn delete` (T083): `DELETE /v1/functions/{name}`, optionally
//! polling until the function is fully gone (`--wait`).

use std::process::ExitCode;
use std::time::Duration;

use super::client::{AdminClient, ClientError};

#[derive(clap::Args)]
pub struct DeleteArgs {
    name: String,
    /// Poll until the function is fully removed (instances stopped, binding
    /// unbound, artifacts deleted) before exiting, instead of returning as
    /// soon as deletion is accepted.
    #[arg(long)]
    wait: bool,
}

pub fn run(client: &AdminClient, args: DeleteArgs) -> ExitCode {
    let rt = match tokio::runtime::Runtime::new() {
        Ok(rt) => rt,
        Err(err) => {
            eprintln!("error: failed to start async runtime: {err}");
            return ExitCode::from(1);
        }
    };
    rt.block_on(async {
        if let Err(err) = client.delete(&args.name).await {
            eprintln!("error: {err}");
            return ExitCode::from(err.suggested_exit_code());
        }
        println!("Deleting {:?}", args.name);

        if args.wait {
            loop {
                match client.describe(&args.name).await {
                    Err(ClientError::ApiError { status: 404, .. }) => break,
                    Err(err) => {
                        eprintln!("error polling delete status: {err}");
                        return ExitCode::from(err.suggested_exit_code());
                    }
                    Ok(_) => tokio::time::sleep(Duration::from_millis(300)).await,
                }
            }
        }

        println!("Deleted {:?}", args.name);
        ExitCode::from(0)
    })
}
