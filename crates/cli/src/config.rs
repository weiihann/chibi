//! `config.toml`: parsed into typed values at load, so everything downstream trusts it.

use std::ffi::OsString;
use std::fmt;
use std::io;
use std::net::{IpAddr, Ipv4Addr, SocketAddr, ToSocketAddrs};
use std::num::{NonZeroU16, NonZeroUsize};
use std::path::{Path, PathBuf};
use std::str::FromStr;

use alloy_primitives::{Address, B256, U256};
use chibi_node::devp2p::Ports;
use chibi_node::{NetConfig, Network, NodeConfig, NodeRecord, TrackedContract, Watchlist};
use clap::Args;
use serde::de::{Error as _, IgnoredAny};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use url::Url;

use crate::rpc::{AllowedHost, OriginPattern, RpcConfig};

/// The JSON-RPC port wallets expect from a local node.
pub const DEFAULT_RPC_PORT: NonZeroU16 = match NonZeroU16::new(8545) {
    Some(port) => port,
    None => panic!("8545 is not zero"),
};

/// Origins of browser-extension pages; ordinary websites cannot read tracked balances.
const DEFAULT_CORS_ORIGINS: [&str; 2] = ["chrome-extension://*", "moz-extension://*"];

/// The contents of `config.toml`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "ConfigFile")]
pub struct Config {
    /// The chain to follow.
    pub network: Network,
    /// Watch-only addresses whose balances, and every registry token's, are tracked.
    #[serde(serialize_with = "serialize_addresses")]
    pub addresses: Vec<Address>,
    /// Beacon API endpoint; the network's default when absent.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub beacon_url: Option<Url>,
    /// Weak-subjectivity checkpoint override.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub checkpoint: Option<B256>,
    /// The host wallets use to reach the JSON-RPC server; it also decides where it listens.
    pub rpc_host: RpcHost,
    /// The JSON-RPC port.
    pub rpc_port: NonZeroU16,
    /// Browser origins allowed to read responses cross-origin.
    pub cors_origins: Vec<OriginPattern>,
    /// `Host` headers the JSON-RPC server answers besides `rpc_host`, as `host` or `host:port`.
    pub allowed_hosts: Vec<AllowedHost>,
    /// devp2p listen port for both RLPx and discovery; the OS picks one when absent.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub p2p_port: Option<u16>,
    /// UDP discovery port when it must differ from `p2p_port`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub discovery_port: Option<u16>,
    /// Absolute root of the per-network data directories; the user data directory when absent.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data_dir: Option<PathBuf>,
    /// Nodes always dialed and kept, besides the network's built-in state peers.
    #[serde(
        skip_serializing_if = "Vec::is_empty",
        serialize_with = "serialize_records"
    )]
    pub trusted_peers: Vec<NodeRecord>,
    /// Connect to `trusted_peers` only: no built-in, remembered or discovered peer.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub trusted_only: bool,
    /// Discovery boot nodes in place of the network's.
    #[serde(
        skip_serializing_if = "Option::is_none",
        serialize_with = "serialize_boot_nodes"
    )]
    pub boot_nodes: Option<Vec<NodeRecord>>,
    /// Most outbound peer sessions; reth's default when absent.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_outbound_peers: Option<NonZeroUsize>,
    /// Most inbound peer sessions; reth's default when absent.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_inbound_peers: Option<usize>,
    /// Run no peer discovery, dialing only the trusted and built-in peers.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub disable_discovery: bool,
    /// Contract accounts tracked with raw storage slots, one entry per address.
    #[serde(
        skip_serializing_if = "Vec::is_empty",
        serialize_with = "serialize_contracts"
    )]
    pub contracts: Vec<TrackedContract>,
}

/// Config keys given on the command line: `chibi init` writes them into the config, and
/// `chibi run` overrides the config with them for that run.
#[derive(Clone, Debug, Default, PartialEq, Eq, Args)]
pub struct ConfigArgs {
    /// Beacon (consensus) API the light client reads sync-committee updates from, instead of
    /// the network's default.
    #[arg(long, value_name = "URL")]
    pub beacon_url: Option<Url>,
    /// Directory to keep per-network data in instead of $XDG_DATA_HOME/chibi.
    #[arg(long, value_name = "PATH", value_parser = absolute_path)]
    pub data_dir: Option<PathBuf>,
    /// Node chibi always dials and keeps, besides the network's built-in state peers; repeat
    /// for several.
    #[arg(long = "trusted-peer", value_name = "ENODE", value_parser = parse_enode)]
    pub trusted_peers: Vec<NodeRecord>,
    /// Connect to the trusted peers only.
    #[arg(long)]
    pub trusted_only: bool,
    /// Discovery boot node in place of the network's; repeat for several.
    #[arg(long = "boot-node", value_name = "ENODE", value_parser = parse_enode)]
    pub boot_nodes: Vec<NodeRecord>,
    /// Most outbound peer sessions [default: 100].
    #[arg(long, value_name = "N")]
    pub max_outbound_peers: Option<NonZeroUsize>,
    /// Most inbound peer sessions [default: 30].
    #[arg(long, value_name = "N")]
    pub max_inbound_peers: Option<usize>,
    /// UDP discovery port, when it must differ from p2p_port.
    #[arg(long, value_name = "PORT")]
    pub discovery_port: Option<u16>,
    /// Run no peer discovery; dial only the trusted and built-in peers.
    #[arg(long)]
    pub disable_discovery: bool,
}

impl ConfigArgs {
    /// Sets every key given on `config`. Repeated flags replace the configured list.
    pub fn apply(&self, config: &mut Config) {
        if let Some(url) = &self.beacon_url {
            config.beacon_url = Some(url.clone());
        }
        if let Some(dir) = &self.data_dir {
            config.data_dir = Some(dir.clone());
        }
        if !self.trusted_peers.is_empty() {
            config.trusted_peers.clone_from(&self.trusted_peers);
        }
        config.trusted_only |= self.trusted_only;
        if !self.boot_nodes.is_empty() {
            config.boot_nodes = Some(self.boot_nodes.clone());
        }
        if let Some(max) = self.max_outbound_peers {
            config.max_outbound_peers = Some(max);
        }
        if let Some(max) = self.max_inbound_peers {
            config.max_inbound_peers = Some(max);
        }
        if let Some(port) = self.discovery_port {
            config.discovery_port = Some(port);
        }
        config.disable_discovery |= self.disable_discovery;
    }

