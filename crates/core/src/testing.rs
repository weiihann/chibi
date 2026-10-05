//! An in-process chain with real tries, for tests here and in chibi-node's offline fixture.
//!
//! [`World`] holds full state, computes real state and storage roots, serves single-key snap
//! replies with real range proofs at any root it has produced, and mines child blocks from
//! explicit [`BlockWrites`], producing headers whose BAL hash, transaction root, receipts root,
//! and logs bloom all match what it serves.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use alloy_consensus::{
    Header, Signed, Transaction as _, TxLegacy, TxReceipt, TxType,
    constants::EMPTY_OMMER_ROOT_HASH,
    proofs::{calculate_transaction_root, calculate_withdrawals_root},
};
use alloy_eip7928::{
    AccountChanges, BalanceChange, BlockAccessIndex, CodeChange, NonceChange, SlotChanges,
    StorageChange, compute_block_access_list_hash,
};
use alloy_eips::eip7685::EMPTY_REQUESTS_HASH;
use alloy_primitives::{
    Address, B256, Bloom, Bytes, KECCAK256_EMPTY, Log, Signature, TxKind, U256, keccak256,
};
use alloy_trie::{EMPTY_ROOT_HASH, HashBuilder, Nibbles, TrieAccount, proof::ProofRetainer};
use reth_chainspec::{ChainSpec, EthereumHardfork, EthereumHardforks, MAINNET, SEPOLIA};

use crate::state::{Key, Miss};
use crate::verify::{
    self, AccountQuery, CodeQuery, Proven, StorageQuery, TrustedHash, VerifiedHeader,
};
use crate::wire::{self, AccountRangeRequest, AccountReply, StorageRangeRequest, StorageReply};

/// Number of the genesis block. Post-merge on mainnet and Sepolia rules.
pub const GENESIS_NUMBER: u64 = 24_000_000;

/// Timestamp of the genesis block of [`World::genesis`]. Osaka rules on mainnet.
pub const GENESIS_TIMESTAMP: u64 = 1_790_000_000;

/// Seconds between mined blocks.
pub const BLOCK_TIME: u64 = 12;

/// Base fee of every mined block.
pub const BASE_FEE: u64 = 1_000_000_000;

/// Gas price of transactions built by [`legacy_transfer`].
pub const GAS_PRICE: u64 = 2_000_000_000;

/// Gas limit of every mined block.
pub const GAS_LIMIT: u64 = 36_000_000;

/// `n` ether in wei.
pub fn eth(n: u64) -> U256 {
    U256::from(n) * U256::from(10u64).pow(U256::from(18))
}

/// A legacy value transfer with a fixed placeholder signature. Its hash is stable, but sender
/// recovery is meaningless.
pub fn legacy_transfer(nonce: u64, to: Address, value: U256) -> wire::Transaction {
    let tx = TxLegacy {
        chain_id: Some(1),
        nonce,
        gas_price: u128::from(GAS_PRICE),
        gas_limit: 21_000,
        to: TxKind::Call(to),
        value,
        input: Bytes::new(),
    };
    let signature = Signature::new(U256::from(1u64), U256::from(1u64), false);
    wire::Transaction::Legacy(Signed::new_unhashed(tx, signature))
}

/// One account of the world.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct WorldAccount {
    /// Balance in wei.
    pub balance: U256,
    /// Nonce.
    pub nonce: u64,
    /// Runtime code, empty for an EOA.
    pub code: Bytes,
    /// Non-zero slots.
    pub storage: BTreeMap<U256, U256>,
}

impl WorldAccount {
    /// An externally owned account.
    pub fn eoa(balance: U256) -> Self {
        Self {
            balance,
            ..Self::default()
        }
    }

    /// A contract with `code` and no balance.
    pub fn contract(code: impl Into<Bytes>) -> Self {
        Self {
            code: code.into(),
            ..Self::default()
        }
    }

