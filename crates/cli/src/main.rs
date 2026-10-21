//! The `chibi` binary: `init`, `node`, `wallet`, `stop`, and `status`.

#![allow(
    clippy::print_stdout,
    clippy::print_stderr,
    reason = "the CLI's user-facing output belongs on the terminal"
)]

mod activity;
mod config;
mod init;
mod names;
mod rpc;
mod run;
mod tui;

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fmt;
use std::io::{self, Write};
use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use alloy_primitives::Address;
use chibi_core::history::{Activity, Coverage, decode, send_check};
use chibi_node::disk::Holder;
use chibi_node::{Disk, History, Network, NodeError, Snapshot, Token, read_snapshot};
use clap::{Parser, Subcommand};
use eyre::WrapErr;

use crate::activity::{ETH_DECIMALS, activity_line, coverage_line, format_units};
use crate::config::{absolute_path, default_config_path, process_env};
use crate::init::InitArgs;
use crate::names::printable;
use crate::run::{NodeArgs, WalletArgs};

/// Activities `chibi status` shows per account.
const RECENT: usize = 5;

#[derive(Debug, Parser)]
#[command(
    name = "chibi",
    version,
    about = "A local Ethereum JSON-RPC endpoint for browser wallets that verifies everything it serves"
)]
struct Cli {
    /// Config file to use instead of $XDG_CONFIG_HOME/chibi/config.toml; defaults to
    /// $CHIBI_CONFIG when that is set and not empty.
    #[arg(long, global = true, value_name = "PATH")]
    config: Option<PathBuf>,
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Write the config file, asking for whatever the flags leave out.
    Init(InitArgs),
    /// Follow the chain and serve JSON-RPC to your browser wallet.
    Node(NodeArgs),
    /// Run the node under a terminal UI that sends ETH or tokens from your keystore.
    Wallet(WalletArgs),
    /// Stop the chibi running in the background.
    Stop(Target),
    /// Show whether chibi runs and the balances saved at the last verified head.
    Status(Target),
}

/// Which chibi `status` and `stop` act on, when it is not the configured one.
#[derive(Debug, Default, PartialEq, Eq, clap::Args)]
struct Target {
    /// Network to act on instead of the configured one.
    #[arg(long)]
    network: Option<Network>,
    /// Data directory root the chibi was started with, instead of the configured one.
    #[arg(long, value_name = "PATH", value_parser = absolute_path)]
    data_dir: Option<PathBuf>,
}

fn main() -> eyre::Result<()> {
    let cli = Cli::parse();
    let (path, named) = config_paths(cli.config, process_env)?;
    match cli.command {
        Command::Init(answers) => init::init(answers, &path, named.as_deref()),
        Command::Node(args) => run::node(&args, &path, named.as_deref()),
        Command::Wallet(args) => run::wallet(&args, &path, named.as_deref()),
        Command::Stop(target) => {
            let (network, dir) = target_dir(&target, &path, named.is_some())?;
            run::stop(network, &dir)
        }
        Command::Status(target) => status(&target, &path, named.is_some()),
    }
}

/// The absolute config path to use, and the same path again when the user named it.
fn config_paths(
    flag: Option<PathBuf>,
    env: impl Fn(&str) -> Option<OsString>,
) -> eyre::Result<(PathBuf, Option<PathBuf>)> {
    let named = named_config(flag, &env)
        .map(std::path::absolute)
        .transpose()
        .wrap_err("cannot resolve the config path")?;
    let path = match &named {
        Some(path) => path.clone(),
        None => default_config_path(&env)?,
    };
    Ok((path, named))
}

/// The config the user named: `--config`, else a non-empty `CHIBI_CONFIG`.
fn named_config(flag: Option<PathBuf>, env: impl Fn(&str) -> Option<OsString>) -> Option<PathBuf> {
    flag.or_else(|| {
        env("CHIBI_CONFIG")
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
    })
}

/// The network `target` names and its data directory, from the defaults when there is no
/// config file at the default path.
fn target_dir(target: &Target, path: &Path, named: bool) -> eyre::Result<(Network, PathBuf)> {
    let mut config = run::config(path, named)?;
    if let Some(dir) = &target.data_dir {
        config.data_dir = Some(dir.clone());
    }
    let network = target.network.unwrap_or(config.network);
    Ok((network, config.network_dir(network, process_env)?))
}

