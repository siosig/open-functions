//! Registry deploy/list/get/delete orchestration (T040, extended by US2's
//! T053). Ties together `Store` (persistence), `Builder` (source →
//! artifact), `InstancePool` (running instances), and (for Pub/Sub-triggered
//! functions) the [`Reconciler`] into the operations
//! `contracts/admin-api.md`'s admin API exposes.
//!
//! Container-image sources / container builds (US4) are modeled in the data
//! types already but not yet wired up here — `register` rejects
//! `Source::Image` for now with `RegisterError::Unsupported`.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{Mutex, Semaphore};

use crate::build::{BuildError, BuildRequest, Builder};
use crate::model::TriggerBinding;
use crate::model::build::{Build, BuildMode, BuildStatus};
use crate::model::function::{
    Function, FunctionState, QueuePolicy as ModelQueuePolicy, Source, Trigger,
};
use crate::model::revision::Revision;
use crate::model::validate::{self, ValidationError};
use crate::pool::{InstancePool, PoolConfig, QueuePolicy as PoolQueuePolicy};
use crate::pubsub::reconcile::{DesiredBinding, Reconciler};
use crate::registry::store::{Store, StoreError};
use crate::runtime::{Driver, InstanceSpec};

/// Pub/Sub-binding configuration, present only when `pubsub.enabled` (the
/// caller — `cf-rs`'s `serve` — constructs a [`Reconciler`] and this struct
/// together, or omits both when Pub/Sub support is disabled).
pub struct PubsubBindingConfig {
    pub reconciler: Arc<Reconciler>,
    pub project: String,
    /// Base URL ps-rs should POST Push deliveries to, e.g.
    /// `http://127.0.0.1:8080`; the full push endpoint is
    /// `{push_base_url}/_cf/push/{function_name}`.
    pub push_base_url: String,
    pub ack_deadline_max_secs: u32,
}

/// Defaults applied to fields the caller of [`RegistryService::register`]
/// left unset, per `ops-config.md`'s `[defaults]` section / GCP's own
/// Cloud Run functions defaults (research.md R12).
#[derive(Debug, Clone)]
pub struct RegistrationDefaults {
    pub timeout_secs: u32,
    pub concurrency: u32,
    pub memory_mib: u32,
    pub min_instances: u32,
    pub max_instances: u32,
    pub idle_timeout_secs: u32,
    pub queue_policy: ModelQueuePolicy,
    pub queue_max_wait_secs: u32,
}

/// Fields a caller may specify when registering a function; `None` means
/// "use the configured default". Mirrors `contracts/admin-api.md`'s `PUT`
/// request body.
#[derive(Debug, Clone, Default)]
pub struct RegisterRequest {
    pub name: String,
    pub trigger: Trigger,
    pub source: Source,
    pub entry_point: Option<String>,
    pub env: std::collections::BTreeMap<String, String>,
    pub timeout_secs: Option<u32>,
    pub concurrency: Option<u32>,
    pub memory_mib: Option<u32>,
    pub min_instances: Option<u32>,
    pub max_instances: Option<u32>,
    pub idle_timeout_secs: Option<u32>,
    pub queue_policy: Option<ModelQueuePolicy>,
    pub queue_max_wait_secs: Option<u32>,
}

