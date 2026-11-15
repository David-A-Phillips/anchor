//! Shared SQLite handle.
//!
//! One [`Db`] is shared by every module that persists state (graph store, embeddings, token ledger,
//! backlog, sync). It owns a single write connection and a small pool of read-only connections on a
//! WAL-mode database, so readers never block the writer and vice versa.
//!
//! # Rules for callers
//!
//! * All methods are **blocking**. From async code use the `*_async` variants (which run on Tokio's
//!   blocking pool) or call from a dedicated thread.
//! * Never call a `Db` method from inside a closure passed to another `Db` method: the in-memory
//!   variant serves reads and writes from one connection behind one mutex and would deadlock.
//! * Keep closures short. A write transaction holds the single writer.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use parking_lot::Mutex;
use rusqlite::{Connection, OpenFlags, Transaction, TransactionBehavior, params};

use super::time::now_ms;
use super::{Error, Result};

/// One schema change of one module.
#[derive(Clone, Copy, Debug)]
pub struct Migration {
    /// Version within the module; versions start at 1 and are contiguous.
    pub version: u32,
    /// Short description, recorded in the `anchor_migrations` table.
    pub name: &'static str,
    /// SQL executed inside a transaction.
    pub sql: &'static str,
}

/// The ordered schema history of one module. Each module owns its tables and declares them here.
#[derive(Clone, Copy, Debug)]
pub struct MigrationSet {
    /// Module name; the namespace of the version numbers.
    pub module: &'static str,
    /// Migrations in ascending version order.
    pub migrations: &'static [Migration],
}

struct Inner {
    path: Option<PathBuf>,
    writer: Mutex<Connection>,
    readers: Vec<Mutex<Connection>>,
    next_reader: AtomicUsize,
}

/// Cheaply cloneable handle to the Anchor database.
#[derive(Clone)]
pub struct Db {
    inner: Arc<Inner>,
}

impl std::fmt::Debug for Db {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Db")
            .field("path", &self.inner.path)
            .field("readers", &self.inner.readers.len())
            .finish()
    }
}

const WRITER_PRAGMAS: &str = "
    PRAGMA journal_mode = WAL;
    PRAGMA synchronous = NORMAL;
    PRAGMA foreign_keys = ON;
    PRAGMA temp_store = MEMORY;
    PRAGMA cache_size = -65536;
    PRAGMA wal_autocheckpoint = 2000;
";

const READER_PRAGMAS: &str = "
    PRAGMA foreign_keys = ON;
    PRAGMA temp_store = MEMORY;
    PRAGMA cache_size = -32768;
    PRAGMA query_only = ON;
";

const MEMORY_PRAGMAS: &str = "
    PRAGMA foreign_keys = ON;
    PRAGMA temp_store = MEMORY;
";