fn status(target: &Target, path: &Path, named: bool) -> eyre::Result<()> {
    let (network, data_dir) = target_dir(target, path, named)?;
    let running = Disk::running(&data_dir)?;
    let snapshot = read_snapshot(&data_dir)?;
    let histories = match &snapshot {
        Some(snapshot) => read_histories(&data_dir, snapshot)?,
        None => BTreeMap::new(),
    };
    let written = write_full_status(
        &mut io::stdout().lock(),
        running.as_ref(),
        snapshot.as_ref(),
        &histories,
        network,
    );
    match written {
        Err(err) if err.kind() == io::ErrorKind::BrokenPipe => Ok(()),
        written => Ok(written?),
    }
}

/// What history holds for one watched account: its coverage and its newest activities.
struct AccountHistory {
    coverage: Coverage,
    recent: Vec<Activity>,
}

/// The history of each account in `snapshot`, read beside any running node. Empty when the
/// data directory has no history or no account has coverage yet, so status reads as it did
/// before history.
fn read_histories(
    data_dir: &Path,
    snapshot: &Snapshot,
) -> Result<BTreeMap<Address, AccountHistory>, NodeError> {
    let Some(history) = History::open(data_dir)? else {
        return Ok(BTreeMap::new());
    };
    let mut histories = BTreeMap::new();
    for holding in &snapshot.holdings {
        let coverage = history.coverage(holding.address)?;
        let recent = recent(&history, holding.address)?;
        histories.insert(holding.address, AccountHistory { coverage, recent });
    }
    let covered = histories.values().any(|account| {
        account
            .coverage
            .ranges
            .iter()
            .any(|(_, ranges)| ranges.first().is_some())
    });
    if !covered {
        histories.clear();
    }
    Ok(histories)
}

/// The account's newest [`RECENT`] activities, newest first.
fn recent(history: &History, account: Address) -> Result<Vec<Activity>, NodeError> {
    let rows = NonZeroUsize::MIN.saturating_add(RECENT - 1);
    let mut found = Vec::with_capacity(RECENT);
    let mut before = None;
    while found.len() < RECENT {
        let page = history.page(account, before, rows)?;
        found.extend(decode(&page));
        match page.next {
            Some(next) => before = Some(next),
            None => break,
        }
    }
    found.truncate(RECENT);
    Ok(found)
}

fn write_full_status<W: Write>(
    output: &mut W,
    running: Option<&Holder>,
    snapshot: Option<&Snapshot>,
    histories: &BTreeMap<Address, AccountHistory>,
    network: Network,
) -> io::Result<()> {
    write_running(output, running)?;
    match snapshot {
        Some(snapshot) => write_status(
            output,
            snapshot,
            histories,
            network.spec().tokens,
            unix_now(),
        ),
        None => writeln!(
            output,
            "no snapshot for {network} yet; it is saved once `chibi node` or `chibi wallet` verifies its first head"
        ),
    }
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs())
}

fn write_running<W: Write>(output: &mut W, running: Option<&Holder>) -> io::Result<()> {
    match running {
        Some(Holder {
            pid,
            endpoint: Some(url),
        }) => writeln!(output, "running (pid {pid}), JSON-RPC {url}"),
        Some(Holder {
            pid,
            endpoint: None,
        }) => writeln!(
            output,
            "running (pid {pid}), starting; JSON-RPC is served once the first head is verified"
        ),
        None => writeln!(output, "not running"),
    }
}

