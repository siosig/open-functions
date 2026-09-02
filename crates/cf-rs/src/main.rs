mod cli;
mod config;
mod forward;
mod ops;
mod serve;
mod server;

fn main() -> std::process::ExitCode {
    cli::run()
}
