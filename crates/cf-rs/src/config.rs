//! Configuration schema and loader for `cf-rs`.
//!
//! Layering order (per `specs/001-cloud-functions-local/contracts/ops-config.md`):
//! built-in defaults < config file < `CF_RS__`-prefixed environment variables.
//!
//! CLI flag overrides are **not** applied by [`load`] — a later task is responsible
//! for merging parsed CLI flags on top of the [`AppConfig`] returned here.
//!
//! `main.rs` does not yet wire this module into `serve`/`check-config` (that is a
//! later task), so several fields below are not read anywhere yet. They still need
//! to exist now to match the full TOML schema in `ops-config.md`, so `dead_code` is
//! silenced at module scope rather than field-by-field.

#![allow(dead_code)]

use std::path::Path;

/// Top-level application configuration, matching the TOML schema in
/// `contracts/ops-config.md`.
#[derive(Debug, Clone, Default, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AppConfig {
    #[serde(default)]
    pub invoke: InvokeConfig,
    #[serde(default)]
    pub admin: AdminConfig,
    #[serde(default)]
    pub storage: StorageConfig,
    #[serde(default)]
    pub build: BuildConfig,
    #[serde(default)]
    pub runtime: RuntimeConfig,
    #[serde(default)]
    pub defaults: DefaultsConfig,
    #[serde(default)]
    pub pubsub: PubsubConfig,
    #[serde(default)]
    pub log: LogConfig,
    #[serde(default)]
    pub metrics: MetricsConfig,
}

/// `[invoke]` section: function invocation / Push receiving listener.
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InvokeConfig {
    #[serde(default = "InvokeConfig::default_listen")]
    pub listen: String,
    #[serde(default)]
    pub public_base_url: String,
    #[serde(default)]
    pub host_suffix: String,
    #[serde(default = "InvokeConfig::default_shutdown_grace_secs")]
    pub shutdown_grace_secs: u32,
}

impl InvokeConfig {
    fn default_listen() -> String {
        "0.0.0.0:8080".to_string()
    }
    fn default_shutdown_grace_secs() -> u32 {
        30
    }
}

impl Default for InvokeConfig {
    fn default() -> Self {
        Self {
            listen: Self::default_listen(),
            public_base_url: String::new(),
            host_suffix: String::new(),
            shutdown_grace_secs: Self::default_shutdown_grace_secs(),
        }
    }
}

/// `[admin]` section: admin API listener and auth.
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AdminConfig {
    #[serde(default = "AdminConfig::default_listen")]
    pub listen: String,
    #[serde(default)]
    pub token: String,
    #[serde(default)]
    pub token_file: String,
    #[serde(default)]
    pub metrics_require_token: bool,
}

impl AdminConfig {
    fn default_listen() -> String {
        "127.0.0.1:8081".to_string()
    }
}

impl Default for AdminConfig {
    fn default() -> Self {
        Self {
            listen: Self::default_listen(),
            token: String::new(),
            token_file: String::new(),
            metrics_require_token: false,
        }
    }
}

/// `[storage]` section.
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StorageConfig {
    #[serde(default = "StorageConfig::default_data_dir")]
    pub data_dir: String,
}

impl StorageConfig {
    fn default_data_dir() -> String {
        "/var/lib/cf-rs".to_string()
    }
}

impl Default for StorageConfig {
    fn default() -> Self {
        Self {
            data_dir: Self::default_data_dir(),
        }
    }
}

/// `[build]` section.
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BuildConfig {
    /// One of `"auto" | "host" | "container"`. Kept as `String`; validated in `validate()`.
    #[serde(default = "BuildConfig::default_mode")]
    pub mode: String,
    #[serde(default = "BuildConfig::default_cargo_bin")]
    pub cargo_bin: String,
    #[serde(default = "BuildConfig::default_container_image")]
    pub container_image: String,
    #[serde(default = "BuildConfig::default_max_parallel")]
    pub max_parallel: u32,
    #[serde(default = "BuildConfig::default_timeout_secs")]
    pub timeout_secs: u32,
}