fn write_status<W: Write>(
    output: &mut W,
    snapshot: &Snapshot,
    histories: &BTreeMap<Address, AccountHistory>,
    registry: &[Token],
    now: u64,
) -> io::Result<()> {
    writeln!(
        output,
        "{} head {} (saved {} ago, not re-verified)",
        snapshot.network,
        snapshot.block.number,
        Age(now.saturating_sub(snapshot.saved_at))
    )?;
    for holding in &snapshot.holdings {
        write!(output, "{}", holding.address)?;
        if let Some(name) = holding.name.as_deref().filter(|name| printable(name)) {
            write!(output, " {name}")?;
        }
        match holding.balance {
            Some(balance) => write!(output, "   {} ETH", format_units(balance, ETH_DECIMALS))?,
            None => write!(output, "   ? ETH")?,
        }
        for (symbol, amount) in &holding.tokens {
            let decimals = registry
                .iter()
                .find(|token| token.symbol == symbol)
                .map(|token| token.decimals);
            match (amount, decimals) {
                (Some(amount), Some(decimals)) => {
                    write!(output, "   {} {symbol}", format_units(*amount, decimals))?;
                }
                (Some(amount), None) => write!(output, "   {amount} {symbol} (base units)")?,
                (None, _) => write!(output, "   ? {symbol}")?,
            }
        }
        writeln!(output)?;
        if let Some(history) = histories.get(&holding.address) {
            let check = send_check(&history.coverage);
            writeln!(output, "  {}", coverage_line(&history.coverage, &check))?;
            for activity in &history.recent {
                writeln!(
                    output,
                    "  {}",
                    activity_line(activity, holding.address, registry, now)
                )?;
            }
        }
    }
    if !snapshot.contracts.is_empty() {
        writeln!(output, "contracts")?;
    }
    for contract in &snapshot.contracts {
        writeln!(output, "{}", contract.address)?;
        for (slot, value) in &contract.slots {
            match value {
                Some(value) => writeln!(output, "  {slot:#x} = {value:#x}")?,
                None => writeln!(output, "  {slot:#x} = ?")?,
            }
        }
    }
    Ok(())
}

/// A duration in seconds, shown in its largest whole unit.
struct Age(u64);

impl fmt::Display for Age {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let seconds = self.0;
        match seconds {
            0..60 => write!(f, "{seconds}s"),
            60..3_600 => write!(f, "{}m", seconds / 60),
            3_600..86_400 => write!(f, "{}h", seconds / 3_600),
            86_400.. => write!(f, "{}d", seconds / 86_400),
        }
    }
}

#[cfg(test)]
mod tests {
    use alloy_eips::BlockNumHash;
    use alloy_primitives::{B256, Bytes, Log, U256, address, b256};
    use chibi_core::history::{
        At, BlockFacts, BlockRow, Capability, LogRow, ScannedBlock, Touch, TxRow, WithdrawalRow,
    };
    use chibi_node::{ContractSnapshot, Holding};

    use crate::config::Config;

    use super::*;

    const ALICE: Address = address!("0x8f3Cf7ad23Cd3CaDbD9735AFf958023239c6A063");
    const BOB: Address = address!("0x00000000000000000000000000000000000000b0");

    #[test]
    fn status_line_names_the_pid_and_the_url_the_running_chibi_serves() {
        let line = |running: Option<Holder>| {
            let mut output = Vec::new();
            write_running(&mut output, running.as_ref()).unwrap();
            String::from_utf8(output).unwrap()
        };
        assert_eq!(
            line(Some(Holder {
                pid: 4242,
                endpoint: Some("http://chibi.local:8745".to_owned()),
            })),
            "running (pid 4242), JSON-RPC http://chibi.local:8745\n"
        );
        assert_eq!(
            line(Some(Holder {
                pid: 7,
                endpoint: None,
            })),
            "running (pid 7), starting; JSON-RPC is served once the first head is verified\n"
        );
        assert_eq!(line(None), "not running\n");
    }

    fn hash(number: u64) -> B256 {
        B256::left_padding_from(&number.to_be_bytes())
    }

