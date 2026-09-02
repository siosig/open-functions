//! Integration tests for `HostCargoBuilder` using the real `examples/hello-http`
//! fixture package. These invoke a genuine `cargo build --release`, so they are
//! slow (tens of seconds) but exercise the exact path a deployed function takes.
//!
//! Note: this crate configures `clippy::unwrap_used`/`clippy::expect_used` as
//! warnings (promoted to errors under `-D warnings`), and that lint config
//! applies to every target in the package, including this integration test
//! binary. `ok`/`err` below stand in for `.unwrap()`/`.expect()` without
//! tripping those lints.

use std::path::{Path, PathBuf};
use std::time::Duration;

use open_functions_core::build::host_cargo::HostCargoBuilder;
use open_functions_core::build::{BuildError, BuildRequest, Builder};

fn ok<T, E: std::fmt::Debug>(result: Result<T, E>, context: &str) -> T {
    result.unwrap_or_else(|e| panic!("{context}: {e:?}"))
}

fn err<T: std::fmt::Debug, E>(result: Result<T, E>, context: &str) -> E {
    match result {
        Ok(v) => panic!("{context}: expected Err, got Ok({v:?})"),
        Err(e) => e,
    }
}

/// `CARGO_MANIFEST_DIR` at test-run time is `<workspace_root>/crates/open-functions-core`;
/// go up two levels to reach the workspace root, then into `examples/hello-http`.
fn hello_http_source_dir() -> PathBuf {
    examples_dir().join("hello-http")
}

/// The workspace's `examples/` directory. `hello-http`'s `Cargo.toml` depends
/// on `open-functions-sdk` via the relative path `../../crates/open-functions-sdk`; any copy of
/// it used as a build fixture must keep that same two-levels-up depth, so
/// callers that copy the source tree should place the copy directly under
/// this directory (see `build_with_compile_error_fails_and_logs_the_error`).
fn examples_dir() -> PathBuf {
    let manifest_dir = ok(
        std::env::var("CARGO_MANIFEST_DIR"),
        "CARGO_MANIFEST_DIR must be set when run via cargo test",
    );
    Path::new(&manifest_dir)
        .join("../../examples")
        .canonicalize()
        .unwrap_or_else(|e| panic!("failed to canonicalize examples dir: {e:?}"))
}

/// A persistent (not per-test-run) `cargo build --release` target directory,
/// shared by every test in this file. `HostCargoBuilder` uses a real `cargo
/// build`, and in production the target dir is likewise reused across
/// revisions (see `RegistryService`'s `build_dir.join(&req.name).join("target")`)
/// so only the very first build of a function pays the full cold-compile
/// cost of `open-functions-sdk`'s dependency tree; every later build is incremental.
/// Using a fresh `tempfile::tempdir()` per test here (as this file used to)
/// defeats that caching and forces a full cold compile on every run, which
/// under CI/parallel-test contention can exceed even a generous timeout —
/// this mirrors the production dir-reuse strategy instead of fighting it.
///
/// Caveat for any test added here: cargo's fingerprint for the *root*
/// package of a build doesn't key on that root package's own source path,
/// so two different source directories that both build a same-named crate
/// into this shared dir can produce a false cache hit. Give any
/// deliberately-different fixture package a distinct name in its copied
/// `Cargo.toml` before building it (see
/// `build_with_compile_error_fails_and_logs_the_error` below).
fn shared_cargo_target_dir() -> PathBuf {
    let dir = examples_dir()
        .parent()
        .unwrap_or_else(|| panic!("examples dir has no parent"))
        .join("target/build-host-test-cache");
    ok(std::fs::create_dir_all(&dir), "create shared target dir");
    dir
}

/// Recursively copies `src` into `dst`, skipping build artifact directories
/// (`target`) so the copy is fast and the original fixture is left untouched.
fn copy_source_tree(src: &Path, dst: &Path) {
    ok(std::fs::create_dir_all(dst), "create dst dir");
    let entries = ok(std::fs::read_dir(src), "read_dir src");
    for entry in entries {
        let entry = ok(entry, "read_dir entry");
        let file_name = entry.file_name();
        if file_name == "target" {
            continue;
        }
        let src_path = entry.path();
        let dst_path = dst.join(&file_name);
        let file_type = ok(entry.file_type(), "entry file_type");
        if file_type.is_dir() {
            copy_source_tree(&src_path, &dst_path);
        } else {
            ok(std::fs::copy(&src_path, &dst_path), "copy file");
        }
    }
}

#[cfg(unix)]
fn assert_executable(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let metadata = ok(std::fs::metadata(path), "artifact metadata");
    let mode = metadata.permissions().mode();
    assert!(
        mode & 0o111 != 0,
        "artifact at {path:?} is not executable (mode {mode:o})"
    );
}

