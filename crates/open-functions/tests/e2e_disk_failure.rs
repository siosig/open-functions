//! E2E test for the "persistent storage is not readable/writable" edge case
//! (spec.md's Edge Cases, FR-025): `serve` must fail fast with a clear error
//! and exit code 2 (`EXIT_CONFIG_ERROR`, ops-config.md's exit-code table)
//! rather than silently starting in a volatile/degraded mode when
//! `storage.data_dir` cannot be created or opened.
//!
//! Runs as the real (non-root) test user, so a parent directory with its
//! write bit removed reliably makes `create_dir_all` fail with a permission
//! error — this would not reproduce as root, where permission checks are
//! bypassed.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::os::unix::fs::PermissionsExt;
use std::process::{Command, Stdio};
use std::time::Duration;

fn workspace_root() -> std::path::PathBuf {
    std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("crates/")
        .parent()
        .expect("workspace root")
        .to_path_buf()
}

/// Skips (rather than fails) under a root-equivalent test runner, where
/// permission bits don't block directory creation and this scenario cannot
/// be reproduced at all. Shells out to `id -u` rather than pulling in the
/// `libc` crate for a single syscall.
fn running_as_root() -> bool {
    Command::new("id")
        .arg("-u")
        .output()
        .ok()
        .map(|out| String::from_utf8_lossy(&out.stdout).trim() == "0")
        .unwrap_or(false)
}

#[test]
fn serve_exits_with_config_error_when_data_dir_is_unwritable() {
    if running_as_root() {
        eprintln!("skipping: test runs as root, permission bits don't block mkdir");
        return;
    }

    let root = tempfile::tempdir().expect("tempdir");
    let parent = root.path().join("readonly-parent");
    std::fs::create_dir(&parent).expect("create parent dir");
    // r-xr-xr-x: readable/traversable but not writable, so creating a child
    // directory inside it fails with a permission error.
    std::fs::set_permissions(&parent, std::fs::Permissions::from_mode(0o555))
        .expect("chmod parent read-only");
    let unwritable_data_dir = parent.join("data");

    let bin = assert_cmd::cargo::cargo_bin("open-functions");
    let mut child = Command::new(bin)
        .current_dir(workspace_root())
        .args([
            "serve",
            "--data-dir",
            &unwritable_data_dir.to_string_lossy(),
            "--invoke-listen",
            "127.0.0.1:28380",
            "--admin-listen",
            "127.0.0.1:28381",
        ])
        // `ops::init_tracing`'s default (json/text) writer is stdout, not
        // stderr, so the startup-failure log line lands there.
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn open-functions serve");

    let status = wait_with_timeout(&mut child, Duration::from_secs(10)).unwrap_or_else(|| {
        let _ = child.kill();
        panic!(
            "open-functions serve did not exit within 10s against an unwritable data_dir \
                 (it should fail fast at startup instead of starting in a degraded mode)"
        )
    });

    // Restore write permission so `tempdir`'s own Drop cleanup can remove
    // `parent` (it would otherwise leak on disk).
    std::fs::set_permissions(&parent, std::fs::Permissions::from_mode(0o755))
        .expect("restore parent permissions for cleanup");

    assert_eq!(
        status.code(),
        Some(2),
        "expected EXIT_CONFIG_ERROR (2) per ops-config.md's exit-code table, got {status:?}"
    );

    let mut stdout = String::new();
    use std::io::Read;
    child
        .stdout
        .take()
        .expect("captured stdout")
        .read_to_string(&mut stdout)
        .expect("read stdout");
    assert!(
        stdout.contains("data_dir") || stdout.to_lowercase().contains("permission"),
        "expected the startup-failure log line to name the failing setting or reason, got: {stdout}"
    );
}

fn wait_with_timeout(
    child: &mut std::process::Child,
    timeout: Duration,
) -> Option<std::process::ExitStatus> {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        if let Ok(Some(status)) = child.try_wait() {
            return Some(status);
        }
        if std::time::Instant::now() >= deadline {
            return None;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}