    /// Sets a slot.
    pub fn with_slot(mut self, slot: U256, value: U256) -> Self {
        if value.is_zero() {
            self.storage.remove(&slot);
        } else {
            self.storage.insert(slot, value);
        }
        self
    }

    /// Sets the nonce.
    pub const fn with_nonce(mut self, nonce: u64) -> Self {
        self.nonce = nonce;
        self
    }

    fn code_hash(&self) -> B256 {
        if self.code.is_empty() {
            KECCAK256_EMPTY
        } else {
            keccak256(&self.code)
        }
    }

    fn is_empty(&self) -> bool {
        self.balance.is_zero() && self.nonce == 0 && self.code.is_empty()
    }

    fn storage_leaves(&self) -> Vec<(B256, U256)> {
        let mut leaves: Vec<(B256, U256)> = self
            .storage
            .iter()
            .map(|(slot, value)| (keccak256(B256::from(*slot)), *value))
            .collect();
        leaves.sort_unstable_by_key(|(key, _)| *key);
        leaves
    }

    fn trie_account(&self) -> TrieAccount {
        TrieAccount {
            nonce: self.nonce,
            balance: self.balance,
            storage_root: trie(&self.storage_leaves(), &[]).0,
            code_hash: self.code_hash(),
        }
    }
}

#[derive(Clone, Copy, Debug)]
enum Write {
    Balance(Address, U256),
    Nonce(Address, u64),
    Slot(Address, U256, U256),
    Read(Address, U256),
}

/// The state writes and transactions of one mined block.
///
/// Writes apply in order, and each gets its own block access index, so writing a field twice
/// records two changes and the later one wins.
#[derive(Clone, Debug, Default)]
pub struct BlockWrites {
    writes: Vec<Write>,
    codes: Vec<(usize, Address, Bytes)>,
    txs: Vec<(wire::Transaction, u64, usize)>,
}

impl BlockWrites {
    /// Sets a balance.
    pub fn balance(mut self, address: Address, balance: U256) -> Self {
        self.writes.push(Write::Balance(address, balance));
        self
    }

    /// Sets a nonce.
    pub fn nonce(mut self, address: Address, nonce: u64) -> Self {
        self.writes.push(Write::Nonce(address, nonce));
        self
    }

    /// Sets a slot; zero clears it.
    pub fn slot(mut self, address: Address, slot: U256, value: U256) -> Self {
        self.writes.push(Write::Slot(address, slot, value));
        self
    }

    /// Sets the code.
    pub fn code(mut self, address: Address, code: impl Into<Bytes>) -> Self {
        self.codes.push((self.writes.len(), address, code.into()));
        self
    }

    /// Records a read-only slot access, which changes nothing.
    pub fn read(mut self, address: Address, slot: U256) -> Self {
        self.writes.push(Write::Read(address, slot));
        self
    }

    /// Includes `tx` with a receipt using `gas_used` and emitting `logs` logs. Transactions do
    /// not execute; state changes come from the explicit writes.
    pub fn tx(mut self, tx: wire::Transaction, gas_used: u64, logs: usize) -> Self {
        self.txs.push((tx, gas_used, logs));
        self
    }
}

#[derive(Clone, Debug)]
struct Block {
    header: Header,
    body: wire::Body,
    receipts: Vec<wire::Receipt>,
    bal: Option<Bytes>,
}

/// An in-process chain with full state.
#[derive(Clone, Debug)]
pub struct World {
    spec: Arc<ChainSpec>,
    genesis_timestamp: u64,
    accounts: BTreeMap<Address, WorldAccount>,
    states: HashMap<B256, BTreeMap<Address, WorldAccount>>,
    codes: HashMap<B256, Bytes>,
    blocks: HashMap<B256, Block>,
    head: B256,
}

impl World {
    /// A mainnet chain under Osaka rules whose genesis holds `accounts`. Genesis carries a BAL
    /// hash.
    pub fn genesis(accounts: impl IntoIterator<Item = (Address, WorldAccount)>) -> Self {
        Self::genesis_on(Arc::clone(&MAINNET), GENESIS_TIMESTAMP, accounts)
    }

