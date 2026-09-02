//! Unit + property tests for `model::validate` (T007).

use std::collections::BTreeMap;

use proptest::prelude::*;

use super::function::{Function, FunctionState, QueuePolicy, Source, Trigger};
use super::validate::{
    ValidationError, validate_env, validate_function, validate_name, validate_topic,
};

/// Builds a minimal, fully-valid `Function` (HTTP trigger, all defaults from
/// data-model.md) so individual tests only need to override the field under test.
fn valid_function() -> Function {
    let now = chrono::Utc::now();
    Function {
        name: "hello-world".to_string(),
        trigger: Trigger::Http,
        source: Source::Dir {
            path: "/tmp/hello-world".to_string(),
            bin: None,
        },
        env: BTreeMap::new(),
        entry_point: "function".to_string(),
        timeout_secs: 60,
        concurrency: 1,
        memory_mib: 256,
        min_instances: 0,
        max_instances: 100,
        idle_timeout_secs: 900,
        queue_policy: QueuePolicy::Wait,
        queue_max_wait_secs: 30,
        state: FunctionState::Building,
        current_revision: None,
        last_error: None,
        created_at: now,
        updated_at: now,
    }
}

// ---- validate_name ----

#[test]
fn name_accepts_valid_lowercase_forms() {
    for name in ["a", "hello", "hello-world", "h1-2-3", &"a".repeat(63)] {
        assert!(validate_name(name).is_ok(), "expected {name:?} to be valid");
    }
}

#[test]
fn name_rejects_uppercase() {
    assert!(matches!(
        validate_name("Hello"),
        Err(ValidationError::InvalidName(_))
    ));
}

#[test]
fn name_rejects_leading_digit() {
    assert!(matches!(
        validate_name("1hello"),
        Err(ValidationError::InvalidName(_))
    ));
}

#[test]
fn name_rejects_cf_reserved_prefix() {
    assert!(matches!(
        validate_name("_cf-internal"),
        Err(ValidationError::InvalidName(_))
    ));
}

#[test]
fn name_rejects_too_long() {
    let too_long = "a".repeat(64);
    assert!(matches!(
        validate_name(&too_long),
        Err(ValidationError::InvalidName(_))
    ));
}

#[test]
fn name_rejects_empty() {
    assert!(matches!(
        validate_name(""),
        Err(ValidationError::InvalidName(_))
    ));
}

// ---- validate_topic ----

#[test]
fn topic_accepts_valid_ids() {
    for topic in ["abc", "Topic-1.2~3+4%5", "A_B_C"] {
        assert!(
            validate_topic(topic).is_ok(),
            "expected {topic:?} to be valid"
        );
    }
}

#[test]
fn topic_rejects_leading_digit() {
    assert!(matches!(
        validate_topic("1abc"),
        Err(ValidationError::InvalidTopic(_))
    ));
}

#[test]
fn topic_rejects_too_short() {
    // Pattern requires length >= 3 (`^[A-Za-z][\w\-.~+%]{2,254}$`).
    assert!(matches!(
        validate_topic("ab"),
        Err(ValidationError::InvalidTopic(_))
    ));
}

#[test]
fn topic_rejects_invalid_char() {
    assert!(matches!(
        validate_topic("abc/def"),
        Err(ValidationError::InvalidTopic(_))
    ));
}

#[test]
fn topic_rejects_too_long() {
    let too_long = format!("a{}", "b".repeat(255));
    assert!(matches!(
        validate_topic(&too_long),
        Err(ValidationError::InvalidTopic(_))
    ));
}

// ---- validate_env ----

#[test]
fn env_accepts_valid_keys() {
    let mut env = BTreeMap::new();
    env.insert("MY_VAR".to_string(), "value".to_string());
    env.insert("_private".to_string(), "value".to_string());
    assert!(validate_env(&env).is_ok());
}

#[test]
fn env_rejects_invalid_key_shape() {
    let mut env = BTreeMap::new();
    env.insert("1BAD".to_string(), "value".to_string());
    assert!(matches!(
        validate_env(&env),
        Err(ValidationError::InvalidEnvKey(_))
    ));
}

#[test]
fn env_rejects_reserved_names() {
    for reserved in [
        "PORT",
        "FUNCTION_TARGET",
        "FUNCTION_SIGNATURE_TYPE",
        "K_SERVICE",
        "K_REVISION",
    ] {
        let mut env = BTreeMap::new();
        env.insert(reserved.to_string(), "value".to_string());
        assert!(
            matches!(validate_env(&env), Err(ValidationError::ReservedEnvKey(_))),
            "expected {reserved:?} to be rejected as reserved"
        );
    }
}