    /// The flags that give the same keys again.
    pub fn to_args(&self) -> Vec<OsString> {
        let mut argv: Vec<OsString> = Vec::new();
        if let Some(url) = &self.beacon_url {
            argv.extend(["--beacon-url".into(), url.as_str().into()]);
        }
        if let Some(dir) = &self.data_dir {
            argv.extend(["--data-dir".into(), dir.as_os_str().to_owned()]);
        }
        for record in &self.trusted_peers {
            argv.extend(["--trusted-peer".into(), record.to_string().into()]);
        }
        if self.trusted_only {
            argv.push("--trusted-only".into());
        }
        for record in &self.boot_nodes {
            argv.extend(["--boot-node".into(), record.to_string().into()]);
        }
        if let Some(max) = self.max_outbound_peers {
            argv.extend(["--max-outbound-peers".into(), max.to_string().into()]);
        }
        if let Some(max) = self.max_inbound_peers {
            argv.extend(["--max-inbound-peers".into(), max.to_string().into()]);
        }
        if let Some(port) = self.discovery_port {
            argv.extend(["--discovery-port".into(), port.to_string().into()]);
        }
        if self.disable_discovery {
            argv.push("--disable-discovery".into());
        }
        argv
    }
}

/// Parses an `enode://` URL as a node record.
fn parse_enode(text: &str) -> Result<NodeRecord, String> {
    text.trim()
        .parse()
        .map_err(|err| format!("{text:?} is not an enode URL: {err}"))
}

/// `text` as an absolute path, resolved against the working directory.
pub fn absolute_path(text: &str) -> Result<PathBuf, String> {
    std::path::absolute(text).map_err(|err| format!("cannot resolve {text:?}: {err}"))
}

/// `config.toml` as written, before its values are checked.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ConfigFile {
    #[serde(default)]
    network: Network,
    #[serde(default, deserialize_with = "deserialize_addresses")]
    addresses: Vec<Address>,
    /// A key earlier versions read, kept only to name the fix when a file still has it.
    #[serde(default)]
    tokens: Option<IgnoredAny>,
    #[serde(default)]
    beacon_url: Option<Url>,
    #[serde(default)]
    checkpoint: Option<B256>,
    #[serde(default)]
    rpc_host: RpcHost,
    #[serde(default = "default_rpc_port")]
    rpc_port: NonZeroU16,
    #[serde(default = "default_cors_origins")]
    cors_origins: Vec<OriginPattern>,
    #[serde(default = "AllowedHost::loopback")]
    allowed_hosts: Vec<AllowedHost>,
    #[serde(default)]
    p2p_port: Option<u16>,
    #[serde(default)]
    discovery_port: Option<u16>,
    #[serde(default)]
    data_dir: Option<PathBuf>,
    #[serde(default)]
    trusted_peers: Vec<String>,
    #[serde(default)]
    trusted_only: bool,
    #[serde(default)]
    boot_nodes: Option<Vec<String>>,
    #[serde(default)]
    max_outbound_peers: Option<NonZeroUsize>,
    #[serde(default)]
    max_inbound_peers: Option<usize>,
    #[serde(default)]
    disable_discovery: bool,
    #[serde(default)]
    contracts: Vec<ContractEntry>,
}

/// One `[[contracts]]` table as written.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ContractEntry {
    address: String,
    #[serde(default)]
    slots: Vec<String>,
}

impl TryFrom<ConfigFile> for Config {
    type Error = InvalidConfig;

    fn try_from(file: ConfigFile) -> Result<Self, InvalidConfig> {
        if file.tokens.is_some() {
            return Err(InvalidConfig::RemovedTokens);
        }
        if let Some(dir) = file.data_dir.as_ref().filter(|dir| !dir.is_absolute()) {
            return Err(InvalidConfig::RelativeDataDir(dir.clone()));
        }
        let config = Self {
            contracts: parse_contracts(&file.contracts)?,
            trusted_peers: parse_records("trusted_peers", &file.trusted_peers)?,
            boot_nodes: file
                .boot_nodes
                .map(|texts| parse_records("boot_nodes", &texts))
                .transpose()?,
            network: file.network,
            addresses: file.addresses,
            beacon_url: file.beacon_url,
            checkpoint: file.checkpoint,
            rpc_host: file.rpc_host,
            rpc_port: file.rpc_port,
            cors_origins: file.cors_origins,
            allowed_hosts: file.allowed_hosts,
            p2p_port: file.p2p_port,
            discovery_port: file.discovery_port,
            data_dir: file.data_dir,
            trusted_only: file.trusted_only,
            max_outbound_peers: file.max_outbound_peers,
            max_inbound_peers: file.max_inbound_peers,
            disable_discovery: file.disable_discovery,
        };
        config.validate()?;
        Ok(config)
    }
}

impl Config {
    /// A config with every optional setting at its default.
    pub fn new(network: Network, addresses: Vec<Address>) -> Self {
        Self {
            network,
            addresses,
            beacon_url: None,
            checkpoint: None,
            rpc_host: RpcHost::default(),
            rpc_port: DEFAULT_RPC_PORT,
            cors_origins: default_cors_origins(),
            allowed_hosts: AllowedHost::loopback(),
            p2p_port: None,
            discovery_port: None,
            data_dir: None,
            trusted_peers: Vec::new(),
            trusted_only: false,
            boot_nodes: None,
            max_outbound_peers: None,
            max_inbound_peers: None,
            disable_discovery: false,
            contracts: Vec::new(),
        }
    }

    /// Checks the rules between keys, which flags can break after the file parsed: trusted
    /// peers to connect to under `trusted_only`, and a peer to dial without discovery.
    pub fn validate(&self) -> Result<(), InvalidConfig> {
        if self.trusted_only && self.trusted_peers.is_empty() {
            return Err(InvalidConfig::TrustedOnlyWithoutPeers);
        }
        let built_in = !self.trusted_only && !self.network.spec().snap_peers.is_empty();
        if self.disable_discovery && self.trusted_peers.is_empty() && !built_in {
            return Err(InvalidConfig::NothingToDial);
        }
        Ok(())
    }

    /// Parses the TOML text of a config file.
    pub fn parse(text: &str) -> Result<Self, toml::de::Error> {
        toml::from_str(text)
    }

