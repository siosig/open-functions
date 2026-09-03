//! Registry deploy/list/get/delete orchestration (T040, extended by US2's
//! T053 and US4's T075). Ties together `Store` (persistence), `Builder`
//! (source → artifact), `InstancePool` (running instances), and (for
//! Pub/Sub-triggered functions) the [`Reconciler`] into the operations
//! `contracts/admin-api.md`'s admin API exposes.
//!
//! `Source::Dir` (source-mode) registrations pick a `Builder`
//! (`host_builder`/`container_builder`) per `build.mode`
//! (`auto`/`host`/`container`, [`BuildModeSetting`]) and always run their
//! resulting instances via `process_driver` — the built artifact is a plain
//! host-runnable executable regardless of where it was compiled.
//! `Source::Image` (image-mode) registrations skip the build step entirely
//! (resolve the image's digest instead) and always run via
//! `container_driver`. Per ops-config.md's "Validation and startup failure" table: an
//! explicit `build.mode = host`/`container` whose tool is unavailable, or
//! `auto` with neither available, is rejected at registration time with
//! `RegisterError::Unsupported` (mapped to `412 FAILED_PRECONDITION` by
//! `admin.rs`) rather than at `open-functions serve` startup — this crate's
//! `RegistryService` has no startup phase of its own to fail during; the
//! contract's stricter "explicit mode unavailable → refuse to start at all"
//! behavior belongs to the `open-functions` binary's own config validation, not here.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{Mutex, Semaphore};

use crate::build::python::{Installer as PythonInstaller, PythonBuildRequest, PythonBuilder};
use crate::build::{BuildError, BuildRequest, Builder};
use crate::logs::ring::LogStore;
use crate::model::TriggerBinding;
use crate::model::build::{Build, BuildMode, BuildStatus};
use crate::model::function::{
    Function, FunctionState, QueuePolicy as ModelQueuePolicy, Source, Trigger,
};
use crate::model::revision::Revision;
use crate::model::runtime::{Runtime, detect_runtime};
use crate::model::validate::{self, ValidationError};
use crate::pool::{InstancePool, PoolConfig, QueuePolicy as PoolQueuePolicy};
use crate::pubsub::reconcile::{DesiredBinding, Reconciler};
use crate::registry::store::{Store, StoreError};
use crate::runtime::docker::{self as docker_helper};
use crate::runtime::launch::{CONTAINER_ARTIFACT_DIR, PythonLaunchMode, python_instance_env};
use crate::runtime::{Driver, InstanceSpec, Launch};

/// `build.mode` (`ops-config.md`'s `[build]` section): which `Builder`
/// source-mode registrations use. Image-mode registrations ignore this
/// entirely (they never build).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BuildModeSetting {
    /// Prefer `host_builder`; fall back to `container_builder` if host
    /// `cargo` isn't usable; reject with 412 if neither is.
    Auto,
    /// Always `host_builder`; reject with 412 if host `cargo` isn't usable.
    Host,
    /// Always `container_builder`; reject with 412 if Docker isn't reachable.
    Container,
}

/// `python.mode` (`ops-config.md`'s `[python]` section): which `PythonBuilder`
/// Python source-mode registrations use. Mirrors [`BuildModeSetting`]'s
/// shape for the Rust pipeline.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PythonModeSetting {
    /// Prefer `host_builder`; fall back to `container_builder` (once T034/T035
    /// wire one in) if no usable Python 3.14 interpreter is found; reject
    /// with 412 if neither is available.
    Auto,
    /// Always `host_builder`; reject with 412 if no usable Python 3.14
    /// interpreter is found.
    Host,
    /// Always `container_builder`; reject with 412 if none is configured or
    /// its Docker daemon isn't reachable.
    Container,
}

/// Everything a Python source-mode registration needs beyond what
/// [`RegistryService`] already carries for Rust: which builder(s) to use,
/// the config values every [`PythonBuildRequest`] is assembled from, and the
/// once-computed install-step env passthrough (`env::passthrough_env`,
/// depends only on the host's own process env and `cache_root`, both fixed
/// for the life of the process).
pub struct PythonSettings {
    pub mode: PythonModeSetting,
    pub host_builder: Arc<dyn PythonBuilder>,
    /// `None` until a `ContainerPythonBuilder` is wired in (T034/T035) --
    /// `python.mode = container`/`auto` then behave as if no container
    /// builder is configured at all.
    pub container_builder: Option<Arc<dyn PythonBuilder>>,
    pub installer: PythonInstaller,
    /// `python.python_bin`; empty means autodetect (`HostPythonBuilder`'s
    /// own `python3.14` -> `python3` -> `python` search).
    pub python_bin: String,
    pub uv_bin: String,
    /// `python.container_image`, recorded on `Revision.container_image` for
    /// container-mode builds and passed to `ContainerPythonBuilder`.
    pub container_image: String,
    /// `python.functions_framework`, e.g. `"functions-framework==3.10.2"`.
    pub functions_framework_spec: String,
    /// `<data_dir>/cache`, the parent of the `uv/`/`pip/` cache subdirectories.
    pub cache_root: PathBuf,
    pub passthrough_env: BTreeMap<String, String>,
}