impl BuildConfig {
    fn default_mode() -> String {
        "auto".to_string()
    }
    fn default_cargo_bin() -> String {
        "cargo".to_string()
    }
    fn default_container_image() -> String {
        "rust:1-bookworm".to_string()
    }
    fn default_max_parallel() -> u32 {
        2
    }
    fn default_timeout_secs() -> u32 {
        1800
    }
}

impl Default for BuildConfig {
    fn default() -> Self {
        Self {
            mode: Self::default_mode(),
            cargo_bin: Self::default_cargo_bin(),
            container_image: Self::default_container_image(),
            max_parallel: Self::default_max_parallel(),
            timeout_secs: Self::default_timeout_secs(),
        }
    }
}

/// `[runtime]` section.
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeConfig {
    #[serde(default = "RuntimeConfig::default_max_total_instances")]
    pub max_total_instances: u32,
    #[serde(default = "RuntimeConfig::default_start_timeout_secs")]
    pub start_timeout_secs: u32,
    #[serde(default = "RuntimeConfig::default_stop_grace_secs")]
    pub stop_grace_secs: u32,
    /// One of `"auto" | "off"`. Kept as `String`; validated in `validate()`.
    #[serde(default = "RuntimeConfig::default_cgroup")]
    pub cgroup: String,
    #[serde(default)]
    pub docker_socket: String,
    #[serde(default = "RuntimeConfig::default_docker_network")]
    pub docker_network: String,
}

impl RuntimeConfig {
    fn default_max_total_instances() -> u32 {
        32
    }
    fn default_start_timeout_secs() -> u32 {
        10
    }
    fn default_stop_grace_secs() -> u32 {
        5
    }
    fn default_cgroup() -> String {
        "auto".to_string()
    }
    fn default_docker_network() -> String {
        "cf-rs".to_string()
    }
}

impl Default for RuntimeConfig {
    fn default() -> Self {
        Self {
            max_total_instances: Self::default_max_total_instances(),
            start_timeout_secs: Self::default_start_timeout_secs(),
            stop_grace_secs: Self::default_stop_grace_secs(),
            cgroup: Self::default_cgroup(),
            docker_socket: String::new(),
            docker_network: Self::default_docker_network(),
        }
    }
}

/// `[defaults]` section: default values applied at function registration time.
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DefaultsConfig {
    #[serde(default = "DefaultsConfig::default_timeout_secs")]
    pub timeout_secs: u32,
    #[serde(default = "DefaultsConfig::default_concurrency")]
    pub concurrency: u32,
    #[serde(default = "DefaultsConfig::default_memory_mib")]
    pub memory_mib: u32,
    #[serde(default)]
    pub min_instances: u32,
    #[serde(default = "DefaultsConfig::default_max_instances")]
    pub max_instances: u32,
    #[serde(default = "DefaultsConfig::default_idle_timeout_secs")]
    pub idle_timeout_secs: u32,
    /// One of `"wait" | "reject"`. Kept as `String`; validated in `validate()`.
    #[serde(default = "DefaultsConfig::default_queue_policy")]
    pub queue_policy: String,
    #[serde(default = "DefaultsConfig::default_queue_max_wait_secs")]
    pub queue_max_wait_secs: u32,
}

impl DefaultsConfig {
    fn default_timeout_secs() -> u32 {
        60
    }
    fn default_concurrency() -> u32 {
        1
    }
    fn default_memory_mib() -> u32 {
        256
    }
    fn default_max_instances() -> u32 {
        100
    }
    fn default_idle_timeout_secs() -> u32 {
        900
    }
    fn default_queue_policy() -> String {
        "wait".to_string()
    }
    fn default_queue_max_wait_secs() -> u32 {
        30
    }
}