    /// A Sepolia chain under Amsterdam rules (EIP-7928, EIP-8037, EIP-2780), its genesis one
    /// block past the fork.
    pub fn amsterdam(accounts: impl IntoIterator<Item = (Address, WorldAccount)>) -> Self {
        let spec = Arc::clone(&SEPOLIA);
        let activation = spec.ethereum_fork_activation(EthereumHardfork::Amsterdam);
        let Some(fork) = activation.as_timestamp() else {
            panic!("Sepolia schedules Amsterdam by {activation:?}, not by timestamp")
        };
        Self::genesis_on(spec, fork + BLOCK_TIME, accounts)
    }

    /// A chain on `spec` whose genesis, at `timestamp`, holds `accounts`. Every block carries a
    /// BAL hash unless mined with [`World::mine_without_bal`]; blocks under Amsterdam also
    /// carry a slot number.
    pub fn genesis_on(
        spec: Arc<ChainSpec>,
        timestamp: u64,
        accounts: impl IntoIterator<Item = (Address, WorldAccount)>,
    ) -> Self {
        let mut world = Self {
            spec,
            genesis_timestamp: timestamp,
            accounts: accounts.into_iter().collect(),
            states: HashMap::new(),
            codes: HashMap::new(),
            blocks: HashMap::new(),
            head: B256::ZERO,
        };
        world.head = world.seal(None, Vec::new(), Some(&[]));
        world
    }

    /// The chain spec the world's headers follow.
    pub fn spec(&self) -> Arc<ChainSpec> {
        Arc::clone(&self.spec)
    }

    /// Hash of the newest block.
    pub const fn head(&self) -> B256 {
        self.head
    }

    /// The header of block `hash`.
    pub fn header(&self, hash: B256) -> Header {
        self.block(hash).header.clone()
    }

    /// The header of block `hash`, verified as the light client would attest it.
    pub fn verified(&self, hash: B256) -> VerifiedHeader {
        match verify::header(TrustedHash::attested(hash), self.header(hash)) {
            Ok(header) => header,
            Err(error) => panic!("world header {hash} does not verify: {error}"),
        }
    }

    /// The body of block `hash`.
    pub fn body(&self, hash: B256) -> wire::Body {
        self.block(hash).body.clone()
    }

    /// The receipts of block `hash`, without blooms as eth/69 sends them.
    pub fn receipts(&self, hash: B256) -> Vec<wire::Receipt> {
        self.block(hash).receipts.clone()
    }

    /// The raw BAL of block `hash`, `None` for blocks mined without one.
    pub fn bal(&self, hash: B256) -> Option<Bytes> {
        self.block(hash).bal.clone()
    }

    /// Headers strictly between `anchor` and `target`, newest first.
    pub fn headers_between(&self, anchor: B256, target: B256) -> Vec<Header> {
        let mut headers = Vec::new();
        let mut cursor = self.block(target).header.parent_hash;
        while cursor != anchor {
            let header = self.header(cursor);
            cursor = header.parent_hash;
            headers.push(header);
        }
        headers
    }

    /// Current state of `address`.
    pub fn account(&self, address: Address) -> Option<&WorldAccount> {
        self.accounts.get(&address)
    }

    /// Applies `writes` and mines a child of the head with a BAL. Returns its hash.
    pub fn mine(&mut self, writes: BlockWrites) -> B256 {
        let bal = self.apply(&writes);
        self.head = self.seal(Some(self.head), writes.txs, Some(&bal));
        self.head
    }

    /// Applies `writes` and mines a child of the head without a BAL hash, as before
    /// Glamsterdam.
    pub fn mine_without_bal(&mut self, writes: BlockWrites) -> B256 {
        self.apply(&writes);
        self.head = self.seal(Some(self.head), writes.txs, None);
        self.head
    }

