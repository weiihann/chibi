//! Account history as text: one line per activity and one coverage line per account for
//! `chibi status`, and one table row with its details per transaction for the wallet. The
//! formatting is pure, so both show history in the same words.

use std::fmt::{self, Write as _};
use std::time::{SystemTime, UNIX_EPOCH};

use alloy_primitives::{Address, TxHash, U256};
use chibi_core::history::{Activity, At, Capability, Coverage, Effect, SendCheck, TopEnd};
use chibi_node::{Explorer, Token};

/// What the coverage line says for an account history holds nothing for.
pub const NO_HISTORY: &str = "no history yet";

/// Decimal places of one ether.
pub const ETH_DECIMALS: u8 = 18;

/// The current Unix time in seconds, 0 if the clock is before 1970.
pub fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs())
}

/// `value` base units as a decimal with `decimals` places, trailing zeros trimmed to two.
pub fn format_units(value: U256, decimals: u8) -> String {
    let digits = value.to_string();
    let decimals = usize::from(decimals);
    if decimals == 0 {
        return digits;
    }
    let padded = if digits.len() <= decimals {
        format!("{}{digits}", "0".repeat(decimals + 1 - digits.len()))
    } else {
        digits
    };
    let (whole, fraction) = padded.split_at(padded.len() - decimals);
    let keep = fraction.trim_end_matches('0').len().max(decimals.min(2));
    format!("{whole}.{}", &fraction[..keep])
}

/// A contract's address as its first four and last four hex digits, enough to tell tokens
/// apart at a glance. Counterparties are never shortened: a lookalike shares these digits.
pub fn short(address: Address) -> String {
    let full = address.to_string();
    format!("{}…{}", &full[..6], &full[full.len() - 4..])
}

/// `n` with a comma between each group of three digits, as block numbers are read.
pub fn grouped(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, digit) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(digit);
    }
    out
}

/// How long ago `seconds` was, in its largest whole unit.
pub fn ago(seconds: u64) -> String {
    match seconds {
        0..60 => format!("{seconds} s ago"),
        60..3_600 => format!("{} min ago", seconds / 60),
        3_600..86_400 => format!("{} h ago", seconds / 3_600),
        86_400.. => format!("{} d ago", seconds / 86_400),
    }
}

/// One activity of `account` as a line: its block, its age at `now` (Unix seconds) and what
/// happened. Tokens in `registry` show their symbol and decimals; others show base units and
/// the contract.
pub fn activity_line(
    activity: &Activity,
    account: Address,
    registry: &[Token],
    now: u64,
) -> String {
    format!(
        "{}  {}  {}",
        grouped(activity.number),
        ago(now.saturating_sub(activity.timestamp)),
        effect_text(activity, account, registry, short)
    )
}

fn eth(amount: U256) -> String {
    format!("{} ETH", format_units(amount, ETH_DECIMALS))
}

/// `amount` of the ERC-20 `token`, with its symbol when the registry knows it, else with its
/// address as `contract` writes it.
fn token_amount(
    amount: U256,
    token: Address,
    registry: &[Token],
    contract: fn(Address) -> String,
) -> String {
    match registry.iter().find(|known| known.address == token) {
        Some(known) => format!("{} {}", format_units(amount, known.decimals), known.symbol),
        None => format!("{amount} of {}", contract(token)),
    }
}

/// `what` moving between `from` and `to`, signed from `account`'s side.
fn movement(what: &str, from: Address, to: Address, account: Address) -> String {
    match (from == account, to == account) {
        (true, true) => format!("{what} to self"),
        (false, true) => format!("+{what} from {from}"),
        (true, false) => format!("-{what} to {to}"),
        (false, false) => format!("{what} from {from} to {to}"),
    }
}

/// What `activity` did, from `account`'s side, with each token and event contract address
/// as `contract` writes it.
fn effect_text(
    activity: &Activity,
    account: Address,
    registry: &[Token],
    contract: fn(Address) -> String,
) -> String {
    match &activity.effect {
        Effect::Sent {
            to,
            nonce,
            success,
            fee,
            value,
        } => {
            let mut text = String::from(if *success { "sent" } else { "failed send" });
            if let Some(value) = value {
                let _ = write!(text, " {}", eth(*value));
            }
            match (to, nonce) {
                (Some(to), _) if *to == account => text.push_str(" to self"),
                (Some(to), _) => {
                    let _ = write!(text, " to {to}");
                }
                (None, Some(_)) => text.push_str(" creating a contract"),
                (None, None) => {}
            }
            let details: Vec<String> = nonce
                .map(|nonce| format!("nonce {nonce}"))
                .into_iter()
                .chain(fee.map(|fee| format!("fee {}", eth(fee))))
                .collect();
            if !details.is_empty() {
                let _ = write!(text, " ({})", details.join(", "));
            }
            text
        }
        Effect::Eth { from, to, amount } => movement(&eth(*amount), *from, *to, account),
        Effect::Token {
            token,
            from,
            to,
            amount,
        } => {
            let text = movement(
                &token_amount(*amount, *token, registry, contract),
                *from,
                *to,
                account,
            );
            if amount.is_zero() && *from == account && *to != account && !activity.sent {
                format!("{text} (zero transfer you did not send; do not copy this address)")
            } else {
                text
            }
        }
        Effect::Nft {
            token,
            from,
            to,
            id,
        } => movement(
            &format!("NFT #{id} of {}", contract(*token)),
            *from,
            *to,
            account,
        ),
        Effect::Multi {
            token,
            from,
            to,
            id,
            amount,
            ..
        } => movement(
            &format!("{amount} of #{id} of {}", contract(*token)),
            *from,
            *to,
            account,
        ),
        Effect::Approval {
            token,
            owner,
            spender,
            amount,
        } => {
            let allowance = if *amount == U256::MAX {
                match registry.iter().find(|known| known.address == *token) {
                    Some(known) => format!("unlimited {}", known.symbol),
                    None => format!("unlimited of {}", contract(*token)),
                }
            } else {
                token_amount(*amount, *token, registry, contract)
            };
            approval(*owner, account, &format!("{spender} for {allowance}"))
        }
        Effect::NftApproval {
            token,
            owner,
            approved,
            id,
        } => approval(
            *owner,
            account,
            &format!("{approved} for NFT #{id} of {}", contract(*token)),
        ),
        Effect::ApprovalForAll {
            token,
            owner,
            operator,
            approved,
        } => {
            let verb = if *approved { "approved" } else { "revoked" };
            let what = format!("{operator} for all of {}", contract(*token));
            if *owner == account {
                format!("{verb} {what}")
            } else {
                format!("{verb} {what} by {owner}")
            }
        }
        Effect::Withdrawal { validator, amount } => {
            format!("+{} withdrawal (validator {validator})", eth(*amount))
        }
        Effect::FeeCredit {
            amount: Some(amount),
        } => format!("+{} fee credit", eth(*amount)),
        Effect::FeeCredit { amount: None } => "fee credit".to_owned(),
        Effect::Called { from } => format!("called by {from}"),
        Effect::BalanceChange {
            before: Some(before),
            after,
        } => format!(
            "balance {} -> {} ETH",
            format_units(*before, ETH_DECIMALS),
            format_units(*after, ETH_DECIMALS)
        ),
        Effect::BalanceChange {
            before: None,
            after,
        } => format!("balance now {}", eth(*after)),
        Effect::Delegated { to, confirmed } => {
            let text = match to {
                Some(to) => format!("delegated to {to}"),
                None => "delegation cleared".to_owned(),
            };
            if *confirmed {
                text
            } else {
                format!("{text} (not confirmed: no state change recorded for this block)")
            }
        }
        Effect::Other { emitter, .. } => format!("event from {}", contract(*emitter)),
    }
}

