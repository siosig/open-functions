//! stdout/stderr → GCP structured log line conversion. Implemented in US1 (T037), extended US5 (T079).
//!
//! See `specs/001-cloud-functions-local/contracts/function-contract.md`, section
//! "Execution ID and logging", for the exact wire format a well-behaved function instance
//! (via `cf-rs-sdk`) emits and the rules the host follows when normalizing it.

use std::collections::BTreeMap;

/// Maximum accepted length (in bytes) of a single log line before truncation.
const MAX_LINE_BYTES: usize = 65536;

/// A single normalized log line ready for the host to re-emit via `tracing`
/// and (later, by US5's ring buffer) retain for `GET /v1/functions/{name}/logs`.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct LogRecord {
    pub severity: String, // "DEBUG" | "INFO" | "WARNING" | "ERROR", GCP scale
    pub message: String,
    /// RFC3339 timestamp assigned by the host at receipt time (function-contract.md
    /// doesn't require trusting a client-supplied `time` field for host-side
    /// record-keeping; the host stamps its own).
    pub time: String,
    pub execution_id: Option<String>,
    /// Any other top-level fields from a parsed JSON line, preserved verbatim
    /// (excluding the ones already extracted above) so nothing is silently
    /// dropped. Empty for non-JSON lines.
    pub extra: BTreeMap<String, serde_json::Value>,
    pub truncated: bool,
}

/// Which stream a line came from — used to pick the default severity
/// (stdout→INFO, stderr→ERROR) when a line isn't parseable JSON, or is JSON
/// without a `severity` field.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stream {
    Stdout,
    Stderr,
}

impl Stream {
    fn default_severity(self) -> &'static str {
        match self {
            Stream::Stdout => "INFO",
            Stream::Stderr => "ERROR",
        }
    }
}

/// Truncates `raw` to at most `MAX_LINE_BYTES` bytes, respecting UTF-8 char
/// boundaries (never splitting a multi-byte character). Returns the
/// (possibly borrowed) truncated string and whether truncation occurred.
fn truncate_to_limit(raw: &str) -> (&str, bool) {
    if raw.len() <= MAX_LINE_BYTES {
        return (raw, false);
    }
    // Walk backward from the byte-limit to the nearest char boundary.
    let mut end = MAX_LINE_BYTES;
    while end > 0 && !raw.is_char_boundary(end) {
        end -= 1;
    }
    (&raw[..end], true)
}

/// The instance's "last known execution id" — when a JSON line lacks
/// `logging.googleapis.com/labels.execution_id`, function-contract.md says the
/// host may attribute it to "this instance's most recent execution id" but explicitly notes
/// this is NOT attempted when concurrent executions could be in flight
/// (ambiguous attribution). Model this simply: a caller-supplied
/// `Option<&str>` "current execution id, only when concurrency=1 for this
/// instance" — when `None` is passed, never guess; only use the JSON line's
/// own explicit `labels.execution_id` if it provided one.
pub fn parse_line(raw: &str, stream: Stream, fallback_execution_id: Option<&str>) -> LogRecord {
    let (truncated_raw, truncated) = truncate_to_limit(raw);
    let time = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Micros, true);

    let parsed: Option<serde_json::Map<String, serde_json::Value>> =
        serde_json::from_str(truncated_raw).ok();

    match parsed {
        Some(mut map) => {
            // message: use the "message" field if present and a string; otherwise
            // (missing or wrong type) fall back to the raw line as the message —
            // a judgment call for malformed input, preferring to surface
            // something useful over an empty message.
            let message = match map.remove("message") {
                Some(serde_json::Value::String(s)) => s,
                _ => truncated_raw.to_string(),
            };

            let severity = match map.remove("severity") {
                Some(serde_json::Value::String(s)) => s,
                _ => stream.default_severity().to_string(),
            };

            // Drop any client-supplied "time" — the host always stamps its own
            // receipt time above.
            map.remove("time");

            let execution_id = match map.remove("logging.googleapis.com/labels") {
                Some(serde_json::Value::Object(mut labels)) => {
                    match labels.remove("execution_id") {
                        Some(serde_json::Value::String(id)) => Some(id),
                        _ => fallback_execution_id.map(str::to_string),
                    }
                }
                _ => fallback_execution_id.map(str::to_string),
            };

            let extra = map.into_iter().collect();

            LogRecord {
                severity,
                message,
                time,
                execution_id,
                extra,
                truncated,
            }
        }
        None => LogRecord {
            severity: stream.default_severity().to_string(),
            message: truncated_raw.to_string(),
            time,
            execution_id: fallback_execution_id.map(str::to_string),
            extra: BTreeMap::new(),
            truncated,
        },
    }
}

/// Reads lines from `reader` (typically a child process's stdout or stderr)
/// until EOF or an I/O error, calling `on_line` for each parsed `LogRecord`.
/// Runs until the stream closes (the process exits or closes the pipe) —
/// callers spawn this per-stream, typically via `tokio::spawn`.
///
/// `fallback_execution_id_provider` is invoked once per line to obtain the
/// instance's current "last known execution id" (see `parse_line`); a real
/// caller would pass something like
/// `move || instance_state.lock().unwrap().current_execution_id.clone()`,
/// since that value can change over the instance's lifetime across multiple
/// requests.
///
/// Returns any I/O error encountered while reading the stream (EOF is not an
/// error and yields `Ok(())`).
pub async fn pump<R, P, F>(
    reader: R,
    stream: Stream,
    fallback_execution_id_provider: P,
    mut on_line: F,
) -> std::io::Result<()>
where
    R: tokio::io::AsyncRead + Unpin,
    P: Fn() -> Option<String>,
    F: FnMut(LogRecord) + Send,
{
    use tokio::io::AsyncBufReadExt;
    let mut lines = tokio::io::BufReader::new(reader).lines();
    while let Some(line) = lines.next_line().await? {
        let fallback = fallback_execution_id_provider();
        let record = parse_line(&line, stream, fallback.as_deref());
        on_line(record);
    }
    Ok(())
}
