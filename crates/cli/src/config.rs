//! `config.toml`: parsed into typed values at load, so everything downstream trusts it.

use std::io;
use std::net::{Ipv4Addr, SocketAddr};
use std::path::{Path, PathBuf};

use alloy_primitives::{Address, B256};
use chibi_node::devp2p::Ports;
use chibi_node::{Network, NodeConfig, Watchlist};
use serde::de::Error as _;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use url::Url;

use crate::rpc::{AllowedHost, OriginPattern, RpcConfig};

/// The JSON-RPC port wallets expect from a local node.
const DEFAULT_RPC_PORT: u16 = 8545;

/// Origins of browser-extension pages; ordinary websites cannot read tracked balances.
const DEFAULT_CORS_ORIGINS: [&str; 2] = ["chrome-extension://*", "moz-extension://*"];

/// The contents of `config.toml`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// The chain to follow.
    #[serde(default)]
    pub network: Network,
    /// Watch-only addresses whose balances are tracked.
    #[serde(
        default,
        deserialize_with = "deserialize_addresses",
        serialize_with = "serialize_addresses"
    )]
    pub addresses: Vec<Address>,
    /// Beacon API endpoint; the network's default when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub beacon_url: Option<Url>,
    /// Weak-subjectivity checkpoint override.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checkpoint: Option<B256>,
    /// Where the JSON-RPC server listens.
    #[serde(default = "default_rpc_addr")]
    pub rpc_addr: SocketAddr,
    /// Browser origins allowed to read responses cross-origin.
    #[serde(default = "default_cors_origins")]
    pub cors_origins: Vec<OriginPattern>,
    /// `Host` headers the JSON-RPC server answers, as `host` or `host:port`.
    #[serde(default = "AllowedHost::loopback")]
    pub allowed_hosts: Vec<AllowedHost>,
    /// devp2p listen port for both RLPx and discovery; the OS picks one when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub p2p_port: Option<u16>,
}

impl Config {
    /// A config with every optional setting at its default.
    pub fn new(network: Network, addresses: Vec<Address>) -> Self {
        Self {
            network,
            addresses,
            beacon_url: None,
            checkpoint: None,
            rpc_addr: default_rpc_addr(),
            cors_origins: default_cors_origins(),
            allowed_hosts: AllowedHost::loopback(),
            p2p_port: None,
        }
    }

    /// Parses the TOML text of a config file.
    pub fn parse(text: &str) -> Result<Self, toml::de::Error> {
        toml::from_str(text)
    }

    /// Reads and parses the config file at `path`.
    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        let text = std::fs::read_to_string(path).map_err(|source| {
            if source.kind() == io::ErrorKind::NotFound {
                ConfigError::Missing(path.to_owned())
            } else {
                ConfigError::Read {
                    path: path.to_owned(),
                    source,
                }
            }
        })?;
        Self::parse(&text).map_err(|source| ConfigError::Parse {
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
                tokens: self.network.spec().token("USDC").into_iter().collect(),
            },
            ports: Ports {
                rlpx: port,
                discovery: port,
            },
        }
    }

    /// The JSON-RPC server settings.
    pub fn rpc(&self) -> RpcConfig {
        RpcConfig {
            addr: self.rpc_addr,
            cors_origins: self.cors_origins.clone(),
            allowed_hosts: self.allowed_hosts.clone(),
        }
    }
}

/// `config.toml` in the user's config directory.
pub fn default_config_path() -> Result<PathBuf, ConfigError> {
    dirs::config_dir()
        .map(|dir| dir.join("chibi").join("config.toml"))
        .ok_or(ConfigError::NoConfigDir)
}