/// An approval `what` from `owner`, worded from `account`'s side.
fn approval(owner: Address, account: Address, what: &str) -> String {
    if owner == account {
        format!("approved {what}")
    } else {
        format!("approved {what} by {owner}")
    }
}

/// The widest an amount cell gets. The detail screen shows every amount exactly.
pub const AMOUNT_COLUMNS: usize = 24;

/// The counterparty cell of a row with a zero transfer the account did not send. Its address
/// is an attacker's lookalike, and a shortened one shows just the part a lookalike copies; the
/// detail screen names it in full with the warning.
const HIDDEN: &str = "⚠ hidden";

/// What the last line of an entry's details says when the read stopped inside it.
const MORE_EFFECTS: &str = "more effects than chibi loaded";

/// Who the account a history is about is to the person reading it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Owner {
    /// The unlocked wallet's own account.
    You,
    /// An address the session only watches.
    Watched,
}

impl Owner {
    /// [`Owner::You`] when `account` is the unlocked `sender`, else [`Owner::Watched`].
    pub fn of(account: Address, sender: Option<Address>) -> Self {
        if sender == Some(account) {
            Self::You
        } else {
            Self::Watched
        }
    }

    const fn label(self) -> &'static str {
        match self {
            Self::You => "you",
            Self::Watched => "watched",
        }
    }
}

/// How a history is read: whose it is, who that is to the reader, the tokens known by name,
/// the time ages count to (Unix seconds), and the explorer transactions link to.
#[derive(Clone, Copy, Debug)]
pub struct Reader<'a> {
    /// The account the history is about.
    pub account: Address,
    /// Who the account is to the reader.
    pub owner: Owner,
    /// The network's registry tokens.
    pub registry: &'a [Token],
    /// Now, in Unix seconds.
    pub now: u64,
    /// The network's block explorer.
    pub explorer: Explorer,
}

/// Which entry of a history: its block, its place in the block, and its order among the
/// entries at that place, since a block's withdrawals share the post-block place.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EntryKey {
    /// Block number.
    pub number: u64,
    /// Where in the block.
    pub at: At,
    /// How many entries before it share the block and the place.
    pub nth: usize,
}

/// An entry's row in the history table, one field per column.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Cells {
    /// The shortened transaction hash, or what the entry is when no transaction made it.
    pub tx: String,
    /// The block number, grouped.
    pub block: String,
    /// How long ago, in one compact unit.
    pub age: String,
    /// Where the value came from.
    pub from: String,
    /// Where the value went.
    pub to: String,
    /// What moved, signed from the account's side, or what the transaction did.
    pub amount: String,
    /// The fee the account paid, in ETH rounded down to 6 decimals.
    pub fee: String,
}

/// One line of an entry's detail screen.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DetailLine {
    /// A labelled value, never shortened.
    Field {
        /// What the value is.
        label: &'static str,
        /// The value.
        value: String,
    },
    /// A blank line between the parts.
    Blank,
    /// A whole line: an effect, a note, or the explorer link.
    Text(String),
}

impl fmt::Display for DetailLine {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Field { label, value } => write!(f, "{label:<11}  {value}"),
            Self::Blank => Ok(()),
            Self::Text(text) => f.write_str(text),
        }
    }
}

/// One transaction of an account's history, or one withdrawal, fee credit or balance change,
/// as the wallet shows it: a table row and a detail screen.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    /// Which entry it is, so a selection survives a refresh.
    pub key: EntryKey,
    /// Its table row.
    pub cells: Cells,
    /// Whether it holds a zero transfer out the account did not send: address-poisoning bait.
    pub warning: bool,
    /// Its detail screen.
    pub detail: Vec<DetailLine>,
}

/// `activities`, newest first, as entries: one per transaction, and one per activity outside
/// any transaction. `full` says the read returned every activity it asked for, so the oldest
/// entry may lack activities past the cut: it is dropped, or marked when it is the only one.
pub fn entries(activities: &[Activity], full: bool, reader: &Reader<'_>) -> Vec<Entry> {
    let mut groups: Vec<&[Activity]> = activities
        .chunk_by(|a, b| a.number == b.number && a.at == b.at && matches!(a.at, At::Tx(_)))
        .collect();
    let cut = full && groups.len() == 1;
    if full && groups.len() > 1 {
        groups.pop();
    }
    let mut entries: Vec<Entry> = Vec::with_capacity(groups.len());
    for group in groups {
        let first = &group[0];
        let nth = entries
            .last()
            .filter(|last| last.key.number == first.number && last.key.at == first.at)
            .map_or(0, |last| last.key.nth + 1);
        let key = EntryKey {
            number: first.number,
            at: first.at,
            nth,
        };
        entries.push(entry(group, key, cut, reader));
    }
    entries
}

/// A value moved to or from the account, as its table cells show it.
struct Movement {
    amount: String,
    from: String,
    to: String,
    flagged: bool,
}

/// The send in an entry: the fields of its [`Effect::Sent`].
struct Send {
    to: Option<Address>,
    nonce: Option<u64>,
    success: bool,
    fee: Option<U256>,
    value: Option<U256>,
}

