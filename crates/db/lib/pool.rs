//! Canonical SQLite connection builder shared by every microsandbox process.
//!
//! Both the host CLI and the in-VM runtime open the same SQLite file and
//! must apply identical PRAGMAs (WAL, busy timeout, foreign keys,
//! synchronous=NORMAL). Centralising the builder keeps that contract in one place — when a new PRAGMA is needed, this is the only file to edit.

use std::{path::Path, time::Duration};

use sea_orm::{DatabaseConnection, SqlxSqliteConnector};
use sqlx::{
    Connection,
    sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions, SqliteSynchronous},
};

use crate::connection::{DbReadConnection, DbWriteConnection};

//--------------------------------------------------------------------------------------------------
// Constants
//--------------------------------------------------------------------------------------------------

/// Default `busy_timeout` PRAGMA value used when a caller has no
/// user-facing knob to plumb (e.g. the in-VM runtime, where the host
/// owns DB-tuning policy and the runtime is not user-configurable).
pub const DEFAULT_BUSY_TIMEOUT_SECS: u64 = 5;

/// Read pool + dedicated single-connection write pool for the same SQLite
/// file. SQLite is single-writer system-wide, so a multi-connection pool
/// fighting for the writer lock just generates `SQLITE_BUSY` and (under
/// deferred transactions) `SQLITE_BUSY_SNAPSHOT`. Funnelling all writes
/// from one process through a single connection turns within-process
/// contention into an in-process queue (deterministic) instead of
/// SQLite-level lock contention (probabilistic, retry-required). Reads
/// keep the wider pool — WAL mode lets readers run concurrently.
#[derive(Debug)]
pub struct DbPools {
    read: DbReadConnection,
    write: DbWriteConnection,
}

impl DbPools {
    /// Open both pools for the SQLite file at `db_path` with shared PRAGMAs.
    ///
    /// The write pool connects first so WAL mode (persisted in the database
    /// header) is set before the read pool opens. `max_read_connections`
    /// sizes only the read pool; the write pool is always single-connection
    /// by design.
    pub async fn open(
        db_path: &Path,
        max_read_connections: u32,
        connect_timeout: Duration,
        busy_timeout: Duration,
    ) -> Result<Self, sqlx::Error> {
        let write = DbWriteConnection::open(db_path, connect_timeout, busy_timeout).await?;
        let read =
            DbReadConnection::open(db_path, max_read_connections, connect_timeout, busy_timeout)
                .await?;
        Ok(Self { read, write })
    }

    /// Borrow the read pool (multi-connection).
    pub fn read(&self) -> &DbReadConnection {
        &self.read
    }

    /// Borrow the write pool (single-connection, retries handled inside).
    pub fn write(&self) -> &DbWriteConnection {
        &self.write
    }
}

