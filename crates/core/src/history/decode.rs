//! Activity decoded from stored history facts at read time. Nothing decoded is ever stored, so
//! a better decoder shows better history without touching the database.

use std::cmp::Reverse;

use alloy_consensus::Transaction as _;
use alloy_eips::eip2718::Decodable2718 as _;
use alloy_primitives::{Address, B256, Log, TxHash, U256, address, b256};

use super::{At, BlockFacts, Coverage, Page, Touch};
use crate::wire;

/// The emitter of EIP-7708 ETH transfer logs.
const ETH_LOGS: Address = address!("0xfffffffffffffffffffffffffffffffffffffffe");
/// `Transfer(address,address,uint256)`: ERC-20, ERC-721 and EIP-7708.
const TRANSFER: B256 = b256!("0xddf252ad1be2c89b69c2b068fc378daa952ba7f163c4a11628f55a4df523b3ef");
/// `Approval(address,address,uint256)`: ERC-20 and ERC-721.
const APPROVAL: B256 = b256!("0x8c5be1e5ebec7d5bd14f71427d1e84f3dd0314c0f7b2291e5b200ac8c7c3b925");
/// `ApprovalForAll(address,address,bool)`: ERC-721 and ERC-1155.
const APPROVAL_FOR_ALL: B256 =
    b256!("0x17307eab39ab6107e8899845ad3d59bd9653f200f220920489ca2b5937696c31");
/// `TransferSingle(address,address,address,uint256,uint256)`: ERC-1155.
const TRANSFER_SINGLE: B256 =
    b256!("0xc3d58168c5ae7397731d063d5bbf3d657854427343f4c083240f7aacaa2d0f62");
/// `TransferBatch(address,address,address,uint256[],uint256[])`: ERC-1155.
const TRANSFER_BATCH: B256 =
    b256!("0x4a39dc06d4c0dbc64b70af90fd698a233a518aa5d07e595d983b8c0526c8f7fb");

/// One thing that happened to an account, at a place in a block.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Activity {
    /// Block number.
    pub number: u64,
    /// Block timestamp in seconds.
    pub timestamp: u64,
    /// Where in the block it happened.
    pub at: At,
    /// The transaction it came from, when it came from one.
    pub tx: Option<TxHash>,
    /// What happened.
    pub effect: Effect,
}

/// What an [`Activity`] did to the account.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Effect {
    /// The account sent a transaction. Its value is not here: ETH moves come only from
    /// [`Effect::Eth`], so a send is never counted twice.
    Sent {
        /// The recipient, `None` for a contract creation or when the encoding is not stored.
        to: Option<Address>,
        /// The nonce, `None` when the encoding is not stored.
        nonce: Option<u64>,
        /// Whether the receipt reports success.
        success: bool,
        /// Execution gas used times the effective gas price, without any blob fee. `None` when
        /// the encoding is not stored, or the price needs a base fee the block lacks.
        fee: Option<U256>,
    },
    /// ETH moved, from an EIP-7708 transfer log.
    Eth {
        /// Sender.
        from: Address,
        /// Recipient.
        to: Address,
        /// Amount in wei.
        amount: U256,
    },
    /// An ERC-20 transfer.
    Token {
        /// The token contract.
        token: Address,
        /// Sender.
        from: Address,
        /// Recipient.
        to: Address,
        /// Amount in the token's smallest unit.
        amount: U256,
    },
    /// An ERC-721 transfer.
    Nft {
        /// The token contract.
        token: Address,
        /// Sender.
        from: Address,
        /// Recipient.
        to: Address,
        /// Token id.
        id: U256,
    },
    /// An ERC-1155 transfer of one id: a `TransferSingle`, or one item of a `TransferBatch`.
    Multi {
        /// The token contract.
        token: Address,
        /// The account that made the transfer.
        operator: Address,
        /// Sender.
        from: Address,
        /// Recipient.
        to: Address,
        /// Token id.
        id: U256,
        /// Amount of that id.
        amount: U256,
    },
    /// An ERC-20 allowance set.
    Approval {
        /// The token contract.
        token: Address,
        /// The account whose tokens may be spent.
        owner: Address,
        /// The account allowed to spend.
        spender: Address,
        /// The allowance.
        amount: U256,
    },
    /// An ERC-721 approval of one token.
    NftApproval {
        /// The token contract.
        token: Address,
        /// The token's owner.
        owner: Address,
        /// The account approved for the token.
        approved: Address,
        /// Token id.
        id: U256,
    },
    /// An operator approved or revoked for all of an owner's tokens.
    ApprovalForAll {
        /// The token contract.
        token: Address,
        /// The tokens' owner.
        owner: Address,
        /// The operator.
        operator: Address,
        /// Whether the operator is now approved.
        approved: bool,
    },
    /// A consensus-layer withdrawal to the account.
    Withdrawal {
        /// Validator index.
        validator: u64,
        /// Amount in wei.
        amount: U256,
    },
    /// A balance change inside a transaction the account took no part in: the fee credit of a
    /// block's fee recipient.
    FeeCredit {
        /// The credit, when the balances before and after are known.
        amount: Option<U256>,
    },
    /// A transaction sent to the account by someone else that did nothing else visible to it.
    Called {
        /// The transaction's sender.
        from: Address,
    },
    /// A balance change before the first or after the last transaction that nothing stored
    /// explains.
    BalanceChange {
        /// The previous known balance.
        before: Option<U256>,
        /// The balance after the change.
        after: U256,
    },
    /// A log naming the account that none of the known shapes match.
    Other {
        /// The contract that emitted it.
        emitter: Address,
        /// Its first topic, the event signature for a Solidity event.
        topic0: Option<B256>,
    },
}

