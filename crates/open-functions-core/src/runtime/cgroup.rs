//! Optional cgroup v2 memory limiting for process-driven instances (FR-014a).
//!
//! Availability is probed once at startup ([`CgroupLimiter::probe`]). If
//! cgroup v2 is not mounted, or this process cannot create/write to a child
//! cgroup under it (no `Delegate=yes`, unprivileged without a systemd user
//! slice, read-only cgroupfs in a container, ...), the limiter disables
//! itself permanently and every subsequent call becomes a silent no-op — an
//! unavailable cgroup must never block instance startup.

use std::sync::atomic::{AtomicBool, Ordering};

use cgroups_rs::CgroupPid;
use cgroups_rs::fs::{Cgroup, Subsystem, hierarchies};

/// Path prefix for open-functions-managed cgroups under the v2 hierarchy root, per
/// plan.md's Design Notes: `open-functions/<function_name>/<instance_id>`.
const ROOT: &str = "open-functions";

pub struct CgroupLimiter {
    /// Becomes `false` permanently after the first failure (probe or apply).
    enabled: AtomicBool,
    /// Whether the one-time unavailability warning has already been logged.
    warned: AtomicBool,
}

impl CgroupLimiter {
    /// A limiter that never applies memory limits, for `runtime.cgroup = "off"`
    /// (ops-config.md). Does not probe availability and never warns — the
    /// operator explicitly opted out.
    pub fn disabled() -> Self {
        Self {
            enabled: AtomicBool::new(false),
            warned: AtomicBool::new(true),
        }
    }

    /// Checks cgroup v2 availability and writability once (by creating and
    /// immediately deleting a throwaway probe cgroup). Logs `tracing::warn!`
    /// exactly once, here, if unavailable — `apply` will then be a silent
    /// no-op for the lifetime of this limiter.
    pub fn probe() -> Self {
        let limiter = Self {
            enabled: AtomicBool::new(true),
            warned: AtomicBool::new(false),
        };

        if !hierarchies::is_cgroup2_unified_mode() {
            limiter.disable("cgroup v2 is not the active unified hierarchy at /sys/fs/cgroup");
            return limiter;
        }

        let probe_path = format!("{ROOT}/.probe");
        match Cgroup::new(hierarchies::auto(), probe_path) {
            Ok(cg) => {
                // Best-effort cleanup of the probe cgroup; nothing to react to
                // if this fails, `apply`/`cleanup` will each try again per
                // instance.
                let _ = cg.delete();
            }
            Err(err) => {
                limiter.disable(&format!("cgroup v2 present but not writable: {err}"));
            }
        }

        limiter
    }

    /// Disables the limiter and logs the one-time warning, if not already logged.
    fn disable(&self, reason: &str) {
        self.enabled.store(false, Ordering::SeqCst);
        if !self.warned.swap(true, Ordering::SeqCst) {
            tracing::warn!(
                reason,
                "cgroup v2 memory limiting unavailable; instances will run without a memory limit"
            );
        }
    }

    /// Creates (or reuses) a cgroup for this instance at
    /// `open-functions/<function_name>/<instance_id>`, sets `memory.max`, and adds
    /// `pid` to it.
    ///
    /// If the limiter has already been disabled (probe failed, or a previous
    /// call to this method failed), this is a silent no-op returning `Ok(())`.
    /// Per FR-014a, an unavailable cgroup must never block instance startup:
    /// a failure here also just disables the limiter (warning at most once
    /// across the limiter's lifetime) rather than propagating an error.
    pub fn apply(
        &self,
        function_name: &str,
        instance_id: &str,
        pid: u32,
        memory_mib: u32,
    ) -> std::io::Result<()> {
        if !self.enabled.load(Ordering::SeqCst) {
            return Ok(());
        }

        let path = format!("{ROOT}/{function_name}/{instance_id}");
        let result = (|| -> cgroups_rs::fs::error::Result<()> {
            let cg = Cgroup::new(hierarchies::auto(), path)?;
            for sub in cg.subsystems() {
                if let Subsystem::Mem(mem) = sub {
                    mem.set_limit(i64::from(memory_mib) * 1024 * 1024)?;
                }
            }
            // `cgroup.procs` (not `cgroup.threads`) is the right target for a
            // whole process: add_task_by_tgid writes there.
            cg.add_task_by_tgid(CgroupPid::from(u64::from(pid)))?;
            Ok(())
        })();

        if let Err(err) = result {
            self.disable(&format!("failed to apply cgroup memory limit: {err}"));
        }

        Ok(())
    }

    /// Removes the cgroup (best-effort; logs a warning on failure, doesn't error).
    pub fn cleanup(&self, function_name: &str, instance_id: &str) {
        if !self.enabled.load(Ordering::SeqCst) {
            return;
        }

        let path = format!("{ROOT}/{function_name}/{instance_id}");
        let cg = Cgroup::load(hierarchies::auto(), path.clone());
        if let Err(err) = cg.delete() {
            tracing::warn!(
                function = %function_name,
                instance = %instance_id,
                path = %path,
                %err,
                "failed to remove cgroup for instance"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::Ordering;

    /// In this sandboxed environment `/sys/fs/cgroup` is not writable by us
    /// (confirmed manually: cgroup v2 is mounted but `open-functions/...` cannot be
    /// created), so `probe()` must disable itself without panicking, `apply`
    /// must still return `Ok(())`, and the one-time warning must fire at most
    /// once even across many `apply` calls.
    #[test]
    fn apply_is_always_ok_and_warns_at_most_once() {
        let limiter = CgroupLimiter::probe();

        for i in 0..5 {
            let result = limiter.apply("test-fn", &format!("instance-{i}"), 999_999, 128);
            assert!(result.is_ok());
        }

        // Once `warned` is true it must never flip back to false; this is the
        // only way to check "at most once" without capturing log output.
        let warned_after = limiter.warned.load(Ordering::SeqCst);
        if !limiter.enabled.load(Ordering::SeqCst) {
            assert!(
                warned_after,
                "disabled limiter must have warned exactly once"
            );
        }

        // cleanup must never panic regardless of enabled state.
        limiter.cleanup("test-fn", "instance-0");
    }
}