fn entry(group: &[Activity], key: EntryKey, cut: bool, reader: &Reader<'_>) -> Entry {
    let first = &group[0];
    let hash = group.iter().find_map(|activity| activity.tx);
    let send = group.iter().find_map(|activity| match activity.effect {
        Effect::Sent {
            to,
            nonce,
            success,
            fee,
            value,
        } => Some(Send {
            to,
            nonce,
            success,
            fee,
            value,
        }),
        Effect::Eth { .. }
        | Effect::Token { .. }
        | Effect::Nft { .. }
        | Effect::Multi { .. }
        | Effect::Approval { .. }
        | Effect::NftApproval { .. }
        | Effect::ApprovalForAll { .. }
        | Effect::Withdrawal { .. }
        | Effect::FeeCredit { .. }
        | Effect::Called { .. }
        | Effect::BalanceChange { .. }
        | Effect::Delegated { .. }
        | Effect::Other { .. } => None,
    });
    let caller = group.iter().find_map(|activity| match activity.effect {
        Effect::Called { from } => Some(from),
        Effect::Sent { .. }
        | Effect::Eth { .. }
        | Effect::Token { .. }
        | Effect::Nft { .. }
        | Effect::Multi { .. }
        | Effect::Approval { .. }
        | Effect::NftApproval { .. }
        | Effect::ApprovalForAll { .. }
        | Effect::Withdrawal { .. }
        | Effect::FeeCredit { .. }
        | Effect::BalanceChange { .. }
        | Effect::Delegated { .. }
        | Effect::Other { .. } => None,
    });
    let mut movements: Vec<Movement> = group
        .iter()
        .filter_map(|activity| moved_value(activity, reader))
        .collect();
    let warning = movements.iter().any(|movement| movement.flagged);
    let you = reader.owner.label().to_owned();
    let party = |address: Address| {
        if address == reader.account {
            you.clone()
        } else {
            short(address)
        }
    };
    let sent_to = |send: &Send| match (send.to, send.nonce) {
        (Some(to), _) => party(to),
        (None, Some(_)) => "new contract".to_owned(),
        (None, None) => String::new(),
    };
    let (amount, from, to) = match movements.len() {
        1 => {
            let Movement {
                amount, from, to, ..
            } = movements.remove(0);
            (amount, from, to)
        }
        0 => {
            let (from, to) = match (&send, caller) {
                (Some(send), _) => (you.clone(), sent_to(send)),
                (None, Some(caller)) => (party(caller), you.clone()),
                (None, None) => (String::new(), String::new()),
            };
            (label(group, reader.registry), from, to)
        }
        several => {
            let listed = movements
                .iter()
                .map(|movement| movement.amount.as_str())
                .collect::<Vec<_>>()
                .join(", ");
            let amount = if listed.chars().count() <= AMOUNT_COLUMNS {
                listed
            } else if warning {
                format!("⚠ {several} transfers")
            } else {
                format!("{several} transfers")
            };
            let (from, to) = match &send {
                Some(_) if warning => (you.clone(), HIDDEN.to_owned()),
                Some(send) => (you.clone(), sent_to(send)),
                None if warning => (HIDDEN.to_owned(), you.clone()),
                None => ("several".to_owned(), you.clone()),
            };
            (amount, from, to)
        }
    };
    let cells = Cells {
        tx: hash.map_or_else(|| untraced(group).to_owned(), short_hash),
        block: grouped(first.number),
        age: age(reader.now.saturating_sub(first.timestamp)),
        from,
        to,
        amount,
        fee: send
            .as_ref()
            .and_then(|send| send.fee)
            .map(fee_cell)
            .unwrap_or_default(),
    };
    Entry {
        key,
        cells,
        warning,
        detail: detail(group, hash, send.as_ref(), caller, cut, reader),
    }
}

/// The value `activity` moved, `None` for an effect that moves none.
fn moved_value(activity: &Activity, reader: &Reader<'_>) -> Option<Movement> {
    let account = reader.account;
    let you = reader.owner.label();
    let party = |address: Address| {
        if address == account {
            you.to_owned()
        } else {
            short(address)
        }
    };
    let moved = |what: String, from: Address, to: Address| {
        let sign = match (from == account, to == account) {
            (true, false) => "-",
            (false, true) => "+",
            (true, true) | (false, false) => "",
        };
        Movement {
            amount: format!("{sign}{what}"),
            from: party(from),
            to: party(to),
            flagged: false,
        }
    };
    let credit = |amount: String, from: String| Movement {
        amount,
        from,
        to: you.to_owned(),
        flagged: false,
    };
    match &activity.effect {
        Effect::Eth { from, to, amount } => Some(moved(eth(*amount), *from, *to)),
        Effect::Token {
            token,
            from,
            to,
            amount,
        } => {
            let what = token_amount(*amount, *token, reader.registry, short);
            if amount.is_zero() && *from == account && *to != account && !activity.sent {
                Some(Movement {
                    amount: format!("⚠ {what}"),
                    from: party(*from),
                    to: HIDDEN.to_owned(),
                    flagged: true,
                })
            } else {
                Some(moved(what, *from, *to))
            }
        }
        Effect::Nft { from, to, id, .. } => Some(moved(format!("NFT #{id}"), *from, *to)),
        Effect::Multi {
            from,
            to,
            id,
            amount,
            ..
        } => Some(moved(format!("{amount} of #{id}"), *from, *to)),
        Effect::Withdrawal { validator, amount } => Some(credit(
            format!("+{}", eth(*amount)),
            format!("validator {validator}"),
        )),
        Effect::FeeCredit { amount } => Some(credit(
            amount
                .map(|amount| format!("+{}", eth(amount)))
                .unwrap_or_default(),
            "block fees".to_owned(),
        )),
        Effect::BalanceChange { after, .. } => Some(Movement {
            amount: format!("= {}", eth(*after)),
            from: String::new(),
            to: String::new(),
            flagged: false,
        }),
        Effect::Sent {
            to: Some(to),
            value: Some(value),
            ..
        } if *to == account => Some(moved(eth(*value), account, account)),
        Effect::Sent { .. }
        | Effect::Approval { .. }
        | Effect::NftApproval { .. }
        | Effect::ApprovalForAll { .. }
        | Effect::Called { .. }
        | Effect::Delegated { .. }
        | Effect::Other { .. } => None,
    }
}

/// What an entry that moved no value did, in a word or two: an approval first, then a
/// delegation, a call, and last an unknown event.
fn label(group: &[Activity], registry: &[Token]) -> String {
    group
        .iter()
        .filter_map(|activity| match &activity.effect {
            Effect::Approval { token, .. } | Effect::NftApproval { token, .. } => {
                Some((0, format!("approve {}", symbol(*token, registry))))
            }
            Effect::ApprovalForAll {
                token, approved, ..
            } => {
                let verb = if *approved { "approve" } else { "revoke" };
                Some((0, format!("{verb} {}", symbol(*token, registry))))
            }
            Effect::Delegated { to: Some(_), .. } => Some((1, "delegate".to_owned())),
            Effect::Delegated { to: None, .. } => Some((1, "undelegate".to_owned())),
            Effect::Sent { .. } | Effect::Called { .. } => Some((2, "call".to_owned())),
            Effect::Other { .. } => Some((3, "event".to_owned())),
            Effect::Eth { .. }
            | Effect::Token { .. }
            | Effect::Nft { .. }
            | Effect::Multi { .. }
            | Effect::Withdrawal { .. }
            | Effect::FeeCredit { .. }
            | Effect::BalanceChange { .. } => None,
        })
        .min_by_key(|(rank, _)| *rank)
        .map(|(_, label)| label)
        .unwrap_or_default()
}

/// What an entry no stored transaction made is, for its transaction cell.
fn untraced(group: &[Activity]) -> &'static str {
    group
        .iter()
        .find_map(|activity| match activity.effect {
            Effect::Withdrawal { .. } => Some("withdrawal"),
            Effect::FeeCredit { .. } => Some("fee credit"),
            Effect::BalanceChange { .. } => Some("balance"),
            Effect::Sent { .. }
            | Effect::Eth { .. }
            | Effect::Token { .. }
            | Effect::Nft { .. }
            | Effect::Multi { .. }
            | Effect::Approval { .. }
            | Effect::NftApproval { .. }
            | Effect::ApprovalForAll { .. }
            | Effect::Called { .. }
            | Effect::Delegated { .. }
            | Effect::Other { .. } => None,
        })
        .unwrap_or_default()
}

