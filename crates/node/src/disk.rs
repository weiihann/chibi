//! The per-network data directory: its lock, the devp2p identity, and the state database.
//!
//! The database holds a display snapshot, the outbox of pending raw transactions, state
//! peers worth dialing first, and the learned working-set keys, most recently used first.
//! Pins are not saved: they follow from the config at each start. Its values are
//! display-only: nothing read from it enters a `Store`.

use std::fs::{self, File, OpenOptions, TryLockError};
use std::io::{ErrorKind, Read as _, Seek as _, SeekFrom, Write as _};
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant};

use alloy_eips::BlockNumHash;
use alloy_primitives::{Address, Bytes, U256, hex};
use chibi_core::state::Key;
use parking_lot::Mutex;
use reth_network_peers::NodeRecord;
use rusqlite::types::{Type, ValueRef};
use rusqlite::{Connection, OpenFlags, OptionalExtension as _, params};
use serde::Deserialize;
use tracing::warn;

use crate::devp2p::{SecretKey, rng_secret_key};
use crate::node::{NodeError, Snapshot};

const LOCK_FILE: &str = "chibi.lock";
const DB_FILE: &str = "chibi.db";
/// How long the writer and `chibi status` wait for each other's lock on the database.
const BUSY_TIMEOUT: Duration = Duration::from_secs(5);
/// The JSON file that held the state before the database. [`Disk::open`] imports it once and
/// deletes it.
const LEGACY_STATE_FILE: &str = "state.json";
const KEY_FILE: &str = "devp2p.key";

/// The schema version recorded in `PRAGMA user_version`; 0 means no schema yet.
const SCHEMA_VERSION: i32 = 2;
const SCHEMA_V1: &str = "
CREATE TABLE snapshot (
    id INTEGER PRIMARY KEY CHECK (id = 0),
    json TEXT NOT NULL
);
CREATE TABLE outbox (
    position INTEGER PRIMARY KEY,
    raw BLOB NOT NULL,
    since_number INTEGER NOT NULL,
    since_hash BLOB NOT NULL
);
CREATE TABLE peers (
    position INTEGER PRIMARY KEY,
    record TEXT NOT NULL
);
";
/// What version 2 adds: the learned keys, most recently used first. `slot` is NULL for an
/// account key.
const SCHEMA_V2: &str = "
CREATE TABLE keys (
    position INTEGER PRIMARY KEY,
    address BLOB NOT NULL,
    slot BLOB
);
";

/// Most state peers remembered across restarts.
const REMEMBERED_PEERS: usize = 16;

/// How long lock-file readers and openers retry. [`Disk::running`] briefly holds a shared lock
/// to probe, and [`Lock::take`] writes the pid just after taking the lock.
const LOCK_RETRY: Duration = Duration::from_millis(100);
const LOCK_POLL: Duration = Duration::from_millis(5);

/// The chibi process holding a data directory, as recorded in its lock file: the pid on the
/// first line and, once announced, the endpoint it serves on the second.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Holder {
    /// The holder's process id.
    pub pid: u32,
    /// What the holder serves, recorded by [`Disk::announce`]; `None` until it does.
    pub endpoint: Option<String>,
}

impl Holder {
    /// Parses the lock file; `None` while it holds no complete pid yet.
    fn parse(text: &str) -> Option<Self> {
        let mut lines = text.lines();
        let pid = lines.next()?.trim().parse().ok()?;
        let endpoint = lines
            .next()
            .map(str::trim)
            .filter(|endpoint| !endpoint.is_empty())
            .map(str::to_owned);
        Some(Self { pid, endpoint })
    }
}

/// A submitted transaction not yet located, and the anchor chibi held when it was submitted.
/// Inclusion can only be in a later block, so a search for it need not reach further back.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
pub(crate) struct OutboxTx {
    /// The EIP-2718 encoding.
    pub(crate) raw: Bytes,
    /// The anchor at submission.
    pub(crate) since: BlockNumHash,
}

/// The saved state. Deserializes from the legacy JSON file.
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize)]
pub(crate) struct Saved {
    snapshot: Option<Snapshot>,
    outbox: Vec<OutboxTx>,
    peers: Vec<String>,
}

impl Saved {
    pub(crate) const fn snapshot(&self) -> Option<&Snapshot> {
        self.snapshot.as_ref()
    }

    pub(crate) fn set_snapshot(&mut self, snapshot: Snapshot) {
        self.snapshot = Some(snapshot);
    }

    /// Transactions submitted and not yet located.
    pub(crate) fn outbox(&self) -> &[OutboxTx] {
        &self.outbox
    }

    pub(crate) fn set_outbox(&mut self, outbox: Vec<OutboxTx>) {
        self.outbox = outbox;
    }

    /// Remembered state peers. Unparseable records are skipped.
    pub(crate) fn peers(&self) -> Vec<NodeRecord> {
        self.peers
            .iter()
            .filter_map(|record| record.parse().ok())
            .collect()
    }

    /// Remembers `records` ahead of earlier ones, so peers that drop off do not erase the list.
    pub(crate) fn remember_peers(&mut self, records: &[NodeRecord]) {
        let mut merged: Vec<NodeRecord> = Vec::with_capacity(REMEMBERED_PEERS);
        for record in records.iter().copied().chain(self.peers()) {
            if merged.len() < REMEMBERED_PEERS && merged.iter().all(|kept| kept.id != record.id) {
                merged.push(record);
            }
        }
        self.peers = merged.iter().map(NodeRecord::to_string).collect();
    }
}

/// An open, locked data directory. Dropping it releases the lock.
#[derive(Debug)]
pub struct Disk {
    db: Mutex<Db>,
    lock: Lock,
}

/// The exclusive lock on a data directory, the one a running chibi holds. The lock file
/// records the holder's process id. Dropping it releases the lock.
#[derive(Debug)]
pub struct Lock {
    dir: PathBuf,
    _file: File,
}

