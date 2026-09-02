//! redb-backed `Store` implementation.
//!
//! Tables and keys follow `specs/001-cloud-functions-local/data-model.md`
//! ("redb tables and file layout"). All values are JSON-encoded via `serde_json`.
//! `revisions` uses a composite `(name, number)` key encoded as a single string
//! `"{name}\0{number:010}"` so lexicographic byte order matches `(name, number)`
//! ordering without requiring redb's multi-field key support.

use crate::model::{Build, Function, Revision, TriggerBinding};
use crate::registry::store::{Store, StoreError};
use redb::{Database, ReadableDatabase, ReadableTable, TableDefinition};
use std::path::Path;

const FUNCTIONS_TABLE: TableDefinition<&str, &[u8]> = TableDefinition::new("functions");
const REVISIONS_TABLE: TableDefinition<&str, &[u8]> = TableDefinition::new("revisions");
const BUILDS_TABLE: TableDefinition<&str, &[u8]> = TableDefinition::new("builds");
const BINDINGS_TABLE: TableDefinition<&str, &[u8]> = TableDefinition::new("bindings");
const META_TABLE: TableDefinition<&str, &[u8]> = TableDefinition::new("meta");

const SCHEMA_VERSION_KEY: &str = "schema_version";
const SCHEMA_VERSION: &str = "1";

/// Separator between the function name and the zero-padded revision number in
/// the `revisions` table's composite key. `\0` sorts below every printable
/// character, so the encoding preserves `(name, number)` ordering.
const REVISION_KEY_SEP: char = '\0';

fn revision_key(name: &str, number: u32) -> String {
    format!("{name}{REVISION_KEY_SEP}{number:010}")
}

fn backend_err<E: std::fmt::Display>(e: E) -> StoreError {
    StoreError::Backend(e.to_string())
}

fn to_json<T: serde::Serialize>(value: &T) -> Result<Vec<u8>, StoreError> {
    serde_json::to_vec(value).map_err(backend_err)
}

fn from_json<T: serde::de::DeserializeOwned>(bytes: &[u8]) -> Result<T, StoreError> {
    serde_json::from_slice(bytes).map_err(backend_err)
}

/// redb-backed implementation of [`Store`].
pub struct RedbStore {
    db: Database,
}

impl RedbStore {
    /// Opens (creating if missing) the redb database at `path` and ensures all
    /// tables exist, writing `meta.schema_version` on first open.
    pub fn open(path: &Path) -> Result<Self, StoreError> {
        let db = Database::create(path).map_err(backend_err)?;

        let write_txn = db.begin_write().map_err(backend_err)?;
        {
            write_txn.open_table(FUNCTIONS_TABLE).map_err(backend_err)?;
            write_txn.open_table(REVISIONS_TABLE).map_err(backend_err)?;
            write_txn.open_table(BUILDS_TABLE).map_err(backend_err)?;
            write_txn.open_table(BINDINGS_TABLE).map_err(backend_err)?;

            let mut meta = write_txn.open_table(META_TABLE).map_err(backend_err)?;
            let already_set = meta.get(SCHEMA_VERSION_KEY).map_err(backend_err)?.is_some();
            if !already_set {
                meta.insert(SCHEMA_VERSION_KEY, SCHEMA_VERSION.as_bytes())
                    .map_err(backend_err)?;
            }
        }
        write_txn.commit().map_err(backend_err)?;

        Ok(Self { db })
    }

    /// Atomically switches a function's `current_revision` to `revision_number`,
    /// also bumping `updated_at`. This is the "atomic revision cutover" the data model
    /// describes: it happens in a single write transaction, not as a separate
    /// `get_function` + `put_function` pair.
    pub fn set_current_revision(&self, name: &str, revision_number: u32) -> Result<(), StoreError> {
        let write_txn = self.db.begin_write().map_err(backend_err)?;
        {
            let mut table = write_txn.open_table(FUNCTIONS_TABLE).map_err(backend_err)?;

            let existing: Option<Vec<u8>> = table
                .get(name)
                .map_err(backend_err)?
                .map(|guard| guard.value().to_vec());

            let bytes = existing.ok_or_else(|| StoreError::NotFound(name.to_string()))?;
            let mut function: Function = from_json(&bytes)?;
            function.current_revision = Some(revision_number);
            function.updated_at = chrono::Utc::now();

            let json = to_json(&function)?;
            table.insert(name, json.as_slice()).map_err(backend_err)?;
        }
        write_txn.commit().map_err(backend_err)?;

        Ok(())
    }
}

impl Store for RedbStore {
    fn get_function(&self, name: &str) -> Result<Option<Function>, StoreError> {
        let read_txn = self.db.begin_read().map_err(backend_err)?;
        let table = read_txn.open_table(FUNCTIONS_TABLE).map_err(backend_err)?;
        match table.get(name).map_err(backend_err)? {
            Some(guard) => Ok(Some(from_json(guard.value())?)),
            None => Ok(None),
        }
    }

    fn list_functions(&self) -> Result<Vec<Function>, StoreError> {
        let read_txn = self.db.begin_read().map_err(backend_err)?;
        let table = read_txn.open_table(FUNCTIONS_TABLE).map_err(backend_err)?;

        let mut functions = Vec::new();
        for entry in table.iter().map_err(backend_err)? {
            let (_key, value) = entry.map_err(backend_err)?;
            functions.push(from_json::<Function>(value.value())?);
        }
        functions.sort_by(|a, b| a.name.cmp(&b.name));

        Ok(functions)
    }