/// The account's activity on `page`, newest first: by block, then position in the block, then
/// log index.
pub fn decode(page: &Page) -> Vec<Activity> {
    let mut found = Vec::new();
    // Balance deltas need the balance before each touch, so blocks are walked oldest first
    // and the result is ordered afterwards.
    let mut balance = page.opening_balance.map(|(_, balance)| balance);
    for facts in page.blocks.iter().rev() {
        decode_block(page.account, facts, &mut balance, &mut found);
    }
    // Stable, so the items of one ERC-1155 batch keep their order.
    found.sort_by_key(|found| Reverse(found.key));
    found.into_iter().map(|found| found.activity).collect()
}

/// An activity with its order in the page: block number, block access index, then a rank
/// within the position (log index + 1 or withdrawal index + 1, 0 for the rest).
struct Found {
    key: (u64, u64, u64),
    activity: Activity,
}

const GWEI: u64 = 1_000_000_000;

fn decode_block(
    account: Address,
    facts: &BlockFacts,
    balance: &mut Option<U256>,
    found: &mut Vec<Found>,
) {
    let block = &facts.block;
    let mut push = |at: At, rank: u64, tx: Option<TxHash>, effect: Effect| {
        found.push(Found {
            key: (block.number, at.index(block.tx_count), rank),
            activity: Activity {
                number: block.number,
                timestamp: block.timestamp,
                at,
                tx,
                effect,
            },
        });
    };

    let mut touches: Vec<&Touch> = facts
        .touches
        .iter()
        .filter(|touch| touch.account == account)
        .collect();
    touches.sort_by_key(|touch| touch.at.index(block.tx_count));
    let withdrawn = facts.withdrawals.iter().any(|w| w.address == account);
    for touch in touches {
        let before = *balance;
        let effect = match touch.at {
            At::Tx(n) if facts.txs.iter().any(|tx| tx.index == n) => None,
            At::Tx(_) => Some(Effect::FeeCredit {
                amount: touch
                    .balance
                    .zip(before)
                    .and_then(|(after, before)| after.checked_sub(before)),
            }),
            At::Post if withdrawn => None,
            At::System | At::Post => touch
                .balance
                .map(|after| Effect::BalanceChange { before, after }),
        };
        if let Some(effect) = effect {
            push(touch.at, 0, None, effect);
        }
        if touch.balance.is_some() {
            *balance = touch.balance;
        }
    }

    for withdrawal in facts.withdrawals.iter().filter(|w| w.address == account) {
        push(
            At::Post,
            withdrawal.index.saturating_add(1),
            None,
            Effect::Withdrawal {
                validator: withdrawal.validator,
                amount: U256::from(withdrawal.amount_gwei) * U256::from(GWEI),
            },
        );
    }

    for tx in &facts.txs {
        let at = At::Tx(tx.index);
        let decoded = tx
            .raw
            .as_ref()
            .and_then(|raw| wire::Transaction::decode_2718_exact(raw).ok());
        let logs = tx.first_log_index..tx.first_log_index.saturating_add(tx.log_count);
        let mut named = false;
        // Other stored logs of the transaction are context for another account or for the
        // transaction as a whole, not something that happened to this one.
        for row in facts
            .logs
            .iter()
            .filter(|row| logs.contains(&row.index) && names(&row.log, account))
        {
            named = true;
            for effect in log_effects(&row.log) {
                push(at, row.index.saturating_add(1), Some(tx.hash), effect);
            }
        }
        let to = decoded.as_ref().and_then(|decoded| decoded.to());
        if tx.sender == account {
            let effect = Effect::Sent {
                to,
                nonce: decoded.as_ref().map(|decoded| decoded.nonce()),
                success: tx.success,
                fee: decoded
                    .as_ref()
                    .and_then(|decoded| fee(decoded, tx.gas_used, block.base_fee)),
            };
            push(at, 0, Some(tx.hash), effect);
        } else if !named && to == Some(account) {
            push(at, 0, Some(tx.hash), Effect::Called { from: tx.sender });
        }
    }
}

