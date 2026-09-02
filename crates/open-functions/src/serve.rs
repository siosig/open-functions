//! Top-level `serve` orchestration (T022, extended by T040/T041/T042/T060/T061):
//! tracing/metrics → storage open → construct the `RegistryService` (build +
//! runtime driver + instance pools) → bind both listeners → startup restore
//! → `READY=1` → run until a shutdown signal → graceful stop → exit.

use std::process::ExitCode;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::Duration;

use open_functions_core::build::container::ContainerBuilder;
use open_functions_core::build::host_cargo::HostCargoBuilder;
use open_functions_core::logs::ring::LogStore;
use open_functions_core::model::function::QueuePolicy as ModelQueuePolicy;
use open_functions_core::pubsub::client::PsRsClient;
use open_functions_core::pubsub::reconcile::Reconciler;
use open_functions_core::registry::redb_store::RedbStore;
use open_functions_core::registry::service::{
    BuildModeSetting, PubsubBindingConfig, RegistrationDefaults, RegistryService,
};
use open_functions_core::resolve::Resolver;
use open_functions_core::runtime::cgroup::CgroupLimiter;
use open_functions_core::runtime::container::{self, ContainerDriver};
use open_functions_core::runtime::docker;
use open_functions_core::runtime::process::ProcessDriver;
use tokio::sync::Semaphore;

use crate::config::AppConfig;
use crate::forward::Forwarder;
use crate::ops;
use crate::server::{admin, invoke};

const EXIT_OK: u8 = 0;
const EXIT_RUNTIME_ERROR: u8 = 1;
const EXIT_CONFIG_ERROR: u8 = 2;
const EXIT_BIND_ERROR: u8 = 3;

