//! Validation rules for `Function` registration (data-model.md validation rules).
//!
//! Two-stage validation per data-model.md: serde (structure/types) happens at
//! deserialization time; this module implements the second stage (range and
//! cross-field correlation checks). I/O-bound checks (source path existence,
//! container runtime availability) are out of scope here and belong to the
//! registry service (a later task).

use std::collections::BTreeMap;

use super::function::{Function, Source, Trigger};
use super::runtime::Runtime;

/// Reserved environment variable name prefix that GCP/open-functions sets automatically
/// (`K_SERVICE`, `K_REVISION`, `K_CONFIGURATION`, ...).
const RESERVED_ENV_PREFIX: &str = "K_";

/// Exact reserved environment variable names (data-model.md `env` constraints).
/// `HOST` / `FUNCTION_SOURCE` / `VIRTUAL_ENV` are reserved for every runtime
/// (002-python-runtime data-model.md's Function validation-rule diff), not just
/// Python functions, so a function can switch runtimes across a re-register
/// without an env key that was fine before suddenly becoming reserved.
const RESERVED_ENV_NAMES: [&str; 6] = [
    "PORT",
    "FUNCTION_TARGET",
    "FUNCTION_SIGNATURE_TYPE",
    "HOST",
    "FUNCTION_SOURCE",
    "VIRTUAL_ENV",
];

/// Maximum total serialized size (bytes) of all env values (data-model.md: 32 KiB).
const ENV_VALUES_MAX_BYTES: usize = 32 * 1024;

/// Function name pattern: `^[a-z][a-z0-9-]{0,62}$`.
const NAME_MAX_LEN: usize = 63;

/// Reserved function name prefix (data-model.md: `_cf` reserved).
const NAME_RESERVED_PREFIX: &str = "_cf";

/// Pub/Sub topic id length bounds (data-model.md: `^[A-Za-z][\w\-.~+%]{2,254}$`).
const TOPIC_MIN_LEN: usize = 3;
const TOPIC_MAX_LEN: usize = 255;

const TIMEOUT_HTTP_MIN: u32 = 1;
const TIMEOUT_HTTP_MAX: u32 = 3600;
const TIMEOUT_PUBSUB_MIN: u32 = 1;
const TIMEOUT_PUBSUB_MAX: u32 = 540;

const CONCURRENCY_MIN: u32 = 1;
const CONCURRENCY_MAX: u32 = 1000;

const MEMORY_MIB_MIN: u32 = 128;
const MEMORY_MIB_MAX: u32 = 32768;

const MAX_INSTANCES_MIN: u32 = 1;
const MAX_INSTANCES_MAX: u32 = 1000;

const IDLE_TIMEOUT_SECS_MIN: u32 = 10;
const IDLE_TIMEOUT_SECS_MAX: u32 = 86400;

const QUEUE_MAX_WAIT_SECS_MIN: u32 = 0;
const QUEUE_MAX_WAIT_SECS_MAX: u32 = 600;

#[derive(Debug, thiserror::Error)]
pub enum ValidationError {
    #[error(
        "invalid function name {0:?}: must match ^[a-z][a-z0-9-]{{0,62}}$ and not start with \"_cf\""
    )]
    InvalidName(String),

    #[error("invalid pubsub topic {0:?}: must match ^[A-Za-z][\\w\\-.~+%]{{2,254}}$")]
    InvalidTopic(String),

    #[error("invalid env var key {0:?}: must match ^[A-Za-z_][A-Za-z0-9_]*$")]
    InvalidEnvKey(String),

    #[error("reserved env var key {0:?} is set automatically and cannot be overridden")]
    ReservedEnvKey(String),

    #[error("total env value size {0} bytes exceeds the {ENV_VALUES_MAX_BYTES} byte limit")]
    EnvTooLarge(usize),

    #[error("field {field}: value {value} out of range [{min}, {max}]")]
    OutOfRange {
        field: &'static str,
        value: i64,
        min: i64,
        max: i64,
    },

    #[error("min_instances ({min}) must not exceed max_instances ({max})")]
    MinExceedsMax { min: u32, max: u32 },

    #[error(
        "invalid entry_point {0:?} for runtime python314: must match ^[A-Za-z_][A-Za-z0-9_]*$ \
         (a Python identifier)"
    )]
    InvalidPythonEntryPoint(String),

    #[error(
        "source.bin is not applicable to runtime python314 (bin target selection is a Rust/cargo \
         concept; a Python source directory has a single main.py entry file)"
    )]
    BinNotApplicableToPython,
}

/// Validates a function name against `^[a-z][a-z0-9-]{0,62}$` and the `_cf` reserved prefix.
pub fn validate_name(name: &str) -> Result<(), ValidationError> {
    let is_valid_shape = !name.is_empty()
        && name.len() <= NAME_MAX_LEN
        && name.as_bytes().first().is_some_and(u8::is_ascii_lowercase)
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-');

    if !is_valid_shape || name.starts_with(NAME_RESERVED_PREFIX) {
        return Err(ValidationError::InvalidName(name.to_string()));
    }
    Ok(())
}

