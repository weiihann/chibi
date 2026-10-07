//! The known-key store and the canonical state around it.
//!
//! [`Store`] is partial state at one verified block, its anchor. [`State`] is the value the
//! follower owns: the published store, the working set of keys it keeps, the finalized
//! header, and the transaction book. Both are plain values, and no method performs I/O.

use std::collections::{BTreeSet, HashMap, VecDeque};
use std::sync::Arc;

use alloy_eip7928::AccountChanges;
use alloy_primitives::{Address, B256, BlockNumber, KECCAK256_EMPTY, U256};
use alloy_trie::TrieAccount;
use revm::bytecode::Bytecode;

use crate::tx::TxBook;
use crate::verify::{Fact, Proven, VerifiedBlock, VerifiedHeader};
use crate::working::WorkingSet;

/// Largest head gap closed by applying BALs. Past it, re-proving pins is cheaper, and old
/// roots fall out of what snap peers serve.
pub const MAX_BAL_GAP: u64 = 32;

/// Ancestor headers held besides the anchor, so `BLOCKHASH` reaches its full 256 blocks.
pub const HEADER_WINDOW: usize = 256;

/// Stores kept besides the published one. Wallet block trackers rewrite `latest` to the last
/// number they polled, up to about 20 s old; 8 Sepolia blocks cover 96 s.
pub const RECENT_STATES: usize = 8;

/// An account without its storage root. Storage roots are not held because BALs would leave
/// them stale.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Account {
    /// Transaction count.
    pub nonce: u64,
    /// Balance in wei.
    pub balance: U256,
    /// Code hash, [`KECCAK256_EMPTY`] without code.
    pub code_hash: B256,
}

impl Default for Account {
    fn default() -> Self {
        Self {
            nonce: 0,
            balance: U256::ZERO,
            code_hash: KECCAK256_EMPTY,
        }
    }
}

impl Account {
    /// EIP-161 emptiness: no nonce, no balance, no code.
    pub fn is_empty(&self) -> bool {
        self.nonce == 0 && self.balance.is_zero() && self.code_hash == KECCAK256_EMPTY
    }

    fn apply(&mut self, changes: &AccountChanges) {
        if let Some(balance) = changes.balance_post_state() {
            self.balance = balance;
        }
        if let Some(nonce) = changes.nonce_post_state() {
            self.nonce = nonce;
        }
        if let Some(code_hash) = changes.code_hash_post_state() {
            self.code_hash = code_hash;
        }
    }
}

impl From<&TrieAccount> for Account {
    fn from(account: &TrieAccount) -> Self {
        Self {
            nonce: account.nonce,
            balance: account.balance,
            code_hash: account.code_hash,
        }
    }
}

/// What the store knows about one address at the anchor.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Entry {
    /// Proven absent. Every slot is a known zero.
    Absent,
    /// Proven present. `slots` holds known slots; any other slot is a miss.
    Present {
        /// The account fields.
        account: Account,
        /// Known slot values, zero included.
        slots: HashMap<U256, U256>,
    },
}

/// A state key the store can hold.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Key {
    /// An account.
    Account(Address),
    /// One storage slot of an account.
    Slot(Address, U256),
}

/// A value the store does not hold. Never read as zero.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, thiserror::Error)]
pub enum Miss {
    /// The account is unknown.
    #[error("account {0} is not held")]
    Account(Address),
    /// The account is known present but the slot is not held.
    #[error("slot {1} of {0} is not held")]
    Storage(Address, U256),
    /// Code by hash is not held.
    #[error("code {0} is not held")]
    Code(B256),
    /// The block hash is outside the header window.
    #[error("block hash {0} is not held")]
    BlockHash(BlockNumber),
}

/// Partial state at exactly one verified block, the anchor.
///
/// Every entry holds its value at the anchor; entries carry no block of their own. Values
/// enter only through [`Store::insert`] (a [`Proven`] at the anchor), [`State::advance`] (a BAL
/// of the anchor's child), and [`State::reanchor`].
#[derive(Clone, Debug)]
pub struct Store {
    /// Verified ancestors of the anchor, oldest first, each the parent of the next, the newest
    /// being the anchor's parent. At most [`HEADER_WINDOW`] entries.
    ancestors: VecDeque<VerifiedHeader>,
    anchor: VerifiedHeader,
    accounts: HashMap<Address, Entry>,
    code: HashMap<B256, Bytecode>,
}

impl Store {
    /// An empty store anchored at `anchor`.
    pub fn new(anchor: VerifiedHeader) -> Self {
        Self {
            ancestors: VecDeque::new(),
            anchor,
            accounts: HashMap::new(),
            code: HashMap::new(),
        }
    }

    /// The block every held value is valid at.
    pub const fn anchor(&self) -> &VerifiedHeader {
        &self.anchor
    }