    /// Block `number` scanned forward for alice: a withdrawal to her, or with `eth_from` an
    /// incoming ETH transfer from that sender.
    fn scanned(number: u64, eth_from: Option<Address>) -> ScannedBlock {
        let block = BlockRow {
            number,
            hash: hash(number),
            timestamp: number * 12,
            beneficiary: Address::repeat_byte(0xfe),
            base_fee: Some(7),
            excess_blob_gas: None,
            tx_count: u32::from(eth_from.is_some()),
        };
        let balance = Some(U256::from(number));
        let facts = match eth_from {
            Some(from) => BlockFacts {
                block,
                txs: vec![TxRow {
                    index: 0,
                    hash: B256::repeat_byte(0xee),
                    sender: from,
                    raw: None,
                    success: true,
                    cumulative_gas: 21_000,
                    gas_used: 21_000,
                    first_log_index: 0,
                    log_count: 1,
                }],
                logs: vec![LogRow {
                    index: 0,
                    log: Log::new_unchecked(
                        address!("0xfffffffffffffffffffffffffffffffffffffffe"),
                        vec![
                            b256!(
                                "0xddf252ad1be2c89b69c2b068fc378daa952ba7f163c4a11628f55a4df523b3ef"
                            ),
                            from.into_word(),
                            ALICE.into_word(),
                        ],
                        Bytes::from(U256::from(1_000_000_000_000_000u64).to_be_bytes_vec()),
                    ),
                }],
                touches: vec![Touch {
                    account: ALICE,
                    at: At::Tx(0),
                    balance,
                    nonce: None,
                    code: None,
                }],
                slots: Vec::new(),
                withdrawals: Vec::new(),
            },
            None => BlockFacts {
                block,
                txs: Vec::new(),
                logs: Vec::new(),
                touches: vec![Touch {
                    account: ALICE,
                    at: At::Post,
                    balance,
                    nonce: None,
                    code: None,
                }],
                slots: Vec::new(),
                withdrawals: vec![WithdrawalRow {
                    index: number,
                    validator: number,
                    address: ALICE,
                    amount_gwei: 500_000_000,
                }],
            },
        };
        ScannedBlock {
            number,
            hash: hash(number),
            parent_hash: hash(number - 1),
            accounts: vec![ALICE],
            capabilities: vec![Capability::Bal, Capability::Logs],
            facts: Some(facts),
        }
    }

    fn watching_alice_and_bob() -> Snapshot {
        Snapshot {
            network: Network::Sepolia,
            block: BlockNumHash::new(107, hash(107)),
            saved_at: 10_000,
            holdings: vec![
                Holding {
                    address: ALICE,
                    name: None,
                    balance: Some(U256::from(1_000_000_000_000_000_000u128)),
                    nonce: Some(0),
                    tokens: Vec::new(),
                },
                Holding {
                    address: BOB,
                    name: None,
                    balance: None,
                    nonce: None,
                    tokens: Vec::new(),
                },
            ],
            contracts: Vec::new(),
        }
    }

    fn status_of(dir: &Path, snapshot: &Snapshot) -> String {
        let histories = read_histories(dir, snapshot).unwrap();
        let mut output = Vec::new();
        write_status(&mut output, snapshot, &histories, &[], 10_008).unwrap();
        String::from_utf8(output).unwrap()
    }

    #[test]
    fn status_shows_coverage_and_the_newest_activities_under_each_account() {
        let dir = tempfile::tempdir().unwrap();
        let disk = Disk::open(dir.path()).unwrap();
        let mut blocks: Vec<ScannedBlock> = (101..=106).map(|n| scanned(n, None)).collect();
        blocks.push(scanned(107, Some(BOB)));
        chibi_node::fixture::record_history(&disk, BlockNumHash::new(100, hash(100)), &blocks)
            .unwrap();
        let snapshot = watching_alice_and_bob();
        assert_eq!(
            status_of(dir.path(), &snapshot),
            format!(
                "sepolia head 107 (saved 8s ago, not re-verified)\n\
                 {ALICE}   1.00 ETH\n\
                 \x20 history since block 101 · gaps: none · no sends found\n\
                 \x20 107  2 h ago  +0.001 ETH from 0x0000…00B0\n\
                 \x20 106  2 h ago  +0.50 ETH withdrawal (validator 106)\n\
                 \x20 105  2 h ago  +0.50 ETH withdrawal (validator 105)\n\
                 \x20 104  2 h ago  +0.50 ETH withdrawal (validator 104)\n\
                 \x20 103  2 h ago  +0.50 ETH withdrawal (validator 103)\n\
                 {BOB}   ? ETH\n\
                 \x20 no history yet\n"
            )
        );
    }

    #[test]
    fn status_without_history_prints_only_the_snapshot() {
        let before = format!(
            "sepolia head 107 (saved 8s ago, not re-verified)\n\
             {ALICE}   1.00 ETH\n\
             {BOB}   ? ETH\n"
        );
        let snapshot = watching_alice_and_bob();
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(status_of(dir.path(), &snapshot), before);
        drop(Disk::open(dir.path()).unwrap());
        assert_eq!(status_of(dir.path(), &snapshot), before);
    }

