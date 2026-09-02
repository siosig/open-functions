//! Unit tests for `build::metadata::resolve_bin_target`. These use hand-written
//! minimal fixture crates under `tempfile::tempdir()` and invoke the real
//! `cargo metadata` binary (offline, zero dependencies, so no network needed) —
//! but never actually compile anything.

use std::path::Path;

use super::metadata::{MetadataError, resolve_bin_target};

// This crate configures `clippy::unwrap_used`/`clippy::expect_used` as
// warnings (promoted to errors under `-D warnings`), applying to every
// target including this test module. `ok`/`err` stand in for
// `.unwrap()`/`.expect()`/`.expect_err()` without tripping those lints.
fn ok<T, E: std::fmt::Debug>(result: Result<T, E>, context: &str) -> T {
    result.unwrap_or_else(|e| panic!("{context}: {e:?}"))
}

fn err<T: std::fmt::Debug, E>(result: Result<T, E>, context: &str) -> E {
    match result {
        Ok(v) => panic!("{context}: expected Err, got Ok({v:?})"),
        Err(e) => e,
    }
}

fn write_file(path: &Path, contents: &str) {
    if let Some(parent) = path.parent() {
        ok(std::fs::create_dir_all(parent), "create fixture dir");
    }
    ok(std::fs::write(path, contents), "write fixture file");
}

fn write_manifest(dir: &Path, name: &str) {
    write_file(
        &dir.join("Cargo.toml"),
        &format!(
            r#"[package]
name = "{name}"
version = "0.1.0"
edition = "2021"
"#
        ),
    );
}

#[test]
fn single_bin_is_auto_detected() {
    let dir = ok(tempfile::tempdir(), "tempdir");
    write_manifest(dir.path(), "single-bin-fixture");
    write_file(&dir.path().join("src/main.rs"), "fn main() {}\n");

    let resolved = ok(
        resolve_bin_target(dir.path(), None),
        "single bin target should resolve",
    );
    assert_eq!(resolved, "single-bin-fixture");
}

#[test]
fn multiple_bins_require_explicit_choice() {
    let dir = ok(tempfile::tempdir(), "tempdir");
    write_manifest(dir.path(), "multi-bin-fixture");
    write_file(&dir.path().join("src/bin/foo.rs"), "fn main() {}\n");
    write_file(&dir.path().join("src/bin/bar.rs"), "fn main() {}\n");

    let result = resolve_bin_target(dir.path(), None);
    let error = err(result, "ambiguous bin target should be an error");
    match error {
        MetadataError::AmbiguousBinTarget { mut names, .. } => {
            names.sort();
            assert_eq!(names, vec!["bar".to_string(), "foo".to_string()]);
        }
        other => panic!("expected AmbiguousBinTarget, got {other:?}"),
    }
}

#[test]
fn multiple_bins_explicit_match_resolves() {
    let dir = ok(tempfile::tempdir(), "tempdir");
    write_manifest(dir.path(), "multi-bin-fixture");
    write_file(&dir.path().join("src/bin/foo.rs"), "fn main() {}\n");
    write_file(&dir.path().join("src/bin/bar.rs"), "fn main() {}\n");

    let resolved = ok(
        resolve_bin_target(dir.path(), Some("foo")),
        "explicit bin should resolve",
    );
    assert_eq!(resolved, "foo");
}

#[test]
fn unknown_requested_bin_errors_with_available_list() {
    let dir = ok(tempfile::tempdir(), "tempdir");
    write_manifest(dir.path(), "multi-bin-fixture");
    write_file(&dir.path().join("src/bin/foo.rs"), "fn main() {}\n");
    write_file(&dir.path().join("src/bin/bar.rs"), "fn main() {}\n");

    let result = resolve_bin_target(dir.path(), Some("baz"));
    let error = err(result, "unknown bin name should be an error");
    match error {
        MetadataError::BinNotFound {
            name,
            mut available,
            ..
        } => {
            assert_eq!(name, "baz");
            available.sort();
            assert_eq!(available, vec!["bar".to_string(), "foo".to_string()]);
        }
        other => panic!("expected BinNotFound, got {other:?}"),
    }
}

#[test]
fn no_bin_target_errors() {
    let dir = ok(tempfile::tempdir(), "tempdir");
    write_manifest(dir.path(), "lib-only-fixture");
    // No src/main.rs and no [[bin]] section: only an implicit lib target
    // (src/lib.rs) exists, so there are zero bin targets.
    write_file(&dir.path().join("src/lib.rs"), "pub fn noop() {}\n");

    let result = resolve_bin_target(dir.path(), None);
    let error = err(result, "no bin target should be an error");
    assert!(matches!(error, MetadataError::NoBinTarget { .. }));
}