    /// Reads and parses the config file at `path`; `None` when there is no file.
    pub fn load(path: &Path) -> Result<Option<Self>, ConfigError> {
        let text = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(source) if source.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(source) => {
                return Err(ConfigError::Read {
                    path: path.to_owned(),
                    source,
                });
            }
        };
        Self::parse(&text)
            .map(Some)
            .map_err(|source| ConfigError::Parse {
                path: path.to_owned(),
                source,
            })
    }

    /// Writes the config to `path`, creating its directory.
    pub fn save(&self, path: &Path) -> Result<(), ConfigError> {
        let write_error = |source| ConfigError::Write {
            path: path.to_owned(),
            source,
        };
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(write_error)?;
        }
        let text = toml::to_string(self)?;
        std::fs::write(path, text).map_err(write_error)
    }

    /// The root of the per-network data directories: `data_dir`, else the user data directory.
    pub fn data_root(
        &self,
        env: impl Fn(&str) -> Option<OsString>,
    ) -> Result<PathBuf, ConfigError> {
        match &self.data_dir {
            Some(dir) => Ok(dir.clone()),
            None => default_data_root(env),
        }
    }

    /// The data directory of `network` under [`Config::data_root`].
    pub fn network_dir(
        &self,
        network: Network,
        env: impl Fn(&str) -> Option<OsString>,
    ) -> Result<PathBuf, ConfigError> {
        Ok(self.data_root(env)?.join(network.name()))
    }

    /// The node settings, with state kept under `data_dir`.
    pub fn node_config(&self, data_dir: PathBuf) -> NodeConfig {
        let port = self.p2p_port.unwrap_or(0);
        NodeConfig {
            network: self.network,
            data_dir,
            beacon_url: self.beacon_url.clone(),
            checkpoint: self.checkpoint,
            watch: Watchlist {
                addresses: self.addresses.clone(),
                contracts: self.contracts.clone(),
            },
            net: NetConfig {
                ports: Ports {
                    rlpx: port,
                    discovery: self.discovery_port.unwrap_or(port),
                },
                trusted_peers: self.trusted_peers.clone(),
                trusted_only: self.trusted_only,
                boot_nodes: self.boot_nodes.clone(),
                max_outbound: self.max_outbound_peers.map(NonZeroUsize::get),
                max_inbound: self.max_inbound_peers,
                disable_discovery: self.disable_discovery,
            },
        }
    }

    /// The JSON-RPC server settings, with `rpc_host` resolved to the listen address.
    pub fn rpc(&self) -> Result<RpcConfig, ResolveError> {
        Ok(RpcConfig {
            addr: self.rpc_host.bind_addr(self.rpc_port)?,
            cors_origins: self.cors_origins.clone(),
            allowed_hosts: self.effective_allowed_hosts(),
        })
    }

    /// `allowed_hosts` plus `rpc_host`, so the name the wallet was given is always answered.
    pub fn effective_allowed_hosts(&self) -> Vec<AllowedHost> {
        let chosen = AllowedHost::bare(self.rpc_host.url_host());
        let mut hosts = self.allowed_hosts.clone();
        if !hosts.contains(&chosen) {
            hosts.push(chosen);
        }
        hosts
    }

    /// The URL a wallet uses to reach the JSON-RPC server.
    pub fn rpc_url(&self) -> String {
        format!("http://{}:{}", self.rpc_host.url_host(), self.rpc_port)
    }
}

impl Default for Config {
    /// Sepolia on 127.0.0.1:8545 with nothing pinned: what `chibi run` uses without a file.
    fn default() -> Self {
        Self::new(Network::default(), Vec::new())
    }
}

/// Where wallets reach the JSON-RPC server: an IP address or a DNS name.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub enum RpcHost {
    /// A literal address, written without brackets.
    Ip(IpAddr),
    /// A DNS name, lowercased.
    Name(String),
}

impl RpcHost {
    /// The host as it appears in a URL or `Host` header, with IPv6 addresses bracketed.
    pub fn url_host(&self) -> String {
        match self {
            Self::Ip(IpAddr::V6(ip)) => format!("[{ip}]"),
            Self::Ip(IpAddr::V4(ip)) => ip.to_string(),
            Self::Name(name) => name.clone(),
        }
    }

    /// The socket address to listen on: the IP itself, or the name's first IPv4 address, else
    /// its first address. `localhost` often resolves to `::1` first, while wallets and tools
    /// commonly try `127.0.0.1`.
    pub fn bind_addr(&self, port: NonZeroU16) -> Result<SocketAddr, ResolveError> {
        match self {
            Self::Ip(ip) => Ok(SocketAddr::new(*ip, port.get())),
            Self::Name(name) => {
                let addrs: Vec<SocketAddr> = (name.as_str(), port.get())
                    .to_socket_addrs()
                    .map_err(|source| ResolveError::Lookup {
                        host: name.clone(),
                        source,
                    })?
                    .collect();
                addrs
                    .iter()
                    .find(|addr| addr.is_ipv4())
                    .or_else(|| addrs.first())
                    .copied()
                    .ok_or_else(|| ResolveError::NoAddress(name.clone()))
            }
        }
    }
}

impl Default for RpcHost {
    fn default() -> Self {
        Self::Ip(IpAddr::V4(Ipv4Addr::LOCALHOST))
    }
}

impl fmt::Display for RpcHost {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Ip(ip) => write!(f, "{ip}"),
            Self::Name(name) => f.write_str(name),
        }
    }
}

impl FromStr for RpcHost {
    type Err = InvalidRpcHost;

    fn from_str(text: &str) -> Result<Self, InvalidRpcHost> {
        let text = text.trim();
        if let Ok(ip) = text.parse() {
            return Ok(Self::Ip(ip));
        }
        let bracketed = text.strip_prefix('[').and_then(|t| t.strip_suffix(']'));
        if let Some(Ok(ip)) = bracketed.map(str::parse::<std::net::Ipv6Addr>) {
            return Ok(Self::Ip(IpAddr::V6(ip)));
        }
        if is_host_name(text) {
            Ok(Self::Name(text.to_ascii_lowercase()))
        } else {
            Err(InvalidRpcHost(text.to_owned()))
        }
    }
}

impl TryFrom<String> for RpcHost {
    type Error = InvalidRpcHost;

    fn try_from(text: String) -> Result<Self, InvalidRpcHost> {
        text.parse()
    }
}

impl From<RpcHost> for String {
    fn from(host: RpcHost) -> Self {
        host.to_string()
    }
}

/// RFC 1123 host name; an all-numeric last label would be a malformed IPv4 address.
fn is_host_name(text: &str) -> bool {
    let valid_label = |label: &str| {
        (1..=63).contains(&label.len())
            && !label.starts_with('-')
            && !label.ends_with('-')
            && label
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-')
    };
    text.len() <= 253
        && text.split('.').all(valid_label)
        && text
            .rsplit('.')
            .next()
            .is_some_and(|last| !last.bytes().all(|b| b.is_ascii_digit()))
}