pub async fn run(cfg: AppConfig) -> ExitCode {
    if let Err(err) = ops::init_tracing(&cfg.log) {
        eprintln!("error: failed to initialize logging: {err}");
        return ExitCode::from(EXIT_RUNTIME_ERROR);
    }

    let metrics_handle = match ops::init_metrics() {
        Ok(handle) => handle,
        Err(err) => {
            tracing::error!(%err, "failed to initialize metrics recorder");
            return ExitCode::from(EXIT_RUNTIME_ERROR);
        }
    };
    // `open_functions_build_info` (T082/US5): always 1, per ops-config.md. `git_sha` is
    // "unknown" until a build.rs embeds it (not yet wired, see
    // `cli::print_version`'s matching note); `version` is always accurate.
    metrics::gauge!(
        "open_functions_build_info",
        "version" => env!("CARGO_PKG_VERSION"),
        "git_sha" => "unknown",
    )
    .set(1.0);

    if let Err(err) = std::fs::create_dir_all(&cfg.storage.data_dir) {
        tracing::error!(data_dir = %cfg.storage.data_dir, %err, "failed to create storage.data_dir");
        return ExitCode::from(EXIT_CONFIG_ERROR);
    }
    let data_dir = std::path::PathBuf::from(&cfg.storage.data_dir);
    let db_path = data_dir.join("meta.redb");
    let store = match RedbStore::open(&db_path) {
        Ok(store) => Arc::new(store),
        Err(err) => {
            tracing::error!(%err, "failed to open storage at {}", db_path.display());
            return ExitCode::from(EXIT_CONFIG_ERROR);
        }
    };

    let queue_policy = match cfg.defaults.queue_policy.as_str() {
        "reject" => ModelQueuePolicy::Reject,
        _ => ModelQueuePolicy::Wait,
    };
    let defaults = RegistrationDefaults {
        timeout_secs: cfg.defaults.timeout_secs,
        concurrency: cfg.defaults.concurrency,
        memory_mib: cfg.defaults.memory_mib,
        min_instances: cfg.defaults.min_instances,
        max_instances: cfg.defaults.max_instances,
        idle_timeout_secs: cfg.defaults.idle_timeout_secs,
        queue_policy,
        queue_max_wait_secs: cfg.defaults.queue_max_wait_secs,
    };

    // Per-function log ring buffers (T079/US5), shared by both drivers (each
    // drains its own instances' stdout/stderr into it) and by `RegistryService`
    // (so `delete()` can forget a function's buffer, and `admin.rs`'s
    // `GET .../logs` can read it via `RegistryService::log_buffer`).
    let log_store = Arc::new(LogStore::new(cfg.log.function_ring_buffer_lines as usize));

    let host_builder = Arc::new(HostCargoBuilder {
        cargo_bin: cfg.build.cargo_bin.clone(),
    });
    let container_builder = Arc::new(ContainerBuilder {
        docker_socket: cfg.runtime.docker_socket.clone(),
    });
    let limiter = Arc::new(if cfg.runtime.cgroup == "off" {
        CgroupLimiter::disabled()
    } else {
        CgroupLimiter::probe()
    });
    let process_driver = Arc::new(ProcessDriver {
        limiter,
        log_store: Arc::clone(&log_store),
    });
    // `docker::connect` only builds a client (cheap, infallible for a
    // well-formed socket path) -- it does not itself require a reachable
    // daemon, so this is safe to construct unconditionally even when
    // `build.mode = host` and/or the daemon never ends up running.
    let container_driver = match docker::connect(&cfg.runtime.docker_socket) {
        Ok(client) => Arc::new(ContainerDriver {
            docker: client,
            log_store: Arc::clone(&log_store),
        }),
        Err(err) => {
            tracing::error!(%err, "failed to construct the Docker client for image-mode support");
            return ExitCode::from(EXIT_CONFIG_ERROR);
        }
    };
    // Startup sweep of containers open-functions created and left behind (per
    // plan.md's ContainerDriver design note) -- best-effort: an unreachable
    // daemon here just means there's nothing to sweep, not a startup
    // failure (image-mode functions may simply be unused / build.mode may
    // not be container/auto).
    match container::sweep_stale_containers(&container_driver.docker).await {
        Ok(removed) if removed > 0 => {
            tracing::info!(
                removed,
                "swept stale containers left by an unclean prior shutdown"
            );
        }
        Ok(_) => {}
        Err(err) => {
            tracing::warn!(%err, "failed to sweep stale containers at startup (continuing)");
        }
    }
    let build_mode = match cfg.build.mode.as_str() {
        "host" => BuildModeSetting::Host,
        "container" => BuildModeSetting::Container,
        // `config::validate()` already rejects anything but
        // "auto"/"host"/"container" before `run` is ever reached; treat an
        // unexpected value the same as "auto" rather than panicking.
        _ => BuildModeSetting::Auto,
    };
    let global_limit = Arc::new(Semaphore::new(cfg.runtime.max_total_instances as usize));

    let invoke_base_url = if cfg.invoke.public_base_url.is_empty() {
        format!("http://{}", display_addr(&cfg.invoke.listen))
    } else {
        cfg.invoke.public_base_url.clone()
    };

    let pubsub_binding = if cfg.pubsub.enabled {
        let client = PsRsClient::new(
            cfg.pubsub.base_url.clone(),
            cfg.pubsub.project.clone(),
            Duration::from_secs(u64::from(cfg.pubsub.request_timeout_secs)),
        );
        let reconciler = Arc::new(Reconciler::new(
            client,
            Arc::clone(&store) as Arc<dyn open_functions_core::registry::store::Store>,
            Duration::from_secs(u64::from(cfg.pubsub.retry_initial_secs)),
            Duration::from_secs(u64::from(cfg.pubsub.retry_max_secs)),
        ));
        let push_base_url = if cfg.pubsub.push_base_url.is_empty() {
            invoke_base_url.clone()
        } else {
            cfg.pubsub.push_base_url.clone()
        };
        // Background retry sweep for bindings ps-rs was unreachable for
        // (or that failed to unbind) at the time they were first attempted.
        let _reaper = Arc::clone(&reconciler).spawn_retry_loop(
            Duration::from_secs(5),
            cfg.pubsub.ack_deadline_max_secs,
            cfg.pubsub.project.clone(),
        );
        Some(PubsubBindingConfig {
            reconciler,
            project: cfg.pubsub.project.clone(),
            push_base_url,
            ack_deadline_max_secs: cfg.pubsub.ack_deadline_max_secs,
        })
    } else {
        None
    };

    let registry = Arc::new(RegistryService::new(
        store,
        host_builder,
        container_builder,
        process_driver,
        container_driver,
        build_mode,
        cfg.runtime.docker_socket.clone(),
        global_limit,
        &data_dir,
        Duration::from_secs(u64::from(cfg.build.timeout_secs)),
        defaults,
        pubsub_binding,
        Arc::clone(&log_store),
    ));

    let host_suffix = if cfg.invoke.host_suffix.is_empty() {
        None
    } else {
        Some(cfg.invoke.host_suffix.clone())
    };
    let resolver = Arc::new(Resolver {
        host_suffix: host_suffix.clone(),
    });
    let ready = Arc::new(AtomicBool::new(false));

    let invoke_listener = match tokio::net::TcpListener::bind(&cfg.invoke.listen).await {
        Ok(listener) => listener,
        Err(err) => {
            tracing::error!(addr = %cfg.invoke.listen, %err, "failed to bind invoke listener");
            return ExitCode::from(EXIT_BIND_ERROR);
        }
    };
    let admin_listener = match tokio::net::TcpListener::bind(&cfg.admin.listen).await {
        Ok(listener) => listener,
        Err(err) => {
            tracing::error!(addr = %cfg.admin.listen, %err, "failed to bind admin listener");
            return ExitCode::from(EXIT_BIND_ERROR);
        }
    };

    let admin_token = if cfg.admin.token.is_empty() {
        None
    } else {
        Some(cfg.admin.token.clone())
    };
    let admin_state = admin::AdminState {
        token: admin_token,
        metrics_enabled: cfg.metrics.enabled,
        metrics_require_token: cfg.admin.metrics_require_token,
        metrics_handle: Arc::new(metrics_handle),
        ready: ready.clone(),
        registry: Arc::clone(&registry),
        invoke_base_url,
        host_suffix,
    };
    let pubsub_project = if cfg.pubsub.enabled {
        Some(cfg.pubsub.project.clone())
    } else {
        None
    };
    let invoke_state = invoke::InvokeState {
        resolver,
        registry: Arc::clone(&registry),
        forwarder: Arc::new(Forwarder::new()),
        pubsub_project,
    };

    let admin_router = admin::router(admin_state);
    let invoke_router =
        invoke::router(invoke_state).into_make_service_with_connect_info::<std::net::SocketAddr>();

    // Startup restore (T060): both listeners are already bound above, and
    // this must finish before `READY=1` per ops-config.md's systemd
    // integration contract ("after both listeners bind + redb open +
    // existing function metadata restore completes"). redb itself was
    // already opened when `RegistryService` was constructed.
    match registry.restore().await {
        Ok(report) => {
            tracing::info!(
                functions_restored = report.functions_restored,
                builds_marked_interrupted = report.builds_marked_interrupted,
                broken_functions = ?report.broken_functions,
                warm_start_failures = ?report.warm_start_failures,
                "startup restore complete"
            );
        }
        Err(err) => {
            tracing::error!(%err, "startup restore failed");
            return ExitCode::from(EXIT_RUNTIME_ERROR);
        }
    }

    let shutdown = ops::Shutdown::new();
    let mut invoke_shutdown_rx = shutdown.subscribe();
    let mut admin_shutdown_rx = shutdown.subscribe();

    let invoke_server =
        axum::serve(invoke_listener, invoke_router).with_graceful_shutdown(async move {
            let _ = invoke_shutdown_rx.changed().await;
        });
    let admin_server =
        axum::serve(admin_listener, admin_router).with_graceful_shutdown(async move {
            let _ = admin_shutdown_rx.changed().await;
        });

    // Spawned (not just constructed) so both listeners are actively serving
    // from this point on, independent of whatever this function awaits next
    // (the shutdown signal, below) — `axum::serve`'s future does nothing
    // until polled.
    let invoke_task = tokio::spawn(async move { invoke_server.await });
    let admin_task = tokio::spawn(async move { admin_server.await });

    ready.store(true, std::sync::atomic::Ordering::SeqCst);
    ops::notify_ready();
    tracing::info!(
        invoke = %cfg.invoke.listen,
        admin = %cfg.admin.listen,
        "open-functions serving"
    );

    // Graceful shutdown sequence (T061), per ops-config.md's signal table:
    // STOPPING=1 -> invoke accept stops (already wired into the two
    // `with_graceful_shutdown` futures above via the watch channel) ->
    // in-flight requests get up to `shutdown_grace_secs` to finish -> every
    // running function instance gets SIGTERM, up to `stop_grace_secs`, then
    // SIGKILL -> process exits 0. `STOPPING=1` fires the moment the signal
    // arrives, before anything else, so the service manager and any external
    // monitoring see shutdown begin immediately rather than only once it has
    // already finished.
    shutdown.wait_for_signal().await;
    ops::notify_stopping();
    tracing::info!("received shutdown signal; draining in-flight requests");

    let shutdown_grace = Duration::from_secs(u64::from(cfg.invoke.shutdown_grace_secs));
    match tokio::time::timeout(shutdown_grace, async {
        tokio::join!(invoke_task, admin_task)
    })
    .await
    {
        Ok((invoke_result, admin_result)) => {
            match invoke_result {
                Ok(Err(err)) => tracing::error!(%err, "invoke listener terminated with an error"),
                Err(err) => tracing::error!(%err, "invoke listener task panicked"),
                Ok(Ok(())) => {}
            }
            match admin_result {
                Ok(Err(err)) => tracing::error!(%err, "admin listener terminated with an error"),
                Err(err) => tracing::error!(%err, "admin listener task panicked"),
                Ok(Ok(())) => {}
            }
        }
        Err(_) => {
            tracing::warn!(
                grace_secs = cfg.invoke.shutdown_grace_secs,
                "shutdown_grace_secs exceeded with requests still in flight; forcing stop"
            );
        }
    }

    let stop_grace = Duration::from_secs(u64::from(cfg.runtime.stop_grace_secs));
    registry.shutdown_all_instances(stop_grace).await;

    ExitCode::from(EXIT_OK)
}

fn display_addr(listen: &str) -> String {
    // "0.0.0.0:8080" isn't dialable by clients as-is; substitute loopback for
    // display purposes only (the actual bind address is unchanged).
    if let Some((host, port)) = listen.rsplit_once(':')
        && (host == "0.0.0.0" || host.is_empty())
    {
        return format!("127.0.0.1:{port}");
    }
    listen.to_string()
}