/// Gas used times the effective gas price. A dynamic-fee price needs the block's base fee.
fn fee(tx: &wire::Transaction, gas_used: u64, base_fee: Option<u64>) -> Option<U256> {
    if tx.is_dynamic_fee() && base_fee.is_none() {
        return None;
    }
    Some(U256::from(gas_used) * U256::from(tx.effective_gas_price(base_fee)))
}

/// Whether `log` names `account` as its emitter or in an indexed topic. Mentions inside the
/// data are not names (spec decision 12).
fn names(log: &Log, account: Address) -> bool {
    log.address == account
        || log
            .topics()
            .iter()
            .skip(1)
            .any(|topic| *topic == account.into_word())
}

/// What a log naming the account did: one effect, one per item of an ERC-1155 batch, or
/// [`Effect::Other`] when the log fits no known shape.
fn log_effects(log: &Log) -> Vec<Effect> {
    known(log).unwrap_or_else(|| {
        vec![Effect::Other {
            emitter: log.address,
            topic0: log.topics().first().copied(),
        }]
    })
}

fn known(log: &Log) -> Option<Vec<Effect>> {
    let token = log.address;
    let data = log.data.data.as_ref();
    let (&topic0, rest) = log.topics().split_first()?;
    let effect = match rest {
        [from, to] if topic0 == TRANSFER && data.len() == 32 => {
            let (from, to, amount) = (address(from)?, address(to)?, U256::from_be_slice(data));
            if token == ETH_LOGS {
                Effect::Eth { from, to, amount }
            } else {
                Effect::Token {
                    token,
                    from,
                    to,
                    amount,
                }
            }
        }
        [from, to, id] if topic0 == TRANSFER && data.is_empty() => Effect::Nft {
            token,
            from: address(from)?,
            to: address(to)?,
            id: U256::from_be_bytes(id.0),
        },
        [owner, spender] if topic0 == APPROVAL && data.len() == 32 => Effect::Approval {
            token,
            owner: address(owner)?,
            spender: address(spender)?,
            amount: U256::from_be_slice(data),
        },
        [owner, approved, id] if topic0 == APPROVAL && data.is_empty() => Effect::NftApproval {
            token,
            owner: address(owner)?,
            approved: address(approved)?,
            id: U256::from_be_bytes(id.0),
        },
        [owner, operator] if topic0 == APPROVAL_FOR_ALL && data.len() == 32 => {
            let flag = U256::from_be_slice(data);
            if flag > U256::from(1u64) {
                return None;
            }
            Effect::ApprovalForAll {
                token,
                owner: address(owner)?,
                operator: address(operator)?,
                approved: flag == U256::from(1u64),
            }
        }
        [operator, from, to] if topic0 == TRANSFER_SINGLE && data.len() == 64 => {
            let (id, amount) = data.split_at(32);
            Effect::Multi {
                token,
                operator: address(operator)?,
                from: address(from)?,
                to: address(to)?,
                id: U256::from_be_slice(id),
                amount: U256::from_be_slice(amount),
            }
        }
        [operator, from, to] if topic0 == TRANSFER_BATCH => {
            let (operator, from, to) = (address(operator)?, address(from)?, address(to)?);
            return Some(
                batch(data)?
                    .map(|(id, amount)| Effect::Multi {
                        token,
                        operator,
                        from,
                        to,
                        id,
                        amount,
                    })
                    .collect(),
            );
        }
        _ => return None,
    };
    Some(vec![effect])
}

