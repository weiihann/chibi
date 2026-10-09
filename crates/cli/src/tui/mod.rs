//! `chibi run --tui`: the node runs in-process, JSON-RPC keeps serving the wallet, and a
//! terminal UI sends ETH or a registry token from the user's keystore while a state peer is
//! connected.

pub mod app;
pub mod transfer;
pub mod view;


#[cfg(test)]
mod tests {
    use std::io::Write as _;

    use alloy_primitives::{U256, address};

    use super::*;

    #[test]
    fn the_log_buffer_keeps_the_last_thousand_lines() {
        let buffer = LogBuffer::default();
        for n in 0..1_005 {
            writeln!(buffer.make_writer(), "line {n}").unwrap();
        }
        let tail = buffer.tail(2_000);
        assert_eq!(tail.len(), 1_000);
        assert_eq!(
            (tail[0].as_str(), tail[999].as_str()),
            ("line 5", "line 1004")
        );
        assert_eq!(buffer.tail(2), ["line 1003", "line 1004"]);
    }

    #[test]
    fn tracing_writes_to_the_log_buffer() {
        let buffer = LogBuffer::default();
        let subscriber = tracing_subscriber::fmt()
            .with_ansi(false)
            .with_writer(buffer.clone())
            .finish();
        tracing::subscriber::with_default(subscriber, || tracing::info!(peers = 3, "hello"));
        let tail = buffer.tail(5);
        assert_eq!(tail.len(), 1);
        assert!(
            tail[0].ends_with("INFO chibi::tui::tests: hello peers=3"),
            "{tail:?}"
        );
    }

    #[test]
    fn balances_list_registry_tokens_held_with_a_nonzero_balance() {
        const fn token(symbol: &'static str) -> Token {
            Token {
                symbol,
                name: symbol,
                address: address!("0x00000000000000000000000000000000000070c3"),
                decimals: 6,
            }
        }
        let registry = [token("AAA"), token("BBB"), token("CCC"), token("DDD")];
        let holding = Holding {
            address: address!("0x17c5185167401ed00cf5f5b2fc97d9bbfdb7d025"),
            name: None,
            balance: Some(U256::from(7)),
            nonce: Some(0),
            tokens: vec![
                ("CCC".to_owned(), Some(U256::from(3))),
                ("BBB".to_owned(), Some(U256::ZERO)),
                ("DDD".to_owned(), None),
                ("AAA".to_owned(), Some(U256::from(1))),
            ],
        };
        assert_eq!(
            balances(&holding, &registry),
            Balances {
                eth: Some(U256::from(7)),
                tokens: vec![(registry[0], U256::from(1)), (registry[2], U256::from(3))],
            }
        );
    }
}