#[derive(Debug, thiserror::Error)]
pub enum RegisterError {
    #[error(transparent)]
    Validation(#[from] ValidationError),
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error("source kind not yet supported: {0}")]
    Unsupported(&'static str),
    #[error("a build for {0:?} is already in progress; retry, or pass force to cancel it")]
    BuildInProgress(String),
    #[error("source path {0:?} does not exist or is not a directory")]
    SourceNotFound(PathBuf),
}

/// Result of `register`: the build has been accepted and started in the
/// background. Poll `GET /v1/functions/{name}/builds/{id}` (via
/// [`RegistryService::get_build`]) for completion.
#[derive(Debug, Clone)]
pub struct RegisterAccepted {
    pub revision: u32,
    pub build_id: String,
}

#[derive(Debug, thiserror::Error)]
pub enum DeleteError {
    #[error("function {0:?} not found")]
    NotFound(String),
    #[error(transparent)]
    Store(#[from] StoreError),
}

/// Ties `Store` + `Builder` + `Driver` + per-function `InstancePool`s
/// together. One `RegistryService` per running `cf-rs` process.
pub struct RegistryService {
    // `pub(crate)` (rather than private) so `registry::restore` can query
    // the store directly (`list_builds`/`list_functions`/`get_revision`,
    // etc.) without a wrapper method per call.
    pub(crate) store: Arc<dyn Store>,
    builder: Arc<dyn Builder>,
    driver: Arc<dyn Driver>,
    global_limit: Arc<Semaphore>,
    artifacts_dir: PathBuf,
    build_dir: PathBuf,
    build_timeout: Duration,
    defaults: RegistrationDefaults,
    pools: Mutex<HashMap<String, Arc<InstancePool>>>,
    reapers: Mutex<HashMap<String, tokio::task::JoinHandle<()>>>,
    /// Function names with a build currently running, so a second concurrent
    /// `register` on the same name is rejected (`RegisterError::BuildInProgress`)
    /// rather than racing (admin-api.md's 409 `ABORTED`).
    building: Mutex<std::collections::HashSet<String>>,
    /// `None` when `pubsub.enabled = false`: `register`/`delete` then skip
    /// binding management entirely for `Trigger::Pubsub` functions (they
    /// still register and build normally, they just never receive events).
    pubsub: Option<PubsubBindingConfig>,
}

impl RegistryService {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        store: Arc<dyn Store>,
        builder: Arc<dyn Builder>,
        driver: Arc<dyn Driver>,
        global_limit: Arc<Semaphore>,
        data_dir: &Path,
        build_timeout: Duration,
        defaults: RegistrationDefaults,
        pubsub: Option<PubsubBindingConfig>,
    ) -> Self {
        Self {
            store,
            builder,
            driver,
            global_limit,
            artifacts_dir: data_dir.join("artifacts"),
            build_dir: data_dir.join("build"),
            build_timeout,
            defaults,
            pools: Mutex::new(HashMap::new()),
            reapers: Mutex::new(HashMap::new()),
            building: Mutex::new(std::collections::HashSet::new()),
            pubsub,
        }
    }

    pub fn get(&self, name: &str) -> Result<Option<Function>, StoreError> {
        self.store.get_function(name)
    }

    pub fn list(&self) -> Result<Vec<Function>, StoreError> {
        self.store.list_functions()
    }

    pub fn get_revision(&self, name: &str, number: u32) -> Result<Option<Revision>, StoreError> {
        self.store.get_revision(name, number)
    }

    pub fn get_build(&self, id: &str) -> Result<Option<Build>, StoreError> {
        self.store.get_build(id)
    }

    pub fn get_binding(&self, name: &str) -> Result<Option<TriggerBinding>, StoreError> {
        self.store.get_binding(name)
    }

    /// Number of currently running instances for `name` (0 if the function
    /// has no pool yet, e.g. never successfully deployed).
    pub async fn instance_count(&self, name: &str) -> usize {
        let pools = self.pools.lock().await;
        match pools.get(name) {
            Some(pool) => pool.instance_count().await,
            None => 0,
        }
    }

    /// Looks up the `InstancePool` for an HTTP/Push invocation. `None` means
    /// "not deployed" or "not yet ready" — the invoke handler maps that to
    /// 404/503 per `function-contract.md`'s status table.
    pub async fn pool_for(&self, name: &str) -> Option<Arc<InstancePool>> {
        self.pools.lock().await.get(name).cloned()
    }

    /// Stops every running instance across every function's pool (SIGTERM →
    /// `grace` → SIGKILL), for graceful process shutdown (T061). Pools are
    /// stopped concurrently with each other (independent functions), each
    /// pool's own instances also concurrently (`InstancePool::stop_all`).
    pub async fn shutdown_all_instances(&self, grace: Duration) {
        let pools: Vec<Arc<InstancePool>> = self.pools.lock().await.values().cloned().collect();
        let mut tasks = Vec::with_capacity(pools.len());
        for pool in pools {
            tasks.push(tokio::spawn(async move {
                pool.stop_all(grace).await;
            }));
        }
        for task in tasks {
            let _ = task.await;
        }
    }