/// Pub/Sub-binding configuration, present only when `pubsub.enabled` (the
/// caller — `open-functions`'s `serve` — constructs a [`Reconciler`] and this struct
/// together, or omits both when Pub/Sub support is disabled).
pub struct PubsubBindingConfig {
    pub reconciler: Arc<Reconciler>,
    pub project: String,
    /// Base URL open-pubusb should POST Push deliveries to, e.g.
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
    /// Explicit runtime override (002-python-runtime). `None` means
    /// auto-detect from `source` when it's `Source::Dir` (see
    /// `model::detect_runtime`); for `Source::Image` it's stored as-is
    /// (display hint only, not validated -- the image-mode contract is
    /// language-agnostic).
    pub runtime: Option<Runtime>,
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
    /// A `412 FAILED_PRECONDITION` in `admin.rs`'s mapping: the requested
    /// registration (source or image mode) can't be fulfilled by anything
    /// currently available (build tool / Docker daemon / Python
    /// interpreter), per ops-config.md's "Validation and startup failure"
    /// table. `needed` lists the missing tool name(s) (e.g. `["python3.14"]`,
    /// `["python3.14", "docker"]`), surfaced verbatim as `admin.rs`'s
    /// `details.needed` (quickstart.md's own example of this shape).
    #[error("precondition not met: {reason}")]
    Unsupported { reason: String, needed: Vec<String> },
    #[error("a build for {0:?} is already in progress; retry, or pass force to cancel it")]
    BuildInProgress(String),
    #[error("source path {0:?} does not exist or is not a directory")]
    SourceNotFound(PathBuf),
    /// A `400 INVALID_ARGUMENT`: the declared (or auto-detected) `runtime`
    /// doesn't match the source directory's actual contents -- an explicit
    /// `runtime = rust` with no `Cargo.toml`, an explicit `runtime =
    /// python314` with no `main.py`, both present with neither declared
    /// (ambiguous), or neither present (nothing detected).
    #[error("{0}")]
    InvalidRuntime(String),
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

#[derive(Debug, thiserror::Error)]
pub enum StopError {
    #[error("function {0:?} not found")]
    NotFound(String),
    #[error(transparent)]
    Store(#[from] StoreError),
}

/// Ties `Store` + `Builder` + `Driver` + per-function `InstancePool`s
/// together. One `RegistryService` per running `open-functions` process.
pub struct RegistryService {
    // `pub(crate)` (rather than private) so `registry::restore` can query
    // the store directly (`list_builds`/`list_functions`/`get_revision`,
    // etc.) without a wrapper method per call.
    pub(crate) store: Arc<dyn Store>,
    host_builder: Arc<dyn Builder>,
    container_builder: Arc<dyn Builder>,
    process_driver: Arc<dyn Driver>,
    container_driver: Arc<dyn Driver>,
    build_mode: BuildModeSetting,
    /// `runtime.docker_socket` from config, reused to connect a fresh
    /// `bollard::Docker` client for image-mode digest resolution at
    /// registration time (kept separate from `container_driver`'s own
    /// internal client so this module doesn't need to downcast the trait
    /// object to reach it).
    docker_socket: String,
    global_limit: Arc<Semaphore>,
    artifacts_dir: PathBuf,
    build_dir: PathBuf,
    /// Shared cargo registry cache (`<data_dir>/cache/cargo`), passed
    /// through to every `BuildRequest` (host builds ignore it; container
    /// builds bind-mount it — see `BuildRequest::cache_dir`).
    cache_dir: PathBuf,
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
    python: PythonSettings,
    /// Per-function log ring buffers (T079/US5), shared with `process_driver`/
    /// `container_driver` (both drain their instances' output into it) and
    /// exposed read-only to `open-functions`'s `admin.rs` `GET .../logs` (T081) via
    /// [`RegistryService::log_buffer`].
    log_store: Arc<LogStore>,
}

impl RegistryService {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        store: Arc<dyn Store>,
        host_builder: Arc<dyn Builder>,
        container_builder: Arc<dyn Builder>,
        process_driver: Arc<dyn Driver>,
        container_driver: Arc<dyn Driver>,
        build_mode: BuildModeSetting,
        docker_socket: String,
        global_limit: Arc<Semaphore>,
        data_dir: &Path,
        build_timeout: Duration,
        defaults: RegistrationDefaults,
        pubsub: Option<PubsubBindingConfig>,
        log_store: Arc<LogStore>,
        python: PythonSettings,
    ) -> Self {
        Self {
            store,
            host_builder,
            container_builder,
            process_driver,
            container_driver,
            build_mode,
            docker_socket,
            global_limit,
            artifacts_dir: data_dir.join("artifacts"),
            build_dir: data_dir.join("build"),
            cache_dir: data_dir.join("cache").join("cargo"),
            build_timeout,
            defaults,
            pools: Mutex::new(HashMap::new()),
            reapers: Mutex::new(HashMap::new()),
            building: Mutex::new(std::collections::HashSet::new()),
            pubsub,
            log_store,
            python,
        }
    }

