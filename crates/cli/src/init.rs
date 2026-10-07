//! `chibi init`: asks for whatever the flags leave out and writes the config. On a terminal the
//! questions are interactive screens; otherwise they are plain lines, so scripts can answer them.

use std::ffi::OsString;
use std::fmt;
use std::io::{self, BufRead, IsTerminal, Write};
use std::net::TcpStream;
use std::num::NonZeroU16;
use std::path::{Path, PathBuf};
use std::time::Duration;

use alloy_primitives::Address;
use chibi_node::disk::Reset;
use chibi_node::{Disk, Network};
use clap::Args;
use inquire::validator::Validation;
use inquire::{Confirm, CustomType, InquireError, Select, Text};

use crate::config::{
    Config, DEFAULT_RPC_PORT, InvalidRpcHost, RpcHost, parse_address, process_env,
};

/// How long the port check waits for a listener to accept.
const PORT_PROBE_TIMEOUT: Duration = Duration::from_millis(300);

/// Answers given on the command line; whatever is left out is asked for.
#[derive(Debug, Default, Args)]
pub struct InitArgs {
    /// Network to follow (sepolia or mainnet).
    #[arg(long)]
    pub network: Option<Network>,
    /// Watch-only address to track; repeat for several.
    #[arg(long = "address", value_name = "ADDRESS", value_parser = parse_address)]
    pub addresses: Vec<Address>,
    /// IP address or domain wallets use to reach chibi; chibi listens on what it resolves to.
    #[arg(long)]
    pub rpc_host: Option<RpcHost>,
    /// JSON-RPC port.
    #[arg(long)]
    pub rpc_port: Option<NonZeroU16>,
    /// Directory to keep per-network data in instead of $XDG_DATA_HOME/chibi.
    #[arg(long, value_name = "PATH", value_parser = absolute_path)]
    pub data_dir: Option<PathBuf>,
}

/// `text` as an absolute path, resolved against the working directory.
fn absolute_path(text: &str) -> Result<PathBuf, String> {
    std::path::absolute(text).map_err(|err| format!("cannot resolve {text:?}: {err}"))
}

