//! Read-only reads of the native state databases cones observes.
//!
//! These run in process against the bundled SQLite. Launching `sqlite3` once per index read,
//! per home, per refresh was one of the dashboard's recurring subprocess costs, and the
//! platform binary is not guaranteed to exist or to accept the flags either.
use anyhow::{Context, Result};
use rusqlite::{Connection, OpenFlags, types::ValueRef};
use serde_json::{Map, Number, Value};
use std::{ffi::OsStr, path::Path, time::Duration};

/// How long a read waits for a native writer holding the database.
const BUSY: Duration = Duration::from_millis(1000);

/// One read-only query, as rows of column name to value.
///
/// The read-only flag is the guarantee that observing a native database never creates,
/// migrates or writes one: SQLite refuses to open a missing file read-only and refuses to
/// run a schema migration on the connection. `path` may be a `file:` URI, which is how a
/// caller asks for an immutable snapshot of a checkpointed database.
pub fn query(path: &OsStr, sql: &str) -> Result<Vec<Value>> {
    crate::observe::read(
        crate::observe::op::SQLITE,
        || read(path, sql),
        Result::is_ok,
    )
}

fn read(path: &OsStr, sql: &str) -> Result<Vec<Value>> {
    let db = Connection::open_with_flags(
        Path::new(path),
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI,
    )
    .with_context(|| format!("opening {} read-only", Path::new(path).display()))?;
    db.busy_timeout(BUSY)?;
    let mut statement = db.prepare(sql)?;
    let names: Vec<String> = statement
        .column_names()
        .into_iter()
        .map(str::to_owned)
        .collect();
    let mut rows = statement.query([])?;
    let mut out = Vec::new();
    // One statement, one short read: never hold a transaction open across a refresh, which
    // would pin a snapshot while the native writer keeps appending to its WAL.
    while let Some(row) = rows.next()? {
        let mut object = Map::new();
        for (i, name) in names.iter().enumerate() {
            object.insert(name.clone(), json(row.get_ref(i)?));
        }
        out.push(Value::Object(object));
    }
    Ok(out)
}

fn json(value: ValueRef<'_>) -> Value {
    match value {
        ValueRef::Null => Value::Null,
        ValueRef::Integer(i) => Value::from(i),
        ValueRef::Real(f) => Number::from_f64(f).map_or(Value::Null, Value::Number),
        ValueRef::Text(t) => Value::from(String::from_utf8_lossy(t).into_owned()),
        // No native column cones reads is a blob; a value it cannot name is absent, not empty.
        ValueRef::Blob(_) => Value::Null,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(path: &Path, sql: &str) {
        let db = Connection::open(path).unwrap();
        db.execute_batch(sql).unwrap();
    }

    #[test]
    fn a_read_returns_named_columns_and_never_creates_or_writes_a_database() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.sqlite");
        write(
            &path,
            "create table threads(id text, name text, n integer, r real, absent text);
             insert into threads values('a', 'Green CI', 4, 1.5, null);",
        );
        let rows = query(path.as_os_str(), "select * from threads").unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["id"], Value::from("a"));
        assert_eq!(rows[0]["name"], Value::from("Green CI"));
        assert_eq!(rows[0]["n"], Value::from(4));
        assert_eq!(rows[0]["r"], Value::from(1.5));
        assert_eq!(rows[0]["absent"], Value::Null);

        let missing = dir.path().join("absent.sqlite");
        assert!(
            query(missing.as_os_str(), "select 1").is_err(),
            "a missing database is an unreadable source, not an empty one"
        );
        assert!(!missing.exists(), "a read must not create a database");

        assert!(
            query(path.as_os_str(), "create table t(x)").is_err(),
            "a read-only connection refuses to write"
        );
        assert!(
            query(path.as_os_str(), "select * from absent_table").is_err(),
            "a missing table is an error the caller can distinguish"
        );
    }

    #[test]
    fn a_checkpointed_wal_database_reads_through_an_immutable_uri() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.sqlite");
        {
            let db = Connection::open(&path).unwrap();
            db.pragma_update(None, "journal_mode", "wal").unwrap();
            db.execute_batch("create table t(x text); insert into t values('kept');")
                .unwrap();
        }
        // The closing connection checkpoints and removes the WAL, which is the case the
        // dashboard meets for an OpenCode database whose client has exited.
        assert!(!path.with_extension("sqlite-wal").exists());
        let uri = format!("file:{}?immutable=1", path.display());
        let rows = query(OsStr::new(&uri), "select x from t").unwrap();
        assert_eq!(rows[0]["x"], Value::from("kept"));
    }

    #[test]
    fn a_read_is_counted_once_and_a_failure_is_counted_as_a_failure() {
        crate::observe::reset();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.sqlite");
        write(&path, "create table t(x); insert into t values(1);");
        query(path.as_os_str(), "select * from t").unwrap();
        let _ = query(dir.path().join("absent.sqlite").as_os_str(), "select 1");
        let counts = crate::observe::snapshot()[crate::observe::op::SQLITE];
        assert_eq!((counts.reads, counts.failures, counts.spawns), (2, 1, 0));
        crate::observe::reset();
    }
}