/// Open a sqlx-backed SQLite pool wrapped as a sea-orm `DatabaseConnection`.
///
/// PRAGMAs are applied to every connection in the pool via
/// `SqliteConnectOptions`, so callers don't need to issue any setup SQL.
///
/// `busy_timeout` is how long SQLite will spin internally on a contended
/// lock before returning `SQLITE_BUSY`. It interacts with the
/// application-level retry policy: a longer busy timeout reduces retry
/// volume at the cost of higher tail latency on contention.
///
/// `create_if_missing` controls whether opening a non-existent DB file creates
/// it. The write side (owned by `msb`) creates the catalog; read-only consumers
/// (e.g. `msb-metrics`) pass `false` so they never author or pre-empt it.
pub(crate) async fn build_pool(
    db_path: &Path,
    max_connections: u32,
    connect_timeout: Duration,
    busy_timeout: Duration,
    create_if_missing: bool,
) -> Result<DatabaseConnection, sqlx::Error> {
    let db_path = crate::private_storage::prepare_database_file(db_path, create_if_missing)
        .map_err(sqlx::Error::Io)?;

    // Held until the pool has connected: see `open_ahead`. Only the side that creates the
    // catalog switches it to WAL; a reader still waits for a locked catalog within the caller's
    // deadline.
    let ahead = if create_if_missing {
        Some(open_ahead(&db_path, busy_timeout).await?)
    } else {
        None
    };

    let connect_options = SqliteConnectOptions::new()
        .filename(&db_path)
        .create_if_missing(false)
        .journal_mode(SqliteJournalMode::Wal)
        .busy_timeout(busy_timeout)
        .foreign_keys(true)
        .synchronous(SqliteSynchronous::Normal);

    // Concurrent first opens can return BUSY while enabling WAL, even with SQLite's
    // busy timeout. Retry the entire connection setup within the caller's deadline.
    // A transient I/O error is retried around that, each attempt with a deadline of its own:
    // see `CONNECT_RETRY_DELAYS`.
    let pool = retry_transient_io(CONNECT_RETRY_DELAYS, || async {
        tokio::time::timeout(
            connect_timeout,
            crate::retry::retry_on_busy(|| {
                SqlitePoolOptions::new()
                    .max_connections(max_connections)
                    .acquire_timeout(connect_timeout)
                    .connect_with(connect_options.clone())
            }),
        )
        .await
        .map_err(|_| sqlx::Error::PoolTimedOut)?
    })
    .await?;
    if let Some(ahead) = ahead
        && let Err(error) = ahead.close().await
    {
        tracing::debug!(%error, "closing the connection opened ahead of the pool failed");
    }

    harden_database_files(&db_path).map_err(sqlx::Error::Io)?;

    Ok(SqlxSqliteConnector::from_sqlx_sqlite_pool(pool))
}

/// How long the first connection to a catalog may take before it is given up.
///
/// A bound for a stuck disk only. It is not a wait for a pool slot, so the caller's connect
/// timeout does not apply to it.
const OPEN_AHEAD_LIMIT: Duration = Duration::from_secs(600);

/// Opens one connection to the catalog before the pool does, outside the pool's connect timeout,
/// and returns it to be held until the pool has connected.
///
/// The first connection to a catalog is where the disk is touched: a new file is switched to WAL
/// mode, or the `-wal` and `-shm` files that the last closer removed are created again. Each of
/// these makes the file system commit its journal, which on a host whose disk is busy with other
/// writers can take tens of seconds (49 s measured with eight writers on a Linux host), far over
/// the pool's connect timeout. The open then failed with `pool timed out while waiting for an
/// open connection` although nothing was contended. This connection absorbs that wait under its
/// own limit; while it is held the pool finds the files in place and its connect takes
/// milliseconds, so the connect timeout again bounds only what its name says.
///
/// `sqlx` applies `journal_mode` before `synchronous`, so inside the pool the one-time switch to
/// WAL also ran with the default full sync; here the file is synchronised normally. WAL mode is
/// persistent in the file, so the switch happens only for a file that is not in it yet. The mode
/// is read through the connection: opening and closing the file by any other means in this
/// process would drop the locks SQLite holds on it. Concurrent first opens can return
/// `SQLITE_BUSY` from the switch, so it is retried as in the pool's own setup.
async fn open_ahead(
    db_path: &Path,
    busy_timeout: Duration,
) -> Result<sqlx::SqliteConnection, sqlx::Error> {
    let options = SqliteConnectOptions::new()
        .filename(db_path)
        .create_if_missing(false)
        .busy_timeout(busy_timeout)
        .synchronous(SqliteSynchronous::Normal);
    let open = retry_transient_io(CONNECT_RETRY_DELAYS, || {
        crate::retry::retry_on_busy(|| async {
            let mut conn = sqlx::SqliteConnection::connect_with(&options).await?;
            let mode: String = sqlx::query_scalar("PRAGMA journal_mode")
                .fetch_one(&mut conn)
                .await?;
            if !mode.eq_ignore_ascii_case("wal") {
                sqlx::query("PRAGMA journal_mode = WAL")
                    .execute(&mut conn)
                    .await?;
            }
            Ok(conn)
        })
    });
    tokio::time::timeout(OPEN_AHEAD_LIMIT, open)
        .await
        .map_err(|_| {
            sqlx::Error::Io(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                format!(
                    "opening {} took over {} s",
                    db_path.display(),
                    OPEN_AHEAD_LIMIT.as_secs()
                ),
            ))
        })?
}