/// The writer's connection and what it last committed.
#[derive(Debug)]
struct Db {
    conn: Connection,
    saved: Saved,
}

/// The rows [`Lock::reset`] deleted from each table it clears.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Reset {
    /// Saved snapshots: zero or one.
    pub snapshot: usize,
    /// Pending transactions.
    pub outbox: usize,
    /// Learned keys.
    pub keys: usize,
}

impl Lock {
    /// Creates `dir` if needed, takes its exclusive lock, and records this process id in the
    /// lock file. Fails while another process holds it.
    pub fn take(dir: &Path) -> Result<Self, NodeError> {
        fs::create_dir_all(dir).map_err(|err| io_error(dir, &err))?;
        let lock_path = dir.join(LOCK_FILE);
        let mut file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&lock_path)
            .map_err(|err| io_error(&lock_path, &err))?;
        let deadline = Instant::now() + LOCK_RETRY;
        loop {
            match file.try_lock() {
                Ok(()) => break,
                Err(TryLockError::WouldBlock) if Instant::now() < deadline => {
                    thread::sleep(LOCK_POLL);
                }
                Err(TryLockError::WouldBlock) => {
                    return Err(NodeError::Disk(format!(
                        "{} is in use by another chibi process",
                        dir.display()
                    )));
                }
                Err(TryLockError::Error(err)) => return Err(io_error(&lock_path, &err)),
            }
        }
        let pid = std::process::id().to_string();
        file.set_len(0)
            .and_then(|()| file.write_all(pid.as_bytes()))
            .and_then(|()| file.sync_all())
            .map_err(|err| io_error(&lock_path, &err))?;
        Ok(Self {
            dir: dir.to_path_buf(),
            _file: file,
        })
    }

    /// Deletes the user state saved in the directory's database: the snapshot, the outbox
    /// and the learned keys, in one transaction. Remembered state peers, the light-client
    /// checkpoint and the devp2p key stay. `None` when there is no database, which is left
    /// uncreated.
    pub fn reset(&self) -> Result<Option<Reset>, NodeError> {
        let path = self.dir.join(DB_FILE);
        if !path.try_exists().map_err(|err| io_error(&path, &err))? {
            return Ok(None);
        }
        let mut db = open_db(&self.dir)?;
        clear(&mut db.conn)
            .map(Some)
            .map_err(|err| db_error(&path, &err))
    }
}

impl Disk {
    /// Takes `dir`'s [`Lock`] and opens the database, importing a legacy state file into a
    /// new one.
    pub fn open(dir: &Path) -> Result<Self, NodeError> {
        let lock = Lock::take(dir)?;
        let db = open_db(dir)?;
        Ok(Self {
            db: Mutex::new(db),
            lock,
        })
    }

    /// The chibi that holds `dir`'s lock, or `None` when no chibi runs there. Does not take the
    /// lock away from anyone.
    pub fn running(dir: &Path) -> Result<Option<Holder>, NodeError> {
        let lock_path = dir.join(LOCK_FILE);
        let lock = match File::open(&lock_path) {
            Ok(lock) => lock,
            Err(err) if err.kind() == ErrorKind::NotFound => return Ok(None),
            Err(err) => return Err(io_error(&lock_path, &err)),
        };
        let deadline = Instant::now() + LOCK_RETRY;
        loop {
            match lock.try_lock_shared() {
                Ok(()) => return Ok(None),
                Err(TryLockError::WouldBlock) => {
                    let text =
                        fs::read_to_string(&lock_path).map_err(|err| io_error(&lock_path, &err))?;
                    if let Some(holder) = Holder::parse(&text) {
                        return Ok(Some(holder));
                    }
                    if Instant::now() >= deadline {
                        return Err(NodeError::Disk(format!(
                            "{} holds no process id",
                            lock_path.display()
                        )));
                    }
                    thread::sleep(LOCK_POLL);
                }
                Err(TryLockError::Error(err)) => return Err(io_error(&lock_path, &err)),
            }
        }
    }