/// A `rpc_host` that is neither an IP address nor a host name.
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
#[error("{0:?} is not an IP address or host name")]
pub struct InvalidRpcHost(String);

/// Why `rpc_host` gave no address to listen on.
#[derive(Debug, thiserror::Error)]
pub enum ResolveError {
    /// The name lookup failed.
    #[error("cannot resolve rpc_host {host}: {source}")]
    Lookup {
        /// The configured name.
        host: String,
        /// The resolver failure.
        source: io::Error,
    },
    /// The name resolved to nothing.
    #[error("rpc_host {0} resolves to no address")]
    NoAddress(String),
}

/// Why a config file's values were rejected after parsing.
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum InvalidConfig {
    /// A `tokens` line from before every registry token was kept for watched addresses.
    #[error(
        "`tokens` was removed: every registry token is now kept for watched addresses; delete the `tokens` line or rerun `chibi init`"
    )]
    RemovedTokens,
    /// A malformed `[[contracts]]` entry.
    #[error(transparent)]
    Contract(#[from] InvalidContract),
    /// `data_dir` would depend on the directory chibi is started from.
    #[error("data_dir {} is not an absolute path", .0.display())]
    RelativeDataDir(PathBuf),
    /// A `trusted_peers` or `boot_nodes` entry that is not an enode URL.
    #[error("{key}[{index}]: {reason}")]
    Enode {
        /// The key the entry is under.
        key: &'static str,
        /// Position of the entry in the list.
        index: usize,
        /// What was wrong with it, quoting the entry.
        reason: String,
    },
    /// `trusted_only` with no peer it may connect to.
    #[error("trusted_only = true needs at least one trusted_peers entry to connect to")]
    TrustedOnlyWithoutPeers,
    /// `disable_discovery` with no trusted peer and no built-in state peer to dial.
    #[error(
        "disable_discovery = true leaves nothing to dial: add trusted_peers or turn discovery back on"
    )]
    NothingToDial,
}

/// A `[[contracts]]` entry that names no valid address or slot.
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum InvalidContract {
    /// The address is malformed or fails its checksum.
    #[error("contracts[{index}]: {source}")]
    Address {
        /// Position of the entry in the file.
        index: usize,
        /// What was wrong with the address.
        source: AddressError,
    },
    /// A slot is not hex or longer than 32 bytes.
    #[error(
        "contracts[{index}] ({address}): {slot:?} is not a storage slot (expected 0x followed by 1 to 64 hex digits)"
    )]
    Slot {
        /// Position of the entry in the file.
        index: usize,
        /// The entry's address.
        address: Address,
        /// The slot as written.
        slot: String,
    },
}

/// The node records `texts` name, under `key` in the file.
fn parse_records(key: &'static str, texts: &[String]) -> Result<Vec<NodeRecord>, InvalidConfig> {
    texts
        .iter()
        .enumerate()
        .map(|(index, text)| {
            parse_enode(text).map_err(|reason| InvalidConfig::Enode { key, index, reason })
        })
        .collect()
}

/// One tracked contract per address, in first-seen order, with its slots deduplicated in order.
fn parse_contracts(entries: &[ContractEntry]) -> Result<Vec<TrackedContract>, InvalidContract> {
    let mut contracts: Vec<TrackedContract> = Vec::with_capacity(entries.len());
    for (index, entry) in entries.iter().enumerate() {
        let address = parse_address(&entry.address)
            .map_err(|source| InvalidContract::Address { index, source })?;
        let slots = entry
            .slots
            .iter()
            .map(|text| {
                parse_slot(text).ok_or_else(|| InvalidContract::Slot {
                    index,
                    address,
                    slot: text.clone(),
                })
            })
            .collect::<Result<Vec<U256>, _>>()?;
        if !contracts.iter().any(|contract| contract.address == address) {
            contracts.push(TrackedContract {
                address,
                slots: Vec::new(),
            });
        }
        if let Some(contract) = contracts.iter_mut().find(|c| c.address == address) {
            for slot in slots {
                if !contract.slots.contains(&slot) {
                    contract.slots.push(slot);
                }
            }
        }
    }
    Ok(contracts)
}

/// A storage key as `eth_getStorageAt` takes it: `0x` and up to 64 hex digits, not hashed.
fn parse_slot(text: &str) -> Option<U256> {
    let hex = text.trim().strip_prefix("0x")?;
    if hex.is_empty() || hex.len() > 64 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    U256::from_str_radix(hex, 16).ok()
}

/// Reads one variable from the process environment, for the path functions below.
pub fn process_env(key: &str) -> Option<OsString> {
    std::env::var_os(key)
}

/// `chibi/config.toml` under `$XDG_CONFIG_HOME`, else under `$HOME/.config`.
pub fn default_config_path(env: impl Fn(&str) -> Option<OsString>) -> Result<PathBuf, ConfigError> {
    xdg_dir(&env, "XDG_CONFIG_HOME", ".config")
        .map(|dir| dir.join("chibi").join("config.toml"))
        .ok_or(ConfigError::NoConfigDir)
}

/// `chibi` under `$XDG_DATA_HOME`, else under `$HOME/.local/share`.
pub fn default_data_root(env: impl Fn(&str) -> Option<OsString>) -> Result<PathBuf, ConfigError> {
    xdg_dir(&env, "XDG_DATA_HOME", ".local/share")
        .map(|dir| dir.join("chibi"))
        .ok_or(ConfigError::NoDataDir)
}

/// `$var` when it holds an absolute path, else `$HOME/fallback`. The XDG base directory spec
/// says relative values are invalid and must be ignored, which also covers empty ones.
fn xdg_dir(env: &impl Fn(&str) -> Option<OsString>, var: &str, fallback: &str) -> Option<PathBuf> {
    let absolute = |key: &str| {
        env(key)
            .map(PathBuf::from)
            .filter(|path| path.is_absolute())
    };
    absolute(var).or_else(|| absolute("HOME").map(|home| home.join(fallback)))
}

/// Parses a `0x`-prefixed address. Mixed-case input must carry a valid EIP-55 checksum.
pub fn parse_address(text: &str) -> Result<Address, AddressError> {
    let text = text.trim();
    let malformed = || AddressError::Malformed(text.to_owned());
    let hex = text.strip_prefix("0x").ok_or_else(malformed)?;
    if hex.len() != 40 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(malformed());
    }
    let address: Address = text.parse().map_err(|_| malformed())?;
    let mixed_case =
        hex.bytes().any(|b| b.is_ascii_lowercase()) && hex.bytes().any(|b| b.is_ascii_uppercase());
    if mixed_case && address.to_checksum(None) != text {
        return Err(AddressError::Checksum(text.to_owned()));
    }
    Ok(address)
}