    #[test]
    fn status_shows_holdings_and_age_unverified() {
        let snapshot = Snapshot {
            network: Network::Sepolia,
            block: BlockNumHash::new(9_318_211, B256::repeat_byte(1)),
            saved_at: 1_000,
            holdings: vec![
                Holding {
                    address: ALICE,
                    name: Some("alice.eth".to_owned()),
                    balance: Some(U256::from(1_203_400_000_000_000_000u128)),
                    nonce: Some(3),
                    tokens: vec![
                        ("USDC".to_owned(), Some(U256::from(250_000_000u64))),
                        ("XYZ".to_owned(), Some(U256::from(5u64))),
                    ],
                },
                Holding {
                    address: BOB,
                    name: None,
                    balance: None,
                    nonce: None,
                    tokens: vec![("USDC".to_owned(), None)],
                },
            ],
            contracts: Vec::new(),
        };
        let mut output = Vec::new();
        write_status(
            &mut output,
            &snapshot,
            &BTreeMap::new(),
            Network::Sepolia.spec().tokens,
            1_008,
        )
        .unwrap();
        assert_eq!(
            String::from_utf8(output).unwrap(),
            format!(
                "sepolia head 9318211 (saved 8s ago, not re-verified)\n\
                 {ALICE} alice.eth   1.2034 ETH   250.00 USDC   5 XYZ (base units)\n\
                 {BOB}   ? ETH   ? USDC\n"
            )
        );
    }

    #[test]
    fn status_drops_names_with_control_or_invisible_characters() {
        let line = |name: &str| {
            let snapshot = Snapshot {
                network: Network::Mainnet,
                block: BlockNumHash::new(7, B256::repeat_byte(1)),
                saved_at: 1_000,
                holdings: vec![Holding {
                    address: BOB,
                    name: Some(name.to_owned()),
                    balance: None,
                    nonce: None,
                    tokens: Vec::new(),
                }],
                contracts: Vec::new(),
            };
            let mut output = Vec::new();
            write_status(&mut output, &snapshot, &BTreeMap::new(), &[], 1_000).unwrap();
            let output = String::from_utf8(output).unwrap();
            output.lines().nth(1).unwrap().to_owned()
        };
        for hostile in [
            "evil\u{1b}[2J.eth",
            "two\nlines.eth",
            "bell\u{7}.eth",
            "del\u{7f}.eth",
            "c1\u{9b}31m.eth",
            "\u{202e}htf.eth",
            "zero\u{200b}width.eth",
            "split\u{2028}.eth",
            "private\u{e000}.eth",
            "cgj\u{34f}.eth",
            "filler\u{3164}.eth",
            "half\u{ffa0}width.eth",
            "choseong\u{115f}.eth",
            "tag\u{e0041}.eth",
        ] {
            assert_eq!(line(hostile), format!("{BOB}   ? ETH"), "{hostile:?}");
        }
        for shown in [
            "ñandú.eth",
            "👨\u{200d}👩\u{200d}👧.eth",
            "❤\u{fe0f}.eth",
            "名前.eth",
        ] {
            assert_eq!(line(shown), format!("{BOB} {shown}   ? ETH"));
        }
    }

    #[test]
    fn status_renders_registry_tokens_with_registry_decimals() {
        let snapshot = Snapshot {
            network: Network::Mainnet,
            block: BlockNumHash::new(21_000_000, B256::repeat_byte(1)),
            saved_at: 1_000,
            holdings: vec![Holding {
                address: ALICE,
                name: None,
                balance: Some(U256::from(1_000_000_000_000_000_000u128)),
                nonce: Some(0),
                tokens: vec![
                    ("USDC".to_owned(), None),
                    ("WBTC".to_owned(), Some(U256::from(150_000_000u64))),
                ],
            }],
            contracts: Vec::new(),
        };
        let mut output = Vec::new();
        write_status(
            &mut output,
            &snapshot,
            &BTreeMap::new(),
            Network::Mainnet.spec().tokens,
            1_000,
        )
        .unwrap();
        assert_eq!(
            String::from_utf8(output).unwrap(),
            format!(
                "mainnet head 21000000 (saved 0s ago, not re-verified)\n\
                 {ALICE}   1.00 ETH   ? USDC   1.50 WBTC\n"
            )
        );
    }