    /// Mines a sibling of the head: a child of the head's parent with `writes` applied to the
    /// current state. The sibling becomes the head.
    pub fn fork_head(&mut self, writes: BlockWrites) -> B256 {
        let parent = self.block(self.head).header.parent_hash;
        let bal = self.apply(&writes);
        self.head = self.seal(Some(parent), writes.txs, Some(&bal));
        self.head
    }

    /// Serves a single-key account range: the leaf at the key, else the next leaf as the
    /// boundary, with a proof for the key and the returned leaf. Unknown roots get an empty
    /// reply, as a peer outside its snap window answers.
    pub fn account_reply(&self, request: &AccountRangeRequest) -> AccountReply {
        let Some(state) = self.states.get(&request.root) else {
            return AccountReply::default();
        };
        let leaves = state_leaves(state);
        let (accounts, proof) = range(&leaves, request.key);
        AccountReply { accounts, proof }
    }

    /// Serves a single-key storage range for one account, as [`World::account_reply`] does.
    pub fn storage_reply(&self, request: &StorageRangeRequest) -> StorageReply {
        let account = self.states.get(&request.root).and_then(|state| {
            state
                .iter()
                .find(|(address, _)| keccak256(address) == request.account)
        });
        let Some((_, account)) = account else {
            return StorageReply::default();
        };
        let (slots, proof) = range(&account.storage_leaves(), request.slot);
        StorageReply { slots, proof }
    }

    /// Code by hash.
    pub fn code(&self, hash: B256) -> Option<Bytes> {
        self.codes.get(&hash).cloned()
    }

    /// Proves `key` at `at` through the production verifier.
    pub fn prove(&self, at: &VerifiedHeader, key: Key) -> Proven {
        let proven = match key {
            Key::Account(address) => {
                let query = AccountQuery::new(at, address);
                query.verify(self.account_reply(&query.request()))
            }
            Key::Slot(address, slot) => {
                let query = StorageQuery::new(at, address, slot);
                query.verify(
                    self.account_reply(&query.account_request()),
                    Some(self.storage_reply(&query.storage_request())),
                )
            }
        };
        match proven {
            Ok(proven) => proven,
            Err(error) => panic!("world proof for {key:?} does not verify: {error}"),
        }
    }

    /// Proves the code hashing to `hash` through the production verifier.
    pub fn prove_code(&self, hash: B256) -> Proven {
        match CodeQuery::new(hash).verify(self.code(hash)) {
            Ok(proven) => proven,
            Err(error) => panic!("world code {hash} does not verify: {error}"),
        }
    }

    /// Proves a store miss at `at`. `None` for block hashes, which are headers, not proofs.
    pub fn prove_miss(&self, at: &VerifiedHeader, miss: Miss) -> Option<Proven> {
        match miss {
            Miss::Account(address) => Some(self.prove(at, Key::Account(address))),
            Miss::Storage(address, slot) => Some(self.prove(at, Key::Slot(address, slot))),
            Miss::Code(hash) => Some(self.prove_code(hash)),
            Miss::BlockHash(_) => None,
        }
    }

    /// Every account and non-zero slot of the current state, as keys.
    pub fn keys(&self) -> Vec<Key> {
        self.accounts
            .iter()
            .flat_map(|(address, account)| {
                std::iter::once(Key::Account(*address)).chain(
                    account
                        .storage
                        .keys()
                        .map(|slot| Key::Slot(*address, *slot)),
                )
            })
            .collect()
    }

    fn block(&self, hash: B256) -> &Block {
        match self.blocks.get(&hash) {
            Some(block) => block,
            None => panic!("world has no block {hash}"),
        }
    }