/// An address that failed [`parse_address`].
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum AddressError {
    /// Not `0x` followed by 40 hex digits.
    #[error("{0:?} is not an address (expected 0x followed by 40 hex digits)")]
    Malformed(String),
    /// Mixed case whose EIP-55 checksum does not match.
    #[error("{0:?} has an invalid checksum")]
    Checksum(String),
}

/// Why the config could not be located, read, or written.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    /// Neither the XDG variable nor HOME gives a config directory.
    #[error(
        "cannot locate the config directory: neither XDG_CONFIG_HOME nor HOME is an absolute path; pass --config"
    )]
    NoConfigDir,
    /// Neither the XDG variable nor HOME gives a data directory.
    #[error(
        "cannot locate the data directory: neither XDG_DATA_HOME nor HOME is an absolute path; set data_dir in the config"
    )]
    NoDataDir,
    /// The file exists but could not be read.
    #[error("failed to read {}: {source}", path.display())]
    Read {
        /// The config path.
        path: PathBuf,
        /// The I/O failure.
        source: io::Error,
    },
    /// The file is not a valid config.
    #[error("invalid config {}: {source}", path.display())]
    Parse {
        /// The config path.
        path: PathBuf,
        /// What was wrong.
        source: toml::de::Error,
    },
    /// The config could not be encoded.
    #[error("failed to encode config: {0}")]
    Encode(#[from] toml::ser::Error),
    /// The file or its directory could not be written.
    #[error("failed to write {}: {source}", path.display())]
    Write {
        /// The config path.
        path: PathBuf,
        /// The I/O failure.
        source: io::Error,
    },
}

fn default_rpc_port() -> NonZeroU16 {
    DEFAULT_RPC_PORT
}

fn default_cors_origins() -> Vec<OriginPattern> {
    DEFAULT_CORS_ORIGINS
        .into_iter()
        .map(|origin| OriginPattern::Prefix(origin.trim_end_matches('*').to_owned()))
        .collect()
}

fn deserialize_addresses<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<Vec<Address>, D::Error> {
    Vec::<String>::deserialize(deserializer)?
        .iter()
        .map(|text| parse_address(text).map_err(D::Error::custom))
        .collect()
}

fn serialize_records<S: Serializer>(
    records: &[NodeRecord],
    serializer: S,
) -> Result<S::Ok, S::Error> {
    serializer.collect_seq(records.iter().map(NodeRecord::to_string))
}

fn serialize_boot_nodes<S: Serializer>(
    records: &Option<Vec<NodeRecord>>,
    serializer: S,
) -> Result<S::Ok, S::Error> {
    match records {
        Some(records) => serialize_records(records, serializer),
        None => serializer.serialize_none(),
    }
}

fn serialize_addresses<S: Serializer>(
    addresses: &[Address],
    serializer: S,
) -> Result<S::Ok, S::Error> {
    serializer.collect_seq(addresses.iter().map(|a| a.to_checksum(None)))
}

fn serialize_contracts<S: Serializer>(
    contracts: &[TrackedContract],
    serializer: S,
) -> Result<S::Ok, S::Error> {
    #[derive(Serialize)]
    struct Entry {
        address: String,
        #[serde(skip_serializing_if = "Vec::is_empty")]
        slots: Vec<String>,
    }
    serializer.collect_seq(contracts.iter().map(|contract| {
        Entry {
            address: contract.address.to_checksum(None),
            slots: contract
                .slots
                .iter()
                .map(|slot| format!("{slot:#x}"))
                .collect(),
        }
    }))
}

#[cfg(test)]
mod tests {
    use std::net::Ipv6Addr;

    use alloy_primitives::address;

    use super::*;

    const ALICE: Address = address!("0x8f3Cf7ad23Cd3CaDbD9735AFf958023239c6A063");
    const USDC: Address = address!("0x1c7D4B196Cb0C7B01d743Fbc6116a902379C7238");

    fn port(port: u16) -> NonZeroU16 {
        NonZeroU16::new(port).unwrap()
    }