    /// The ring buffer of recent log lines for `name` (T079/US5), for
    /// `open-functions`'s `GET .../logs?tail&follow` (T081). Always returns a buffer
    /// (creating an empty one if this function has never logged anything
    /// yet) rather than an `Option` -- an empty tail and "function has no
    /// logs yet" are indistinguishable to a caller anyway, and the admin
    /// handler already 404s separately on an unknown function name via
    /// `RegistryService::get`.
    pub fn log_buffer(&self, name: &str) -> Arc<crate::logs::ring::LogRingBuffer> {
        self.log_store.buffer_for(name)
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

    /// Every `Build` recorded for `name`, in whatever order the store
    /// yields (`admin.rs`'s `revisions[]`/`builds[]` describe fields sort
    /// afterward). No dedicated per-function index exists (`Store` has no
    /// `list_builds_for`), so this filters `list_builds()`'s full result --
    /// acceptable for a local dev tool's admin API, not a hot path.
    pub fn list_builds_for(&self, name: &str) -> Result<Vec<Build>, StoreError> {
        Ok(self
            .store
            .list_builds()?
            .into_iter()
            .filter(|b| b.function_name == name)
            .collect())
    }

    /// Every `Revision` recorded for `name`, from `1` through its current
    /// `current_revision` (inclusive) -- `Store` only exposes `get_revision`
    /// per-number, not a per-function list, so this reconstructs the
    /// sequence from the numbering scheme `register_source_rust`/
    /// `register_source_python`/`register_image` all share (revisions are
    /// always `existing.current_revision.unwrap_or(0) + 1`, so every number
    /// from `1` to the latest was allocated at some point). A number with no
    /// stored `Revision` (a build that failed before ever producing one) is
    /// simply skipped.
    pub fn list_revisions_for(&self, name: &str, up_to: u32) -> Result<Vec<Revision>, StoreError> {
        let mut revisions = Vec::new();
        for number in 1..=up_to {
            if let Some(revision) = self.store.get_revision(name, number)? {
                revisions.push(revision);
            }
        }
        Ok(revisions)
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

    /// Validates and accepts a registration, then builds (source-mode) or
    /// resolves the image digest (image-mode) and deploys it in the
    /// background. Returns as soon as the request is accepted (structural
    /// validation done, no build has necessarily completed yet) — this is the
    /// `202 Accepted` behavior `admin-api.md`'s `PUT` describes.
    pub async fn register(&self, req: RegisterRequest) -> Result<RegisterAccepted, RegisterError> {
        {
            let mut building = self.building.lock().await;
            if building.contains(&req.name) {
                return Err(RegisterError::BuildInProgress(req.name.clone()));
            }
            building.insert(req.name.clone());
        }

        let result = match req.source.clone() {
            Source::Dir { path, bin } => {
                let source_path = PathBuf::from(path);
                if !source_path.is_dir() {
                    Err(RegisterError::SourceNotFound(source_path))
                } else {
                    self.register_source(req.clone(), source_path, bin).await
                }
            }
            Source::Image { image_ref } => self.register_image(req.clone(), image_ref).await,
        };

        self.building.lock().await.remove(&req.name);
        result
    }

    /// Builds the common `Function` record every registration (source- or
    /// image-mode) produces, from `req` and the previous registration (if
    /// any) of the same name. Callers still need to `validate_function` and
    /// `put_function` it themselves — the exact point in each flow where
    /// that should happen differs slightly (image-mode has no build step in
    /// between).
    fn build_function_record(
        &self,
        req: &RegisterRequest,
        existing: Option<&Function>,
    ) -> Function {
        let now = chrono::Utc::now();
        Function {
            name: req.name.clone(),
            trigger: req.trigger.clone(),
            source: req.source.clone(),
            // `req.runtime` is the caller's explicit override (if any) --
            // auto-detection for `Source::Dir` (model::detect_runtime) is
            // resolved by the caller before building this request, so by
            // the time we get here it's already the final value.
            runtime: req.runtime,
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
                .clone()
                .unwrap_or_else(|| self.defaults.queue_policy.clone()),
            queue_max_wait_secs: req
                .queue_max_wait_secs
                .unwrap_or(self.defaults.queue_max_wait_secs),
            state: FunctionState::Building,
            // Keep the previous `current_revision` (if any) until the new one
            // is proven ready — a failed re-deploy must leave the old version
            // serving, per FR-007 / plan.md's registry deploy-flow design.
            current_revision: existing.and_then(|f| f.current_revision),
            last_error: None,
            created_at: existing.map(|f| f.created_at).unwrap_or(now),
            updated_at: now,
        }
    }

    /// Picks the `Builder` for a source-mode registration per `build_mode`,
    /// probing real tool/daemon availability (not just "was a builder
    /// constructed" — both builders always are, cheaply, regardless of
    /// whether their tool is actually usable). Returns
    /// `RegisterError::Unsupported` (412 `FAILED_PRECONDITION`) if the
    /// configured mode's tool isn't available, per ops-config.md's
    /// "Validation and startup failure" table.
    async fn select_source_builder(&self) -> Result<(&Arc<dyn Builder>, BuildMode), RegisterError> {
        match self.build_mode {
            BuildModeSetting::Host => {
                if self.host_builder.is_available().await {
                    Ok((&self.host_builder, BuildMode::Host))
                } else {
                    Err(RegisterError::Unsupported {
                        reason: "build.mode = host but the host cargo toolchain is not available"
                            .to_string(),
                        needed: vec!["cargo".to_string()],
                    })
                }
            }
            BuildModeSetting::Container => {
                if self.container_builder.is_available().await {
                    Ok((&self.container_builder, BuildMode::Container))
                } else {
                    Err(RegisterError::Unsupported {
                        reason: "build.mode = container but the Docker daemon is not reachable"
                            .to_string(),
                        needed: vec!["docker".to_string()],
                    })
                }
            }
            BuildModeSetting::Auto => {
                if self.host_builder.is_available().await {
                    Ok((&self.host_builder, BuildMode::Host))
                } else if self.container_builder.is_available().await {
                    Ok((&self.container_builder, BuildMode::Container))
                } else {
                    Err(RegisterError::Unsupported {
                        reason: "build.mode = auto but neither the host cargo toolchain nor the \
                         Docker daemon is available"
                            .to_string(),
                        needed: vec!["cargo".to_string(), "docker".to_string()],
                    })
                }
            }
        }
    }

    /// Resolves and dispatches a `Source::Dir` registration to the
    /// runtime-appropriate flow (002-python-runtime T029): determines
    /// `Runtime` (explicit `req.runtime`, checked for consistency against
    /// the directory's actual contents, or auto-detected via
    /// `model::detect_runtime` when unset), stamps it onto `req.runtime` so
    /// every downstream record (`Function.runtime`, metrics labels) sees the
    /// resolved value rather than a possibly-`None` caller-supplied one, then
    /// calls `register_source_rust` or `register_source_python`.
    async fn register_source(
        &self,
        mut req: RegisterRequest,
        source_path: PathBuf,
        bin: Option<String>,
    ) -> Result<RegisterAccepted, RegisterError> {
        let runtime = self.resolve_dir_runtime(&req, &source_path)?;
        req.runtime = Some(runtime);
        match runtime {
            Runtime::Rust => self.register_source_rust(req, source_path, bin).await,
            Runtime::Python314 => self.register_source_python(req, source_path).await,
        }
    }

    /// data-model.md's "Validation rules (diff)": an explicit `runtime = rust`
    /// requires `Cargo.toml`, an explicit `runtime = python314` requires
    /// `main.py`; with no explicit `runtime`, `model::detect_runtime` picks
    /// one from whichever of the two is present (ambiguous/neither present
    /// are both rejected there too). Every path yields `RegisterError::
    /// InvalidRuntime` (400 `INVALID_ARGUMENT`) on mismatch, never a build
    /// failure -- this check runs before any build is attempted.
    fn resolve_dir_runtime(
        &self,
        req: &RegisterRequest,
        source_path: &Path,
    ) -> Result<Runtime, RegisterError> {
        match req.runtime {
            Some(Runtime::Rust) => {
                if source_path.join("Cargo.toml").is_file() {
                    Ok(Runtime::Rust)
                } else {
                    Err(RegisterError::InvalidRuntime(format!(
                        "runtime = rust requires a Cargo.toml in {}",
                        source_path.display()
                    )))
                }
            }
            Some(Runtime::Python314) => {
                if source_path.join("main.py").is_file() {
                    Ok(Runtime::Python314)
                } else {
                    Err(RegisterError::InvalidRuntime(format!(
                        "runtime = python314 requires a main.py in {}",
                        source_path.display()
                    )))
                }
            }
            None => detect_runtime(source_path)
                .map_err(|err| RegisterError::InvalidRuntime(err.to_string())),
        }
    }

    /// Picks the `PythonBuilder` for a Python source-mode registration per
    /// `python.mode`, mirroring `select_source_builder`'s shape for the Rust
    /// pipeline. `Container` currently always fails (`container_builder` is
    /// `None` until T034/T035 wire a `ContainerPythonBuilder` in); `Auto`
    /// therefore behaves like `Host` until then.
    async fn select_python_builder(
        &self,
    ) -> Result<(&Arc<dyn PythonBuilder>, BuildMode), RegisterError> {
        match self.python.mode {
            PythonModeSetting::Host => {
                if self.python.host_builder.is_available().await {
                    Ok((&self.python.host_builder, BuildMode::Host))
                } else {
                    Err(RegisterError::Unsupported {
                        reason: "python.mode = host but no usable Python 3.14 interpreter was \
                         found"
                            .to_string(),
                        needed: vec!["python3.14".to_string()],
                    })
                }
            }
            PythonModeSetting::Container => match &self.python.container_builder {
                Some(builder) if builder.is_available().await => {
                    Ok((builder, BuildMode::Container))
                }
                _ => Err(RegisterError::Unsupported {
                    reason: "python.mode = container but no container Python builder is \
                     available"
                        .to_string(),
                    needed: vec!["docker".to_string()],
                }),
            },
            PythonModeSetting::Auto => {
                if self.python.host_builder.is_available().await {
                    Ok((&self.python.host_builder, BuildMode::Host))
                } else if let Some(builder) = &self.python.container_builder
                    && builder.is_available().await
                {
                    Ok((builder, BuildMode::Container))
                } else {
                    Err(RegisterError::Unsupported {
                        reason: "python.mode = auto but no usable Python 3.14 interpreter or \
                         container Python builder is available"
                            .to_string(),
                        needed: vec!["python3.14".to_string(), "docker".to_string()],
                    })
                }
            }
        }
    }

    async fn register_source_python(
        &self,
        req: RegisterRequest,
        source_path: PathBuf,
    ) -> Result<RegisterAccepted, RegisterError> {
        let (builder, build_mode) = self.select_python_builder().await?;

        let existing = self.store.get_function(&req.name)?;
        let revision_number = existing
            .as_ref()
            .and_then(|f| f.current_revision)
            .unwrap_or(0)
            + 1;

        let function = self.build_function_record(&req, existing.as_ref());
        validate::validate_function(&function)?;
        self.store.put_function(&function)?;
        self.report_function_state_gauge();

        if let Trigger::Pubsub { topic } = &function.trigger {
            self.bind_pubsub_trigger(&function.name, topic, function.timeout_secs)
                .await;
        }

        let build_id = uuid::Uuid::new_v4().simple().to_string();
        let artifact_dir = self
            .artifacts_dir
            .join(&req.name)
            .join(revision_number.to_string());
        let log_path = artifact_dir.join("build.log");

        let build = Build {
            id: build_id.clone(),
            function_name: req.name.clone(),
            revision: revision_number,
            mode: build_mode,
            status: BuildStatus::Running,
            log_path: log_path.to_string_lossy().to_string(),
            exit_code: None,
            started_at: function.created_at.max(function.updated_at),
            finished_at: None,
            // Not known until the build finishes (uv vs pip) -- filled in below.
            tool: None,
        };
        self.store.put_build(&build)?;

        let python_bin = if self.python.python_bin.is_empty() {
            None
        } else {
            Some(self.python.python_bin.clone())
        };
        let build_request = PythonBuildRequest {
            function_name: req.name.clone(),
            revision: revision_number,
            source_dir: source_path,
            artifact_dir: artifact_dir.clone(),
            entry_point: function.entry_point.clone(),
            timeout: self.build_timeout,
            cache_root: self.python.cache_root.clone(),
            functions_framework_spec: self.python.functions_framework_spec.clone(),
            installer: self.python.installer,
            python_bin,
            uv_bin: self.python.uv_bin.clone(),
            container_image: self.python.container_image.clone(),
            passthrough_env: self.python.passthrough_env.clone(),
        };

        let build_started = std::time::Instant::now();
        let build_outcome = builder.build(&build_request).await;
        let build_mode_label: &'static str = match build_mode {
            BuildMode::Host => "host",
            BuildMode::Container => "container",
        };
        metrics::histogram!("open_functions_build_duration_seconds", "mode" => build_mode_label)
            .record(build_started.elapsed().as_secs_f64());

        let mut build_record = build;
        build_record.finished_at = Some(chrono::Utc::now());
        let (function_state, last_error, revision_ready, tool) = match &build_outcome {
            Ok(outcome) => {
                metrics::counter!(
                    "open_functions_builds_total",
                    "function" => req.name.clone(), "mode" => build_mode_label, "result" => "ok",
                )
                .increment(1);
                build_record.status = BuildStatus::Succeeded;
                build_record.exit_code = Some(0);
                (FunctionState::Ready, None, true, Some(outcome.tool.clone()))
            }
            Err(err) => {
                metrics::counter!(
                    "open_functions_builds_total",
                    "function" => req.name.clone(), "mode" => build_mode_label, "result" => "fail",
                )
                .increment(1);
                build_record.status = BuildStatus::Failed;
                build_record.exit_code = None;
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
                (state, Some(err.to_string()), false, None)
            }
        };
        build_record.tool = tool;
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
                artifact_path: Some(artifact_dir.to_string_lossy().to_string()),
                image_digest: None,
                build_id: Some(build_id.clone()),
                snapshot,
                build_mode: Some(build_mode),
                container_image: if build_mode == BuildMode::Container {
                    Some(self.python.container_image.clone())
                } else {
                    None
                },
                artifact_pruned: false,
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
            self.report_function_state_gauge();
        }

        Ok(RegisterAccepted {
            revision: revision_number,
            build_id,
        })
    }

