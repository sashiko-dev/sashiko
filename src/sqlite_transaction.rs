// Copyright 2026 The Sashiko Authors
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     https://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use anyhow::Result;
use tracing::warn;

/// Guards an open transaction on the transaction `libsql::Connection` so
/// concurrent tasks cannot start a nested transaction before the active one
/// finishes, and suppresses `libsql`'s drop-time rollback `.unwrap()` panic.
pub struct SqliteTransaction {
    tx: Option<libsql::Transaction>,
    _guard: tokio::sync::OwnedMutexGuard<()>,
}

impl std::ops::Deref for SqliteTransaction {
    type Target = libsql::Transaction;

    fn deref(&self) -> &Self::Target {
        // Only Drop removes the private transaction, and completion consumes
        // the guard, so a caller cannot access it after completion.
        self.tx
            .as_ref()
            .expect("transaction accessed after completion")
    }
}

impl SqliteTransaction {
    /// Opens an immediate transaction after locking its connection.
    pub async fn begin(
        connection: &libsql::Connection,
        lock: std::sync::Arc<tokio::sync::Mutex<()>>,
    ) -> Result<Self> {
        let guard = lock.lock_owned().await;
        let tx = connection
            .transaction_with_behavior(libsql::TransactionBehavior::Immediate)
            .await?;
        Ok(Self {
            tx: Some(tx),
            _guard: guard,
        })
    }

    fn drop_quietly(tx: libsql::Transaction) {
        if std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(tx))).is_err() {
            warn!("Suppressed libsql transaction rollback panic during drop");
        }
    }

    /// Commits the transaction while keeping `self.tx` inside `self` across `.await`.
    ///
    /// We intentionally execute `COMMIT` directly instead of calling
    /// `libsql::Transaction::commit(self)`:
    /// 1. `libsql::Transaction::commit(self)` takes `local::Transaction` out of its
    ///    `Option` and drops it *inside* `commit()` before returning `Err`, so a
    ///    failed `COMMIT` triggers `local::Transaction::drop`'s `do_rollback().unwrap()`
    ///    panic before the caller can intercept it.
    /// 2. `local::Transaction::drop` checks `if self.conn.is_autocommit() { return; }`
    ///    (`sqlite3_get_autocommit`) on its first line. Once `COMMIT` succeeds,
    ///    SQLite returns to autocommit mode (`tx.is_autocommit() == true`), so
    ///    dropping `self.tx` in `SqliteTransaction::drop` is a no-op that never
    ///    runs `ROLLBACK` or panics.
    pub async fn commit(self) -> Result<()> {
        if let Some(tx) = self.tx.as_ref() {
            if let Err(err) = tx.execute("COMMIT", ()).await {
                if !tx.is_autocommit() {
                    let _ = tx.execute("ROLLBACK", ()).await;
                }
                return Err(err.into());
            }
            debug_assert!(tx.is_autocommit());
        }
        Ok(())
    }

    /// Rolls back the transaction while keeping `self.tx` inside `self` across `.await`.
    ///
    /// Once `ROLLBACK` succeeds, `tx.is_autocommit()` (`sqlite3_get_autocommit`)
    /// is `true`, so `local::Transaction::drop` returns immediately without
    /// issuing a second `ROLLBACK` or panicking.
    #[cfg(any(feature = "server", test))]
    pub async fn rollback(self) -> Result<()> {
        if let Some(tx) = self.tx.as_ref()
            && !tx.is_autocommit()
        {
            tx.execute("ROLLBACK", ()).await?;
            debug_assert!(tx.is_autocommit());
        }
        Ok(())
    }
}

impl Drop for SqliteTransaction {
    fn drop(&mut self) {
        if let Some(tx) = self.tx.take() {
            Self::drop_quietly(tx);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use tokio::sync::Mutex;

    #[tokio::test]
    async fn failed_commit_rolls_back_and_releases_connection_lock() -> Result<()> {
        let db = libsql::Builder::new_local(":memory:").build().await?;
        let connection = db.connect()?;
        connection
            .execute_batch(
                "PRAGMA foreign_keys = ON;
                 CREATE TABLE parent (id INTEGER PRIMARY KEY);
                 CREATE TABLE child (parent_id INTEGER REFERENCES parent(id)
                     DEFERRABLE INITIALLY DEFERRED);",
            )
            .await?;
        let lock = Arc::new(Mutex::new(()));
        let transaction = SqliteTransaction::begin(&connection, lock.clone()).await?;
        transaction
            .execute("INSERT INTO child VALUES (1)", ())
            .await?;
        assert!(transaction.commit().await.is_err());
        assert!(connection.is_autocommit());
        assert!(lock.try_lock().is_ok());
        let mut rows = connection.query("SELECT COUNT(*) FROM child", ()).await?;
        assert_eq!(rows.next().await?.unwrap().get::<i64>(0)?, 0);
        Ok(())
    }

    #[tokio::test]
    async fn cancellation_rolls_back_before_releasing_connection_lock() -> Result<()> {
        let db = libsql::Builder::new_local(":memory:").build().await?;
        let connection = db.connect()?;
        connection
            .execute("CREATE TABLE entries (id INTEGER)", ())
            .await?;
        let lock = Arc::new(Mutex::new(()));
        let task_connection = connection.clone();
        let task_lock = lock.clone();
        let (ready, started) = tokio::sync::oneshot::channel();
        let task = tokio::spawn(async move {
            let transaction = SqliteTransaction::begin(&task_connection, task_lock).await?;
            transaction
                .execute("INSERT INTO entries VALUES (1)", ())
                .await?;
            ready.send(()).unwrap();
            std::future::pending::<()>().await;
            transaction.commit().await
        });
        started.await?;
        assert!(lock.try_lock().is_err());
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        assert!(connection.is_autocommit());
        assert!(lock.try_lock().is_ok());
        let mut rows = connection.query("SELECT COUNT(*) FROM entries", ()).await?;
        assert_eq!(rows.next().await?.unwrap().get::<i64>(0)?, 0);
        Ok(())
    }
}
