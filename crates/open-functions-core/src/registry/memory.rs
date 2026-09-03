// In-memory `Store` implementation for tests. Implemented in Foundational phase (T014).

use std::collections::HashMap;
use std::sync::RwLock;

use super::store::{Store, StoreError};
use crate::model::{Build, Function, Revision, TriggerBinding};

/// An in-memory, thread-safe `Store` implementation intended for unit and
/// integration tests. Not persisted to disk.
#[derive(Debug, Default)]
pub struct MemoryStore {
    functions: RwLock<HashMap<String, Function>>,
    revisions: RwLock<HashMap<(String, u32), Revision>>,
    builds: RwLock<HashMap<String, Build>>,
    bindings: RwLock<HashMap<String, TriggerBinding>>,
}

impl MemoryStore {
    pub fn new() -> Self {
        Self::default()
    }
}

impl Store for MemoryStore {
    fn get_function(&self, name: &str) -> Result<Option<Function>, StoreError> {
        let functions = self.functions.read().unwrap_or_else(|e| e.into_inner());
        Ok(functions.get(name).cloned())
    }

    fn list_functions(&self) -> Result<Vec<Function>, StoreError> {
        let functions = self.functions.read().unwrap_or_else(|e| e.into_inner());
        let mut list: Vec<Function> = functions.values().cloned().collect();
        list.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(list)
    }

    fn put_function(&self, function: &Function) -> Result<(), StoreError> {
        let mut functions = self.functions.write().unwrap_or_else(|e| e.into_inner());
        functions.insert(function.name.clone(), function.clone());
        Ok(())
    }

    fn delete_function(&self, name: &str) -> Result<(), StoreError> {
        let mut functions = self.functions.write().unwrap_or_else(|e| e.into_inner());
        functions.remove(name);
        Ok(())
    }

    fn put_revision(&self, revision: &Revision) -> Result<(), StoreError> {
        let mut revisions = self.revisions.write().unwrap_or_else(|e| e.into_inner());
        revisions.insert(
            (revision.function_name.clone(), revision.number),
            revision.clone(),
        );
        Ok(())
    }

    fn get_revision(&self, name: &str, number: u32) -> Result<Option<Revision>, StoreError> {
        let revisions = self.revisions.read().unwrap_or_else(|e| e.into_inner());
        Ok(revisions.get(&(name.to_string(), number)).cloned())
    }

    fn put_build(&self, build: &Build) -> Result<(), StoreError> {
        let mut builds = self.builds.write().unwrap_or_else(|e| e.into_inner());
        builds.insert(build.id.clone(), build.clone());
        Ok(())
    }

    fn get_build(&self, id: &str) -> Result<Option<Build>, StoreError> {
        let builds = self.builds.read().unwrap_or_else(|e| e.into_inner());
        Ok(builds.get(id).cloned())
    }

    fn list_builds(&self) -> Result<Vec<Build>, StoreError> {
        let builds = self.builds.read().unwrap_or_else(|e| e.into_inner());
        Ok(builds.values().cloned().collect())
    }

    fn get_binding(&self, name: &str) -> Result<Option<TriggerBinding>, StoreError> {
        let bindings = self.bindings.read().unwrap_or_else(|e| e.into_inner());
        Ok(bindings.get(name).cloned())
    }

    fn put_binding(&self, binding: &TriggerBinding) -> Result<(), StoreError> {
        let mut bindings = self.bindings.write().unwrap_or_else(|e| e.into_inner());
        bindings.insert(binding.function_name.clone(), binding.clone());
        Ok(())
    }

    fn delete_binding(&self, name: &str) -> Result<(), StoreError> {
        let mut bindings = self.bindings.write().unwrap_or_else(|e| e.into_inner());
        bindings.remove(name);
        Ok(())
    }

