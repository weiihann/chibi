//! The `chibi` binary: `init`, `run`, `stop`, and `status`.

#![allow(
    clippy::print_stdout,
    clippy::print_stderr,
    reason = "the CLI's user-facing output belongs on the terminal"
)]

mod config;
mod init;
mod rpc;
mod run;

use std::ffi::OsString;
use std::fmt;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use alloy_primitives::U256;
use chibi_node::disk::Holder;
use chibi_node::{Disk, Network, Snapshot, Token, read_snapshot};
use clap::{Parser, Subcommand};
use eyre::WrapErr;

use crate::config::{Config, default_config_path, process_env};
use crate::init::InitArgs;
use crate::run::RunArgs;

/// Decimal places of one ether.
const ETH_DECIMALS: u8 = 18;

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
    /// Follow the chain and serve JSON-RPC to the wallet.
    Run(RunArgs),
    /// Stop the chibi running in the background.
    Stop {
        /// Network whose chibi to stop instead of the configured one.
        #[arg(long)]
        network: Option<Network>,
    },
    /// Show whether chibi runs and the balances saved at the last verified head.
    Status {
        /// Network to show instead of the configured one.
        #[arg(long)]
        network: Option<Network>,
    },
}

fn main() -> eyre::Result<()> {
    let cli = Cli::parse();
    let named = named_config(cli.config, process_env)
        .map(std::path::absolute)
        .transpose()
        .wrap_err("cannot resolve the config path")?;
    let path = match &named {
        Some(path) => path.clone(),
        None => default_config_path(process_env)?,
    };
    match cli.command {
        Command::Init(answers) => init::init(answers, &path, named.as_deref()),
        Command::Run(args) => run::run(&args, &path, named.as_deref()),
        Command::Stop { network } => {
            let config = Config::load(&path)?.unwrap_or_default();
            let network = network.unwrap_or(config.network);
            run::stop(network, &config.network_dir(network, process_env)?)
        }
        Command::Status { network } => status(network, &path),
    }
}

/// The config the user named: `--config`, else a non-empty `CHIBI_CONFIG`.
fn named_config(flag: Option<PathBuf>, env: impl Fn(&str) -> Option<OsString>) -> Option<PathBuf> {
    flag.or_else(|| {
        env("CHIBI_CONFIG")
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
    })
}

/// The network to show and its data directory, from the defaults when there is no config file.
fn status_target(network: Option<Network>, path: &Path) -> eyre::Result<(Network, PathBuf)> {
    let config = Config::load(path)?.unwrap_or_default();
    let network = network.unwrap_or(config.network);
    Ok((network, config.network_dir(network, process_env)?))
}

fn status(network: Option<Network>, path: &Path) -> eyre::Result<()> {
    let (network, data_dir) = status_target(network, path)?;
    let running = Disk::running(&data_dir)?;
    let snapshot = read_snapshot(&data_dir)?;
    let written = write_full_status(
        &mut io::stdout().lock(),
        running.as_ref(),
        snapshot.as_ref(),
        network,
    );
    match written {
        Err(err) if err.kind() == io::ErrorKind::BrokenPipe => Ok(()),
        written => Ok(written?),
    }
}

fn write_full_status<W: Write>(
    output: &mut W,
    running: Option<&Holder>,
    snapshot: Option<&Snapshot>,
    network: Network,
) -> io::Result<()> {
    write_running(output, running)?;
    match snapshot {
        Some(snapshot) => write_status(output, snapshot, network.spec().tokens, unix_now()),
        None => writeln!(
            output,
            "no snapshot for {network} yet; it is saved once `chibi run` verifies its first head"
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

/// Whether `name` prints as itself. ENS names are chain data anyone can set, so a name with a
/// control character, a line or paragraph separator, a default-ignorable code point (which
/// renders as nothing, so the name could pass for another), or a private-use character is
/// dropped rather than shown.
fn printable(name: &str) -> bool {
    name.chars().all(|c| {
        !c.is_control()
            && (c == ' ' || !c.is_whitespace())
            && !matches!(c, '\u{e000}'..='\u{f8ff}' | '\u{f0000}'..)
            && !DEFAULT_IGNORABLE
                .iter()
                .any(|&(first, last)| (first..=last).contains(&c))
    })
}

/// Default_Ignorable_Code_Point ranges from Unicode 16.0 DerivedCoreProperties.txt, adjacent
/// ranges merged, less U+200D ZERO WIDTH JOINER and U+FE0F VARIATION SELECTOR-16, which emoji
/// names need.
const DEFAULT_IGNORABLE: &[(char, char)] = &[
    ('\u{ad}', '\u{ad}'),
    ('\u{34f}', '\u{34f}'),
    ('\u{61c}', '\u{61c}'),
    ('\u{115f}', '\u{1160}'),
    ('\u{17b4}', '\u{17b5}'),
    ('\u{180b}', '\u{180f}'),
    ('\u{200b}', '\u{200c}'),
    ('\u{200e}', '\u{200f}'),
    ('\u{202a}', '\u{202e}'),
    ('\u{2060}', '\u{206f}'),
    ('\u{3164}', '\u{3164}'),
    ('\u{fe00}', '\u{fe0e}'),
    ('\u{feff}', '\u{feff}'),
    ('\u{ffa0}', '\u{ffa0}'),
    ('\u{fff0}', '\u{fff8}'),
    ('\u{1bca0}', '\u{1bca3}'),
    ('\u{1d173}', '\u{1d17a}'),
    ('\u{e0000}', '\u{e0fff}'),
];

/// `value` base units as a decimal with `decimals` places, trailing zeros trimmed to two.
fn format_units(value: U256, decimals: u8) -> String {
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
    use alloy_primitives::{Address, B256, address};
    use chibi_node::{ContractSnapshot, Holding};

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

    #[test]
    fn units_keep_full_precision() {
        let eth = |wei: u128| format_units(U256::from(wei), ETH_DECIMALS);
        assert_eq!(eth(1_203_400_000_000_000_000), "1.2034");
        assert_eq!(eth(1), "0.000000000000000001");
        assert_eq!(eth(0), "0.00");
        assert_eq!(format_units(U256::from(250_000_000u64), 6), "250.00");
        assert_eq!(format_units(U256::from(12u64), 0), "12");
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
            write_status(&mut output, &snapshot, &[], 1_000).unwrap();
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
    fn status_renders_unpinned_registry_tokens_with_registry_decimals() {
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
        write_status(&mut output, &snapshot, &[], 1_000).unwrap();
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
            ["chibi", "--config", "/c.toml", "run"],
            ["chibi", "run", "--config", "/c.toml"],
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
            Command::Stop {
                network: Some(Network::Mainnet)
            }
        ));
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
        let error = run::config(&path, flag.is_some())
            .unwrap_err()
            .to_string();
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
