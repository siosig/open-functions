//! Integration test (T020) for `HostPythonBuilder`: builds real venvs with
//! the host's own `python3.14`/`uv`/`pip`, against `examples/hello-python-http`
//! and small ad-hoc fixtures for the failure-classification cases.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use open_functions_core::build::python::env::passthrough_env;
use open_functions_core::build::python::host::HostPythonBuilder;
use open_functions_core::build::python::{
    Installer, PythonBuildError, PythonBuildRequest, PythonBuilder,
};

fn python314_available() -> bool {
    std::process::Command::new("python3.14")
        .args(["-c", "import sys; print(sys.version_info[:2])"])
        .output()
        .map(|o| o.status.success() && String::from_utf8_lossy(&o.stdout).trim() == "(3, 14)")
        .unwrap_or(false)
}

macro_rules! require_python314 {
    () => {
        if !python314_available() {
            eprintln!("skipping {}: python3.14 not found on PATH", module_path!());
            return;
        }
    };
}

fn hello_python_http_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../examples/hello-python-http")
        .canonicalize()
        .expect("examples/hello-python-http should exist relative to open-functions-core")
}

fn base_request(
    source_dir: PathBuf,
    artifact_dir: &Path,
    cache_root: &Path,
    installer: Installer,
) -> PythonBuildRequest {
    let host_env: BTreeMap<String, String> = std::env::vars().collect();
    PythonBuildRequest {
        function_name: "hello-py".to_string(),
        revision: 1,
        source_dir,
        artifact_dir: artifact_dir.to_path_buf(),
        entry_point: "hello".to_string(),
        timeout: Duration::from_secs(180),
        cache_root: cache_root.to_path_buf(),
        functions_framework_spec: "functions-framework==3.10.2".to_string(),
        installer,
        python_bin: None,
        uv_bin: "uv".to_string(),
        container_image: "unused".to_string(),
        passthrough_env: passthrough_env(&host_env, cache_root),
    }
}

fn builder() -> HostPythonBuilder {
    HostPythonBuilder {
        python_bin_override: String::new(),
        uv_bin: "uv".to_string(),
    }
}

fn write_source(dir: &Path, main_py: &str, requirements_txt: Option<&str>) {
    std::fs::create_dir_all(dir).expect("mkdir source dir");
    std::fs::write(dir.join("main.py"), main_py).expect("write main.py");
    if let Some(reqs) = requirements_txt {
        std::fs::write(dir.join("requirements.txt"), reqs).expect("write requirements.txt");
    }
}

#[tokio::test]
async fn host_build_produces_a_working_venv_with_auto_installer_choosing_uv() {
    require_python314!();
    let artifact_dir = tempfile::tempdir().expect("tempdir");
    let cache_root = tempfile::tempdir().expect("tempdir");
    let request = base_request(
        hello_python_http_dir(),
        artifact_dir.path(),
        cache_root.path(),
        Installer::Auto,
    );

    let outcome = builder()
        .build(&request)
        .await
        .expect("build should succeed for examples/hello-python-http");

    assert_eq!(outcome.tool, "uv", "auto should prefer uv when it's usable");
    assert!(
        artifact_dir
            .path()
            .join("venv/bin/functions-framework")
            .exists(),
        "venv/bin/functions-framework should exist after a successful build"
    );

    let requirements =
        std::fs::read_to_string(artifact_dir.path().join("requirements.open-functions.txt"))
            .expect("read requirements.open-functions.txt");
    assert!(
        requirements.contains("functions-framework==3.10.2"),
        "functions-framework should have been auto-added: {requirements:?}"
    );

    let log =
        std::fs::read_to_string(artifact_dir.path().join("build.log")).expect("read build.log");
    assert!(
        log.contains("== step: snapshot =="),
        "log missing snapshot step: {log}"
    );
    assert!(
        log.contains("== step: install (uv) =="),
        "log missing install (uv) step: {log}"
    );
    assert!(
        log.contains("== step: verify-entry-point =="),
        "log missing verify-entry-point step: {log}"
    );
}

#[tokio::test]
async fn pip_installer_forced_produces_a_working_venv_with_tool_pip() {
    require_python314!();
    let artifact_dir = tempfile::tempdir().expect("tempdir");
    let cache_root = tempfile::tempdir().expect("tempdir");
    let request = base_request(
        hello_python_http_dir(),
        artifact_dir.path(),
        cache_root.path(),
        Installer::Pip,
    );

    let outcome = builder()
        .build(&request)
        .await
        .expect("build should succeed with installer=pip forced");

    assert_eq!(outcome.tool, "pip");
    assert!(
        artifact_dir
            .path()
            .join("venv/bin/functions-framework")
            .exists(),
        "venv/bin/functions-framework should exist after a pip-forced build"
    );
}

#[tokio::test]
async fn nonexistent_package_fails_as_install_error_with_uv_error_in_log() {
    require_python314!();
    let source_dir = tempfile::tempdir().expect("tempdir");
    write_source(
        source_dir.path(),
        "def hello(request):\n    return 'hi'\n",
        Some("this-package-definitely-does-not-exist-open-functions-9999==0.0.0\n"),
    );
    let artifact_dir = tempfile::tempdir().expect("tempdir");
    let cache_root = tempfile::tempdir().expect("tempdir");
    let request = base_request(
        source_dir.path().to_path_buf(),
        artifact_dir.path(),
        cache_root.path(),
        Installer::Auto,
    );

    let err = builder()
        .build(&request)
        .await
        .expect_err("build should fail: the pinned package doesn't exist");
    assert!(
        matches!(err, PythonBuildError::Install(_)),
        "expected PythonBuildError::Install, got {err:?}"
    );

    let log =
        std::fs::read_to_string(artifact_dir.path().join("build.log")).expect("read build.log");
    assert!(
        log.contains("install (uv)"),
        "log missing install (uv) step: {log}"
    );
    assert!(
        log.to_lowercase()
            .contains("this-package-definitely-does-not-exist-open-functions-9999"),
        "log should contain uv's error naming the missing package: {log}"
    );
}