    fn list_bindings(&self) -> Result<Vec<TriggerBinding>, StoreError> {
        let bindings = self.bindings.read().unwrap_or_else(|e| e.into_inner());
        Ok(bindings.values().cloned().collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Runtime;
    use crate::model::binding::BindingState;
    use crate::model::build::{BuildMode, BuildStatus};
    use crate::model::function::{FunctionState, QueuePolicy, Source, Trigger};
    use chrono::Utc;

    // This crate configures `clippy::unwrap_used`/`clippy::expect_used` as
    // warnings (promoted to errors under `-D warnings`), applying to every
    // target including this test module. `ok`/`some` stand in for
    // `.unwrap()`/`.expect()` without tripping those lints.
    fn ok<T, E: std::fmt::Debug>(result: Result<T, E>, context: &str) -> T {
        result.unwrap_or_else(|e| panic!("{context}: {e:?}"))
    }

    fn some<T>(option: Option<T>, context: &str) -> T {
        option.unwrap_or_else(|| panic!("{context}: expected Some, got None"))
    }

    fn sample_function(name: &str) -> Function {
        let now = Utc::now();
        Function {
            name: name.to_string(),
            trigger: Trigger::Http,
            runtime: Some(Runtime::Rust),
            source: Source::Dir {
                path: "/tmp/src".to_string(),
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

    #[test]
    fn put_and_get_function_round_trips() {
        let store = MemoryStore::new();
        let f = sample_function("alpha");
        ok(store.put_function(&f), "put_function");

        let got = ok(store.get_function("alpha"), "get_function");
        let got = some(got, "expected alpha to be present");
        assert_eq!(got.name, "alpha");

        let missing = ok(store.get_function("missing"), "get_function missing");
        assert!(missing.is_none());
    }

    #[test]
    fn list_functions_is_sorted_by_name() {
        let store = MemoryStore::new();
        ok(
            store.put_function(&sample_function("charlie")),
            "put charlie",
        );
        ok(store.put_function(&sample_function("alpha")), "put alpha");
        ok(store.put_function(&sample_function("bravo")), "put bravo");

        let names: Vec<String> = ok(store.list_functions(), "list_functions")
            .into_iter()
            .map(|f| f.name)
            .collect();
        assert_eq!(names, vec!["alpha", "bravo", "charlie"]);
    }

    #[test]
    fn delete_function_removes_it() {
        let store = MemoryStore::new();
        ok(
            store.put_function(&sample_function("alpha")),
            "put_function",
        );
        ok(store.delete_function("alpha"), "delete_function");
        assert!(ok(store.get_function("alpha"), "get_function after delete").is_none());
        // Deleting a nonexistent function is not an error.
        ok(store.delete_function("alpha"), "delete_function again");
    }

    #[test]
    fn put_and_get_revision_round_trips() {
        let store = MemoryStore::new();
        let snapshot = sample_function("alpha");
        let revision = Revision {
            function_name: "alpha".to_string(),
            number: 1,
            artifact_path: Some("/tmp/artifact".to_string()),
            image_digest: None,
            build_id: Some("build-1".to_string()),
            snapshot,
            build_mode: Some(BuildMode::Host),
            container_image: None,
            artifact_pruned: false,
            created_at: Utc::now(),
        };
        ok(store.put_revision(&revision), "put_revision");

        let got = ok(store.get_revision("alpha", 1), "get_revision");
        let got = some(got, "expected revision 1 to be present");
        assert_eq!(got.number, 1);

        assert!(ok(store.get_revision("alpha", 2), "get_revision wrong number").is_none());
        assert!(ok(store.get_revision("missing", 1), "get_revision wrong name").is_none());
    }

    #[test]
    fn put_and_get_build_round_trips() {
        let store = MemoryStore::new();
        let build = Build {
            id: "build-1".to_string(),
            function_name: "alpha".to_string(),
            revision: 1,
            mode: BuildMode::Host,
            status: BuildStatus::Succeeded,
            log_path: "/tmp/log".to_string(),
            exit_code: Some(0),
            started_at: Utc::now(),
            finished_at: Some(Utc::now()),
            tool: Some("cargo".to_string()),
        };
        ok(store.put_build(&build), "put_build");

        let got = ok(store.get_build("build-1"), "get_build");
        let got = some(got, "expected build-1 to be present");
        assert_eq!(got.id, "build-1");

        assert!(ok(store.get_build("missing"), "get_build missing").is_none());
    }

    #[test]
    fn put_get_delete_binding_round_trips() {
        let store = MemoryStore::new();
        let binding = TriggerBinding {
            function_name: "alpha".to_string(),
            subscription: "sub-alpha".to_string(),
            topic: "topic-alpha".to_string(),
            push_endpoint: "http://localhost:8080/push".to_string(),
            state: BindingState::Bound,
            last_error: None,
            next_retry_at: None,
        };
        ok(store.put_binding(&binding), "put_binding");

        let got = ok(store.get_binding("alpha"), "get_binding");
        let got = some(got, "expected binding for alpha to be present");
        assert_eq!(got.subscription, "sub-alpha");

        ok(store.delete_binding("alpha"), "delete_binding");
        assert!(ok(store.get_binding("alpha"), "get_binding after delete").is_none());
    }
}
