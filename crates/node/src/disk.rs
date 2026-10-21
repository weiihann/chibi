//! The per-network data directory: its lock, the devp2p identity, and the state database.
//!
//! The database holds a display snapshot, the outbox of pending raw transactions, state
//! peers worth dialing first, the learned working-set keys, most recently used first, and
//! the account history. Pins are not saved: they follow from the config at each start. Its
//! values are display-only: nothing read from it enters a `Store`.

use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions, TryLockError};
use std::io::{ErrorKind, Read as _, Seek as _, SeekFrom, Write as _};
use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant};

use alloy_eips::BlockNumHash;
use alloy_primitives::{Address, B256, Bytes, Log, U256, hex};
use chibi_core::history::{
    Activity, At, BlockFacts, BlockRow, Capability, Coverage, LogRow, NonceChange, OpenState, Page,
    Ranges, ScannedBlock, SlotChange, Touch, TxRow, View, WithdrawalRow, decode,
};
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

/// The schema version recorded in `PRAGMA user_version`; 0 means no schema yet. Versions
/// 3..=99 only add to earlier ones, so a reader of older tables reads them all.
const SCHEMA_VERSION: i32 = 3;
/// The first version a reader cannot read: a breaking change.
const BREAKING_VERSION: i32 = 100;
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
/// What version 3 adds: the account history, permanent across resets. Each later version
/// only adds tables or columns. `history_backfill` is reserved and unused: the backward walk
/// restarts from a verified anchor instead of resuming from a hash read off disk.
const SCHEMA_V3: &str = "
CREATE TABLE history_block (
    number INTEGER PRIMARY KEY, hash BLOB NOT NULL CHECK (length(hash) = 32),
    timestamp INTEGER NOT NULL, beneficiary BLOB NOT NULL CHECK (length(beneficiary) = 20),
    base_fee INTEGER, excess_blob_gas INTEGER, tx_count INTEGER NOT NULL) STRICT;
CREATE TABLE history_tx (
    number INTEGER NOT NULL, tx_index INTEGER NOT NULL,
    hash BLOB NOT NULL UNIQUE CHECK (length(hash) = 32),
    sender BLOB NOT NULL CHECK (length(sender) = 20),
    raw BLOB,
    success INTEGER NOT NULL CHECK (success IN (0, 1)),
    cumulative_gas INTEGER NOT NULL, gas_used INTEGER NOT NULL,
    first_log_index INTEGER NOT NULL, log_count INTEGER NOT NULL,
    PRIMARY KEY (number, tx_index)) STRICT, WITHOUT ROWID;
CREATE INDEX history_tx_sender ON history_tx (sender, number);
CREATE TABLE history_log (
    number INTEGER NOT NULL, log_index INTEGER NOT NULL,
    address BLOB NOT NULL CHECK (length(address) = 20),
    topic0 BLOB CHECK (length(topic0) = 32), topic1 BLOB CHECK (length(topic1) = 32),
    topic2 BLOB CHECK (length(topic2) = 32), topic3 BLOB CHECK (length(topic3) = 32),
    data BLOB NOT NULL, PRIMARY KEY (number, log_index)) STRICT, WITHOUT ROWID;
CREATE TABLE history_touch (
    account BLOB NOT NULL CHECK (length(account) = 20),
    number INTEGER NOT NULL, access_index INTEGER NOT NULL,
    balance BLOB CHECK (length(balance) = 32), nonce INTEGER, code BLOB,
    PRIMARY KEY (account, number, access_index)) STRICT, WITHOUT ROWID;
CREATE INDEX history_touch_number ON history_touch (number);
CREATE TABLE history_slot (
    account BLOB NOT NULL CHECK (length(account) = 20),
    number INTEGER NOT NULL, access_index INTEGER NOT NULL,
    contract BLOB NOT NULL CHECK (length(contract) = 20),
    slot BLOB NOT NULL CHECK (length(slot) = 32),
    value BLOB NOT NULL CHECK (length(value) = 32),
    PRIMARY KEY (account, number, access_index, contract, slot)) STRICT, WITHOUT ROWID;
CREATE TABLE history_withdrawal (
    number INTEGER NOT NULL, withdrawal_index INTEGER NOT NULL,
    validator INTEGER NOT NULL, address BLOB NOT NULL CHECK (length(address) = 20),
    amount_gwei INTEGER NOT NULL, PRIMARY KEY (number, withdrawal_index)) STRICT, WITHOUT ROWID;
CREATE TABLE history_open (
    account BLOB NOT NULL CHECK (length(account) = 20),
    number INTEGER NOT NULL, hash BLOB NOT NULL CHECK (length(hash) = 32),
    balance BLOB NOT NULL CHECK (length(balance) = 32), nonce INTEGER NOT NULL,
    PRIMARY KEY (account, number)) STRICT, WITHOUT ROWID;
CREATE TABLE history_open_slot (
    account BLOB NOT NULL CHECK (length(account) = 20), number INTEGER NOT NULL,
    contract BLOB NOT NULL CHECK (length(contract) = 20),
    slot BLOB NOT NULL CHECK (length(slot) = 32), value BLOB NOT NULL CHECK (length(value) = 32),
    PRIMARY KEY (account, number, contract, slot)) STRICT, WITHOUT ROWID;
CREATE TABLE history_coverage (
    account BLOB NOT NULL CHECK (length(account) = 20),
    capability INTEGER NOT NULL CHECK (capability > 0),
    first INTEGER NOT NULL, last INTEGER NOT NULL, CHECK (first <= last),
    PRIMARY KEY (account, capability, first)) STRICT, WITHOUT ROWID;
CREATE TABLE history_unsettled (number INTEGER PRIMARY KEY,
    hash BLOB NOT NULL CHECK (length(hash) = 32)) STRICT;
CREATE TABLE history_settled (id INTEGER PRIMARY KEY CHECK (id = 0), number INTEGER NOT NULL,
    hash BLOB NOT NULL CHECK (length(hash) = 32)) STRICT;
CREATE TABLE history_backfill (id INTEGER PRIMARY KEY CHECK (id = 0),
    number INTEGER NOT NULL, hash BLOB NOT NULL CHECK (length(hash) = 32)) STRICT;
";
/// Each version's additions, in order: version `n` is the first `n` of them.
const SCHEMAS: [&str; SCHEMA_VERSION as usize] = [SCHEMA_V1, SCHEMA_V2, SCHEMA_V3];

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
    /// and the learned keys, in one transaction. Remembered state peers, the account
    /// history, the light-client checkpoint and the devp2p key stay. `None` when there is
    /// no database, which is left uncreated.
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

/// Account history writes. Each is one transaction on the writer's connection, so a failed
/// write leaves nothing behind. Blocks on fsync; call them off the async workers.
impl Disk {
    /// The settled mark's height: history at or below it no longer changes. `None` before
    /// history settles.
    pub(crate) fn history_settled(&self) -> Result<Option<u64>, NodeError> {
        self.history(|conn| Ok(settled(conn)?.map(|mark| mark.number)))
    }

    /// The blocks covered for `account` with `capability`.
    pub(crate) fn history_ranges(
        &self,
        account: Address,
        capability: Capability,
    ) -> Result<Ranges, NodeError> {
        self.history(|conn| {
            let mut select = conn.prepare_cached(
                "SELECT first, last FROM history_coverage WHERE account = ?1 AND capability = ?2",
            )?;
            let ranges = select
                .query_map(params![account.as_slice(), capability.code()], |row| {
                    Ok((row.get(0)?, row.get(1)?))
                })?
                .collect::<rusqlite::Result<Ranges>>()?;
            Ok(ranges)
        })
    }

    /// The lowest block a reconcile may still retract: the oldest unsettled block. `None` when
    /// nothing is unsettled.
    pub(crate) fn lowest_unsettled(&self) -> Result<Option<u64>, NodeError> {
        self.history(|conn| {
            Ok(
                conn.query_row("SELECT min(number) FROM history_unsettled", [], |row| {
                    row.get(0)
                })?,
            )
        })
    }

    /// Records consecutive verified blocks on the followed chain after `parent`, the verified
    /// parent of the first. Blocks history already holds with the same hash are skipped, so a
    /// replay writes nothing. The rest must extend the tip or start above it: a gap after a
    /// re-anchor is allowed, a fork below the tip needs [`Disk::reconcile_history`] first.
    pub(crate) fn record_head(
        &self,
        parent: BlockNumHash,
        blocks: &[ScannedBlock],
    ) -> Result<(), NodeError> {
        self.history(|conn| {
            check_chain(parent, blocks)?;
            let mut from = parent;
            let mut rest = blocks;
            while let [block, tail @ ..] = rest
                && followed_hash(conn, block.number)? == Some(block.hash)
            {
                from = BlockNumHash::new(block.number, block.hash);
                rest = tail;
            }
            if rest.is_empty() {
                return Ok(());
            }
            if let Some(tip) = history_tip(conn)?
                && (from.number < tip.number || from.number == tip.number && from.hash != tip.hash)
            {
                // The highest block history may still agree on: `from` itself when it is a
                // replayed block below the tip, the one under it when the tip's height forks.
                let agreed = if from.number < tip.number {
                    from.number
                } else {
                    from.number.saturating_sub(1)
                };
                return Err(HistoryError::Reconcile { above: agreed });
            }
            for block in rest {
                conn.execute(
                    "INSERT INTO history_unsettled (number, hash) VALUES (?1, ?2)",
                    params![block.number, block.hash.as_slice()],
                )?;
                if let Some(facts) = &block.facts {
                    insert_facts(conn, facts, &RECORD)?;
                }
            }
            union_coverage(conn, rest)?;
            Ok(())
        })
    }

    /// Settles history at `finalized`, or at the tip when the tip is lower, when the stored
    /// hash there agrees: unsettled rows at or below it are dropped and the mark only rises.
    /// Otherwise does nothing.
    pub(crate) fn settle_history(&self, finalized: BlockNumHash) -> Result<(), NodeError> {
        self.history(|conn| {
            let Some(tip) = history_tip(conn)? else {
                return Ok(());
            };
            let target = finalized.number.min(tip.number);
            let Some(hash) = followed_hash(conn, target)? else {
                return Ok(());
            };
            if target == finalized.number && hash != finalized.hash {
                return Ok(());
            }
            if settled(conn)?.is_none_or(|mark| mark.number < target) {
                conn.execute(
                    "INSERT INTO history_settled (id, number, hash) VALUES (0, ?1, ?2)
                     ON CONFLICT (id) DO UPDATE SET number = excluded.number, hash = excluded.hash",
                    params![target, hash.as_slice()],
                )?;
            }
            conn.execute("DELETE FROM history_unsettled WHERE number <= ?1", [target])?;
            Ok(())
        })
    }