    /// Validates and accepts a registration, then builds and deploys it in
    /// the background. Returns as soon as the request is accepted (structural
    /// validation done, no build has necessarily completed yet) — this is the
    /// `202 Accepted` behavior `admin-api.md`'s `PUT` describes.
    pub async fn register(&self, req: RegisterRequest) -> Result<RegisterAccepted, RegisterError> {
        let Source::Dir { path, bin } = &req.source else {
            return Err(RegisterError::Unsupported(
                "source.kind = image (container images land in US4)",
            ));
        };
        let source_path = PathBuf::from(path);
        if !source_path.is_dir() {
            return Err(RegisterError::SourceNotFound(source_path));
        }

        {
            let mut building = self.building.lock().await;
            if building.contains(&req.name) {
                return Err(RegisterError::BuildInProgress(req.name.clone()));
            }
            building.insert(req.name.clone());
        }

        let result = self
            .register_inner(req.clone(), source_path, bin.clone())
            .await;

        self.building.lock().await.remove(&req.name);
        result
    }

    async fn register_inner(
        &self,
        req: RegisterRequest,
        source_path: PathBuf,
        bin: Option<String>,
    ) -> Result<RegisterAccepted, RegisterError> {
        let now = chrono::Utc::now();
        let existing = self.store.get_function(&req.name)?;
        let revision_number = existing
            .as_ref()
            .and_then(|f| f.current_revision)
            .unwrap_or(0)
            + 1;

        let function = Function {
            name: req.name.clone(),
            trigger: req.trigger.clone(),
            source: req.source.clone(),
            env: req.env.clone(),
            entry_point: req
                .entry_point
                .clone()
                .unwrap_or_else(|| "function".to_string()),
            timeout_secs: req.timeout_secs.unwrap_or(self.defaults.timeout_secs),
            concurrency: req.concurrency.unwrap_or(self.defaults.concurrency),
            memory_mib: req.memory_mib.unwrap_or(self.defaults.memory_mib),
            min_instances: req.min_instances.unwrap_or(self.defaults.min_instances),
            max_instances: req.max_instances.unwrap_or(self.defaults.max_instances),
            idle_timeout_secs: req
                .idle_timeout_secs
                .unwrap_or(self.defaults.idle_timeout_secs),
            queue_policy: req
                .queue_policy
                .unwrap_or_else(|| self.defaults.queue_policy.clone()),
            queue_max_wait_secs: req
                .queue_max_wait_secs
                .unwrap_or(self.defaults.queue_max_wait_secs),
            state: FunctionState::Building,
            // Keep the previous `current_revision` (if any) until the new one
            // is proven ready — a failed re-deploy must leave the old version
            // serving, per FR-007 / plan.md's registry deploy-flow design.
            current_revision: existing.as_ref().and_then(|f| f.current_revision),
            last_error: None,
            created_at: existing.as_ref().map(|f| f.created_at).unwrap_or(now),
            updated_at: now,
        };
        validate::validate_function(&function)?;
        self.store.put_function(&function)?;

        // FR-010: a Pub/Sub-triggered function's binding is created at
        // registration time, independent of whether the build succeeds —
        // the function isn't reachable yet either way, and ps-rs's own
        // retry policy handles a temporarily-unready push endpoint the same
        // as it would a slow HTTP function.
        if let Trigger::Pubsub { topic } = &function.trigger {
            self.bind_pubsub_trigger(&function.name, topic, function.timeout_secs)
                .await;
        }

        let build_id = uuid::Uuid::new_v4().simple().to_string();
        let artifact_dir = self
            .artifacts_dir
            .join(&req.name)
            .join(revision_number.to_string());
        let artifact_path = artifact_dir.join("function");
        let log_path = artifact_dir.join("build.log");
        let cargo_target_dir = self.build_dir.join(&req.name).join("target");

        let build = Build {
            id: build_id.clone(),
            function_name: req.name.clone(),
            revision: revision_number,
            mode: BuildMode::Host,
            status: BuildStatus::Running,
            log_path: log_path.to_string_lossy().to_string(),
            exit_code: None,
            started_at: now,
            finished_at: None,
        };
        self.store.put_build(&build)?;

        let build_request = BuildRequest {
            function_name: req.name.clone(),
            revision: revision_number,
            source_dir: source_path,
            bin,
            artifact_path: artifact_path.clone(),
            log_path,
            cargo_target_dir,
            timeout: self.build_timeout,
        };

        let build_outcome = self.builder.build(&build_request).await;

        let mut build_record = build;
        build_record.finished_at = Some(chrono::Utc::now());
        let (function_state, last_error, revision_ready) = match &build_outcome {
            Ok(()) => {
                build_record.status = BuildStatus::Succeeded;
                build_record.exit_code = Some(0);
                (FunctionState::Ready, None, true)
            }
            Err(err) => {
                build_record.status = BuildStatus::Failed;
                build_record.exit_code = match err {
                    BuildError::NonZeroExit(code, _) => Some(*code),
                    _ => None,
                };
                // Per FR-007: a failed re-deploy leaves the prior `ready`
                // revision serving, if there was one.
                let state = if existing
                    .as_ref()
                    .is_some_and(|f| f.state == FunctionState::Ready)
                {
                    FunctionState::Ready
                } else {
                    FunctionState::Failed
                };
                (state, Some(err.to_string()), false)
            }
        };
        self.store.put_build(&build_record)?;

        if revision_ready {
            let snapshot = {
                let mut snap = function.clone();
                snap.state = FunctionState::Ready;
                snap.current_revision = Some(revision_number);
                snap
            };
            let revision = Revision {
                function_name: req.name.clone(),
                number: revision_number,
                artifact_path: Some(artifact_path.to_string_lossy().to_string()),
                image_digest: None,
                build_id: Some(build_id.clone()),
                snapshot,
                created_at: chrono::Utc::now(),
            };
            self.store.put_revision(&revision)?;

            self.activate_revision(&req.name, &revision).await?;
        } else {
            let mut updated = function.clone();
            updated.state = function_state;
            updated.last_error = last_error;
            updated.updated_at = chrono::Utc::now();
            self.store.put_function(&updated)?;
        }

        Ok(RegisterAccepted {
            revision: revision_number,
            build_id,
        })
    }

