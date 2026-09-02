//! Startup restore (T060): reconciles state left by an unclean shutdown and
//! recreates the in-memory `InstancePool`s that redb never persists —
//! `RegistryService`'s `pools` map always starts empty on a fresh process,
//! so every `Ready` function's pool must be rebuilt from its persisted
//! current `Revision` before the invoke listener starts accepting traffic
//! (`ops-config.md`'s `READY=1` fires only after this completes, per
//! `serve`'s call site).
//!
//! Per data-model.md's Build entity note: a build left `running` by a crash
//! or SIGKILL is reconciled to `failed` (`last_error = "interrupted by
//! restart"`), and its function falls back to `failed` (or stays `ready` if
//! an older revision was already serving) — never left stuck `building`
//! forever.
//!
//! The Pub/Sub binding reconciler's retry sweep (`Reconciler::spawn_retry_loop`,
//! started independently by `serve`) already re-derives its own state from
//! `list_bindings()` on its first tick, so resuming `pending`/`unbinding`
//! bindings needs no separate action here.

use crate::model::build::BuildStatus;
use crate::model::function::FunctionState;
use crate::registry::service::RegistryService;
use crate::registry::store::StoreError;

/// Summary of what restore did, for the startup log line `serve` emits.
#[derive(Debug, Default, Clone)]
pub struct RestoreReport {
    /// Builds found `running` (interrupted by an unclean shutdown) and
    /// reconciled to `failed`.
    pub builds_marked_interrupted: usize,
    /// `Ready` functions whose `InstancePool` was successfully recreated.
    pub functions_restored: usize,
    /// `Ready` functions whose current revision's build artifact is missing
    /// from disk (`storage.data_dir` moved, volume not actually persistent,
    /// etc.) — the function stays `ready` in the store (its next invocation
    /// will surface a clear spawn error) but is flagged here so `serve` can
    /// warn loudly at startup instead of failing silently until first use.
    pub broken_functions: Vec<String>,
    /// Functions whose `min_instances` pre-warm did not fully succeed
    /// (already logged individually via `tracing::warn!`).
    pub warm_start_failures: Vec<String>,
}

impl RegistryService {
    /// Runs the full startup restore sequence once, before the caller
    /// accepts any invoke/admin traffic. Idempotent to call again (it only
    /// ever reconciles from the store's current state), but `serve` calls it
    /// exactly once, right after `RegistryService::new`.
    pub async fn restore(&self) -> Result<RestoreReport, StoreError> {
        let mut report = RestoreReport::default();
        self.reconcile_interrupted_builds(&mut report).await?;
        self.restore_ready_function_pools(&mut report).await?;
        Ok(report)
    }

    async fn reconcile_interrupted_builds(
        &self,
        report: &mut RestoreReport,
    ) -> Result<(), StoreError> {
        for build in self.store.list_builds()? {
            if build.status != BuildStatus::Running {
                continue;
            }

            let mut updated = build;
            updated.status = BuildStatus::Failed;
            updated.finished_at = Some(chrono::Utc::now());
            self.store.put_build(&updated)?;
            report.builds_marked_interrupted += 1;

            if let Some(mut function) = self.store.get_function(&updated.function_name)? {
                function.last_error = Some("interrupted by restart".to_string());
                function.state = if function.current_revision.is_some() {
                    FunctionState::Ready
                } else {
                    FunctionState::Failed
                };
                function.updated_at = chrono::Utc::now();
                self.store.put_function(&function)?;
            }
        }
        Ok(())
    }

    async fn restore_ready_function_pools(
        &self,
        report: &mut RestoreReport,
    ) -> Result<(), StoreError> {
        for function in self.store.list_functions()? {
            if function.state != FunctionState::Ready {
                continue;
            }
            let Some(revision_number) = function.current_revision else {
                continue;
            };
            let Some(revision) = self.store.get_revision(&function.name, revision_number)? else {
                tracing::warn!(
                    function = %function.name,
                    revision = revision_number,
                    "restore: current_revision has no stored Revision record; leaving unrestored"
                );
                continue;
            };

            if let Some(artifact_path) = &revision.artifact_path
                && !std::path::Path::new(artifact_path).exists()
            {
                tracing::warn!(
                    function = %function.name,
                    artifact_path,
                    "restore: function is ready but its build artifact is missing on disk (broken)"
                );
                report.broken_functions.push(function.name.clone());
            }

            // `activate_revision` unconditionally clears `last_error` (correct
            // for its primary caller, a freshly *succeeded* deploy) — but here
            // `function.last_error` may already carry an "interrupted by
            // restart" marker this same restore pass just set (see
            // `reconcile_interrupted_builds`, above) for an in-progress
            // redeploy that never finished. Re-apply it afterward so restore
            // doesn't silently erase that signal from `fn describe`.
            let last_error_before_activate = function.last_error.clone();
            if let Err(err) = self.activate_revision(&function.name, &revision).await {
                tracing::warn!(function = %function.name, %err, "restore: failed to recreate instance pool");
                continue;
            }
            if last_error_before_activate.is_some()
                && let Some(mut current) = self.store.get_function(&function.name)?
            {
                current.last_error = last_error_before_activate;
                self.store.put_function(&current)?;
            }
            report.functions_restored += 1;

            if function.min_instances > 0
                && let Some(pool) = self.pool_for(&function.name).await
                && let Err(err) = pool.warm_to(function.min_instances).await
            {
                tracing::warn!(function = %function.name, %err, "restore: failed to pre-warm min_instances");
                report.warm_start_failures.push(function.name.clone());
            }
        }
        Ok(())
    }
}