/// An entry's detail screen: the transaction, its block, what the account's send says about
/// it, every effect in full, and the explorer link.
fn detail(
    group: &[Activity],
    hash: Option<TxHash>,
    send: Option<&Send>,
    caller: Option<Address>,
    cut: bool,
    reader: &Reader<'_>,
) -> Vec<DetailLine> {
    let first = &group[0];
    let field = |label: &'static str, value: String| DetailLine::Field { label, value };
    let named = |address: Address| {
        if address == reader.account {
            format!("{address} ({})", reader.owner.label())
        } else {
            address.to_string()
        }
    };
    let mut lines = Vec::new();
    if let Some(hash) = hash {
        lines.push(field("transaction", hash.to_string()));
    }
    lines.push(field(
        "block",
        format!(
            "{} · {}",
            grouped(first.number),
            ago(reader.now.saturating_sub(first.timestamp))
        ),
    ));
    match (send, caller) {
        (Some(send), _) => {
            let status = if send.success { "success" } else { "failed" };
            lines.push(field("status", status.to_owned()));
            lines.push(field("from", named(reader.account)));
            match (send.to, send.nonce) {
                (Some(to), _) => lines.push(field("to", named(to))),
                (None, Some(_)) => lines.push(field("to", "new contract".to_owned())),
                (None, None) => {}
            }
            if let Some(value) = send.value {
                lines.push(field("value", eth(value)));
            }
            if let Some(nonce) = send.nonce {
                lines.push(field("nonce", nonce.to_string()));
            }
            if let Some(fee) = send.fee {
                lines.push(field("fee", eth(fee)));
            }
        }
        (None, Some(caller)) => lines.push(field("from", named(caller))),
        (None, None) => {}
    }
    let effects: Vec<DetailLine> = group
        .iter()
        .filter(|activity| !matches!(activity.effect, Effect::Sent { .. }))
        .map(|activity| {
            DetailLine::Text(effect_text(
                activity,
                reader.account,
                reader.registry,
                |address| address.to_string(),
            ))
        })
        .collect();
    if !effects.is_empty() || cut {
        lines.push(DetailLine::Blank);
    }
    lines.extend(effects);
    if cut {
        lines.push(DetailLine::Text(MORE_EFFECTS.to_owned()));
    }
    if let Some(hash) = hash {
        lines.push(DetailLine::Blank);
        lines.push(DetailLine::Text(reader.explorer.tx_url(hash)));
    }
    lines
}

/// A token's symbol when the registry knows it, else its shortened address.
fn symbol(token: Address, registry: &[Token]) -> String {
    registry
        .iter()
        .find(|known| known.address == token)
        .map_or_else(|| short(token), |known| known.symbol.to_owned())
}

/// A transaction hash as its first four and last four hex digits.
fn short_hash(hash: TxHash) -> String {
    let full = hash.to_string();
    format!("{}…{}", &full[..6], &full[full.len() - 4..])
}

/// How long ago `seconds` was, as a number and a one-letter unit.
fn age(seconds: u64) -> String {
    match seconds {
        0..60 => format!("{seconds}s"),
        60..3_600 => format!("{}m", seconds / 60),
        3_600..86_400 => format!("{}h", seconds / 3_600),
        86_400.. => format!("{}d", seconds / 86_400),
    }
}

/// A fee in wei as ETH rounded down to 6 decimals, without the unit: a nonzero fee that
/// rounds to nothing shows as below the smallest step.
fn fee_cell(fee: U256) -> String {
    let micro = fee / U256::from(1_000_000_000_000u64);
    if micro.is_zero() && !fee.is_zero() {
        return "<0.000001".to_owned();
    }
    format_units(micro, 6)
}

/// What history holds for an account, as one line: where its log coverage starts, the gaps
/// inside it, and whether every nonce since its earliest send is explained.
pub fn coverage_line(coverage: &Coverage, check: &SendCheck) -> String {
    let logs = coverage
        .ranges
        .iter()
        .find(|(capability, _)| *capability == Capability::Logs)
        .map(|(_, ranges)| ranges);
    let Some((ranges, first, last)) =
        logs.and_then(|ranges| Some((ranges, ranges.first()?, ranges.last()?)))
    else {
        return NO_HISTORY.to_owned();
    };
    let gaps: Vec<String> = ranges
        .gaps(first, last)
        .into_iter()
        .map(|(low, high)| span(low, high, grouped))
        .collect();
    let mut parts = vec![
        format!("history since block {}", grouped(first)),
        format!(
            "gaps: {}",
            if gaps.is_empty() {
                "none".to_owned()
            } else {
                gaps.join(", ")
            }
        ),
    ];
    match check.earliest {
        None => parts.push("no sends found".to_owned()),
        Some(earliest) => {
            let complete = matches!(
                check.top,
                TopEnd::Checked {
                    unexplained: None,
                    ..
                }
            );
            if check.unexplained.is_empty() && complete {
                parts.push(format!("sends from nonce {earliest} complete"));
            }
            for unexplained in &check.unexplained {
                let (low, high) = unexplained.nonces;
                let (below, above) = unexplained.between;
                parts.push(format!(
                    "{} unexplained between blocks {} and {}",
                    nonces(low, high),
                    grouped(below),
                    grouped(above)
                ));
            }
            match check.top {
                TopEnd::Checked {
                    unexplained: Some((low, high)),
                    ..
                } => parts.push(format!(
                    "{} unexplained after the newest send",
                    nonces(low, high)
                )),
                TopEnd::Checked {
                    unexplained: None, ..
                } => {}
                TopEnd::Unchecked => {
                    if let Some(highest) = check.nonces.last() {
                        parts.push(format!("nonces after {highest} unchecked"));
                    }
                }
            }
            if earliest > 0 {
                parts.push("earlier sends may predate the history".to_owned());
            }
        }
    }
    parts.join(" · ")
}

fn span(low: u64, high: u64, show: impl Fn(u64) -> String) -> String {
    if low == high {
        show(low)
    } else {
        format!("{}-{}", show(low), show(high))
    }
}

fn nonces(low: u64, high: u64) -> String {
    if low == high {
        format!("nonce {low}")
    } else {
        format!("nonces {low}-{high}")
    }
}

#[cfg(test)]
mod tests {
    use alloy_consensus::{Signed, TxLegacy};
    use alloy_eips::eip2718::Encodable2718 as _;
    use alloy_primitives::{B256, Bytes, Signature, TxKind, address};
    use chibi_core::history::{At, OpenState, Ranges, TxRow, send_check};
    use chibi_core::wire;

    use super::*;

    const ME: Address = address!("0x00000000000000000000000000000000000000aa");
    const PEER: Address = address!("0x1111000000000000000000000000000000002222");
    const USDC: Address = address!("0x3333000000000000000000000000000000004444");
    const ODD: Address = address!("0x7070000000000000000000000000000000007070");
    const NOW: u64 = 1_000_000;
    const REGISTRY: [Token; 1] = [Token {
        symbol: "USDC",
        name: "USD Coin",
        address: USDC,
        decimals: 6,
    }];

    fn line(effect: Effect) -> String {
        line_sent(effect, false)
    }

    fn line_sent(effect: Effect, sent: bool) -> String {
        let activity = Activity {
            number: 11_856_337,
            timestamp: NOW - 12 * 60,
            at: At::Tx(0),
            tx: None,
            sent,
            effect,
        };
        activity_line(&activity, ME, &REGISTRY, NOW)
    }

    const MIXED: Address = address!("0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48");

    #[test]
    fn a_token_transfer_shows_the_full_checksummed_counterparty() {
        let transfer = Effect::Token {
            token: USDC,
            from: ME,
            to: MIXED,
            amount: U256::from(2_000_000u64),
        };
        assert_eq!(
            line_sent(transfer, true),
            "11,856,337  12 min ago  -2.00 USDC to 0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48"
        );
    }

