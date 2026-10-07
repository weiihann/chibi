//! Prints every network's token registry as tab-separated `network symbol address decimals`
//! lines, so `scripts/registry-check.sh` checks the compiled-in registry against the chain
//! without a second copy of it.
//!
//! Run with `cargo run -q -p chibi-node --example registry`.

use std::io::{self, Write};

use chibi_node::Network;

fn main() -> io::Result<()> {
    let mut out = io::stdout().lock();
    for network in Network::ALL {
        for token in network.spec().tokens {
            writeln!(
                out,
                "{network}\t{}\t{}\t{}",
                token.symbol, token.address, token.decimals
            )?;
        }
    }
    Ok(())
}