/// Validates a Pub/Sub topic id against `^[A-Za-z][\w\-.~+%]{2,254}$`.
pub fn validate_topic(topic: &str) -> Result<(), ValidationError> {
    let is_valid = topic.len() >= TOPIC_MIN_LEN
        && topic.len() <= TOPIC_MAX_LEN
        && topic
            .as_bytes()
            .first()
            .is_some_and(u8::is_ascii_alphabetic)
        && topic.bytes().all(is_topic_char);

    if !is_valid {
        return Err(ValidationError::InvalidTopic(topic.to_string()));
    }
    Ok(())
}

/// `\w` (word char) plus the Pub/Sub-specific extras `-.~+%`.
fn is_topic_char(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_' || matches!(b, b'-' | b'.' | b'~' | b'+' | b'%')
}

/// Validates env var keys (format + reserved names) and the total value size.
pub fn validate_env(env: &BTreeMap<String, String>) -> Result<(), ValidationError> {
    for key in env.keys() {
        if !is_valid_env_key_shape(key) {
            return Err(ValidationError::InvalidEnvKey(key.clone()));
        }
        if is_reserved_env_key(key) {
            return Err(ValidationError::ReservedEnvKey(key.clone()));
        }
    }

    let total_bytes: usize = env.values().map(String::len).sum();
    if total_bytes > ENV_VALUES_MAX_BYTES {
        return Err(ValidationError::EnvTooLarge(total_bytes));
    }

    Ok(())
}

fn is_valid_env_key_shape(key: &str) -> bool {
    key.as_bytes()
        .first()
        .is_some_and(|&b| b.is_ascii_alphabetic() || b == b'_')
        && key.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
}

fn is_reserved_env_key(key: &str) -> bool {
    RESERVED_ENV_NAMES.contains(&key) || key.starts_with(RESERVED_ENV_PREFIX)
}

/// Validates a range for a `u32` field, converting bounds/value to `i64` for the error payload.
fn validate_range_u32(
    field: &'static str,
    value: u32,
    min: u32,
    max: u32,
) -> Result<(), ValidationError> {
    if value < min || value > max {
        return Err(ValidationError::OutOfRange {
            field,
            value: i64::from(value),
            min: i64::from(min),
            max: i64::from(max),
        });
    }
    Ok(())
}

/// Validates a `Function` per data-model.md validation rules (structural/range checks only;
/// no I/O such as source-path existence or container runtime availability).
pub fn validate_function(f: &Function) -> Result<(), ValidationError> {
    validate_name(&f.name)?;

    match &f.trigger {
        Trigger::Http => {
            validate_range_u32(
                "timeout_secs",
                f.timeout_secs,
                TIMEOUT_HTTP_MIN,
                TIMEOUT_HTTP_MAX,
            )?;
        }
        Trigger::Pubsub { topic } => {
            validate_topic(topic)?;
            validate_range_u32(
                "timeout_secs",
                f.timeout_secs,
                TIMEOUT_PUBSUB_MIN,
                TIMEOUT_PUBSUB_MAX,
            )?;
        }
    }

    validate_range_u32(
        "concurrency",
        f.concurrency,
        CONCURRENCY_MIN,
        CONCURRENCY_MAX,
    )?;
    validate_range_u32("memory_mib", f.memory_mib, MEMORY_MIB_MIN, MEMORY_MIB_MAX)?;
    validate_range_u32(
        "max_instances",
        f.max_instances,
        MAX_INSTANCES_MIN,
        MAX_INSTANCES_MAX,
    )?;
    validate_range_u32(
        "idle_timeout_secs",
        f.idle_timeout_secs,
        IDLE_TIMEOUT_SECS_MIN,
        IDLE_TIMEOUT_SECS_MAX,
    )?;
    validate_range_u32(
        "queue_max_wait_secs",
        f.queue_max_wait_secs,
        QUEUE_MAX_WAIT_SECS_MIN,
        QUEUE_MAX_WAIT_SECS_MAX,
    )?;

    if f.min_instances > f.max_instances {
        return Err(ValidationError::MinExceedsMax {
            min: f.min_instances,
            max: f.max_instances,
        });
    }

    validate_env(&f.env)?;

    // Python-specific rules apply only to source-mode: image-mode's
    // `runtime` is a display-only hint (data-model.md's "validation rules
    // (delta)" section: "source.kind = image: runtime is optional and
    // never validated"), so an image-mode function declaring
    // `runtime = python314` must not be rejected for an
    // `entry_point` shape or a `bin` field that only matter when open-functions
    // itself builds and launches a Python venv.
    if f.runtime == Some(Runtime::Python314)
        && let Source::Dir { bin, .. } = &f.source
    {
        validate_python_entry_point(&f.entry_point)?;
        if bin.is_some() {
            return Err(ValidationError::BinNotApplicableToPython);
        }
    }

    Ok(())
}

/// Validates a Python entry-point name against `^[A-Za-z_][A-Za-z0-9_]*$`
/// (a valid Python identifier -- `FUNCTION_TARGET` is passed straight
/// through to `functions-framework`, which imports it as an attribute of
/// `main.py`, so anything else can never resolve).
fn validate_python_entry_point(entry_point: &str) -> Result<(), ValidationError> {
    let is_valid = entry_point
        .as_bytes()
        .first()
        .is_some_and(|&b| b.is_ascii_alphabetic() || b == b'_')
        && entry_point
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_');

    if !is_valid {
        return Err(ValidationError::InvalidPythonEntryPoint(
            entry_point.to_string(),
        ));
    }
    Ok(())
}
