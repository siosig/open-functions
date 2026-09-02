//! Top-level `serve` orchestration (T022, extended by T040/T041/T042):
//! tracing/metrics → storage open → construct the `RegistryService` (build +
//! runtime driver + instance pools) → bind both listeners → `READY=1` → run
//! until a shutdown signal → graceful stop → exit.

use std::process::ExitCode;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::Duration;

use cf_rs_core::build::host_cargo::HostCargoBuilder;
use cf_rs_core::model::function::QueuePolicy as ModelQueuePolicy;
use cf_rs_core::pubsub::client::PsRsClient;
use cf_rs_core::pubsub::reconcile::Reconciler;
use cf_rs_core::registry::redb_store::RedbStore;
use cf_rs_core::registry::service::{PubsubBindingConfig, RegistrationDefaults, RegistryService};
use cf_rs_core::resolve::Resolver;
use cf_rs_core::runtime::cgroup::CgroupLimiter;
use cf_rs_core::runtime::process::ProcessDriver;
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

    let builder = Arc::new(HostCargoBuilder {
        cargo_bin: cfg.build.cargo_bin.clone(),
    });
    let limiter = Arc::new(if cfg.runtime.cgroup == "off" {
        CgroupLimiter::disabled()
    } else {
        CgroupLimiter::probe()
    });
    let driver = Arc::new(ProcessDriver { limiter });
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
            Arc::clone(&store) as Arc<dyn cf_rs_core::registry::store::Store>,
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
        builder,
        driver,
        global_limit,
        &data_dir,
        Duration::from_secs(u64::from(cfg.build.timeout_secs)),
        defaults,
        pubsub_binding,
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

    ready.store(true, std::sync::atomic::Ordering::SeqCst);
    ops::notify_ready();
    tracing::info!(
        invoke = %cfg.invoke.listen,
        admin = %cfg.admin.listen,
        "cf-rs serving"
    );

    let signal_task = tokio::spawn(async move {
        shutdown.wait_for_signal().await;
    });

    let (invoke_result, admin_result) = tokio::join!(invoke_server, admin_server);
    ops::notify_stopping();
    let _ = signal_task.await;

    if let Err(err) = invoke_result {
        tracing::error!(%err, "invoke listener terminated with an error");
        return ExitCode::from(EXIT_RUNTIME_ERROR);
    }
    if let Err(err) = admin_result {
        tracing::error!(%err, "admin listener terminated with an error");
        return ExitCode::from(EXIT_RUNTIME_ERROR);
    }

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
