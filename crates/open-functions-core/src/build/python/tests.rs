//! Unit tests (T019) for the pure/std-only pieces of the Python build
//! pipeline: `requirements.rs`'s PEP 503 normalization, `snapshot.rs`'s
//! exclude rules, and `env.rs`'s allowlist.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeMap;

use super::requirements::build_requirements_content;
use super::{env, snapshot};

// ---- requirements.rs ----

#[test]
fn detects_functions_framework_under_pep503_variants() {
    for existing in [
        "Functions_Framework\n",
        "functions-framework[x]>=3\n",
        "functions-framework==3.10.2 ; python_version>='3.10'\n",
    ] {
        let out = build_requirements_content(Some(existing), "functions-framework==3.10.2");
        assert_eq!(
            out, existing,
            "an existing functions-framework declaration ({existing:?}) must be preserved verbatim, with nothing appended"
        );
    }
}

#[test]
fn comment_and_option_lines_are_not_mistaken_for_a_declaration_and_are_preserved() {
    let existing = "# functions-framework pinned elsewhere\n-r base.txt\n--index-url https://example.invalid/simple\nrequests==2.31.0\n";
    let out = build_requirements_content(Some(existing), "functions-framework==3.10.2");
    assert!(
        out.starts_with(existing),
        "comment/option/unrelated lines must be preserved verbatim: {out:?}"
    );
    assert!(
        out.ends_with("functions-framework==3.10.2\n"),
        "functions-framework must be appended since no real requirement line declared it: {out:?}"
    );
}

#[test]
fn appends_configured_spec_when_missing() {
    let out = build_requirements_content(Some("requests==2.31.0\n"), "functions-framework==3.10.2");
    assert_eq!(out, "requests==2.31.0\nfunctions-framework==3.10.2\n");
}

#[test]
fn no_requirements_file_yields_only_the_appended_line() {
    let out = build_requirements_content(None, "functions-framework==3.10.2");
    assert_eq!(out, "functions-framework==3.10.2\n");
}

// ---- snapshot.rs ----

#[test]
fn snapshot_excludes_venvs_caches_and_pyc_but_copies_everything_else() {
    let src = tempfile::tempdir().expect("tempdir");
    let dst = tempfile::tempdir().expect("tempdir");

    std::fs::write(
        src.path().join("main.py"),
        "def hello(request):\n    return 'hi'\n",
    )
    .expect("write main.py");
    std::fs::write(src.path().join("requirements.txt"), "requests==2.31.0\n").expect("write reqs");
    std::fs::write(src.path().join("stale.pyc"), b"bytecode").expect("write pyc");
    for dir in [
        ".venv",
        "venv",
        "__pycache__",
        ".git",
        ".mypy_cache",
        ".pytest_cache",
        ".ruff_cache",
    ] {
        let d = src.path().join(dir);
        std::fs::create_dir_all(&d).expect("mkdir excluded dir");
        std::fs::write(d.join("marker"), b"x").expect("write marker");
    }
    let sub = src.path().join("pkg");
    std::fs::create_dir_all(&sub).expect("mkdir pkg");
    std::fs::write(sub.join("__init__.py"), "").expect("write pkg init");

    snapshot::snapshot_source(src.path(), dst.path()).expect("snapshot_source");

    assert!(dst.path().join("main.py").exists());
    assert!(dst.path().join("requirements.txt").exists());
    assert!(dst.path().join("pkg/__init__.py").exists());
    assert!(!dst.path().join("stale.pyc").exists());
    for dir in [
        ".venv",
        "venv",
        "__pycache__",
        ".git",
        ".mypy_cache",
        ".pytest_cache",
        ".ruff_cache",
    ] {
        assert!(
            !dst.path().join(dir).exists(),
            "{dir} should have been excluded from the snapshot"
        );
    }
}

#[cfg(unix)]
#[test]
fn snapshot_recreates_symlinks_rather_than_dereferencing_them() {
    let src = tempfile::tempdir().expect("tempdir");
    let dst = tempfile::tempdir().expect("tempdir");
    std::fs::write(src.path().join("real.py"), "TARGET = 1\n").expect("write real.py");
    std::os::unix::fs::symlink("real.py", src.path().join("link.py")).expect("symlink");

    snapshot::snapshot_source(src.path(), dst.path()).expect("snapshot_source");

    let link = dst.path().join("link.py");
    let meta = std::fs::symlink_metadata(&link).expect("symlink_metadata");
    assert!(
        meta.file_type().is_symlink(),
        "link.py should stay a symlink"
    );
    assert_eq!(
        std::fs::read_link(&link).expect("read_link"),
        std::path::PathBuf::from("real.py")
    );
}