    async fn register_source_rust(
        &self,
        req: RegisterRequest,
        source_path: PathBuf,
        bin: Option<String>,
    ) -> Result<RegisterAccepted, RegisterError> {
        let (builder, build_mode) = self.select_source_builder().await?;

        let existing = self.store.get_function(&req.name)?;
        let revision_number = existing
            .as_ref()
            .and_then(|f| f.current_revision)
            .unwrap_or(0)
            + 1;

        let function = self.build_function_record(&req, existing.as_ref());
        validate::validate_function(&function)?;
        self.store.put_function(&function)?;
        self.report_function_state_gauge();

        // FR-010: a Pub/Sub-triggered function's binding is created at
        // registration time, independent of whether the build succeeds —
        // the function isn't reachable yet either way, and open-pubusb's own
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
            mode: build_mode,
            status: BuildStatus::Running,
            log_path: log_path.to_string_lossy().to_string(),
            exit_code: None,
            started_at: function.created_at.max(function.updated_at),
            finished_at: None,
            // This is the Rust source-mode path (register_source_rust); Python's
            // uv/pip tool is recorded by register_source_python instead.
            tool: Some("cargo".to_string()),
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
            cache_dir: self.cache_dir.clone(),
            timeout: self.build_timeout,
        };

        let build_started = std::time::Instant::now();
        let build_outcome = builder.build(&build_request).await;
        let build_mode_label: &'static str = match build_mode {
            BuildMode::Host => "host",
            BuildMode::Container => "container",
        };
        metrics::histogram!("open_functions_build_duration_seconds", "mode" => build_mode_label)
            .record(build_started.elapsed().as_secs_f64());

