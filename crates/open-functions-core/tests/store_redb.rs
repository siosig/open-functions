//! Integration tests for `RedbStore` against the public `Store` trait plus its
//! `set_current_revision` inherent method.
//!
//! Note: this crate configures `clippy::unwrap_used`/`clippy::expect_used` as
//! warnings (promoted to errors under `-D warnings`), and that lint config
//! applies to every target in the package, including this integration test
//! binary. `ok`/`some` below stand in for `.unwrap()`/`.expect()` without
//! tripping those lints.

use open_functions_core::model::function::{FunctionState, QueuePolicy, Source, Trigger};
use open_functions_core::model::{Function, Revision};
use open_functions_core::registry::redb_store::RedbStore;
use open_functions_core::registry::store::{Store, StoreError};

fn ok<T, E: std::fmt::Debug>(result: Result<T, E>, context: &str) -> T {
    result.unwrap_or_else(|e| panic!("{context}: {e:?}"))
}

fn some<T>(option: Option<T>, context: &str) -> T {
    option.unwrap_or_else(|| panic!("{context}: expected Some, got None"))
}

fn sample_function(name: &str) -> Function {
    let now = chrono::Utc::now();
    Function {
        name: name.to_string(),
        trigger: Trigger::Http,
        source: Source::Dir {
            path: format!("/tmp/{name}"),
            bin: None,
        },
        env: Default::default(),
        entry_point: "function".to_string(),
        timeout_secs: 60,
        concurrency: 1,
        memory_mib: 256,
        min_instances: 0,
        max_instances: 1,
        idle_timeout_secs: 300,
        queue_policy: QueuePolicy::Wait,
        queue_max_wait_secs: 30,
        state: FunctionState::Ready,
        current_revision: None,
        last_error: None,
        created_at: now,
        updated_at: now,
    }
}

fn sample_revision(name: &str, number: u32) -> Revision {
    Revision {
        function_name: name.to_string(),
        number,
        artifact_path: Some(format!("/tmp/{name}/{number}/function")),
        image_digest: None,
        build_id: None,
        snapshot: sample_function(name),
        created_at: chrono::Utc::now(),
    }
}

#[test]
fn put_get_list_delete_function() {
    let dir = ok(tempfile::tempdir(), "tempdir");
    let db_path = dir.path().join("meta.redb");
    let store = ok(RedbStore::open(&db_path), "open store");

    let f1 = sample_function("alpha");
    let f2 = sample_function("beta");
    ok(store.put_function(&f1), "put alpha");
    ok(store.put_function(&f2), "put beta");

    let got = ok(store.get_function("alpha"), "get alpha");
    let got = some(got, "alpha present");
    assert_eq!(got.name, "alpha");

    let missing = ok(store.get_function("does-not-exist"), "get missing");
    assert!(missing.is_none());

    let mut listed = ok(store.list_functions(), "list");
    assert_eq!(listed.len(), 2);
    listed.sort_by(|a, b| a.name.cmp(&b.name));
    assert_eq!(listed[0].name, "alpha");
    assert_eq!(listed[1].name, "beta");

    ok(store.delete_function("alpha"), "delete alpha");
    let listed_after_delete = ok(store.list_functions(), "list after delete");
    assert_eq!(listed_after_delete.len(), 1);
    assert_eq!(listed_after_delete[0].name, "beta");
    let after_delete = ok(store.get_function("alpha"), "get after delete");
    assert!(after_delete.is_none());
}

#[test]
fn revisions_with_same_function_name_do_not_collide() {
    let dir = ok(tempfile::tempdir(), "tempdir");
    let db_path = dir.path().join("meta.redb");
    let store = ok(RedbStore::open(&db_path), "open store");

    let rev1 = sample_revision("gamma", 1);
    let rev2 = sample_revision("gamma", 2);
    ok(store.put_revision(&rev1), "put rev1");
    ok(store.put_revision(&rev2), "put rev2");

    let got1 = some(
        ok(store.get_revision("gamma", 1), "get rev1"),
        "rev1 exists",
    );
    let got2 = some(
        ok(store.get_revision("gamma", 2), "get rev2"),
        "rev2 exists",
    );

    assert_eq!(got1.number, 1);
    assert_eq!(got2.number, 2);
    assert_eq!(got1.function_name, "gamma");
    assert_eq!(got2.function_name, "gamma");

    let missing = ok(store.get_revision("gamma", 3), "get rev3");
    assert!(missing.is_none());
}

#[test]
fn set_current_revision_persists_and_is_visible_on_next_read() {
    let dir = ok(tempfile::tempdir(), "tempdir");
    let db_path = dir.path().join("meta.redb");
    let store = ok(RedbStore::open(&db_path), "open store");

    let function = sample_function("delta");
    ok(store.put_function(&function), "put delta");
    let before = some(ok(store.get_function("delta"), "get delta"), "delta exists");
    assert_eq!(before.current_revision, None);

    ok(
        store.set_current_revision("delta", 3),
        "set current revision",
    );

    let updated = some(
        ok(store.get_function("delta"), "get delta after set"),
        "delta still exists",
    );
    assert_eq!(updated.current_revision, Some(3));
    assert!(updated.updated_at >= function.updated_at);
}

#[test]
fn set_current_revision_on_missing_function_returns_not_found() {
    let dir = ok(tempfile::tempdir(), "tempdir");
    let db_path = dir.path().join("meta.redb");
    let store = ok(RedbStore::open(&db_path), "open store");

    let result = store.set_current_revision("no-such-function", 1);
    match result {
        Err(StoreError::NotFound(name)) => assert_eq!(name, "no-such-function"),
        other => panic!("expected NotFound, got {other:?}"),
    }
}

#[test]
fn reopening_same_db_file_round_trips_data() {
    let dir = ok(tempfile::tempdir(), "tempdir");
    let db_path = dir.path().join("meta.redb");

    {
        let store = ok(RedbStore::open(&db_path), "open store first time");
        let function = sample_function("epsilon");
        ok(store.put_function(&function), "put epsilon");
        let revision = sample_revision("epsilon", 1);
        ok(store.put_revision(&revision), "put revision 1");
        // `store` is dropped here, closing the redb `Database`.
    }

    let store = ok(RedbStore::open(&db_path), "reopen store");
    let function = some(
        ok(store.get_function("epsilon"), "get epsilon after reopen"),
        "epsilon exists after reopen",
    );
    assert_eq!(function.name, "epsilon");

    let revision = some(
        ok(
            store.get_revision("epsilon", 1),
            "get revision after reopen",
        ),
        "revision exists after reopen",
    );
    assert_eq!(revision.function_name, "epsilon");
    assert_eq!(revision.number, 1);
}