    fn put_function(&self, function: &Function) -> Result<(), StoreError> {
        let json = to_json(function)?;
        let write_txn = self.db.begin_write().map_err(backend_err)?;
        {
            let mut table = write_txn.open_table(FUNCTIONS_TABLE).map_err(backend_err)?;
            table
                .insert(function.name.as_str(), json.as_slice())
                .map_err(backend_err)?;
        }
        write_txn.commit().map_err(backend_err)?;

        Ok(())
    }

    fn delete_function(&self, name: &str) -> Result<(), StoreError> {
        let write_txn = self.db.begin_write().map_err(backend_err)?;
        {
            let mut table = write_txn.open_table(FUNCTIONS_TABLE).map_err(backend_err)?;
            table.remove(name).map_err(backend_err)?;
        }
        write_txn.commit().map_err(backend_err)?;

        Ok(())
    }

    fn put_revision(&self, revision: &Revision) -> Result<(), StoreError> {
        let key = revision_key(&revision.function_name, revision.number);
        let json = to_json(revision)?;
        let write_txn = self.db.begin_write().map_err(backend_err)?;
        {
            let mut table = write_txn.open_table(REVISIONS_TABLE).map_err(backend_err)?;
            table
                .insert(key.as_str(), json.as_slice())
                .map_err(backend_err)?;
        }
        write_txn.commit().map_err(backend_err)?;

        Ok(())
    }

    fn get_revision(&self, name: &str, number: u32) -> Result<Option<Revision>, StoreError> {
        let key = revision_key(name, number);
        let read_txn = self.db.begin_read().map_err(backend_err)?;
        let table = read_txn.open_table(REVISIONS_TABLE).map_err(backend_err)?;
        match table.get(key.as_str()).map_err(backend_err)? {
            Some(guard) => Ok(Some(from_json(guard.value())?)),
            None => Ok(None),
        }
    }

    fn put_build(&self, build: &Build) -> Result<(), StoreError> {
        let json = to_json(build)?;
        let write_txn = self.db.begin_write().map_err(backend_err)?;
        {
            let mut table = write_txn.open_table(BUILDS_TABLE).map_err(backend_err)?;
            table
                .insert(build.id.as_str(), json.as_slice())
                .map_err(backend_err)?;
        }
        write_txn.commit().map_err(backend_err)?;

        Ok(())
    }

    fn get_build(&self, id: &str) -> Result<Option<Build>, StoreError> {
        let read_txn = self.db.begin_read().map_err(backend_err)?;
        let table = read_txn.open_table(BUILDS_TABLE).map_err(backend_err)?;
        match table.get(id).map_err(backend_err)? {
            Some(guard) => Ok(Some(from_json(guard.value())?)),
            None => Ok(None),
        }
    }

    fn list_builds(&self) -> Result<Vec<Build>, StoreError> {
        let read_txn = self.db.begin_read().map_err(backend_err)?;
        let table = read_txn.open_table(BUILDS_TABLE).map_err(backend_err)?;

        let mut builds = Vec::new();
        for entry in table.iter().map_err(backend_err)? {
            let (_key, value) = entry.map_err(backend_err)?;
            builds.push(from_json::<Build>(value.value())?);
        }

        Ok(builds)
    }

    fn get_binding(&self, name: &str) -> Result<Option<TriggerBinding>, StoreError> {
        let read_txn = self.db.begin_read().map_err(backend_err)?;
        let table = read_txn.open_table(BINDINGS_TABLE).map_err(backend_err)?;
        match table.get(name).map_err(backend_err)? {
            Some(guard) => Ok(Some(from_json(guard.value())?)),
            None => Ok(None),
        }
    }

    fn put_binding(&self, binding: &TriggerBinding) -> Result<(), StoreError> {
        let json = to_json(binding)?;
        let write_txn = self.db.begin_write().map_err(backend_err)?;
        {
            let mut table = write_txn.open_table(BINDINGS_TABLE).map_err(backend_err)?;
            table
                .insert(binding.function_name.as_str(), json.as_slice())
                .map_err(backend_err)?;
        }
        write_txn.commit().map_err(backend_err)?;

        Ok(())
    }

    fn delete_binding(&self, name: &str) -> Result<(), StoreError> {
        let write_txn = self.db.begin_write().map_err(backend_err)?;
        {
            let mut table = write_txn.open_table(BINDINGS_TABLE).map_err(backend_err)?;
            table.remove(name).map_err(backend_err)?;
        }
        write_txn.commit().map_err(backend_err)?;

        Ok(())
    }

    fn list_bindings(&self) -> Result<Vec<TriggerBinding>, StoreError> {
        let read_txn = self.db.begin_read().map_err(backend_err)?;
        let table = read_txn.open_table(BINDINGS_TABLE).map_err(backend_err)?;

        let mut bindings = Vec::new();
        for entry in table.iter().map_err(backend_err)? {
            let (_key, value) = entry.map_err(backend_err)?;
            bindings.push(from_json::<TriggerBinding>(value.value())?);
        }

        Ok(bindings)
    }
}
