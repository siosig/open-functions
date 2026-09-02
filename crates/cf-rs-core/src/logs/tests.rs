use super::pipe::{LogRecord, Stream, parse_line, pump};

#[test]
fn well_formed_json_extracts_all_fields_and_preserves_extra() {
    let line = r#"{"severity":"WARNING","message":"hello","time":"2020-01-01T00:00:00Z","logging.googleapis.com/labels":{"execution_id":"abc123"},"logging.googleapis.com/trace":"trace-1","custom":42}"#;
    let record = parse_line(line, Stream::Stdout, None);

    assert_eq!(record.severity, "WARNING");
    assert_eq!(record.message, "hello");
    assert_eq!(record.execution_id.as_deref(), Some("abc123"));
    assert!(!record.truncated);
    // "message", "severity", "time", and "logging.googleapis.com/labels" are
    // extracted; everything else survives in `extra`.
    assert_eq!(record.extra.len(), 2);
    assert_eq!(
        record
            .extra
            .get("logging.googleapis.com/trace")
            .and_then(|v| v.as_str()),
        Some("trace-1")
    );
    assert_eq!(
        record.extra.get("custom").and_then(|v| v.as_i64()),
        Some(42)
    );
    assert!(!record.extra.contains_key("message"));
    assert!(!record.extra.contains_key("severity"));
    assert!(!record.extra.contains_key("time"));
    assert!(!record.extra.contains_key("logging.googleapis.com/labels"));
}

#[test]
fn json_missing_severity_defaults_by_stream_stdout() {
    let line = r#"{"message":"hi"}"#;
    let record = parse_line(line, Stream::Stdout, None);
    assert_eq!(record.severity, "INFO");
    assert_eq!(record.message, "hi");
}

#[test]
fn json_missing_severity_defaults_by_stream_stderr() {
    let line = r#"{"message":"boom"}"#;
    let record = parse_line(line, Stream::Stderr, None);
    assert_eq!(record.severity, "ERROR");
    assert_eq!(record.message, "boom");
}

#[test]
fn json_missing_labels_falls_back_to_provided_fallback() {
    let line = r#"{"message":"hi"}"#;
    let record = parse_line(line, Stream::Stdout, Some("fallback-exec-id"));
    assert_eq!(record.execution_id.as_deref(), Some("fallback-exec-id"));
}

#[test]
fn json_missing_labels_falls_back_to_none_when_no_fallback_given() {
    let line = r#"{"message":"hi"}"#;
    let record = parse_line(line, Stream::Stdout, None);
    assert_eq!(record.execution_id, None);
}

#[test]
fn non_json_line_is_wrapped_with_raw_text_as_message() {
    let line = "plain text log line";
    let record_out = parse_line(line, Stream::Stdout, None);
    assert_eq!(record_out.message, "plain text log line");
    assert_eq!(record_out.severity, "INFO");
    assert!(record_out.extra.is_empty());

    let record_err = parse_line(line, Stream::Stderr, None);
    assert_eq!(record_err.message, "plain text log line");
    assert_eq!(record_err.severity, "ERROR");
}

#[test]
fn json_bare_array_is_treated_as_non_json() {
    let line = r#"[1,2,3]"#;
    let record = parse_line(line, Stream::Stdout, None);
    assert_eq!(record.message, line);
    assert_eq!(record.severity, "INFO");
    assert!(record.extra.is_empty());
}

#[test]
fn json_bare_string_is_treated_as_non_json() {
    let line = r#""just a string""#;
    let record = parse_line(line, Stream::Stderr, None);
    assert_eq!(record.message, line);
    assert_eq!(record.severity, "ERROR");
    assert!(record.extra.is_empty());
}

#[test]
fn oversized_line_is_truncated_safely_at_multibyte_utf8_boundary() {
    // '€' (U+20AC) is 3 bytes in UTF-8. Repeating it 21846 times yields
    // 65538 bytes total, which straddles the 65536-byte truncation limit
    // mid-character (65536 is not a char boundary: 3 * 21845 = 65535 is
    // the nearest valid boundary at or before the limit).
    let euro_count = 21846;
    let raw: String = "€".repeat(euro_count);
    assert!(raw.len() > 65536);
    assert!(!raw.is_char_boundary(65536));

    let record = parse_line(&raw, Stream::Stdout, None);
    assert!(record.truncated);
    // The message must be valid UTF-8 (guaranteed by the type system, but we
    // also assert it didn't grow past the limit and lands on a boundary).
    assert!(record.message.len() <= 65536);
    assert!(raw.is_char_boundary(record.message.len()));
}

#[test]
fn line_within_limit_is_not_truncated() {
    let raw = "short line";
    let record = parse_line(raw, Stream::Stdout, None);
    assert!(!record.truncated);
    assert_eq!(record.message, raw);
}

#[tokio::test]
async fn pump_reads_all_lines_in_order_with_correct_parsing() {
    let input = concat!(
        "plain line one\n",
        r#"{"severity":"DEBUG","message":"structured line"}"#,
        "\n",
        "plain line two\n",
    );
    let reader = input.as_bytes();

    let mut records: Vec<LogRecord> = Vec::new();
    let result = pump(
        reader,
        Stream::Stdout,
        || None,
        |record| records.push(record),
    )
    .await;

    assert!(result.is_ok());
    assert_eq!(records.len(), 3);

    assert_eq!(records[0].message, "plain line one");
    assert_eq!(records[0].severity, "INFO");

    assert_eq!(records[1].message, "structured line");
    assert_eq!(records[1].severity, "DEBUG");

    assert_eq!(records[2].message, "plain line two");
    assert_eq!(records[2].severity, "INFO");
}

#[tokio::test]
async fn pump_uses_fallback_execution_id_provider_per_line() {
    let input = "line one\nline two\n";
    let reader = input.as_bytes();

    let calls = std::sync::atomic::AtomicUsize::new(0);
    let mut records: Vec<LogRecord> = Vec::new();
    let result = pump(
        reader,
        Stream::Stderr,
        || {
            let n = calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Some(format!("exec-{n}"))
        },
        |record| records.push(record),
    )
    .await;

    assert!(result.is_ok());
    assert_eq!(records.len(), 2);
    assert_eq!(records[0].execution_id.as_deref(), Some("exec-0"));
    assert_eq!(records[1].execution_id.as_deref(), Some("exec-1"));
}