/// The address in an indexed address topic, `None` when its 12 padding bytes are not zero.
fn address(topic: &B256) -> Option<Address> {
    let (padding, address) = topic.split_at(12);
    padding
        .iter()
        .all(|&byte| byte == 0)
        .then(|| Address::from_slice(address))
}

/// The `(id, amount)` pairs of ABI-encoded `(uint256[] ids, uint256[] values)`, `None` when the
/// encoding is malformed or the arrays differ in length.
fn batch(data: &[u8]) -> Option<impl Iterator<Item = (U256, U256)> + '_> {
    let ids = array(data, 0)?;
    let amounts = array(data, 32)?;
    (ids.len() == amounts.len()).then(|| ids.zip(amounts))
}

/// The `uint256[]` whose offset is the head word at byte `head` of ABI-encoded `data`.
fn array(data: &[u8], head: usize) -> Option<impl ExactSizeIterator<Item = U256> + '_> {
    let offset = word(data, head)?;
    if offset % 32 != 0 {
        return None;
    }
    let len = word(data, offset)?;
    let start = offset.checked_add(32)?;
    let body = data.get(start..start.checked_add(len.checked_mul(32)?)?)?;
    Some(
        body.as_chunks::<32>()
            .0
            .iter()
            .map(|w| U256::from_be_bytes(*w)),
    )
}

/// The 32-byte word at byte `at` of `data` as a size, `None` when it is out of bounds or too
/// large to be one.
fn word(data: &[u8], at: usize) -> Option<usize> {
    let word = data.get(at..at.checked_add(32)?)?;
    usize::try_from(U256::from_be_slice(word)).ok()
}

/// A run of nonces with no stored send, though sends on both sides of it are stored.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Missing {
    /// The missing nonces, inclusive.
    pub nonces: (u64, u64),
    /// The blocks of the stored sends just below and just above the run, so the missing sends
    /// lie between them.
    pub between: (u64, u64),
}

/// What the stored sends prove about the account's nonces: which sends are missing from
/// history, and where.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SendCheck {
    /// The nonces of the stored sends, ascending.
    pub nonces: Vec<u64>,
    /// Runs of nonces missing between stored sends.
    pub missing: Vec<Missing>,
    /// The lowest stored nonce. Sends below it are unknown: they may predate history.
    pub earliest: Option<u64>,
    /// The newest proven open state as `(block number, nonce)`.
    pub open_nonce: Option<(u64, u64)>,
    /// The nonces from the highest stored one + 1 to the open nonce - 1, when the newest open
    /// state lies at or above every stored send and they do not reach its nonce.
    pub top_gap: Option<(u64, u64)>,
}