    /// Held headers, oldest first, ending at the anchor.
    pub fn headers(&self) -> impl DoubleEndedIterator<Item = &VerifiedHeader> {
        self.ancestors.iter().chain(std::iter::once(&self.anchor))
    }

    /// A held header by number.
    pub fn header(&self, number: BlockNumber) -> Option<&VerifiedHeader> {
        if number == self.anchor.number() {
            return Some(&self.anchor);
        }
        let oldest = self.ancestors.front()?.number();
        let offset = usize::try_from(number.checked_sub(oldest)?).ok()?;
        self.ancestors.get(offset)
    }

    /// The raw entry for `address`, if held.
    pub fn entry(&self, address: Address) -> Option<&Entry> {
        self.accounts.get(&address)
    }

    /// `Ok(None)` is a proven-absent account.
    pub fn account(&self, address: Address) -> Result<Option<&Account>, Miss> {
        match self.accounts.get(&address) {
            None => Err(Miss::Account(address)),
            Some(Entry::Absent) => Ok(None),
            Some(Entry::Present { account, .. }) => Ok(Some(account)),
        }
    }

    /// A slot of a proven-absent account is a known zero.
    pub fn slot(&self, address: Address, slot: U256) -> Result<U256, Miss> {
        match self.accounts.get(&address) {
            None => Err(Miss::Account(address)),
            Some(Entry::Absent) => Ok(U256::ZERO),
            Some(Entry::Present { slots, .. }) => slots
                .get(&slot)
                .copied()
                .ok_or(Miss::Storage(address, slot)),
        }
    }

    /// Code by hash. Empty code is always known.
    pub fn code(&self, hash: B256) -> Result<Bytecode, Miss> {
        if hash == KECCAK256_EMPTY {
            return Ok(Bytecode::new());
        }
        self.code.get(&hash).cloned().ok_or(Miss::Code(hash))
    }

    /// Hash of a held header by number.
    pub fn block_hash(&self, number: BlockNumber) -> Result<B256, Miss> {
        self.header(number)
            .map(VerifiedHeader::hash)
            .ok_or(Miss::BlockHash(number))
    }

    /// Whether `key` is held.
    pub fn holds(&self, key: Key) -> bool {
        match key {
            Key::Account(address) => self.accounts.contains_key(&address),
            Key::Slot(address, slot) => self.slot(address, slot).is_ok(),
        }
    }

    /// Adds a proven fact. Rejects a fact proven at any block but the anchor.
    pub fn insert(&mut self, proven: &Proven) -> Result<(), StateError> {
        self.check(proven)?;
        self.insert_unchecked(proven);
        Ok(())
    }

    fn check(&self, proven: &Proven) -> Result<(), StateError> {
        let anchor = self.anchor().hash();
        match proven.at() {
            Some(at) if at.hash != anchor => Err(StateError::Stale {
                proven_at: at.hash,
                anchor,
            }),
            Some(_) | None => Ok(()),
        }
    }

    fn insert_unchecked(&mut self, proven: &Proven) {
        match proven.fact() {
            Fact::Account {
                address,
                account: None,
            } => {
                self.accounts.insert(*address, Entry::Absent);
            }
            Fact::Account {
                address,
                account: Some(account),
            } => match self.accounts.get_mut(address) {
                Some(Entry::Present { account: held, .. }) => *held = *account,
                Some(Entry::Absent) | None => {
                    self.accounts.insert(
                        *address,
                        Entry::Present {
                            account: *account,
                            slots: HashMap::new(),
                        },
                    );
                }
            },
            Fact::Slot {
                address,
                account,
                slot,
                value,
            } => match self.accounts.get_mut(address) {
                Some(Entry::Present {
                    account: held,
                    slots,
                }) => {
                    *held = *account;
                    slots.insert(*slot, *value);
                }
                Some(Entry::Absent) | None => {
                    self.accounts.insert(
                        *address,
                        Entry::Present {
                            account: *account,
                            slots: HashMap::from([(*slot, *value)]),
                        },
                    );
                }
            },
            Fact::Code { hash, code } => {
                self.code.insert(*hash, code.clone());
            }
        }
    }

    /// Prepends verified ancestors, ascending, the newest being the parent of the oldest held
    /// header.
    ///
    /// All or nothing: refuses ancestors that do not link or that would not all fit in the
    /// window, so a caller fetching for a [`Miss::BlockHash`] learns it cannot be served.
    pub fn insert_ancestors(&mut self, ancestors: Vec<VerifiedHeader>) -> Result<(), StateError> {
        let oldest = self.ancestors.front().unwrap_or(&self.anchor);
        let linked = ancestors
            .last()
            .is_none_or(|newest| newest.is_parent_of(oldest))
            && ancestors
                .windows(2)
                .all(|pair| pair[0].is_parent_of(&pair[1]));
        if !linked {
            return Err(StateError::Unlinked);
        }
        let room = HEADER_WINDOW - self.ancestors.len();
        if ancestors.len() > room {
            return Err(StateError::WindowFull {
                offered: ancestors.len(),
                room,
            });
        }
        for header in ancestors.into_iter().rev() {
            self.ancestors.push_front(header);
        }
        Ok(())
    }