    #[test]
    fn status_lists_contract_slots_in_hex() {
        let snapshot = Snapshot {
            network: Network::Sepolia,
            block: BlockNumHash::new(7, B256::repeat_byte(1)),
            saved_at: 1_000,
            holdings: Vec::new(),
            contracts: vec![
                ContractSnapshot {
                    address: ALICE,
                    slots: vec![
                        (U256::from(1), Some(U256::from(0x2a))),
                        (U256::from(0xff), None),
                    ],
                },
                ContractSnapshot {
                    address: BOB,
                    slots: Vec::new(),
                },
            ],
        };
        let mut output = Vec::new();
        write_status(&mut output, &snapshot, &BTreeMap::new(), &[], 1_000).unwrap();
        assert_eq!(
            String::from_utf8(output).unwrap(),
            format!(
                "sepolia head 7 (saved 0s ago, not re-verified)\n\
                 contracts\n\
                 {ALICE}\n\
                 \x20 0x1 = 0x2a\n\
                 \x20 0xff = ?\n\
                 {BOB}\n"
            )
        );
    }

    #[test]
    fn config_is_accepted_before_or_after_the_subcommand() {
        for argv in [
            ["chibi", "--config", "/c.toml", "node"],
            ["chibi", "wallet", "--config", "/c.toml"],
            ["chibi", "status", "--config", "/c.toml"],
            ["chibi", "--config", "/c.toml", "init"],
        ] {
            let cli = Cli::try_parse_from(argv).unwrap();
            assert_eq!(
                cli.config.as_deref(),
                Some(Path::new("/c.toml")),
                "{argv:?}"
            );
        }
        let cli = Cli::try_parse_from(["chibi", "stop", "--network", "mainnet"]).unwrap();
        assert!(matches!(
            cli.command,
            Command::Stop(Target {
                network: Some(Network::Mainnet),
                data_dir: None,
            })
        ));
    }