/// Waits between attempts to connect when SQLite reports a transient I/O error, about 6.4 s in all.
///
/// Opening the catalog while another msb process that used it is exiting can fail with
/// `SQLITE_IOERR_TRUNCATE` ("disk I/O error", code 1546) on Windows. SQLite's busy handler does
/// not cover I/O errors, and the next attempt a moment later succeeds: puddle's Windows runs saw
/// one such failure per run and a single retry 500 ms later always resolved it. Most clear within
/// 0.25 s; the tail (1.6 s and 3.2 s) covers the rare longer event, such as one after a hard kill of
/// the runtime, and costs nothing unless an attempt fails.
pub(crate) const CONNECT_RETRY_DELAYS: &[Duration] = &[
    Duration::from_millis(25),
    Duration::from_millis(50),
    Duration::from_millis(100),
    Duration::from_millis(200),
    Duration::from_millis(400),
    Duration::from_millis(800),
    Duration::from_millis(1600),
    Duration::from_millis(3200),
];

/// SQLite's primary result code for I/O errors; extended codes carry it in their low byte.
const SQLITE_IOERR: i32 = 10;

/// Whether `err` is an I/O error reported by SQLite (`SQLITE_IOERR` or an extended code of it).
pub(crate) fn is_sqlite_io_error(err: &sqlx::Error) -> bool {
    let sqlx::Error::Database(db_err) = err else {
        return false;
    };
    db_err
        .code()
        .and_then(|code| code.parse::<i32>().ok())
        .is_some_and(|code| code & 0xff == SQLITE_IOERR)
}

/// Runs `connect`, trying again after each of `delays` while it fails with a SQLite I/O error.
/// Every other error, and the last I/O error once the delays are used up, is returned as is.
pub(crate) async fn retry_transient_io<F, Fut, T>(
    delays: &[Duration],
    mut connect: F,
) -> Result<T, sqlx::Error>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<T, sqlx::Error>>,
{
    let mut attempt = 0;
    loop {
        match connect().await {
            Err(err) if is_sqlite_io_error(&err) && attempt < delays.len() => {
                tracing::warn!(error = %err, attempt = attempt + 1, "SQLite I/O error while connecting, retrying");
                tokio::time::sleep(delays[attempt]).await;
                attempt += 1;
            }
            other => return other,
        }
    }
}