    /// Drops a held key. Forgetting an account forgets its slots.
    fn forget(&mut self, key: Key) {
        match key {
            Key::Account(address) => {
                self.accounts.remove(&address);
            }
            Key::Slot(address, slot) => {
                if let Some(Entry::Present { slots, .. }) = self.accounts.get_mut(&address) {
                    slots.remove(&slot);
                }
            }
        }
    }

    /// Makes the anchor's child the new anchor, keeping the window bounded.
    fn push_anchor(&mut self, child: VerifiedHeader) {
        let parent = std::mem::replace(&mut self.anchor, child);
        self.ancestors.push_back(parent);
        while self.ancestors.len() > HEADER_WINDOW {
            self.ancestors.pop_front();
        }
    }

    /// BAL semantics after reth's `block_access_list_update`. Only held keys change.
    fn apply_bal(&mut self, bal: &[AccountChanges]) {
        for changes in bal {
            let info = changes.account_info();
            if !info.changes_state_root(changes) {
                continue;
            }
            let Some(entry) = self.accounts.get_mut(&changes.address) else {
                continue;
            };
            let (mut account, mut slots) = match std::mem::replace(entry, Entry::Absent) {
                Entry::Absent => (Account::default(), HashMap::new()),
                Entry::Present { account, slots } => (account, slots),
            };
            account.apply(changes);
            if account.is_empty() {
                continue;
            }
            for (slot, value) in changes.storage_post_states() {
                if let Some(held) = slots.get_mut(&slot) {
                    *held = value;
                }
            }
            *entry = Entry::Present { account, slots };
            if let Some((hash, code)) = info
                .code_hash
                .zip(changes.code_post_state().filter(|code| !code.is_empty()))
            {
                // A BAL matching a verified header commits to this code, and new_raw_checked
                // rejects bytes revm would panic on.
                if let Ok(code) = Bytecode::new_raw_checked(code.clone()) {
                    self.code.insert(hash, code);
                }
            }
        }
    }
}

/// How the store reaches a newer verified head.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Plan {
    /// The target is the anchor or a held ancestor.
    Current,
    /// Fetch and apply this many BALs, ending at the target.
    Extend {
        /// Number of blocks from the anchor's child through the target.
        blocks: u64,
    },
    /// Prove the pins at the target into a fresh store; learned keys are warmed afterwards.
    Reanchor,
}

/// Outcome of folding a request's facts into [`State`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Merge {
    /// The facts for working-set keys were inserted.
    Kept,
    /// The anchor moved while the request ran; only code was kept.
    Dropped,
}

/// The canonical value the follower owns.
///
/// The store holds a key only if the working set does. Working-set keys the store lacks are
/// unheld, and the follower proves them at the anchor.
#[derive(Clone, Debug)]
pub struct State {
    store: Arc<Store>,
    /// Stores published before `store`, newest first, each anchored at a held ancestor of its
    /// anchor. At most [`RECENT_STATES`]. Nothing is folded into them.
    recent: VecDeque<Arc<Store>>,
    working: WorkingSet,
    finalized: Option<VerifiedHeader>,
    txs: TxBook,
}

impl State {
    /// State at `anchor` with nothing held yet: every key of `working` starts unheld.
    pub fn new(anchor: VerifiedHeader, working: WorkingSet) -> Self {
        Self {
            store: Arc::new(Store::new(anchor)),
            recent: VecDeque::new(),
            working,
            finalized: None,
            txs: TxBook::default(),
        }
    }

    /// The published store.
    pub const fn store(&self) -> &Arc<Store> {
        &self.store
    }

    /// Pins the current store.
    pub fn snapshot(&self) -> Arc<Store> {
        Arc::clone(&self.store)
    }

    /// Pins the store anchored at `hash`: the published one or a retained recent one.
    pub fn snapshot_at(&self, hash: B256) -> Option<Arc<Store>> {
        std::iter::once(&self.store)
            .chain(&self.recent)
            .find(|store| store.anchor().hash() == hash)
            .map(Arc::clone)
    }

    /// Keeps `previous`, the store just replaced, and drops retained stores that are no
    /// longer held ancestors of the anchor, then all but the newest [`RECENT_STATES`].
    fn retain(&mut self, previous: Arc<Store>) {
        self.recent.push_front(previous);
        let store = &self.store;
        let anchor = store.anchor().number();
        self.recent.retain(|old| {
            let at = old.anchor();
            at.number() < anchor
                && store
                    .header(at.number())
                    .is_some_and(|held| held.hash() == at.hash())
        });
        self.recent.truncate(RECENT_STATES);
    }

