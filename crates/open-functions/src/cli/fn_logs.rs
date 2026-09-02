//! `open-functions fn logs` (T083): `GET /v1/functions/{name}/logs?tail&follow`.

use std::process::ExitCode;

use futures_util::StreamExt;

use super::client::AdminClient;
use super::output::{OutputFormat, resolve_output};

#[derive(clap::Args)]
pub struct LogsArgs {
    name: String,
    #[arg(long, default_value_t = 100)]
    tail: usize,
    #[arg(long)]
    follow: bool,
    #[arg(long, value_enum)]
    output: Option<OutputFormat>,
}

pub fn run(client: &AdminClient, args: LogsArgs) -> ExitCode {
    let rt = match tokio::runtime::Runtime::new() {
        Ok(rt) => rt,
        Err(err) => {
            eprintln!("error: failed to start async runtime: {err}");
            return ExitCode::from(1);
        }
    };
    let format = resolve_output(args.output);
    rt.block_on(async {
        let resp = match client
            .function_logs(&args.name, args.tail, args.follow)
            .await
        {
            Ok(resp) => resp,
            Err(err) => {
                eprintln!("error: {err}");
                return ExitCode::from(err.suggested_exit_code());
            }
        };

        let mut stream = resp.bytes_stream();
        let mut buf = String::new();
        while let Some(chunk) = stream.next().await {
            let chunk = match chunk {
                Ok(chunk) => chunk,
                Err(err) => {
                    eprintln!("error reading log stream: {err}");
                    return ExitCode::from(1);
                }
            };
            buf.push_str(&String::from_utf8_lossy(&chunk));
            while let Some(pos) = buf.find('\n') {
                let line: String = buf.drain(..=pos).collect();
                let line = line.trim_end_matches('\n');
                if !line.is_empty() {
                    print_log_line(line, format);
                }
            }
        }
        ExitCode::from(0)
    })
}

fn print_log_line(line: &str, format: OutputFormat) {
    if format == OutputFormat::Json {
        println!("{line}");
        return;
    }
    match serde_json::from_str::<serde_json::Value>(line) {
        Ok(record) => {
            let time = record.get("time").and_then(|v| v.as_str()).unwrap_or("-");
            let severity = record
                .get("severity")
                .and_then(|v| v.as_str())
                .unwrap_or("-");
            let message = record
                .get("message")
                .and_then(|v| v.as_str())
                .unwrap_or("-");
            println!("{time} {severity:<8} {message}");
        }
        Err(_) => println!("{line}"),
    }
}
