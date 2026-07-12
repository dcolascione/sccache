// Copyright 2026 Mozilla Foundation
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use crate::errors::*;
use crate::server::ServerStats;
use fs2::FileExt;
use rusqlite::{Connection, ErrorCode, OptionalExtension, TransactionBehavior, params};
use std::fs::{self, File, OpenOptions};
use std::path::{Path, PathBuf};
use std::time::Duration;

const BUSY_TIMEOUT: Duration = Duration::from_secs(30);

/// Cross-process statistics storage for serverless invocations.
pub(crate) struct StatsStore {
    path: PathBuf,
}

impl StatsStore {
    pub(crate) fn new(path: PathBuf) -> Self {
        Self { path }
    }

    pub(crate) fn read(&self) -> Result<ServerStats> {
        self.with_recovery(|connection| read_stats(connection))
    }

    pub(crate) fn merge(&self, delta: ServerStats) -> Result<()> {
        self.with_recovery(|connection| {
            let transaction =
                connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
            let mut stats = read_stats(&transaction)?;
            stats += delta.clone();
            write_stats(&transaction, &stats)?;
            transaction.commit()?;
            Ok(())
        })
    }

    pub(crate) fn zero(&self) -> Result<()> {
        self.with_recovery(|connection| {
            let transaction =
                connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
            write_stats(&transaction, &ServerStats::default())?;
            transaction.commit()?;
            Ok(())
        })
    }

    fn with_recovery<T>(
        &self,
        mut operation: impl FnMut(&mut Connection) -> Result<T>,
    ) -> Result<T> {
        // Serializing all operations also prevents corruption recovery from
        // unlinking a database that another process is still updating.
        let _database_lock = self.lock_database()?;
        match self.run(&mut operation) {
            Ok(value) => Ok(value),
            Err(error) if is_corruption(&error) => {
                eprintln!(
                    "sccache: warning: statistics database {} is corrupt; rebuilding it",
                    self.path.display()
                );
                self.remove_database_files()?;
                self.run(&mut operation)
            }
            Err(error) => Err(error),
        }
    }

    fn run<T>(&self, operation: &mut impl FnMut(&mut Connection) -> Result<T>) -> Result<T> {
        let mut connection = self.open()?;
        operation(&mut connection)
    }

    fn open(&self) -> Result<Connection> {
        fs::create_dir_all(self.root()).with_context(|| {
            format!(
                "Failed to create statistics database directory {}",
                self.root().display()
            )
        })?;
        let connection = Connection::open(&self.path).with_context(|| {
            format!("Failed to open statistics database {}", self.path.display())
        })?;
        connection.busy_timeout(BUSY_TIMEOUT)?;
        connection.execute_batch(
            "
            PRAGMA journal_mode=OFF;
            PRAGMA synchronous=OFF;
            PRAGMA temp_store=MEMORY;
            CREATE TABLE IF NOT EXISTS serverless_stats (
                id INTEGER PRIMARY KEY CHECK (id = 1),
                data BLOB NOT NULL
            );
            ",
        )?;
        Ok(connection)
    }

    fn root(&self) -> &Path {
        self.path.parent().unwrap_or_else(|| Path::new("."))
    }

    fn lock_database(&self) -> Result<File> {
        fs::create_dir_all(self.root())?;
        let path = append_to_path(&self.path, ".lock");
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .open(&path)
            .with_context(|| {
                format!("Failed to open statistics database lock {}", path.display())
            })?;
        FileExt::lock_exclusive(&file).with_context(|| {
            format!("Failed to lock statistics database lock {}", path.display())
        })?;
        Ok(file)
    }

    fn remove_database_files(&self) -> Result<()> {
        for suffix in ["", "-journal", "-wal", "-shm"] {
            let path = append_to_path(&self.path, suffix);
            match fs::remove_file(&path) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => {
                    return Err(error).with_context(|| {
                        format!(
                            "Failed to remove corrupt statistics database file {}",
                            path.display()
                        )
                    });
                }
            }
        }
        Ok(())
    }
}

fn read_stats(connection: &Connection) -> Result<ServerStats> {
    let data: Option<Vec<u8>> = connection
        .query_row(
            "SELECT data FROM serverless_stats WHERE id = 1",
            [],
            |row| row.get(0),
        )
        .optional()?;
    match data {
        Some(data) => serde_json::from_slice(&data).map_err(Into::into),
        None => Ok(ServerStats::default()),
    }
}

fn write_stats(connection: &Connection, stats: &ServerStats) -> Result<()> {
    let data = serde_json::to_vec(stats)?;
    connection.execute(
        "
        INSERT INTO serverless_stats (id, data) VALUES (1, ?1)
        ON CONFLICT(id) DO UPDATE SET data = excluded.data
        ",
        params![data],
    )?;
    Ok(())
}

fn is_corruption(error: &Error) -> bool {
    error.chain().any(|cause| {
        cause.downcast_ref::<serde_json::Error>().is_some()
            || cause
                .downcast_ref::<rusqlite::Error>()
                .is_some_and(|error| {
                    matches!(
                        error,
                        rusqlite::Error::SqliteFailure(sqlite_error, _)
                            if matches!(
                                sqlite_error.code,
                                ErrorCode::DatabaseCorrupt | ErrorCode::NotADatabase
                            )
                    )
                })
    })
}

fn append_to_path(path: &Path, suffix: &str) -> PathBuf {
    let mut value = path.as_os_str().to_owned();
    value.push(suffix);
    value.into()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn concurrent_merges_and_zero() {
        let root = tempfile::tempdir().unwrap();
        let stats = Arc::new(StatsStore::new(root.path().join("stats.sqlite3")));

        let threads: Vec<_> = (0..8)
            .map(|_| {
                let stats = Arc::clone(&stats);
                std::thread::spawn(move || {
                    let mut delta = ServerStats::default();
                    delta.compile_requests = 1;
                    stats.merge(delta).unwrap();
                })
            })
            .collect();
        for thread in threads {
            thread.join().unwrap();
        }

        assert_eq!(stats.read().unwrap().compile_requests, 8);
        stats.zero().unwrap();
        assert_eq!(stats.read().unwrap().compile_requests, 0);
    }

    #[test]
    fn corrupt_database_is_rebuilt() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("stats.sqlite3");
        fs::write(&path, b"not a sqlite database").unwrap();

        let stats = StatsStore::new(path);
        assert_eq!(stats.read().unwrap().compile_requests, 0);

        let mut delta = ServerStats::default();
        delta.compile_requests = 1;
        stats.merge(delta).unwrap();
        assert_eq!(stats.read().unwrap().compile_requests, 1);
    }
}