/// Checks the account's stored sends against each other and against the newest open state.
/// Sends whose encoding is not stored carry no nonce and are skipped.
pub fn send_check(coverage: &Coverage) -> SendCheck {
    let mut sends: Vec<(u64, u64)> = coverage
        .sends
        .iter()
        .filter_map(|(number, tx)| {
            let raw = tx.raw.as_ref()?;
            let nonce = wire::Transaction::decode_2718_exact(raw).ok()?.nonce();
            Some((nonce, *number))
        })
        .collect();
    sends.sort_unstable();
    sends.dedup_by_key(|&mut (nonce, _)| nonce);
    let missing = sends
        .windows(2)
        .filter_map(|pair| match *pair {
            [(low, below), (high, above)] if high - low > 1 => Some(Missing {
                nonces: (low + 1, high - 1),
                between: (below, above),
            }),
            _ => None,
        })
        .collect();
    let open_nonce = coverage
        .opens
        .iter()
        .max_by_key(|open| open.number)
        .map(|open| (open.number, open.nonce));
    let newest_send = sends.iter().map(|&(_, number)| number).max();
    let top_gap = match (open_nonce, sends.last(), newest_send) {
        (Some((at, open)), Some(&(highest, _)), Some(newest))
            if at >= newest && highest.saturating_add(1) < open =>
        {
            Some((highest + 1, open - 1))
        }
        _ => None,
    };
    SendCheck {
        nonces: sends.iter().map(|&(nonce, _)| nonce).collect(),
        missing,
        earliest: sends.first().map(|&(nonce, _)| nonce),
        open_nonce,
        top_gap,
    }
}

#[cfg(test)]
mod tests {
    use alloy_consensus::{Signed, TxEip1559, TxLegacy};
    use alloy_eips::eip2718::Encodable2718 as _;
    use alloy_primitives::{Bytes, Log, Signature, TxKind, keccak256};

    use super::*;
    use crate::history::{BlockFacts, BlockRow, LogRow, OpenState, Touch, TxRow, WithdrawalRow};
    use crate::wire;

    const A: Address = address!("0x00000000000000000000000000000000000000aa");
    const B: Address = address!("0x00000000000000000000000000000000000000bb");
    const C: Address = address!("0x00000000000000000000000000000000000000cc");
    const TOKEN: Address = address!("0x0000000000000000000000000000000000007070");

    fn hash(n: u8) -> TxHash {
        B256::with_last_byte(n)
    }

    fn signature() -> Signature {
        Signature::new(U256::from(1u64), U256::from(1u64), false)
    }

    fn eip1559(nonce: u64, to: Address, max_fee: u128, priority: u128) -> Bytes {
        let tx = TxEip1559 {
            chain_id: 11_155_111,
            nonce,
            gas_limit: 21_000,
            max_fee_per_gas: max_fee,
            max_priority_fee_per_gas: priority,
            to: TxKind::Call(to),
            value: U256::from(5u64),
            ..TxEip1559::default()
        };
        wire::Transaction::Eip1559(Signed::new_unhashed(tx, signature()))
            .encoded_2718()
            .into()
    }

    fn legacy(nonce: u64, to: Address, gas_price: u128) -> Bytes {
        let tx = TxLegacy {
            chain_id: Some(11_155_111),
            nonce,
            gas_price,
            gas_limit: 21_000,
            to: TxKind::Call(to),
            value: U256::from(5u64),
            input: Bytes::new(),
        };
        wire::Transaction::Legacy(Signed::new_unhashed(tx, signature()))
            .encoded_2718()
            .into()
    }

    fn facts(number: u64, tx_count: u32, base_fee: Option<u64>) -> BlockFacts {
        BlockFacts {
            block: BlockRow {
                number,
                hash: B256::with_last_byte(0xb0),
                timestamp: 1_000 + number,
                beneficiary: C,
                base_fee,
                excess_blob_gas: None,
                tx_count,
            },
            txs: vec![],
            logs: vec![],
            touches: vec![],
            slots: vec![],
            withdrawals: vec![],
        }
    }

    fn tx(index: u32, sender: Address, raw: Option<Bytes>, success: bool) -> TxRow {
        TxRow {
            index,
            hash: hash(u8::try_from(index).unwrap() + 1),
            sender,
            raw,
            success,
            cumulative_gas: 21_000,
            gas_used: 21_000,
            first_log_index: 0,
            log_count: 0,
        }
    }

