//! RAL-393 Stage 3: a small pool of read-only SQLite connections, so a
//! `SELECT`-only request can run concurrently with the single writer (and
//! with other readers) instead of waiting on [`crate::store_lock::StoreMutex`].
//!
//! [`crate::store::Store`] holds one `rusqlite::Connection` used for every
//! mutating operation; that connection stays behind `StoreMutex` and is
//! untouched by this module. `RwLock<Store>` is not an option here --
//! `rusqlite::Connection` (pinned at 0.32.1) is `Send` but not `Sync`
//! (SQLite's own connection handle is not safe to call from two threads at
//! once without external synchronization), so `Store` is not `Sync` and
//! cannot sit behind a `RwLock`. `unsafe impl Sync` is unavailable --
//! `unsafe_code = "forbid"` at the workspace level. A pool of independent
//! `Connection`s sidesteps the problem entirely: each pooled connection is
//! only ever touched by whichever thread currently holds it, exactly like
//! the existing writer connection.
//!
//! WAL mode (already enabled in [`crate::store::Store::open`]) is what
//! makes this safe: a WAL reader never blocks behind (or blocks) the single
//! writer, so pooled read-only connections can run alongside write activity
//! on the writer connection with no additional coordination.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use parking_lot::{Condvar, Mutex};
use rusqlite::{Connection, OpenFlags};

/// How long a connection (writer or pooled reader) waits on SQLite's own
/// internal busy handler before giving up with `SQLITE_BUSY`, per the
/// ticket's Stage 3 requirement. WAL readers essentially never contend with
/// the single writer, but this is cheap insurance against a stray
/// `SQLITE_BUSY` during, e.g., a WAL checkpoint.
pub(crate) const BUSY_TIMEOUT: Duration = Duration::from_secs(5);

/// Page cache per connection, as the negative-kibibyte form SQLite reads as
/// "this many KiB" rather than "this many pages" (which varies with page
/// size). 64 MiB comfortably holds the whole of a 40 MB database plus its
/// indexes, against a SQLite default of 2 MB -- on a database that size the
/// default guarantees the hot read paths keep going back to the OS for pages
/// they just read.
///
/// Costed per connection, so the real figure is this times 1 writer + 4
/// pooled readers. That is the intended trade: 320 MiB of virtual cache
/// against a daemon whose read amplification (RC-4: the board re-hydrates
/// everything on every SSE event) is the thing being paid for.
const CACHE_SIZE_KIB: i64 = -65_536;

/// Memory-mapped I/O window, in bytes. Reads inside the window are served
/// straight out of the page cache with no `read()` syscall and no copy into
/// SQLite's own cache; 256 MiB covers the whole database with room to grow.
/// SQLite silently caps this at whatever it can actually map and treats it as
/// advisory, so an over-generous value is safe.
const MMAP_SIZE_BYTES: i64 = 268_435_456;

/// PRAGMAs every connection to a `Store`'s database wants -- the writer and
/// each pooled reader alike.
///
/// Kept in one function so the writer and the pool cannot drift apart: a
/// reader with a 2 MB cache reading the same 40 MB database the writer reads
/// with 64 MB is a silent asymmetry that only shows up as unexplained read
/// latency on the pooled paths.
///
/// `execute_batch` rather than `pragma_update`: several of these return their
/// new value as a result row, which the `execute` path underneath
/// `pragma_update` rejects.
pub(crate) fn apply_shared_pragmas(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(&format!(
        "PRAGMA cache_size = {CACHE_SIZE_KIB};
         PRAGMA mmap_size = {MMAP_SIZE_BYTES};
         -- Sorting and `DISTINCT` build temporary b-trees. On disk they are
         -- another source of write I/O on the database's own filesystem,
         -- competing with the WAL; `board`-shaped queries (`ORDER BY` over
         -- squads, `DISTINCT` over branches) hit this routinely.
         PRAGMA temp_store = MEMORY;"
    ))
}

/// PRAGMAs that only make sense on the one connection that commits.
///
/// `synchronous = NORMAL` is the change: SQLite defaults to `FULL`, which
/// fsyncs the WAL on every single commit, and that fsync is the entire cost of
/// a small write -- measured at 404 writes/sec in
/// `daemon/tests/store_write_throughput.rs` against a store that does nothing
/// else. Under `NORMAL` in WAL mode the WAL is still fsynced at each
/// checkpoint, so the database **cannot** be corrupted by an OS crash or power
/// loss; what can be lost is the most recent transactions. For a daemon whose
/// state is regenerable agent task bookkeeping -- squads, cells, proof results
/// and a log -- losing the last few hundred milliseconds of a crash is a much
/// smaller cost than an fsync on every row, and the plan records it as a
/// deliberate trade (§8).
pub(crate) fn apply_writer_pragmas(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch("PRAGMA synchronous = NORMAL;")
}

/// How many pooled read-only connections to open per `Store`. Small and
/// fixed -- this is meant to absorb HTTP `GET` read bursts, not to be a
/// general-purpose connection pool sized to core count.
const POOL_SIZE: usize = 4;

/// Where a [`crate::store::Store`]'s database lives, so [`ReadConnPool`] can
/// open its own independent connections to the same database.
#[derive(Debug, Clone)]
pub(crate) enum DbLocation {
    /// A real on-disk database file, opened in WAL mode by the writer.
    File(PathBuf),
    /// An in-memory database, identified by a process-unique shared-cache
    /// name (see [`memory_location`]) so pooled connections attach to the
    /// same in-memory database as the writer instead of each getting their
    /// own private one.
    Memory(String),
}

