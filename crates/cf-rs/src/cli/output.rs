//! Shared `--output json|table` resolution (T083/US5), per admin-api.md's
//! `cf-rs fn` section: default `table`, `json` when stdout isn't a TTY.

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum OutputFormat {
    Json,
    Table,
}

/// Resolves the effective output format: an explicit `--output` always wins;
/// otherwise `table` when stdout is a TTY, `json` when it's redirected/piped
/// (so scripted usage gets machine-readable output without needing
/// `--output json` spelled out every time).
pub fn resolve_output(explicit: Option<OutputFormat>) -> OutputFormat {
    explicit.unwrap_or_else(|| {
        if std::io::IsTerminal::is_terminal(&std::io::stdout()) {
            OutputFormat::Table
        } else {
            OutputFormat::Json
        }
    })
}

/// Prints `value` as pretty JSON, per [`OutputFormat::Json`]. Shared by every
/// `fn` subcommand's json branch so a serialization failure (should never
/// happen for a `serde_json::Value` built from a parsed API response) falls
/// back to the value's own `Display` the same way everywhere.
pub fn print_json(value: &serde_json::Value) {
    match serde_json::to_string_pretty(value) {
        Ok(pretty) => println!("{pretty}"),
        Err(_) => println!("{value}"),
    }
}
