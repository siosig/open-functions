//! Integration test for `ContainerBuilder` (T070), using the real
//! `examples/hello-http` fixture package, against a real Docker daemon.
//!
//! Opt-in and skipped by default: it needs a real, reachable Docker daemon,
//! and its first run pulls `rust:1-bookworm` (a large image) and does a full
//! cold dependency compile inside the container, which can take several
//! minutes. Set `OPEN_FUNCTIONS_TEST_DOCKER` (to any value) to run it, mirroring the
//! `OPEN_FUNCTIONS_TEST_PSRS_URL` opt-in idiom used by `crates/open-functions/tests/e2e_pubsub.rs`
//! for other tests that need a real external dependency.
//!
//! Note: this crate configures `clippy::unwrap_used`/`clippy::expect_used` as
//! warnings (promoted to errors under `-D warnings`), and that lint config
//! applies to every target in the package, including this integration test
//! binary. `ok`/`err` below stand in for `.unwrap()`/`.expect()` without
//! tripping those lints (same helpers as `tests/build_host.rs`).
//!
//! Unlike `tests/build_host.rs` (which builds the real `examples/hello-http`
//! directory in place), this test builds a *self-contained copy* of it. The
//! real `examples/hello-http/Cargo.toml` depends on `open-functions-sdk` via the
//! relative path `../../crates/open-functions-sdk`, which escapes `examples/hello-http`
//! itself. `HostCargoBuilder` can follow that path because it runs `cargo`
//! directly on the host, which sees the whole filesystem; `ContainerBuilder`
//! deliberately bind-mounts only `request.source_dir` (read-only) into the
//! build container, per plan.md's design (the point being that a container
//! build is isolated to just the function's own source), so a source
//! directory with dependencies outside itself cannot be built this way. This
//! is a real, correct constraint of container-mode builds, not a bug: a
//! function registered for container-mode builds needs a self-contained
//! source directory. This test constructs exactly that: it copies
//! `examples/hello-http` plus a *standalone* (no workspace inheritance) copy
//! of `crates/open-functions-sdk` into one fixture directory, with the copied
//! `Cargo.toml`'s path dependency pointed at the vendored copy.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use open_functions_core::build::container::ContainerBuilder;
use open_functions_core::build::{BuildRequest, Builder};

fn ok<T, E: std::fmt::Debug>(result: Result<T, E>, context: &str) -> T {
    result.unwrap_or_else(|e| panic!("{context}: {e:?}"))
}

fn workspace_root() -> PathBuf {
    let manifest_dir = ok(
        std::env::var("CARGO_MANIFEST_DIR"),
        "CARGO_MANIFEST_DIR must be set when run via cargo test",
    );
    // CARGO_MANIFEST_DIR is `<workspace_root>/crates/open-functions-core`.
    Path::new(&manifest_dir)
        .join("../..")
        .canonicalize()
        .unwrap_or_else(|e| panic!("failed to canonicalize workspace root: {e:?}"))
}

/// Recursively copies `src` into `dst`, skipping `target` (build artifacts)
/// so the copy is fast and the original fixture is left untouched. Keeps
/// `Cargo.lock` (unlike `tests/build_host.rs`'s equivalent helper, which
/// doesn't need to preserve it): `request.source_dir` is bind-mounted
/// *read-only* into the build container, so `cargo build` has nowhere to
/// write a lockfile if one isn't already present -- Cargo.lock entries for
/// path dependencies (like `open-functions-sdk` here) don't embed the dependency's
/// on-disk path, only its name/version/dependency-edges, so the copied
/// lockfile stays valid even once `open-functions-sdk` is vendored to a new path
/// below.
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

