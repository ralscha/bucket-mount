//! Durable sync baselines and LRU metadata; never store object contents or credentials here.
use crate::remote::RemoteObject;
use anyhow::{Context, Result, ensure};
use rusqlite::{Connection, OptionalExtension, params};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path,
    sync::Mutex,
};

pub struct State {
    db: Mutex<Connection>,
    touched: Mutex<BTreeSet<String>>,
}
impl State {
    pub fn open(path: &Path, binding: &str) -> Result<Self> {
        let db = Connection::open(path).context("open sync database")?;
        db.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL;
            CREATE TABLE IF NOT EXISTS settings (key TEXT PRIMARY KEY, value TEXT NOT NULL);
            CREATE TABLE IF NOT EXISTS objects (key TEXT PRIMARY KEY, etag TEXT NOT NULL, size INTEGER NOT NULL, modified_ms INTEGER NOT NULL, accessed_ms INTEGER NOT NULL);")?;
        let stored: Option<String> = db
            .query_row("SELECT value FROM settings WHERE key='binding'", [], |r| {
                r.get(0)
            })
            .optional()?;
        if let Some(stored) = stored {
            ensure!(
                stored == binding,
                "state directory belongs to a different bucket, prefix, endpoint or mount; use a new state directory"
            );
        } else {
            db.execute(
                "INSERT INTO settings(key,value) VALUES('binding',?1)",
                [binding],
            )?;
        }
        Ok(Self {
            db: Mutex::new(db),
            touched: Mutex::new(BTreeSet::new()),
        })
    }
    pub fn all(&self) -> Result<BTreeMap<String, RemoteObject>> {
        let db = self.db.lock().unwrap_or_else(|p| p.into_inner());
        let mut query = db.prepare("SELECT key,etag,size,modified_ms FROM objects ORDER BY key")?;
        let rows = query.query_map([], |r| {
            let size: i64 = r.get(2)?;
            let size = u64::try_from(size).map_err(|e| {
                rusqlite::Error::FromSqlConversionFailure(
                    2,
                    rusqlite::types::Type::Integer,
                    Box::new(e),
                )
            })?;
            Ok(RemoteObject {
                key: r.get(0)?,
                etag: r.get(1)?,
                size,
                modified_ms: r.get(3)?,
            })
        })?;
        let mut objects = BTreeMap::new();
        for object in rows {
            let object = object?;
            objects.insert(object.key.clone(), object);
        }
        Ok(objects)
    }
    pub fn bind(&self, key: &str, value: &str) -> Result<()> {
        let db = self.db.lock().unwrap_or_else(|p| p.into_inner());
        let stored: Option<String> = db
            .query_row("SELECT value FROM settings WHERE key=?1", [key], |r| {
                r.get(0)
            })
            .optional()?;
        if let Some(stored) = stored {
            ensure!(
                stored == value,
                "{key} changed; sync stopped to prevent unintended deletions"
            );
        } else {
            db.execute(
                "INSERT INTO settings(key,value) VALUES(?1,?2)",
                params![key, value],
            )?;
        }
        Ok(())
    }
    pub fn save(&self, object: &RemoteObject) -> Result<()> {
        let size = i64::try_from(object.size).context("object exceeds supported size")?;
        self.db.lock().unwrap_or_else(|p| p.into_inner()).execute("INSERT INTO objects(key,etag,size,modified_ms,accessed_ms) VALUES(?1,?2,?3,?4,?5) ON CONFLICT(key) DO UPDATE SET etag=excluded.etag,size=excluded.size,modified_ms=excluded.modified_ms", params![object.key,object.etag,size,object.modified_ms,now_ms()])?;
        Ok(())
    }
    pub fn remove(&self, key: &str) -> Result<()> {
        self.db
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .execute("DELETE FROM objects WHERE key=?1", [key])?;
        Ok(())
    }
    pub fn touch(&self, key: &str) -> Result<()> {
        self.touched
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .insert(key.to_owned());
        Ok(())
    }
    pub fn lru(&self) -> Result<Vec<String>> {
        let touched = std::mem::take(&mut *self.touched.lock().unwrap_or_else(|p| p.into_inner()));
        let mut db = self.db.lock().unwrap_or_else(|p| p.into_inner());
        if !touched.is_empty() {
            let transaction = db.transaction()?;
            let now = now_ms();
            for key in touched {
                transaction.execute(
                    "UPDATE objects SET accessed_ms=?2 WHERE key=?1",
                    params![key, now],
                )?;
            }
            transaction.commit()?;
        }
        let mut q = db.prepare("SELECT key FROM objects ORDER BY accessed_ms,key")?;
        Ok(q.query_map([], |r| r.get(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?)
    }
}
pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(i64::MAX as u128) as i64
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn baseline_survives_restart_and_rejects_other_mount() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("state.db");
        let s = State::open(&p, "bucket-a").unwrap();
        let o = RemoteObject {
            key: "f".into(),
            etag: "v1".into(),
            size: 12,
            modified_ms: 5,
        };
        s.save(&o).unwrap();
        drop(s);
        let s = State::open(&p, "bucket-a").unwrap();
        assert_eq!(s.all().unwrap()["f"], o);
        assert!(State::open(&p, "bucket-b").is_err());
        s.remove("f").unwrap();
        assert!(s.all().unwrap().is_empty());
    }
}
