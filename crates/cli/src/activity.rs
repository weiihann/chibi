//! Account history as lines of text: one line per activity and one coverage line per account.
//! The formatting is pure, so `chibi status` and the wallet show history in the same words.

use std::fmt::Write as _;
use std::time::{SystemTime, UNIX_EPOCH};

use alloy_primitives::{Address, U256};
use chibi_core::history::{Activity, Capability, Coverage, Effect, SendCheck, TopEnd};
use chibi_node::Token;

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
        effect_text(activity, account, registry)
    )
}

fn eth(amount: U256) -> String {
    format!("{} ETH", format_units(amount, ETH_DECIMALS))
}

/// `amount` of the ERC-20 `token`, with its symbol when the registry knows it.
fn token_amount(amount: U256, token: Address, registry: &[Token]) -> String {
    match registry.iter().find(|known| known.address == token) {
        Some(known) => format!("{} {}", format_units(amount, known.decimals), known.symbol),
        None => format!("{amount} of {}", short(token)),
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

fn effect_text(activity: &Activity, account: Address, registry: &[Token]) -> String {
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
                &token_amount(*amount, *token, registry),
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
            &format!("NFT #{id} of {}", short(*token)),
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
            &format!("{amount} of #{id} of {}", short(*token)),
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
                    None => format!("unlimited of {}", short(*token)),
                }
            } else {
                token_amount(*amount, *token, registry)
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
            &format!("{approved} for NFT #{id} of {}", short(*token)),
        ),
        Effect::ApprovalForAll {
            token,
            owner,
            operator,
            approved,
        } => {
            let verb = if *approved { "approved" } else { "revoked" };
            let what = format!("{operator} for all of {}", short(*token));
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
        Effect::Delegated { to: Some(to) } => format!("delegated to {to}"),
        Effect::Delegated { to: None } => "delegation cleared".to_owned(),
        Effect::Other { emitter, .. } => format!("event from {}", short(*emitter)),
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
                Effect::Delegated { to: Some(ODD) },
                "delegated to 0x7070000000000000000000000000000000007070",
            ),
            (Effect::Delegated { to: None }, "delegation cleared"),
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
        coverage_line(&coverage, &send_check(&coverage))
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
}
