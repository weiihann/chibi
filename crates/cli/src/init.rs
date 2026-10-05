//! `chibi init`: asks for whatever the flags leave out and writes the config. On a terminal the
//! questions are interactive screens; otherwise they are plain lines, so scripts can answer them.

use std::fmt;
use std::io::{self, BufRead, IsTerminal, Write};
use std::net::TcpStream;
use std::num::NonZeroU16;
use std::path::Path;
use std::time::Duration;

use alloy_primitives::Address;
use chibi_node::{Network, Token};
use clap::Args;
use inquire::validator::Validation;
use inquire::{Confirm, CustomType, InquireError, MultiSelect, Select, Text};

use crate::config::{
    Config, DEFAULT_RPC_PORT, DEFAULT_TOKENS, InvalidRpcHost, RpcHost, UnknownToken,
    default_tokens, parse_address, resolve_tokens,
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
    /// Token to track, by symbol from the network's registry; repeat for several.
    #[arg(long = "token", value_name = "SYMBOL")]
    pub tokens: Vec<String>,
    /// IP address or domain wallets use to reach chibi; chibi listens on what it resolves to.
    #[arg(long)]
    pub rpc_host: Option<RpcHost>,
    /// JSON-RPC port.
    #[arg(long)]
    pub rpc_port: Option<NonZeroU16>,
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
    Token(#[from] UnknownToken),
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
    fn tokens(&mut self, network: Network) -> Result<Vec<Token>, InitError>;
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
        Some(config) => {
            config.save(path)?;
            write_next_steps(&mut output, path, &config, config_flag)?;
        }
        None => writeln!(output, "kept the existing config at {}", path.display())?,
    }
    Ok(())
}