    #[test]
    fn a_zero_transfer_out_the_account_did_not_send_warns() {
        let zero = Effect::Token {
            token: USDC,
            from: ME,
            to: MIXED,
            amount: U256::ZERO,
        };
        assert_eq!(
            line_sent(zero.clone(), false),
            "11,856,337  12 min ago  -0.00 USDC to 0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48 \
             (zero transfer you did not send; do not copy this address)"
        );
        assert_eq!(
            line_sent(zero, true),
            "11,856,337  12 min ago  -0.00 USDC to 0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48"
        );
        let nonzero = Effect::Token {
            token: USDC,
            from: ME,
            to: MIXED,
            amount: U256::from(1u64),
        };
        assert_eq!(
            line_sent(nonzero, false),
            "11,856,337  12 min ago  -0.000001 USDC to 0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48"
        );
    }

    fn milli_eth(n: u64) -> U256 {
        U256::from(n) * U256::from(1_000_000_000_000_000u64)
    }

    #[test]
    fn each_effect_reads_as_one_line_from_the_accounts_side() {
        let cases = [
            (
                Effect::Token {
                    token: USDC,
                    from: PEER,
                    to: ME,
                    amount: U256::from(1_500_000u64),
                },
                "+1.50 USDC from 0x1111000000000000000000000000000000002222",
            ),
            (
                Effect::Token {
                    token: ODD,
                    from: ME,
                    to: PEER,
                    amount: U256::from(42u64),
                },
                "-42 of 0x7070…7070 to 0x1111000000000000000000000000000000002222",
            ),
            (
                Effect::Eth {
                    from: ME,
                    to: PEER,
                    amount: milli_eth(1),
                },
                "-0.001 ETH to 0x1111000000000000000000000000000000002222",
            ),
            (
                Effect::Eth {
                    from: ME,
                    to: ME,
                    amount: milli_eth(1),
                },
                "0.001 ETH to self",
            ),
            (
                Effect::Eth {
                    from: PEER,
                    to: ME,
                    amount: milli_eth(1_500),
                },
                "+1.50 ETH from 0x1111000000000000000000000000000000002222",
            ),
            (
                Effect::Sent {
                    to: Some(PEER),
                    nonce: Some(42),
                    success: true,
                    fee: Some(U256::from(21_000_000_000_000u64)),
                    value: None,
                },
                "sent to 0x1111000000000000000000000000000000002222 (nonce 42, fee 0.000021 ETH)",
            ),
            (
                Effect::Sent {
                    to: Some(ME),
                    nonce: Some(44),
                    success: true,
                    fee: None,
                    value: Some(U256::from(1_000_000_000u64)),
                },
                "sent 0.000000001 ETH to self (nonce 44)",
            ),
            (
                Effect::Sent {
                    to: Some(PEER),
                    nonce: Some(43),
                    success: false,
                    fee: None,
                    value: None,
                },
                "failed send to 0x1111000000000000000000000000000000002222 (nonce 43)",
            ),
            (
                Effect::Delegated {
                    to: Some(ODD),
                    confirmed: true,
                },
                "delegated to 0x7070000000000000000000000000000000007070",
            ),
            (
                Effect::Delegated {
                    to: None,
                    confirmed: true,
                },
                "delegation cleared",
            ),
            (
                Effect::Delegated {
                    to: Some(ODD),
                    confirmed: false,
                },
                "delegated to 0x7070000000000000000000000000000000007070 (not confirmed: no state change recorded for this block)",
            ),
            (
                Effect::Delegated {
                    to: None,
                    confirmed: false,
                },
                "delegation cleared (not confirmed: no state change recorded for this block)",
            ),
            (
                Effect::Nft {
                    token: ODD,
                    from: PEER,
                    to: ME,
                    id: U256::from(7u64),
                },
                "+NFT #7 of 0x7070…7070 from 0x1111000000000000000000000000000000002222",
            ),
            (
                Effect::Multi {
                    token: ODD,
                    operator: PEER,
                    from: PEER,
                    to: ME,
                    id: U256::from(7u64),
                    amount: U256::from(3u64),
                },
                "+3 of #7 of 0x7070…7070 from 0x1111000000000000000000000000000000002222",
            ),
            (
                Effect::Approval {
                    token: USDC,
                    owner: ME,
                    spender: PEER,
                    amount: U256::from(100_000_000u64),
                },
                "approved 0x1111000000000000000000000000000000002222 for 100.00 USDC",
            ),
            (
                Effect::NftApproval {
                    token: ODD,
                    owner: ME,
                    approved: PEER,
                    id: U256::from(7u64),
                },
                "approved 0x1111000000000000000000000000000000002222 for NFT #7 of 0x7070…7070",
            ),
            (
                Effect::ApprovalForAll {
                    token: ODD,
                    owner: ME,
                    operator: PEER,
                    approved: false,
                },
                "revoked 0x1111000000000000000000000000000000002222 for all of 0x7070…7070",
            ),
            (
                Effect::Withdrawal {
                    validator: 12,
                    amount: milli_eth(500),
                },
                "+0.50 ETH withdrawal (validator 12)",
            ),
            (
                Effect::FeeCredit {
                    amount: Some(U256::from(100_000_000_000_000u64)),
                },
                "+0.0001 ETH fee credit",
            ),
            (
                Effect::Called { from: PEER },
                "called by 0x1111000000000000000000000000000000002222",
            ),
            (
                Effect::BalanceChange {
                    before: Some(milli_eth(1_200)),
                    after: milli_eth(1_300),
                },
                "balance 1.20 -> 1.30 ETH",
            ),
            (
                Effect::Other {
                    emitter: ODD,
                    topic0: Some(B256::repeat_byte(9)),
                },
                "event from 0x7070…7070",
            ),
        ];
        for (effect, what) in cases {
            assert_eq!(line(effect), format!("11,856,337  12 min ago  {what}"));
        }
    }

    #[test]
    fn age_turns_over_at_the_minute_hour_and_day() {
        let shown = [0, 59, 60, 3_599, 3_600, 86_399, 86_400, 5 * 86_400].map(ago);
        assert_eq!(
            shown,
            [
                "0 s ago",
                "59 s ago",
                "1 min ago",
                "59 min ago",
                "1 h ago",
                "23 h ago",
                "1 d ago",
                "5 d ago"
            ]
        );
    }

    #[test]
    fn units_keep_full_precision() {
        let eth = |wei: u128| format_units(U256::from(wei), ETH_DECIMALS);
        assert_eq!(eth(1_203_400_000_000_000_000), "1.2034");
        assert_eq!(eth(1), "0.000000000000000001");
        assert_eq!(eth(0), "0.00");
        assert_eq!(format_units(U256::from(250_000_000u64), 6), "250.00");
        assert_eq!(format_units(U256::from(12u64), 0), "12");
    }