    fn touch(at: At, balance: Option<u64>) -> Touch {
        Touch {
            account: A,
            at,
            balance: balance.map(U256::from),
            nonce: None,
            code: None,
        }
    }

    fn log(index: u64, emitter: Address, topics: &[B256], data: Vec<u8>) -> LogRow {
        LogRow {
            index,
            log: Log::new_unchecked(emitter, topics.to_vec(), data.into()),
        }
    }

    fn word(n: u64) -> [u8; 32] {
        U256::from(n).to_be_bytes()
    }

    fn words(ns: &[u64]) -> Vec<u8> {
        ns.iter().flat_map(|&n| word(n)).collect()
    }

    fn page(blocks: Vec<BlockFacts>, opening_balance: Option<(u64, U256)>) -> Page {
        Page {
            account: A,
            blocks,
            opening_balance,
            next: None,
        }
    }

    fn activity(number: u64, at: At, tx: Option<TxHash>, effect: Effect) -> Activity {
        Activity {
            number,
            timestamp: 1_000 + number,
            at,
            tx,
            effect,
        }
    }

    #[test]
    fn event_topics_are_the_hashes_of_their_signatures() {
        assert_eq!(TRANSFER, keccak256("Transfer(address,address,uint256)"));
        assert_eq!(APPROVAL, keccak256("Approval(address,address,uint256)"));
        assert_eq!(
            APPROVAL_FOR_ALL,
            keccak256("ApprovalForAll(address,address,bool)")
        );
        assert_eq!(
            TRANSFER_SINGLE,
            keccak256("TransferSingle(address,address,address,uint256,uint256)")
        );
        assert_eq!(
            TRANSFER_BATCH,
            keccak256("TransferBatch(address,address,address,uint256[],uint256[])")
        );
    }

    #[test]
    fn a_self_send_is_one_send_with_its_fee_and_one_eth_move() {
        let mut block = facts(100, 1, Some(7));
        block.txs = vec![TxRow {
            log_count: 1,
            ..tx(0, A, Some(eip1559(3, A, 20, 2)), true)
        }];
        block.logs = vec![log(
            0,
            ETH_LOGS,
            &[TRANSFER, A.into_word(), A.into_word()],
            word(5).to_vec(),
        )];
        block.touches = vec![touch(At::Tx(0), Some(811_000))];

        assert_eq!(
            decode(&page(vec![block], Some((99, U256::from(1_000_000u64))))),
            [
                activity(
                    100,
                    At::Tx(0),
                    Some(hash(1)),
                    Effect::Eth {
                        from: A,
                        to: A,
                        amount: U256::from(5u64),
                    },
                ),
                activity(
                    100,
                    At::Tx(0),
                    Some(hash(1)),
                    Effect::Sent {
                        to: Some(A),
                        nonce: Some(3),
                        success: true,
                        fee: Some(U256::from(189_000u64)),
                    },
                ),
            ]
        );
    }

    #[test]
    fn incoming_token_and_nft_transfers_show_and_an_unrelated_log_does_not() {
        let mut block = facts(100, 1, Some(7));
        block.txs = vec![TxRow {
            log_count: 3,
            ..tx(0, B, None, true)
        }];
        block.logs = vec![
            log(
                0,
                TOKEN,
                &[TRANSFER, B.into_word(), C.into_word()],
                word(1).to_vec(),
            ),
            log(
                1,
                TOKEN,
                &[TRANSFER, B.into_word(), A.into_word()],
                word(50).to_vec(),
            ),
            log(
                2,
                C,
                &[
                    TRANSFER,
                    B.into_word(),
                    A.into_word(),
                    B256::from(U256::from(7u64)),
                ],
                vec![],
            ),
        ];
        block.touches = vec![touch(At::Tx(0), None)];

        assert_eq!(
            decode(&page(vec![block], None)),
            [
                activity(
                    100,
                    At::Tx(0),
                    Some(hash(1)),
                    Effect::Nft {
                        token: C,
                        from: B,
                        to: A,
                        id: U256::from(7u64),
                    },
                ),
                activity(
                    100,
                    At::Tx(0),
                    Some(hash(1)),
                    Effect::Token {
                        token: TOKEN,
                        from: B,
                        to: A,
                        amount: U256::from(50u64),
                    },
                ),
            ]
        );
    }