impl Db {
    /// Opens (creating if necessary) the database at `path` and applies every pending migration of
    /// every module registered in [`crate::schema::ALL`].
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        Self::open_with(path, crate::schema::ALL)
    }

    /// Like [`Self::open`] with an explicit set of module schemas (useful for module-level tests).
    pub fn open_with(path: impl AsRef<Path>, schemas: &[MigrationSet]) -> Result<Self> {
        let path = path.as_ref();
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent)?;
        }

        let mut writer = Connection::open(path)?;
        writer.busy_timeout(Duration::from_secs(10))?;
        writer.execute_batch(WRITER_PRAGMAS)?;
        writer.set_prepared_statement_cache_capacity(256);
        migrate(&mut writer, schemas)?;

        let reader_count = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(2)
            .clamp(2, 8);
        let mut readers = Vec::with_capacity(reader_count);
        for _ in 0..reader_count {
            let reader = Connection::open_with_flags(
                path,
                OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
            )?;
            reader.busy_timeout(Duration::from_secs(10))?;
            reader.execute_batch(READER_PRAGMAS)?;
            reader.set_prepared_statement_cache_capacity(128);
            readers.push(Mutex::new(reader));
        }

        Ok(Self {
            inner: Arc::new(Inner {
                path: Some(path.to_path_buf()),
                writer: Mutex::new(writer),
                readers,
                next_reader: AtomicUsize::new(0),
            }),
        })
    }

    /// Opens a private in-memory database with every registered module schema applied.
    pub fn open_in_memory() -> Result<Self> {
        Self::open_in_memory_with(crate::schema::ALL)
    }

    /// Like [`Self::open_in_memory`] with an explicit set of module schemas.
    pub fn open_in_memory_with(schemas: &[MigrationSet]) -> Result<Self> {
        let mut writer = Connection::open_in_memory()?;
        writer.execute_batch(MEMORY_PRAGMAS)?;
        writer.set_prepared_statement_cache_capacity(256);
        migrate(&mut writer, schemas)?;
        Ok(Self {
            inner: Arc::new(Inner {
                path: None,
                writer: Mutex::new(writer),
                readers: Vec::new(),
                next_reader: AtomicUsize::new(0),
            }),
        })
    }

    /// Runs a read-only closure on a pooled reader connection.
    pub fn read<T>(&self, f: impl FnOnce(&Connection) -> Result<T>) -> Result<T> {
        let readers = &self.inner.readers;
        if readers.is_empty() {
            let conn = self.inner.writer.lock();
            return f(&conn);
        }
        let start = self.inner.next_reader.fetch_add(1, Ordering::Relaxed);
        for offset in 0..readers.len() {
            if let Some(conn) = readers[(start + offset) % readers.len()].try_lock() {
                return f(&conn);
            }
        }
        let conn = readers[start % readers.len()].lock();
        f(&conn)
    }

    /// Runs a closure on the write connection without an explicit transaction.
    pub fn write<T>(&self, f: impl FnOnce(&mut Connection) -> Result<T>) -> Result<T> {
        let mut conn = self.inner.writer.lock();
        f(&mut conn)
    }

    /// Runs a closure inside an `IMMEDIATE` transaction on the write connection. The transaction
    /// commits when the closure returns `Ok` and rolls back when it returns `Err` or panics.
    pub fn transaction<T>(&self, f: impl FnOnce(&Transaction<'_>) -> Result<T>) -> Result<T> {
        let mut conn = self.inner.writer.lock();
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let value = f(&tx)?;
        tx.commit()?;
        Ok(value)
    }

    /// [`Self::read`] on Tokio's blocking pool.
    pub async fn read_async<T, F>(&self, f: F) -> Result<T>
    where
        T: Send + 'static,
        F: FnOnce(&Connection) -> Result<T> + Send + 'static,
    {
        let db = self.clone();
        tokio::task::spawn_blocking(move || db.read(f)).await?
    }

    /// [`Self::write`] on Tokio's blocking pool.
    pub async fn write_async<T, F>(&self, f: F) -> Result<T>
    where
        T: Send + 'static,
        F: FnOnce(&mut Connection) -> Result<T> + Send + 'static,
    {
        let db = self.clone();
        tokio::task::spawn_blocking(move || db.write(f)).await?
    }

    /// [`Self::transaction`] on Tokio's blocking pool.
    pub async fn transaction_async<T, F>(&self, f: F) -> Result<T>
    where
        T: Send + 'static,
        F: FnOnce(&Transaction<'_>) -> Result<T> + Send + 'static,
    {
        let db = self.clone();
        tokio::task::spawn_blocking(move || db.transaction(f)).await?
    }

    /// Path of the database file; `None` for in-memory databases.
    pub fn path(&self) -> Option<&Path> {
        self.inner.path.as_deref()
    }

    /// Bytes used on disk by the database file plus its write-ahead log; `0` for in-memory.
    pub fn size_on_disk(&self) -> Result<u64> {
        let Some(path) = self.path() else {
            return Ok(0);
        };
        let mut total = std::fs::metadata(path).map(|meta| meta.len()).unwrap_or(0);
        let mut wal = path.as_os_str().to_owned();
        wal.push("-wal");
        total += std::fs::metadata(PathBuf::from(wal))
            .map(|meta| meta.len())
            .unwrap_or(0);
        Ok(total)
    }

    /// Folds the write-ahead log back into the main database file and truncates it.
    pub fn checkpoint(&self) -> Result<()> {
        if self.path().is_none() {
            return Ok(());
        }
        self.write(|conn| {
            conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);")?;
            Ok(())
        })
    }

    /// Rebuilds the database file, reclaiming free pages. Expensive; never on a hot path.
    pub fn vacuum(&self) -> Result<()> {
        self.write(|conn| {
            conn.execute_batch("VACUUM;")?;
            Ok(())
        })?;
        self.checkpoint()
    }

    /// Schema version currently applied for `module`; `0` when the module has no tables yet.
    pub fn schema_version(&self, module: &str) -> Result<u32> {
        self.read(|conn| current_version(conn, module))
    }
}

fn current_version(conn: &Connection, module: &str) -> Result<u32> {
    let version: u32 = conn.query_row(
        "SELECT COALESCE(MAX(version), 0) FROM anchor_migrations WHERE module = ?1",
        [module],
        |row| row.get(0),
    )?;
    Ok(version)
}