/// Why `chibi init` wrote nothing.
#[derive(Debug, thiserror::Error)]
pub enum InitError {
    #[error(transparent)]
    Io(#[from] io::Error),
    #[error("input ended before {0}")]
    Ended(&'static str),
    #[error("init cancelled; nothing was written")]
    Cancelled,
    #[error(transparent)]
    Host(#[from] InvalidRpcHost),
    #[error("prompt failed: {0}")]
    Prompt(InquireError),
}

impl From<InquireError> for InitError {
    fn from(error: InquireError) -> Self {
        match error {
            InquireError::OperationCanceled | InquireError::OperationInterrupted => Self::Cancelled,
            InquireError::IO(error) => Self::Io(error),
            other @ (InquireError::NotTTY
            | InquireError::InvalidConfiguration(_)
            | InquireError::Custom(_)) => Self::Prompt(other),
        }
    }
}

/// Asks one question per missing answer.
trait Ask {
    fn overwrite(&mut self, path: &Path) -> Result<bool, InitError>;
    fn network(&mut self) -> Result<Network, InitError>;
    fn addresses(&mut self) -> Result<Vec<Address>, InitError>;
    fn rpc_host(&mut self) -> Result<RpcHost, InitError>;
    fn rpc_port(&mut self, host: &RpcHost) -> Result<NonZeroU16, InitError>;
}

/// Writes the config at `path`. `config_flag` is the `--config` the user passed, repeated in
/// the commands the final message suggests.
pub fn init(args: InitArgs, path: &Path, config_flag: Option<&Path>) -> eyre::Result<()> {
    let config = if io::stdin().is_terminal() && io::stdout().is_terminal() {
        collect(&mut Screens, args, path)?
    } else {
        let mut lines = Lines {
            input: io::stdin().lock(),
            output: io::stdout().lock(),
        };
        collect(&mut lines, args, path)?
    };
    let mut output = io::stdout().lock();
    match config {
        Some((config, carried)) => {
            apply(
                &mut output,
                path,
                &config,
                &carried,
                config_flag,
                process_env,
            )?;
        }
        None => writeln!(output, "kept the existing config at {}", path.display())?,
    }
    Ok(())
}

/// Clears the saved state of the config's network, then writes the config to `path` and
/// says what changed. Refuses, writing nothing, while a chibi runs on that network's data
/// directory.
fn apply<W: Write>(
    output: &mut W,
    path: &Path,
    config: &Config,
    carried: &Carried,
    config_flag: Option<&Path>,
    env: impl Fn(&str) -> Option<OsString>,
) -> eyre::Result<()> {
    let dir = config.network_dir(config.network, env)?;
    if let Some(holder) = Disk::running(&dir)? {
        eyre::bail!(
            "chibi is running on {} (pid {}); stop it before rerunning chibi init. Nothing was written",
            dir.display(),
            holder.pid
        );
    }
    if let Some(reset) = Disk::reset(&dir)? {
        write_reset(output, config.network, &dir, reset)?;
    }
    config.save(path)?;
    write_next_steps(output, path, config, carried, config_flag)?;
    Ok(())
}

fn write_reset<W: Write>(
    output: &mut W,
    network: Network,
    dir: &Path,
    reset: Reset,
) -> io::Result<()> {
    let count = |n: usize, noun: &str| {
        if n == 1 {
            format!("1 {noun}")
        } else {
            format!("{n} {noun}s")
        }
    };
    writeln!(
        output,
        "reset the saved {network} state in {}: deleted {}, {} and {}; kept remembered state peers, the light-client checkpoint and devp2p.key",
        dir.join("chibi.db").display(),
        count(reset.snapshot, "snapshot"),
        count(reset.outbox, "pending transaction"),
        count(reset.keys, "remembered key"),
    )
}

/// What an overwritten config passed on to its replacement, since init never asks for it.
#[derive(Debug, Default, PartialEq, Eq)]
struct Carried {
    data_dir: Option<PathBuf>,
    contracts: usize,
}

/// The config to write and what it carried over, or `None` when the user keeps the existing
/// one. An existing config that parses passes on its `[[contracts]]` and, unless `--data-dir`
/// is given, its `data_dir`; one that does not parse is replaced outright.
fn collect<A: Ask>(
    ask: &mut A,
    args: InitArgs,
    path: &Path,
) -> Result<Option<(Config, Carried)>, InitError> {
    let existing = if path.exists() {
        if !ask.overwrite(path)? {
            return Ok(None);
        }
        Config::load(path).ok().flatten()
    } else {
        None
    };
    let network = match args.network {
        Some(network) => network,
        None => ask.network()?,
    };
    let addresses = if args.addresses.is_empty() {
        ask.addresses()?
    } else {
        unique(args.addresses)
    };
    let rpc_host = match args.rpc_host {
        Some(host) => host,
        None => ask.rpc_host()?,
    };
    let rpc_port = match args.rpc_port {
        Some(port) => port,
        None => ask.rpc_port(&rpc_host)?,
    };
    let mut config = Config::new(network, addresses);
    config.rpc_host = rpc_host;
    config.rpc_port = rpc_port;
    config.data_dir = args.data_dir;
    let mut carried = Carried::default();
    if let Some(existing) = existing {
        if config.data_dir.is_none() {
            carried.data_dir.clone_from(&existing.data_dir);
            config.data_dir = existing.data_dir;
        }
        carried.contracts = existing.contracts.len();
        config.contracts = existing.contracts;
    }
    Ok(Some((config, carried)))
}

/// Interactive terminal screens.
struct Screens;

impl Ask for Screens {
    fn overwrite(&mut self, path: &Path) -> Result<bool, InitError> {
        let question = format!("{} already exists. Overwrite it?", path.display());
        Ok(Confirm::new(&question).with_default(false).prompt()?)
    }

    fn network(&mut self) -> Result<Network, InitError> {
        let default = Network::ALL
            .iter()
            .position(|network| *network == Network::default())
            .unwrap_or(0);
        Ok(Select::new("Network", Network::ALL.to_vec())
            .with_starting_cursor(default)
            .prompt()?)
    }

    fn addresses(&mut self) -> Result<Vec<Address>, InitError> {
        let mut addresses = Vec::new();
        loop {
            let taken = addresses.clone();
            let help = if addresses.is_empty() {
                "0x followed by 40 hex digits, or Enter for none"
            } else {
                "another address, or Enter to finish"
            };
            let answer = Text::new("Watch-only address")
                .with_help_message(help)
                .with_validator(move |text: &str| {
                    Ok(match address_answer(text.trim(), &taken) {
                        AddressAnswer::Retry(message) => Validation::Invalid(message.into()),
                        AddressAnswer::Finish | AddressAnswer::Add(_) => Validation::Valid,
                    })
                })
                .prompt()?;
            match address_answer(answer.trim(), &addresses) {
                AddressAnswer::Finish => return Ok(addresses),
                AddressAnswer::Add(address) => addresses.push(address),
                AddressAnswer::Retry(_) => {}
            }
        }
    }

    fn rpc_host(&mut self) -> Result<RpcHost, InitError> {
        let default = RpcHost::default().to_string();
        let answer = Text::new("RPC host")
            .with_default(&default)
            .with_help_message("IP address or domain your wallet uses to reach chibi")
            .with_validator(|text: &str| {
                Ok(match text.parse::<RpcHost>() {
                    Ok(_) => Validation::Valid,
                    Err(invalid) => Validation::Invalid(invalid.to_string().into()),
                })
            })
            .prompt()?;
        Ok(answer.parse()?)
    }

    fn rpc_port(&mut self, host: &RpcHost) -> Result<NonZeroU16, InitError> {
        loop {
            let port = CustomType::<NonZeroU16>::new("RPC port")
                .with_default(DEFAULT_RPC_PORT)
                .with_error_message("Enter a port from 1 to 65535")
                .prompt()?;
            if !port_in_use(host, port) {
                return Ok(port);
            }
            let question = format!("Something already listens on port {port}. Use it anyway?");
            if Confirm::new(&question).with_default(false).prompt()? {
                return Ok(port);
            }
        }
    }
}

/// Whether something accepts connections on `port` at `host`.
fn port_in_use(host: &RpcHost, port: NonZeroU16) -> bool {
    host.bind_addr(port)
        .is_ok_and(|addr| TcpStream::connect_timeout(&addr, PORT_PROBE_TIMEOUT).is_ok())
}

/// Plain line prompts.
struct Lines<R, W> {
    input: R,
    output: W,
}

impl<R: BufRead, W: Write> Ask for Lines<R, W> {
    fn overwrite(&mut self, path: &Path) -> Result<bool, InitError> {
        let question = format!("{} already exists. Overwrite it?", path.display());
        Ok(confirm(&mut self.input, &mut self.output, &question)?)
    }

    fn network(&mut self) -> Result<Network, InitError> {
        prompt_network(&mut self.input, &mut self.output)
    }

    fn addresses(&mut self) -> Result<Vec<Address>, InitError> {
        prompt_addresses(&mut self.input, &mut self.output)
    }

    fn rpc_host(&mut self) -> Result<RpcHost, InitError> {
        prompt_rpc_host(&mut self.input, &mut self.output)
    }

    fn rpc_port(&mut self, _host: &RpcHost) -> Result<NonZeroU16, InitError> {
        prompt_rpc_port(&mut self.input, &mut self.output)
    }
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
) -> Result<Network, InitError> {
    let default = Network::default();
    loop {
        let answer = ask(input, output, format_args!("Network [{default}]: "))?
            .ok_or(InitError::Ended("a network was chosen"))?;
        if answer.is_empty() {
            return Ok(default);
        }
        match answer.to_ascii_lowercase().parse() {
            Ok(network) => return Ok(network),
            Err(unknown) => writeln!(output, "{unknown}")?,
        }
    }
}

/// What one answer to the address question means, given the addresses already entered.
enum AddressAnswer {
    Finish,
    Add(Address),
    Retry(String),
}

fn address_answer(text: &str, taken: &[Address]) -> AddressAnswer {
    if text.is_empty() {
        return AddressAnswer::Finish;
    }
    match parse_address(text) {
        Ok(address) if taken.contains(&address) => {
            AddressAnswer::Retry(format!("{address} is already on the list."))
        }
        Ok(address) => AddressAnswer::Add(address),
        Err(invalid) => AddressAnswer::Retry(invalid.to_string()),
    }
}

fn prompt_addresses<R: BufRead, W: Write>(
    input: &mut R,
    output: &mut W,
) -> Result<Vec<Address>, InitError> {
    let mut addresses = Vec::new();
    loop {
        let answer = ask(
            input,
            output,
            format_args!("Watch-only address (blank to finish): "),
        )?;
        let Some(answer) = answer else {
            return Ok(addresses);
        };
        match address_answer(&answer, &addresses) {
            AddressAnswer::Finish => return Ok(addresses),
            AddressAnswer::Add(address) => addresses.push(address),
            AddressAnswer::Retry(message) => writeln!(output, "{message}")?,
        }
    }
}

/// Reads the RPC host. Blank or ended input keeps the default.
fn prompt_rpc_host<R: BufRead, W: Write>(
    input: &mut R,
    output: &mut W,
) -> Result<RpcHost, InitError> {
    let default = RpcHost::default();
    loop {
        let prompt = format_args!("RPC host, an IP address or domain [{default}]: ");
        let answer = ask(input, output, prompt)?;
        match answer.as_deref() {
            None | Some("") => return Ok(default),
            Some(text) => match text.parse() {
                Ok(host) => return Ok(host),
                Err(invalid) => writeln!(output, "{invalid}")?,
            },
        }
    }
}

/// Reads the RPC port. Blank or ended input keeps the default.
fn prompt_rpc_port<R: BufRead, W: Write>(
    input: &mut R,
    output: &mut W,
) -> Result<NonZeroU16, InitError> {
    loop {
        let answer = ask(
            input,
            output,
            format_args!("RPC port [{DEFAULT_RPC_PORT}]: "),
        )?;
        match answer.as_deref() {
            None | Some("") => return Ok(DEFAULT_RPC_PORT),
            Some(text) => match text.parse() {
                Ok(port) => return Ok(port),
                Err(_) => writeln!(output, "{text:?} is not a port; enter 1 to 65535")?,
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

fn write_next_steps<W: Write>(
    output: &mut W,
    path: &Path,
    config: &Config,
    carried: &Carried,
    config_flag: Option<&Path>,
) -> io::Result<()> {
    let count = config.addresses.len();
    let plural = if count == 1 { "" } else { "es" };
    writeln!(
        output,
        "wrote {} ({}, {count} address{plural})",
        path.display(),
        config.network
    )?;
    if let Some(dir) = &carried.data_dir {
        writeln!(
            output,
            "kept data_dir = {} from the existing config",
            dir.display()
        )?;
    }
    if carried.contracts > 0 {
        let entries = if carried.contracts == 1 {
            "entry"
        } else {
            "entries"
        };
        writeln!(
            output,
            "kept {} [[contracts]] {entries} from the existing config",
            carried.contracts
        )?;
    }
    let flag = config_flag
        .map(|path| format!(" --config {}", path.display()))
        .unwrap_or_default();
    writeln!(output)?;
    writeln!(output, "Next steps:")?;
    writeln!(
        output,
        "  1. Start chibi with `chibi run{flag}` (foreground) or `chibi run -b{flag}` (background)."
    )?;
    writeln!(
        output,
        "  2. In your wallet, add a custom network with RPC URL {} and chain id {}.",
        config.rpc_url(),
        config.network.spec().chain_id()
    )?;
    writeln!(
        output,
        "  3. Check on it with `chibi status{flag}`; stop it with `chibi stop{flag}`."
    )
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;
    use std::path::PathBuf;

    use alloy_primitives::address;

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

    fn port(port: u16) -> NonZeroU16 {
        NonZeroU16::new(port).unwrap()
    }

    #[tokio::test]
    async fn writing_resets_the_networks_saved_state_and_refuses_while_chibi_runs_there() {
        use chibi_core::testing::{WorldAccount, eth};
        use chibi_node::fixture::Fixture;
        use chibi_node::{BlockSel, Disk, Node, Stepped, Watchlist, read_snapshot};

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let mut config = Config::new(Network::Mainnet, vec![ALICE]);
        config.data_dir = Some(dir.path().join("data"));
        let network_dir = dir.path().join("data").join("mainnet");
        let write = |config: &Config| {
            let mut output = Vec::new();
            apply(
                &mut output,
                &path,
                config,
                &Carried::default(),
                None,
                |_| None,
            )
            .map(|()| String::from_utf8(output).unwrap())
        };

        let fixture = Fixture::genesis([
            (ALICE, WorldAccount::eoa(eth(2))),
            (BOB, WorldAccount::eoa(eth(3))),
        ]);
        {
            let (node, mut follower) = Node::with_parts(
                Fixture::spec(&[]),
                fixture.peers(),
                fixture.heads(),
                Disk::open(&network_dir).unwrap(),
                &Watchlist {
                    addresses: vec![ALICE],
                    contracts: Vec::new(),
                },
            )
            .await
            .unwrap();
            assert_eq!(follower.step().await.unwrap(), Stepped::Current);
            node.account(BOB, BlockSel::Latest).await.unwrap();
            node.save_keys().await.unwrap();

            let error = write(&config).unwrap_err().to_string();
            assert_eq!(
                error,
                format!(
                    "chibi is running on {} (pid {}); stop it before rerunning chibi init. Nothing was written",
                    network_dir.display(),
                    std::process::id()
                )
            );
            assert!(!path.exists());
        }
        assert!(read_snapshot(&network_dir).unwrap().is_some());

        let output = write(&config).unwrap();
        assert!(
            output.starts_with(&format!(
                "reset the saved mainnet state in {}: deleted 1 snapshot, 0 pending transactions \
                 and 1 remembered key; kept remembered state peers, the light-client checkpoint \
                 and devp2p.key\n\
                 wrote {} (mainnet, 1 address)\n",
                network_dir.join("chibi.db").display(),
                path.display()
            )),
            "{output}"
        );
        assert_eq!(read_snapshot(&network_dir).unwrap(), None);
        assert_eq!(Config::load(&path).unwrap(), Some(config.clone()));

        config.data_dir = Some(dir.path().join("fresh"));
        let output = write(&config).unwrap();
        assert!(
            output.starts_with(&format!("wrote {} (mainnet, 1 address)\n", path.display())),
            "{output}"
        );
        assert!(!output.contains("reset"), "{output}");
        assert!(!dir.path().join("fresh").exists());
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
        assert!(matches!(network, Err(InitError::Ended(_))));
    }

    #[test]
    fn address_prompt_validates_and_dedupes() {
        let script = "0x1234\n\
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
                "{prompt}\"0x1234\" is not an address (expected 0x followed by 40 hex digits)\n\
                 {prompt}\"0x8f3Cf7ad23Cd3CaDbD9735AFf958023239c6a063\" has an invalid checksum\n\
                 {prompt}\
                 {prompt}{ALICE} is already on the list.\n\
                 {prompt}\
                 {prompt}"
            )
        );
    }

    #[test]
    fn address_prompt_ends_with_input() {
        let (addresses, _) = scripted("0x00000000000000000000000000000000000000b0\n", |i, o| {
            prompt_addresses(i, o)
        });
        assert_eq!(addresses.unwrap(), vec![BOB]);
    }

    #[test]
    fn init_accepts_no_watched_addresses() {
        for script in ["\n", ""] {
            let (addresses, _) = scripted(script, |i, o| prompt_addresses(i, o));
            assert_eq!(addresses.unwrap(), Vec::<Address>::new(), "{script:?}");
        }

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let mut lines = Lines {
            input: Cursor::new(&b"\n\n\n\n"[..]),
            output: Vec::new(),
        };
        let (config, _) = collect(&mut lines, InitArgs::default(), &path)
            .unwrap()
            .unwrap();
        assert_eq!(config, Config::new(Network::Sepolia, Vec::new()));
        let mut output = Vec::new();
        write_next_steps(&mut output, &path, &config, &Carried::default(), None).unwrap();
        let output = String::from_utf8(output).unwrap();
        assert!(
            output.starts_with(&format!(
                "wrote {} (sepolia, 0 addresses)\n",
                path.display()
            )),
            "{output}"
        );
    }

    #[test]
    fn host_prompt_defaults_and_reprompts_on_invalid() {
        for script in ["\n", ""] {
            let (host, output) = scripted(script, |i, o| prompt_rpc_host(i, o));
            assert_eq!(host.unwrap(), RpcHost::default(), "{script:?}");
            assert!(output.starts_with("RPC host, an IP address or domain [127.0.0.1]: "));
        }
        let (host, output) = scripted("chibi_local\nChibi.Local\n", |i, o| prompt_rpc_host(i, o));
        assert_eq!(host.unwrap(), RpcHost::Name("chibi.local".to_owned()));
        let prompt = "RPC host, an IP address or domain [127.0.0.1]: ";
        assert_eq!(
            output,
            format!("{prompt}\"chibi_local\" is not an IP address or host name\n{prompt}")
        );
    }

    #[test]
    fn port_prompt_defaults_and_reprompts_on_invalid() {
        for script in ["\n", ""] {
            let (chosen, _) = scripted(script, |i, o| prompt_rpc_port(i, o));
            assert_eq!(chosen.unwrap(), port(8545), "{script:?}");
        }
        let (chosen, output) = scripted("0\n70000\nabc\n8745\n", |i, o| prompt_rpc_port(i, o));
        assert_eq!(chosen.unwrap(), port(8745));
        let prompt = "RPC port [8545]: ";
        assert_eq!(
            output,
            format!(
                "{prompt}\"0\" is not a port; enter 1 to 65535\n\
                 {prompt}\"70000\" is not a port; enter 1 to 65535\n\
                 {prompt}\"abc\" is not a port; enter 1 to 65535\n\
                 {prompt}"
            )
        );
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
    fn plain_init_asks_only_for_what_flags_leave_out() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let mut lines = Lines {
            input: Cursor::new(
                "0x00000000000000000000000000000000000000b0\n\nchibi.local\n".as_bytes(),
            ),
            output: Vec::new(),
        };
        let args = InitArgs {
            network: Some(Network::Mainnet),
            rpc_port: Some(port(9545)),
            ..InitArgs::default()
        };
        let (config, carried) = collect(&mut lines, args, &path).unwrap().unwrap();
        assert_eq!(carried, Carried::default());
        assert_eq!(config.network, Network::Mainnet);
        assert_eq!(config.addresses, vec![BOB]);
        assert_eq!(config.rpc_url(), "http://chibi.local:9545");
        let output = String::from_utf8(lines.output).unwrap();
        assert!(!output.contains("Network"), "{output}");
        assert!(!output.contains("RPC port"), "{output}");
    }

    #[test]
    fn flag_addresses_are_deduplicated_in_order() {
        assert_eq!(unique(vec![BOB, ALICE, BOB, ALICE]), vec![BOB, ALICE]);
    }

    #[test]
    fn data_dir_and_beacon_url_flags_are_written_only_when_given() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let answered = |keys: ConfigArgs| {
            let mut lines = Lines {
                input: Cursor::new(&b""[..]),
                output: Vec::new(),
            };
            let args = InitArgs {
                network: Some(Network::Sepolia),
                addresses: vec![ALICE],
                rpc_host: Some(RpcHost::default()),
                rpc_port: Some(port(8545)),
                keys,
            };
            collect(&mut lines, args, &path).unwrap().unwrap().0
        };

        let plain = answered(ConfigArgs::default());
        plain.save(&path).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(!text.contains("data_dir"), "{text}");
        assert!(!text.contains("beacon_url"), "{text}");
        std::fs::remove_file(&path).unwrap();

        let custom = answered(ConfigArgs {
            beacon_url: Some("http://127.0.0.1:5052".parse().unwrap()),
            data_dir: Some(dir.path().join("data")),
            ..ConfigArgs::default()
        });
        assert_eq!(custom.data_dir, Some(dir.path().join("data")));
        custom.save(&path).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(
            text.contains("beacon_url = \"http://127.0.0.1:5052/\""),
            "{text}"
        );
        assert!(
            text.contains(&format!("data_dir = {:?}", dir.path().join("data"))),
            "{text}"
        );
        assert_eq!(Config::load(&path).unwrap(), Some(custom));
    }

    #[test]
    fn overwrite_keeps_data_dir_and_contracts_unless_replaced() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let old_dir = dir.path().join("old");
        let contracts = vec![chibi_node::TrackedContract {
            address: BOB,
            slots: vec![alloy_primitives::U256::from(1)],
        }];
        let mut existing = Config::new(Network::Mainnet, vec![BOB]);
        existing.data_dir = Some(old_dir.clone());
        existing.contracts.clone_from(&contracts);
        let overwrite = |data_dir: Option<PathBuf>| {
            let mut lines = Lines {
                input: Cursor::new(&b"y\n"[..]),
                output: Vec::new(),
            };
            let args = InitArgs {
                network: Some(Network::Sepolia),
                addresses: vec![ALICE],
                rpc_host: Some(RpcHost::default()),
                rpc_port: Some(port(8545)),
                keys: ConfigArgs {
                    data_dir,
                    ..ConfigArgs::default()
                },
            };
            collect(&mut lines, args, &path).unwrap().unwrap()
        };

        existing.save(&path).unwrap();
        let (config, carried) = overwrite(None);
        assert_eq!(config.addresses, vec![ALICE]);
        assert_eq!(config.data_dir, Some(old_dir.clone()));
        assert_eq!(config.contracts, contracts);
        assert_eq!(config.network, Network::Sepolia);
        assert_eq!(
            carried,
            Carried {
                data_dir: Some(old_dir.clone()),
                contracts: 1,
            }
        );
        let mut output = Vec::new();
        write_next_steps(&mut output, &path, &config, &carried, None).unwrap();
        let output = String::from_utf8(output).unwrap();
        assert_eq!(
            output,
            format!(
                "wrote {} (sepolia, 1 address)\n\
                 kept data_dir = {} from the existing config\n\
                 kept 1 [[contracts]] entry from the existing config\n\
                 \n\
                 Next steps:\n\
                 \x20 1. Start chibi with `chibi run` (foreground) or `chibi run -b` (background).\n\
                 \x20 2. In your wallet, add a custom network with RPC URL http://127.0.0.1:8545 and chain id 11155111.\n\
                 \x20 3. Check on it with `chibi status`; stop it with `chibi stop`.\n",
                path.display(),
                old_dir.display()
            )
        );

        let new_dir = dir.path().join("new");
        let (config, carried) = overwrite(Some(new_dir.clone()));
        assert_eq!(config.data_dir, Some(new_dir));
        assert_eq!(config.contracts, contracts);
        assert_eq!(
            carried,
            Carried {
                data_dir: None,
                contracts: 1,
            }
        );

        std::fs::write(&path, "not = [valid").unwrap();
        let (config, carried) = overwrite(None);
        assert_eq!(config.data_dir, None);
        assert!(config.contracts.is_empty());
        assert_eq!(carried, Carried::default());
    }

    #[test]
    fn next_steps_give_the_url_and_how_to_start() {
        let mut output = Vec::new();
        let mut config = Config::new(Network::Sepolia, vec![ALICE]);
        config.rpc_host = "chibi.local".parse().unwrap();
        config.rpc_port = port(8745);
        write_next_steps(
            &mut output,
            Path::new("/cfg/config.toml"),
            &config,
            &Carried::default(),
            None,
        )
        .unwrap();
        assert_eq!(
            String::from_utf8(output).unwrap(),
            "wrote /cfg/config.toml (sepolia, 1 address)\n\
             \n\
             Next steps:\n\
             \x20 1. Start chibi with `chibi run` (foreground) or `chibi run -b` (background).\n\
             \x20 2. In your wallet, add a custom network with RPC URL http://chibi.local:8745 and chain id 11155111.\n\
             \x20 3. Check on it with `chibi status`; stop it with `chibi stop`.\n"
        );
    }

    #[test]
    fn next_steps_repeat_a_custom_config_path() {
        let mut output = Vec::new();
        let config = Config::new(Network::Mainnet, vec![ALICE, BOB]);
        let custom = Path::new("/tmp/c.toml");
        write_next_steps(
            &mut output,
            custom,
            &config,
            &Carried::default(),
            Some(custom),
        )
        .unwrap();
        let output = String::from_utf8(output).unwrap();
        assert_eq!(
            output,
            "wrote /tmp/c.toml (mainnet, 2 addresses)\n\
             \n\
             Next steps:\n\
             \x20 1. Start chibi with `chibi run --config /tmp/c.toml` (foreground) or `chibi run -b --config /tmp/c.toml` (background).\n\
             \x20 2. In your wallet, add a custom network with RPC URL http://127.0.0.1:8545 and chain id 1.\n\
             \x20 3. Check on it with `chibi status --config /tmp/c.toml`; stop it with `chibi stop --config /tmp/c.toml`.\n"
        );
    }
}