/// Generate a process-unique name for an in-memory `Store`'s shared-cache
/// database (used by tests, which call `Store::open_in_memory`). Plain
/// `Connection::open_in_memory()` gives every connection its own private
/// database, so a pooled reader would see an empty schema; `cache=shared`
/// with this name lets pooled connections see the same in-memory database
/// as the writer for as long as the writer's own connection stays open.
pub(crate) fn memory_location() -> DbLocation {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    DbLocation::Memory(format!("ral393_store_{n}"))
}

fn open_reader(location: &DbLocation) -> rusqlite::Result<Connection> {
    let flags = OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI;
    let conn = match location {
        DbLocation::File(path) => Connection::open_with_flags(path, flags)?,
        DbLocation::Memory(name) => {
            Connection::open_with_flags(format!("file:{name}?mode=memory&cache=shared"), flags)?
        }
    };
    conn.busy_timeout(BUSY_TIMEOUT)?;
    apply_shared_pragmas(&conn)?;
    Ok(conn)
}

/// A small pool of read-only connections to one `Store`'s database. Reads
/// classified in `store.rs` as safe for pooled access go through
/// [`ReadConnPool::acquire`] instead of [`crate::store_lock::StoreMutex::lock`],
/// so they never wait behind the scheduler/merge writer or behind other
/// readers.
pub(crate) struct ReadConnPool {
    /// How many connections were successfully opened at construction time.
    /// `0` means every open attempt failed (e.g. a transient FS error) --
    /// [`ReadConnPool::acquire`] returns `None` in that case so callers fall
    /// back to the always-correct `StoreMutex` path rather than blocking
    /// forever on a pool that will never hand out a connection.
    total: usize,
    free: Mutex<Vec<Connection>>,
    available: Condvar,
}

impl ReadConnPool {
    pub(crate) fn open(location: &DbLocation) -> Arc<Self> {
        let mut free = Vec::with_capacity(POOL_SIZE);
        for _ in 0..POOL_SIZE {
            match open_reader(location) {
                Ok(conn) => free.push(conn),
                Err(e) => {
                    eprintln!(
                        "ralphus [store] failed to open a pooled read connection: {e} -- reads needing the pool will fall back to the writer lock"
                    );
                    break;
                }
            }
        }
        Arc::new(Self {
            total: free.len(),
            free: Mutex::new(free),
            available: Condvar::new(),
        })
    }

    /// Check out a connection, blocking until one is free. Returns `None`
    /// only when the pool has no connections at all (every open attempt
    /// failed at startup) -- callers must fall back to locking the `Store`
    /// mutex and using its writer connection instead.
    pub(crate) fn acquire(self: &Arc<Self>) -> Option<PooledConn> {
        if self.total == 0 {
            return None;
        }
        let mut guard = self.free.lock();
        while guard.is_empty() {
            self.available.wait(&mut guard);
        }
        let conn = guard.pop().expect("just checked non-empty");
        drop(guard);
        Some(PooledConn {
            conn: Some(conn),
            pool: Arc::clone(self),
        })
    }

    fn release(&self, conn: Connection) {
        self.free.lock().push(conn);
        self.available.notify_one();
    }
}

/// A checked-out pooled read connection. Returned to the pool on drop.
pub(crate) struct PooledConn {
    conn: Option<Connection>,
    pool: Arc<ReadConnPool>,
}

impl std::ops::Deref for PooledConn {
    type Target = Connection;

    fn deref(&self) -> &Connection {
        self.conn
            .as_ref()
            .expect("PooledConn used after its connection was taken")
    }
}

impl Drop for PooledConn {
    fn drop(&mut self) {
        if let Some(conn) = self.conn.take() {
            self.pool.release(conn);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn open_memory_pool() -> (Connection, Arc<ReadConnPool>) {
        let location = memory_location();
        let writer = match &location {
            DbLocation::Memory(name) => Connection::open_with_flags(
                format!("file:{name}?mode=memory&cache=shared"),
                OpenFlags::SQLITE_OPEN_READ_WRITE
                    | OpenFlags::SQLITE_OPEN_CREATE
                    | OpenFlags::SQLITE_OPEN_URI,
            )
            .expect("open shared-cache memory writer"),
            DbLocation::File(_) => unreachable!(),
        };
        writer
            .execute_batch("CREATE TABLE t (id INTEGER PRIMARY KEY); INSERT INTO t VALUES (1);")
            .expect("seed schema");
        let pool = ReadConnPool::open(&location);
        (writer, pool)
    }

    #[test]
    fn pooled_reader_sees_writer_schema_and_data() {
        let (_writer, pool) = open_memory_pool();
        let conn = pool.acquire().expect("pool should have opened connections");
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM t", [], |r| r.get(0))
            .expect("query pooled connection");
        assert_eq!(count, 1);
    }

    #[test]
    fn pooled_reader_sees_later_writes() {
        let (writer, pool) = open_memory_pool();
        writer
            .execute("INSERT INTO t VALUES (2)", [])
            .expect("insert via writer");
        let conn = pool.acquire().expect("acquire");
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM t", [], |r| r.get(0))
            .expect("query pooled connection");
        assert_eq!(count, 2);
    }

    #[test]
    fn acquire_returns_none_when_every_open_attempt_failed() {
        let pool = Arc::new(ReadConnPool {
            total: 0,
            free: Mutex::new(Vec::new()),
            available: Condvar::new(),
        });
        assert!(pool.acquire().is_none());
    }

    #[test]
    fn released_connection_is_reusable() {
        let (_writer, pool) = open_memory_pool();
        {
            let conn = pool.acquire().expect("first acquire");
            let _: i64 = conn
                .query_row("SELECT COUNT(*) FROM t", [], |r| r.get(0))
                .expect("query");
        }
        let conn = pool.acquire().expect("second acquire after release");
        let _: i64 = conn
            .query_row("SELECT COUNT(*) FROM t", [], |r| r.get(0))
            .expect("query again");
    }
}
