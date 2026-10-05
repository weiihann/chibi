//! The per-network data directory: its lock, the devp2p identity, and the state file.
//!
//! The state file holds tracked keys, a display snapshot, the outbox of pending raw
//! transactions, and snap/2 peers worth dialing first. It is replaced atomically (temp file,
//! fsync, rename). Its values are display-only: nothing read from it enters a `Store`.

use std::fs::{self, File, OpenOptions, TryLockError};
use std::io::{ErrorKind, Write as _};
use std::path::{Path, PathBuf};

use alloy_eips::BlockNumHash;
use alloy_primitives::{Address, Bytes, U256, hex};
use chibi_core::state::Key;
use parking_lot::Mutex;
use reth_network_peers::NodeRecord;
use serde::{Deserialize, Serialize};
use tracing::warn;

use crate::devp2p::{SecretKey, rng_secret_key};
use crate::node::{NodeError, Snapshot};

const LOCK_FILE: &str = "chibi.lock";
const STATE_FILE: &str = "state.json";
const STATE_TEMP: &str = "state.json.tmp";
const KEY_FILE: &str = "devp2p.key";

/// Most snap/2 peers remembered across restarts.
const REMEMBERED_PEERS: usize = 16;

/// A key as the state file stores it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
enum SavedKey {
    Account { address: Address },
    Slot { address: Address, slot: U256 },
}

impl From<Key> for SavedKey {
    fn from(key: Key) -> Self {
        match key {
            Key::Account(address) => Self::Account { address },
            Key::Slot(address, slot) => Self::Slot { address, slot },
        }
    }
}

impl From<SavedKey> for Key {
    fn from(key: SavedKey) -> Self {
        match key {
            SavedKey::Account { address } => Self::Account(address),
            SavedKey::Slot { address, slot } => Self::Slot(address, slot),
        }
    }
}

/// A submitted transaction not yet located, and the anchor chibi held when it was submitted.
/// Inclusion can only be in a later block, so a search for it need not reach further back.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct OutboxTx {
    /// The EIP-2718 encoding.
    pub(crate) raw: Bytes,
    /// The anchor at submission.
    pub(crate) since: BlockNumHash,
}

/// The state file's contents.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct StateFile {
    tracked: Vec<SavedKey>,
    snapshot: Option<Snapshot>,
    outbox: Vec<OutboxTx>,
    peers: Vec<String>,
}

impl StateFile {
    /// Keys to prove at the first anchor.
    pub(crate) fn tracked(&self) -> impl Iterator<Item = Key> + '_ {
        self.tracked.iter().copied().map(Key::from)
    }

    pub(crate) fn set_tracked(&mut self, keys: impl IntoIterator<Item = Key>) {
        self.tracked = keys.into_iter().map(SavedKey::from).collect();
        self.tracked.sort_unstable_by_key(|key| Key::from(*key));
    }

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

    /// Remembered snap/2 peers. Unparseable records are skipped.
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
    dir: PathBuf,
    _lock: File,
    file: Mutex<StateFile>,
}