    fn apply(&mut self, writes: &BlockWrites) -> Vec<AccountChanges> {
        let mut changes: BTreeMap<Address, AccountChanges> = BTreeMap::new();
        let mut codes = writes.codes.iter().peekable();
        for (position, write) in writes.writes.iter().enumerate() {
            while let Some((_, address, code)) = codes.next_if(|(at, ..)| *at == position) {
                self.set_code(&mut changes, position, *address, code);
            }
            let index = BlockAccessIndex(position as u64 + 1);
            match *write {
                Write::Balance(address, balance) => {
                    self.accounts.entry(address).or_default().balance = balance;
                    entry(&mut changes, address)
                        .balance_changes
                        .push(BalanceChange::new(index, balance));
                }
                Write::Nonce(address, nonce) => {
                    self.accounts.entry(address).or_default().nonce = nonce;
                    entry(&mut changes, address)
                        .nonce_changes
                        .push(NonceChange::new(index, nonce));
                }
                Write::Slot(address, slot, value) => {
                    let account = self.accounts.entry(address).or_default();
                    if value.is_zero() {
                        account.storage.remove(&slot);
                    } else {
                        account.storage.insert(slot, value);
                    }
                    let account_changes = entry(&mut changes, address);
                    let change = StorageChange::new(index, value);
                    match account_changes
                        .storage_changes
                        .iter_mut()
                        .find(|s| s.slot == slot)
                    {
                        Some(slot_changes) => slot_changes.push(change),
                        None => account_changes
                            .storage_changes
                            .push(SlotChanges::new(slot, vec![change])),
                    }
                }
                Write::Read(address, slot) => {
                    entry(&mut changes, address).storage_reads.push(slot);
                }
            }
        }
        for (position, address, code) in codes {
            self.set_code(&mut changes, *position, *address, code);
        }
        self.accounts.retain(|_, account| !account.is_empty());
        changes
            .into_values()
            .map(|mut account| {
                account.storage_changes.sort_unstable_by_key(|s| s.slot);
                account.storage_reads.sort_unstable();
                account.storage_reads.dedup();
                account
            })
            .collect()
    }

    fn set_code(
        &mut self,
        changes: &mut BTreeMap<Address, AccountChanges>,
        position: usize,
        address: Address,
        code: &Bytes,
    ) {
        self.accounts.entry(address).or_default().code = code.clone();
        let index = BlockAccessIndex(position as u64 + 1);
        entry(changes, address)
            .code_changes
            .push(CodeChange::new(index, code.clone()));
    }

    fn seal(
        &mut self,
        parent: Option<B256>,
        txs: Vec<(wire::Transaction, u64, usize)>,
        bal: Option<&[AccountChanges]>,
    ) -> B256 {
        for account in self
            .accounts
            .values()
            .filter(|account| !account.code.is_empty())
        {
            self.codes.insert(account.code_hash(), account.code.clone());
        }
        let state_root = trie(&state_leaves(&self.accounts), &[]).0;
        self.states.insert(state_root, self.accounts.clone());

        let mut cumulative = 0;
        let mut transactions = Vec::with_capacity(txs.len());
        let mut receipts = Vec::with_capacity(txs.len());
        for (tx, gas_used, logs) in txs {
            cumulative += gas_used;
            let emitter = match tx.kind() {
                TxKind::Call(to) => to,
                TxKind::Create => Address::ZERO,
            };
            let logs = (0..logs)
                .map(|i| {
                    Log::new_unchecked(emitter, vec![B256::with_last_byte(i as u8)], Bytes::new())
                })
                .collect();
            receipts.push(wire::Receipt {
                tx_type: TxType::Legacy,
                success: true,
                cumulative_gas_used: cumulative,
                logs,
            });
            transactions.push(tx);
        }
        let logs_bloom = receipts.iter().fold(Bloom::ZERO, |mut acc, receipt| {
            acc.accrue_bloom(&receipt.bloom());
            acc
        });
        let body = wire::Body {
            transactions,
            ommers: Vec::new(),
            withdrawals: Some(Default::default()),
        };

        let (parent_hash, number) = match parent {
            Some(hash) => (hash, self.block(hash).header.number + 1),
            None => (B256::ZERO, GENESIS_NUMBER),
        };
        let raw_bal = bal.map(|bal| {
            let mut raw = Vec::new();
            alloy_rlp::encode_list(bal, &mut raw);
            Bytes::from(raw)
        });
        let timestamp = self.genesis_timestamp + BLOCK_TIME * (number - GENESIS_NUMBER);
        let block_access_list_hash = bal.map(compute_block_access_list_hash);
        // Header RLP only carries the slot number after a BAL hash.
        let slot_number = (block_access_list_hash.is_some()
            && self.spec.is_amsterdam_active_at_timestamp(timestamp))
        .then_some(number);
        let header = Header {
            parent_hash,
            ommers_hash: EMPTY_OMMER_ROOT_HASH,
            state_root,
            transactions_root: calculate_transaction_root(&body.transactions),
            receipts_root: wire::Receipt::calculate_receipt_root_no_memo(&receipts),
            logs_bloom,
            number,
            gas_limit: GAS_LIMIT,
            gas_used: cumulative,
            timestamp,
            base_fee_per_gas: Some(BASE_FEE),
            withdrawals_root: Some(calculate_withdrawals_root(&[])),
            blob_gas_used: Some(0),
            excess_blob_gas: Some(0),
            parent_beacon_block_root: Some(B256::ZERO),
            requests_hash: Some(EMPTY_REQUESTS_HASH),
            block_access_list_hash,
            slot_number,
            ..Header::default()
        };
        let hash = header.hash_slow();
        self.blocks.insert(
            hash,
            Block {
                header,
                body,
                receipts,
                bal: raw_bal,
            },
        );
        hash
    }
}