/// The config to write, or `None` when the user keeps the existing one.
fn collect<A: Ask>(ask: &mut A, args: InitArgs, path: &Path) -> Result<Option<Config>, InitError> {
    if path.exists() && !ask.overwrite(path)? {
        return Ok(None);
    }
    let network = match args.network {
        Some(network) => network,
        None => ask.network()?,
    };
    let flag_tokens = resolve_tokens(network, &args.tokens)?;
    let addresses = if args.addresses.is_empty() {
        ask.addresses()?
    } else {
        unique(args.addresses)
    };
    let tokens = if args.tokens.is_empty() {
        ask.tokens(network)?
    } else {
        flag_tokens
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
    config.tokens = tokens;
    config.rpc_host = rpc_host;
    config.rpc_port = rpc_port;
    Ok(Some(config))
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
                "0x followed by 40 hex digits"
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

    fn tokens(&mut self, network: Network) -> Result<Vec<Token>, InitError> {
        let choices: Vec<TokenChoice> = network
            .spec()
            .tokens
            .iter()
            .copied()
            .map(TokenChoice)
            .collect();
        let defaults: Vec<usize> = choices
            .iter()
            .enumerate()
            .filter(|(_, choice)| DEFAULT_TOKENS.contains(&choice.0.symbol))
            .map(|(index, _)| index)
            .collect();
        let chosen = MultiSelect::new("Tokens to track", choices)
            .with_default(&defaults)
            .with_help_message("space ticks, enter confirms; ETH is always tracked")
            .prompt()?;
        Ok(chosen.into_iter().map(|choice| choice.0).collect())
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

/// A registry token as a tick-box line.
#[derive(Clone, Copy)]
struct TokenChoice(Token);

impl fmt::Display for TokenChoice {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let Token {
            symbol,
            name,
            address,
            ..
        } = self.0;
        write!(f, "{symbol:<5} {name}  {address}")
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

    fn tokens(&mut self, network: Network) -> Result<Vec<Token>, InitError> {
        prompt_tokens(&mut self.input, &mut self.output, network)
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
        return if taken.is_empty() {
            AddressAnswer::Retry("Enter at least one address.".to_owned())
        } else {
            AddressAnswer::Finish
        };
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
            if addresses.is_empty() {
                return Err(InitError::Ended("a watch-only address was entered"));
            }
            return Ok(addresses);
        };
        match address_answer(&answer, &addresses) {
            AddressAnswer::Finish => return Ok(addresses),
            AddressAnswer::Add(address) => addresses.push(address),
            AddressAnswer::Retry(message) => writeln!(output, "{message}")?,
        }
    }
}

/// Lists the network's tokens and reads a comma-separated choice. Blank or ended input keeps
/// the default; `none` tracks no tokens.
fn prompt_tokens<R: BufRead, W: Write>(
    input: &mut R,
    output: &mut W,
    network: Network,
) -> Result<Vec<Token>, InitError> {
    writeln!(output, "Tokens on {network}:")?;
    for token in network.spec().tokens {
        writeln!(output, "  {}", TokenChoice(*token))?;
    }
    let default = DEFAULT_TOKENS.join(",");
    loop {
        let prompt = format_args!("Tokens to track, comma-separated or none [{default}]: ");
        let Some(answer) = ask(input, output, prompt)? else {
            return Ok(default_tokens(network));
        };
        if answer.is_empty() {
            return Ok(default_tokens(network));
        }
        if answer.eq_ignore_ascii_case("none") {
            return Ok(Vec::new());
        }
        let symbols: Vec<&str> = answer
            .split(',')
            .map(str::trim)
            .filter(|symbol| !symbol.is_empty())
            .collect();
        if symbols.is_empty() {
            writeln!(output, "Enter token symbols separated by commas, or none.")?;
            continue;
        }
        match resolve_tokens(network, &symbols) {
            Ok(tokens) => return Ok(tokens),
            Err(unknown) => writeln!(output, "{unknown}")?,
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
    config_flag: Option<&Path>,
) -> io::Result<()> {
    let count = config.addresses.len();
    let plural = if count == 1 { "" } else { "es" };
    write!(
        output,
        "wrote {} ({}, {count} address{plural}",
        path.display(),
        config.network
    )?;
    let symbols: Vec<&str> = config.tokens.iter().map(|token| token.symbol).collect();
    if symbols.is_empty() {
        writeln!(output, ", no tokens)")?;
    } else {
        writeln!(output, ", tracking {})", symbols.join(", "))?;
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

    fn symbols(tokens: &[Token]) -> Vec<&'static str> {
        tokens.iter().map(|token| token.symbol).collect()
    }

    fn port(port: u16) -> NonZeroU16 {
        NonZeroU16::new(port).unwrap()
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
        assert!(matches!(none, Err(InitError::Ended(_))));
    }

    #[test]
    fn token_prompt_lists_the_registry_and_defaults_to_usdc() {
        for script in ["\n", ""] {
            let (tokens, output) = scripted(script, |i, o| prompt_tokens(i, o, Network::Sepolia));
            assert_eq!(symbols(&tokens.unwrap()), ["USDC"], "{script:?}");
            assert!(
                output.starts_with(
                    "Tokens on sepolia:\n  \
                     USDC  USD Coin (Circle)  0x1c7D4B196Cb0C7B01d743Fbc6116a902379C7238\n"
                ),
                "{output}"
            );
            assert!(
                output.contains("  LINK  Chainlink  0x779877A7B0D9E8603169DdbD7836e478b4624789\n")
            );
            assert!(!output.contains("WBTC"));
        }
    }

    #[test]
    fn token_prompt_reprompts_on_unknown_symbols() {
        let (tokens, output) = scripted("usdc, wbtc\n , \n weth,usdc ,DAI\n", |i, o| {
            prompt_tokens(i, o, Network::Sepolia)
        });
        assert_eq!(symbols(&tokens.unwrap()), ["WETH", "USDC", "DAI"]);
        let prompt = "Tokens to track, comma-separated or none [USDC]: ";
        let answers = output.split_once(prompt).map(|(_, rest)| rest).unwrap();
        assert_eq!(
            answers,
            format!(
                "unknown token \"wbtc\" on sepolia; known tokens: USDC, USDT, DAI, WETH, LINK\n\
                 {prompt}Enter token symbols separated by commas, or none.\n\
                 {prompt}"
            )
        );

        let (mainnet, _) = scripted("WBTC\n", |i, o| prompt_tokens(i, o, Network::Mainnet));
        assert_eq!(symbols(&mainnet.unwrap()), ["WBTC"]);
        let (none, _) = scripted("None\n", |i, o| prompt_tokens(i, o, Network::Sepolia));
        assert!(none.unwrap().is_empty());
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
            tokens: vec!["wbtc".to_owned(), "Dai".to_owned()],
            rpc_port: Some(port(9545)),
            ..InitArgs::default()
        };
        let config = collect(&mut lines, args, &path).unwrap().unwrap();
        assert_eq!(config.network, Network::Mainnet);
        assert_eq!(config.addresses, vec![BOB]);
        assert_eq!(symbols(&config.tokens), ["WBTC", "DAI"]);
        assert_eq!(config.rpc_url(), "http://chibi.local:9545");
        let output = String::from_utf8(lines.output).unwrap();
        assert!(!output.contains("Network"), "{output}");
        assert!(!output.contains("Tokens"), "{output}");
        assert!(!output.contains("RPC port"), "{output}");
    }

    #[test]
    fn unknown_token_flag_fails_before_any_question() {
        let mut lines = Lines {
            input: Cursor::new(&b""[..]),
            output: Vec::new(),
        };
        let args = InitArgs {
            network: Some(Network::Sepolia),
            tokens: vec!["WBTC".to_owned()],
            ..InitArgs::default()
        };
        let error =
            collect(&mut lines, args, &PathBuf::from("/nonexistent/config.toml")).unwrap_err();
        assert!(matches!(error, InitError::Token(_)), "{error}");
        assert!(lines.output.is_empty());
    }

    #[test]
    fn flag_addresses_are_deduplicated_in_order() {
        assert_eq!(unique(vec![BOB, ALICE, BOB, ALICE]), vec![BOB, ALICE]);
    }

    #[test]
    fn next_steps_list_tokens_url_and_how_to_start() {
        let mut output = Vec::new();
        let mut config = Config::new(Network::Sepolia, vec![ALICE]);
        config.tokens = resolve_tokens(Network::Sepolia, &["USDC", "WETH"]).unwrap();
        config.rpc_host = "chibi.local".parse().unwrap();
        config.rpc_port = port(8745);
        write_next_steps(&mut output, Path::new("/cfg/config.toml"), &config, None).unwrap();
        assert_eq!(
            String::from_utf8(output).unwrap(),
            "wrote /cfg/config.toml (sepolia, 1 address, tracking USDC, WETH)\n\
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
        let mut config = Config::new(Network::Mainnet, vec![ALICE, BOB]);
        config.tokens.clear();
        let custom = Path::new("/tmp/c.toml");
        write_next_steps(&mut output, custom, &config, Some(custom)).unwrap();
        let output = String::from_utf8(output).unwrap();
        assert!(output.starts_with("wrote /tmp/c.toml (mainnet, 2 addresses, no tokens)\n"));
        assert!(
            output.contains("`chibi run -b --config /tmp/c.toml`"),
            "{output}"
        );
        assert!(
            output.contains("`chibi stop --config /tmp/c.toml`"),
            "{output}"
        );
    }
}