#[test]
fn env_rejects_oversized_values() {
    let mut env = BTreeMap::new();
    env.insert("BIG".to_string(), "x".repeat(32 * 1024 + 1));
    assert!(matches!(
        validate_env(&env),
        Err(ValidationError::EnvTooLarge(_))
    ));
}

#[test]
fn env_accepts_values_at_size_limit() {
    let mut env = BTreeMap::new();
    env.insert("BIG".to_string(), "x".repeat(32 * 1024));
    assert!(validate_env(&env).is_ok());
}

// ---- validate_function: range validation ----

#[test]
fn concurrency_boundaries() {
    let mut f = valid_function();

    f.concurrency = 0;
    assert!(validate_function(&f).is_err());

    f.concurrency = 1001;
    assert!(validate_function(&f).is_err());

    f.concurrency = 1;
    assert!(validate_function(&f).is_ok());

    f.concurrency = 1000;
    assert!(validate_function(&f).is_ok());
}

#[test]
fn memory_mib_boundaries() {
    let mut f = valid_function();

    f.memory_mib = 127;
    assert!(validate_function(&f).is_err());

    f.memory_mib = 32769;
    assert!(validate_function(&f).is_err());

    f.memory_mib = 128;
    assert!(validate_function(&f).is_ok());

    f.memory_mib = 32768;
    assert!(validate_function(&f).is_ok());
}

#[test]
fn min_instances_exceeding_max_is_rejected() {
    let mut f = valid_function();
    f.min_instances = 5;
    f.max_instances = 4;
    assert!(matches!(
        validate_function(&f),
        Err(ValidationError::MinExceedsMax { min: 5, max: 4 })
    ));
}

#[test]
fn min_instances_equal_to_max_is_accepted() {
    let mut f = valid_function();
    f.min_instances = 4;
    f.max_instances = 4;
    assert!(validate_function(&f).is_ok());
}

// ---- validate_function: trigger-specific timeout ----

#[test]
fn pubsub_timeout_over_540_is_rejected() {
    let mut f = valid_function();
    f.trigger = Trigger::Pubsub {
        topic: "my-topic".to_string(),
    };
    f.timeout_secs = 600;
    assert!(validate_function(&f).is_err());
}

#[test]
fn pubsub_timeout_at_540_is_accepted() {
    let mut f = valid_function();
    f.trigger = Trigger::Pubsub {
        topic: "my-topic".to_string(),
    };
    f.timeout_secs = 540;
    assert!(validate_function(&f).is_ok());
}

#[test]
fn http_timeout_up_to_3600_is_accepted() {
    let mut f = valid_function();
    f.trigger = Trigger::Http;
    f.timeout_secs = 3600;
    assert!(validate_function(&f).is_ok());
}

#[test]
fn http_timeout_over_3600_is_rejected() {
    let mut f = valid_function();
    f.trigger = Trigger::Http;
    f.timeout_secs = 3601;
    assert!(validate_function(&f).is_err());
}

#[test]
fn pubsub_trigger_with_invalid_topic_is_rejected() {
    let mut f = valid_function();
    f.trigger = Trigger::Pubsub {
        topic: "1-bad-topic".to_string(),
    };
    assert!(matches!(
        validate_function(&f),
        Err(ValidationError::InvalidTopic(_))
    ));
}

#[test]
fn valid_function_passes() {
    assert!(validate_function(&valid_function()).is_ok());
}

// ---- property tests ----

proptest! {
    /// Any string matching the name grammar (and not starting with the reserved
    /// `_cf` prefix, which the regex below never produces because it starts
    /// with a letter followed by digits/hyphens only after position 0 — but we
    /// still assert explicitly for documentation purposes) must validate.
    #[test]
    fn prop_generated_valid_names_are_accepted(name in "[a-z][a-z0-9-]{0,62}") {
        prop_assert!(validate_name(&name).is_ok());
    }

    /// Strings containing an uppercase letter or a disallowed symbol must be rejected.
    #[test]
    fn prop_names_with_uppercase_or_symbols_are_rejected(
        prefix in "[a-zA-Z0-9]{0,10}",
        bad_char in prop::char::range('A', 'Z'),
        suffix in "[a-zA-Z0-9]{0,10}",
    ) {
        let name = format!("{prefix}{bad_char}{suffix}");
        prop_assert!(validate_name(&name).is_err());
    }

    /// Strings containing a symbol outside `[a-z0-9-]` (after a valid first char)
    /// must always be rejected.
    #[test]
    fn prop_names_with_disallowed_symbols_are_rejected(
        head in "[a-z]",
        symbol in prop::sample::select(vec!['_', '.', '/', '@', '!', ' ']),
        tail in "[a-z0-9-]{0,10}",
    ) {
        let name = format!("{head}{symbol}{tail}");
        prop_assert!(validate_name(&name).is_err());
    }
}