    #[test]
    fn an_erc1155_batch_is_one_transfer_per_item_and_a_malformed_one_is_other() {
        let topics = [TRANSFER_BATCH, B.into_word(), B.into_word(), A.into_word()];
        let mut block = facts(100, 2, Some(7));
        block.txs = vec![
            TxRow {
                log_count: 1,
                ..tx(0, B, None, true)
            },
            TxRow {
                first_log_index: 1,
                log_count: 1,
                ..tx(1, B, None, true)
            },
        ];
        block.logs = vec![
            log(0, TOKEN, &topics, words(&[64, 160, 2, 11, 12, 2, 100, 200])),
            log(1, TOKEN, &topics, words(&[64, 160, 2, 11, 12, 2, 100])),
        ];
        block.touches = vec![touch(At::Tx(0), None), touch(At::Tx(1), None)];
        let multi = |id: u64, amount: u64| {
            activity(
                100,
                At::Tx(0),
                Some(hash(1)),
                Effect::Multi {
                    token: TOKEN,
                    operator: B,
                    from: B,
                    to: A,
                    id: U256::from(id),
                    amount: U256::from(amount),
                },
            )
        };

        assert_eq!(
            decode(&page(vec![block], None)),
            [
                activity(
                    100,
                    At::Tx(1),
                    Some(hash(2)),
                    Effect::Other {
                        emitter: TOKEN,
                        topic0: Some(TRANSFER_BATCH),
                    },
                ),
                multi(11, 100),
                multi(12, 200),
            ]
        );
    }

    #[test]
    fn a_failed_send_is_a_failed_send_and_nothing_else() {
        let mut block = facts(100, 1, None);
        block.txs = vec![tx(0, A, Some(legacy(2, B, 3)), false)];
        block.touches = vec![touch(At::Tx(0), Some(937_000))];

        assert_eq!(
            decode(&page(vec![block], Some((99, U256::from(1_000_000u64))))),
            [activity(
                100,
                At::Tx(0),
                Some(hash(1)),
                Effect::Sent {
                    to: Some(B),
                    nonce: Some(2),
                    success: false,
                    fee: Some(U256::from(63_000u64)),
                },
            )]
        );
    }

    #[test]
    fn withdrawals_fee_credits_and_unexplained_balances_each_show() {
        let mut older = facts(100, 2, Some(7));
        older.touches = vec![
            touch(At::Post, Some(2_000_001_300)),
            touch(At::Tx(1), Some(1_300)),
        ];
        older.withdrawals = vec![
            WithdrawalRow {
                index: 9,
                validator: 42,
                address: A,
                amount_gwei: 2,
            },
            WithdrawalRow {
                index: 10,
                validator: 43,
                address: B,
                amount_gwei: 5,
            },
        ];
        let mut newer = facts(101, 0, Some(7));
        newer.touches = vec![touch(At::System, Some(4)), touch(At::Post, Some(5))];

        assert_eq!(
            decode(&page(vec![newer, older], Some((90, U256::from(1_000u64))))),
            [
                activity(
                    101,
                    At::Post,
                    None,
                    Effect::BalanceChange {
                        before: Some(U256::from(4u64)),
                        after: U256::from(5u64),
                    },
                ),
                activity(
                    101,
                    At::System,
                    None,
                    Effect::BalanceChange {
                        before: Some(U256::from(2_000_001_300u64)),
                        after: U256::from(4u64),
                    },
                ),
                activity(
                    100,
                    At::Post,
                    None,
                    Effect::Withdrawal {
                        validator: 42,
                        amount: U256::from(2_000_000_000u64),
                    },
                ),
                activity(
                    100,
                    At::Tx(1),
                    None,
                    Effect::FeeCredit {
                        amount: Some(U256::from(300u64)),
                    },
                ),
            ]
        );
    }