    /// Builds (or replaces) the `InstancePool` for `name` from a freshly
    /// built `Revision`, atomically switches `current_revision` in the store,
    /// and starts the idle reaper if this is the pool's first activation.
    /// `pub(crate)` (rather than private) so `registry::restore` can reuse
    /// it to recreate pools from a persisted `Revision` at startup, without
    /// duplicating the `InstanceSpec`/`PoolConfig` construction logic.
    pub(crate) async fn activate_revision(
        &self,
        name: &str,
        revision: &Revision,
    ) -> Result<(), RegisterError> {
        let f = &revision.snapshot;
        let artifact_path = revision
            .artifact_path
            .clone()
            .map(PathBuf::from)
            .unwrap_or_default();

        let signature_type: &'static str = match &f.trigger {
            Trigger::Http => "http",
            Trigger::Pubsub { .. } => "cloudevent",
        };

        let spec = InstanceSpec {
            function_name: name.to_string(),
            revision: revision.number,
            entry_point: f.entry_point.clone(),
            signature_type,
            env: f.env.clone(),
            memory_mib: f.memory_mib,
            start_timeout: Duration::from_secs(10),
            artifact_path,
        };

        let pool_config = PoolConfig {
            concurrency: f.concurrency,
            min_instances: f.min_instances,
            max_instances: f.max_instances,
            idle_timeout: Duration::from_secs(u64::from(f.idle_timeout_secs)),
            queue_policy: match f.queue_policy {
                ModelQueuePolicy::Wait => PoolQueuePolicy::Wait,
                ModelQueuePolicy::Reject => PoolQueuePolicy::Reject,
            },
            queue_max_wait: Duration::from_secs(u64::from(f.queue_max_wait_secs)),
            start_timeout: Duration::from_secs(10),
            stop_grace: Duration::from_secs(5),
        };