#[tokio::test]
async fn syntax_error_in_main_fails_as_entry_point_error_with_traceback_in_log() {
    require_python314!();
    let source_dir = tempfile::tempdir().expect("tempdir");
    write_source(source_dir.path(), "def hello(:\n    pass\n", None);
    let artifact_dir = tempfile::tempdir().expect("tempdir");
    let cache_root = tempfile::tempdir().expect("tempdir");
    let request = base_request(
        source_dir.path().to_path_buf(),
        artifact_dir.path(),
        cache_root.path(),
        Installer::Auto,
    );

    let err = builder()
        .build(&request)
        .await
        .expect_err("build should fail: main.py has a syntax error");
    assert!(
        matches!(err, PythonBuildError::EntryPoint(_)),
        "expected PythonBuildError::EntryPoint, got {err:?}"
    );

    let log =
        std::fs::read_to_string(artifact_dir.path().join("build.log")).expect("read build.log");
    assert!(
        log.contains("Traceback") && log.contains("SyntaxError"),
        "log should contain the Python traceback for the syntax error: {log}"
    );
}

#[tokio::test]
async fn missing_entry_point_fails_as_entry_point_error_with_attributeerror_in_log() {
    require_python314!();
    let source_dir = tempfile::tempdir().expect("tempdir");
    write_source(
        source_dir.path(),
        "def not_hello(request):\n    return 'hi'\n",
        None,
    );
    let artifact_dir = tempfile::tempdir().expect("tempdir");
    let cache_root = tempfile::tempdir().expect("tempdir");
    let request = base_request(
        source_dir.path().to_path_buf(),
        artifact_dir.path(),
        cache_root.path(),
        Installer::Auto,
    );

    let err = builder()
        .build(&request)
        .await
        .expect_err("build should fail: main.py has no `hello` attribute");
    assert!(
        matches!(err, PythonBuildError::EntryPoint(_)),
        "expected PythonBuildError::EntryPoint, got {err:?}"
    );

    let log =
        std::fs::read_to_string(artifact_dir.path().join("build.log")).expect("read build.log");
    assert!(
        log.contains("AttributeError"),
        "log should contain AttributeError for the missing entry point: {log}"
    );
}

#[tokio::test]
async fn a_retry_at_the_same_artifact_dir_after_a_failed_build_succeeds() {
    // Regression: a build attempt that fails after venv creation (e.g. the
    // missing-entry-point case above) leaves a partial venv behind. Because
    // a revision number is only advanced on *success* (registry::service's
    // `existing.current_revision + 1`, reused across repeated failures), a
    // retry -- with the problem fixed -- targets the exact same
    // `artifact_dir`/`venv_dir`. `uv venv`/`python -m venv` used to refuse
    // to create a venv over one that already exists, so the retry failed on
    // the venv step regardless of whether the real problem was fixed.
    require_python314!();
    let source_dir = tempfile::tempdir().expect("tempdir");
    let artifact_dir = tempfile::tempdir().expect("tempdir");
    let cache_root = tempfile::tempdir().expect("tempdir");

    write_source(
        source_dir.path(),
        "def not_hello(request):\n    return 'hi'\n",
        None,
    );
    let failing_request = base_request(
        source_dir.path().to_path_buf(),
        artifact_dir.path(),
        cache_root.path(),
        Installer::Auto,
    );
    builder()
        .build(&failing_request)
        .await
        .expect_err("first attempt should fail: main.py has no `hello` attribute");
    assert!(
        artifact_dir.path().join("venv/bin").is_dir(),
        "the failed attempt should have left a partial venv behind, or this test isn't exercising the collision"
    );

    write_source(
        source_dir.path(),
        "def hello(request):\n    return 'hi'\n",
        None,
    );
    let retry_request = base_request(
        source_dir.path().to_path_buf(),
        artifact_dir.path(),
        cache_root.path(),
        Installer::Auto,
    );
    builder()
        .build(&retry_request)
        .await
        .expect("retry at the same artifact_dir, with the problem fixed, should succeed");
}

#[tokio::test]
async fn python_bin_pointing_at_a_312_interpreter_is_rejected_as_unsupported() {
    require_python314!();
    let python312_available = std::process::Command::new("python3.12")
        .arg("--version")
        .output()
        .is_ok_and(|o| o.status.success());
    if !python312_available {
        eprintln!("skipping: python3.12 not found on PATH");
        return;
    }

    let artifact_dir = tempfile::tempdir().expect("tempdir");
    let cache_root = tempfile::tempdir().expect("tempdir");
    let mut request = base_request(
        hello_python_http_dir(),
        artifact_dir.path(),
        cache_root.path(),
        Installer::Auto,
    );
    request.python_bin = Some("python3.12".to_string());

    let err = builder()
        .build(&request)
        .await
        .expect_err("build should reject a python_bin that isn't 3.14");
    assert!(
        matches!(err, PythonBuildError::UnsupportedPython { .. }),
        "expected PythonBuildError::UnsupportedPython, got {err:?}"
    );
}