    /// The anchor header.
    pub fn anchor(&self) -> &VerifiedHeader {
        self.store.anchor()
    }

    /// The keys chibi keeps.
    pub const fn working(&self) -> &WorkingSet {
        &self.working
    }

    /// A request read `keys`: records the use. Evicted keys' values leave the store.
    pub fn learn(&mut self, keys: impl IntoIterator<Item = Key>) {
        let evicted = self.working.touch(keys);
        self.forget(evicted);
    }

    /// Replaces the keys pinned by seeding with `keys`.
    pub fn set_seeded(&mut self, keys: BTreeSet<Key>) {
        let evicted = self.working.set_seeded(keys);
        self.forget(evicted);
    }

    /// Working-set keys the store lacks: pins first, then learned keys most recently used
    /// first.
    pub fn unheld(&self) -> impl Iterator<Item = Key> + '_ {
        self.working
            .pins()
            .chain(self.working.learned())
            .filter(|key| !self.store.holds(*key))
    }

    fn forget(&mut self, evicted: Vec<Key>) {
        if evicted.iter().any(|key| self.store.holds(*key)) {
            let store = Arc::make_mut(&mut self.store);
            for key in evicted {
                store.forget(key);
            }
        }
    }

    /// The latest finalized header.
    pub const fn finalized(&self) -> Option<&VerifiedHeader> {
        self.finalized.as_ref()
    }

    /// Records the latest finalized header.
    pub fn set_finalized(&mut self, header: VerifiedHeader) {
        self.finalized = Some(header);
    }

    /// Submitted transactions.
    pub const fn txs(&self) -> &TxBook {
        &self.txs
    }

    /// Submitted transactions, mutably.
    pub const fn txs_mut(&mut self) -> &mut TxBook {
        &mut self.txs
    }

    /// Decides how to reach `target`.
    ///
    /// BAL hashes appear at a fork and stay, so when both the anchor and the target carry one,
    /// every header between them does too; a gap straddling the fork reanchors.
    pub fn plan(&self, target: &VerifiedHeader) -> Plan {
        let anchor = self.store.anchor();
        if self
            .store
            .header(target.number())
            .is_some_and(|held| held.hash() == target.hash())
        {
            return Plan::Current;
        }
        let Some(blocks) = target
            .number()
            .checked_sub(anchor.number())
            .filter(|gap| *gap > 0)
        else {
            return Plan::Reanchor;
        };
        let has_bals = anchor.header().block_access_list_hash.is_some()
            && target.header().block_access_list_hash.is_some();
        let links = blocks > 1 || anchor.is_parent_of(target);
        if blocks <= MAX_BAL_GAP && has_bals && links {
            Plan::Extend { blocks }
        } else {
            Plan::Reanchor
        }
    }

    /// Moves the anchor to its child and applies the child's BAL to held keys. The replaced
    /// store stays readable through [`State::snapshot_at`]. Located transactions whose block
    /// is no longer on the held chain return to pending.
    ///
    /// All or nothing: the parent check runs before any mutation.
    pub fn advance(&mut self, block: VerifiedBlock) -> Result<(), StateError> {
        let anchor = self.store.anchor();
        if !anchor.is_parent_of(block.header()) {
            return Err(StateError::NotChild {
                anchor: anchor.hash(),
                child: block.header().hash(),
            });
        }
        let previous = Arc::clone(&self.store);
        let store = Arc::make_mut(&mut self.store);
        let (header, bal) = block.into_parts();
        store.apply_bal(&bal);
        store.push_anchor(header);
        self.retain(previous);
        self.txs.revalidate(&self.store);
        Ok(())
    }

    /// Replaces the store with one anchored at `target` holding `facts`, the proven pins.
    /// Every learned key becomes unheld. Code carries over because it is content-addressed.
    /// The header window carries over when `target` is the anchor's child. The replaced store
    /// is retained like [`State::advance`] retains it, and retained stores whose blocks are
    /// not held ancestors of `target` are dropped. Located transactions whose block is no
    /// longer on the held chain return to pending.
    ///
    /// All or nothing: every fact's block is checked before the store is replaced.
    pub fn reanchor(&mut self, target: VerifiedHeader, facts: &[Proven]) -> Result<(), StateError> {
        let target_hash = target.hash();
        if let Some(at) = facts
            .iter()
            .filter_map(Proven::at)
            .find(|at| at.hash != target_hash)
        {
            return Err(StateError::Stale {
                proven_at: at.hash,
                anchor: target_hash,
            });
        }
        let old = &self.store;
        let mut store = Store {
            ancestors: VecDeque::new(),
            anchor: old.anchor.clone(),
            accounts: HashMap::new(),
            code: old.code.clone(),
        };
        if old.anchor.is_parent_of(&target) {
            store.ancestors.clone_from(&old.ancestors);
        }
        store.push_anchor(target);
        if store
            .ancestors
            .back()
            .is_some_and(|parent| !parent.is_parent_of(&store.anchor))
        {
            store.ancestors.clear();
        }
        for proven in facts {
            store.insert_unchecked(proven);
        }
        let previous = std::mem::replace(&mut self.store, Arc::new(store));
        self.retain(previous);
        self.txs.revalidate(&self.store);
        Ok(())
    }

    /// Folds facts a request proved at `anchor` into the canonical store. Facts for keys
    /// outside the working set are skipped. If `anchor` is not the current one, because the
    /// anchor moved or the request read a retained store, only code is kept: it is valid at
    /// every block.
    pub fn merge(&mut self, anchor: B256, facts: &[Proven]) -> Merge {
        let current = anchor == self.store.anchor().hash()
            && facts.iter().all(|proven| self.store.check(proven).is_ok());
        let kept: Vec<&Proven> = facts
            .iter()
            .filter(|proven| match proven.key() {
                None => true,
                Some(key) => current && self.working.contains(key),
            })
            .collect();
        if !kept.is_empty() {
            let store = Arc::make_mut(&mut self.store);
            for proven in kept {
                store.insert_unchecked(proven);
            }
        }
        if current { Merge::Kept } else { Merge::Dropped }
    }
}

