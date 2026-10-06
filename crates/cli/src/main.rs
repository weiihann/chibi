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

use std::fmt;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use alloy_primitives::U256;
use chibi_node::disk::Holder;
use chibi_node::{Disk, Network, Snapshot, Token, read_snapshot};
use clap::{Parser, Subcommand};
use eyre::WrapErr;

use crate::config::{Config, ConfigError, default_config_path, default_data_root, process_env};
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
    /// Config file to use instead of $XDG_CONFIG_HOME/chibi/config.toml.
    #[arg(long, global = true, env = "CHIBI_CONFIG", value_name = "PATH")]
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
    let named = cli
        .config
        .map(std::path::absolute)
        .transpose()
        .wrap_err("cannot resolve the --config path")?;
    let path = match &named {
        Some(path) => path.clone(),
        None => default_config_path(process_env)?,
    };
    match cli.command {
        Command::Init(answers) => init::init(answers, &path, named.as_deref()),
        Command::Run(args) => run::run(&args, &path, named.as_deref()),
        Command::Stop { network } => {
            let config = Config::load(&path)?;
            let network = network.unwrap_or(config.network);
            run::stop(network, &config.network_dir(network, process_env)?)
        }
        Command::Status { network } => status(network, &path),
    }
}

/// The network to show and its data directory. Without a config file, `--network` alone still
/// finds the default data directory.
fn status_target(network: Option<Network>, path: &Path) -> eyre::Result<(Network, PathBuf)> {
    match Config::load(path) {
        Ok(config) => {
            let network = network.unwrap_or(config.network);
            Ok((network, config.network_dir(network, process_env)?))
        }
        Err(ConfigError::Missing(missing)) => match network {
            Some(network) => Ok((
                network,
                default_data_root(process_env)?.join(network.name()),
            )),
            None => Err(ConfigError::Missing(missing).into()),
        },
        Err(err) => Err(err.into()),
    }
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
    tokens: &[Token],
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
        match holding.balance {
            Some(balance) => write!(output, "   {} ETH", format_units(balance, ETH_DECIMALS))?,
            None => write!(output, "   ? ETH")?,
        }
        for (symbol, amount) in &holding.tokens {
            let decimals = tokens
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
                    balance: Some(U256::from(1_203_400_000_000_000_000u128)),
                    nonce: Some(3),
                    tokens: vec![
                        ("USDC".to_owned(), Some(U256::from(250_000_000u64))),
                        ("XYZ".to_owned(), Some(U256::from(5u64))),
                    ],
                },
                Holding {
                    address: BOB,
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
                 {ALICE}   1.2034 ETH   250.00 USDC   5 XYZ (base units)\n\
                 {BOB}   ? ETH   ? USDC\n"
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
    fn age_uses_largest_whole_unit() {
        let shown = [0, 59, 60, 3_599, 3_600, 86_399, 86_400, 200_000]
            .map(|seconds| Age(seconds).to_string());
        assert_eq!(shown, ["0s", "59s", "1m", "59m", "1h", "23h", "1d", "2d"]);
    }
}