        let mut build_record = build;
        build_record.finished_at = Some(chrono::Utc::now());
        let (function_state, last_error, revision_ready) = match &build_outcome {
            Ok(()) => {
                metrics::counter!(
                    "open_functions_builds_total",
                    "function" => req.name.clone(), "mode" => build_mode_label, "result" => "ok",
                )
                .increment(1);
                build_record.status = BuildStatus::Succeeded;
                build_record.exit_code = Some(0);
                (FunctionState::Ready, None, true)
            }
            Err(err) => {
                metrics::counter!(
                    "open_functions_builds_total",
                    "function" => req.name.clone(), "mode" => build_mode_label, "result" => "fail",
                )
                .increment(1);
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
                build_mode: Some(build_mode),
                container_image: None,
                artifact_pruned: false,
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
            self.report_function_state_gauge();
        }

        Ok(RegisterAccepted {
            revision: revision_number,
            build_id,
        })
    }

    /// Image-mode registration (US4): no build step — resolves the image's
    /// content digest via the Docker daemon and activates a revision
    /// pointing at it directly. Per US4's Independent Test (deploy a real
    /// `docker build`-produced image and get a working invocation; with the
    /// Docker daemon stopped, an `--image` registration gets 412 while
    /// existing source-mode functions keep working): rejects with
    /// `RegisterError::Unsupported` (412 `FAILED_PRECONDITION`) if the
    /// Docker daemon isn't reachable, independent of `build.mode`
    /// (image-mode never builds, so `build.mode` is irrelevant to it).
    async fn register_image(
        &self,
        req: RegisterRequest,
        image_ref: String,
    ) -> Result<RegisterAccepted, RegisterError> {
        if !self.container_driver.is_available().await {
            return Err(RegisterError::Unsupported {
                reason: "source.kind = image but the Docker daemon is not reachable".to_string(),
                needed: vec!["docker".to_string()],
            });
        }

        let docker = docker_helper::connect(&self.docker_socket).map_err(|err| {
            RegisterError::Unsupported {
                reason: err.to_string(),
                needed: vec!["docker".to_string()],
            }
        })?;
        let digest = resolve_image_digest(&docker, &image_ref)
            .await
            .map_err(|reason| RegisterError::Unsupported {
                reason,
                needed: vec!["docker".to_string()],
            })?;

        let existing = self.store.get_function(&req.name)?;
        let revision_number = existing
            .as_ref()
            .and_then(|f| f.current_revision)
            .unwrap_or(0)
            + 1;

        let mut function = self.build_function_record(&req, existing.as_ref());
        // Image-mode has no build step to wait on: go straight to `ready`.
        function.state = FunctionState::Ready;
        function.current_revision = Some(revision_number);
        validate::validate_function(&function)?;
        self.store.put_function(&function)?;

        if let Trigger::Pubsub { topic } = &function.trigger {
            self.bind_pubsub_trigger(&function.name, topic, function.timeout_secs)
                .await;
        }

        let revision = Revision {
            function_name: req.name.clone(),
            number: revision_number,
            artifact_path: None,
            image_digest: Some(digest),
            build_id: None,
            snapshot: function,
            build_mode: None,
            container_image: None,
            artifact_pruned: false,
            created_at: chrono::Utc::now(),
        };
        self.store.put_revision(&revision)?;
        self.activate_revision(&req.name, &revision).await?;

        Ok(RegisterAccepted {
            revision: revision_number,
            build_id: String::new(),
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
        // Image-mode instances run via `container_driver` and carry
        // `Launch::image` (ignoring `artifact_path`). Source-mode instances
        // branch further on `Function.runtime` × `Revision.build_mode`:
        // Rust (or no declared runtime, a pre-002 record) runs via
        // `process_driver`/`Launch::rust_process`; Python runs via
        // `process_driver`/`Launch::python_host` (host build) or
        // `container_driver`/`Launch::python_container` (container build,
        // T034/T035). `python_instance_env`'s defaults are layered under
        // the function's own declared `env` (last write wins, matching that
        // function's own doc comment) -- Rust/image-mode instances get no
        // such defaults, just `f.env` as-is, unchanged from before 002.
        let (driver, launch, env): (
            &Arc<dyn Driver>,
            Launch,
            std::collections::BTreeMap<String, String>,
        ) = match (&f.source, f.runtime) {
            (Source::Image { image_ref }, _) => (
                &self.container_driver,
                Launch::image(image_ref.clone()),
                f.env.clone(),
            ),
            (Source::Dir { .. }, Some(Runtime::Python314)) => match revision.build_mode {
                Some(BuildMode::Container) => {
                    let image = revision
                        .container_image
                        .clone()
                        .unwrap_or_else(|| self.python.container_image.clone());
                    let venv_dir = PathBuf::from(CONTAINER_ARTIFACT_DIR).join("venv");
                    let mut env = python_instance_env(f, PythonLaunchMode::Container, &venv_dir);
                    env.extend(f.env.clone());
                    (
                        &self.container_driver,
                        Launch::python_container(&artifact_path, image),
                        env,
                    )
                }
                _ => {
                    let venv_dir = artifact_path.join("venv");
                    let mut env = python_instance_env(f, PythonLaunchMode::Process, &venv_dir);
                    env.extend(f.env.clone());
                    (
                        &self.process_driver,
                        Launch::python_host(&artifact_path),
                        env,
                    )
                }
            },
            (Source::Dir { .. }, _) => (
                &self.process_driver,
                Launch::rust_process(artifact_path),
                f.env.clone(),
            ),
        };

        let signature_type: &'static str = match &f.trigger {
            Trigger::Http => "http",
            Trigger::Pubsub { .. } => "cloudevent",
        };

        let spec = InstanceSpec {
            function_name: name.to_string(),
            revision: revision.number,
            entry_point: f.entry_point.clone(),
            signature_type,
            env,
            memory_mib: f.memory_mib,
            start_timeout: Duration::from_secs(10),
            launch,
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
                    Arc::clone(driver),
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
            self.report_function_state_gauge();
        }

        Ok(())
    }

    /// Creates or fixes the open-pubusb Push subscription for a Pub/Sub-triggered
    /// function, per FR-010/FR-013. A no-op (with a one-time `warn!`) if
    /// Pub/Sub support is disabled (`pubsub.enabled = false`). Failures
    /// (including open-pubusb being unreachable) are not propagated as
    /// `RegisterError` — `Reconciler::try_bind` already persists a `pending`
    /// `TriggerBinding` for the retry sweep to pick up, so a temporarily
    /// down open-pubusb must not fail the whole registration.
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

    /// Removes the open-pubusb Push subscription for a function being deleted, if
    /// it had a Pub/Sub trigger and Pub/Sub support is enabled.
    async fn unbind_pubsub_trigger(&self, function_name: &str) {
        let Some(pubsub) = &self.pubsub else {
            return;
        };
        if let Err(err) = pubsub.reconciler.try_unbind(function_name).await {
            tracing::warn!(function = %function_name, %err, "failed to persist pubsub unbind outcome");
        }
    }

    /// Deletes a function (T080/US5), per `admin-api.md`'s `DELETE` contract
    /// (`deleting` state -> instances stopped -> binding unbound -> artifacts
    /// removed -> registry entry removed). Idempotent-ish: deleting an
    /// unknown function is `DeleteError::NotFound`, matching `admin-api.md`'s
    /// 404. Runs synchronously to completion (matching `register`'s own
    /// synchronous-under-the-hood MVP shape, see this module's top doc
    /// comment) rather than returning immediately and finishing in the
    /// background -- `admin.rs`'s `202 {"state":"deleting"}` response is
    /// therefore sent only once teardown has actually finished, and a `GET`
    /// racing a concurrent `DELETE` can observe `state: deleting` only for
    /// the (typically sub-second) window between persisting that state below
    /// and the final `store.delete_function` a few lines later.
    pub async fn delete(&self, name: &str) -> Result<(), DeleteError> {
        let Some(mut existing) = self.store.get_function(name)? else {
            return Err(DeleteError::NotFound(name.to_string()));
        };

        existing.state = FunctionState::Deleting;
        existing.updated_at = chrono::Utc::now();
        self.store.put_function(&existing)?;
        self.report_function_state_gauge();

        if let Some(handle) = self.reapers.lock().await.remove(name) {
            handle.abort();
        }
        if let Some(pool) = self.pools.lock().await.remove(name) {
            pool.begin_drain().await;
            // Full stop (not just a reap of already-idle instances): wait
            // for every instance to actually exit (SIGTERM -> grace ->
            // SIGKILL, `InstancePool::stop_all`'s own semantics) before
            // continuing, so a completed `delete()` really has scaled the
            // function to zero, per admin-api.md's "stop instances" step.
            pool.stop_all(Duration::from_secs(5)).await;
        }

        if matches!(existing.trigger, Trigger::Pubsub { .. }) {
            self.unbind_pubsub_trigger(name).await;
        }

        self.log_store.remove(name);
        self.store.delete_function(name)?;
        self.report_function_state_gauge();

        let function_artifacts_dir = self.artifacts_dir.join(name);
        let _ = tokio::fs::remove_dir_all(&function_artifacts_dir).await;
        let function_build_dir = self.build_dir.join(name);
        let _ = tokio::fs::remove_dir_all(&function_build_dir).await;

        Ok(())
    }

    /// Stops every running instance of `name` (`POST .../:stop`, T081/US5) --
    /// a forced scale-to-zero, distinct from [`RegistryService::delete`]:
    /// the function itself, its bindings, and its artifacts are untouched,
    /// and a subsequent invocation starts a fresh instance on demand exactly
    /// as it would after an idle-reaper stop.
    pub async fn stop_instances(&self, name: &str) -> Result<(), StopError> {
        if self.store.get_function(name)?.is_none() {
            return Err(StopError::NotFound(name.to_string()));
        }
        if let Some(pool) = self.pools.lock().await.get(name).cloned() {
            pool.stop_all(Duration::from_secs(5)).await;
        }
        Ok(())
    }

    /// Refreshes the `open_functions_functions{state}` gauge (T082/US5) from the
    /// store's current contents. Called after every state-changing
    /// operation (`register_source_rust`, `register_image`, `activate_revision`,
    /// `delete`) rather than incrementally tracked, since the number of
    /// functions is always small enough that a full re-list is cheap and
    /// this avoids the gauge ever drifting out of sync with reality.
    fn report_function_state_gauge(&self) {
        let functions = match self.store.list_functions() {
            Ok(functions) => functions,
            Err(err) => {
                tracing::warn!(%err, "failed to list functions for open_functions_functions gauge");
                return;
            }
        };
        let mut counts = [0u64; 4];
        for f in &functions {
            let idx = match f.state {
                FunctionState::Building => 0,
                FunctionState::Ready => 1,
                FunctionState::Failed => 2,
                FunctionState::Deleting => 3,
            };
            counts[idx] += 1;
        }
        metrics::gauge!("open_functions_functions", "state" => "building").set(counts[0] as f64);
        metrics::gauge!("open_functions_functions", "state" => "ready").set(counts[1] as f64);
        metrics::gauge!("open_functions_functions", "state" => "failed").set(counts[2] as f64);
        metrics::gauge!("open_functions_functions", "state" => "deleting").set(counts[3] as f64);
    }
}

/// Resolves `image_ref`'s content digest (`ImageInspect.id`, e.g.
/// `sha256:...`) for `Revision.image_digest`, per plan.md's "digest resolution" step of the image-mode register flow. Pulls the
/// image first if it isn't already present locally (`bollard::Docker::
/// inspect_image` 404), draining the pull stream to completion — the same
/// "pull if missing" logic `ContainerDriver::spawn` has internally
/// (duplicated here rather than shared, to avoid depending on a private
/// helper of a sibling module for a two-call sequence).
async fn resolve_image_digest(docker: &bollard::Docker, image_ref: &str) -> Result<String, String> {
    use bollard::query_parameters::CreateImageOptionsBuilder;

    let already_present = docker.inspect_image(image_ref).await;
    if let Err(bollard::errors::Error::DockerResponseServerError {
        status_code: 404, ..
    }) = already_present
    {
        let options = CreateImageOptionsBuilder::default()
            .from_image(image_ref)
            .build();
        let mut pull_stream = docker.create_image(Some(options), None, None);
        use futures_util::StreamExt;
        while let Some(item) = pull_stream.next().await {
            item.map_err(|err| format!("failed to pull image {image_ref:?}: {err}"))?;
        }
    } else if let Err(err) = already_present {
        return Err(format!("failed to inspect image {image_ref:?}: {err}"));
    }

    let inspected = docker
        .inspect_image(image_ref)
        .await
        .map_err(|err| format!("failed to inspect image {image_ref:?} after pull: {err}"))?;
    inspected
        .id
        .ok_or_else(|| format!("image {image_ref:?} has no content digest after inspect"))
}