    #[test]
    fn minimal_config_takes_defaults() {
        let config =
            Config::parse(r#"addresses = ["0x8f3Cf7ad23Cd3CaDbD9735AFf958023239c6A063"]"#).unwrap();
        assert_eq!(config, Config::new(Network::Sepolia, vec![ALICE]));
        assert_eq!(config.rpc_host, RpcHost::Ip(Ipv4Addr::LOCALHOST.into()));
        assert_eq!(config.rpc_port, port(8545));
        assert_eq!(config.rpc_url(), "http://127.0.0.1:8545");
        assert_eq!(
            config.cors_origins,
            vec![
                OriginPattern::Prefix("chrome-extension://".to_owned()),
                OriginPattern::Prefix("moz-extension://".to_owned()),
            ]
        );
        assert_eq!(
            config
                .allowed_hosts
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>(),
            ["localhost", "127.0.0.1", "[::1]"]
        );
    }

    fn enode(byte: u8, port: u16) -> String {
        format!(
            "enode://{}@10.0.0.{byte}:{port}",
            alloy_primitives::hex::encode([byte; 64])
        )
    }

    fn record(byte: u8, port: u16) -> NodeRecord {
        enode(byte, port).parse().unwrap()
    }

    #[test]
    fn full_config_parses_every_field() {
        let config = Config::parse(&format!(
            r#"
            network = "mainnet"
            addresses = ["0x8f3cf7ad23cd3cadbd9735aff958023239c6a063"]
            beacon_url = "https://beacon.example/"
            checkpoint = "0x1111111111111111111111111111111111111111111111111111111111111111"
            rpc_host = "Chibi.Example"
            rpc_port = 9545
            cors_origins = ["https://app.example", "chrome-extension://abc*"]
            allowed_hosts = ["localhost", "chibi.local:9545"]
            p2p_port = 30303
            discovery_port = 30304
            data_dir = "/srv/chibi"
            trusted_peers = ["{}", "{}"]
            trusted_only = true
            boot_nodes = ["{}"]
            max_outbound_peers = 5
            max_inbound_peers = 0
            disable_discovery = true

            [[contracts]]
            address = "0x1c7D4B196Cb0C7B01d743Fbc6116a902379C7238"
            slots = ["0x1"]
            "#,
            enode(1, 30303),
            enode(2, 30305),
            enode(3, 30303)
        ))
        .unwrap();
        assert_eq!(
            config,
            Config {
                network: Network::Mainnet,
                addresses: vec![ALICE],
                beacon_url: Some(Url::parse("https://beacon.example/").unwrap()),
                checkpoint: Some(B256::repeat_byte(0x11)),
                rpc_host: RpcHost::Name("chibi.example".to_owned()),
                rpc_port: port(9545),
                cors_origins: vec![
                    OriginPattern::Exact("https://app.example".to_owned()),
                    OriginPattern::Prefix("chrome-extension://abc".to_owned()),
                ],
                allowed_hosts: vec![
                    AllowedHost::try_from("localhost".to_owned()).unwrap(),
                    AllowedHost::try_from("chibi.local:9545".to_owned()).unwrap(),
                ],
                p2p_port: Some(30303),
                discovery_port: Some(30304),
                data_dir: Some(PathBuf::from("/srv/chibi")),
                trusted_peers: vec![record(1, 30303), record(2, 30305)],
                trusted_only: true,
                boot_nodes: Some(vec![record(3, 30303)]),
                max_outbound_peers: NonZeroUsize::new(5),
                max_inbound_peers: Some(0),
                disable_discovery: true,
                contracts: vec![TrackedContract {
                    address: USDC,
                    slots: vec![U256::from(1)],
                }],
            }
        );
        let node = config.node_config(PathBuf::from("/data"));
        assert_eq!(
            node.net,
            NetConfig {
                ports: Ports {
                    rlpx: 30303,
                    discovery: 30304
                },
                trusted_peers: vec![record(1, 30303), record(2, 30305)],
                trusted_only: true,
                boot_nodes: Some(vec![record(3, 30303)]),
                max_outbound: Some(5),
                max_inbound: Some(0),
                disable_discovery: true,
            }
        );
        assert_eq!(
            node.watch,
            Watchlist {
                addresses: vec![ALICE],
                contracts: config.contracts,
            }
        );
    }

    #[test]
    fn peer_keys_default_to_the_networks_peers_and_reths_limits() {
        let mut config = Config::default();
        assert_eq!(
            config.node_config(PathBuf::from("/data")).net,
            NetConfig::default()
        );
        config.p2p_port = Some(30310);
        assert_eq!(
            config.node_config(PathBuf::from("/data")).net.ports,
            Ports {
                rlpx: 30310,
                discovery: 30310
            },
            "discovery listens on p2p_port unless discovery_port is set"
        );
    }

    #[test]
    fn peer_keys_save_and_load_back_equal() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let mut config = Config::new(Network::Mainnet, Vec::new());
        config.trusted_peers = vec![record(1, 30303), record(2, 30305)];
        config.trusted_only = true;
        config.boot_nodes = Some(Vec::new());
        config.max_outbound_peers = NonZeroUsize::new(3);
        config.max_inbound_peers = Some(7);
        config.discovery_port = Some(30399);
        config.disable_discovery = true;
        config.save(&path).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        for line in [
            format!(
                "trusted_peers = [\"{}\", \"{}\"]",
                enode(1, 30303),
                enode(2, 30305)
            ),
            "trusted_only = true".to_owned(),
            "boot_nodes = []".to_owned(),
            "max_outbound_peers = 3".to_owned(),
            "max_inbound_peers = 7".to_owned(),
            "discovery_port = 30399".to_owned(),
            "disable_discovery = true".to_owned(),
        ] {
            assert!(text.contains(&line), "{line} in {text}");
        }
        assert_eq!(Config::load(&path).unwrap(), Some(config));

        Config::default().save(&path).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        for key in ["trusted", "boot_nodes", "max_", "discovery"] {
            assert!(!text.contains(key), "{key} in {text}");
        }
    }

    #[test]
    fn a_bad_enode_is_rejected_naming_the_entry() {
        let error = |text: String| Config::parse(&text).unwrap_err().to_string();
        let trusted = error(format!(
            "trusted_peers = [\"{}\", \"enode://bad@10.0.0.1:30303\"]",
            enode(1, 30303)
        ));
        assert!(
            trusted
                .contains("trusted_peers[1]: \"enode://bad@10.0.0.1:30303\" is not an enode URL"),
            "{trusted}"
        );
        let boot = error("boot_nodes = [\"10.0.0.1:30303\"]".to_owned());
        assert!(
            boot.contains("boot_nodes[0]: \"10.0.0.1:30303\" is not an enode URL"),
            "{boot}"
        );
        assert!(Config::parse("max_outbound_peers = 0").is_err());
        assert!(Config::parse("discovery_port = 70000").is_err());
    }

    #[test]
    fn trusted_only_and_disable_discovery_need_a_peer_to_dial() {
        let error = |text: &str| Config::parse(text).unwrap_err().to_string();
        assert!(
            error("trusted_only = true").contains(
                "trusted_only = true needs at least one trusted_peers entry to connect to"
            ),
            "{}",
            error("trusted_only = true")
        );
        let nothing = "disable_discovery = true leaves nothing to dial: add trusted_peers";
        assert!(
            error("network = \"mainnet\"\ndisable_discovery = true").contains(nothing),
            "mainnet has no built-in state peers"
        );
        let trusted_only = format!(
            "trusted_only = true\ndisable_discovery = true\ntrusted_peers = [\"{}\"]",
            enode(1, 30303)
        );
        assert!(Config::parse(&trusted_only).is_ok());
        assert!(
            Config::parse("disable_discovery = true").is_ok(),
            "sepolia dials its built-in state peers"
        );
        let mut config = Config::parse("disable_discovery = true").unwrap();
        config.network = Network::Mainnet;
        assert!(
            config.validate().unwrap_err().to_string().contains(nothing),
            "a network switch is checked again"
        );
    }

    #[test]
    fn a_tokens_line_is_rejected_naming_the_fix() {
        for text in [
            r#"tokens = ["USDC"]"#,
            "tokens = []",
            "addresses = [\"0x8f3Cf7ad23Cd3CaDbD9735AFf958023239c6A063\"]\ntokens = [\"DAI\"]",
        ] {
            let error = Config::parse(text).unwrap_err().to_string();
            assert!(
                error.contains(
                    "`tokens` was removed: every registry token is now kept for watched \
                     addresses; delete the `tokens` line or rerun `chibi init`"
                ),
                "{text}: {error}"
            );
        }
    }

