//! The `chibi` binary: `init`, `run`, and `status`.

#![allow(
    clippy::print_stdout,
    clippy::print_stderr,
    reason = "the CLI's user-facing output belongs on the terminal"
)]

mod config;
mod rpc;

use std::fmt;
use std::io::{self, BufRead, Write};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use alloy_primitives::{Address, U256};
use chibi_node::{Network, Node, NodeConfig, Snapshot, Token, read_snapshot};
use clap::{Parser, Subcommand};
use eyre::WrapErr;
use tracing::info;
use tracing_subscriber::EnvFilter;

use crate::config::{Config, data_dir, default_config_path, parse_address};
use crate::rpc::RpcConfig;

/// Decimal places of one ether.
const ETH_DECIMALS: u8 = 18;

#[derive(Debug, Parser)]
#[command(
    name = "chibi",
    version,
    about = "A local Ethereum JSON-RPC endpoint for browser wallets that verifies everything it serves"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Write the config file, asking for whatever the flags leave out.
    Init {
        /// Network to follow (sepolia or mainnet); prompted for when omitted.
        #[arg(long)]
        network: Option<Network>,
        /// Watch-only address to track; repeat for several. Prompted for when omitted.
        #[arg(long = "address", value_name = "ADDRESS", value_parser = parse_address)]
        addresses: Vec<Address>,
        /// Config file to write instead of the default location.
        #[arg(long)]
        config: Option<PathBuf>,
    },
    /// Follow the chain and serve JSON-RPC to the wallet.
    Run {
        /// Network to follow instead of the configured one.
        #[arg(long)]
        network: Option<Network>,
        /// Address to serve JSON-RPC on instead of the configured one.
        #[arg(long)]
        rpc_addr: Option<SocketAddr>,
        /// Config file to read instead of the default location.
        #[arg(long)]
        config: Option<PathBuf>,
    },
    /// Show the balances saved at the last verified head, without starting a node.
    Status {
        /// Config file to read instead of the default location.
        #[arg(long)]
        config: Option<PathBuf>,
    },
}

fn main() -> eyre::Result<()> {
    match Cli::parse().command {
        Command::Init {
            network,
            addresses,
            config,
        } => init(network, addresses, config),
        Command::Run {
            network,
            rpc_addr,
            config,
        } => run(network, rpc_addr, config),
        Command::Status { config } => status(config),
    }
}

fn config_path(path: Option<PathBuf>) -> eyre::Result<PathBuf> {
    match path {
        Some(path) => Ok(path),
        None => Ok(default_config_path()?),
    }
}

fn init(
    network: Option<Network>,
    addresses: Vec<Address>,
    path: Option<PathBuf>,
) -> eyre::Result<()> {
    let path = config_path(path)?;
    let mut input = io::stdin().lock();
    let mut output = io::stdout().lock();
    if path.exists() {
        let question = format!("{} already exists. Overwrite it?", path.display());
        if !confirm(&mut input, &mut output, &question)? {
            writeln!(output, "kept the existing config at {}", path.display())?;
            return Ok(());
        }
    }
    let network = match network {
        Some(network) => network,
        None => prompt_network(&mut input, &mut output)?,
    };
    let addresses = if addresses.is_empty() {
        prompt_addresses(&mut input, &mut output)?
    } else {
        unique(addresses)
    };
    let config = Config::new(network, addresses);
    config.save(&path)?;
    write_next_steps(&mut output, &path, &config)?;
    Ok(())
}

fn run(
    network: Option<Network>,
    rpc_addr: Option<SocketAddr>,
    path: Option<PathBuf>,
) -> eyre::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();
    let mut config = Config::load(&config_path(path)?)?;
    if let Some(network) = network {
        config.network = network;
    }
    if let Some(rpc_addr) = rpc_addr {
        config.rpc_addr = rpc_addr;
    }
    let node_config = config.node_config(data_dir(config.network)?);
    tokio::runtime::Runtime::new()?.block_on(serve(node_config, config.rpc()))
}

async fn serve(node_config: NodeConfig, rpc_config: RpcConfig) -> eyre::Result<()> {
    info!(
        network = %node_config.network,
        watched = node_config.watch.addresses.len(),
        tokens = node_config.watch.tokens.len(),
        data_dir = %node_config.data_dir.display(),
        "starting"
    );
    let (node, follower) = Node::start(node_config).await?;
    tokio::select! {
        result = follower.run() => result.wrap_err("head follower stopped"),
        result = rpc::serve(node, rpc_config) => result.wrap_err("JSON-RPC server stopped"),
        result = tokio::signal::ctrl_c() => {
            result?;
            info!("shutting down");
            Ok(())
        }
    }
}