fn entry(changes: &mut BTreeMap<Address, AccountChanges>, address: Address) -> &mut AccountChanges {
    changes
        .entry(address)
        .or_insert_with(|| AccountChanges::new(address))
}

fn state_leaves(state: &BTreeMap<Address, WorldAccount>) -> Vec<(B256, TrieAccount)> {
    let mut leaves: Vec<(B256, TrieAccount)> = state
        .iter()
        .map(|(address, account)| (keccak256(address), account.trie_account()))
        .collect();
    leaves.sort_unstable_by_key(|(key, _)| *key);
    leaves
}

/// The single-key reply for `key`: the leaf at or after it, and proofs for both.
fn range<V: alloy_rlp::Encodable + Clone>(
    leaves: &[(B256, V)],
    key: B256,
) -> (Vec<(B256, V)>, Vec<Bytes>) {
    let position = leaves.partition_point(|(hash, _)| *hash < key);
    let returned: Vec<(B256, V)> = leaves.get(position).cloned().into_iter().collect();
    let mut targets = vec![key];
    targets.extend(returned.iter().map(|(hash, _)| *hash));
    let (_, proof) = trie(leaves, &targets);
    (returned, proof)
}

/// Root of the trie over sorted `leaves`, and the proof nodes for `targets`.
fn trie<V: alloy_rlp::Encodable>(leaves: &[(B256, V)], targets: &[B256]) -> (B256, Vec<Bytes>) {
    if leaves.is_empty() {
        return (EMPTY_ROOT_HASH, Vec::new());
    }
    let targets = targets.iter().copied().map(Nibbles::unpack).collect();
    let mut builder = HashBuilder::default().with_proof_retainer(ProofRetainer::new(targets));
    for (key, value) in leaves {
        builder.add_leaf(Nibbles::unpack(*key), &alloy_rlp::encode(value));
    }
    let root = builder.root();
    let proof = builder
        .take_proof_nodes()
        .into_nodes_sorted()
        .into_iter()
        .map(|(_, node)| node)
        .collect();
    (root, proof)
}