    #[test]
    fn the_node_gets_the_watched_addresses_and_no_token_choice() {
        for (text, addresses) in [
            ("", Vec::new()),
            (
                r#"addresses = ["0x8f3Cf7ad23Cd3CaDbD9735AFf958023239c6A063"]"#,
                vec![ALICE],
            ),
        ] {
            let config = Config::parse(text).unwrap();
            assert_eq!(
                config.node_config(PathBuf::from("/data")).watch,
                Watchlist {
                    addresses,
                    contracts: Vec::new(),
                },
                "{text}"
            );
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("config.toml");
            config.save(&path).unwrap();
            let saved = std::fs::read_to_string(&path).unwrap();
            assert!(!saved.contains("tokens"), "{saved}");
            assert_eq!(Config::load(&path).unwrap(), Some(config));
        }
    }

    #[test]
    fn rejects_bad_addresses() {
        for bad in [
            "0x8f3c",
            "8f3cf7ad23cd3cadbd9735aff958023239c6a063",
            "0x8f3cf7ad23cd3cadbd9735aff958023239c6a06g",
            "0x8f3Cf7ad23Cd3CaDbD9735AFf958023239c6A064",
        ] {
            let text = format!("addresses = [{bad:?}]");
            let error = Config::parse(&text).unwrap_err().to_string();
            assert!(error.contains(bad), "{bad}: {error}");
        }
    }

    #[test]
    fn checksum_applies_only_to_mixed_case() {
        assert_eq!(
            parse_address("0x8F3CF7AD23CD3CADBD9735AFF958023239C6A063"),
            Ok(ALICE)
        );
        assert_eq!(
            parse_address(" 0x8f3Cf7ad23Cd3CaDbD9735AFf958023239c6A063\n"),
            Ok(ALICE)
        );
        assert_eq!(
            parse_address("0x8f3Cf7ad23Cd3CaDbD9735AFf958023239c6a063"),
            Err(AddressError::Checksum(
                "0x8f3Cf7ad23Cd3CaDbD9735AFf958023239c6a063".to_owned()
            ))
        );
    }