    /// Records `endpoint` in `dir`'s lock file for [`Disk::running`] to report. Only the
    /// process holding `dir` may announce; a later announcement replaces an earlier one.
    pub fn announce(dir: &Path, endpoint: &str) -> Result<(), NodeError> {
        if endpoint.contains(['\n', '\r']) {
            return Err(NodeError::Disk(format!(
                "endpoint {endpoint:?} spans several lines"
            )));
        }
        let lock_path = dir.join(LOCK_FILE);
        let fail = |err: std::io::Error| io_error(&lock_path, &err);
        let mut lock = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&lock_path)
            .map_err(fail)?;
        let mut text = String::new();
        lock.read_to_string(&mut text).map_err(fail)?;
        let pid = text.lines().next().unwrap_or_default();
        if pid.trim() != std::process::id().to_string() {
            return Err(NodeError::Disk(format!(
                "{} is not held by this process",
                dir.display()
            )));
        }
        let pid_len = u64::try_from(pid.len()).unwrap_or(u64::MAX);
        lock.set_len(pid_len)
            .and_then(|()| lock.seek(SeekFrom::End(0)))
            .and_then(|_| lock.write_all(format!("\n{endpoint}").as_bytes()))
            .and_then(|()| lock.sync_all())
            .map_err(fail)
    }

    /// The data directory.
    pub fn dir(&self) -> &Path {
        &self.lock.dir
    }

    /// Reads the loaded state.
    pub(crate) fn read<R>(&self, f: impl FnOnce(&Saved) -> R) -> R {
        f(&self.db.lock().saved)
    }

    /// Edits the state and commits the tables it changed before releasing it, so concurrent
    /// writers cannot reorder their writes. Blocks on fsync; call it off the async workers.
    pub(crate) fn save(&self, edit: impl FnOnce(&mut Saved)) -> Result<(), NodeError> {
        self.save_then(edit, || ())
    }

    /// Like [`Disk::save`], and runs `written` once the commit is durable, still holding the
    /// database, so no other write lands between the two. On failure the loaded state is left
    /// as it was and `written` does not run.
    pub(crate) fn save_then<R>(
        &self,
        edit: impl FnOnce(&mut Saved),
        written: impl FnOnce() -> R,
    ) -> Result<R, NodeError> {
        let mut db = self.db.lock();
        let Db { conn, saved } = &mut *db;
        let mut next = saved.clone();
        edit(&mut next);
        if next != *saved {
            commit(conn, saved, &next).map_err(|err| db_error(&self.dir().join(DB_FILE), &err))?;
            *saved = next;
        }
        Ok(written())
    }

    /// The learned keys the last save stored, most recently used first. Rows that do not
    /// parse are skipped.
    pub(crate) fn load_keys(&self) -> Result<Vec<Key>, NodeError> {
        let db = self.db.lock();
        let mut select = db
            .conn
            .prepare("SELECT address, slot FROM keys ORDER BY position")
            .map_err(|err| db_error(&self.dir().join(DB_FILE), &err))?;
        let rows = select
            .query_map([], |row| Ok(parse_key(row.get_ref(0)?, row.get_ref(1)?)))
            .map_err(|err| db_error(&self.dir().join(DB_FILE), &err))?;
        let mut keys = Vec::new();
        for row in rows {
            keys.extend(row.map_err(|err| db_error(&self.dir().join(DB_FILE), &err))?);
        }
        Ok(keys)
    }

    /// Replaces the stored learned keys with `keys`, most recently used first, in one
    /// transaction. Blocks on fsync; call it off the async workers.
    pub(crate) fn save_keys(&self, keys: &[Key]) -> Result<(), NodeError> {
        let mut db = self.db.lock();
        write_keys(&mut db.conn, keys).map_err(|err| db_error(&self.dir().join(DB_FILE), &err))
    }

    /// The devp2p secret key, created on first use so the node keeps one identity.
    pub(crate) fn secret_key(&self) -> Result<SecretKey, NodeError> {
        let path = self.dir().join(KEY_FILE);
        match fs::read_to_string(&path) {
            Ok(text) => {
                let bytes = hex::decode(text.trim())
                    .map_err(|err| NodeError::Disk(format!("{}: {err}", path.display())))?;
                SecretKey::from_slice(&bytes)
                    .map_err(|err| NodeError::Disk(format!("{}: {err}", path.display())))
            }
            Err(err) if err.kind() == ErrorKind::NotFound => {
                let key = rng_secret_key();
                let mut file = private_file(&path).map_err(|err| io_error(&path, &err))?;
                file.write_all(hex::encode(key.secret_bytes()).as_bytes())
                    .and_then(|()| file.sync_all())
                    .map_err(|err| io_error(&path, &err))?;
                Ok(key)
            }
            Err(err) => Err(io_error(&path, &err)),
        }
    }
}

/// Reads the state saved in `dir` without taking the lock, alongside a running writer, and
/// without creating files there. Until a node has created the database, reads the legacy
/// state file instead. `None` if there is neither.
pub(crate) fn read_state(dir: &Path) -> Result<Option<Saved>, NodeError> {
    let path = dir.join(DB_FILE);
    if !path.try_exists().map_err(|err| io_error(&path, &err))? {
        return read_legacy(dir);
    }
    let fail = |err: rusqlite::Error| match err.sqlite_error() {
        // A crashed writer left a hot journal, which a read-only connection cannot roll back.
        Some(failure) if failure.extended_code == rusqlite::ffi::SQLITE_READONLY_ROLLBACK => {
            NodeError::Disk(format!(
                "{}: interrupted write; start the node once to recover the database",
                path.display()
            ))
        }
        _ => db_error(&path, &err),
    };
    let flags = OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX;
    let mut conn = Connection::open_with_flags(&path, flags).map_err(fail)?;
    conn.busy_timeout(BUSY_TIMEOUT).map_err(fail)?;
    let tx = conn.transaction().map_err(fail)?;
    match schema_version(&tx).map_err(fail)? {
        0 => read_legacy(dir),
        1 | SCHEMA_VERSION => load(&tx).map(Some).map_err(fail),
        newer => Err(newer_schema(&path, newer)),
    }
}

/// Opens the writer's connection to `dir`'s database. A new database gets the schema and
/// the contents of a legacy state file if there is one.
fn open_db(dir: &Path) -> Result<Db, NodeError> {
    let path = dir.join(DB_FILE);
    let fail = |err| db_error(&path, &err);
    let mut conn = Connection::open(&path).map_err(fail)?;
    conn.busy_timeout(BUSY_TIMEOUT).map_err(fail)?;
    // The rollback journal, so readers create no files.
    let mode: String = conn
        .pragma_update_and_check(None, "journal_mode", "DELETE", |row| row.get(0))
        .map_err(fail)?;
    if !mode.eq_ignore_ascii_case("delete") {
        return Err(NodeError::Disk(format!(
            "{}: journal mode is {mode}, not DELETE",
            path.display()
        )));
    }
    // With a rollback journal, FULL can lose a committed transaction to power loss: the
    // journal's deletion is not synced, so it can come back as hot and roll the commit back.
    conn.pragma_update(None, "synchronous", "EXTRA")
        .map_err(fail)?;
    match schema_version(&conn).map_err(fail)? {
        0 => create(&mut conn, dir).map_err(fail)?,
        1 => upgrade_from_v1(&mut conn).map_err(fail)?,
        SCHEMA_VERSION => {}
        newer => return Err(newer_schema(&path, newer)),
    }
    let saved = load(&conn).map_err(fail)?;
    Ok(Db { conn, saved })
}