/// A standalone `open-functions-sdk` manifest with every `.workspace = true`
/// inheritance resolved to the concrete value the real root `Cargo.toml`
/// (`[workspace.package]` / `[workspace.dependencies]`) currently declares.
/// Needed because the vendored copy lives inside the container fixture's own
/// (deliberately isolated) directory tree, with no ancestor `[workspace]` to
/// inherit from -- see this file's module doc comment.
const STANDALONE_OPEN_FUNCTIONS_SDK_MANIFEST: &str = r#"[package]
name = "open-functions-sdk"
version = "0.1.0"
edition = "2024"
license = "MIT OR Apache-2.0"

[dependencies]
tokio = { version = "1.53", features = ["full"] }
axum = "0.8.9"
http = "1.2"
http-body-util = "0.1"
cloudevents-sdk = { version = "0.9.0", features = ["axum", "http-binding"] }
tracing = "0.1.44"
tracing-subscriber = { version = "0.3", features = ["env-filter", "json"] }
thiserror = "2.0.20"
serde = { version = "1", features = ["derive"] }
serde_json = "1.0.151"
base64 = "0.22"
chrono = { version = "0.4", features = ["serde"] }
"#;

/// Builds the self-contained container-mode fixture described in this file's
/// module doc comment under `dest` (which must not yet exist) and returns
/// `dest` for convenience.
fn build_self_contained_fixture(dest: &Path) -> PathBuf {
    let root = workspace_root();
    copy_source_tree(&root.join("examples/hello-http"), dest);

    let vendored_sdk_dir = dest.join("vendor/open-functions-sdk");
    copy_source_tree(&root.join("crates/open-functions-sdk"), &vendored_sdk_dir);
    ok(
        std::fs::write(
            vendored_sdk_dir.join("Cargo.toml"),
            STANDALONE_OPEN_FUNCTIONS_SDK_MANIFEST,
        ),
        "write standalone open-functions-sdk Cargo.toml",
    );

    let cargo_toml_path = dest.join("Cargo.toml");
    let manifest = ok(std::fs::read_to_string(&cargo_toml_path), "read Cargo.toml");
    let rewritten = manifest.replace(
        "open-functions-sdk = { path = \"../../crates/open-functions-sdk\" }",
        "open-functions-sdk = { path = \"vendor/open-functions-sdk\" }",
    );
    assert_ne!(
        manifest, rewritten,
        "expected to find and rewrite the open-functions-sdk path dependency in {cargo_toml_path:?}"
    );
    ok(
        std::fs::write(&cargo_toml_path, rewritten),
        "rewrite hello-http Cargo.toml",
    );

    dest.to_path_buf()
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

/// Docker daemon socket to use. Empty string means "bollard's own default
/// resolution" (`DOCKER_HOST` env var, else the platform-default local
/// socket), matching `ContainerBuilder::docker_socket`'s documented contract.
fn docker_socket() -> String {
    std::env::var("OPEN_FUNCTIONS_TEST_DOCKER_SOCKET").unwrap_or_default()
}

#[tokio::test]
async fn successful_build_produces_executable_artifact_and_reuses_the_registry_cache() {
    if std::env::var("OPEN_FUNCTIONS_TEST_DOCKER").is_err() {
        eprintln!(
            "skipping: set OPEN_FUNCTIONS_TEST_DOCKER (e.g. OPEN_FUNCTIONS_TEST_DOCKER=1) to run against a real Docker daemon"
        );
        return;
    }

    let tmp = ok(tempfile::tempdir(), "tempdir");
    let source_dir = build_self_contained_fixture(&tmp.path().join("fixture"));
    let cache_dir = tmp.path().join("cache/cargo");

    let builder = ContainerBuilder {
        docker_socket: docker_socket(),
    };

    // First build: a cold `rust:1-bookworm` pull (if not already cached
    // locally) plus a full dependency compile, so give it a generous
    // timeout matching this repo's other cold-build test budgets.
    let artifact_path_1 = tmp.path().join("artifacts/hello/1/function");
    let log_path_1 = tmp.path().join("artifacts/hello/1/build.log");
    let request_1 = BuildRequest {
        function_name: "hello".to_string(),
        revision: 1,
        source_dir: source_dir.clone(),
        bin: None,
        artifact_path: artifact_path_1.clone(),
        log_path: log_path_1.clone(),
        cargo_target_dir: tmp.path().join("build/hello/target"),
        cache_dir: cache_dir.clone(),
        timeout: Duration::from_secs(300),
    };

    let started = Instant::now();
    let result = builder.build(&request_1).await;
    let first_build_elapsed = started.elapsed();
    ok(
        result,
        "expected the container build of hello-http to succeed",
    );
    eprintln!("first (cold) container build took {first_build_elapsed:?}");

    assert!(
        artifact_path_1.exists(),
        "artifact should exist at {artifact_path_1:?}"
    );
    #[cfg(unix)]
    assert_executable(&artifact_path_1);

    let log_contents = ok(std::fs::read_to_string(&log_path_1), "read build log");
    assert!(!log_contents.is_empty(), "build log should not be empty");
    assert!(
        log_contents.contains("Compiling") || log_contents.contains("Finished"),
        "build log should contain cargo's typical output, got:\n{log_contents}"
    );

    // Verify the shared registry cache dir on the HOST actually got
    // populated by the container's build: this is the real proof that the
    // bind-mount (`cache_dir` -> `/usr/local/cargo/registry` inside the
    // container) plus the `CARGO_HOME` redirection genuinely worked, since a
    // container writing to the wrong internal path would silently leave
    // this host-side directory empty even though the build itself
    // succeeded (it would just re-download every dependency from
    // crates.io, inside the container's own filesystem layer, instead).
    let cache_has_content = ok(std::fs::read_dir(&cache_dir), "read_dir cache_dir")
        .next()
        .is_some();
    assert!(
        cache_has_content,
        "expected the shared cargo registry cache dir {cache_dir:?} to contain \
         data (registry index / downloaded crates) after the first build, \
         proving the cache bind-mount was actually used"
    );

    // Second build, into a *fresh* BuildRequest that shares the same
    // cache_dir but uses a separate CARGO_TARGET_DIR (so this build cannot
    // benefit from incremental-compilation reuse of the first build's own
    // target dir -- only from the shared registry cache), reusing the same
    // Docker image (already pulled) and registry cache. It should be
    // meaningfully faster than the first cold build.
    let artifact_path_2 = tmp.path().join("artifacts/hello/2/function");
    let log_path_2 = tmp.path().join("artifacts/hello/2/build.log");
    let request_2 = BuildRequest {
        function_name: "hello".to_string(),
        revision: 2,
        source_dir,
        bin: None,
        artifact_path: artifact_path_2.clone(),
        log_path: log_path_2,
        cargo_target_dir: tmp.path().join("build/hello/target-2"),
        cache_dir,
        timeout: Duration::from_secs(300),
    };

    let started = Instant::now();
    let result = builder.build(&request_2).await;
    let second_build_elapsed = started.elapsed();
    ok(
        result,
        "expected the second container build of hello-http to succeed",
    );
    eprintln!("second (cache-warm) container build took {second_build_elapsed:?}");

    assert!(
        artifact_path_2.exists(),
        "artifact should exist at {artifact_path_2:?}"
    );

    // Informational only, not asserted: both builds use a *fresh*
    // `CARGO_TARGET_DIR`, so neither benefits from incremental compilation,
    // and the shared registry cache only saves the "Downloading" step, not
    // the (dominant) "Compiling" step -- so the wall-clock gap between the
    // two is real but can be a small fraction of total time, and asserting
    // a hard inequality here would be a flaky, machine-load-dependent check
    // on top of the `cache_has_content` assertion above, which is the
    // actual, direct proof that the cache bind-mount worked.
    if second_build_elapsed >= first_build_elapsed {
        eprintln!(
            "note: second build ({second_build_elapsed:?}) was not faster than the \
             first ({first_build_elapsed:?}); not asserted on since fresh target dirs \
             mean the registry cache only saves download time, not compile time"
        );
    }
}