impl Default for DefaultsConfig {
    fn default() -> Self {
        Self {
            timeout_secs: Self::default_timeout_secs(),
            concurrency: Self::default_concurrency(),
            memory_mib: Self::default_memory_mib(),
            min_instances: 0,
            max_instances: Self::default_max_instances(),
            idle_timeout_secs: Self::default_idle_timeout_secs(),
            queue_policy: Self::default_queue_policy(),
            queue_max_wait_secs: Self::default_queue_max_wait_secs(),
        }
    }
}

/// `[pubsub]` section.
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PubsubConfig {
    #[serde(default = "PubsubConfig::default_enabled")]
    pub enabled: bool,
    #[serde(default = "PubsubConfig::default_base_url")]
    pub base_url: String,
    #[serde(default = "PubsubConfig::default_project")]
    pub project: String,
    #[serde(default)]
    pub push_base_url: String,
    #[serde(default = "PubsubConfig::default_ack_deadline_max_secs")]
    pub ack_deadline_max_secs: u32,
    #[serde(default = "PubsubConfig::default_retry_initial_secs")]
    pub retry_initial_secs: u32,
    #[serde(default = "PubsubConfig::default_retry_max_secs")]
    pub retry_max_secs: u32,
    #[serde(default = "PubsubConfig::default_request_timeout_secs")]
    pub request_timeout_secs: u32,
}

impl PubsubConfig {
    fn default_enabled() -> bool {
        true
    }
    fn default_base_url() -> String {
        "http://127.0.0.1:8085".to_string()
    }
    fn default_project() -> String {
        "local".to_string()
    }
    fn default_ack_deadline_max_secs() -> u32 {
        600
    }
    fn default_retry_initial_secs() -> u32 {
        5
    }
    fn default_retry_max_secs() -> u32 {
        60
    }
    fn default_request_timeout_secs() -> u32 {
        10
    }
}

impl Default for PubsubConfig {
    fn default() -> Self {
        Self {
            enabled: Self::default_enabled(),
            base_url: Self::default_base_url(),
            project: Self::default_project(),
            push_base_url: String::new(),
            ack_deadline_max_secs: Self::default_ack_deadline_max_secs(),
            retry_initial_secs: Self::default_retry_initial_secs(),
            retry_max_secs: Self::default_retry_max_secs(),
            request_timeout_secs: Self::default_request_timeout_secs(),
        }
    }
}

/// `[log]` section.
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LogConfig {
    /// One of `"json" | "text" | "journald"`. Kept as `String`; validated in `validate()`.
    #[serde(default = "LogConfig::default_format")]
    pub format: String,
    #[serde(default = "LogConfig::default_level")]
    pub level: String,
    #[serde(default = "LogConfig::default_function_ring_buffer_lines")]
    pub function_ring_buffer_lines: u32,
}

impl LogConfig {
    fn default_format() -> String {
        "json".to_string()
    }
    fn default_level() -> String {
        "info".to_string()
    }
    fn default_function_ring_buffer_lines() -> u32 {
        1000
    }
}

impl Default for LogConfig {
    fn default() -> Self {
        Self {
            format: Self::default_format(),
            level: Self::default_level(),
            function_ring_buffer_lines: Self::default_function_ring_buffer_lines(),
        }
    }
}

/// `[metrics]` section.
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MetricsConfig {
    #[serde(default = "MetricsConfig::default_enabled")]
    pub enabled: bool,
}

impl MetricsConfig {
    fn default_enabled() -> bool {
        true
    }
}

impl Default for MetricsConfig {
    fn default() -> Self {
        Self {
            enabled: Self::default_enabled(),
        }
    }
}

/// Errors produced while loading or validating configuration.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("failed to load configuration: {0}")]
    Load(String),
    #[error("invalid configuration at {field}: {reason}")]
    Invalid { field: &'static str, reason: String },
}