impl Disk {
    /// Creates `dir` if needed, takes its exclusive lock, records this process id in the lock
    /// file, and loads the state file.
    pub fn open(dir: &Path) -> Result<Self, NodeError> {
        fs::create_dir_all(dir).map_err(|err| io_error(dir, &err))?;
        let lock_path = dir.join(LOCK_FILE);
        let mut lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&lock_path)
            .map_err(|err| io_error(&lock_path, &err))?;
        match lock.try_lock() {
            Ok(()) => {}
            Err(TryLockError::WouldBlock) => {
                return Err(NodeError::Disk(format!(
                    "{} is in use by another chibi process",
                    dir.display()
                )));
            }
            Err(TryLockError::Error(err)) => return Err(io_error(&lock_path, &err)),
        }
        lock.set_len(0)
            .and_then(|()| write!(lock, "{}", std::process::id()))
            .and_then(|()| lock.sync_all())
            .map_err(|err| io_error(&lock_path, &err))?;
        let file = read_state(dir)?.unwrap_or_default();
        Ok(Self {
            dir: dir.to_path_buf(),
            _lock: lock,
            file: Mutex::new(file),
        })
    }

    /// The process id of the chibi that holds `dir`'s lock, or `None` when no chibi runs there.
    /// Does not take the lock away from anyone.
    pub fn running(dir: &Path) -> Result<Option<u32>, NodeError> {
        let lock_path = dir.join(LOCK_FILE);
        let lock = match File::open(&lock_path) {
            Ok(lock) => lock,
            Err(err) if err.kind() == ErrorKind::NotFound => return Ok(None),
            Err(err) => return Err(io_error(&lock_path, &err)),
        };
        match lock.try_lock_shared() {
            Ok(()) => Ok(None),
            Err(TryLockError::WouldBlock) => {
                let pid =
                    fs::read_to_string(&lock_path).map_err(|err| io_error(&lock_path, &err))?;
                pid.trim().parse().map(Some).map_err(|_| {
                    NodeError::Disk(format!("{} holds no process id", lock_path.display()))
                })
            }
            Err(TryLockError::Error(err)) => Err(io_error(&lock_path, &err)),
        }
    }

    /// The data directory.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Reads the loaded state file.
    pub(crate) fn read<R>(&self, f: impl FnOnce(&StateFile) -> R) -> R {
        f(&self.file.lock())
    }

    /// Edits the state file and writes it out before releasing it, so concurrent writers
    /// cannot reorder their writes. Blocks on fsync; call it off the async workers.
    pub(crate) fn save(&self, edit: impl FnOnce(&mut StateFile)) -> Result<(), NodeError> {
        self.save_then(edit, || ())
    }

    /// Like [`Disk::save`], and runs `written` once the file is durable, still holding the
    /// file, so no other write lands between the two. On failure the loaded state is left as
    /// it was and `written` does not run.
    pub(crate) fn save_then<R>(
        &self,
        edit: impl FnOnce(&mut StateFile),
        written: impl FnOnce() -> R,
    ) -> Result<R, NodeError> {
        let mut file = self.file.lock();
        let mut next = file.clone();
        edit(&mut next);
        let json = serde_json::to_vec_pretty(&next)
            .map_err(|err| NodeError::Disk(format!("encoding the state file: {err}")))?;
        write_atomic(&self.dir, &json)?;
        *file = next;
        Ok(written())
    }

    /// The devp2p secret key, created on first use so the node keeps one identity.
    pub(crate) fn secret_key(&self) -> Result<SecretKey, NodeError> {
        let path = self.dir.join(KEY_FILE);
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

/// Reads the state file in `dir` without taking the lock. `None` if there is none yet.
pub(crate) fn read_state(dir: &Path) -> Result<Option<StateFile>, NodeError> {
    let path = dir.join(STATE_FILE);
    let bytes = match fs::read(&path) {
        Ok(bytes) => bytes,
        Err(err) if err.kind() == ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(io_error(&path, &err)),
    };
    serde_json::from_slice(&bytes)
        .map(Some)
        .map_err(|err| NodeError::Disk(format!("{}: {err}", path.display())))
}

fn write_atomic(dir: &Path, bytes: &[u8]) -> Result<(), NodeError> {
    let temp = dir.join(STATE_TEMP);
    let path = dir.join(STATE_FILE);
    let mut file = File::create(&temp).map_err(|err| io_error(&temp, &err))?;
    file.write_all(bytes)
        .and_then(|()| file.sync_all())
        .map_err(|err| io_error(&temp, &err))?;
    fs::rename(&temp, &path).map_err(|err| io_error(&path, &err))?;
    // The rename is durable only once the directory entry is synced.
    if let Err(err) = File::open(dir).and_then(|dir| dir.sync_all()) {
        warn!(target: "chibi::disk", dir = %dir.display(), %err, "could not sync the data directory");
    }
    Ok(())
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
mod tests {
    use alloy_primitives::{B256, address};

    use super::*;

    #[test]
    fn second_open_of_a_locked_dir_fails_until_the_first_closes() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(Disk::running(dir.path()).unwrap(), None, "no lock file yet");
        let first = Disk::open(dir.path()).unwrap();
        assert!(matches!(Disk::open(dir.path()), Err(NodeError::Disk(_))));
        assert_eq!(
            Disk::running(dir.path()).unwrap(),
            Some(std::process::id()),
            "the holder's pid is readable while it holds the lock"
        );
        drop(first);
        assert_eq!(Disk::running(dir.path()).unwrap(), None);
        assert!(Disk::open(dir.path()).is_ok());
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

    #[test]
    fn saved_state_round_trips_and_leaves_no_temp_file() {
        let dir = tempfile::tempdir().unwrap();
        let alice = address!("0x00000000000000000000000000000000000a11ce");
        let keys = [Key::Slot(alice, U256::from(7)), Key::Account(alice)];
        let outbox = vec![queued(1, 9), queued(2, 11)];
        {
            let disk = Disk::open(dir.path()).unwrap();
            disk.save(|file| {
                file.set_tracked(keys);
                file.set_outbox(outbox.clone());
                file.remember_peers(&[record(1, 30303)]);
            })
            .unwrap();
        }
        assert!(!dir.path().join(STATE_TEMP).exists());
        let disk = Disk::open(dir.path()).unwrap();
        disk.read(|file| {
            assert_eq!(
                file.tracked().collect::<Vec<_>>(),
                vec![Key::Account(alice), Key::Slot(alice, U256::from(7))]
            );
            assert_eq!(file.outbox(), outbox.as_slice());
            assert_eq!(file.peers(), vec![record(1, 30303)]);
        });
    }

    #[test]
    fn remembered_peers_merge_newest_first_dedupe_by_id_and_cap() {
        let mut file = StateFile::default();
        file.remember_peers(&[record(1, 1), record(2, 2)]);
        file.remember_peers(&[]);
        assert_eq!(file.peers(), vec![record(1, 1), record(2, 2)]);
        file.remember_peers(&[record(2, 9)]);
        assert_eq!(file.peers(), vec![record(2, 9), record(1, 1)]);
        let many: Vec<NodeRecord> = (10..40).map(|id| record(id, 1)).collect();
        file.remember_peers(&many);
        assert_eq!(file.peers(), many[..REMEMBERED_PEERS].to_vec());
    }

    #[cfg(unix)]
    #[test]
    fn a_failed_save_leaves_the_loaded_state_and_skips_the_commit() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = tempfile::tempdir().unwrap();
        let disk = Disk::open(dir.path()).unwrap();
        disk.save(|file| file.set_outbox(vec![queued(1, 1)]))
            .unwrap();
        fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o500)).unwrap();
        let mut committed = false;
        let saved = disk.save_then(
            |file| file.set_outbox(vec![queued(2, 1)]),
            || committed = true,
        );
        fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o700)).unwrap();
        assert!(matches!(saved, Err(NodeError::Disk(_))));
        assert!(!committed);
        disk.read(|file| assert_eq!(file.outbox(), &[queued(1, 1)]));
    }
}