    /// Retracts history that left the followed chain after a re-anchor. `ancestry` holds the
    /// verified hashes of the new chain by number. History is kept up to the newest unsettled
    /// block it agrees with, or up to the settled mark when none agrees; heights `ancestry`
    /// lacks are not evidence either way. Returns the new tip.
    pub(crate) fn reconcile_history(
        &self,
        ancestry: &BTreeMap<u64, B256>,
    ) -> Result<Option<BlockNumHash>, NodeError> {
        self.history(|conn| {
            let settled = settled(conn)?.map(|mark| mark.number);
            let mut keep = settled;
            {
                let mut select = conn
                    .prepare("SELECT number, hash FROM history_unsettled ORDER BY number DESC")?;
                for stored in select.query_map([], num_hash)? {
                    let stored = stored?;
                    if ancestry.get(&stored.number) == Some(&stored.hash) {
                        keep = Some(stored.number).max(settled);
                        break;
                    }
                }
            }
            retract_above(conn, keep)?;
            Ok(history_tip(conn)?)
        })
    }

    /// Adds blocks at or below the settled mark: the backward walk and supplements. Facts
    /// already stored are kept, except that a transaction stored without its bytes gets them
    /// once it is found to be an account's own. Coverage only grows, and a block whose hash
    /// differs from the one stored at its height is refused.
    pub(crate) fn augment_history(&self, blocks: &[ScannedBlock]) -> Result<(), NodeError> {
        self.history(|conn| {
            let settled = settled(conn)?;
            for block in blocks {
                let Some(mark) = settled else {
                    return Err(HistoryError::Refused(format!(
                        "block {} cannot be added before history settles",
                        block.number
                    )));
                };
                if block.number > mark.number {
                    return Err(HistoryError::Refused(format!(
                        "block {} is above the settled history at block {}",
                        block.number, mark.number
                    )));
                }
                check_facts(block)?;
                let stored = match block_hash(conn, block.number)? {
                    Some(hash) => Some(hash),
                    None => (mark.number == block.number).then_some(mark.hash),
                };
                if let Some(stored) = stored
                    && stored != block.hash
                {
                    return Err(HistoryError::Refused(format!(
                        "block {} has hash {} but history holds {stored}",
                        block.number, block.hash
                    )));
                }
                if let Some(facts) = &block.facts {
                    insert_facts(conn, facts, &AUGMENT)?;
                }
            }
            union_coverage(conn, blocks)?;
            Ok(())
        })
    }

    /// Records an account's proven state where its forward coverage opens. A state already
    /// recorded at that block is kept.
    pub(crate) fn open_history(&self, open: &OpenState) -> Result<(), NodeError> {
        self.history(|conn| {
            conn.execute(
                "INSERT OR IGNORE INTO history_open (account, number, hash, balance, nonce)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![
                    open.account.as_slice(),
                    open.number,
                    open.hash.as_slice(),
                    open.balance.to_be_bytes::<32>().as_slice(),
                    open.nonce
                ],
            )?;
            let mut insert = conn.prepare(
                "INSERT OR IGNORE INTO history_open_slot (account, number, contract, slot, value)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
            )?;
            for (contract, slot, value) in &open.slots {
                insert.execute(params![
                    open.account.as_slice(),
                    open.number,
                    contract.as_slice(),
                    slot.to_be_bytes::<32>().as_slice(),
                    value.to_be_bytes::<32>().as_slice()
                ])?;
            }
            Ok(())
        })
    }

    /// Runs `write` in one transaction on the writer's connection, committing only on success.
    fn history<R>(
        &self,
        write: impl FnOnce(&Connection) -> Result<R, HistoryError>,
    ) -> Result<R, NodeError> {
        let mut db = self.db.lock();
        in_transaction(&mut db.conn, write).map_err(|err| {
            let path = self.dir().join(DB_FILE);
            match err {
                HistoryError::Db(err) => db_error(&path, &err),
                HistoryError::Refused(why) => NodeError::Disk(format!("{}: {why}", path.display())),
                HistoryError::Reconcile { above } => NodeError::HistoryReconcile { above },
            }
        })
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
    let fail = |err: rusqlite::Error| read_error(&path, &err);
    let mut conn = open_read_only(&path)?;
    let tx = conn.transaction().map_err(fail)?;
    match schema_version(&tx).map_err(fail)? {
        0 => read_legacy(dir),
        1..BREAKING_VERSION => load(&tx).map(Some).map_err(fail),
        newer => Err(newer_schema(&path, newer)),
    }
}

/// Opens the database at `path` for reading beside a running writer: read-only, so it never
/// creates files or changes modes, and waiting for the writer's lock like the writer waits.
fn open_read_only(path: &Path) -> Result<Connection, NodeError> {
    let flags = OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX;
    let conn = Connection::open_with_flags(path, flags).map_err(|err| read_error(path, &err))?;
    conn.busy_timeout(BUSY_TIMEOUT)
        .map_err(|err| read_error(path, &err))?;
    Ok(conn)
}

/// A read-only connection's failure, naming the one cause a reader cannot recover from.
fn read_error(path: &Path, err: &rusqlite::Error) -> NodeError {
    match err.sqlite_error() {
        // A crashed writer left a hot journal, which a read-only connection cannot roll back.
        Some(failure) if failure.extended_code == rusqlite::ffi::SQLITE_READONLY_ROLLBACK => {
            NodeError::Disk(format!(
                "{}: interrupted write; start the node once to recover the database",
                path.display()
            ))
        }
        _ => db_error(path, err),
    }
}

/// The first schema version with the account history tables.
const HISTORY_VERSION: i32 = 3;
/// The highest block number SQLite can bind: stored numbers are below 2^63.
const UNBOUNDED: u64 = i64::MAX.unsigned_abs();

/// The account history in a data directory's database, read without taking the lock, beside a
/// running writer. Each call reads in its own transaction, so a call sees one consistent state
/// and nothing holds the database between calls.
#[derive(Debug)]
pub struct History {
    conn: Connection,
    path: PathBuf,
}

impl History {
    /// Opens the history in `dir` read-only. `None` when there is no database yet or it
    /// predates the history tables; an error for a schema newer than this chibi reads.
    pub fn open(dir: &Path) -> Result<Option<History>, NodeError> {
        let path = dir.join(DB_FILE);
        if !path.try_exists().map_err(|err| io_error(&path, &err))? {
            return Ok(None);
        }
        let conn = open_read_only(&path)?;
        match schema_version(&conn).map_err(|err| read_error(&path, &err))? {
            i32::MIN..HISTORY_VERSION => Ok(None),
            HISTORY_VERSION..BREAKING_VERSION => Ok(Some(History { conn, path })),
            newer => Err(newer_schema(&path, newer)),
        }
    }

    /// The account's whole blocks below `before` (from the newest when `None`), newest first,
    /// adding blocks until the page holds at least `rows` of its touches. A block is never
    /// split, so a page may hold more.
    pub fn page(
        &self,
        account: Address,
        before: Option<u64>,
        rows: NonZeroUsize,
    ) -> Result<Page, NodeError> {
        self.read(|conn| read_page(conn, account, before, rows))
    }

    /// The account's coverage per capability, the settled mark, its open states and its own
    /// sends.
    pub fn coverage(&self, account: Address) -> Result<Coverage, NodeError> {
        self.read(|conn| read_coverage(conn, account))
    }

    /// The account's coverage, with the send check's input, and its newest `activities`
    /// activities, newest first, read in one transaction so a writer between them cannot make
    /// them disagree.
    pub fn view(&self, account: Address, activities: usize) -> Result<View, NodeError> {
        self.view_between(account, activities, || {})
    }

    /// [`History::view`], running `between` after the coverage is read and before the
    /// activities are, so a test can write there.
    fn view_between(
        &self,
        account: Address,
        activities: usize,
        between: impl FnOnce(),
    ) -> Result<View, NodeError> {
        self.read(|conn| {
            let coverage = read_coverage(conn, account)?;
            between();
            let activities = read_activities(conn, account, activities)?;
            Ok(View {
                coverage,
                activities,
            })
        })
    }

    /// Runs `read` in one deferred transaction, ended before returning.
    fn read<R>(
        &self,
        read: impl FnOnce(&Connection) -> rusqlite::Result<R>,
    ) -> Result<R, NodeError> {
        let fail = |err: rusqlite::Error| read_error(&self.path, &err);
        let tx = self.conn.unchecked_transaction().map_err(fail)?;
        let result = read(&tx).map_err(fail)?;
        tx.commit().map_err(fail)?;
        Ok(result)
    }
}

/// The newest `count` activities of `account`, newest first, decoded from as many pages as
/// hold them.
fn read_activities(
    conn: &Connection,
    account: Address,
    count: usize,
) -> rusqlite::Result<Vec<Activity>> {
    let rows = NonZeroUsize::MIN.saturating_add(count.saturating_sub(1));
    let mut found = Vec::with_capacity(count);
    let mut before = None;
    while found.len() < count {
        let page = read_page(conn, account, before, rows)?;
        found.extend(decode(&page));
        match page.next {
            Some(next) => before = Some(next),
            None => break,
        }
    }
    found.truncate(count);
    Ok(found)
}

fn read_page(
    conn: &Connection,
    account: Address,
    before: Option<u64>,
    rows: NonZeroUsize,
) -> rusqlite::Result<Page> {
    let below = before.map_or(UNBOUNDED, |number| number.min(UNBOUNDED));
    let mut numbers = Vec::new();
    {
        let mut select = conn.prepare_cached(
            "SELECT number, count(*) FROM history_touch WHERE account = ?1 AND number < ?2
             GROUP BY number ORDER BY number DESC",
        )?;
        let mut found = select.query(params![account.as_slice(), below])?;
        let mut touches = 0_usize;
        while touches < rows.get()
            && let Some(row) = found.next()?
        {
            numbers.push(row.get::<_, u64>(0)?);
            touches = touches.saturating_add(row.get(1)?);
        }
    }
    let next = match numbers.last() {
        Some(&lowest) => conn
            .prepare_cached(
                "SELECT 1 FROM history_touch WHERE account = ?1 AND number < ?2 LIMIT 1",
            )?
            .exists(params![account.as_slice(), lowest])?
            .then_some(lowest),
        None => None,
    };
    let floor = numbers.last().copied().unwrap_or(below);
    let newest = numbers.first().copied();
    let mut blocks = Vec::with_capacity(numbers.len());
    for number in numbers {
        blocks.extend(read_block(conn, account, number)?);
    }
    let opening_balance = opening_balance(conn, account, floor)?;
    let (bal, opens) = match newest {
        Some(newest) => {
            let from = opening_balance.map_or(floor, |(number, _)| number);
            let bal = conn
                .prepare_cached(
                    "SELECT first, last FROM history_coverage
                     WHERE account = ?1 AND capability = ?2 AND last >= ?3 AND first <= ?4",
                )?
                .query_map(
                    params![account.as_slice(), Capability::Bal.code(), from, newest],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )?
                .collect::<rusqlite::Result<Ranges>>()?;
            let opens = parsed(
                conn,
                "SELECT number, balance FROM history_open
                 WHERE account = ?1 AND number >= ?2 AND number < ?3 ORDER BY number",
                params![account.as_slice(), floor, newest],
                "history_open",
                number_balance,
            )?;
            (bal, opens)
        }
        None => (Ranges::default(), Vec::new()),
    };
    Ok(Page {
        account,
        blocks,
        opening_balance,
        bal,
        opens,
        next,
    })
}