/// The per-network data directory in the user's data directory.
pub fn data_dir(network: Network) -> Result<PathBuf, ConfigError> {
    dirs::data_dir()
        .map(|dir| dir.join("chibi").join(network.name()))
        .ok_or(ConfigError::NoDataDir)
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
    /// The platform has no user config directory.
    #[error("no user config directory on this platform; pass --config")]
    NoConfigDir,
    /// The platform has no user data directory.
    #[error("no user data directory on this platform")]
    NoDataDir,
    /// No config file exists yet.
    #[error("no config at {}; run `chibi init` first", .0.display())]
    Missing(PathBuf),
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

fn default_rpc_addr() -> SocketAddr {
    SocketAddr::from((Ipv4Addr::LOCALHOST, DEFAULT_RPC_PORT))
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

fn serialize_addresses<S: Serializer>(
    addresses: &[Address],
    serializer: S,
) -> Result<S::Ok, S::Error> {
    serializer.collect_seq(addresses.iter().map(|a| a.to_checksum(None)))
}

#[cfg(test)]
mod tests {
    use alloy_primitives::address;

    use super::*;

    const ALICE: Address = address!("0x8f3Cf7ad23Cd3CaDbD9735AFf958023239c6A063");

    #[test]
    fn minimal_config_takes_defaults() {
        let config =
            Config::parse(r#"addresses = ["0x8f3Cf7ad23Cd3CaDbD9735AFf958023239c6A063"]"#).unwrap();
        assert_eq!(config, Config::new(Network::Sepolia, vec![ALICE]));
        assert_eq!(config.rpc_addr.to_string(), "127.0.0.1:8545");
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

    #[test]
    fn full_config_parses_every_field() {
        let config = Config::parse(
            r#"
            network = "mainnet"
            addresses = ["0x8f3cf7ad23cd3cadbd9735aff958023239c6a063"]
            beacon_url = "https://beacon.example/"
            checkpoint = "0x1111111111111111111111111111111111111111111111111111111111111111"
            rpc_addr = "127.0.0.1:9545"
            cors_origins = ["https://app.example", "chrome-extension://abc*"]
            allowed_hosts = ["localhost", "chibi.local:9545"]
            p2p_port = 30303
            "#,
        )
        .unwrap();
        assert_eq!(
            config,
            Config {
                network: Network::Mainnet,
                addresses: vec![ALICE],
                beacon_url: Some(Url::parse("https://beacon.example/").unwrap()),
                checkpoint: Some(B256::repeat_byte(0x11)),
                rpc_addr: "127.0.0.1:9545".parse().unwrap(),
                cors_origins: vec![
                    OriginPattern::Exact("https://app.example".to_owned()),
                    OriginPattern::Prefix("chrome-extension://abc".to_owned()),
                ],
                allowed_hosts: vec![
                    AllowedHost::try_from("localhost".to_owned()).unwrap(),
                    AllowedHost::try_from("chibi.local:9545".to_owned()).unwrap(),
                ],
                p2p_port: Some(30303),
            }
        );
        let node = config.node_config(PathBuf::from("/data"));
        assert_eq!(
            node.ports,
            Ports {
                rlpx: 30303,
                discovery: 30303
            }
        );
        assert_eq!(node.watch.addresses, vec![ALICE]);
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
        let error = Config::parse("rpc_port = 8545").unwrap_err().to_string();
        assert!(error.contains("rpc_port"), "{error}");
    }

    #[test]
    fn rejects_bad_values() {
        for text in [
            r#"beacon_url = "not a url""#,
            r#"checkpoint = "0x11""#,
            r#"rpc_addr = "localhost""#,
            r#"cors_origins = ["chrome-*-extension://"]"#,
            r#"cors_origins = [""]"#,
            r#"allowed_hosts = ["http://localhost"]"#,
            "p2p_port = 70000",
        ] {
            assert!(Config::parse(text).is_err(), "{text}");
        }
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
        config.save(&path).unwrap();
        assert_eq!(Config::load(&path).unwrap(), config);
    }

    #[test]
    fn missing_file_says_to_run_init() {
        let dir = tempfile::tempdir().unwrap();
        let error = Config::load(&dir.path().join("config.toml")).unwrap_err();
        assert!(matches!(error, ConfigError::Missing(_)), "{error}");
        assert!(error.to_string().contains("chibi init"));
    }
}
