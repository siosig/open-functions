//! Integration tests for `open-functions`'s configuration loader (`src/config.rs`).
//!
//! `open-functions` is a binary-only crate (no `lib` target), so this test includes the
//! source module directly via `#[path]` rather than importing it as `open_functions::config`.
//!
//! Panicking via `unwrap`/`expect` on setup failures is the desired behavior in
//! tests (it fails the test with a clear message), so the crate-wide
//! `unwrap_used`/`expect_used` lints are relaxed here.
#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "../src/config.rs"]
mod config;

use config::ConfigError;
use std::io::Write;
use std::sync::Mutex;

/// `cargo test` runs tests concurrently on multiple threads within one process,
/// but environment variables are process-global state. Every test in this file
/// calls `config::load`, whose behavior depends on `OPEN_FUNCTIONS_CONFIG` and any
/// `OPEN_FUNCTIONS__*` variable — including tests that only assert on *defaults*, which
/// would observe another thread's `OPEN_FUNCTIONS__ADMIN__LISTEN` override mid-flight
/// without serialization. Every test acquires this lock first, so at most one
/// `config::load` call (and its surrounding env setup/teardown) runs at a time.
static ENV_MUTEX: Mutex<()> = Mutex::new(());

/// Ensures `OPEN_FUNCTIONS_CONFIG` does not leak into a test run. Callers must hold
/// `ENV_MUTEX` before calling this (and before touching any `OPEN_FUNCTIONS__*` var).
fn clear_process_env() {
    // SAFETY: caller holds ENV_MUTEX, so no other test's set/remove of any
    // OPEN_FUNCTIONS*-prefixed variable can run concurrently with this one.
    unsafe {
        std::env::remove_var("OPEN_FUNCTIONS_CONFIG");
    }
}

/// Recovers the mutex guard on a previous test panic (poisoning) rather than
/// panicking here too, which would otherwise cascade into an unrelated,
/// confusing failure for every test that runs after the first one that panics.
fn lock_env() -> std::sync::MutexGuard<'static, ()> {
    ENV_MUTEX
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[test]
fn defaults_only_load_produces_documented_defaults() {
    let _guard = lock_env();
    clear_process_env();

    let cfg = config::load(None).expect("defaults-only load should succeed");

    assert_eq!(cfg.invoke.listen, "0.0.0.0:8080");
    assert_eq!(cfg.invoke.shutdown_grace_secs, 30);
    assert_eq!(cfg.admin.listen, "127.0.0.1:8081");
    assert_eq!(cfg.admin.token, "");
    assert_eq!(cfg.storage.data_dir, "/var/lib/open-functions");
    assert_eq!(cfg.build.mode, "auto");
    assert_eq!(cfg.defaults.timeout_secs, 60);
    assert_eq!(cfg.defaults.concurrency, 1);
    assert_eq!(cfg.defaults.memory_mib, 256);
    assert!(cfg.pubsub.enabled);
    assert_eq!(cfg.log.format, "json");
    assert!(cfg.metrics.enabled);
}

#[test]
fn env_var_override_wins_over_default() {
    let _guard = lock_env();
    clear_process_env();

    // SAFETY: set for the duration of this test only, removed before returning.
    unsafe {
        std::env::set_var("OPEN_FUNCTIONS__ADMIN__LISTEN", "127.0.0.1:9999");
    }

    let result = config::load(None);

    // SAFETY: cleanup, always run regardless of assertion outcome below.
    unsafe {
        std::env::remove_var("OPEN_FUNCTIONS__ADMIN__LISTEN");
    }

    let cfg = result.expect("env-overridden load should succeed");
    assert_eq!(cfg.admin.listen, "127.0.0.1:9999");
}

#[test]
fn unknown_key_in_config_file_fails_load() {
    let _guard = lock_env();
    clear_process_env();

    let mut file = tempfile::NamedTempFile::with_suffix(".toml").expect("create temp file");
    writeln!(
        file,
        r#"
[invoke]
listen = "0.0.0.0:8080"
totally_unknown_field = "boom"
"#
    )
    .expect("write temp config");

    let result = config::load(Some(file.path()));

    assert!(result.is_err(), "unknown key should fail to load");
    match result {
        Err(ConfigError::Load(_)) => {}
        other => panic!("expected ConfigError::Load, got {other:?}"),
    }
}

#[test]
fn validate_rejects_non_loopback_admin_listen_without_token() {
    let _guard = lock_env();
    clear_process_env();

    let mut file = tempfile::NamedTempFile::with_suffix(".toml").expect("create temp file");
    writeln!(
        file,
        r#"
[admin]
listen = "0.0.0.0:8081"
"#
    )
    .expect("write temp config");

    let cfg = config::load(Some(file.path())).expect("load should succeed (validate is separate)");
    let result = config::validate(&cfg);

    match result {
        Err(ConfigError::Invalid { field, .. }) => assert_eq!(field, "admin.token"),
        other => panic!("expected ConfigError::Invalid{{field: \"admin.token\"}}, got {other:?}"),
    }
}

#[test]
fn validate_accepts_non_loopback_admin_listen_with_token() {
    let _guard = lock_env();
    clear_process_env();

    let mut file = tempfile::NamedTempFile::with_suffix(".toml").expect("create temp file");
    writeln!(
        file,
        r#"
[admin]
listen = "0.0.0.0:8081"
token = "secret"
"#
    )
    .expect("write temp config");

    let cfg = config::load(Some(file.path())).expect("load should succeed");
    let result = config::validate(&cfg);

    assert!(result.is_ok(), "expected Ok(()), got {result:?}");
}

#[test]
fn validate_rejects_invalid_build_mode() {
    let _guard = lock_env();
    clear_process_env();

    let mut file = tempfile::NamedTempFile::with_suffix(".toml").expect("create temp file");
    writeln!(
        file,
        r#"
[build]
mode = "bogus"
"#
    )
    .expect("write temp config");

    let cfg = config::load(Some(file.path())).expect("load should succeed (validate is separate)");
    let result = config::validate(&cfg);

    match result {
        Err(ConfigError::Invalid { field, .. }) => assert_eq!(field, "build.mode"),
        other => panic!("expected ConfigError::Invalid{{field: \"build.mode\"}}, got {other:?}"),
    }
}