/// What block `number` holds for `account`: its touches, slots and withdrawals, the
/// transactions at its touched indices and those transactions' stored logs.
fn read_block(
    conn: &Connection,
    account: Address,
    number: u64,
) -> rusqlite::Result<Option<BlockFacts>> {
    let Some(block) = parsed(
        conn,
        "SELECT hash, timestamp, beneficiary, base_fee, excess_blob_gas, tx_count
         FROM history_block WHERE number = ?1",
        [number],
        "history_block",
        |row| {
            let (Some(hash), Some(beneficiary)) = (fixed::<32>(row, 0)?, fixed::<20>(row, 2)?)
            else {
                return Ok(None);
            };
            Ok(Some(BlockRow {
                number,
                hash: hash.into(),
                timestamp: row.get(1)?,
                beneficiary: beneficiary.into(),
                base_fee: row.get(3)?,
                excess_blob_gas: row.get(4)?,
                tx_count: row.get(5)?,
            }))
        },
    )?
    .pop() else {
        warn!(target: "chibi::disk", number, "skipping history touches of a block with no block row");
        return Ok(None);
    };
    let key = params![account.as_slice(), number];
    let touches = parsed(
        conn,
        "SELECT access_index, balance, nonce, code FROM history_touch
         WHERE account = ?1 AND number = ?2 ORDER BY access_index",
        key,
        "history_touch",
        |row| {
            let (Some(at), Some(balance)) = (
                At::from_index(row.get(0)?, block.tx_count),
                nullable::<32>(row, 1)?,
            ) else {
                return Ok(None);
            };
            Ok(Some(Touch {
                account,
                at,
                balance: balance.map(U256::from_be_bytes),
                nonce: row.get(2)?,
                code: row.get::<_, Option<Vec<u8>>>(3)?.map(Bytes::from),
            }))
        },
    )?;
    let slots = parsed(
        conn,
        "SELECT access_index, contract, slot, value FROM history_slot
         WHERE account = ?1 AND number = ?2 ORDER BY access_index, contract, slot",
        key,
        "history_slot",
        |row| {
            let (Some(at), Some(contract), Some(slot), Some(value)) = (
                At::from_index(row.get(0)?, block.tx_count),
                fixed::<20>(row, 1)?,
                fixed::<32>(row, 2)?,
                fixed::<32>(row, 3)?,
            ) else {
                return Ok(None);
            };
            Ok(Some(SlotChange {
                account,
                at,
                contract: contract.into(),
                slot: U256::from_be_bytes(slot),
                value: U256::from_be_bytes(value),
            }))
        },
    )?;
    let withdrawals = parsed(
        conn,
        "SELECT withdrawal_index, validator, amount_gwei FROM history_withdrawal
         WHERE address = ?1 AND number = ?2 ORDER BY withdrawal_index",
        key,
        "history_withdrawal",
        |row| {
            Ok(Some(WithdrawalRow {
                index: row.get(0)?,
                validator: row.get(1)?,
                address: account,
                amount_gwei: row.get(2)?,
            }))
        },
    )?;
    let mut txs = Vec::new();
    let mut logs = Vec::new();
    for touch in &touches {
        let index = match touch.at {
            At::Tx(index) => index,
            At::System | At::Post => continue,
        };
        let Some(tx) = parsed(
            conn,
            "SELECT tx_index, hash, sender, raw, success, cumulative_gas, gas_used,
             first_log_index, log_count FROM history_tx WHERE number = ?1 AND tx_index = ?2",
            params![number, index],
            "history_tx",
            tx_row,
        )?
        .pop() else {
            continue;
        };
        let end = tx
            .first_log_index
            .saturating_add(tx.log_count)
            .min(UNBOUNDED);
        logs.extend(parsed(
            conn,
            "SELECT log_index, address, topic0, topic1, topic2, topic3, data FROM history_log
             WHERE number = ?1 AND log_index >= ?2 AND log_index < ?3 ORDER BY log_index",
            params![number, tx.first_log_index, end],
            "history_log",
            log_row,
        )?);
        txs.push(tx);
    }
    Ok(Some(BlockFacts {
        block,
        txs,
        logs,
        touches,
        slots,
        withdrawals,
    }))
}

/// The newest known balance of `account` below block `below`: the newer of its newest
/// balance-bearing touch and its newest open state, the touch when both are at one block.
fn opening_balance(
    conn: &Connection,
    account: Address,
    below: u64,
) -> rusqlite::Result<Option<(u64, U256)>> {
    let key = params![account.as_slice(), below];
    let touch = parsed(
        conn,
        "SELECT number, balance FROM history_touch
         WHERE account = ?1 AND number < ?2 AND balance IS NOT NULL
         ORDER BY number DESC, access_index DESC LIMIT 1",
        key,
        "history_touch",
        number_balance,
    )?
    .pop();
    let open = parsed(
        conn,
        "SELECT number, balance FROM history_open WHERE account = ?1 AND number < ?2
         ORDER BY number DESC LIMIT 1",
        key,
        "history_open",
        number_balance,
    )?
    .pop();
    Ok(match (touch, open) {
        (Some(touch), Some(open)) if open.0 > touch.0 => Some(open),
        (touch, open) => touch.or(open),
    })
}

fn read_coverage(conn: &Connection, account: Address) -> rusqlite::Result<Coverage> {
    let mut by_code: BTreeMap<u32, (Capability, Vec<(u64, u64)>)> = BTreeMap::new();
    {
        let mut select = conn.prepare_cached(
            "SELECT capability, first, last FROM history_coverage WHERE account = ?1",
        )?;
        let mut rows = select.query([account.as_slice()])?;
        while let Some(row) = rows.next()? {
            let code = u32::try_from(row.get::<_, i64>(0)?).ok();
            // Codes this version does not know belong to a later one and are ignored.
            let Some(capability) = code.and_then(Capability::from_code) else {
                continue;
            };
            by_code
                .entry(capability.code())
                .or_insert_with(|| (capability, Vec::new()))
                .1
                .push((row.get(1)?, row.get(2)?));
        }
    }
    let ranges = by_code
        .into_values()
        .map(|(capability, rows)| (capability, rows.into_iter().collect()))
        .collect();
    let mut opens = parsed(
        conn,
        "SELECT number, hash, balance, nonce FROM history_open WHERE account = ?1
         ORDER BY number",
        [account.as_slice()],
        "history_open",
        |row| {
            let (Some(hash), Some(balance)) = (fixed::<32>(row, 1)?, fixed::<32>(row, 2)?) else {
                return Ok(None);
            };
            Ok(Some(OpenState {
                account,
                number: row.get(0)?,
                hash: hash.into(),
                balance: U256::from_be_bytes(balance),
                nonce: row.get(3)?,
                slots: Vec::new(),
            }))
        },
    )?;
    for open in &mut opens {
        open.slots = parsed(
            conn,
            "SELECT contract, slot, value FROM history_open_slot
             WHERE account = ?1 AND number = ?2 ORDER BY contract, slot",
            params![account.as_slice(), open.number],
            "history_open_slot",
            |row| {
                let (Some(contract), Some(slot), Some(value)) = (
                    fixed::<20>(row, 0)?,
                    fixed::<32>(row, 1)?,
                    fixed::<32>(row, 2)?,
                ) else {
                    return Ok(None);
                };
                Ok(Some((
                    contract.into(),
                    U256::from_be_bytes(slot),
                    U256::from_be_bytes(value),
                )))
            },
        )?;
    }
    // Every transaction relevant to the account has a touch at its index, and those stored
    // whole carry the account's sends and authorizations.
    let txs = parsed(
        conn,
        "SELECT x.tx_index, x.hash, x.sender, x.raw, x.success, x.cumulative_gas, x.gas_used,
         x.first_log_index, x.log_count, x.number
         FROM history_touch AS t JOIN history_tx AS x
         ON x.number = t.number AND x.tx_index = t.access_index - 1
         WHERE t.account = ?1 AND x.raw IS NOT NULL
         ORDER BY x.number, x.tx_index",
        [account.as_slice()],
        "history_tx",
        |row| {
            let Some(tx) = tx_row(row)? else {
                return Ok(None);
            };
            Ok(Some((row.get(9)?, tx)))
        },
    )?;
    let nonces = parsed(
        conn,
        "SELECT number, access_index, nonce FROM history_touch
         WHERE account = ?1 AND nonce IS NOT NULL ORDER BY number, access_index",
        [account.as_slice()],
        "history_touch",
        |row| {
            Ok(Some(NonceChange {
                number: row.get(0)?,
                index: row.get(1)?,
                nonce: row.get(2)?,
            }))
        },
    )?;
    Ok(Coverage {
        account,
        ranges,
        settled: settled(conn)?.map(|mark| mark.number),
        opens,
        txs,
        nonces,
    })
}