/// Creates the schema, filled from the legacy state file if there is one, and deletes that
/// file once the import is committed. An unreadable legacy file is left in place and ignored.
fn create(conn: &mut Connection, dir: &Path) -> rusqlite::Result<()> {
    let legacy = read_legacy(dir).unwrap_or_else(|err| {
        warn!(target: "chibi::disk", %err, "ignoring the unreadable legacy state file");
        None
    });
    let tx = conn.transaction()?;
    tx.execute_batch(SCHEMA_V1)?;
    tx.execute_batch(SCHEMA_V2)?;
    if let Some(legacy) = &legacy {
        write_tables(&tx, &Saved::default(), legacy)?;
    }
    tx.pragma_update(None, "user_version", SCHEMA_VERSION)?;
    tx.commit()?;
    let legacy_path = dir.join(LEGACY_STATE_FILE);
    if legacy.is_some()
        && let Err(err) = fs::remove_file(&legacy_path)
    {
        warn!(target: "chibi::disk", path = %legacy_path.display(), %err, "could not delete the imported legacy state file");
    }
    Ok(())
}

/// Adds the keys table to a version 1 database in one transaction.
fn upgrade_from_v1(conn: &mut Connection) -> rusqlite::Result<()> {
    let tx = conn.transaction()?;
    tx.execute_batch(SCHEMA_V2)?;
    tx.pragma_update(None, "user_version", SCHEMA_VERSION)?;
    tx.commit()
}

/// Reads the legacy state file in `dir`. `None` if there is none.
fn read_legacy(dir: &Path) -> Result<Option<Saved>, NodeError> {
    let path = dir.join(LEGACY_STATE_FILE);
    let bytes = match fs::read(&path) {
        Ok(bytes) => bytes,
        Err(err) if err.kind() == ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(io_error(&path, &err)),
    };
    serde_json::from_slice(&bytes)
        .map(Some)
        .map_err(|err| NodeError::Disk(format!("{}: {err}", path.display())))
}

fn schema_version(conn: &Connection) -> rusqlite::Result<i32> {
    conn.pragma_query_value(None, "user_version", |row| row.get(0))
}

