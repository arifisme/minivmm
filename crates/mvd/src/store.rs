use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use mv_proto::api::DeployConfig;
use rusqlite::{Connection, OptionalExtension, params};

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS functions (
    id INTEGER PRIMARY KEY,
    name TEXT NOT NULL UNIQUE,
    revision INTEGER NOT NULL,
    config TEXT NOT NULL,
    code_sha256 TEXT NOT NULL,
    code_size INTEGER NOT NULL,
    init_duration_ms REAL NOT NULL,
    rootfs_identity TEXT NOT NULL,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS counters (
    name TEXT PRIMARY KEY,
    value INTEGER NOT NULL
);
";

#[derive(Clone, Debug, PartialEq)]
pub struct FunctionRecord {
    pub id: i64,
    pub name: String,
    pub revision: u64,
    pub config: DeployConfig,
    pub code_sha256: String,
    pub code_size: u64,
    pub init_duration_ms: f64,
    pub rootfs_identity: String,
    pub created_at: String,
    pub updated_at: String,
}

pub struct Store {
    data_dir: PathBuf,
    db: Mutex<Connection>,
}

pub const CODE_FILE: &str = "code.tar.gz";
pub const STATE_FILE: &str = "snapshot.state";
pub const MEMORY_FILE: &str = "snapshot.mem";

impl Store {
    pub fn open(data_dir: &Path) -> anyhow::Result<Self> {
        fs::create_dir_all(data_dir.join("functions"))?;
        let db = Connection::open(data_dir.join("mvd.db"))?;
        db.pragma_update(None, "journal_mode", "WAL")?;
        db.pragma_update(None, "synchronous", "FULL")?;
        db.execute_batch(SCHEMA)?;
        Ok(Self {
            data_dir: data_dir.to_path_buf(),
            db: Mutex::new(db),
        })
    }

    fn db(&self) -> std::sync::MutexGuard<'_, Connection> {
        self.db.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn function_dir(&self, id: i64) -> PathBuf {
        self.data_dir.join("functions").join(id.to_string())
    }

    pub fn revision_dir(&self, id: i64, revision: u64) -> PathBuf {
        self.function_dir(id).join(format!("rev-{revision}"))
    }

    pub fn allocate_id(&self) -> anyhow::Result<i64> {
        let db = self.db();
        let id: i64 = db.query_row(
            "INSERT INTO counters (name, value) VALUES ('function_id', 1)
             ON CONFLICT(name) DO UPDATE SET value = value + 1
             RETURNING value",
            [],
            |row| row.get(0),
        )?;
        Ok(id)
    }

    pub fn list(&self) -> anyhow::Result<Vec<FunctionRecord>> {
        let db = self.db();
        let mut statement = db.prepare(
            "SELECT id, name, revision, config, code_sha256, code_size, init_duration_ms,
                    rootfs_identity, created_at, updated_at
             FROM functions ORDER BY name",
        )?;
        let rows = statement.query_map([], row_to_record)?;
        let mut records = Vec::new();
        for row in rows {
            records.push(row??);
        }
        Ok(records)
    }

    pub fn get(&self, name: &str) -> anyhow::Result<Option<FunctionRecord>> {
        let db = self.db();
        let record = db
            .query_row(
                "SELECT id, name, revision, config, code_sha256, code_size, init_duration_ms,
                        rootfs_identity, created_at, updated_at
                 FROM functions WHERE name = ?1",
                [name],
                row_to_record,
            )
            .optional()?;
        record.transpose()
    }

    pub fn save(&self, record: &FunctionRecord) -> anyhow::Result<()> {
        let config = serde_json::to_string(&record.config)?;
        self.db().execute(
            "INSERT INTO functions (id, name, revision, config, code_sha256, code_size,
                                    init_duration_ms, rootfs_identity, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
             ON CONFLICT(name) DO UPDATE SET
                revision = excluded.revision, config = excluded.config,
                code_sha256 = excluded.code_sha256, code_size = excluded.code_size,
                init_duration_ms = excluded.init_duration_ms,
                rootfs_identity = excluded.rootfs_identity, updated_at = excluded.updated_at",
            params![
                record.id,
                record.name,
                record.revision as i64,
                config,
                record.code_sha256,
                record.code_size as i64,
                record.init_duration_ms,
                record.rootfs_identity,
                record.created_at,
                record.updated_at,
            ],
        )?;
        Ok(())
    }

    pub fn delete(&self, name: &str) -> anyhow::Result<bool> {
        Ok(self
            .db()
            .execute("DELETE FROM functions WHERE name = ?1", [name])?
            > 0)
    }
}

fn row_to_record(row: &rusqlite::Row<'_>) -> rusqlite::Result<anyhow::Result<FunctionRecord>> {
    let config: String = row.get(3)?;
    let config = match serde_json::from_str(&config) {
        Ok(config) => config,
        Err(err) => {
            return Ok(Err(anyhow::anyhow!(
                "stored configuration is invalid: {err}"
            )));
        }
    };
    let revision: i64 = row.get(2)?;
    let code_size: i64 = row.get(5)?;
    Ok(Ok(FunctionRecord {
        id: row.get(0)?,
        name: row.get(1)?,
        revision: revision as u64,
        config,
        code_sha256: row.get(4)?,
        code_size: code_size as u64,
        init_duration_ms: row.get(6)?,
        rootfs_identity: row.get(7)?,
        created_at: row.get(8)?,
        updated_at: row.get(9)?,
    }))
}

#[cfg(test)]
mod tests {
    use mv_proto::agent::Runtime;

    use super::*;

    fn record(store: &Store, name: &str) -> FunctionRecord {
        FunctionRecord {
            id: store.allocate_id().unwrap(),
            name: name.into(),
            revision: 1,
            config: serde_json::from_str(r#"{"runtime":"python","env":{"A":"b"}}"#).unwrap(),
            code_sha256: "abc".into(),
            code_size: 10,
            init_duration_ms: 12.5,
            rootfs_identity: "1-2".into(),
            created_at: "2026-01-01T00:00:00Z".into(),
            updated_at: "2026-01-01T00:00:00Z".into(),
        }
    }

    #[test]
    fn records_survive_reopening() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        let mut first = record(&store, "alpha");
        store.save(&first).unwrap();
        store.save(&record(&store, "beta")).unwrap();
        first.revision = 2;
        first.updated_at = "2026-02-01T00:00:00Z".into();
        store.save(&first).unwrap();
        drop(store);

        let store = Store::open(dir.path()).unwrap();
        let listed = store.list().unwrap();
        assert_eq!(listed.len(), 2);
        assert_eq!(listed[0], first);
        assert_eq!(listed[0].config.runtime, Runtime::Python);
        assert_eq!(store.get("beta").unwrap().unwrap().id, 2);
        assert!(store.delete("beta").unwrap());
        assert!(!store.delete("beta").unwrap());
        assert!(store.get("beta").unwrap().is_none());
        assert_eq!(store.allocate_id().unwrap(), 3, "ids are never reused");
        assert_eq!(
            store.revision_dir(4, 2),
            dir.path().join("functions/4/rev-2")
        );
    }
}