/// Runs `sql` and keeps the rows `parse` reads, warning about each row it cannot: a stored
/// value of the wrong shape loses that row, not the whole read.
fn parsed<T>(
    conn: &Connection,
    sql: &str,
    params: impl rusqlite::Params,
    table: &'static str,
    mut parse: impl FnMut(&rusqlite::Row<'_>) -> rusqlite::Result<Option<T>>,
) -> rusqlite::Result<Vec<T>> {
    let mut select = conn.prepare_cached(sql)?;
    let mut rows = select.query(params)?;
    let mut parsed = Vec::new();
    while let Some(row) = rows.next()? {
        match parse(row)? {
            Some(value) => parsed.push(value),
            None => {
                warn!(target: "chibi::disk", table, "skipping a history row that does not parse")
            }
        }
    }
    Ok(parsed)
}

/// Column `column` as a blob of exactly `N` bytes, `None` for any other value.
fn fixed<const N: usize>(
    row: &rusqlite::Row<'_>,
    column: usize,
) -> rusqlite::Result<Option<[u8; N]>> {
    Ok(row
        .get_ref(column)?
        .as_blob()
        .ok()
        .and_then(|blob| blob.try_into().ok()))
}

/// Column `column` as NULL or a blob of exactly `N` bytes, `None` for any other value.
fn nullable<const N: usize>(
    row: &rusqlite::Row<'_>,
    column: usize,
) -> rusqlite::Result<Option<Option<[u8; N]>>> {
    Ok(match row.get_ref(column)?.as_blob_or_null() {
        Ok(None) => Some(None),
        Ok(Some(blob)) => <[u8; N]>::try_from(blob).ok().map(Some),
        Err(_) => None,
    })
}

fn number_balance(row: &rusqlite::Row<'_>) -> rusqlite::Result<Option<(u64, U256)>> {
    let Some(balance) = fixed::<32>(row, 1)? else {
        return Ok(None);
    };
    Ok(Some((row.get(0)?, U256::from_be_bytes(balance))))
}

/// A `history_tx` row selected as `tx_index, hash, sender, raw, success, cumulative_gas,
/// gas_used, first_log_index, log_count`.
fn tx_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Option<TxRow>> {
    let (Some(hash), Some(sender)) = (fixed::<32>(row, 1)?, fixed::<20>(row, 2)?) else {
        return Ok(None);
    };
    Ok(Some(TxRow {
        index: row.get(0)?,
        hash: hash.into(),
        sender: sender.into(),
        raw: row.get::<_, Option<Vec<u8>>>(3)?.map(Bytes::from),
        success: row.get(4)?,
        cumulative_gas: row.get(5)?,
        gas_used: row.get(6)?,
        first_log_index: row.get(7)?,
        log_count: row.get(8)?,
    }))
}

/// A `history_log` row selected as `log_index, address, topic0, topic1, topic2, topic3, data`.
/// Topics are stored from topic0 up, so a topic after a missing one is malformed.
fn log_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Option<LogRow>> {
    let Some(address) = fixed::<20>(row, 1)? else {
        return Ok(None);
    };
    let mut topics = Vec::with_capacity(4);
    let mut ended = false;
    for column in 2..6 {
        match nullable::<32>(row, column)? {
            Some(None) => ended = true,
            Some(Some(topic)) if !ended => topics.push(B256::from(topic)),
            Some(Some(_)) | None => return Ok(None),
        }
    }
    Ok(Some(LogRow {
        index: row.get(0)?,
        log: Log::new_unchecked(
            address.into(),
            topics,
            Bytes::from(row.get::<_, Vec<u8>>(6)?),
        ),
    }))
}

/// Opens the writer's connection to `dir`'s database. A new database is owner-only and gets
/// the schema and the contents of a legacy state file if there is one; an older one is
/// upgraded.
fn open_db(dir: &Path) -> Result<Db, NodeError> {
    let path = dir.join(DB_FILE);
    let fail = |err| db_error(&path, &err);
    match private_file(&path) {
        Ok(_) => {}
        Err(err) if err.kind() == ErrorKind::AlreadyExists => {}
        Err(err) => return Err(io_error(&path, &err)),
    }
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
        SCHEMA_VERSION => {}
        older @ 1..SCHEMA_VERSION => upgrade(&mut conn, &path, older)?,
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
    for schema in SCHEMAS {
        tx.execute_batch(schema)?;
    }
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

/// Adds every later version's tables to a database at version `from` in one transaction,
/// making the file owner-only as it reaches version 3: earlier versions created it with the
/// default mode, and later opens leave whatever mode the user sets.
fn upgrade(conn: &mut Connection, path: &Path, from: i32) -> Result<(), NodeError> {
    let fail = |err| db_error(path, &err);
    let tx = conn.transaction().map_err(fail)?;
    for schema in &SCHEMAS[from as usize..] {
        tx.execute_batch(schema).map_err(fail)?;
    }
    tx.pragma_update(None, "user_version", SCHEMA_VERSION)
        .map_err(fail)?;
    #[cfg(unix)]
    if from < 3 {
        use std::os::unix::fs::PermissionsExt as _;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))
            .map_err(|err| io_error(path, &err))?;
    }
    tx.commit().map_err(fail)
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

/// Why a history write failed: the database, a forward write that forks from the stored
/// history, or another write the stored history refuses.
enum HistoryError {
    Db(rusqlite::Error),
    Reconcile { above: u64 },
    Refused(String),
}

impl From<rusqlite::Error> for HistoryError {
    fn from(err: rusqlite::Error) -> Self {
        Self::Db(err)
    }
}

fn in_transaction<R>(
    conn: &mut Connection,
    write: impl FnOnce(&Connection) -> Result<R, HistoryError>,
) -> Result<R, HistoryError> {
    let tx = conn.transaction()?;
    let result = write(&tx)?;
    tx.commit()?;
    Ok(result)
}

fn num_hash(row: &rusqlite::Row<'_>) -> rusqlite::Result<BlockNumHash> {
    Ok(BlockNumHash::new(
        row.get(0)?,
        B256::from(row.get::<_, [u8; 32]>(1)?),
    ))
}

fn history_tip(conn: &Connection) -> rusqlite::Result<Option<BlockNumHash>> {
    let newest = conn
        .query_row(
            "SELECT number, hash FROM history_unsettled ORDER BY number DESC LIMIT 1",
            [],
            num_hash,
        )
        .optional()?;
    match newest {
        Some(tip) => Ok(Some(tip)),
        None => settled(conn),
    }
}

fn settled(conn: &Connection) -> rusqlite::Result<Option<BlockNumHash>> {
    conn.query_row("SELECT number, hash FROM history_settled", [], num_hash)
        .optional()
}

/// The hash history holds for block `number` of the followed chain: its unsettled row, or the
/// settled mark at that height.
fn followed_hash(conn: &Connection, number: u64) -> rusqlite::Result<Option<B256>> {
    let unsettled = conn
        .query_row(
            "SELECT hash FROM history_unsettled WHERE number = ?1",
            [number],
            |row| row.get::<_, [u8; 32]>(0),
        )
        .optional()?;
    match unsettled {
        Some(hash) => Ok(Some(B256::from(hash))),
        None => Ok(settled(conn)?
            .filter(|mark| mark.number == number)
            .map(|mark| mark.hash)),
    }
}

fn block_hash(conn: &Connection, number: u64) -> rusqlite::Result<Option<B256>> {
    conn.query_row(
        "SELECT hash FROM history_block WHERE number = ?1",
        [number],
        |row| row.get::<_, [u8; 32]>(0),
    )
    .optional()
    .map(|hash| hash.map(B256::from))
}

/// Refuses blocks that are not consecutive children of `parent`, or whose facts do not fit.
fn check_chain(parent: BlockNumHash, blocks: &[ScannedBlock]) -> Result<(), HistoryError> {
    let mut previous = parent;
    for block in blocks {
        if previous.number.checked_add(1) != Some(block.number)
            || block.parent_hash != previous.hash
        {
            return Err(HistoryError::Refused(format!(
                "block {} ({}) does not extend block {} ({})",
                block.number, block.hash, previous.number, previous.hash
            )));
        }
        check_facts(block)?;
        previous = BlockNumHash::new(block.number, block.hash);
    }
    Ok(())
}

/// Refuses facts recorded for another block, and logs the table cannot hold.
fn check_facts(block: &ScannedBlock) -> Result<(), HistoryError> {
    let Some(facts) = &block.facts else {
        return Ok(());
    };
    if facts.block.number != block.number || facts.block.hash != block.hash {
        return Err(HistoryError::Refused(format!(
            "block {} carries the facts of block {} ({})",
            block.number, facts.block.number, facts.block.hash
        )));
    }
    if let Some(row) = facts.logs.iter().find(|row| row.log.topics().len() > 4) {
        return Err(HistoryError::Refused(format!(
            "log {} of block {} has more than four topics",
            row.index, block.number
        )));
    }
    Ok(())
}

/// The statements that store a block's facts, under one conflict rule.
struct FactSql {
    block: &'static str,
    tx: &'static str,
    log: &'static str,
    touch: &'static str,
    slot: &'static str,
    withdrawal: &'static str,
}

macro_rules! fact_sql {
    ($insert:literal, $tx_insert:literal, $tx_conflict:literal) => {
        FactSql {
            block: concat!(
                $insert,
                " INTO history_block (number, hash, timestamp, beneficiary, base_fee,
                 excess_blob_gas, tx_count) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)"
            ),
            tx: concat!(
                $tx_insert,
                " INTO history_tx (number, tx_index, hash, sender, raw, success, cumulative_gas,
                 gas_used, first_log_index, log_count)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
                $tx_conflict
            ),
            log: concat!(
                $insert,
                " INTO history_log (number, log_index, address, topic0, topic1, topic2, topic3,
                 data) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)"
            ),
            touch: concat!(
                $insert,
                " INTO history_touch (account, number, access_index, balance, nonce, code)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)"
            ),
            slot: concat!(
                $insert,
                " INTO history_slot (account, number, access_index, contract, slot, value)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)"
            ),
            withdrawal: concat!(
                $insert,
                " INTO history_withdrawal (number, withdrawal_index, validator, address,
                 amount_gwei) VALUES (?1, ?2, ?3, ?4, ?5)"
            ),
        }
    };
}

/// Forward writes: blocks above the tip hold no facts yet, so a conflict is a bug to surface.
const RECORD: FactSql = fact_sql!("INSERT", "INSERT", "");
/// Backward writes and supplements keep the facts already stored. A transaction first stored
/// for an account it only mentions has no bytes; storing it as another account's own adds them.
const AUGMENT: FactSql = fact_sql!(
    "INSERT OR IGNORE",
    "INSERT",
    " ON CONFLICT DO UPDATE SET raw = COALESCE(history_tx.raw, excluded.raw)"
);

fn insert_facts(conn: &Connection, facts: &BlockFacts, sql: &FactSql) -> rusqlite::Result<()> {
    let block = &facts.block;
    conn.prepare_cached(sql.block)?.execute(params![
        block.number,
        block.hash.as_slice(),
        block.timestamp,
        block.beneficiary.as_slice(),
        block.base_fee,
        block.excess_blob_gas,
        block.tx_count
    ])?;
    let mut insert = conn.prepare_cached(sql.tx)?;
    for tx in &facts.txs {
        insert.execute(params![
            block.number,
            tx.index,
            tx.hash.as_slice(),
            tx.sender.as_slice(),
            tx.raw.as_ref().map(|raw| &raw[..]),
            tx.success,
            tx.cumulative_gas,
            tx.gas_used,
            tx.first_log_index,
            tx.log_count
        ])?;
    }
    let mut insert = conn.prepare_cached(sql.log)?;
    for row in &facts.logs {
        let topic = |i: usize| row.log.topics().get(i).map(B256::as_slice);
        insert.execute(params![
            block.number,
            row.index,
            row.log.address.as_slice(),
            topic(0),
            topic(1),
            topic(2),
            topic(3),
            &row.log.data.data[..]
        ])?;
    }
    let mut insert = conn.prepare_cached(sql.touch)?;
    for touch in &facts.touches {
        let balance = touch.balance.map(|balance| balance.to_be_bytes::<32>());
        insert.execute(params![
            touch.account.as_slice(),
            block.number,
            touch.at.index(block.tx_count),
            balance.as_ref().map(<[u8; 32]>::as_slice),
            touch.nonce,
            touch.code.as_ref().map(|code| &code[..])
        ])?;
    }
    let mut insert = conn.prepare_cached(sql.slot)?;
    for change in &facts.slots {
        insert.execute(params![
            change.account.as_slice(),
            block.number,
            change.at.index(block.tx_count),
            change.contract.as_slice(),
            change.slot.to_be_bytes::<32>().as_slice(),
            change.value.to_be_bytes::<32>().as_slice()
        ])?;
    }
    let mut insert = conn.prepare_cached(sql.withdrawal)?;
    for withdrawal in &facts.withdrawals {
        insert.execute(params![
            block.number,
            withdrawal.index,
            withdrawal.validator,
            withdrawal.address.as_slice(),
            withdrawal.amount_gwei
        ])?;
    }
    Ok(())
}