    #[test]
    fn node_and_wallet_replace_run_and_its_tui_flag() {
        use clap::error::ErrorKind;

        for argv in [
            &["chibi", "node"][..],
            &["chibi", "node", "-b"],
            &["chibi", "node", "--background", "--rpc-port", "8745"],
            &["chibi", "wallet"],
            &["chibi", "wallet", "--logs", "--keystore", "/k.json"],
            &["chibi", "init", "--keystore", "/k.json"],
        ] {
            assert!(Cli::try_parse_from(argv).is_ok(), "{argv:?}");
        }
        let error = Cli::try_parse_from(["chibi", "run"]).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::InvalidSubcommand);
        for argv in [
            &["chibi", "node", "--tui"][..],
            &["chibi", "node", "--logs"],
            &["chibi", "wallet", "--tui"],
            &["chibi", "wallet", "-b"],
            &["chibi", "wallet", "--background"],
            &["chibi", "wallet", "--detached"],
            &["chibi", "node", "--keystore", "/k.json"],
        ] {
            let error = Cli::try_parse_from(argv).unwrap_err();
            assert_eq!(error.kind(), ErrorKind::UnknownArgument, "{argv:?}");
        }
    }

    #[test]
    fn status_and_stop_find_the_instance_a_data_dir_flag_names() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        for command in ["status", "stop"] {
            let cli = Cli::try_parse_from(["chibi", command, "--data-dir", "/srv/chibi"]).unwrap();
            let (Command::Status(target) | Command::Stop(target)) = cli.command else {
                panic!("{command} parsed to another command");
            };
            assert_eq!(
                target_dir(&target, &path, false).unwrap(),
                (Network::Sepolia, PathBuf::from("/srv/chibi/sepolia"))
            );
        }
        let cli = Cli::try_parse_from(["chibi", "status", "--data-dir", "data"]).unwrap();
        let Command::Status(target) = cli.command else {
            panic!("status parsed to another command");
        };
        let data_dir = target.data_dir.unwrap();
        assert!(data_dir.is_absolute() && data_dir.ends_with("data"));
    }

    #[test]
    fn peer_flags_are_checked_like_the_config() {
        for command in ["init", "node", "wallet"] {
            for (flag, value) in [
                ("--trusted-peer", "enode://bad@10.0.0.1:30303"),
                ("--boot-node", "10.0.0.1:30303"),
                ("--max-outbound-peers", "0"),
                ("--max-inbound-peers", "-1"),
                ("--discovery-port", "70000"),
            ] {
                let error = Cli::try_parse_from(["chibi", command, flag, value])
                    .unwrap_err()
                    .to_string();
                assert!(error.contains(value), "{command} {flag}: {error}");
            }
        }
    }

    #[test]
    fn beacon_url_and_data_dir_flags_are_checked_like_the_config() {
        for command in ["init", "node", "wallet"] {
            let cli = Cli::try_parse_from([
                "chibi",
                command,
                "--beacon-url",
                "http://127.0.0.1:5052",
                "--data-dir",
                "data",
            ])
            .unwrap();
            let keys = match cli.command {
                Command::Init(args) => args.keys,
                Command::Node(args) => args.overrides.keys,
                Command::Wallet(args) => args.overrides.keys,
                Command::Stop(_) | Command::Status(_) => {
                    panic!("{command} parsed to another command")
                }
            };
            assert_eq!(
                keys.beacon_url.map(String::from).as_deref(),
                Some("http://127.0.0.1:5052/")
            );
            let data_dir = keys.data_dir.unwrap();
            assert!(data_dir.is_absolute() && data_dir.ends_with("data"));
            let error = Cli::try_parse_from(["chibi", command, "--beacon-url", "not a url"])
                .unwrap_err()
                .to_string();
            assert!(error.contains("not a url"), "{error}");
        }
    }

    #[test]
    fn chibi_config_applies_when_non_empty_and_the_flag_wins() {
        let env = |value: Option<&'static str>| {
            move |key: &str| {
                assert_eq!(key, "CHIBI_CONFIG");
                value.map(OsString::from)
            }
        };
        let flag = || Some(PathBuf::from("/flag.toml"));
        assert_eq!(named_config(None, env(None)), None);
        assert_eq!(named_config(None, env(Some(""))), None);
        assert_eq!(
            named_config(None, env(Some("/env.toml"))),
            Some(PathBuf::from("/env.toml"))
        );
        assert_eq!(named_config(flag(), env(Some("/env.toml"))), flag());
        assert_eq!(named_config(flag(), env(Some(""))), flag());
    }

    #[test]
    fn a_missing_chibi_config_is_an_error_and_a_missing_default_is_the_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let named = dir.path().join("typo.toml");
        let xdg = dir.path().join("xdg");
        let env = |chibi_config: Option<PathBuf>| {
            let xdg = xdg.clone();
            move |key: &str| match key {
                "CHIBI_CONFIG" => chibi_config.clone().map(PathBuf::into_os_string),
                "XDG_CONFIG_HOME" => Some(xdg.clone().into_os_string()),
                _ => None,
            }
        };

        let (path, flag) = config_paths(None, env(Some(named.clone()))).unwrap();
        assert_eq!((&path, flag.as_deref()), (&named, Some(named.as_path())));
        let error = run::config(&path, flag.is_some()).unwrap_err().to_string();
        assert!(error.contains(&named.display().to_string()), "{error}");

        let (path, flag) = config_paths(Some(named.clone()), env(None)).unwrap();
        assert_eq!((&path, flag.as_deref()), (&named, Some(named.as_path())));
        assert!(run::config(&path, flag.is_some()).is_err());

        let (path, flag) = config_paths(None, env(None)).unwrap();
        assert_eq!(path, xdg.join("chibi").join("config.toml"));
        assert_eq!(flag, None);
        assert_eq!(
            run::config(&path, flag.is_some()).unwrap(),
            Config::default()
        );
    }

    #[test]
    fn age_uses_largest_whole_unit() {
        let shown = [0, 59, 60, 3_599, 3_600, 86_399, 86_400, 200_000]
            .map(|seconds| Age(seconds).to_string());
        assert_eq!(shown, ["0s", "59s", "1m", "59m", "1h", "23h", "1d", "2d"]);
    }
}
