//! The network registry. Every chain-specific fact lives in one [`NetworkSpec`] per
//! [`Network`] and nowhere else.

use std::fmt;
use std::str::FromStr;
use std::sync::Arc;

use alloy_primitives::{Address, BlockNumber, address};
use reth_chainspec::{ChainSpec, MAINNET, SEPOLIA, mainnet, sepolia};
use reth_network_peers::{NodeRecord, mainnet_nodes, parse_nodes, sepolia_nodes};
use serde::{Deserialize, Serialize};

/// Sepolia peers seen serving snap/2 by a 12-minute census on 2026-10-05 (all Nethermind,
/// outbound sessions), less one that timed out on every dial on 2026-10-09. Few Sepolia peers
/// serve snap/2, so discovery alone can take minutes.
const SEPOLIA_SNAP_PEERS: [&str; 4] = [
    "enode://362709518ae0c4b870216dfd29d1034b4992212f0688c8ceabb1e6aa7619e3db116b9d1f9160c3c187d1b6763bd519dd3c79314cf489bc6aa12af53597d813aa@34.73.25.56:30303",
    "enode://4a4f8312ca02a1869d2634272ff557a12a183fa909c0824892cae43db95f84a8be8ffb7ba8cf2825e5bad37e3599b40cb955343a680b90c9607f241419c4ca68@104.196.102.71:30303",
    "enode://e78580877dcd5f35b2ff2c511c22eaf819c40c034b3f729b4324ece852e6ed2404e0c1f75a304df84a5fd03a0d8dbe339de139294416b44fccb897e9a7e73ffc@34.26.190.129:30303",
    "enode://f94e4fb5e306d643b0be79cc5fb46d82b975277810a119d82df29767a04b4e9547d52ab36cd1d8b8969a63e41861cc1c25459c9ce3890559d6f1100d1796757a@35.231.59.139:30303",
];

/// Seconds per slot on every supported network.
const SLOT_SECONDS: u64 = 12;

/// ENS's UniversalResolver on Sepolia, where it fronts ENSv2.
const SEPOLIA_ENS_RESOLVER: Address = address!("0xeEeEEEeE14D718C2B47D9923Deab1335E144EeEe");

/// ENS's UniversalResolver on mainnet.
pub(crate) const MAINNET_ENS_RESOLVER: Address =
    address!("0xeEeEEEeE14D718C2B47D9923Deab1335E144EeEe");

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
                default_beacon: "https://beacon.sepolia.ethpandaops.io/nimbus",
                boot_nodes: sepolia_nodes(),
                state_peers: parse_nodes(SEPOLIA_SNAP_PEERS),
                tokens: SEPOLIA_TOKENS,
                ens_resolver: SEPOLIA_ENS_RESOLVER,
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
                state_peers: Vec::new(),
                tokens: MAINNET_TOKENS,
                ens_resolver: MAINNET_ENS_RESOLVER,
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
    /// Known state peers (snap/2), dialed as static peers at start because discovery can take
    /// minutes to find one.
    pub state_peers: Vec<NodeRecord>,
    /// Tokens chibi knows on this network. Every one is pinned for each watched address.
    pub tokens: &'static [Token],
    /// ENS's UniversalResolver, whose `reverse` gives each watched address's primary name
    /// after checking that the name resolves back to the address.
    pub ens_resolver: Address,
    /// A block with a known number and timestamp, for estimating the head from the wall clock.
    pub clock: BlockClock,
}

impl NetworkSpec {
    /// The EIP-155 chain id.
    pub fn chain_id(&self) -> u64 {
        self.chain.chain.id()
    }

    /// The known token with this ticker, ignoring case.
    pub fn token(&self, symbol: &str) -> Option<Token> {
        self.tokens
            .iter()
            .find(|token| token.symbol.eq_ignore_ascii_case(symbol))
            .copied()
    }
}

/// An ERC-20 token chibi knows how to track for watched addresses.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Token {
    /// Ticker shown to the user.
    pub symbol: &'static str,
    /// Name shown when choosing tokens, including where a testnet deployment comes from.
    pub name: &'static str,
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