/// Adds each block to the coverage of every (account, capability) it was scanned for,
/// rewriting each set's rows once so they stay disjoint and merged.
fn union_coverage(conn: &Connection, blocks: &[ScannedBlock]) -> rusqlite::Result<()> {
    let mut added: BTreeMap<(Address, u32), Ranges> = BTreeMap::new();
    for block in blocks {
        for &account in &block.accounts {
            for capability in &block.capabilities {
                added
                    .entry((account, capability.code()))
                    .or_default()
                    .insert(block.number, block.number);
            }
        }
    }
    let mut select = conn.prepare_cached(
        "SELECT first, last FROM history_coverage WHERE account = ?1 AND capability = ?2",
    )?;
    let mut delete =
        conn.prepare_cached("DELETE FROM history_coverage WHERE account = ?1 AND capability = ?2")?;
    let mut insert = conn.prepare_cached(
        "INSERT INTO history_coverage (account, capability, first, last) VALUES (?1, ?2, ?3, ?4)",
    )?;
    for ((account, code), ranges) in added {
        let key = params![account.as_slice(), code];
        let mut stored = select
            .query_map(key, |row| Ok((row.get(0)?, row.get(1)?)))?
            .collect::<rusqlite::Result<Ranges>>()?;
        for (first, last) in ranges.iter() {
            stored.insert(first, last);
        }
        delete.execute(key)?;
        for (first, last) in stored.iter() {
            insert.execute(params![account.as_slice(), code, first, last])?;
        }
    }
    Ok(())
}

/// Every table keyed by block number, cleared from a height up.
const RETRACT: [&str; 9] = [
    "DELETE FROM history_block WHERE number >= ?1",
    "DELETE FROM history_tx WHERE number >= ?1",
    "DELETE FROM history_log WHERE number >= ?1",
    "DELETE FROM history_touch WHERE number >= ?1",
    "DELETE FROM history_slot WHERE number >= ?1",
    "DELETE FROM history_withdrawal WHERE number >= ?1",
    "DELETE FROM history_open WHERE number >= ?1",
    "DELETE FROM history_open_slot WHERE number >= ?1",
    "DELETE FROM history_unsettled WHERE number >= ?1",
];