/// A state transition was refused. The state is unchanged.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum StateError {
    /// A fact was proven at a block other than the anchor.
    #[error("fact proven at {proven_at} but the store is anchored at {anchor}")]
    Stale {
        /// Block the fact was proven at.
        proven_at: B256,
        /// Current anchor.
        anchor: B256,
    },
    /// A block is not the anchor's child.
    #[error("block {child} does not extend anchor {anchor}")]
    NotChild {
        /// Current anchor.
        anchor: B256,
        /// The offered block.
        child: B256,
    },
    /// Ancestor headers do not link to the oldest held header.
    #[error("ancestor headers do not link to the oldest held header")]
    Unlinked,
    /// Ancestors were offered past the window. None were inserted.
    #[error("{offered} ancestors offered but the header window has room for {room}")]
    WindowFull {
        /// Number of ancestors offered.
        offered: usize,
        /// Free slots in the window.
        room: usize,
    },
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::{BlockWrites, World, WorldAccount, eth};
    use crate::verify::{self, VerifyError};
    use crate::working::WORKING_SET_CAP;
    use alloy_primitives::{Bytes, address, keccak256};
    use alloy_rlp::Decodable;

    const ALICE: Address = address!("0x00000000000000000000000000000000000a11ce");
    const TOKEN: Address = address!("0x0000000000000000000000000000000000070c3e");
    const DOOMED: Address = address!("0x00000000000000000000000000000000000d00b3");
    const NEWBIE: Address = address!("0x000000000000000000000000000000000000beef");
    const STRANGER: Address = address!("0x0000000000000000000000000000000000057a9e");

    fn slot(n: u64) -> U256 {
        U256::from(n)
    }

    fn accounts() -> Vec<(Address, WorldAccount)> {
        vec![
            (ALICE, WorldAccount::eoa(eth(5)).with_nonce(1)),
            (
                TOKEN,
                WorldAccount::contract(vec![0x00])
                    .with_slot(slot(1), slot(11))
                    .with_slot(slot(2), slot(22)),
            ),
            (
                DOOMED,
                WorldAccount::eoa(U256::from(1)).with_slot(slot(1), slot(5)),
            ),
            (STRANGER, WorldAccount::eoa(eth(9))),
        ]
    }

    fn world() -> World {
        World::genesis(accounts())
    }

    fn pins() -> Vec<Key> {
        vec![
            Key::Account(ALICE),
            Key::Slot(TOKEN, slot(1)),
            Key::Slot(TOKEN, slot(2)),
            Key::Slot(DOOMED, slot(1)),
            Key::Account(NEWBIE),
        ]
    }

    fn prove_all(world: &World, at: &VerifiedHeader, keys: &[Key]) -> Vec<Proven> {
        keys.iter().map(|key| world.prove(at, *key)).collect()
    }

    fn anchored(world: &World) -> State {
        let head = world.verified(world.head());
        let working = WorkingSet::new(WORKING_SET_CAP, pins(), Vec::new());
        let mut state = State::new(head.clone(), working);
        state
            .reanchor(head.clone(), &prove_all(world, &head, &pins()))
            .unwrap();
        state
    }

    /// Every key the store holds reads exactly what a fresh proof at the anchor says.
    fn assert_matches_world(store: &Store, world: &World, keys: &[Key]) {
        let mut fresh = Store::new(store.anchor().clone());
        for proven in prove_all(world, store.anchor(), keys) {
            fresh.insert(&proven).unwrap();
        }
        for key in keys {
            match key {
                Key::Account(address) => {
                    assert_eq!(store.account(*address), fresh.account(*address), "{key:?}");
                }
                Key::Slot(address, slot) => {
                    assert_eq!(
                        store.slot(*address, *slot),
                        fresh.slot(*address, *slot),
                        "{key:?}"
                    );
                }
            }
        }
    }

    fn mine_bal(world: &mut World, writes: BlockWrites) -> VerifiedBlock {
        let hash = world.mine(writes);
        verify::bal(world.verified(hash), world.bal(hash)).unwrap()
    }

    #[test]
    fn bal_applies_post_values_to_held_keys() {
        assert_bal_applies(world());
    }

    #[test]
    fn bal_applies_post_values_under_amsterdam() {
        let world = World::amsterdam(accounts());
        assert!(world.header(world.head()).slot_number.is_some());
        assert_bal_applies(world);
    }

    fn assert_bal_applies(mut world: World) {
        let mut state = anchored(&world);
        let code = Bytes::from_static(&[0x60, 0x01, 0x00]);
        let block = mine_bal(
            &mut world,
            BlockWrites::default()
                .balance(ALICE, eth(4))
                .balance(ALICE, eth(3))
                .nonce(ALICE, 2)
                .slot(TOKEN, slot(1), U256::ZERO)
                .slot(TOKEN, slot(3), slot(33))
                .read(TOKEN, slot(2))
                .balance(NEWBIE, eth(1))
                .code(NEWBIE, code.clone())
                .balance(DOOMED, U256::ZERO)
                .balance(STRANGER, eth(10)),
        );
        let child = block.header().clone();
        state.advance(block).unwrap();
        let store = state.store();

        assert_eq!(store.anchor(), &child);
        assert_eq!(
            store.account(ALICE),
            Ok(Some(&Account {
                nonce: 2,
                balance: eth(3),
                code_hash: KECCAK256_EMPTY
            }))
        );
        assert_eq!(
            (
                store.slot(TOKEN, slot(1)),
                store.slot(TOKEN, slot(2)),
                store.slot(TOKEN, slot(3))
            ),
            (
                Ok(U256::ZERO),
                Ok(slot(22)),
                Err(Miss::Storage(TOKEN, slot(3)))
            )
        );
        assert_eq!(
            store.account(NEWBIE),
            Ok(Some(&Account {
                nonce: 0,
                balance: eth(1),
                code_hash: keccak256(&code)
            }))
        );
        assert_eq!(
            store.code(keccak256(&code)).map(|c| c.original_bytes()),
            Ok(code)
        );
        assert_eq!(store.entry(DOOMED), Some(&Entry::Absent));
        assert_eq!(store.slot(DOOMED, slot(1)), Ok(U256::ZERO));
        assert_eq!(store.account(STRANGER), Err(Miss::Account(STRANGER)));

        let mut held = pins();
        held.push(Key::Account(DOOMED));
        assert_matches_world(store, &world, &held);
    }

    #[test]
    fn bal_must_match_header_hash() {
        let mut world = world();
        let hash = world.mine(BlockWrites::default().balance(ALICE, eth(4)));
        let header = world.verified(hash);
        let expected = header.header().block_access_list_hash.unwrap();

        let mut bal =
            Vec::<AccountChanges>::decode(&mut world.bal(hash).unwrap().as_ref()).unwrap();
        bal[0].balance_changes[0].post_balance = eth(400);
        let forged = Bytes::from(alloy_rlp::encode(&bal));
        assert_eq!(
            verify::bal(header.clone(), Some(forged)).unwrap_err(),
            VerifyError::BalHash {
                expected,
                got: alloy_eip7928::compute_block_access_list_hash(&bal)
            }
        );
        let unavailable = verify::bal(header, None).unwrap_err();
        assert_eq!(unavailable, VerifyError::BalUnavailable { block: hash });
        assert!(!unavailable.blames_peer());

        let pre_fork = world.mine_without_bal(BlockWrites::default());
        assert_eq!(
            verify::bal(world.verified(pre_fork), None).unwrap_err(),
            VerifyError::NoBalHash { block: pre_fork }
        );
    }

    #[test]
    fn plan_extends_short_bal_gaps_and_applies_them() {
        assert_extends(world());
    }

    #[test]
    fn plan_extends_under_amsterdam() {
        assert_extends(World::amsterdam(accounts()));
    }

    fn assert_extends(mut world: World) {
        let mut state = anchored(&world);
        let anchor = state.anchor().clone();
        assert_eq!(state.plan(&anchor), Plan::Current);

        for i in 1..=3 {
            world.mine(
                BlockWrites::default()
                    .balance(ALICE, eth(i))
                    .slot(TOKEN, slot(2), slot(i)),
            );
        }
        let target = world.verified(world.head());
        assert_eq!(state.plan(&target), Plan::Extend { blocks: 3 });

        let between = world.headers_between(anchor.hash(), target.hash());
        for header in verify::chain(&anchor, target.clone(), between).unwrap() {
            let raw = world.bal(header.hash());
            state.advance(verify::bal(header, raw).unwrap()).unwrap();
        }
        assert_eq!(state.anchor(), &target);
        assert_eq!(state.plan(&anchor), Plan::Current);
        assert_eq!(state.store().block_hash(anchor.number()), Ok(anchor.hash()));
        assert_matches_world(state.store(), &world, &pins());
    }

    #[test]
    fn plan_reanchors_long_gaps() {
        let mut world = world();
        let state = anchored(&world);
        for _ in 0..=MAX_BAL_GAP {
            world.mine(BlockWrites::default());
        }
        assert_eq!(state.plan(&world.verified(world.head())), Plan::Reanchor);
    }

    #[test]
    fn plan_reanchors_without_bal_hashes() {
        let mut world = world();
        let state = anchored(&world);
        let pre_fork = world.mine_without_bal(BlockWrites::default());
        assert_eq!(state.plan(&world.verified(pre_fork)), Plan::Reanchor);

        let mut pre_fork_state = anchored(&world);
        assert_eq!(pre_fork_state.anchor().hash(), pre_fork);
        let post_fork = world.mine(BlockWrites::default());
        assert_eq!(
            pre_fork_state.plan(&world.verified(post_fork)),
            Plan::Reanchor
        );

        let block = verify::bal(world.verified(post_fork), world.bal(post_fork)).unwrap();
        pre_fork_state.advance(block).unwrap();
        assert_eq!(pre_fork_state.anchor().hash(), post_fork);
    }

    #[test]
    fn plan_reanchors_on_non_linking_heads() {
        let mut world = world();
        world.mine(BlockWrites::default().balance(ALICE, eth(1)));
        let mut state = anchored(&world);
        let sibling = world.fork_head(BlockWrites::default().balance(ALICE, eth(2)));
        assert_eq!(state.plan(&world.verified(sibling)), Plan::Reanchor);

        let nephew = world.mine(BlockWrites::default());
        assert_eq!(state.plan(&world.verified(nephew)), Plan::Reanchor);

        let before = state.store().anchor().clone();
        let block = verify::bal(world.verified(nephew), world.bal(nephew)).unwrap();
        assert_eq!(
            state.advance(block),
            Err(StateError::NotChild {
                anchor: before.hash(),
                child: nephew
            })
        );
        assert_eq!(state.anchor(), &before);
    }

    #[test]
    fn unknown_keys_are_misses_never_zero() {
        let world = world();
        let head = world.verified(world.head());
        let mut store = Store::new(head.clone());
        assert_eq!(store.account(ALICE), Err(Miss::Account(ALICE)));
        assert_eq!(store.slot(TOKEN, slot(1)), Err(Miss::Account(TOKEN)));

        store
            .insert(&world.prove(&head, Key::Account(TOKEN)))
            .unwrap();
        assert_eq!(
            store.slot(TOKEN, slot(1)),
            Err(Miss::Storage(TOKEN, slot(1)))
        );
        let code_hash = store.account(TOKEN).unwrap().unwrap().code_hash;
        assert_eq!(store.code(code_hash), Err(Miss::Code(code_hash)));
        assert_eq!(
            store.block_hash(head.number() - 1),
            Err(Miss::BlockHash(head.number() - 1))
        );
        assert_eq!(store.block_hash(head.number()), Ok(head.hash()));
        assert!(!store.holds(Key::Slot(TOKEN, slot(1))));
    }

    #[test]
    fn facts_enter_only_at_the_anchor() {
        let mut world = world();
        let mut state = anchored(&world);
        let anchor = state.anchor().clone();
        let fact = world.prove(&anchor, Key::Account(STRANGER));
        state.learn([Key::Account(STRANGER)]);

        assert_eq!(
            state.merge(anchor.hash(), std::slice::from_ref(&fact)),
            Merge::Kept
        );
        assert!(state.store().holds(Key::Account(STRANGER)));

        let block = mine_bal(&mut world, BlockWrites::default().balance(STRANGER, eth(1)));
        state.advance(block).unwrap();
        assert_eq!(
            state.store().account(STRANGER).unwrap().unwrap().balance,
            eth(1)
        );
        assert_eq!(
            state.merge(anchor.hash(), std::slice::from_ref(&fact)),
            Merge::Dropped
        );

        let mut store = Store::clone(state.store());
        assert_eq!(
            store.insert(&fact),
            Err(StateError::Stale {
                proven_at: anchor.hash(),
                anchor: state.anchor().hash()
            })
        );
        let target = state.anchor().clone();
        assert_eq!(
            state.reanchor(target.clone(), std::slice::from_ref(&fact)),
            Err(StateError::Stale {
                proven_at: anchor.hash(),
                anchor: target.hash()
            })
        );
        state
            .reanchor(target.clone(), &prove_all(&world, &target, &pins()))
            .unwrap();
        assert_eq!(
            state.unheld().collect::<Vec<_>>(),
            vec![Key::Account(STRANGER)]
        );
        assert_eq!(
            state.store().account(STRANGER),
            Err(Miss::Account(STRANGER))
        );
        assert_matches_world(state.store(), &world, &pins());
    }

    #[test]
    fn merge_keeps_only_working_set_keys_and_keeps_code_across_moves() {
        let mut world = world();
        let mut state = anchored(&world);
        let anchor = state.anchor().clone();
        let stranger = world.prove(&anchor, Key::Account(STRANGER));
        let code = world.prove_code(keccak256([0x00]));
        assert_eq!(
            state.merge(anchor.hash(), std::slice::from_ref(&stranger)),
            Merge::Kept
        );
        assert!(!state.store().holds(Key::Account(STRANGER)));

        let block = mine_bal(&mut world, BlockWrites::default());
        state.advance(block).unwrap();
        assert_eq!(
            state.merge(anchor.hash(), &[stranger, code]),
            Merge::Dropped
        );
        assert_eq!(
            state
                .store()
                .code(keccak256([0x00]))
                .map(|c| c.original_bytes()),
            Ok(Bytes::from_static(&[0x00]))
        );
    }

    #[test]
    fn an_evicted_key_leaves_the_store() {
        let world = world();
        let head = world.verified(world.head());
        let working = WorkingSet::new(2, [Key::Account(ALICE)], Vec::new());
        let mut state = State::new(head.clone(), working);
        for address in [STRANGER, DOOMED] {
            state.learn([Key::Account(address)]);
            let fact = world.prove(&head, Key::Account(address));
            assert_eq!(
                state.merge(head.hash(), std::slice::from_ref(&fact)),
                Merge::Kept
            );
        }
        assert!(!state.store().holds(Key::Account(STRANGER)));
        assert!(state.store().holds(Key::Account(DOOMED)));
        assert_eq!(
            state.unheld().collect::<Vec<_>>(),
            vec![Key::Account(ALICE)]
        );
    }

    #[test]
    fn ancestors_extend_the_header_window() {
        let mut world = world();
        let first = world.head();
        for _ in 0..3 {
            world.mine(BlockWrites::default());
        }
        let head = world.verified(world.head());
        let mut store = Store::new(head.clone());
        let ancestors =
            verify::ancestors(&head, world.headers_between(first, head.hash())).unwrap();
        store.insert_ancestors(ancestors).unwrap();
        assert_eq!(store.headers().count(), 3);
        assert_eq!(
            store.block_hash(head.number() - 2),
            Ok(store.header(head.number() - 2).unwrap().hash())
        );
        assert_eq!(
            store.insert_ancestors(vec![world.verified(world.head())]),
            Err(StateError::Unlinked)
        );
    }

    #[test]
    fn full_window_serves_blockhash_256_back() {
        let mut world = world();
        let mut state = anchored(&world);
        let genesis = state.anchor().clone();
        for _ in 0..=HEADER_WINDOW {
            let block = mine_bal(&mut world, BlockWrites::default());
            state.advance(block).unwrap();
        }
        let store = state.store();
        let anchor = store.anchor().number();
        assert_eq!(anchor, genesis.number() + HEADER_WINDOW as u64 + 1);
        assert_eq!(store.headers().count(), HEADER_WINDOW + 1);
        let oldest = world.headers_between(genesis.hash(), store.anchor().hash());
        assert_eq!(
            store.block_hash(anchor - HEADER_WINDOW as u64),
            Ok(oldest[HEADER_WINDOW - 1].hash_slow())
        );
        assert_eq!(
            store.block_hash(genesis.number()),
            Err(Miss::BlockHash(genesis.number()))
        );
        let mut store = Store::clone(store);
        assert_eq!(
            store.insert_ancestors(vec![genesis]),
            Err(StateError::WindowFull {
                offered: 1,
                room: 0
            })
        );
    }

    #[test]
    fn ancestors_past_the_window_are_refused_whole() {
        let mut world = world();
        let first = world.head();
        for _ in 0..=HEADER_WINDOW + 1 {
            world.mine(BlockWrites::default());
        }
        let head = world.verified(world.head());
        let ancestors =
            verify::ancestors(&head, world.headers_between(first, head.hash())).unwrap();
        assert_eq!(ancestors.len(), HEADER_WINDOW + 1);
        let mut store = Store::new(head.clone());
        assert_eq!(
            store.insert_ancestors(ancestors.clone()),
            Err(StateError::WindowFull {
                offered: HEADER_WINDOW + 1,
                room: HEADER_WINDOW
            })
        );
        assert_eq!(store.headers().count(), 1);
        store.insert_ancestors(ancestors[1..].to_vec()).unwrap();
        let back = head.number() - HEADER_WINDOW as u64;
        assert_eq!(store.block_hash(back), Ok(ancestors[1].hash()));
    }
}