// ---- env.rs ----

#[test]
fn passthrough_env_allows_uv_pip_and_proxy_vars_but_drops_uv_python() {
    let mut host_env = BTreeMap::new();
    host_env.insert(
        "UV_INDEX".to_string(),
        "https://example.invalid".to_string(),
    );
    host_env.insert("UV_PYTHON".to_string(), "/usr/bin/python3.12".to_string());
    host_env.insert(
        "PIP_INDEX_URL".to_string(),
        "https://example.invalid".to_string(),
    );
    host_env.insert("HTTP_PROXY".to_string(), "http://proxy:8080".to_string());
    host_env.insert("https_proxy".to_string(), "http://proxy:8080".to_string());
    host_env.insert("NO_PROXY".to_string(), "localhost".to_string());
    host_env.insert("SSL_CERT_FILE".to_string(), "/etc/ssl/cert.pem".to_string());
    host_env.insert("SSL_CERT_DIR".to_string(), "/etc/ssl/certs".to_string());
    host_env.insert("NETRC".to_string(), "/home/u/.netrc".to_string());
    host_env.insert("HOME".to_string(), "/home/u".to_string());
    host_env.insert("PATH".to_string(), "/usr/bin".to_string());
    host_env.insert("SECRET_TOKEN".to_string(), "shh".to_string());

    let cache_root = std::path::Path::new("/data/cache");
    let env = env::passthrough_env(&host_env, cache_root);

    assert_eq!(
        env.get("UV_INDEX").map(String::as_str),
        Some("https://example.invalid")
    );
    assert_eq!(
        env.get("PIP_INDEX_URL").map(String::as_str),
        Some("https://example.invalid")
    );
    assert_eq!(
        env.get("HTTP_PROXY").map(String::as_str),
        Some("http://proxy:8080")
    );
    assert_eq!(
        env.get("https_proxy").map(String::as_str),
        Some("http://proxy:8080")
    );
    assert_eq!(env.get("NO_PROXY").map(String::as_str), Some("localhost"));
    assert_eq!(
        env.get("SSL_CERT_FILE").map(String::as_str),
        Some("/etc/ssl/cert.pem")
    );
    assert_eq!(
        env.get("SSL_CERT_DIR").map(String::as_str),
        Some("/etc/ssl/certs")
    );
    assert_eq!(env.get("NETRC").map(String::as_str), Some("/home/u/.netrc"));

    assert!(!env.contains_key("UV_PYTHON"), "UV_PYTHON must be dropped");
    assert!(!env.contains_key("HOME"), "HOME must not pass through");
    assert!(!env.contains_key("PATH"), "PATH must not pass through");
    assert!(
        !env.contains_key("SECRET_TOKEN"),
        "arbitrary host vars must not pass through"
    );
}

#[test]
fn passthrough_env_host_overrides_win_over_user_values() {
    let mut host_env = BTreeMap::new();
    host_env.insert("UV_CACHE_DIR".to_string(), "/somewhere/else".to_string());
    host_env.insert("PIP_CACHE_DIR".to_string(), "/somewhere/else".to_string());
    host_env.insert("UV_PYTHON_DOWNLOADS".to_string(), "automatic".to_string());

    let cache_root = std::path::Path::new("/data/cache");
    let env = env::passthrough_env(&host_env, cache_root);

    assert_eq!(
        env.get("UV_CACHE_DIR").map(String::as_str),
        Some("/data/cache/uv")
    );
    assert_eq!(
        env.get("PIP_CACHE_DIR").map(String::as_str),
        Some("/data/cache/pip")
    );
    assert_eq!(
        env.get("UV_PYTHON_DOWNLOADS").map(String::as_str),
        Some("never")
    );
    assert_eq!(
        env.get("UV_NO_MANAGED_PYTHON").map(String::as_str),
        Some("1")
    );
    assert_eq!(
        env.get("PIP_DISABLE_PIP_VERSION_CHECK").map(String::as_str),
        Some("1")
    );
}