/// `balanceOf`-style implementation: returns `sload(keccak256(calldata[4..36] . 0))`, the
/// slot of a `mapping(address => uint256)` at slot 0, for any selector.
pub fn mapping_reader_code() -> Bytes {
    Bytes::from_static(&[
        0x60, 0x04, 0x35, // calldataload(4)
        0x60, 0x00, 0x52, // mstore(0, holder)
        0x60, 0x00, 0x60, 0x20, 0x52, // mstore(32, 0)
        0x60, 0x40, 0x60, 0x00, 0x20, // keccak256(0, 64)
        0x54, // sload
        0x60, 0x00, 0x52, // mstore(0, value)
        0x60, 0x20, 0x60, 0x00, 0xf3, // return(0, 32)
    ])
}

/// The EIP-1967 implementation slot.
pub const EIP1967_IMPLEMENTATION_SLOT: B256 =
    alloy_primitives::b256!("0x360894a13ba1a3210667c828492db98dca3e2076cc3735a920a3ca505d382bbc");

/// A proxy that DELEGATECALLs the address in [`EIP1967_IMPLEMENTATION_SLOT`] with its calldata
/// and returns the return data.
pub fn delegating_proxy_code() -> Bytes {
    let mut code = vec![
        0x36, 0x60, 0x00, 0x60, 0x00, 0x37, // calldatacopy(0, 0, calldatasize)
        0x60, 0x00, 0x60, 0x00, 0x36, 0x60,
        0x00, // ret size, ret offset, args size, args offset
        0x7f, // push32 slot
    ];
    code.extend_from_slice(EIP1967_IMPLEMENTATION_SLOT.as_slice());
    code.extend_from_slice(&[
        0x54, 0x5a, 0xf4, // sload, gas, delegatecall
        0x3d, 0x60, 0x00, 0x60, 0x00, 0x3e, // returndatacopy(0, 0, returndatasize)
        0x3d, 0x60, 0x00, 0xf3, // return(0, returndatasize)
    ]);
    Bytes::from(code)
}

/// The storage slot of `holder` in a `mapping(address => uint256)` at slot 0.
pub fn mapping_slot(holder: Address) -> U256 {
    let mut preimage = [0u8; 64];
    preimage[12..32].copy_from_slice(holder.as_slice());
    U256::from_be_bytes(keccak256(preimage).0)
}

/// ABI calldata for `balanceOf(holder)`.
pub fn balance_of_calldata(holder: Address) -> Bytes {
    let mut data = vec![0x70, 0xa0, 0x82, 0x31];
    data.extend_from_slice(B256::left_padding_from(holder.as_slice()).as_slice());
    Bytes::from(data)
}

/// A minimal token: `transfer(to, amount)` for any selector, moving `amount` between the
/// caller's and `to`'s entries of a `mapping(address => uint256)` at slot 0 and returning
/// `true`. Reverts when the caller's balance is short.
pub fn mapping_token_code() -> Bytes {
    Bytes::from_static(&[
        0x33, 0x60, 0x00, 0x52, // mstore(0, caller)
        0x60, 0x00, 0x60, 0x20, 0x52, // mstore(32, 0)
        0x60, 0x40, 0x60, 0x00, 0x20, // from = keccak256(0, 64)
        0x80, 0x54, // balance = sload(from)
        0x60, 0x24, 0x35, // amount = calldataload(36)
        0x80, 0x82, 0x10, 0x61, 0x00, 0x3a, 0x57, // if balance < amount goto revert
        0x80, 0x91, 0x03, 0x82, 0x55, // sstore(from, balance - amount)
        0x60, 0x04, 0x35, 0x60, 0x00, 0x52, // mstore(0, to)
        0x60, 0x40, 0x60, 0x00, 0x20, // slot = keccak256(0, 64)
        0x80, 0x54, 0x82, 0x01, 0x90, 0x55, // sstore(slot, sload(slot) + amount)
        0x60, 0x01, 0x60, 0x00, 0x52, // mstore(0, 1)
        0x60, 0x20, 0x60, 0x00, 0xf3, // return(0, 32)
        0x5b, 0x60, 0x00, 0x60, 0x00, 0xfd, // revert: revert(0, 0)
    ])
}

/// The storage slot [`pausable_token_code`] reads as its paused flag.
pub const PAUSED_SLOT: U256 = U256::from_limbs([1, 0, 0, 0]);