    /// A stored send of `nonce` in block `number`.
    fn send(number: u64, nonce: u64) -> (u64, TxRow) {
        let tx = TxLegacy {
            nonce,
            gas_limit: 21_000,
            to: TxKind::Call(PEER),
            ..TxLegacy::default()
        };
        let signature = Signature::new(U256::from(1u64), U256::from(1u64), false);
        let signed = Signed::new_unhashed(tx, signature);
        let raw = wire::Transaction::Legacy(signed).encoded_2718();
        let row = TxRow {
            index: 0,
            hash: B256::repeat_byte(1),
            sender: ME,
            raw: Some(Bytes::from(raw)),
            success: true,
            cumulative_gas: 21_000,
            gas_used: 21_000,
            first_log_index: 0,
            log_count: 0,
        };
        (number, row)
    }

    fn coverage(logs: &[(u64, u64)], txs: Vec<(u64, TxRow)>, opens: Vec<OpenState>) -> String {
        let coverage = Coverage {
            account: ME,
            ranges: vec![
                (
                    Capability::Bal,
                    Ranges::from_iter([(11_856_337, 11_990_000)]),
                ),
                (Capability::Logs, logs.iter().copied().collect()),
            ],
            settled: None,
            opens,
            txs,
            nonces: Vec::new(),
        };
        coverage_line(&coverage, &send_check(&coverage, 11_155_111))
    }

    /// My proven state at block `number` with nonce `nonce`.
    fn open(number: u64, nonce: u64) -> OpenState {
        OpenState {
            account: ME,
            number,
            hash: B256::repeat_byte(2),
            balance: U256::ZERO,
            nonce,
            slots: Vec::new(),
        }
    }

    #[test]
    fn coverage_with_no_gaps_and_complete_sends() {
        assert_eq!(
            coverage(
                &[(11_856_337, 11_990_000)],
                vec![send(11_900_000, 17), send(11_950_000, 18)],
                vec![open(11_960_000, 19)]
            ),
            "history since block 11,856,337 · gaps: none · sends from nonce 17 complete · \
             earlier sends may predate the history"
        );
        assert_eq!(
            coverage(&[(100, 200)], vec![send(150, 0)], vec![open(160, 1)]),
            "history since block 100 · gaps: none · sends from nonce 0 complete"
        );
        assert_eq!(
            coverage(&[(100, 200)], Vec::new(), Vec::new()),
            "history since block 100 · gaps: none · no sends found"
        );
    }

    #[test]
    fn coverage_without_an_open_state_leaves_later_sends_unchecked() {
        assert_eq!(
            coverage(&[(100, 200)], vec![send(150, 0)], Vec::new()),
            "history since block 100 · gaps: none · nonces after 0 unchecked"
        );
    }

    #[test]
    fn coverage_lists_the_gaps_between_its_first_and_last_block() {
        assert_eq!(
            coverage(
                &[
                    (11_856_337, 11_901_199),
                    (11_912_801, 11_950_000),
                    (11_950_002, 11_990_000)
                ],
                vec![send(11_900_000, 0)],
                vec![open(11_950_000, 1)]
            ),
            "history since block 11,856,337 · gaps: 11,901,200-11,912,800, 11,950,001 · \
             sends from nonce 0 complete"
        );
    }

    #[test]
    fn coverage_names_unexplained_nonces_and_where_they_lie() {
        assert_eq!(
            coverage(
                &[(1_000, 2_000)],
                vec![send(1_100, 5), send(1_500, 7), send(1_600, 10)],
                vec![open(1_700, 11)]
            ),
            "history since block 1,000 · gaps: none · nonce 6 unexplained between blocks 1,100 \
             and 1,500 · nonces 8-9 unexplained between blocks 1,500 and 1,600 · \
             earlier sends may predate the history"
        );
    }

    #[test]
    fn coverage_names_a_top_gap_below_the_open_nonce() {
        assert_eq!(
            coverage(
                &[(1_000, 2_000)],
                vec![send(1_100, 0)],
                vec![open(1_800, 3)]
            ),
            "history since block 1,000 · gaps: none · nonces 1-2 unexplained after the newest \
             send"
        );
    }

    #[test]
    fn coverage_without_log_ranges_says_there_is_no_history_yet() {
        assert_eq!(coverage(&[], Vec::new(), Vec::new()), "no history yet");
    }

    const ACCOUNT: Address = address!("0x3499ddf9E66b53Ca484554d5309Bf32521AAE5a7");
    const FRIEND: Address = address!("0xd72A04f52788544702EA111E54B4B226Fd328146");
    const SEPOLIA_USDC: Address = address!("0x1c7D4B196Cb0C7B01d743Fbc6116a902379C7238");
    const HASH: B256 = B256::repeat_byte(0x67);
    const OTHER_HASH: B256 = B256::repeat_byte(0x42);
    const SEPOLIA: [Token; 1] = [Token {
        symbol: "USDC",
        name: "USD Coin",
        address: SEPOLIA_USDC,
        decimals: 6,
    }];
    const BLOCK: u64 = 11_969_090;