    #[test]
    fn rejects_unknown_network() {
        let error = Config::parse(r#"network = "goerli""#)
            .unwrap_err()
            .to_string();
        assert!(error.contains("goerli"), "{error}");
    }

    #[test]
    fn rejects_unknown_field() {
        let error = Config::parse(r#"rpc_addr = "127.0.0.1:8545""#)
            .unwrap_err()
            .to_string();
        assert!(error.contains("rpc_addr"), "{error}");
    }

    #[test]
    fn rejects_bad_values() {
        for text in [
            r#"beacon_url = "not a url""#,
            r#"checkpoint = "0x11""#,
            r#"rpc_host = "chibi_local""#,
            r#"rpc_host = "http://localhost""#,
            "rpc_port = 0",
            "rpc_port = 70000",
            r#"cors_origins = ["chrome-*-extension://"]"#,
            r#"cors_origins = [""]"#,
            r#"allowed_hosts = ["http://localhost"]"#,
            "p2p_port = 70000",
        ] {
            assert!(Config::parse(text).is_err(), "{text}");
        }
    }

    #[test]
    fn rpc_host_accepts_ips_and_host_names() {
        let parse = |text: &str| text.parse::<RpcHost>();
        assert_eq!(
            parse("0.0.0.0"),
            Ok(RpcHost::Ip(Ipv4Addr::UNSPECIFIED.into()))
        );
        assert_eq!(parse("::1"), Ok(RpcHost::Ip(Ipv6Addr::LOCALHOST.into())));
        assert_eq!(parse("[::1]"), Ok(RpcHost::Ip(Ipv6Addr::LOCALHOST.into())));
        assert_eq!(
            parse("LocalHost"),
            Ok(RpcHost::Name("localhost".to_owned()))
        );
        assert_eq!(
            parse("node-1.chibi.example"),
            Ok(RpcHost::Name("node-1.chibi.example".to_owned()))
        );
        for bad in [
            "",
            "1.2.3",
            "300.1.1.1",
            "-chibi.example",
            "chibi..example",
            "a b",
            "host:8545",
        ] {
            assert_eq!(parse(bad), Err(InvalidRpcHost(bad.to_owned())), "{bad}");
        }
    }

    #[test]
    fn allowlist_adds_the_chosen_rpc_host() {
        let mut config = Config::new(Network::Sepolia, vec![ALICE]);
        let shown = |config: &Config| {
            config
                .effective_allowed_hosts()
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
        };
        assert_eq!(shown(&config), ["localhost", "127.0.0.1", "[::1]"]);

        config.rpc_host = "::1".parse().unwrap();
        assert_eq!(shown(&config), ["localhost", "127.0.0.1", "[::1]"]);

        config.rpc_host = "chibi.example".parse().unwrap();
        assert_eq!(
            shown(&config),
            ["localhost", "127.0.0.1", "[::1]", "chibi.example"]
        );
        assert_eq!(config.rpc_url(), "http://chibi.example:8545");
    }

    #[test]
    fn bind_address_resolves_names_and_keeps_ips() {
        let localhost = RpcHost::Name("localhost".to_owned())
            .bind_addr(port(8745))
            .unwrap();
        assert!(localhost.ip().is_loopback(), "{localhost}");
        assert_eq!(localhost.port(), 8745);

        let any = RpcHost::Ip(Ipv4Addr::UNSPECIFIED.into());
        assert_eq!(
            any.bind_addr(port(8545)).unwrap(),
            SocketAddr::from((Ipv4Addr::UNSPECIFIED, 8545))
        );
        let v6 = RpcHost::Ip(Ipv6Addr::LOCALHOST.into());
        assert_eq!(v6.url_host(), "[::1]");
        assert_eq!(v6.bind_addr(port(9)).unwrap().to_string(), "[::1]:9");
    }

    #[test]
    fn saved_config_loads_back_equal() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join("config.toml");
        let mut config = Config::new(Network::Mainnet, vec![ALICE, Address::repeat_byte(7)]);
        config
            .cors_origins
            .push(OriginPattern::Exact("https://app.example".to_owned()));
        config.p2p_port = Some(30304);
        config.data_dir = Some(PathBuf::from("/srv/chibi"));
        config.contracts = vec![
            TrackedContract {
                address: USDC,
                slots: vec![U256::from(1), U256::MAX],
            },
            TrackedContract {
                address: ALICE,
                slots: Vec::new(),
            },
        ];
        config.rpc_host = "::1".parse().unwrap();
        config.rpc_port = port(9545);
        config.save(&path).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(!text.contains("tokens"), "{text}");
        assert!(text.contains(r#"rpc_host = "::1""#), "{text}");
        assert!(text.contains("[[contracts]]"), "{text}");
        assert!(text.contains(r#"slots = ["0x1", "0xffff"#), "{text}");
        assert_eq!(Config::load(&path).unwrap(), Some(config));
    }

    #[test]
    fn missing_file_loads_as_none() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(Config::load(&dir.path().join("config.toml")).unwrap(), None);
    }

    fn env<'a>(vars: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<OsString> + 'a {
        move |key| {
            vars.iter()
                .find(|(name, _)| *name == key)
                .map(|(_, value)| OsString::from(value))
        }
    }

    #[test]
    fn paths_follow_xdg_variables_when_absolute() {
        let vars = [
            ("HOME", "/home/u"),
            ("XDG_CONFIG_HOME", "/xdg/config"),
            ("XDG_DATA_HOME", "/xdg/data"),
        ];
        assert_eq!(
            default_config_path(env(&vars)).unwrap(),
            PathBuf::from("/xdg/config/chibi/config.toml")
        );
        assert_eq!(
            default_data_root(env(&vars)).unwrap(),
            PathBuf::from("/xdg/data/chibi")
        );
    }

    #[test]
    fn paths_fall_back_to_home_when_xdg_is_unset_empty_or_relative() {
        for xdg in [None, Some(""), Some("relative/dir")] {
            let mut vars = vec![("HOME", "/home/u")];
            if let Some(value) = xdg {
                vars.extend([("XDG_CONFIG_HOME", value), ("XDG_DATA_HOME", value)]);
            }
            assert_eq!(
                default_config_path(env(&vars)).unwrap(),
                PathBuf::from("/home/u/.config/chibi/config.toml"),
                "{xdg:?}"
            );
            assert_eq!(
                default_data_root(env(&vars)).unwrap(),
                PathBuf::from("/home/u/.local/share/chibi"),
                "{xdg:?}"
            );
        }
        assert!(matches!(
            default_config_path(env(&[("HOME", "")])),
            Err(ConfigError::NoConfigDir)
        ));
        assert!(matches!(
            default_data_root(env(&[])),
            Err(ConfigError::NoDataDir)
        ));
    }

    #[test]
    fn data_dir_replaces_the_data_root_and_keeps_the_network_subdir() {
        let vars = [("HOME", "/home/u")];
        let mut config = Config::new(Network::Sepolia, vec![ALICE]);
        assert_eq!(
            config.network_dir(Network::Sepolia, env(&vars)).unwrap(),
            PathBuf::from("/home/u/.local/share/chibi/sepolia")
        );
        config.data_dir = Some(PathBuf::from("/srv/chibi"));
        assert_eq!(
            config.data_root(env(&[])).unwrap(),
            PathBuf::from("/srv/chibi")
        );
        assert_eq!(
            config.network_dir(Network::Mainnet, env(&[])).unwrap(),
            PathBuf::from("/srv/chibi/mainnet")
        );
    }

    #[test]
    fn relative_data_dir_is_rejected() {
        let error = Config::parse(r#"data_dir = "data""#)
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("data_dir data is not an absolute path"),
            "{error}"
        );
    }

    #[test]
    fn contracts_parse_with_and_without_slots() {
        let config = Config::parse(
            r#"
            [[contracts]]
            address = "0x1c7D4B196Cb0C7B01d743Fbc6116a902379C7238"
            slots = ["0x7050c9e0f4ca769c69bd3a8ef740bc37934f8e2c036e5a723fd8ee048ed3f8c3", "0x1"]

            [[contracts]]
            address = "0x8f3cf7ad23cd3cadbd9735aff958023239c6a063"
            "#,
        )
        .unwrap();
        let mapping = U256::from_str_radix(
            "7050c9e0f4ca769c69bd3a8ef740bc37934f8e2c036e5a723fd8ee048ed3f8c3",
            16,
        )
        .unwrap();
        assert_eq!(
            config.contracts,
            vec![
                TrackedContract {
                    address: USDC,
                    slots: vec![mapping, U256::from(1)],
                },
                TrackedContract {
                    address: ALICE,
                    slots: Vec::new(),
                },
            ]
        );
        assert!(Config::parse("").unwrap().contracts.is_empty());
    }

    #[test]
    fn duplicate_contracts_merge_and_slots_dedupe() {
        let config = Config::parse(
            r#"
            [[contracts]]
            address = "0x1c7d4b196cb0c7b01d743fbc6116a902379c7238"
            slots = ["0x1", "0x01", "0x2"]

            [[contracts]]
            address = "0x8f3Cf7ad23Cd3CaDbD9735AFf958023239c6A063"

            [[contracts]]
            address = "0x1c7D4B196Cb0C7B01d743Fbc6116a902379C7238"
            slots = ["0x3", "0x0000000000000000000000000000000000000000000000000000000000000002"]
            "#,
        )
        .unwrap();
        assert_eq!(
            config.contracts,
            vec![
                TrackedContract {
                    address: USDC,
                    slots: vec![U256::from(1), U256::from(2), U256::from(3)],
                },
                TrackedContract {
                    address: ALICE,
                    slots: Vec::new(),
                },
            ]
        );
    }

    #[test]
    fn bad_contract_entries_are_rejected_naming_the_entry() {
        let entry = |address: &str, slot: &str| {
            format!(
                "[[contracts]]\naddress = \"{USDC}\"\n\n[[contracts]]\naddress = {address:?}\nslots = [{slot:?}]\n"
            )
        };
        let error = |text: String| Config::parse(&text).unwrap_err().to_string();

        let bad_address = error(entry("0x1234", "0x1"));
        assert!(
            bad_address.contains("contracts[1]: \"0x1234\" is not an address"),
            "{bad_address}"
        );
        let bad_checksum = error(entry("0x1c7D4B196Cb0C7B01d743Fbc6116a902379C7239", "0x1"));
        assert!(
            bad_checksum.contains("contracts[1]: ") && bad_checksum.contains("invalid checksum"),
            "{bad_checksum}"
        );

        let too_long = format!("0x1{}", "0".repeat(64));
        for slot in ["0xzz", "1", "0x", "", too_long.as_str()] {
            let message = error(entry(&ALICE.to_string(), slot));
            assert!(
                message.contains(&format!(
                    "contracts[1] ({ALICE}): {slot:?} is not a storage slot"
                )),
                "{slot}: {message}"
            );
        }
        assert!(Config::parse("[[contracts]]\nslots = [\"0x1\"]\n").is_err());
        assert!(
            Config::parse(&format!(
                "[[contracts]]\naddress = \"{USDC}\"\nslot = [\"0x1\"]\n"
            ))
            .is_err(),
            "unknown keys are rejected"
        );
    }
}