/// Sepolia deployments of the mainnet registry's tokens, where one exists.
const SEPOLIA_TOKENS: &[Token] = &[
    Token {
        symbol: "USDT",
        name: "Tether USD (Aave faucet test token, not issued by Tether)",
        address: address!("0xaA8E23Fb1079EA71e0a56F48a2aA51851D8433D0"),
        decimals: 6,
    },
    Token {
        symbol: "USDC",
        name: "USD Coin (Circle)",
        address: address!("0x1c7D4B196Cb0C7B01d743Fbc6116a902379C7238"),
        decimals: 6,
    },
    Token {
        symbol: "WETH",
        name: "Wrapped Ether",
        address: address!("0xfFf9976782d46CC05630D1f6eBAb18b2324d6B14"),
        decimals: 18,
    },
    Token {
        symbol: "WBTC",
        name: "Wrapped Bitcoin (Aave faucet test token, not issued by BitGo)",
        address: address!("0x29f2D40B0605204364af54EC677bD022dA425d03"),
        decimals: 8,
    },
    Token {
        symbol: "UNI",
        name: "Uniswap",
        address: address!("0x1f9840a85d5aF5bf1D1762F925BDADdC4201F984"),
        decimals: 18,
    },
    Token {
        symbol: "LINK",
        name: "Chainlink",
        address: address!("0x779877A7B0D9E8603169DdbD7836e478b4624789"),
        decimals: 18,
    },
    Token {
        symbol: "DAI",
        name: "Dai (Aave faucet test token, not issued by Maker)",
        address: address!("0xFF34B3d4Aee8ddCd6F9AFFFB6Fe49bD371b8a357"),
        decimals: 18,
    },
    Token {
        symbol: "PYUSD",
        name: "PayPal USD (Paxos)",
        address: address!("0xCaC524BcA292aaade2DF8A05cC58F0a65B1B3bB9"),
        decimals: 6,
    },
    Token {
        symbol: "AAVE",
        name: "Aave (Aave faucet test token)",
        address: address!("0x88541670E55cC00bEEFD87eB59EDd1b7C511AC9a"),
        decimals: 18,
    },
];

/// The 15 ERC-20s with the most transfer transactions on mainnet in the 30 days to 2026-10-07,
/// most used first.
const MAINNET_TOKENS: &[Token] = &[
    Token {
        symbol: "USDT",
        name: "Tether USD",
        address: address!("0xdAC17F958D2ee523a2206206994597C13D831ec7"),
        decimals: 6,
    },
    Token {
        symbol: "USDC",
        name: "USD Coin (Circle)",
        address: address!("0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48"),
        decimals: 6,
    },
    Token {
        symbol: "WETH",
        name: "Wrapped Ether",
        address: address!("0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2"),
        decimals: 18,
    },
    Token {
        symbol: "WBTC",
        name: "Wrapped Bitcoin",
        address: address!("0x2260FAC5E5542a773Aa44fBCfeDf7C193bc2C599"),
        decimals: 8,
    },
    Token {
        symbol: "UNI",
        name: "Uniswap",
        address: address!("0x1f9840a85d5aF5bf1D1762F925BDADdC4201F984"),
        decimals: 18,
    },
    Token {
        symbol: "LINK",
        name: "Chainlink",
        address: address!("0x514910771AF9Ca656af840dff83E8264EcF986CA"),
        decimals: 18,
    },
    Token {
        symbol: "QNT",
        name: "Quant",
        address: address!("0x4a220E6096B25EADb88358cb44068A3248254675"),
        decimals: 18,
    },
    Token {
        symbol: "DAI",
        name: "Dai",
        address: address!("0x6B175474E89094C44Da98b954EedeAC495271d0F"),
        decimals: 18,
    },
    Token {
        symbol: "PYUSD",
        name: "PayPal USD (Paxos)",
        address: address!("0x6c3ea9036406852006290770BEdFcAbA0e23A0e8"),
        decimals: 6,
    },
    Token {
        symbol: "ONDO",
        name: "Ondo",
        address: address!("0xfAbA6f8e4a5E8Ab82F62fe7C39859FA577269BE3"),
        decimals: 18,
    },
    Token {
        symbol: "AAVE",
        name: "Aave",
        address: address!("0x7Fc66500c84A76Ad7e9c93437bFc5Ac33E2DDaE9"),
        decimals: 18,
    },
    Token {
        symbol: "PEPE",
        name: "Pepe",
        address: address!("0x6982508145454Ce325dDbE47a25d4ec3d2311933"),
        decimals: 18,
    },
    Token {
        symbol: "SHIB",
        name: "Shiba Inu",
        address: address!("0x95aD61b0a150d79219dCF64E1E6Cc01f0B64C4cE"),
        decimals: 18,
    },
    Token {
        symbol: "1INCH",
        name: "1inch",
        address: address!("0x111111111117dC0aa78b770fA6A738034120C302"),
        decimals: 18,
    },
    Token {
        symbol: "CRV",
        name: "Curve DAO Token",
        address: address!("0xD533a949740bb3306d119CC777fa900bA034cd52"),
        decimals: 18,
    },
];

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