/// Deletes everything history holds above block `keep`, or all of it for `None`, and trims
/// coverage to match. The settled mark stays.
fn retract_above(conn: &Connection, keep: Option<u64>) -> rusqlite::Result<()> {
    let Some(lowest) = keep.map_or(Some(0), |keep| keep.checked_add(1)) else {
        return Ok(());
    };
    for statement in RETRACT {
        conn.execute(statement, [lowest])?;
    }
    conn.execute("DELETE FROM history_coverage WHERE first >= ?1", [lowest])?;
    if let Some(keep) = keep {
        conn.execute(
            "UPDATE history_coverage SET last = ?1 WHERE last > ?1",
            [keep],
        )?;
    }
    Ok(())
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

    const HISTORY_TABLES: [&str; 12] = [
        "history_backfill",
        "history_block",
        "history_coverage",
        "history_log",
        "history_open",
        "history_open_slot",
        "history_settled",
        "history_slot",
        "history_touch",
        "history_tx",
        "history_unsettled",
        "history_withdrawal",
    ];

    fn history_tables(conn: &Connection) -> Vec<String> {
        let mut stmt = conn
            .prepare("SELECT name FROM sqlite_schema WHERE type = 'table' AND name LIKE 'history_%' ORDER BY name")
            .unwrap();
        stmt.query_map([], |row| row.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap()
    }

    /// Rewrites `dir`'s database as the given older version would have left it.
    fn downgrade(dir: &Path, version: i32) {
        let conn = Connection::open(dir.join(DB_FILE)).unwrap();
        for table in HISTORY_TABLES {
            conn.execute_batch(&format!("DROP TABLE {table}")).unwrap();
        }
        if version == 1 {
            conn.execute_batch("DROP TABLE keys").unwrap();
        }
        conn.pragma_update(None, "user_version", version).unwrap();
    }

    fn set_version(dir: &Path, version: i32) {
        let conn = Connection::open(dir.join(DB_FILE)).unwrap();
        conn.pragma_update(None, "user_version", version).unwrap();
    }

    #[test]
    fn a_new_database_is_at_version_3_and_keeps_learned_keys_in_order() {
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
        assert_eq!(schema_version(&disk.db.lock().conn).unwrap(), 3);
    }

    #[test]
    fn a_new_database_has_every_history_table_and_they_are_strict() {
        let dir = tempfile::tempdir().unwrap();
        let disk = Disk::open(dir.path()).unwrap();
        let db = disk.db.lock();
        assert_eq!(history_tables(&db.conn), HISTORY_TABLES);
        let text_number = db.conn.execute(
            "INSERT INTO history_unsettled (number, hash) VALUES ('ten', zeroblob(32))",
            [],
        );
        assert!(text_number.is_err(), "a STRICT table rejects a text number");
    }

    #[test]
    fn a_version_1_database_upgrades_to_version_3_and_keeps_its_state() {
        let dir = tempfile::tempdir().unwrap();
        {
            let disk = Disk::open(dir.path()).unwrap();
            disk.save(|saved| {
                saved.set_outbox(vec![queued(1, 9)]);
                saved.remember_peers(&[record(1, 30303)]);
            })
            .unwrap();
        }
        downgrade(dir.path(), 1);
        assert!(
            read_state(dir.path()).unwrap().is_some(),
            "status reads version 1"
        );

        let disk = Disk::open(dir.path()).unwrap();
        disk.read(|saved| {
            assert_eq!(saved.outbox(), &[queued(1, 9)]);
            assert_eq!(saved.peers(), vec![record(1, 30303)]);
        });
        assert_eq!(disk.load_keys().unwrap(), vec![]);
        disk.save_keys(&[Key::Account(Address::repeat_byte(1))])
            .unwrap();
        let db = disk.db.lock();
        assert_eq!(schema_version(&db.conn).unwrap(), 3);
        assert_eq!(history_tables(&db.conn), HISTORY_TABLES);
    }

    #[test]
    fn a_version_2_database_upgrades_to_version_3_and_keeps_its_state() {
        let dir = tempfile::tempdir().unwrap();
        let snapshot: Snapshot = serde_json::from_str(&pre_contracts_snapshot_json()).unwrap();
        let keys = [Key::Account(Address::repeat_byte(4))];
        {
            let disk = Disk::open(dir.path()).unwrap();
            disk.save(|saved| {
                saved.set_outbox(vec![queued(1, 9)]);
                saved.set_snapshot(snapshot);
                saved.remember_peers(&[record(1, 30303)]);
            })
            .unwrap();
            disk.save_keys(&keys).unwrap();
        }
        downgrade(dir.path(), 2);

        let disk = Disk::open(dir.path()).unwrap();
        disk.read(|saved| {
            assert_eq!(saved.outbox(), &[queued(1, 9)]);
            assert_eq!(saved.peers(), vec![record(1, 30303)]);
            assert_pre_contracts_snapshot(saved.snapshot().unwrap());
        });
        assert_eq!(disk.load_keys().unwrap(), keys);
        let db = disk.db.lock();
        assert_eq!(schema_version(&db.conn).unwrap(), 3);
        assert_eq!(history_tables(&db.conn), HISTORY_TABLES);
    }

    #[test]
    fn the_writer_refuses_version_4_and_status_reads_it_until_version_100() {
        let dir = tempfile::tempdir().unwrap();
        {
            let disk = Disk::open(dir.path()).unwrap();
            disk.save(|saved| saved.set_outbox(vec![queued(1, 9)]))
                .unwrap();
        }
        set_version(dir.path(), 4);
        match Disk::open(dir.path()) {
            Err(NodeError::Disk(message)) => assert!(
                message.ends_with("schema version 4 is newer than this chibi's 3; upgrade chibi"),
                "{message}"
            ),
            Err(err) => panic!("{err}"),
            Ok(_) => panic!("opened a version 4 database"),
        }
        let saved = read_state(dir.path()).unwrap().unwrap();
        assert_eq!(saved.outbox(), &[queued(1, 9)]);

        set_version(dir.path(), 100);
        match read_state(dir.path()) {
            Err(NodeError::Disk(message)) => assert!(
                message.ends_with("schema version 100 is newer than this chibi's 3; upgrade chibi"),
                "{message}"
            ),
            Err(err) => panic!("{err}"),
            Ok(_) => panic!("read a version 100 database"),
        }
    }

    #[cfg(unix)]
    fn mode(path: &Path) -> u32 {
        use std::os::unix::fs::PermissionsExt as _;
        fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    #[cfg(unix)]
    fn set_mode(path: &Path, mode: u32) {
        use std::os::unix::fs::PermissionsExt as _;
        fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn a_new_database_is_owner_only() {
        let dir = tempfile::tempdir().unwrap();
        drop(Disk::open(dir.path()).unwrap());
        assert_eq!(mode(&dir.path().join(DB_FILE)), 0o600);
    }

    #[cfg(unix)]
    #[test]
    fn the_upgrade_to_version_3_makes_the_database_owner_only_once() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(DB_FILE);
        drop(Disk::open(dir.path()).unwrap());
        downgrade(dir.path(), 2);
        set_mode(&path, 0o644);
        drop(Disk::open(dir.path()).unwrap());
        assert_eq!(mode(&path), 0o600);

        set_mode(&path, 0o640);
        drop(Disk::open(dir.path()).unwrap());
        assert_eq!(mode(&path), 0o640);
        read_state(dir.path()).unwrap();
        assert_eq!(mode(&path), 0o640);
    }

    #[test]
    fn reset_keeps_the_account_history() {
        let dir = tempfile::tempdir().unwrap();
        {
            let disk = Disk::open(dir.path()).unwrap();
            let db = disk.db.lock();
            db.conn
                .execute_batch(
                    "INSERT INTO history_coverage (account, capability, first, last)
                         VALUES (zeroblob(20), 1, 5, 9);
                     INSERT INTO history_tx (number, tx_index, hash, sender, raw, success,
                         cumulative_gas, gas_used, first_log_index, log_count)
                         VALUES (5, 0, zeroblob(32), zeroblob(20), NULL, 1, 21000, 21000, 0, 0);",
                )
                .unwrap();
        }
        Lock::take(dir.path()).unwrap().reset().unwrap();
        let conn = Connection::open(dir.path().join(DB_FILE)).unwrap();
        let count = |table: &str| -> i64 {
            conn.query_row(&format!("SELECT count(*) FROM {table}"), [], |row| {
                row.get(0)
            })
            .unwrap()
        };
        assert_eq!((count("history_coverage"), count("history_tx")), (1, 1));
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

    mod history {
        use std::collections::BTreeMap;
        use std::num::NonZeroUsize;

        use alloy_primitives::{Log, TxHash};
        use chibi_core::history::{
            At, BlockFacts, BlockRow, Capability, LogRow, OpenState, Page, Ranges, ScannedBlock,
            SlotChange, Touch, TxRow, WithdrawalRow,
        };

        use super::*;

        fn alice() -> Address {
            Address::repeat_byte(0xa1)
        }

        fn bob() -> Address {
            Address::repeat_byte(0xb0)
        }

        fn token() -> Address {
            Address::repeat_byte(0x70)
        }

        /// The hash of block `number` on fork `fork`: the fork in the first byte, the number in
        /// the last eight.
        fn hash(number: u64, fork: u8) -> B256 {
            let mut bytes = [0; 32];
            bytes[0] = fork;
            bytes[24..].copy_from_slice(&number.to_be_bytes());
            B256::from(bytes)
        }

        fn at(number: u64, fork: u8) -> BlockNumHash {
            BlockNumHash::new(number, hash(number, fork))
        }

        fn tx_hash(block: B256) -> TxHash {
            let mut tx = block;
            tx.0[1] = 0xee;
            tx
        }

        /// One of each fact for alice in a one-transaction block.
        fn facts(number: u64, hash: B256) -> BlockFacts {
            BlockFacts {
                block: BlockRow {
                    number,
                    hash,
                    timestamp: 1_700_000_000 + number,
                    beneficiary: Address::repeat_byte(0xfe),
                    base_fee: Some(7),
                    excess_blob_gas: None,
                    tx_count: 1,
                },
                txs: vec![TxRow {
                    index: 0,
                    hash: tx_hash(hash),
                    sender: alice(),
                    raw: Some(Bytes::from_static(&[2, 1])),
                    success: true,
                    cumulative_gas: 21_000,
                    gas_used: 21_000,
                    first_log_index: 0,
                    log_count: 1,
                }],
                logs: vec![LogRow {
                    index: 0,
                    log: Log::new_unchecked(
                        token(),
                        vec![B256::repeat_byte(0xdd), alice().into_word()],
                        Bytes::from_static(&[1]),
                    ),
                }],
                touches: vec![Touch {
                    account: alice(),
                    at: At::Tx(0),
                    balance: Some(U256::from(number)),
                    nonce: Some(number),
                    code: None,
                }],
                slots: vec![SlotChange {
                    account: alice(),
                    at: At::Tx(0),
                    contract: token(),
                    slot: U256::from(3),
                    value: U256::from(number),
                }],
                withdrawals: vec![WithdrawalRow {
                    index: number,
                    validator: 9,
                    address: alice(),
                    amount_gwei: 1,
                }],
            }
        }

        /// `len` blocks on fork `fork` after `parent`, scanned forward for alice, each with
        /// one of every fact.
        fn chain(parent: BlockNumHash, fork: u8, len: u64) -> Vec<ScannedBlock> {
            let mut parent = parent;
            (0..len)
                .map(|_| {
                    let number = parent.number + 1;
                    let hash = hash(number, fork);
                    let block = ScannedBlock {
                        number,
                        hash,
                        parent_hash: parent.hash,
                        accounts: vec![alice()],
                        capabilities: vec![Capability::Bal, Capability::Logs],
                        facts: Some(facts(number, hash)),
                    };
                    parent = BlockNumHash::new(number, hash);
                    block
                })
                .collect()
        }

        /// Block `number` on fork `fork` scanned backward for alice's logs, with every fact.
        fn backward(number: u64, fork: u8) -> ScannedBlock {
            ScannedBlock {
                number,
                hash: hash(number, fork),
                parent_hash: hash(number - 1, fork),
                accounts: vec![alice()],
                capabilities: vec![Capability::Logs],
                facts: Some(facts(number, hash(number, fork))),
            }
        }

        fn counts(disk: &Disk) -> Vec<(&'static str, u64)> {
            let db = disk.db.lock();
            HISTORY_TABLES
                .iter()
                .map(|&table| {
                    let count = db
                        .conn
                        .query_row(&format!("SELECT count(*) FROM {table}"), [], |row| {
                            row.get(0)
                        })
                        .unwrap();
                    (table, count)
                })
                .collect()
        }

        fn numbers(disk: &Disk, table: &str) -> Vec<u64> {
            let db = disk.db.lock();
            let mut select = db
                .conn
                .prepare(&format!(
                    "SELECT DISTINCT number FROM {table} ORDER BY number"
                ))
                .unwrap();
            select
                .query_map([], |row| row.get(0))
                .unwrap()
                .collect::<rusqlite::Result<_>>()
                .unwrap()
        }

        fn coverage(disk: &Disk, account: Address, capability: Capability) -> Vec<(u64, u64)> {
            let db = disk.db.lock();
            let mut select = db
                .conn
                .prepare(
                    "SELECT first, last FROM history_coverage
                     WHERE account = ?1 AND capability = ?2 ORDER BY first",
                )
                .unwrap();
            select
                .query_map(params![account.as_slice(), capability.code()], |row| {
                    Ok((row.get(0)?, row.get(1)?))
                })
                .unwrap()
                .collect::<rusqlite::Result<_>>()
                .unwrap()
        }

        fn settled_mark(disk: &Disk) -> Option<BlockNumHash> {
            settled(&disk.db.lock().conn).unwrap()
        }

        fn tip(disk: &Disk) -> Option<BlockNumHash> {
            history_tip(&disk.db.lock().conn).unwrap()
        }

        fn fresh() -> (tempfile::TempDir, Disk) {
            let dir = tempfile::tempdir().unwrap();
            let disk = Disk::open(dir.path()).unwrap();
            (dir, disk)
        }

        #[test]
        fn recording_a_step_twice_leaves_every_table_unchanged() {
            let (_dir, disk) = fresh();
            let step = chain(at(9, 1), 1, 3);
            disk.record_head(at(9, 1), &step).unwrap();
            let once = counts(&disk);
            assert_eq!(
                once,
                [
                    ("history_backfill", 0),
                    ("history_block", 3),
                    ("history_coverage", 2),
                    ("history_log", 3),
                    ("history_open", 0),
                    ("history_open_slot", 0),
                    ("history_settled", 0),
                    ("history_slot", 3),
                    ("history_touch", 3),
                    ("history_tx", 3),
                    ("history_unsettled", 3),
                    ("history_withdrawal", 3),
                ]
            );
            disk.record_head(at(9, 1), &step).unwrap();
            assert_eq!(counts(&disk), once);
            assert_eq!(tip(&disk), Some(at(12, 1)));
        }

        #[test]
        fn a_step_that_replays_part_of_the_tip_writes_only_the_new_blocks() {
            let (_dir, disk) = fresh();
            disk.record_head(at(9, 1), &chain(at(9, 1), 1, 3)).unwrap();
            disk.record_head(at(10, 1), &chain(at(10, 1), 1, 4))
                .unwrap();
            assert_eq!(numbers(&disk, "history_unsettled"), [10, 11, 12, 13, 14]);
            assert_eq!(numbers(&disk, "history_tx"), [10, 11, 12, 13, 14]);
            assert_eq!(coverage(&disk, alice(), Capability::Bal), [(10, 14)]);
        }

        #[test]
        fn a_block_whose_parent_is_the_tip_height_with_another_hash_is_refused() {
            let (_dir, disk) = fresh();
            disk.record_head(at(9, 1), &chain(at(9, 1), 1, 3)).unwrap();
            let before = counts(&disk);
            assert!(matches!(
                disk.record_head(at(12, 2), &chain(at(12, 2), 2, 1)),
                Err(NodeError::HistoryReconcile { above: 11 })
            ));
            assert_eq!(counts(&disk), before, "nothing is written");
            assert!(
                matches!(
                    disk.record_head(at(10, 1), &chain(at(10, 1), 2, 1)),
                    Err(NodeError::HistoryReconcile { above: 10 })
                ),
                "a parent below the tip needs a reconcile too"
            );
            assert_eq!(counts(&disk), before);
            assert_eq!(tip(&disk), Some(at(12, 1)));
        }

        #[test]
        fn blocks_that_do_not_chain_are_refused() {
            let (_dir, disk) = fresh();
            let mut step = chain(at(9, 1), 1, 2);
            step[1].parent_hash = hash(10, 2);
            assert!(disk.record_head(at(9, 1), &step).is_err());
            assert!(disk.record_head(at(8, 1), &chain(at(9, 1), 1, 2)).is_err());
            assert_eq!(tip(&disk), None);
        }

        #[test]
        fn a_step_whose_parent_is_above_the_tip_is_accepted_and_leaves_a_coverage_gap() {
            let (_dir, disk) = fresh();
            disk.record_head(at(9, 1), &chain(at(9, 1), 1, 2)).unwrap();
            disk.record_head(at(20, 1), &chain(at(20, 1), 1, 2))
                .unwrap();
            assert_eq!(tip(&disk), Some(at(22, 1)));
            assert_eq!(
                coverage(&disk, alice(), Capability::Bal),
                [(10, 11), (21, 22)]
            );
            assert_eq!(
                coverage(&disk, alice(), Capability::Logs),
                [(10, 11), (21, 22)]
            );
        }

        #[test]
        fn reconcile_keeps_everything_when_the_ancestry_matches_the_tip() {
            let (_dir, disk) = fresh();
            disk.record_head(at(9, 1), &chain(at(9, 1), 1, 3)).unwrap();
            let before = counts(&disk);
            let ancestry = BTreeMap::from([(12, hash(12, 1)), (11, hash(11, 1))]);
            assert_eq!(disk.reconcile_history(&ancestry).unwrap(), Some(at(12, 1)));
            assert_eq!(counts(&disk), before);
        }

        #[test]
        fn reconcile_retracts_to_the_fork_when_the_ancestry_matches_lower() {
            let (_dir, disk) = fresh();
            disk.record_head(at(9, 1), &chain(at(9, 1), 1, 5)).unwrap();
            let ancestry = BTreeMap::from([
                (15, hash(15, 2)),
                (13, hash(13, 2)),
                (12, hash(12, 1)),
                (11, hash(11, 1)),
            ]);
            assert_eq!(disk.reconcile_history(&ancestry).unwrap(), Some(at(12, 1)));
            for table in [
                "history_unsettled",
                "history_block",
                "history_tx",
                "history_log",
                "history_touch",
                "history_slot",
                "history_withdrawal",
            ] {
                assert_eq!(numbers(&disk, table), [10, 11, 12], "{table}");
            }
            assert_eq!(coverage(&disk, alice(), Capability::Bal), [(10, 12)]);
            assert_eq!(coverage(&disk, alice(), Capability::Logs), [(10, 12)]);
            disk.record_head(at(12, 1), &chain(at(12, 1), 2, 2))
                .unwrap();
            assert_eq!(tip(&disk), Some(at(14, 2)));
        }

        #[test]
        fn reconcile_retracts_to_settled_when_nothing_matches_and_never_below() {
            let (_dir, disk) = fresh();
            disk.record_head(at(9, 1), &chain(at(9, 1), 1, 5)).unwrap();
            disk.settle_history(at(11, 1)).unwrap();
            disk.augment_history(&[backward(5, 1)]).unwrap();
            let ancestry = BTreeMap::from([(13, hash(13, 2)), (12, hash(12, 2))]);
            assert_eq!(disk.reconcile_history(&ancestry).unwrap(), Some(at(11, 1)));
            assert_eq!(numbers(&disk, "history_unsettled"), Vec::<u64>::new());
            assert_eq!(numbers(&disk, "history_block"), [5, 10, 11]);
            assert_eq!(numbers(&disk, "history_touch"), [5, 10, 11]);
            assert_eq!(coverage(&disk, alice(), Capability::Bal), [(10, 11)]);
            assert_eq!(
                coverage(&disk, alice(), Capability::Logs),
                [(5, 5), (10, 11)]
            );
            assert_eq!(settled_mark(&disk), Some(at(11, 1)));
            assert_eq!(
                disk.reconcile_history(&BTreeMap::new()).unwrap(),
                Some(at(11, 1))
            );
            assert_eq!(numbers(&disk, "history_block"), [5, 10, 11]);
        }

        #[test]
        fn reconcile_with_nothing_settled_and_nothing_matching_retracts_everything() {
            let (_dir, disk) = fresh();
            disk.record_head(at(9, 1), &chain(at(9, 1), 1, 3)).unwrap();
            let ancestry = BTreeMap::from([(12, hash(12, 2))]);
            assert_eq!(disk.reconcile_history(&ancestry).unwrap(), None);
            assert!(counts(&disk).iter().all(|&(_, count)| count == 0));
        }

        #[test]
        fn settling_at_a_finalized_block_drops_unsettled_rows_at_and_below_it() {
            let (_dir, disk) = fresh();
            disk.record_head(at(9, 1), &chain(at(9, 1), 1, 5)).unwrap();
            disk.settle_history(at(12, 1)).unwrap();
            assert_eq!(settled_mark(&disk), Some(at(12, 1)));
            assert_eq!(numbers(&disk, "history_unsettled"), [13, 14]);
            assert_eq!(numbers(&disk, "history_block"), [10, 11, 12, 13, 14]);
            assert_eq!(tip(&disk), Some(at(14, 1)));

            disk.settle_history(at(13, 2)).unwrap();
            disk.settle_history(at(11, 1)).unwrap();
            assert_eq!(
                settled_mark(&disk),
                Some(at(12, 1)),
                "a hash that disagrees or a lower block moves nothing"
            );
            assert_eq!(numbers(&disk, "history_unsettled"), [13, 14]);
        }

        #[test]
        fn with_the_tip_below_finalized_settling_settles_at_the_tip() {
            let (_dir, disk) = fresh();
            disk.settle_history(at(20, 1)).unwrap();
            assert_eq!(settled_mark(&disk), None, "no history, nothing to settle");
            disk.record_head(at(9, 1), &chain(at(9, 1), 1, 3)).unwrap();
            disk.settle_history(at(20, 7)).unwrap();
            assert_eq!(settled_mark(&disk), Some(at(12, 1)));
            assert_eq!(numbers(&disk, "history_unsettled"), Vec::<u64>::new());
            assert_eq!(tip(&disk), Some(at(12, 1)));
            disk.record_head(at(12, 1), &chain(at(12, 1), 1, 1))
                .unwrap();
            assert_eq!(tip(&disk), Some(at(13, 1)));
        }

        #[test]
        fn the_lowest_unsettled_block_rises_as_history_settles() {
            let (_dir, disk) = fresh();
            assert_eq!(disk.lowest_unsettled().unwrap(), None);
            disk.record_head(at(9, 1), &chain(at(9, 1), 1, 3)).unwrap();
            assert_eq!(disk.lowest_unsettled().unwrap(), Some(10));
            disk.settle_history(at(11, 1)).unwrap();
            assert_eq!(disk.lowest_unsettled().unwrap(), Some(12));
            disk.settle_history(at(12, 1)).unwrap();
            assert_eq!(disk.lowest_unsettled().unwrap(), None);
        }

        #[test]
        fn augmenting_below_settled_adds_facts_and_unions_coverage_once() {
            let (_dir, disk) = fresh();
            disk.record_head(at(9, 1), &chain(at(9, 1), 1, 3)).unwrap();
            disk.settle_history(at(12, 1)).unwrap();
            disk.augment_history(&[backward(5, 1), backward(6, 1)])
                .unwrap();
            assert_eq!(
                coverage(&disk, alice(), Capability::Logs),
                [(5, 6), (10, 12)]
            );
            disk.augment_history(&[backward(6, 1), backward(7, 1)])
                .unwrap();
            disk.augment_history(&[backward(10, 1)]).unwrap();
            assert_eq!(
                coverage(&disk, alice(), Capability::Logs),
                [(5, 7), (10, 12)]
            );
            assert_eq!(coverage(&disk, alice(), Capability::Bal), [(10, 12)]);
            assert_eq!(numbers(&disk, "history_block"), [5, 6, 7, 10, 11, 12]);
            assert_eq!(
                counts(&disk),
                [
                    ("history_backfill", 0),
                    ("history_block", 6),
                    ("history_coverage", 3),
                    ("history_log", 6),
                    ("history_open", 0),
                    ("history_open_slot", 0),
                    ("history_settled", 1),
                    ("history_slot", 6),
                    ("history_touch", 6),
                    ("history_tx", 6),
                    ("history_unsettled", 0),
                    ("history_withdrawal", 6),
                ]
            );
        }

        #[test]
        fn augmenting_a_block_whose_hash_conflicts_is_refused() {
            let (_dir, disk) = fresh();
            let mut step = chain(at(9, 1), 1, 3);
            step[2].facts = None;
            disk.record_head(at(9, 1), &step).unwrap();
            disk.settle_history(at(12, 1)).unwrap();
            let before = counts(&disk);
            assert!(
                disk.augment_history(&[backward(11, 2)]).is_err(),
                "a stored block row has another hash"
            );
            assert!(
                disk.augment_history(&[backward(12, 2)]).is_err(),
                "the settled mark has another hash"
            );
            assert!(
                disk.augment_history(&[backward(5, 1), backward(11, 2)])
                    .is_err()
            );
            assert_eq!(counts(&disk), before, "nothing is written");
        }

        #[test]
        fn augmenting_above_settled_or_before_anything_settles_is_refused() {
            let (_dir, disk) = fresh();
            assert!(disk.augment_history(&[backward(5, 1)]).is_err());
            disk.record_head(at(9, 1), &chain(at(9, 1), 1, 3)).unwrap();
            assert!(disk.augment_history(&[backward(5, 1)]).is_err());
            disk.settle_history(at(11, 1)).unwrap();
            assert!(disk.augment_history(&[backward(12, 1)]).is_err());
            assert_eq!(numbers(&disk, "history_block"), [10, 11, 12]);
        }

        #[test]
        fn a_tx_stored_without_its_bytes_gets_them_when_augmented_as_an_accounts_own() {
            let (_dir, disk) = fresh();
            let mut step = chain(at(9, 1), 1, 1);
            step[0].facts.as_mut().unwrap().txs[0].raw = None;
            disk.record_head(at(9, 1), &step).unwrap();
            disk.settle_history(at(10, 1)).unwrap();
            let raw = |disk: &Disk| -> Option<Vec<u8>> {
                disk.db
                    .lock()
                    .conn
                    .query_row("SELECT raw FROM history_tx WHERE number = 10", [], |row| {
                        row.get(0)
                    })
                    .unwrap()
            };
            assert_eq!(raw(&disk), None);

            disk.augment_history(&[backward(10, 1)]).unwrap();
            assert_eq!(raw(&disk), Some(vec![2, 1]));

            let mut other = backward(10, 1);
            other.facts.as_mut().unwrap().txs[0].raw = Some(Bytes::from_static(&[9]));
            disk.augment_history(&[other]).unwrap();
            assert_eq!(raw(&disk), Some(vec![2, 1]), "stored bytes are kept");
            assert_eq!(numbers(&disk, "history_tx"), [10]);
        }

        #[test]
        fn two_accounts_sharing_one_tx_store_one_tx_row_and_a_touch_each() {
            let (_dir, disk) = fresh();
            let block_hash = hash(10, 1);
            let mut facts = facts(10, block_hash);
            facts.block.tx_count = 2;
            facts.touches.push(Touch {
                account: bob(),
                at: At::Tx(0),
                balance: None,
                nonce: None,
                code: None,
            });
            facts.touches.push(Touch {
                account: bob(),
                at: At::Post,
                balance: Some(U256::from(5)),
                nonce: None,
                code: None,
            });
            let block = ScannedBlock {
                number: 10,
                hash: block_hash,
                parent_hash: hash(9, 1),
                accounts: vec![alice(), bob()],
                capabilities: vec![Capability::Bal, Capability::Logs],
                facts: Some(facts),
            };
            disk.record_head(at(9, 1), &[block]).unwrap();
            assert_eq!(numbers(&disk, "history_tx"), [10]);
            let db = disk.db.lock();
            let touches: Vec<(Vec<u8>, u64, u64)> = db
                .conn
                .prepare(
                    "SELECT account, number, access_index FROM history_touch
                     ORDER BY account, access_index",
                )
                .unwrap()
                .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
                .unwrap()
                .collect::<rusqlite::Result<_>>()
                .unwrap();
            drop(db);
            assert_eq!(
                touches,
                [
                    (alice().to_vec(), 10, 1),
                    (bob().to_vec(), 10, 1),
                    (bob().to_vec(), 10, 3),
                ]
            );
            assert_eq!(coverage(&disk, alice(), Capability::Bal), [(10, 10)]);
            assert_eq!(coverage(&disk, bob(), Capability::Logs), [(10, 10)]);
        }

        #[test]
        fn opening_an_account_twice_stores_its_state_once() {
            let (_dir, disk) = fresh();
            let open = OpenState {
                account: alice(),
                number: 9,
                hash: hash(9, 1),
                balance: U256::from(1_000),
                nonce: 4,
                slots: vec![
                    (token(), U256::from(3), U256::from(50)),
                    (token(), U256::from(4), U256::from(60)),
                ],
            };
            disk.open_history(&open).unwrap();
            disk.open_history(&open).unwrap();
            let counts = counts(&disk);
            assert_eq!(counts[4], ("history_open", 1));
            assert_eq!(counts[5], ("history_open_slot", 2));
        }

        fn page(history: &History, account: Address, before: Option<u64>, rows: usize) -> Page {
            history
                .page(account, before, NonZeroUsize::new(rows).unwrap())
                .unwrap()
        }

        fn block_numbers(page: &Page) -> Vec<u64> {
            page.blocks.iter().map(|facts| facts.block.number).collect()
        }

        fn reader(dir: &tempfile::TempDir) -> History {
            History::open(dir.path()).unwrap().unwrap()
        }

        #[test]
        fn the_reader_opens_nothing_before_version_3_and_reads_beside_a_writer() {
            let dir = tempfile::tempdir().unwrap();
            assert!(History::open(dir.path()).unwrap().is_none(), "no database");
            {
                let disk = Disk::open(dir.path()).unwrap();
                disk.record_head(at(9, 1), &chain(at(9, 1), 1, 2)).unwrap();
                let history = reader(&dir);
                assert_eq!(block_numbers(&page(&history, alice(), None, 5)), [11, 10]);
                disk.record_head(at(11, 1), &chain(at(11, 1), 1, 1))
                    .unwrap();
                assert_eq!(
                    block_numbers(&page(&history, alice(), None, 5)),
                    [12, 11, 10]
                );
            }
            set_version(dir.path(), 100);
            assert!(History::open(dir.path()).is_err(), "a breaking version");
            downgrade(dir.path(), 2);
            assert!(History::open(dir.path()).unwrap().is_none(), "version 2");
        }

        #[test]
        fn a_page_holds_whole_blocks_newest_first_and_next_pages_to_the_end() {
            let (dir, disk) = fresh();
            let mut step = chain(at(9, 1), 1, 5);
            step[3].facts.as_mut().unwrap().touches.push(Touch {
                account: alice(),
                at: At::Post,
                balance: Some(U256::from(99)),
                nonce: None,
                code: None,
            });
            disk.record_head(at(9, 1), &step).unwrap();
            let history = reader(&dir);

            let first = page(&history, alice(), None, 2);
            assert_eq!(first.account, alice());
            assert_eq!(block_numbers(&first), [14, 13]);
            assert_eq!(first.blocks[1].touches.len(), 2, "block 13 is never split");
            assert_eq!(first.next, Some(13));
            let second = page(&history, alice(), first.next, 2);
            assert_eq!(block_numbers(&second), [12, 11]);
            assert_eq!(second.next, Some(11));
            let last = page(&history, alice(), second.next, 2);
            assert_eq!(last.blocks, [facts(10, hash(10, 1))]);
            assert_eq!(last.next, None);
            assert_eq!(page(&history, bob(), None, 2).blocks, []);
        }

        #[test]
        fn two_accounts_sharing_a_tx_each_see_it_and_only_their_own_other_txs() {
            let (dir, disk) = fresh();
            let block_hash = hash(10, 1);
            let mut facts = facts(10, block_hash);
            facts.block.tx_count = 2;
            facts.txs[0].log_count = 2;
            let bob_log = |index: u64| LogRow {
                index,
                log: Log::new_unchecked(
                    token(),
                    vec![B256::repeat_byte(0xdd), bob().into_word()],
                    Bytes::from_static(&[2]),
                ),
            };
            facts.logs.push(bob_log(1));
            facts.logs.push(bob_log(2));
            facts.txs.push(TxRow {
                index: 1,
                hash: B256::repeat_byte(0x11),
                sender: token(),
                raw: None,
                success: true,
                cumulative_gas: 70_000,
                gas_used: 49_000,
                first_log_index: 2,
                log_count: 1,
            });
            for index in [0, 1] {
                facts.touches.push(Touch {
                    account: bob(),
                    at: At::Tx(index),
                    balance: None,
                    nonce: None,
                    code: None,
                });
            }
            let block = ScannedBlock {
                number: 10,
                hash: block_hash,
                parent_hash: hash(9, 1),
                accounts: vec![alice(), bob()],
                capabilities: vec![Capability::Bal, Capability::Logs],
                facts: Some(facts.clone()),
            };
            disk.record_head(at(9, 1), &[block]).unwrap();
            let history = reader(&dir);

            let alices = page(&history, alice(), None, 1);
            let [alices] = &alices.blocks[..] else {
                panic!("{alices:?}")
            };
            assert_eq!(alices.block, facts.block);
            assert_eq!(alices.txs, facts.txs[..1]);
            assert_eq!(
                alices.logs,
                facts.logs[..2],
                "bob's log under the shared tx"
            );
            assert_eq!(alices.touches, facts.touches[..1]);
            assert_eq!(alices.slots, facts.slots);
            assert_eq!(alices.withdrawals, facts.withdrawals);

            let bobs = page(&history, bob(), None, 1);
            let [bobs] = &bobs.blocks[..] else {
                panic!("{bobs:?}")
            };
            assert_eq!(bobs.txs, facts.txs);
            assert_eq!(bobs.logs, facts.logs);
            assert_eq!(bobs.touches, facts.touches[1..]);
            assert_eq!(bobs.slots, []);
            assert_eq!(bobs.withdrawals, []);
        }

        #[test]
        fn the_opening_balance_is_the_newest_touch_balance_below_else_the_open_state() {
            let (dir, disk) = fresh();
            disk.open_history(&OpenState {
                account: alice(),
                number: 9,
                hash: hash(9, 1),
                balance: U256::from(1_000),
                nonce: 4,
                slots: vec![],
            })
            .unwrap();
            let mut step = chain(at(9, 1), 1, 3);
            step[1].facts.as_mut().unwrap().touches[0].balance = None;
            disk.record_head(at(9, 1), &step).unwrap();
            let history = reader(&dir);

            let newest = page(&history, alice(), None, 1);
            assert_eq!(block_numbers(&newest), [12]);
            assert_eq!(newest.opening_balance, Some((10, U256::from(10))));
            let middle = page(&history, alice(), newest.next, 1);
            assert_eq!(middle.opening_balance, Some((10, U256::from(10))));
            let oldest = page(&history, alice(), middle.next, 1);
            assert_eq!(block_numbers(&oldest), [10]);
            assert_eq!(oldest.opening_balance, Some((9, U256::from(1_000))));
        }

        #[test]
        fn a_page_carries_its_bal_coverage_and_the_open_states_inside_it() {
            let (dir, disk) = fresh();
            disk.record_head(at(9, 1), &chain(at(9, 1), 1, 3)).unwrap();
            disk.record_head(at(20, 1), &chain(at(20, 1), 1, 2))
                .unwrap();
            disk.open_history(&OpenState {
                account: alice(),
                number: 20,
                hash: hash(20, 1),
                balance: U256::from(2_000),
                nonce: 20,
                slots: vec![],
            })
            .unwrap();
            let history = reader(&dir);

            let newest = page(&history, alice(), None, 3);
            assert_eq!(block_numbers(&newest), [22, 21, 12]);
            assert_eq!(newest.opening_balance, Some((11, U256::from(11))));
            assert_eq!(newest.bal, Ranges::from_iter([(10, 12), (21, 22)]));
            assert_eq!(newest.opens, [(20, U256::from(2_000))]);
            let older = page(&history, alice(), newest.next, 1);
            assert_eq!(block_numbers(&older), [11]);
            assert_eq!(older.bal, Ranges::from_iter([(10, 12)]));
            assert_eq!(older.opens, []);
        }

        #[test]
        fn a_view_reads_coverage_and_activities_from_one_snapshot() {
            let (dir, disk) = fresh();
            disk.record_head(at(9, 1), &chain(at(9, 1), 1, 2)).unwrap();
            let history = reader(&dir);
            disk.db
                .lock()
                .conn
                .busy_timeout(Duration::from_millis(10))
                .unwrap();

            let view = history
                .view_between(alice(), 1, || {
                    // Refused while the view's snapshot is open; accepted between two reads.
                    let _ = disk.record_head(at(11, 1), &chain(at(11, 1), 1, 1));
                })
                .unwrap();

            assert_eq!(view.coverage.ranges[0].0, Capability::Bal);
            assert_eq!(view.coverage.ranges[0].1.last(), Some(11));
            let newest: Vec<_> = view.activities.iter().map(|found| found.number).collect();
            assert_eq!(newest, [11]);
        }

        #[test]
        fn rows_that_do_not_parse_are_skipped() {
            let (dir, disk) = fresh();
            disk.record_head(at(9, 1), &chain(at(9, 1), 1, 1)).unwrap();
            disk.db
                .lock()
                .conn
                .execute(
                    "INSERT INTO history_touch (account, number, access_index) VALUES (?1, 10, 9)",
                    [alice().as_slice()],
                )
                .unwrap();
            let history = reader(&dir);
            assert_eq!(
                page(&history, alice(), None, 1).blocks,
                [facts(10, hash(10, 1))]
            );
        }

        #[test]
        fn coverage_lists_ranges_the_settled_mark_opens_txs_at_touches_and_nonces_in_order() {
            let (dir, disk) = fresh();
            let mut step = chain(at(9, 1), 1, 3);
            let facts = step[1].facts.as_mut().unwrap();
            facts.block.tx_count = 2;
            facts.txs.push(TxRow {
                index: 1,
                hash: B256::repeat_byte(0x22),
                sender: bob(),
                raw: Some(Bytes::from_static(&[2, 2])),
                success: true,
                cumulative_gas: 42_000,
                gas_used: 21_000,
                first_log_index: 1,
                log_count: 0,
            });
            facts.touches.push(Touch {
                account: bob(),
                at: At::Tx(1),
                balance: None,
                nonce: Some(1),
                code: None,
            });
            step[1].accounts.push(bob());
            let relayed = step[2].facts.as_mut().unwrap();
            relayed.block.tx_count = 2;
            relayed.txs.push(TxRow {
                index: 1,
                hash: B256::repeat_byte(0x33),
                sender: bob(),
                raw: Some(Bytes::from_static(&[4, 1])),
                success: true,
                cumulative_gas: 42_000,
                gas_used: 21_000,
                first_log_index: 1,
                log_count: 0,
            });
            relayed.touches.push(Touch {
                account: alice(),
                at: At::Tx(1),
                balance: None,
                nonce: None,
                code: None,
            });
            disk.record_head(at(9, 1), &step).unwrap();
            disk.settle_history(at(11, 1)).unwrap();
            disk.augment_history(&[backward(5, 1)]).unwrap();
            disk.record_head(at(20, 1), &chain(at(20, 1), 1, 2))
                .unwrap();
            let open = |number: u64, slots| OpenState {
                account: alice(),
                number,
                hash: hash(number, 1),
                balance: U256::from(number * 100),
                nonce: number,
                slots,
            };
            let opens = [
                open(9, vec![(token(), U256::from(3), U256::from(50))]),
                open(20, vec![]),
            ];
            for state in opens.iter().rev() {
                disk.open_history(state).unwrap();
            }
            disk.open_history(&OpenState {
                account: bob(),
                ..open(9, vec![])
            })
            .unwrap();
            disk.db
                .lock()
                .conn
                .execute(
                    "INSERT INTO history_coverage (account, capability, first, last)
                     VALUES (?1, 7, 1, 2)",
                    [alice().as_slice()],
                )
                .unwrap();

            let coverage = reader(&dir).coverage(alice()).unwrap();
            assert_eq!(coverage.account, alice());
            assert_eq!(
                coverage.ranges,
                [
                    (Capability::Bal, Ranges::from_iter([(10, 12), (21, 22)])),
                    (
                        Capability::Logs,
                        Ranges::from_iter([(5, 5), (10, 12), (21, 22)])
                    ),
                ]
            );
            assert_eq!(coverage.settled, Some(11));
            assert_eq!(coverage.opens, opens);
            let txs: Vec<_> = coverage
                .txs
                .iter()
                .map(|(number, tx)| (*number, tx.index, tx.sender, tx.raw.clone()))
                .collect();
            let own = |number| (number, 0, alice(), Some(Bytes::from_static(&[2, 1])));
            let relayed = (12, 1, bob(), Some(Bytes::from_static(&[4, 1])));
            assert_eq!(
                txs,
                [own(5), own(10), own(11), own(12), relayed, own(21), own(22)]
            );
            let nonces: Vec<_> = coverage
                .nonces
                .iter()
                .map(|change| (change.number, change.index, change.nonce))
                .collect();
            assert_eq!(
                nonces,
                [
                    (5, 1, 5),
                    (10, 1, 10),
                    (11, 1, 11),
                    (12, 1, 12),
                    (21, 1, 21),
                    (22, 1, 22)
                ]
            );
        }
    }
}