//--------------------------------------------------------------------------------------------------
// Tests
//--------------------------------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use std::{
        borrow::Cow,
        sync::atomic::{AtomicUsize, Ordering},
    };

    use sea_orm::{ConnectionTrait, DatabaseBackend, Statement};
    use sqlx::{
        ConnectOptions, Executor,
        error::{DatabaseError, ErrorKind},
    };

    use super::*;

    /// A driver error with a chosen SQLite result code.
    #[derive(Debug)]
    struct CodedError(&'static str);

    impl std::fmt::Display for CodedError {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(f, "(code: {}) test error", self.0)
        }
    }

    impl std::error::Error for CodedError {}

    impl DatabaseError for CodedError {
        fn message(&self) -> &str {
            "test error"
        }
        fn code(&self) -> Option<Cow<'_, str>> {
            Some(Cow::Borrowed(self.0))
        }
        fn as_error(&self) -> &(dyn std::error::Error + Send + Sync + 'static) {
            self
        }
        fn as_error_mut(&mut self) -> &mut (dyn std::error::Error + Send + Sync + 'static) {
            self
        }
        fn into_error(self: Box<Self>) -> Box<dyn std::error::Error + Send + Sync + 'static> {
            self
        }
        fn kind(&self) -> ErrorKind {
            ErrorKind::Other
        }
    }

    fn coded(code: &'static str) -> sqlx::Error {
        sqlx::Error::Database(Box::new(CodedError(code)))
    }

    const FAST: &[Duration] = &[Duration::from_millis(1); 3];

    #[test]
    fn io_errors_are_recognised_by_primary_and_extended_code() {
        // IOERR, IOERR_READ, IOERR_FSYNC, IOERR_TRUNCATE (what Windows reported), IOERR_SHMMAP.
        for code in ["10", "266", "1034", "1546", "5130"] {
            assert!(is_sqlite_io_error(&coded(code)), "{code}");
        }
        // ERROR, BUSY, BUSY_SNAPSHOT, CANTOPEN, a code that is not a number.
        for code in ["1", "5", "517", "14", "x"] {
            assert!(!is_sqlite_io_error(&coded(code)), "{code}");
        }
        assert!(!is_sqlite_io_error(&sqlx::Error::PoolTimedOut));
    }

    #[tokio::test]
    async fn a_transient_io_error_is_retried_until_it_clears() {
        let attempts = AtomicUsize::new(0);
        let result = retry_transient_io(FAST, || async {
            if attempts.fetch_add(1, Ordering::SeqCst) < 2 {
                Err(coded("1546"))
            } else {
                Ok("connected")
            }
        })
        .await;
        assert_eq!(result.unwrap(), "connected");
        assert_eq!(attempts.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn a_persistent_io_error_gives_up_after_the_last_delay() {
        let attempts = AtomicUsize::new(0);
        let result: Result<(), _> = retry_transient_io(FAST, || async {
            attempts.fetch_add(1, Ordering::SeqCst);
            Err(coded("1546"))
        })
        .await;
        assert!(is_sqlite_io_error(&result.unwrap_err()));
        assert_eq!(attempts.load(Ordering::SeqCst), FAST.len() + 1);
    }

    #[tokio::test]
    async fn other_errors_are_not_retried() {
        let attempts = AtomicUsize::new(0);
        let result: Result<(), _> = retry_transient_io(FAST, || async {
            attempts.fetch_add(1, Ordering::SeqCst);
            Err(coded("1"))
        })
        .await;
        assert!(result.is_err());
        assert_eq!(attempts.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn catalog_open_retries_busy_setup_within_connect_timeout() {
        let root = tempfile::tempdir().unwrap();
        let path = crate::private_storage::prepare_database_file(&root.path().join("msb.db"), true)
            .unwrap();
        let mut owner = SqliteConnectOptions::new()
            .filename(&path)
            .journal_mode(SqliteJournalMode::Delete)
            .connect()
            .await
            .unwrap();
        owner
            .execute(
                "CREATE TABLE retained(value); INSERT INTO retained VALUES (42); BEGIN EXCLUSIVE",
            )
            .await
            .unwrap();

        let error = build_pool(
            &path,
            1,
            Duration::from_millis(50),
            Duration::from_millis(1),
            false,
        )
        .await
        .unwrap_err();
        assert!(matches!(error, sqlx::Error::PoolTimedOut), "{error}");

        let opening = build_pool(
            &path,
            1,
            Duration::from_secs(5),
            Duration::from_millis(1),
            false,
        );
        tokio::pin!(opening);
        assert!(
            tokio::time::timeout(Duration::from_millis(50), &mut opening)
                .await
                .is_err(),
            "opening should wait while the existing catalog is locked"
        );

        owner.execute("COMMIT").await.unwrap();
        let pool = opening.await.unwrap();

        let row = pool
            .query_one_raw(Statement::from_string(
                DatabaseBackend::Sqlite,
                "SELECT value FROM retained",
            ))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(row.try_get_by_index::<i64>(0).unwrap(), 42);
    }
}

//--------------------------------------------------------------------------------------------------
// Re-Exports
//--------------------------------------------------------------------------------------------------

#[cfg(unix)]
pub use crate::private_storage::{PRIVATE_DATABASE_DIRECTORY_MODE, PRIVATE_DATABASE_FILE_MODE};
pub use crate::private_storage::{ensure_private_database_directory, harden_database_files};