    fn reader(owner: Owner) -> Reader<'static> {
        Reader {
            account: ACCOUNT,
            owner,
            registry: &SEPOLIA,
            now: NOW,
            explorer: Explorer("https://sepolia.etherscan.io"),
        }
    }

    /// An activity of block [`BLOCK`], mined 7 minutes before [`NOW`].
    fn activity(at: At, tx: Option<B256>, sent: bool, effect: Effect) -> Activity {
        Activity {
            number: BLOCK,
            timestamp: NOW - 7 * 60,
            at,
            tx,
            sent,
            effect,
        }
    }

    fn sent_by_me(effect: Effect) -> Activity {
        activity(At::Tx(3), Some(HASH), true, effect)
    }

    fn sent(to: Address, nonce: u64, fee: u64) -> Effect {
        Effect::Sent {
            to: Some(to),
            nonce: Some(nonce),
            success: true,
            fee: Some(U256::from(fee)),
            value: None,
        }
    }

    fn usdc(units: u64, from: Address, to: Address) -> Effect {
        Effect::Token {
            token: SEPOLIA_USDC,
            from,
            to,
            amount: U256::from(units),
        }
    }

    fn wei(from: Address, to: Address, amount: u64) -> Effect {
        Effect::Eth {
            from,
            to,
            amount: U256::from(amount),
        }
    }

    fn cells(tx: &str, from: &str, to: &str, amount: &str, fee: &str) -> Cells {
        Cells {
            tx: tx.to_owned(),
            block: "11,969,090".to_owned(),
            age: "7m".to_owned(),
            from: from.to_owned(),
            to: to.to_owned(),
            amount: amount.to_owned(),
            fee: fee.to_owned(),
        }
    }

    fn rows(activities: &[Activity], owner: Owner) -> Vec<Cells> {
        entries(activities, false, &reader(owner))
            .into_iter()
            .map(|entry| entry.cells)
            .collect()
    }

    fn detail_text(entry: &Entry) -> Vec<String> {
        entry.detail.iter().map(ToString::to_string).collect()
    }

    #[test]
    fn a_send_and_its_eth_move_are_one_row() {
        let activities = [
            sent_by_me(wei(ACCOUNT, FRIEND, 1_000_000_000)),
            sent_by_me(sent(FRIEND, 46, 1_669_629)),
        ];
        assert_eq!(
            rows(&activities, Owner::You),
            [cells(
                "0x6767…6767",
                "you",
                "0xd72A…8146",
                "-0.000000001 ETH",
                "<0.000001"
            )]
        );
    }

    #[test]
    fn a_token_send_and_its_sent_are_one_row_whose_details_say_everything_in_full() {
        let activities = [
            sent_by_me(usdc(1_000_000, ACCOUNT, FRIEND)),
            sent_by_me(sent(SEPOLIA_USDC, 46, 1_669_629)),
        ];
        let entries = entries(&activities, false, &reader(Owner::You));
        assert_eq!(entries.len(), 1);
        assert_eq!(
            entries[0].cells,
            cells(
                "0x6767…6767",
                "you",
                "0xd72A…8146",
                "-1.00 USDC",
                "<0.000001"
            )
        );
        assert_eq!(
            detail_text(&entries[0]),
            [
                "transaction  0x6767676767676767676767676767676767676767676767676767676767676767",
                "block        11,969,090 · 7 min ago",
                "status       success",
                "from         0x3499ddf9E66b53Ca484554d5309Bf32521AAE5a7 (you)",
                "to           0x1c7D4B196Cb0C7B01d743Fbc6116a902379C7238",
                "nonce        46",
                "fee          0.000000000001669629 ETH",
                "",
                "-1.00 USDC to 0xd72A04f52788544702EA111E54B4B226Fd328146",
                "",
                "https://sepolia.etherscan.io/tx/0x6767676767676767676767676767676767676767676767676767676767676767",
            ]
        );
        assert_eq!(
            entries[0].key,
            EntryKey {
                number: BLOCK,
                at: At::Tx(3),
                nth: 0
            }
        );
    }

    #[test]
    fn two_withdrawals_in_one_block_are_two_rows() {
        let withdrawal = |validator| {
            activity(
                At::Post,
                None,
                false,
                Effect::Withdrawal {
                    validator,
                    amount: milli_eth(500),
                },
            )
        };
        let entries = entries(
            &[withdrawal(42), withdrawal(43)],
            false,
            &reader(Owner::You),
        );
        let shown: Vec<(EntryKey, Cells)> = entries
            .into_iter()
            .map(|entry| (entry.key, entry.cells))
            .collect();
        let key = |nth| EntryKey {
            number: BLOCK,
            at: At::Post,
            nth,
        };
        assert_eq!(
            shown,
            [
                (
                    key(0),
                    cells("withdrawal", "validator 42", "you", "+0.50 ETH", "")
                ),
                (
                    key(1),
                    cells("withdrawal", "validator 43", "you", "+0.50 ETH", "")
                ),
            ]
        );
    }

    #[test]
    fn a_swap_lists_both_movements_when_they_fit_and_counts_them_when_not() {
        let swap = |paid| {
            [
                sent_by_me(wei(ACCOUNT, FRIEND, paid)),
                sent_by_me(usdc(500_000_000, FRIEND, ACCOUNT)),
                sent_by_me(sent(FRIEND, 47, 512_000_000_000_000)),
            ]
        };
        assert_eq!(
            rows(&swap(100_000_000_000_000_000), Owner::You),
            [cells(
                "0x6767…6767",
                "you",
                "0xd72A…8146",
                "-0.10 ETH, +500.00 USDC",
                "0.000512"
            )]
        );
        assert_eq!(
            rows(&swap(1_000_000_000), Owner::You),
            [cells(
                "0x6767…6767",
                "you",
                "0xd72A…8146",
                "2 transfers",
                "0.000512"
            )]
        );
        let received = [
            activity(At::Tx(5), Some(HASH), false, wei(FRIEND, ACCOUNT, 1)),
            activity(
                At::Tx(5),
                Some(HASH),
                false,
                usdc(1_000_000, SEPOLIA_USDC, ACCOUNT),
            ),
        ];
        assert_eq!(
            rows(&received, Owner::You),
            [cells("0x6767…6767", "several", "you", "2 transfers", "")]
        );
    }

    #[test]
    fn a_send_to_oneself_shows_its_value_between_the_account_and_itself() {
        let to_self = [sent_by_me(Effect::Sent {
            to: Some(ACCOUNT),
            nonce: Some(44),
            success: true,
            fee: None,
            value: Some(U256::from(1_000_000_000u64)),
        })];
        assert_eq!(
            rows(&to_self, Owner::You),
            [cells("0x6767…6767", "you", "you", "0.000000001 ETH", "")]
        );
        assert_eq!(
            rows(&to_self, Owner::Watched),
            [cells(
                "0x6767…6767",
                "watched",
                "watched",
                "0.000000001 ETH",
                ""
            )]
        );
    }

    #[test]
    fn the_fee_rounds_down_to_six_decimals_and_a_tiny_one_shows_as_below_the_step() {
        let shown = [
            1_669_629u64,
            512_000_000_000_000,
            512_999_999_999_999,
            1_000_000_000_000,
            0,
        ]
        .map(|wei| fee_cell(U256::from(wei)));
        assert_eq!(
            shown,
            ["<0.000001", "0.000512", "0.000512", "0.000001", "0.00"]
        );
    }

    #[test]
    fn age_shows_one_compact_unit() {
        let shown = [45, 7 * 60 + 5, 3 * 3_600, 2 * 86_400 + 5].map(age);
        assert_eq!(shown, ["45s", "7m", "3h", "2d"]);
    }

    #[test]
    fn the_account_is_you_only_when_unlocked_and_watched_otherwise() {
        assert_eq!(Owner::of(ACCOUNT, Some(ACCOUNT)), Owner::You);
        assert_eq!(Owner::of(ACCOUNT, None), Owner::Watched);
        let received = [activity(
            At::Tx(1),
            Some(HASH),
            false,
            wei(FRIEND, ACCOUNT, 1_500_000_000_000_000_000),
        )];
        let row = |owner| cells("0x6767…6767", "0xd72A…8146", owner, "+1.50 ETH", "");
        assert_eq!(rows(&received, Owner::You), [row("you")]);
        assert_eq!(rows(&received, Owner::Watched), [row("watched")]);

        let to_self = [sent_by_me(sent(ACCOUNT, 1, 0))];
        let entries = entries(&to_self, false, &reader(Owner::Watched));
        assert!(
            detail_text(&entries[0]).contains(
                &"to           0x3499ddf9E66b53Ca484554d5309Bf32521AAE5a7 (watched)".to_owned()
            ),
            "{:#?}",
            entries[0].detail
        );
    }

    #[test]
    fn a_zero_transfer_out_the_account_did_not_send_is_flagged_in_text() {
        let bait = [activity(
            At::Tx(9),
            Some(HASH),
            false,
            usdc(0, ACCOUNT, FRIEND),
        )];
        let flagged = entries(&bait, false, &reader(Owner::You));
        assert!(flagged[0].warning);
        assert_eq!(
            flagged[0].cells,
            cells("0x6767…6767", "you", "⚠ hidden", "⚠ 0.00 USDC", "")
        );
        assert!(
            detail_text(&flagged[0]).contains(
                &"-0.00 USDC to 0xd72A04f52788544702EA111E54B4B226Fd328146 (zero transfer you did \
              not send; do not copy this address)"
                    .to_owned()
            )
        );

        let mine = [sent_by_me(usdc(0, ACCOUNT, FRIEND))];
        let unflagged = entries(&mine, false, &reader(Owner::You));
        assert!(!unflagged[0].warning);
        assert_eq!(unflagged[0].cells.amount, "-0.00 USDC");
    }

    #[test]
    fn a_grouped_summary_keeps_the_warning_marker_and_hides_its_counterparty() {
        let bait = || activity(At::Tx(9), Some(HASH), false, usdc(0, ACCOUNT, FRIEND));
        let three = entries(&[bait(), bait(), bait()], false, &reader(Owner::You));
        assert!(three[0].warning);
        assert_eq!(
            (
                three[0].cells.from.as_str(),
                three[0].cells.to.as_str(),
                three[0].cells.amount.as_str()
            ),
            ("⚠ hidden", "you", "⚠ 3 transfers")
        );

        let joined = [
            bait(),
            activity(
                At::Tx(9),
                Some(HASH),
                false,
                usdc(1_000_000, FRIEND, ACCOUNT),
            ),
        ];
        let row = &rows(&joined, Owner::You)[0];
        assert_eq!(
            (row.from.as_str(), row.to.as_str(), row.amount.as_str()),
            ("⚠ hidden", "you", "⚠ 0.00 USDC, +1.00 USDC")
        );
    }

    #[test]
    fn a_flagged_row_never_shows_the_short_lookalike_address() {
        let bait = || activity(At::Tx(9), Some(HASH), false, usdc(0, ACCOUNT, FRIEND));
        let sent = activity(
            At::Tx(9),
            Some(HASH),
            true,
            Effect::Sent {
                to: Some(FRIEND),
                nonce: Some(4),
                success: true,
                fee: None,
                value: None,
            },
        );
        for group in [
            vec![bait()],
            vec![bait(), bait(), bait()],
            vec![sent, bait(), bait()],
        ] {
            let entry = &entries(&group, false, &reader(Owner::You))[0];
            let Cells { from, to, .. } = &entry.cells;
            assert!(entry.warning);
            assert!(
                [from, to].contains(&&"⚠ hidden".to_owned()),
                "{:?}",
                entry.cells
            );
            assert!(
                !format!("{:?}", entry.cells).contains("0xd72A"),
                "{:?}",
                entry.cells
            );
            assert!(
                detail_text(entry)
                    .iter()
                    .any(|line| line.contains("0xd72A04f52788544702EA111E54B4B226Fd328146")),
                "{:#?}",
                entry.detail
            );
        }
    }

    #[test]
    fn details_name_unknown_tokens_and_event_emitters_in_full() {
        let mystery = address!("0xAbFC25F2A93829e9A500A5641222F1f2530BD06f");
        let unknown = activity(
            At::Tx(2),
            Some(HASH),
            false,
            Effect::Token {
                token: mystery,
                from: FRIEND,
                to: ACCOUNT,
                amount: U256::from(5),
            },
        );
        let event = activity(
            At::Tx(2),
            Some(HASH),
            false,
            Effect::Other {
                emitter: mystery,
                topic0: None,
            },
        );
        let shown = entries(std::slice::from_ref(&unknown), false, &reader(Owner::You));
        assert_eq!(shown[0].cells.amount, "+5 of 0xAbFC…D06f");
        assert!(
            detail_text(&shown[0]).contains(
                &"+5 of 0xAbFC25F2A93829e9A500A5641222F1f2530BD06f from \
                  0xd72A04f52788544702EA111E54B4B226Fd328146"
                    .to_owned()
            ),
            "{:#?}",
            shown[0].detail
        );
        let shown = entries(&[event], false, &reader(Owner::You));
        assert!(
            detail_text(&shown[0])
                .contains(&"event from 0xAbFC25F2A93829e9A500A5641222F1f2530BD06f".to_owned()),
            "{:#?}",
            shown[0].detail
        );
        assert_eq!(
            activity_line(&unknown, ACCOUNT, &SEPOLIA, NOW),
            "11,969,090  7 min ago  +5 of 0xAbFC…D06f from 0xd72A04f52788544702EA111E54B4B226Fd328146"
        );
    }

    #[test]
    fn a_full_read_drops_its_oldest_entry_unless_it_is_the_only_one() {
        let newer = activity(At::Tx(4), Some(OTHER_HASH), false, wei(FRIEND, ACCOUNT, 1));
        let older = [
            sent_by_me(usdc(1_000_000, ACCOUNT, FRIEND)),
            sent_by_me(sent(SEPOLIA_USDC, 46, 1)),
        ];
        let mut both = vec![newer];
        both.extend(older.clone());
        let places = |activities: &[Activity], full| {
            entries(activities, full, &reader(Owner::You))
                .into_iter()
                .map(|entry| entry.key.at)
                .collect::<Vec<_>>()
        };
        assert_eq!(places(&both, false), [At::Tx(4), At::Tx(3)]);
        assert_eq!(places(&both, true), [At::Tx(4)]);

        let alone = entries(&older, true, &reader(Owner::You));
        assert_eq!(alone.len(), 1);
        assert_eq!(
            detail_text(&alone[0])[7..],
            [
                "",
                "-1.00 USDC to 0xd72A04f52788544702EA111E54B4B226Fd328146",
                "more effects than chibi loaded",
                "",
                "https://sepolia.etherscan.io/tx/0x6767676767676767676767676767676767676767676767676767676767676767",
            ]
        );
        let unmarked = entries(&older, false, &reader(Owner::You));
        assert!(!detail_text(&unmarked[0]).contains(&"more effects than chibi loaded".to_owned()));
    }

    #[test]
    fn an_entry_that_moves_nothing_says_what_it_did() {
        let approve = [
            sent_by_me(Effect::Approval {
                token: SEPOLIA_USDC,
                owner: ACCOUNT,
                spender: FRIEND,
                amount: U256::MAX,
            }),
            sent_by_me(sent(SEPOLIA_USDC, 48, 0)),
        ];
        let called = [activity(
            At::Tx(2),
            Some(HASH),
            false,
            Effect::Called { from: FRIEND },
        )];
        let delegated = [sent_by_me(Effect::Delegated {
            to: Some(FRIEND),
            confirmed: true,
        })];
        let event = [activity(
            At::Tx(2),
            Some(HASH),
            false,
            Effect::Other {
                emitter: FRIEND,
                topic0: None,
            },
        )];
        let fee_credit = [activity(
            At::Tx(2),
            None,
            false,
            Effect::FeeCredit {
                amount: Some(U256::from(100_000_000_000_000u64)),
            },
        )];
        let balance = [activity(
            At::System,
            None,
            false,
            Effect::BalanceChange {
                before: None,
                after: milli_eth(1_300),
            },
        )];
        let hashed = "0x6767…6767";
        let cases = [
            (
                &approve[..],
                cells(hashed, "you", "0x1c7D…7238", "approve USDC", "0.00"),
            ),
            (&called, cells(hashed, "0xd72A…8146", "you", "call", "")),
            (&delegated, cells(hashed, "", "", "delegate", "")),
            (&event, cells(hashed, "", "", "event", "")),
            (
                &fee_credit,
                cells("fee credit", "block fees", "you", "+0.0001 ETH", ""),
            ),
            (&balance, cells("balance", "", "", "= 1.30 ETH", "")),
        ];
        for (activities, expected) in cases {
            assert_eq!(rows(activities, Owner::You), [expected]);
        }
    }
}
