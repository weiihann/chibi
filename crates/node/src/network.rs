//! The network registry. Every chain-specific fact lives in one [`NetworkSpec`] per
//! [`Network`] and nowhere else.

use std::fmt;
use std::str::FromStr;
use std::sync::Arc;

use alloy_primitives::{Address, BlockNumber, address};
use reth_chainspec::{ChainSpec, MAINNET, SEPOLIA, mainnet, sepolia};
use reth_network_peers::{NodeRecord, mainnet_nodes, sepolia_nodes};
use serde::{Deserialize, Serialize};

/// Seconds per slot on every supported network.
const SLOT_SECONDS: u64 = 12;

/// A supported network, as named in `config.toml` and on the command line.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Network {
    /// The Sepolia testnet.
    #[default]
    Sepolia,
    /// Ethereum mainnet.
    Mainnet,
}

impl Network {
    /// Every supported network.
    pub const ALL: [Self; 2] = [Self::Sepolia, Self::Mainnet];

    /// The lowercase name used in configuration.
    pub const fn name(self) -> &'static str {
        match self {
            Self::Sepolia => "sepolia",
            Self::Mainnet => "mainnet",
        }
    }

    /// Every chain fact chibi needs for this network.
    pub fn spec(self) -> NetworkSpec {
        match self {
            Self::Sepolia => NetworkSpec {
                network: self,
                chain: Arc::clone(&SEPOLIA),
                helios: helios_ethereum::config::networks::Network::Sepolia,
                default_beacon: "https://ethereum-sepolia-beacon-api.publicnode.com",
                boot_nodes: sepolia_nodes(),
                tokens: SEPOLIA_TOKENS,
                clock: BlockClock {
                    number: sepolia::SEPOLIA_OSAKA_BLOCK,
                    timestamp: sepolia::SEPOLIA_OSAKA_TIMESTAMP,
                },
            },
            Self::Mainnet => NetworkSpec {
                network: self,
                chain: Arc::clone(&MAINNET),
                helios: helios_ethereum::config::networks::Network::Mainnet,
                default_beacon: "https://ethereum-beacon-api.publicnode.com",
                boot_nodes: mainnet_nodes(),
                tokens: MAINNET_TOKENS,
                clock: BlockClock {
                    number: mainnet::MAINNET_OSAKA_BLOCK,
                    timestamp: mainnet::MAINNET_OSAKA_TIMESTAMP,
                },
            },
        }
    }
}

impl fmt::Display for Network {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

impl FromStr for Network {
    type Err = UnknownNetwork;

    fn from_str(name: &str) -> Result<Self, UnknownNetwork> {
        Self::ALL
            .into_iter()
            .find(|network| network.name() == name)
            .ok_or_else(|| UnknownNetwork(name.to_owned()))
    }
}

/// A network name that is not `sepolia` or `mainnet`.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
#[error("unknown network {0:?}; expected sepolia or mainnet")]
pub struct UnknownNetwork(pub String);

/// Everything chibi knows about one network.
#[derive(Clone, Debug)]
pub struct NetworkSpec {
    /// The network these facts describe.
    pub network: Network,
    /// reth's chain spec. It drives both the devp2p fork id and the EVM fork schedule and blob
    /// parameters; its Amsterdam timestamp decides when headers start carrying BAL hashes.
    pub chain: Arc<ChainSpec>,
    /// helios's name for the network, which selects its fork schedule and default checkpoint.
    pub helios: helios_ethereum::config::networks::Network,
    /// Default beacon REST endpoint. It can stall the light client but cannot make it accept a
    /// wrong header.
    pub default_beacon: &'static str,
    /// devp2p boot nodes.
    pub boot_nodes: Vec<NodeRecord>,
    /// Tokens tracked for every watched address.
    pub tokens: &'static [Token],
    /// A block with a known number and timestamp, for estimating the head from the wall clock.
    pub clock: BlockClock,
}

impl NetworkSpec {
    /// The EIP-155 chain id.
    pub fn chain_id(&self) -> u64 {
        self.chain.chain.id()
    }
}

/// An ERC-20 token chibi tracks for every watched address.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Token {
    /// Ticker shown to the user.
    pub symbol: &'static str,
    /// Contract address.
    pub address: Address,
    /// Decimal places of one whole token.
    pub decimals: u8,
}

/// A post-merge block whose number and timestamp are known.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BlockClock {
    /// Block number.
    pub number: BlockNumber,
    /// Block timestamp in Unix seconds.
    pub timestamp: u64,
}

impl BlockClock {
    /// The head block number at Unix time `now`, assuming no missed slots since this block.
    /// Missed slots make the estimate run slightly ahead of the real head.
    pub const fn estimate(&self, now: u64) -> BlockNumber {
        self.number + now.saturating_sub(self.timestamp) / SLOT_SECONDS
    }
}

const SEPOLIA_TOKENS: &[Token] = &[Token {
    symbol: "USDC",
    address: address!("0x1c7D4B196Cb0C7B01d743Fbc6116a902379C7238"),
    decimals: 6,
}];

const MAINNET_TOKENS: &[Token] = &[Token {
    symbol: "USDC",
    address: address!("0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48"),
    decimals: 6,
}];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_round_trip_through_display_and_from_str() {
        for network in Network::ALL {
            assert_eq!(network.to_string().parse::<Network>(), Ok(network));
        }
        assert_eq!(Network::default(), Network::Sepolia);
    }

    #[test]
    fn unknown_and_miscased_names_are_rejected() {
        for name in ["", "Sepolia", "holesky", "mainnet "] {
            assert_eq!(
                name.parse::<Network>(),
                Err(UnknownNetwork(name.to_owned()))
            );
        }
    }

    #[test]
    fn serde_names_match_display() {
        for network in Network::ALL {
            let json = serde_json::to_string(&network).unwrap();
            assert_eq!(json, format!("\"{network}\""));
            assert_eq!(serde_json::from_str::<Network>(&json).unwrap(), network);
        }
    }

    #[test]
    fn specs_agree_with_helios_and_parse() {
        for network in Network::ALL {
            let spec = network.spec();
            assert_eq!(spec.network, network);
            let helios = spec.helios.to_base_config();
            assert_eq!(spec.chain_id(), helios.chain.chain_id);
            assert!(url::Url::parse(spec.default_beacon).is_ok());
            assert!(!spec.boot_nodes.is_empty());
        }
    }

    #[test]
    fn clock_never_runs_backwards_before_its_anchor() {
        let clock = BlockClock {
            number: 100,
            timestamp: 1_000,
        };
        assert_eq!(clock.estimate(0), 100);
        assert_eq!(clock.estimate(1_000), 100);
        assert_eq!(clock.estimate(1_000 + 12 * 50 + 11), 150);
    }
}