/// A token with a pause switch: `balanceOf(holder)` reads the holder's entry of a
/// `mapping(address => uint256)` at slot 0; any other selector is `transfer(to, amount)` as in
/// [`mapping_token_code`], which first reverts if [`PAUSED_SLOT`] is nonzero.
pub fn pausable_token_code() -> Bytes {
    Bytes::from_static(&[
        0x60, 0x00, 0x35, 0x60, 0xe0, 0x1c, // selector = calldataload(0) >> 224
        0x63, 0x70, 0xa0, 0x82, 0x31, 0x14, // selector == balanceOf
        0x61, 0x00, 0x57, 0x57, // goto balanceOf
        0x60, 0x01, 0x54, // sload(PAUSED_SLOT)
        0x61, 0x00, 0x51, 0x57, // if paused goto revert
        0x33, 0x60, 0x00, 0x52, // mstore(0, caller)
        0x60, 0x00, 0x60, 0x20, 0x52, // mstore(32, 0)
        0x60, 0x40, 0x60, 0x00, 0x20, // from = keccak256(0, 64)
        0x80, 0x54, // balance = sload(from)
        0x60, 0x24, 0x35, // amount = calldataload(36)
        0x80, 0x82, 0x10, 0x61, 0x00, 0x51, 0x57, // if balance < amount goto revert
        0x80, 0x91, 0x03, 0x82, 0x55, // sstore(from, balance - amount)
        0x60, 0x04, 0x35, 0x60, 0x00, 0x52, // mstore(0, to)
        0x60, 0x40, 0x60, 0x00, 0x20, // slot = keccak256(0, 64)
        0x80, 0x54, 0x82, 0x01, 0x90, 0x55, // sstore(slot, sload(slot) + amount)
        0x60, 0x01, 0x60, 0x00, 0x52, // mstore(0, 1)
        0x60, 0x20, 0x60, 0x00, 0xf3, // return(0, 32)
        0x5b, 0x60, 0x00, 0x60, 0x00, 0xfd, // revert: revert(0, 0)
        0x5b, // balanceOf:
        0x60, 0x04, 0x35, // calldataload(4)
        0x60, 0x00, 0x52, // mstore(0, holder)
        0x60, 0x00, 0x60, 0x20, 0x52, // mstore(32, 0)
        0x60, 0x40, 0x60, 0x00, 0x20, // keccak256(0, 64)
        0x54, // sload
        0x60, 0x00, 0x52, // mstore(0, value)
        0x60, 0x20, 0x60, 0x00, 0xf3, // return(0, 32)
    ])
}

/// ABI calldata for `transfer(to, amount)`.
pub fn transfer_calldata(to: Address, amount: U256) -> Bytes {
    let mut data = vec![0xa9, 0x05, 0x9c, 0xbb];
    data.extend_from_slice(B256::left_padding_from(to.as_slice()).as_slice());
    data.extend_from_slice(&amount.to_be_bytes::<32>());
    Bytes::from(data)
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::address;

    #[test]
    fn mined_header_commits_to_served_data() {
        let alice = address!("0x00000000000000000000000000000000000a11ce");
        let mut world = World::genesis([(alice, WorldAccount::eoa(eth(3)))]);
        let hash = world.mine(BlockWrites::default().balance(alice, eth(2)).tx(
            legacy_transfer(0, Address::ZERO, eth(1)),
            21_000,
            1,
        ));
        let header = world.verified(hash);
        assert!(verify::bal(header.clone(), world.bal(hash)).is_ok());
        assert!(verify::body(header.clone(), world.body(hash)).is_ok());
        assert!(verify::receipts(header, world.receipts(hash)).is_ok());
        assert_eq!(
            world.header(hash).state_root,
            alloy_trie::root::state_root_unhashed([(
                alice,
                WorldAccount::eoa(eth(2)).trie_account()
            )])
        );
    }
}