    #[test]
    fn token_nft_and_operator_approvals_each_keep_their_own_shape() {
        let mut block = facts(100, 1, Some(7));
        block.txs = vec![TxRow {
            log_count: 4,
            ..tx(0, A, None, true)
        }];
        block.logs = vec![
            log(
                0,
                TOKEN,
                &[APPROVAL, A.into_word(), B.into_word()],
                word(9).to_vec(),
            ),
            log(
                1,
                C,
                &[
                    APPROVAL,
                    A.into_word(),
                    B.into_word(),
                    B256::from(U256::from(7u64)),
                ],
                vec![],
            ),
            log(
                2,
                C,
                &[APPROVAL_FOR_ALL, A.into_word(), B.into_word()],
                word(1).to_vec(),
            ),
            log(
                3,
                C,
                &[APPROVAL_FOR_ALL, A.into_word(), B.into_word()],
                word(2).to_vec(),
            ),
        ];
        block.touches = vec![touch(At::Tx(0), None)];
        let at = |effect| activity(100, At::Tx(0), Some(hash(1)), effect);

        assert_eq!(
            decode(&page(vec![block], None)),
            [
                at(Effect::Other {
                    emitter: C,
                    topic0: Some(APPROVAL_FOR_ALL),
                }),
                at(Effect::ApprovalForAll {
                    token: C,
                    owner: A,
                    operator: B,
                    approved: true,
                }),
                at(Effect::NftApproval {
                    token: C,
                    owner: A,
                    approved: B,
                    id: U256::from(7u64),
                }),
                at(Effect::Approval {
                    token: TOKEN,
                    owner: A,
                    spender: B,
                    amount: U256::from(9u64),
                }),
                at(Effect::Sent {
                    to: None,
                    nonce: None,
                    success: true,
                    fee: None,
                }),
            ]
        );
    }

    #[test]
    fn a_call_from_someone_else_that_did_nothing_else_shows_who_called() {
        let mut block = facts(100, 1, Some(7));
        block.txs = vec![tx(0, B, Some(legacy(0, A, 3)), true)];
        block.touches = vec![touch(At::Tx(0), None)];

        assert_eq!(
            decode(&page(vec![block], None)),
            [activity(
                100,
                At::Tx(0),
                Some(hash(1)),
                Effect::Called { from: B },
            )]
        );
    }

    fn coverage(sends: &[(u64, Option<u64>)], opens: &[(u64, u64)]) -> Coverage {
        Coverage {
            account: A,
            ranges: vec![],
            settled: None,
            opens: opens
                .iter()
                .map(|&(number, nonce)| OpenState {
                    account: A,
                    number,
                    hash: B256::with_last_byte(0xb0),
                    balance: U256::ZERO,
                    nonce,
                    slots: vec![],
                })
                .collect(),
            sends: sends
                .iter()
                .map(|&(number, nonce)| (number, tx(0, A, nonce.map(|n| legacy(n, B, 3)), true)))
                .collect(),
        }
    }

    #[test]
    fn a_nonce_gap_between_sends_and_below_the_open_nonce_are_both_missing() {
        let check = send_check(&coverage(
            &[(10, Some(4)), (11, Some(5)), (13, None), (15, Some(7))],
            &[(3, 2), (20, 9)],
        ));

        assert_eq!(
            check,
            SendCheck {
                nonces: vec![4, 5, 7],
                missing: vec![Missing {
                    nonces: (6, 6),
                    between: (11, 15),
                }],
                earliest: Some(4),
                open_nonce: Some((20, 9)),
                top_gap: Some((8, 8)),
            }
        );
    }

    #[test]
    fn contiguous_sends_up_to_the_open_nonce_have_no_gaps() {
        let check = send_check(&coverage(&[(10, Some(4)), (11, Some(5))], &[(20, 6)]));

        assert_eq!(
            check,
            SendCheck {
                nonces: vec![4, 5],
                missing: vec![],
                earliest: Some(4),
                open_nonce: Some((20, 6)),
                top_gap: None,
            }
        );
    }
}
