//! A catalog whose switch to WAL mode is slow still opens.
//!
//! Creating a catalog and switching it to WAL makes SQLite sync the file. On a host whose disk is
//! busy with other writers one sync can take longer than the pool's connect timeout, and the open
//! used to fail with `pool timed out while waiting for an open connection` although nothing was
//! contended. A held lock makes the switch slow here in a way a test can control.

use std::{path::Path, time::Duration};

use microsandbox_db::pool::DbPools;
use sqlx::{Connection, SqliteConnection, sqlite::SqliteConnectOptions};

const CONNECT_TIMEOUT: Duration = Duration::from_millis(300);
const BUSY_TIMEOUT: Duration = Duration::from_secs(10);
const LOCK_HELD: Duration = Duration::from_millis(1500);

/// A rollback-journal file with one table: what a catalog is before its first switch to WAL.
async fn make_delete_mode_catalog(path: &Path) {
    let mut conn = SqliteConnection::connect_with(
        &SqliteConnectOptions::new()
            .filename(path)
            .create_if_missing(true),
    )
    .await
    .unwrap();
    sqlx::query("CREATE TABLE t (v TEXT)")
        .execute(&mut conn)
        .await
        .unwrap();
    conn.close().await.unwrap();
}

#[tokio::test]
async fn a_switch_to_wal_slower_than_the_connect_timeout_still_opens() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("msb.db");
    make_delete_mode_catalog(&path).await;

    // Another process writing: it holds the file's exclusive lock, which the switch to WAL has to
    // wait for.
    let mut holder = SqliteConnection::connect_with(&SqliteConnectOptions::new().filename(&path))
        .await
        .unwrap();
    sqlx::query("BEGIN EXCLUSIVE")
        .execute(&mut holder)
        .await
        .unwrap();
    let release = tokio::spawn(async move {
        tokio::time::sleep(LOCK_HELD).await;
        sqlx::query("COMMIT").execute(&mut holder).await.unwrap();
        holder.close().await.unwrap();
    });

    let started = std::time::Instant::now();
    let pools = DbPools::open(&path, 1, CONNECT_TIMEOUT, BUSY_TIMEOUT)
        .await
        .expect("the open waits for the lock instead of timing out");
    assert!(
        started.elapsed() > CONNECT_TIMEOUT,
        "the switch must have been slower than the connect timeout for this test to mean anything"
    );
    release.await.unwrap();

    let mode: String = sqlx::query_scalar("PRAGMA journal_mode")
        .fetch_one(
            &mut SqliteConnection::connect_with(&SqliteConnectOptions::new().filename(&path))
                .await
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(mode, "wal");
    drop(pools);
}

#[tokio::test]
async fn opening_a_catalog_already_in_wal_mode_waits_for_no_switch() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("msb.db");
    drop(
        DbPools::open(&path, 1, Duration::from_secs(5), BUSY_TIMEOUT)
            .await
            .unwrap(),
    );

    let started = std::time::Instant::now();
    DbPools::open(&path, 1, Duration::from_secs(5), BUSY_TIMEOUT)
        .await
        .unwrap();
    assert!(started.elapsed() < Duration::from_secs(2));
}