/// Loads [`AppConfig`] by layering, in increasing precedence:
///
/// 1. built-in defaults (via `#[serde(default)]`, applied when no file/env key is present)
/// 2. an optional TOML config file
/// 3. `CF_RS__`-prefixed environment variables (double underscore as section separator)
///
/// CLI flags are **not** applied here; a later stage merges parsed CLI flags on top of the
/// value returned by this function.
///
/// The config file path is resolved as follows:
/// - `config_path`, if `Some`, is used and must exist/parse (errors become [`ConfigError::Load`]).
/// - else `CF_RS_CONFIG` env var, if set, is used and must exist/parse.
/// - else `/etc/cf-rs/config.toml` is used **only if it exists**; if absent, no file is loaded
///   and defaults (plus any env overrides) apply.
pub fn load(config_path: Option<&Path>) -> Result<AppConfig, ConfigError> {
    let (path, required) = match config_path {
        Some(p) => (Some(p.to_path_buf()), true),
        None => match std::env::var("CF_RS_CONFIG") {
            Ok(v) if !v.is_empty() => (Some(std::path::PathBuf::from(v)), true),
            _ => {
                let default_path = std::path::PathBuf::from("/etc/cf-rs/config.toml");
                if default_path.exists() {
                    (Some(default_path), false)
                } else {
                    (None, false)
                }
            }
        },
    };

    let mut builder = config::Config::builder();
    if let Some(path) = path {
        builder = builder.add_source(config::File::from(path).required(required));
    }
    builder = builder.add_source(config::Environment::with_prefix("CF_RS").separator("__"));

    let built = builder
        .build()
        .map_err(|e| ConfigError::Load(e.to_string()))?;

    built
        .try_deserialize::<AppConfig>()
        .map_err(|e| ConfigError::Load(e.to_string()))
}

fn is_loopback_host(listen: &str) -> bool {
    let host = listen.rsplit_once(':').map_or(listen, |(h, _)| h);
    let host = host.trim_start_matches('[').trim_end_matches(']');
    host == "127.0.0.1" || host == "localhost" || host == "::1"
}

/// Validates configuration-file-shape rules from the "Validation and startup failure" table in
/// `ops-config.md` that are checkable without I/O. Rules requiring runtime probing
/// (`storage.data_dir` writability, `build.mode` tool availability, ps-rs reachability,
/// cgroup writability) are intentionally out of scope for this pure function.
pub fn validate(cfg: &AppConfig) -> Result<(), ConfigError> {
    if !is_loopback_host(&cfg.admin.listen)
        && cfg.admin.token.is_empty()
        && cfg.admin.token_file.is_empty()
    {
        return Err(ConfigError::Invalid {
            field: "admin.token",
            reason: "admin.token or admin.token_file is required when admin.listen is not loopback"
                .to_string(),
        });
    }

    match cfg.build.mode.as_str() {
        "auto" | "host" | "container" => {}
        other => {
            return Err(ConfigError::Invalid {
                field: "build.mode",
                reason: format!("must be one of \"auto\", \"host\", \"container\", got {other:?}"),
            });
        }
    }

    match cfg.runtime.cgroup.as_str() {
        "auto" | "off" => {}
        other => {
            return Err(ConfigError::Invalid {
                field: "runtime.cgroup",
                reason: format!("must be one of \"auto\", \"off\", got {other:?}"),
            });
        }
    }

    match cfg.defaults.queue_policy.as_str() {
        "wait" | "reject" => {}
        other => {
            return Err(ConfigError::Invalid {
                field: "defaults.queue_policy",
                reason: format!("must be one of \"wait\", \"reject\", got {other:?}"),
            });
        }
    }

    match cfg.log.format.as_str() {
        "json" | "text" | "journald" => {}
        other => {
            return Err(ConfigError::Invalid {
                field: "log.format",
                reason: format!("must be one of \"json\", \"text\", \"journald\", got {other:?}"),
            });
        }
    }

    Ok(())
}