fn status(path: Option<PathBuf>) -> eyre::Result<()> {
    let config = Config::load(&config_path(path)?)?;
    match read_snapshot(&data_dir(config.network)?)? {
        Some(snapshot) => {
            let spec = config.network.spec();
            write_status(&mut io::stdout().lock(), &snapshot, spec.tokens, unix_now())?;
        }
        None => println!(
            "no snapshot for {} yet; it is saved once `chibi run` verifies its first head",
            config.network
        ),
    }
    Ok(())
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs())
}

/// Input ended before a required answer.
#[derive(Debug, thiserror::Error)]
enum PromptError {
    #[error(transparent)]
    Io(#[from] io::Error),
    #[error("input ended before {0}")]
    Ended(&'static str),
}

/// Prints `prompt` and reads one trimmed line; `None` once input is exhausted.
fn ask<R: BufRead, W: Write>(
    input: &mut R,
    output: &mut W,
    prompt: fmt::Arguments<'_>,
) -> io::Result<Option<String>> {
    output.write_fmt(prompt)?;
    output.flush()?;
    let mut line = String::new();
    if input.read_line(&mut line)? == 0 {
        writeln!(output)?;
        return Ok(None);
    }
    Ok(Some(line.trim().to_owned()))
}

fn prompt_network<R: BufRead, W: Write>(
    input: &mut R,
    output: &mut W,
) -> Result<Network, PromptError> {
    let default = Network::default();
    loop {
        let answer = ask(input, output, format_args!("Network [{default}]: "))?
            .ok_or(PromptError::Ended("a network was chosen"))?;
        if answer.is_empty() {
            return Ok(default);
        }
        match answer.to_ascii_lowercase().parse() {
            Ok(network) => return Ok(network),
            Err(unknown) => writeln!(output, "{unknown}")?,
        }
    }
}

fn prompt_addresses<R: BufRead, W: Write>(
    input: &mut R,
    output: &mut W,
) -> Result<Vec<Address>, PromptError> {
    let mut addresses = Vec::new();
    loop {
        let answer = ask(
            input,
            output,
            format_args!("Watch-only address (blank to finish): "),
        )?;
        match answer.as_deref() {
            None if addresses.is_empty() => {
                return Err(PromptError::Ended("a watch-only address was entered"));
            }
            None => return Ok(addresses),
            Some("") if addresses.is_empty() => {
                writeln!(output, "Enter at least one address.")?;
            }
            Some("") => return Ok(addresses),
            Some(text) => match parse_address(text) {
                Ok(address) if addresses.contains(&address) => {
                    writeln!(output, "{address} is already on the list.")?;
                }
                Ok(address) => addresses.push(address),
                Err(invalid) => writeln!(output, "{invalid}")?,
            },
        }
    }
}

fn confirm<R: BufRead, W: Write>(
    input: &mut R,
    output: &mut W,
    question: &str,
) -> io::Result<bool> {
    let answer = ask(input, output, format_args!("{question} [y/N] "))?;
    Ok(answer.is_some_and(|answer| {
        answer.eq_ignore_ascii_case("y") || answer.eq_ignore_ascii_case("yes")
    }))
}

fn unique(addresses: Vec<Address>) -> Vec<Address> {
    let mut kept = Vec::with_capacity(addresses.len());
    for address in addresses {
        if !kept.contains(&address) {
            kept.push(address);
        }
    }
    kept
}

fn write_next_steps<W: Write>(output: &mut W, path: &Path, config: &Config) -> io::Result<()> {
    let spec = config.network.spec();
    let count = config.addresses.len();
    let plural = if count == 1 { "" } else { "es" };
    write!(
        output,
        "wrote {} ({}, {count} address{plural}",
        path.display(),
        config.network
    )?;
    for (i, token) in spec.token("USDC").iter().enumerate() {
        let lead = if i == 0 { ", tracking " } else { ", " };
        write!(output, "{lead}{}", token.symbol)?;
    }
    writeln!(output, ")")?;
    writeln!(output)?;
    writeln!(output, "Next steps:")?;
    writeln!(output, "  1. chibi run")?;
    writeln!(
        output,
        "  2. In your wallet, add a custom network with RPC URL http://{} and chain id {}.",
        config.rpc_addr,
        spec.chain_id()
    )
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
    use std::io::Cursor;

    use alloy_eips::BlockNumHash;
    use alloy_primitives::{B256, address};
    use chibi_node::Holding;

    use super::*;

    const ALICE: Address = address!("0x8f3Cf7ad23Cd3CaDbD9735AFf958023239c6A063");
    const BOB: Address = address!("0x00000000000000000000000000000000000000b0");

    fn scripted<T>(
        script: &str,
        prompt: impl FnOnce(&mut Cursor<&[u8]>, &mut Vec<u8>) -> T,
    ) -> (T, String) {
        let mut input = Cursor::new(script.as_bytes());
        let mut output = Vec::new();
        let result = prompt(&mut input, &mut output);
        (result, String::from_utf8(output).unwrap())
    }

    #[test]
    fn network_prompt_defaults_to_sepolia() {
        let (network, output) = scripted("\n", |i, o| prompt_network(i, o));
        assert_eq!(network.unwrap(), Network::Sepolia);
        assert_eq!(output, "Network [sepolia]: ");
    }

    #[test]
    fn network_prompt_reprompts_on_unknown_names() {
        let (network, output) = scripted("goerli\n  Mainnet \n", |i, o| prompt_network(i, o));
        assert_eq!(network.unwrap(), Network::Mainnet);
        assert_eq!(
            output,
            "Network [sepolia]: unknown network \"goerli\"; expected sepolia or mainnet\n\
             Network [sepolia]: "
        );
    }

    #[test]
    fn network_prompt_fails_when_input_ends() {
        let (network, _) = scripted("goerli\n", |i, o| prompt_network(i, o));
        assert!(matches!(network, Err(PromptError::Ended(_))));
    }

    #[test]
    fn address_prompt_validates_dedupes_and_requires_one() {
        let script = "\n\
                      0x1234\n\
                      0x8f3Cf7ad23Cd3CaDbD9735AFf958023239c6a063\n\
                      0x8f3Cf7ad23Cd3CaDbD9735AFf958023239c6A063\n\
                      0x8f3cf7ad23cd3cadbd9735aff958023239c6a063\n\
                      0x00000000000000000000000000000000000000b0\n\
                      \n";
        let (addresses, output) = scripted(script, |i, o| prompt_addresses(i, o));
        assert_eq!(addresses.unwrap(), vec![ALICE, BOB]);
        let prompt = "Watch-only address (blank to finish): ";
        assert_eq!(
            output,
            format!(
                "{prompt}Enter at least one address.\n\
                 {prompt}\"0x1234\" is not an address (expected 0x followed by 40 hex digits)\n\
                 {prompt}\"0x8f3Cf7ad23Cd3CaDbD9735AFf958023239c6a063\" has an invalid checksum\n\
                 {prompt}\
                 {prompt}{ALICE} is already on the list.\n\
                 {prompt}\
                 {prompt}"
            )
        );
    }

    #[test]
    fn address_prompt_ends_with_input_once_one_is_entered() {
        let (addresses, _) = scripted("0x00000000000000000000000000000000000000b0\n", |i, o| {
            prompt_addresses(i, o)
        });
        assert_eq!(addresses.unwrap(), vec![BOB]);

        let (none, _) = scripted("\n", |i, o| prompt_addresses(i, o));
        assert!(matches!(none, Err(PromptError::Ended(_))));
    }

    #[test]
    fn overwrite_needs_an_explicit_yes() {
        for (script, expected) in [("y\n", true), ("YES\n", true), ("\n", false), ("", false)] {
            let (answer, output) = scripted(script, |i, o| confirm(i, o, "Overwrite?"));
            assert_eq!(answer.unwrap(), expected, "{script:?}");
            assert!(output.starts_with("Overwrite? [y/N] "));
        }
    }

    #[test]
    fn flag_addresses_are_deduplicated_in_order() {
        assert_eq!(unique(vec![BOB, ALICE, BOB, ALICE]), vec![BOB, ALICE]);
    }

    #[test]
    fn next_steps_name_the_rpc_url_and_chain_id() {
        let mut output = Vec::new();
        let config = Config::new(Network::Sepolia, vec![ALICE]);
        write_next_steps(&mut output, Path::new("/cfg/config.toml"), &config).unwrap();
        assert_eq!(
            String::from_utf8(output).unwrap(),
            "wrote /cfg/config.toml (sepolia, 1 address, tracking USDC)\n\
             \n\
             Next steps:\n\
             \x20 1. chibi run\n\
             \x20 2. In your wallet, add a custom network with RPC URL http://127.0.0.1:8545 and chain id 11155111.\n"
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
    fn age_uses_largest_whole_unit() {
        let shown = [0, 59, 60, 3_599, 3_600, 86_399, 86_400, 200_000]
            .map(|seconds| Age(seconds).to_string());
        assert_eq!(shown, ["0s", "59s", "1m", "59m", "1h", "23h", "1d", "2d"]);
    }
}