#[tokio::test]
async fn successful_build_produces_executable_artifact_and_log() {
    let source_dir = hello_http_source_dir();
    let tmp = ok(tempfile::tempdir(), "tempdir");

    let artifact_path = tmp.path().join("artifacts/hello/1/function");
    let log_path = tmp.path().join("artifacts/hello/1/build.log");
    let cargo_target_dir = shared_cargo_target_dir();

    let request = BuildRequest {
        function_name: "hello".to_string(),
        revision: 1,
        source_dir,
        bin: None,
        artifact_path: artifact_path.clone(),
        log_path: log_path.clone(),
        cargo_target_dir,
        cache_dir: tmp.path().join("cache/cargo"),
        timeout: Duration::from_secs(300),
    };

    let builder = HostCargoBuilder {
        cargo_bin: "cargo".to_string(),
    };

    let result = builder.build(&request).await;
    ok(result, "expected the hello-http build to succeed");

    assert!(
        artifact_path.exists(),
        "artifact should exist at {artifact_path:?}"
    );
    assert_executable(&artifact_path);

    let log_contents = ok(std::fs::read_to_string(&log_path), "read build log");
    assert!(!log_contents.is_empty(), "build log should not be empty");
    assert!(
        log_contents.contains("Compiling") || log_contents.contains("Finished"),
        "build log should contain cargo's typical output, got:\n{log_contents}"
    );
}

#[tokio::test]
async fn build_with_compile_error_fails_and_logs_the_error() {
    let source_dir = hello_http_source_dir();
    let tmp = ok(tempfile::tempdir(), "tempdir");

    // The copy must live directly under `examples/` (a sibling of the real
    // `hello-http`, at the same directory depth), not under an arbitrary
    // system tempdir: its Cargo.toml depends on `open-functions-sdk` via the relative
    // path `../../crates/open-functions-sdk`, which only resolves at that depth.
    let copy_holder = ok(
        tempfile::Builder::new()
            .prefix("hello-http-broken-")
            .tempdir_in(examples_dir()),
        "tempdir_in examples/",
    );
    let copy_dir = copy_holder.path().to_path_buf();
    copy_source_tree(&source_dir, &copy_dir);

    // This copy shares `shared_cargo_target_dir()` with the other test in
    // this file, but it is a *different* source directory for a package
    // that would otherwise carry the exact same name+version as the pristine
    // `hello-http` built there. Cargo's fingerprint for the root package of
    // a `cargo build` invocation doesn't key on that root package's own
    // source path (only its dependencies' paths matter to the hash) — so
    // two different directories building same-named "hello-http" v0.1.0
    // into one target dir causes cargo to treat this build as a cache hit
    // against the other one's successful artifact, silently skipping the
    // compile instead of failing. Renaming the package here keeps the
    // deliberately-broken build's cargo identity from ever colliding with
    // the pristine one's, while still sharing (and benefiting from) the
    // cached compilation of their common dependencies.
    let cargo_toml = copy_dir.join("Cargo.toml");
    let manifest = ok(std::fs::read_to_string(&cargo_toml), "read Cargo.toml");
    let renamed_manifest = manifest.replacen(
        "name = \"hello-http\"",
        "name = \"hello-http-broken-fixture\"",
        1,
    );
    ok(
        std::fs::write(&cargo_toml, renamed_manifest),
        "rename package in Cargo.toml",
    );

    // Inject a syntax error into the copy so the build fails deterministically.
    let main_rs = copy_dir.join("src/main.rs");
    let mut contents = ok(std::fs::read_to_string(&main_rs), "read main.rs");
    contents.push_str("\nthis is not valid rust\n");
    ok(std::fs::write(&main_rs, contents), "write broken main.rs");

    let artifact_path = tmp.path().join("artifacts/hello/2/function");
    let log_path = tmp.path().join("artifacts/hello/2/build.log");
    let cargo_target_dir = shared_cargo_target_dir();

    let request = BuildRequest {
        function_name: "hello".to_string(),
        revision: 2,
        source_dir: copy_dir,
        bin: None,
        artifact_path,
        log_path: log_path.clone(),
        cargo_target_dir,
        cache_dir: tmp.path().join("cache/cargo"),
        timeout: Duration::from_secs(300),
    };

    let builder = HostCargoBuilder {
        cargo_bin: "cargo".to_string(),
    };

    let result = builder.build(&request).await;
    let error = err(result, "expected the broken build to fail");
    match error {
        BuildError::NonZeroExit(code, log) => {
            assert_ne!(code, 0);
            assert_eq!(log, log_path);
        }
        other => panic!("expected BuildError::NonZeroExit, got {other:?}"),
    }

    let log_contents = ok(std::fs::read_to_string(&log_path), "read build log");
    assert!(
        log_contents.contains("error"),
        "build log should contain a compiler error, got:\n{log_contents}"
    );
}