        let mut pools = self.pools.lock().await;
        match pools.get(name) {
            Some(existing_pool) => {
                // Same function, new revision: swap the template new
                // instances will use. Already-running old-revision instances
                // keep serving until the idle reaper (or a future explicit
                // drain) retires them — per FR-007, no in-flight disruption.
                existing_pool.set_spec_template(spec).await;
            }
            None => {
                let pool = Arc::new(InstancePool::new(
                    name.to_string(),
                    Arc::clone(&self.driver),
                    spec,
                    pool_config,
                    Arc::clone(&self.global_limit),
                ));
                let reaper = Arc::clone(&pool).spawn_idle_reaper();
                self.reapers.lock().await.insert(name.to_string(), reaper);
                pools.insert(name.to_string(), pool);
            }
        }
        drop(pools);

        if let Some(mut current) = self.store.get_function(name)? {
            current.current_revision = Some(revision.number);
            current.state = FunctionState::Ready;
            current.last_error = None;
            current.updated_at = chrono::Utc::now();
            self.store.put_function(&current)?;
        }

        Ok(())
    }

    /// Creates or fixes the ps-rs Push subscription for a Pub/Sub-triggered
    /// function, per FR-010/FR-013. A no-op (with a one-time `warn!`) if
    /// Pub/Sub support is disabled (`pubsub.enabled = false`). Failures
    /// (including ps-rs being unreachable) are not propagated as
    /// `RegisterError` — `Reconciler::try_bind` already persists a `pending`
    /// `TriggerBinding` for the retry sweep to pick up, so a temporarily
    /// down ps-rs must not fail the whole registration.
    async fn bind_pubsub_trigger(&self, function_name: &str, topic: &str, timeout_secs: u32) {
        let Some(pubsub) = &self.pubsub else {
            tracing::warn!(
                function = %function_name,
                "function has a pubsub trigger but pubsub.enabled = false; it will never receive events"
            );
            return;
        };
        let desired = DesiredBinding {
            function_name: function_name.to_string(),
            project: pubsub.project.clone(),
            topic: topic.to_string(),
            push_endpoint: format!(
                "{}/_cf/push/{function_name}",
                pubsub.push_base_url.trim_end_matches('/')
            ),
            // FR-013 note in plan.md: ackDeadlineSeconds = min(600, timeout+10).
            ack_deadline_seconds: pubsub.ack_deadline_max_secs.min(timeout_secs + 10),
        };
        if let Err(err) = pubsub.reconciler.try_bind(&desired).await {
            tracing::warn!(function = %function_name, %err, "failed to persist pubsub binding outcome");
        }
    }

    /// Removes the ps-rs Push subscription for a function being deleted, if
    /// it had a Pub/Sub trigger and Pub/Sub support is enabled.
    async fn unbind_pubsub_trigger(&self, function_name: &str) {
        let Some(pubsub) = &self.pubsub else {
            return;
        };
        if let Err(err) = pubsub.reconciler.try_unbind(function_name).await {
            tracing::warn!(function = %function_name, %err, "failed to persist pubsub unbind outcome");
        }
    }

    /// Stops the function's instances, its idle reaper, removes it from the
    /// store, and deletes its artifacts. Idempotent-ish: deleting an unknown
    /// function is `DeleteError::NotFound`, matching `admin-api.md`'s 404.
    pub async fn delete(&self, name: &str) -> Result<(), DeleteError> {
        let existing = self.store.get_function(name)?;
        let Some(existing) = existing else {
            return Err(DeleteError::NotFound(name.to_string()));
        };

        if matches!(existing.trigger, Trigger::Pubsub { .. }) {
            self.unbind_pubsub_trigger(name).await;
        }

        if let Some(handle) = self.reapers.lock().await.remove(name) {
            handle.abort();
        }
        if let Some(pool) = self.pools.lock().await.remove(name) {
            pool.begin_drain().await;
            // Best-effort: reap whatever's idle right now, and let anything
            // still in-flight finish naturally (its permit drop doesn't stop
            // the instance, but nothing new will be routed to it since the
            // pool itself is being dropped along with the last `Arc` clone
            // once any in-flight forwarder calls complete).
            pool.reap_idle_once().await;
        }

        self.store.delete_function(name)?;

        let function_artifacts_dir = self.artifacts_dir.join(name);
        let _ = tokio::fs::remove_dir_all(&function_artifacts_dir).await;
        let function_build_dir = self.build_dir.join(name);
        let _ = tokio::fs::remove_dir_all(&function_build_dir).await;

        Ok(())
    }
}