fn load(conn: &Connection) -> rusqlite::Result<Saved> {
    let snapshot = conn
        .query_row("SELECT json FROM snapshot", [], |row| {
            row.get::<_, String>(0)
        })
        .optional()?
        .map(|json| serde_json::from_str(&json))
        .transpose()
        .map_err(|err| rusqlite::Error::FromSqlConversionFailure(0, Type::Text, Box::new(err)))?;
    let outbox = conn
        .prepare("SELECT raw, since_number, since_hash FROM outbox ORDER BY position")?
        .query_map([], |row| {
            Ok(OutboxTx {
                raw: Bytes::from(row.get::<_, Vec<u8>>(0)?),
                since: BlockNumHash::new(row.get(1)?, row.get::<_, [u8; 32]>(2)?.into()),
            })
        })?
        .collect::<rusqlite::Result<_>>()?;
    let peers = conn
        .prepare("SELECT record FROM peers ORDER BY position")?
        .query_map([], |row| row.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    Ok(Saved {
        snapshot,
        outbox,
        peers,
    })
}

/// Commits `after` over `before` in one transaction.
fn commit(conn: &mut Connection, before: &Saved, after: &Saved) -> rusqlite::Result<()> {
    let tx = conn.transaction()?;
    write_tables(&tx, before, after)?;
    tx.commit()
}

/// Rewrites the tables whose content differs between `before` and `after`.
fn write_tables(conn: &Connection, before: &Saved, after: &Saved) -> rusqlite::Result<()> {
    if after.snapshot != before.snapshot {
        conn.execute("DELETE FROM snapshot", [])?;
        if let Some(snapshot) = &after.snapshot {
            let json = serde_json::to_string(snapshot)
                .map_err(|err| rusqlite::Error::ToSqlConversionFailure(Box::new(err)))?;
            conn.execute("INSERT INTO snapshot (id, json) VALUES (0, ?1)", [json])?;
        }
    }
    if after.outbox != before.outbox {
        conn.execute("DELETE FROM outbox", [])?;
        let mut insert = conn.prepare(
            "INSERT INTO outbox (position, raw, since_number, since_hash) VALUES (?1, ?2, ?3, ?4)",
        )?;
        for (position, entry) in after.outbox.iter().enumerate() {
            insert.execute(params![
                position,
                entry.raw.as_ref(),
                entry.since.number,
                entry.since.hash.as_slice()
            ])?;
        }
    }
    if after.peers != before.peers {
        conn.execute("DELETE FROM peers", [])?;
        let mut insert = conn.prepare("INSERT INTO peers (position, record) VALUES (?1, ?2)")?;
        for (position, record) in after.peers.iter().enumerate() {
            insert.execute(params![position, record])?;
        }
    }
    Ok(())
}

fn write_keys(conn: &mut Connection, keys: &[Key]) -> rusqlite::Result<()> {
    let tx = conn.transaction()?;
    tx.execute("DELETE FROM keys", [])?;
    {
        let mut insert =
            tx.prepare("INSERT INTO keys (position, address, slot) VALUES (?1, ?2, ?3)")?;
        for (position, key) in keys.iter().enumerate() {
            match key {
                Key::Account(address) => {
                    insert.execute(params![position, address.as_slice(), None::<&[u8]>])?;
                }
                Key::Slot(address, slot) => {
                    insert.execute(params![
                        position,
                        address.as_slice(),
                        slot.to_be_bytes::<32>().as_slice()
                    ])?;
                }
            }
        }
    }
    tx.commit()
}

/// Deletes the snapshot, outbox and keys rows in one transaction.
fn clear(conn: &mut Connection) -> rusqlite::Result<Reset> {
    let tx = conn.transaction()?;
    let reset = Reset {
        snapshot: tx.execute("DELETE FROM snapshot", [])?,
        outbox: tx.execute("DELETE FROM outbox", [])?,
        keys: tx.execute("DELETE FROM keys", [])?,
    };
    tx.commit()?;
    Ok(reset)
}

/// The key a `keys` row names: a 20-byte address and either no slot or a 32-byte one.
fn parse_key(address: ValueRef<'_>, slot: ValueRef<'_>) -> Option<Key> {
    let ValueRef::Blob(address) = address else {
        return None;
    };
    let address = Address::try_from(address).ok()?;
    match slot {
        ValueRef::Null => Some(Key::Account(address)),
        ValueRef::Blob(slot) if slot.len() == 32 => {
            Some(Key::Slot(address, U256::from_be_slice(slot)))
        }
        ValueRef::Integer(_) | ValueRef::Real(_) | ValueRef::Text(_) | ValueRef::Blob(_) => None,
    }
}

fn newer_schema(path: &Path, version: i32) -> NodeError {
    NodeError::Disk(format!(
        "{}: schema version {version} is newer than this chibi's {SCHEMA_VERSION}; upgrade chibi",
        path.display()
    ))
}

fn db_error(path: &Path, err: &rusqlite::Error) -> NodeError {
    NodeError::Disk(format!("{}: {err}", path.display()))
}

#[cfg(unix)]
fn private_file(path: &Path) -> std::io::Result<File> {
    use std::os::unix::fs::OpenOptionsExt as _;
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
}

#[cfg(not(unix))]
fn private_file(path: &Path) -> std::io::Result<File> {
    OpenOptions::new().write(true).create_new(true).open(path)
}

fn io_error(path: &Path, err: &std::io::Error) -> NodeError {
    NodeError::Disk(format!("{}: {err}", path.display()))
}

#[cfg(test)]
impl Disk {
    /// Makes saves fail until the returned connection drops: it holds the database's write
    /// lock, and this writer stops waiting for it after 10 ms.
    pub(crate) fn block_writes(&self) -> Connection {
        self.db
            .lock()
            .conn
            .busy_timeout(Duration::from_millis(10))
            .unwrap();
        let other = Connection::open(self.dir().join(DB_FILE)).unwrap();
        other.execute_batch("BEGIN EXCLUSIVE").unwrap();
        other
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::{B256, U256};

    use super::*;
    use crate::node::read_snapshot;

    fn holder(endpoint: Option<&str>) -> Option<Holder> {
        Some(Holder {
            pid: std::process::id(),
            endpoint: endpoint.map(str::to_owned),
        })
    }

    #[test]
    fn second_open_of_a_locked_dir_fails_until_the_first_closes() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(Disk::running(dir.path()).unwrap(), None, "no lock file yet");
        let first = Disk::open(dir.path()).unwrap();
        assert!(matches!(Disk::open(dir.path()), Err(NodeError::Disk(_))));
        assert_eq!(
            Disk::running(dir.path()).unwrap(),
            holder(None),
            "the holder's pid is readable while it holds the lock"
        );
        drop(first);
        assert_eq!(Disk::running(dir.path()).unwrap(), None);
        assert!(Disk::open(dir.path()).is_ok());
    }

    #[test]
    fn announced_endpoint_is_reported_with_the_pid_and_cleared_by_the_next_open() {
        let dir = tempfile::tempdir().unwrap();
        let first = Disk::open(dir.path()).unwrap();
        Disk::announce(dir.path(), "http://chibi.local:8745").unwrap();
        assert_eq!(
            Disk::running(dir.path()).unwrap(),
            holder(Some("http://chibi.local:8745"))
        );
        Disk::announce(dir.path(), "http://127.0.0.1:8545").unwrap();
        assert_eq!(
            Disk::running(dir.path()).unwrap(),
            holder(Some("http://127.0.0.1:8545"))
        );
        assert!(Disk::announce(dir.path(), "a\nb").is_err());
        drop(first);
        assert_eq!(Disk::running(dir.path()).unwrap(), None);
        let _second = Disk::open(dir.path()).unwrap();
        assert_eq!(Disk::running(dir.path()).unwrap(), holder(None));
    }

    #[test]
    fn only_the_holder_announces() {
        let dir = tempfile::tempdir().unwrap();
        let _disk = Disk::open(dir.path()).unwrap();
        let lock_path = dir.path().join(LOCK_FILE);
        fs::write(&lock_path, "1").unwrap();
        assert!(matches!(
            Disk::announce(dir.path(), "http://127.0.0.1:8545"),
            Err(NodeError::Disk(_))
        ));
        assert_eq!(fs::read_to_string(&lock_path).unwrap(), "1");
    }

    #[test]
    fn an_empty_lock_file_under_a_held_lock_is_a_holder_still_starting() {
        let dir = tempfile::tempdir().unwrap();
        let _disk = Disk::open(dir.path()).unwrap();
        let lock_path = dir.path().join(LOCK_FILE);
        fs::write(&lock_path, "").unwrap();
        let writer = {
            let lock_path = lock_path.clone();
            thread::spawn(move || {
                thread::sleep(Duration::from_millis(20));
                fs::write(lock_path, std::process::id().to_string()).unwrap();
            })
        };
        assert_eq!(Disk::running(dir.path()).unwrap(), holder(None));
        writer.join().unwrap();

        fs::write(&lock_path, "").unwrap();
        assert!(
            matches!(Disk::running(dir.path()), Err(NodeError::Disk(_))),
            "a pid that never appears is an error once the retry runs out"
        );
    }

    #[test]
    fn probing_does_not_make_a_concurrent_open_fail() {
        let dir = tempfile::tempdir().unwrap();
        let probing = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
        let prober = {
            let dir = dir.path().to_path_buf();
            let probing = std::sync::Arc::clone(&probing);
            thread::spawn(move || {
                while probing.load(std::sync::atomic::Ordering::Relaxed) {
                    Disk::running(&dir).unwrap();
                }
            })
        };
        for _ in 0..200 {
            drop(Disk::open(dir.path()).unwrap());
        }
        probing.store(false, std::sync::atomic::Ordering::Relaxed);
        prober.join().unwrap();
    }

    #[test]
    fn secret_key_is_stable_across_opens() {
        let dir = tempfile::tempdir().unwrap();
        let key = Disk::open(dir.path()).unwrap().secret_key().unwrap();
        let again = Disk::open(dir.path()).unwrap().secret_key().unwrap();
        assert_eq!(key, again);
    }

    fn record(id: u8, port: u16) -> NodeRecord {
        NodeRecord::new(
            ([10, 0, 0, id], port).into(),
            alloy_primitives::B512::repeat_byte(id),
        )
    }

    fn queued(byte: u8, since: u64) -> OutboxTx {
        OutboxTx {
            raw: Bytes::from(vec![byte]),
            since: BlockNumHash::new(since, B256::repeat_byte(byte)),
        }
    }

    /// A snapshot as written before contracts were tracked.
    fn pre_contracts_snapshot_json() -> String {
        let hash = B256::repeat_byte(1);
        format!(
            r#"{{
    "network": "sepolia",
    "block": {{ "number": 11849838, "hash": "{hash}" }},
    "saved_at": 1759700000,
    "holdings": [
      {{
        "address": "0x8c03000000000000000000000000000000062ee0",
        "balance": "0x2386f26fc10000",
        "nonce": 4,
        "tokens": [["USDC", "0xe4e1c0"], ["WETH", null]]
      }}
    ]
  }}"#
        )
    }

    fn assert_pre_contracts_snapshot(snapshot: &Snapshot) {
        assert_eq!(snapshot.block.number, 11_849_838);
        assert_eq!(snapshot.block.hash, B256::repeat_byte(1));
        assert_eq!(snapshot.saved_at, 1_759_700_000);
        assert_eq!(snapshot.holdings[0].nonce, Some(4));
        assert_eq!(
            snapshot.holdings[0].tokens,
            vec![
                ("USDC".to_owned(), Some(U256::from(15_000_000u64))),
                ("WETH".to_owned(), None)
            ]
        );
        assert!(snapshot.contracts.is_empty());
    }

    #[test]
    fn a_snapshot_saved_before_names_loads_with_no_names() {
        let snapshot: Snapshot = serde_json::from_str(&pre_contracts_snapshot_json()).unwrap();
        assert_pre_contracts_snapshot(&snapshot);
        assert_eq!(snapshot.holdings[0].name, None);
    }

    /// Writes a legacy state file holding the pre-contracts snapshot, one outbox entry equal
    /// to `queued(1, 9)`, and `record(1, 30303)`.
    fn write_legacy_state(dir: &Path) {
        let snapshot = pre_contracts_snapshot_json();
        let hash = B256::repeat_byte(1);
        let peer = record(1, 30303);
        let json = format!(
            r#"{{
  "snapshot": {snapshot},
  "outbox": [{{ "raw": "0x01", "since": {{ "number": 9, "hash": "{hash}" }} }}],
  "peers": ["{peer}"]
}}"#
        );
        fs::write(dir.join(LEGACY_STATE_FILE), json).unwrap();
    }

    #[test]
    fn outbox_peers_and_snapshot_survive_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let snapshot: Snapshot = serde_json::from_str(&pre_contracts_snapshot_json()).unwrap();
        {
            let disk = Disk::open(dir.path()).unwrap();
            disk.save(|saved| {
                saved.set_outbox(vec![queued(1, 9), queued(2, 11)]);
                saved.set_snapshot(snapshot.clone());
                saved.remember_peers(&[record(1, 30303)]);
            })
            .unwrap();
            disk.save(|saved| saved.remember_peers(&[record(2, 30304)]))
                .unwrap();
        }
        let disk = Disk::open(dir.path()).unwrap();
        disk.read(|saved| {
            assert_eq!(saved.outbox(), &[queued(1, 9), queued(2, 11)]);
            assert_eq!(saved.peers(), vec![record(2, 30304), record(1, 30303)]);
            assert_pre_contracts_snapshot(saved.snapshot().unwrap());
        });
    }

    #[test]
    fn reset_deletes_snapshot_outbox_and_keys_and_keeps_peers_checkpoint_and_key() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(Lock::take(dir.path()).unwrap().reset().unwrap(), None);
        assert!(!dir.path().join(DB_FILE).exists());
        let snapshot: Snapshot = serde_json::from_str(&pre_contracts_snapshot_json()).unwrap();
        let key = {
            let disk = Disk::open(dir.path()).unwrap();
            disk.save(|saved| {
                saved.set_outbox(vec![queued(1, 9), queued(2, 11)]);
                saved.set_snapshot(snapshot);
                saved.remember_peers(&[record(1, 30303), record(2, 30304)]);
            })
            .unwrap();
            disk.save_keys(&[
                Key::Account(Address::repeat_byte(1)),
                Key::Slot(Address::repeat_byte(2), U256::from(3)),
                Key::Account(Address::repeat_byte(4)),
            ])
            .unwrap();
            assert!(
                matches!(Lock::take(dir.path()), Err(NodeError::Disk(_))),
                "a held directory cannot be reset"
            );
            disk.secret_key().unwrap()
        };
        fs::write(dir.path().join("checkpoint"), [7u8; 32]).unwrap();

        let lock = Lock::take(dir.path()).unwrap();
        assert!(matches!(Disk::open(dir.path()), Err(NodeError::Disk(_))));
        assert_eq!(
            lock.reset().unwrap(),
            Some(Reset {
                snapshot: 1,
                outbox: 2,
                keys: 3,
            })
        );
        assert_eq!(lock.reset().unwrap(), Some(Reset::default()));
        drop(lock);
        assert_eq!(read_snapshot(dir.path()).unwrap(), None);
        assert_eq!(fs::read(dir.path().join("checkpoint")).unwrap(), [7u8; 32]);
        let disk = Disk::open(dir.path()).unwrap();
        assert_eq!(disk.secret_key().unwrap(), key);
        assert_eq!(disk.load_keys().unwrap(), Vec::new());
        disk.read(|saved| {
            assert_eq!(saved.snapshot(), None);
            assert_eq!(saved.outbox(), &[]);
            assert_eq!(saved.peers(), vec![record(1, 30303), record(2, 30304)]);
        });
    }

    #[test]
    fn a_legacy_state_file_from_before_contracts_is_imported_then_deleted() {
        let dir = tempfile::tempdir().unwrap();
        write_legacy_state(dir.path());
        drop(Disk::open(dir.path()).unwrap());
        assert!(!dir.path().join(LEGACY_STATE_FILE).exists());
        let disk = Disk::open(dir.path()).unwrap();
        disk.read(|saved| {
            assert_eq!(saved.outbox(), &[queued(1, 9)]);
            assert_eq!(saved.peers(), vec![record(1, 30303)]);
            assert_pre_contracts_snapshot(saved.snapshot().unwrap());
        });
    }

    #[test]
    fn status_reads_the_snapshot_while_a_disk_holds_the_directory() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(read_snapshot(dir.path()).unwrap(), None);
        let disk = Disk::open(dir.path()).unwrap();
        assert_eq!(read_snapshot(dir.path()).unwrap(), None);
        let snapshot: Snapshot = serde_json::from_str(&pre_contracts_snapshot_json()).unwrap();
        disk.save(|saved| saved.set_snapshot(snapshot)).unwrap();
        assert_pre_contracts_snapshot(&read_snapshot(dir.path()).unwrap().unwrap());
        disk.save(|saved| saved.set_outbox(vec![queued(1, 9)]))
            .unwrap();
        assert_eq!(
            read_state(dir.path()).unwrap().unwrap().outbox(),
            &[queued(1, 9)]
        );
    }

    #[test]
    fn status_reads_a_legacy_only_directory_without_writing() {
        let dir = tempfile::tempdir().unwrap();
        write_legacy_state(dir.path());
        assert_pre_contracts_snapshot(&read_snapshot(dir.path()).unwrap().unwrap());
        let names: Vec<_> = fs::read_dir(dir.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert_eq!(names, vec![std::ffi::OsString::from(LEGACY_STATE_FILE)]);
    }

    fn listing(dir: &Path) -> Vec<std::ffi::OsString> {
        let mut names: Vec<_> = fs::read_dir(dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        names.sort();
        names
    }

    /// Saves a snapshot in `dir`'s database and closes it as a stopping node does.
    fn write_closed_database(dir: &Path) {
        let disk = Disk::open(dir).unwrap();
        let snapshot: Snapshot = serde_json::from_str(&pre_contracts_snapshot_json()).unwrap();
        disk.save(|saved| saved.set_snapshot(snapshot)).unwrap();
        drop(disk);
        assert!(listing(dir).contains(&DB_FILE.into()));
    }

    #[test]
    fn status_reads_a_closed_database_without_creating_files() {
        let dir = tempfile::tempdir().unwrap();
        write_closed_database(dir.path());
        let before = listing(dir.path());
        assert_pre_contracts_snapshot(&read_snapshot(dir.path()).unwrap().unwrap());
        assert_eq!(listing(dir.path()), before);
    }

    #[test]
    fn status_refuses_a_database_with_a_hot_journal() {
        let dir = tempfile::tempdir().unwrap();
        write_closed_database(dir.path());
        // A transaction too big for the page cache spills to the database file after syncing
        // its journal. Copy the files out then, as a crash would leave them.
        let mut writer = Connection::open(dir.path().join(DB_FILE)).unwrap();
        writer.pragma_update(None, "cache_size", 2).unwrap();
        let tx = writer.transaction().unwrap();
        tx.execute("DELETE FROM snapshot", []).unwrap();
        for position in 0..64 {
            tx.execute(
                "INSERT INTO outbox VALUES (?1, zeroblob(4096), 0, x'')",
                [position],
            )
            .unwrap();
        }
        let crashed = tempfile::tempdir().unwrap();
        for name in [DB_FILE, "chibi.db-journal"] {
            fs::copy(dir.path().join(name), crashed.path().join(name)).unwrap();
        }
        drop(tx);
        let before = listing(crashed.path());
        let err = read_state(crashed.path()).unwrap_err().to_string();
        assert!(err.contains("start the node once"), "{err}");
        assert_eq!(listing(crashed.path()), before);
        drop(Disk::open(crashed.path()).unwrap());
        assert_pre_contracts_snapshot(&read_snapshot(crashed.path()).unwrap().unwrap());
    }

    #[cfg(unix)]
    #[test]
    fn status_reads_a_closed_database_in_a_read_only_directory() {
        use std::os::unix::fs::PermissionsExt as _;

        /// Makes the directory writable again, so the temporary directory can be removed.
        struct Writable<'a>(&'a Path);
        impl Drop for Writable<'_> {
            fn drop(&mut self) {
                let _ = fs::set_permissions(self.0, fs::Permissions::from_mode(0o700));
            }
        }

        let dir = tempfile::tempdir().unwrap();
        write_closed_database(dir.path());
        let before = listing(dir.path());
        fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o500)).unwrap();
        let _writable = Writable(dir.path());
        assert_pre_contracts_snapshot(&read_snapshot(dir.path()).unwrap().unwrap());
        assert_eq!(listing(dir.path()), before);
    }

    #[test]
    fn status_beside_a_writer_that_keeps_reopening_reads_each_saved_state_in_order() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicU8, Ordering};

        const SAVES: u8 = 40;
        let dir = tempfile::tempdir().unwrap();
        drop(Disk::open(dir.path()).unwrap());
        let committed = Arc::new(AtomicU8::new(0));
        let writer = {
            let dir = dir.path().to_owned();
            let committed = Arc::clone(&committed);
            thread::spawn(move || {
                for byte in 1..=SAVES {
                    let disk = Disk::open(&dir).unwrap();
                    disk.save(|saved| saved.set_outbox(vec![queued(byte, byte.into())]))
                        .unwrap();
                    committed.store(byte, Ordering::Release);
                    drop(disk);
                    let closed_gap = Duration::from_millis(1);
                    thread::sleep(closed_gap);
                }
            })
        };
        let mut last = 0;
        while !writer.is_finished() {
            let floor = committed.load(Ordering::Acquire);
            let saved = read_state(dir.path()).unwrap().unwrap();
            let byte = match saved.outbox() {
                [] => 0,
                [tx] => {
                    assert_eq!(tx, &queued(tx.raw[0], tx.raw[0].into()));
                    tx.raw[0]
                }
                more => panic!("one save holds one transaction, read {}", more.len()),
            };
            assert!(byte >= floor, "read {byte} after {floor} was committed");
            assert!(byte >= last, "read {byte} after reading {last}");
            last = byte;
            thread::sleep(Duration::from_micros(300));
        }
        writer.join().unwrap();
        assert_eq!(
            read_state(dir.path()).unwrap().unwrap().outbox(),
            &[queued(SAVES, SAVES.into())]
        );
    }

    #[test]
    fn an_unreadable_legacy_state_file_is_left_and_the_state_starts_empty() {
        let dir = tempfile::tempdir().unwrap();
        let legacy = dir.path().join(LEGACY_STATE_FILE);
        fs::write(&legacy, "{ not json").unwrap();
        let disk = Disk::open(dir.path()).unwrap();
        disk.read(|saved| assert_eq!(saved, &Saved::default()));
        assert_eq!(fs::read_to_string(&legacy).unwrap(), "{ not json");
    }

    #[test]
    fn remembered_peers_merge_newest_first_dedupe_by_id_and_cap() {
        let mut file = Saved::default();
        file.remember_peers(&[record(1, 1), record(2, 2)]);
        file.remember_peers(&[]);
        assert_eq!(file.peers(), vec![record(1, 1), record(2, 2)]);
        file.remember_peers(&[record(2, 9)]);
        assert_eq!(file.peers(), vec![record(2, 9), record(1, 1)]);
        let many: Vec<NodeRecord> = (10..40).map(|id| record(id, 1)).collect();
        file.remember_peers(&many);
        assert_eq!(file.peers(), many[..REMEMBERED_PEERS].to_vec());
    }

    #[test]
    fn a_new_database_is_at_version_2_and_keeps_learned_keys_in_order() {
        let dir = tempfile::tempdir().unwrap();
        let keys = [
            Key::Slot(Address::repeat_byte(2), U256::from(7)),
            Key::Account(Address::repeat_byte(2)),
            Key::Account(Address::repeat_byte(1)),
        ];
        {
            let disk = Disk::open(dir.path()).unwrap();
            assert_eq!(disk.load_keys().unwrap(), vec![]);
            disk.save_keys(&keys).unwrap();
        }
        let disk = Disk::open(dir.path()).unwrap();
        assert_eq!(disk.load_keys().unwrap(), keys);
        assert_eq!(schema_version(&disk.db.lock().conn).unwrap(), 2);
    }

    #[test]
    fn a_version_1_database_gains_the_keys_table_and_keeps_its_state() {
        let dir = tempfile::tempdir().unwrap();
        {
            let disk = Disk::open(dir.path()).unwrap();
            disk.save(|saved| saved.set_outbox(vec![queued(1, 9)]))
                .unwrap();
        }
        let conn = Connection::open(dir.path().join(DB_FILE)).unwrap();
        conn.execute_batch("DROP TABLE keys; PRAGMA user_version = 1;")
            .unwrap();
        drop(conn);
        assert!(
            read_state(dir.path()).unwrap().is_some(),
            "status reads version 1"
        );

        let disk = Disk::open(dir.path()).unwrap();
        disk.read(|saved| assert_eq!(saved.outbox(), &[queued(1, 9)]));
        assert_eq!(disk.load_keys().unwrap(), vec![]);
        disk.save_keys(&[Key::Account(Address::repeat_byte(1))])
            .unwrap();
        assert_eq!(schema_version(&disk.db.lock().conn).unwrap(), 2);
    }

    #[test]
    fn a_failed_save_leaves_the_loaded_state_and_skips_written() {
        let dir = tempfile::tempdir().unwrap();
        let disk = Disk::open(dir.path()).unwrap();
        disk.save(|saved| saved.set_outbox(vec![queued(1, 1)]))
            .unwrap();
        let blocker = disk.block_writes();
        let mut written = false;
        let result = disk.save_then(
            |saved| saved.set_outbox(vec![queued(2, 1)]),
            || written = true,
        );
        assert!(matches!(result, Err(NodeError::Disk(_))));
        assert!(!written);
        disk.read(|saved| assert_eq!(saved.outbox(), &[queued(1, 1)]));

        drop(blocker);
        disk.save(|saved| saved.remember_peers(&[record(1, 1)]))
            .unwrap();
        drop(disk);
        let reopened = Disk::open(dir.path()).unwrap();
        reopened.read(|saved| {
            assert_eq!(saved.outbox(), &[queued(1, 1)]);
            assert_eq!(saved.peers(), vec![record(1, 1)]);
        });
    }
}