fn migrate(conn: &mut Connection, schemas: &[MigrationSet]) -> Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS anchor_migrations (
             module     TEXT    NOT NULL,
             version    INTEGER NOT NULL,
             name       TEXT    NOT NULL,
             applied_at INTEGER NOT NULL,
             PRIMARY KEY (module, version)
         ) WITHOUT ROWID;",
    )?;

    for set in schemas {
        let mut applied = current_version(conn, set.module)?;
        for migration in set.migrations {
            if migration.version <= applied {
                continue;
            }
            if migration.version != applied + 1 {
                return Err(Error::Config(format!(
                    "schema history of module `{}` is not contiguous: expected version {}, found {}",
                    set.module,
                    applied + 1,
                    migration.version
                )));
            }
            let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
            tx.execute_batch(migration.sql).map_err(|err| {
                Error::Config(format!(
                    "migration `{}` v{} ({}) failed: {err}",
                    set.module, migration.version, migration.name
                ))
            })?;
            tx.execute(
                "INSERT INTO anchor_migrations (module, version, name, applied_at)
                 VALUES (?1, ?2, ?3, ?4)",
                params![set.module, migration.version, migration.name, now_ms()],
            )?;
            tx.commit()?;
            applied = migration.version;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const V1: &[Migration] = &[Migration {
        version: 1,
        name: "create things",
        sql: "CREATE TABLE things (id INTEGER PRIMARY KEY, name TEXT NOT NULL);",
    }];

    const V2: &[Migration] = &[
        Migration {
            version: 1,
            name: "create things",
            sql: "CREATE TABLE things (id INTEGER PRIMARY KEY, name TEXT NOT NULL);",
        },
        Migration {
            version: 2,
            name: "add colour",
            sql: "ALTER TABLE things ADD COLUMN colour TEXT;",
        },
    ];

    const GAP: &[Migration] = &[Migration {
        version: 3,
        name: "skips ahead",
        sql: "CREATE TABLE never (id INTEGER);",
    }];

    fn set(migrations: &'static [Migration]) -> [MigrationSet; 1] {
        [MigrationSet {
            module: "test",
            migrations,
        }]
    }

    fn count(db: &Db) -> i64 {
        db.read(|conn| Ok(conn.query_row("SELECT COUNT(*) FROM things", [], |row| row.get(0))?))
            .expect("count")
    }

    #[test]
    fn file_database_migrates_incrementally_and_reads_its_own_writes() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("nested/anchor.db");

        let db = Db::open_with(&path, &set(V1)).expect("open v1");
        assert_eq!(db.schema_version("test").expect("version"), 1);
        db.transaction(|tx| {
            tx.execute("INSERT INTO things (name) VALUES ('a'), ('b')", [])?;
            Ok(())
        })
        .expect("insert");
        assert_eq!(count(&db), 2);
        drop(db);

        let db = Db::open_with(&path, &set(V2)).expect("open v2");
        assert_eq!(db.schema_version("test").expect("version"), 2);
        assert_eq!(count(&db), 2);
        db.write(|conn| {
            conn.execute("UPDATE things SET colour = 'red'", [])?;
            Ok(())
        })
        .expect("update");
        assert!(db.size_on_disk().expect("size") > 0);
        db.checkpoint().expect("checkpoint");
        db.vacuum().expect("vacuum");
    }

    #[test]
    fn failed_transaction_rolls_back() {
        let db = Db::open_in_memory_with(&set(V1)).expect("open");
        let result: Result<()> = db.transaction(|tx| {
            tx.execute("INSERT INTO things (name) VALUES ('kept?')", [])?;
            Err(Error::other("boom"))
        });
        assert!(result.is_err());
        assert_eq!(count(&db), 0);
    }

    #[test]
    fn readers_cannot_write() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db = Db::open_with(dir.path().join("anchor.db"), &set(V1)).expect("open");
        let result = db.read(|conn| {
            conn.execute("INSERT INTO things (name) VALUES ('nope')", [])?;
            Ok(())
        });
        assert!(result.is_err());
    }

    #[test]
    fn non_contiguous_history_is_rejected() {
        assert!(Db::open_in_memory_with(&set(GAP)).is_err());
    }

    #[test]
    fn registered_module_schemas_apply_cleanly() {
        let db = Db::open_in_memory().expect("open with every module schema");
        assert_eq!(db.size_on_disk().expect("size"), 0);
        assert!(db.path().is_none());
    }

    #[tokio::test]
    async fn async_helpers_run_on_the_blocking_pool() {
        let db = Db::open_in_memory_with(&set(V1)).expect("open");
        db.transaction_async(|tx| {
            tx.execute("INSERT INTO things (name) VALUES ('x')", [])?;
            Ok(())
        })
        .await
        .expect("insert");
        let names: Vec<String> = db
            .read_async(|conn| {
                let mut statement = conn.prepare("SELECT name FROM things")?;
                let rows = statement.query_map([], |row| row.get(0))?;
                Ok(rows.collect::<std::result::Result<Vec<String>, _>>()?)
            })
            .await
            .expect("read");
        assert_eq!(names, vec!["x"]);
    }
}
