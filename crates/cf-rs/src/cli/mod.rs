//! CLI entry point: subcommand parsing and dispatch.
//!
//! `serve` / `check-config` / `health` / `version` are Foundational-phase.
//! `fn deploy` / `fn describe` are T043 (User Story 1); `fn list` / `delete` /
//! `logs` / `build-log` / `stop` land with later user-story tasks (T056,
//! T076, T083) as this enum grows.

mod client;
mod fn_deploy;
mod fn_describe;

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};

use client::AdminClient;

const EXIT_OK: u8 = 0;
const EXIT_RUNTIME_ERROR: u8 = 1;
const EXIT_CONFIG_ERROR: u8 = 2;

#[derive(Parser)]
#[command(
    name = "cf-rs",
    version,
    about = "Google Cloud Run functions compatible local function host"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Start the invoke and admin listeners and serve until terminated.
    Serve(ServeArgs),
    /// Load and validate configuration, then exit (0 = valid, 2 = invalid).
    CheckConfig(CheckConfigArgs),
    /// Query the admin listener's /readyz endpoint (for container HEALTHCHECK).
    Health(HealthArgs),
    /// Print version information.
    Version,
    /// Manage registered functions via the admin API.
    #[command(subcommand)]
    Fn(FnCommand),
}

#[derive(Subcommand)]
enum FnCommand {
    /// Register (or re-register) a function and build it.
    Deploy(fn_deploy::DeployArgs),
    /// Print a function's current detail.
    Describe(fn_describe::DescribeArgs),
}

#[derive(clap::Args)]
struct ServeArgs {
    #[arg(long)]
    config: Option<PathBuf>,
    #[arg(long)]
    invoke_listen: Option<String>,
    #[arg(long)]
    admin_listen: Option<String>,
    #[arg(long)]
    data_dir: Option<String>,
}

#[derive(clap::Args)]
struct CheckConfigArgs {
    #[arg(long)]
    config: Option<PathBuf>,
}

#[derive(clap::Args)]
struct HealthArgs {
    #[arg(long, default_value = "http://127.0.0.1:8081/readyz")]
    url: String,
    #[arg(long, default_value_t = 5)]
    timeout: u64,
}

pub fn run() -> ExitCode {
    let cli = Cli::parse();
    match cli.command {
        Command::Serve(args) => run_serve(args),
        Command::CheckConfig(args) => run_check_config(args),
        Command::Health(args) => run_health(args),
        Command::Version => {
            print_version();
            ExitCode::from(EXIT_OK)
        }
        Command::Fn(fn_command) => run_fn(fn_command),
    }
}

fn run_fn(command: FnCommand) -> ExitCode {
    // Admin connection flags are read directly from the process environment
    // here (rather than via `#[command(flatten)]` on each subcommand) to
    // keep `fn_deploy`/`fn_describe`'s arg structs focused purely on their
    // own domain flags; this mirrors how `--admin-url`/`--admin-token` are
    // documented in admin-api.md as environment-first (`CF_RS_ADMIN_URL`,
    // `CF_RS_ADMIN_TOKEN`) with `--admin-url`/`--admin-token` overrides.
    let admin_url =
        std::env::var("CF_RS_ADMIN_URL").unwrap_or_else(|_| "http://127.0.0.1:8081".to_string());
    let admin_token = std::env::var("CF_RS_ADMIN_TOKEN").ok();
    let client = AdminClient::new(admin_url, admin_token);

    match command {
        FnCommand::Deploy(args) => fn_deploy::run(&client, args),
        FnCommand::Describe(args) => fn_describe::run(&client, args),
    }
}

fn load_and_validate(
    config_path: Option<&std::path::Path>,
) -> Result<crate::config::AppConfig, ExitCode> {
    let cfg = crate::config::load(config_path).map_err(|err| {
        eprintln!("error: {err}");
        ExitCode::from(EXIT_CONFIG_ERROR)
    })?;
    crate::config::validate(&cfg).map_err(|err| {
        eprintln!("error: {err}");
        ExitCode::from(EXIT_CONFIG_ERROR)
    })?;
    Ok(cfg)
}

fn run_check_config(args: CheckConfigArgs) -> ExitCode {
    match load_and_validate(args.config.as_deref()) {
        Ok(_) => ExitCode::from(EXIT_OK),
        Err(code) => code,
    }
}

fn run_serve(args: ServeArgs) -> ExitCode {
    let mut cfg = match load_and_validate(args.config.as_deref()) {
        Ok(cfg) => cfg,
        Err(code) => return code,
    };

    if let Some(v) = args.invoke_listen {
        cfg.invoke.listen = v;
    }
    if let Some(v) = args.admin_listen {
        cfg.admin.listen = v;
    }
    if let Some(v) = args.data_dir {
        cfg.storage.data_dir = v;
    }

    // CLI overrides may have invalidated the config again (e.g. a non-loopback
    // --admin-listen without a token already set in the file/env).
    if let Err(err) = crate::config::validate(&cfg) {
        eprintln!("error: {err}");
        return ExitCode::from(EXIT_CONFIG_ERROR);
    }

    let rt = match tokio::runtime::Runtime::new() {
        Ok(rt) => rt,
        Err(err) => {
            eprintln!("error: failed to start async runtime: {err}");
            return ExitCode::from(EXIT_RUNTIME_ERROR);
        }
    };

    rt.block_on(crate::serve::run(cfg))
}

fn run_health(args: HealthArgs) -> ExitCode {
    let rt = match tokio::runtime::Runtime::new() {
        Ok(rt) => rt,
        Err(err) => {
            eprintln!("error: failed to start async runtime: {err}");
            return ExitCode::from(EXIT_RUNTIME_ERROR);
        }
    };

    rt.block_on(async {
        let client = reqwest::Client::new();
        let outcome = tokio::time::timeout(
            std::time::Duration::from_secs(args.timeout),
            client.get(&args.url).send(),
        )
        .await;

        match outcome {
            Ok(Ok(resp)) if resp.status().is_success() => ExitCode::from(EXIT_OK),
            Ok(Ok(resp)) => {
                eprintln!("error: health check returned status {}", resp.status());
                ExitCode::from(EXIT_RUNTIME_ERROR)
            }
            Ok(Err(err)) => {
                eprintln!("error: health check request failed: {err}");
                ExitCode::from(EXIT_RUNTIME_ERROR)
            }
            Err(_) => {
                eprintln!("error: health check timed out after {}s", args.timeout);
                ExitCode::from(EXIT_RUNTIME_ERROR)
            }
        }
    })
}

fn print_version() {
    // Git SHA embedding (via build.rs) is not yet wired; version is semver-only
    // until a Polish-phase task adds it.
    println!("cf-rs {}", env!("CARGO_PKG_VERSION"));
}
