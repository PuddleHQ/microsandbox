//! Opening the catalog while another process that used it is exiting.
//!
//! On Windows an exiting process can still hold the database's lock ranges and
//! WAL index for a moment after the process object is signalled, and SQLite
//! reports that to an opener as a plain "disk I/O error" (extended code 1546 or
//! similar), which neither its busy handler nor the busy retry in this crate
//! treats as transient. This test spawns short-lived writer processes and opens
//! the catalog from the parent while they exit and straight after.
//!
//! The child is this test binary re-run with `MSB_DB_EXIT_CHILD=<db path>`.

use std::{
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};

use microsandbox_db::pool::DbPools;
use sea_orm::ConnectionTrait;

const TIMEOUT: Duration = Duration::from_secs(5);
/// Big enough rows that the writers and the parent fill the WAL past SQLite's automatic
/// checkpoint threshold, which also truncates the database file.
const PAYLOAD_BYTES: usize = 4096;
const CHILD_ROWS: usize = 40;
const PARENT_ROWS: usize = 100;
const OPENERS: usize = 4;
const CHILD_ENV: &str = "MSB_DB_EXIT_CHILD";
const CHILD_MODE_ENV: &str = "MSB_DB_EXIT_CHILD_MODE";

/// Entry point of the child process; a no-op in the parent's test run.
#[test]
fn child_entry() {
    let Ok(path) = std::env::var(CHILD_ENV) else {
        return;
    };
    let mode = std::env::var(CHILD_MODE_ENV).unwrap_or_default();
    let graceful = mode == "graceful";
    let payload = "x".repeat(PAYLOAD_BYTES);
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async {
        let pools = DbPools::open(Path::new(&path), 2, TIMEOUT, TIMEOUT)
            .await
            .expect("child open");
        let insert = format!("INSERT INTO t (v) VALUES ('{payload}')");
        if mode == "kill" {
            // Write until the parent terminates this process.
            loop {
                pools
                    .write()
                    .execute_unprepared(&insert)
                    .await
                    .expect("child write");
            }
        }
        for _ in 0..CHILD_ROWS {
            pools
                .write()
                .execute_unprepared(&insert)
                .await
                .expect("child write");
        }
        if graceful {
            // Close for real: the last close checkpoints, truncates the database file and
            // deletes the WAL and its index. Dropping a pool only schedules this.
            pools
                .write()
                .inner()
                .clone()
                .close()
                .await
                .expect("close writer");
            pools
                .read()
                .inner()
                .clone()
                .close()
                .await
                .expect("close reader");
        } else {
            // What msb's sandbox process does: leave with the pool still open.
            std::process::exit(0);
        }
    });
}

fn spawn_child(path: &Path, mode: &str) -> Child {
    Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "child_entry", "--nocapture", "--test-threads=1"])
        .env(CHILD_ENV, path)
        .env(CHILD_MODE_ENV, mode)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap()
}

/// Opens both pools and reads, as `LocalBackend::db` does, returning the error text.
async fn open_and_read(path: &Path) -> Result<(), String> {
    let pools = DbPools::open(path, 2, TIMEOUT, TIMEOUT)
        .await
        .map_err(|e| format!("open: {e}"))?;
    pools
        .read()
        .query_one_raw(sea_orm::Statement::from_string(
            sea_orm::DbBackend::Sqlite,
            "SELECT COUNT(*) FROM t",
        ))
        .await
        .map_err(|e| format!("read: {e}"))?;
    // A write transaction large enough to force a checkpoint from this process.
    let payload = "y".repeat(PAYLOAD_BYTES);
    pools
        .write()
        .transaction::<_, _, _, sea_orm::DbErr>(|txn| {
            let payload = payload.clone();
            async move {
                for _ in 0..PARENT_ROWS {
                    txn.execute_unprepared(&format!("INSERT INTO p (v) VALUES ('{payload}')"))
                        .await?;
                }
                Ok((txn, ()))
            }
        })
        .await
        .map_err(|e| format!("write: {e}"))?;
    // Close for real so this process also checkpoints and truncates when it is the last one out.
    pools
        .write()
        .inner()
        .clone()
        .close()
        .await
        .map_err(|e| format!("close: {e}"))?;
    pools
        .read()
        .inner()
        .clone()
        .close()
        .await
        .map_err(|e| format!("close: {e}"))?;
    Ok(())
}

async fn run(mode: &str, rounds: usize, parallel: usize) -> Vec<String> {
    let dir = tempfile::tempdir().unwrap();
    let path: PathBuf = dir.path().join("msb.db");
    {
        let pools = DbPools::open(&path, 2, TIMEOUT, TIMEOUT).await.unwrap();
        pools
            .write()
            .execute_unprepared("CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT); CREATE TABLE p (id INTEGER PRIMARY KEY, v TEXT)")
            .await
            .unwrap();
    }
    let mut failures = Vec::new();
    let started = Instant::now();
    for round in 0..rounds {
        let mut children: Vec<Child> = (0..parallel).map(|_| spawn_child(&path, mode)).collect();
        let kill_at = Instant::now() + Duration::from_millis(30 + (round as u64 * 7) % 90);
        // Several openers at once in this process, as a test binary running tests in parallel
        // against one home does, while the children start, write and exit.
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let openers: Vec<_> = (0..OPENERS)
            .map(|_| {
                let (path, stop) = (path.clone(), stop.clone());
                tokio::spawn(async move {
                    let mut errors = Vec::new();
                    while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                        if let Err(e) = open_and_read(&path).await {
                            errors.push(e);
                        }
                    }
                    errors
                })
            })
            .collect();
        loop {
            if mode == "kill" && Instant::now() >= kill_at {
                for child in &mut children {
                    let _ = child.kill();
                }
            }
            let mut alive = false;
            for child in &mut children {
                alive |= child.try_wait().unwrap().is_none();
            }
            if !alive {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        for opener in openers {
            for e in opener.await.unwrap() {
                failures.push(format!("round {round} (during): {e}"));
            }
        }
        if let Err(e) = open_and_read(&path).await {
            failures.push(format!("round {round} (after): {e}"));
        }
    }
    // Every child must really have run and committed its rows.
    let pools = DbPools::open(&path, 1, TIMEOUT, TIMEOUT).await.unwrap();
    let row = pools
        .read()
        .query_one_raw(sea_orm::Statement::from_string(
            sea_orm::DbBackend::Sqlite,
            "SELECT COUNT(*) FROM t",
        ))
        .await
        .unwrap()
        .unwrap();
    let count = row.try_get_by_index::<i64>(0).unwrap();
    if mode == "kill" {
        assert!(count > 0, "no child wrote anything");
    } else {
        assert_eq!(count, (rounds * parallel * CHILD_ROWS) as i64);
    }
    eprintln!(
        "mode {mode}: {rounds} rounds x {parallel} children in {:?}, {} open failures",
        started.elapsed(),
        failures.len()
    );
    for f in failures.iter().take(10) {
        eprintln!("  {f}");
    }
    failures
}

fn rounds() -> usize {
    std::env::var("MSB_DB_EXIT_ROUNDS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(3)
}

#[tokio::test(flavor = "multi_thread")]
async fn open_succeeds_while_writer_exits_with_pool_open() {
    let failures = run("abrupt", rounds(), 3).await;
    assert!(failures.is_empty(), "{failures:#?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn open_succeeds_while_writer_exits_after_closing_pool() {
    let failures = run("graceful", rounds(), 3).await;
    assert!(failures.is_empty(), "{failures:#?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn open_succeeds_while_writer_is_terminated() {
    let failures = run("kill", rounds(), 3).await;
    assert!(failures.is_empty(), "{failures:#?}");
}

/// Opens an existing catalog over and over until a stop file appears, for the stress job that
/// lets real msb sandbox runtimes start and exit meanwhile. Does nothing unless
/// `MSB_DB_OPEN_LOOP` names the database file; `MSB_DB_OPEN_LOOP_STOP` the stop file.
#[tokio::test(flavor = "multi_thread")]
async fn open_loop_against_running_msb() {
    let (Ok(db), Ok(stop)) = (
        std::env::var("MSB_DB_OPEN_LOOP"),
        std::env::var("MSB_DB_OPEN_LOOP_STOP"),
    ) else {
        return;
    };
    let (db, stop) = (PathBuf::from(db), PathBuf::from(stop));
    let (mut opens, mut failures) = (0u64, Vec::new());
    while !stop.exists() {
        opens += 1;
        if let Err(e) = open_loop_once(&db, opens % 2 == 0).await {
            failures.push(format!("open {opens}: {e}"));
        }
    }
    eprintln!("open loop: {opens} opens, {} failures", failures.len());
    for f in failures.iter().take(20) {
        eprintln!("  {f}");
    }
    assert!(failures.is_empty(), "{failures:#?}");
}

async fn open_loop_once(db: &Path, close: bool) -> Result<(), String> {
    let pools = DbPools::open(db, 8, TIMEOUT, TIMEOUT)
        .await
        .map_err(|e| format!("connect: {e}"))?;
    pools
        .read()
        .query_one_raw(sea_orm::Statement::from_string(
            sea_orm::DbBackend::Sqlite,
            "SELECT COUNT(*) FROM sandbox",
        ))
        .await
        .map_err(|e| format!("read: {e}"))?;
    if close {
        pools
            .write()
            .inner()
            .clone()
            .close()
            .await
            .map_err(|e| format!("close: {e}"))?;
        pools
            .read()
            .inner()
            .clone()
            .close()
            .await
            .map_err(|e| format!("close: {e}"))?;
    }
    tokio::time::sleep(Duration::from_millis(20)).await;
    Ok(())
}
