# AGENTS.md

chibi is a light Ethereum node that serves a browser wallet's JSON-RPC on the user's machine. Every value it serves is proven from devp2p snap peers against headers a light client verified, and it comes with a terminal wallet.

Follow [RUST_BEST_PRACTICES.md](RUST_BEST_PRACTICES.md) for every Rust change.

## Layout

Dependencies point one way: `cli -> node -> core`.

- `crates/core` (`chibi-core`): pure logic, no I/O and no tokio.
  - `verify.rs`: the trust boundary, where peer bytes become verified values.
  - `state.rs`: the known-key `Store` and the canonical `State`.
  - `working.rs`: the keys chibi keeps proven (pins plus learned keys, capped).
  - `exec.rs`: the local EVM over a pinned store. `tx.rs`: submitted transactions and receipts.
  - `wire.rs`: untrusted requests and replies in chibi's own shapes. `testing.rs`: an in-process chain with real tries.
- `crates/node` (`chibi-node`): the async shell. reth-network and helios compile only here.
  - `node.rs`: the request-side API. `follow.rs`: the one head follower. `fetch.rs`: verified fetches.
  - `devp2p.rs`, `snap1.rs`, `peers.rs`: peers and snap. `beacon.rs`, `heads.rs`: the light client.
  - `disk.rs`: the data directory, its lock and `chibi.db`. `network.rs`: every per-chain fact.
  - `fixture.rs`: the offline test chain, with faults. `tests.rs`: node tests over it.
  - `examples/`: live Sepolia probes (`live_probe`, `live_node`) and the token registry printer.
- `crates/cli` (`chibi` binary): `main.rs` (commands), `init.rs`, `run.rs` (`node`, `wallet`, `stop`), `config.rs`, `rpc.rs` (the only place JSON-RPC types and error codes live), `names.rs`.
  - `tui/mod.rs`: the render thread and the control loop. `tui/app.rs`: the screens as a pure state machine.
  - `tui/view.rs`: `App` to `View` and drawing. `tui/transfer.rs`: unlock, name resolution, build and send.
  - `tui/wallet.rs`: create or import a wallet and write the keystore. `tui/cat.rs`: the pixel cats.
- `scripts/`: `oracle-check.sh` (compare a running chibi with a public RPC) and `registry-check.sh`.
- `.agents/skills/verify-chibi`: the skill that drives a real chibi on Sepolia and records evidence.

## Commands

```sh
cargo fmt --all
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-features
cargo install --locked --path crates/cli   # build and install the chibi binary
```

The toolchain is pinned in `rust-toolchain.toml`. All three checks must pass before a commit.

## Invariants

Do not break these. A change that needs to is a design change: say so first.

- `core::verify` is the only code that reads peer bytes. Its outputs (`VerifiedHeader`, `Proven`, and the rest) have private fields and no public constructors. Values enter a `Store` only from them.
- A key never fetched is a miss, never zero. Only a key proven absent reads as zero. An empty snap reply without a proof means the peer lacks the root, not that the key is absent.
- The canonical `State` sits behind one `parking_lot::Mutex`, reached only through `read` and `update`, which take synchronous closures. No guard crosses an await.
- Exactly one `Follower` exists, and only it advances or reanchors the store.
- JSON-RPC opens only after `Node::start` anchors at a verified head.
- Proof requests go to state peers: snap/2 sessions at once, snap/1 sessions once connected 3 s (`SNAP1_SETTLE`). BAL requests go only to snap/2, then eth/71, never snap/1.
- chibi never contacts a third-party RPC, gateway or service at runtime. A public RPC is only a test oracle. chibi never follows CCIP-read.
- The TUI's `View` never holds a password or a typed secret: they are masked before the view is built. The only secret it carries is a new recovery phrase on the Backup screen.
- Sepolia first. Mainnet is untested, and sending on mainnet stays disabled.

## Workflow

- Write the failing test first, then the fix.
- Make small commits, one change each, with plain messages written to a file and passed with `git commit -F`.
- Verify every user-facing change live with the verify-chibi skill. Unit tests do not prove behavior.
- Never touch the user's own chibi: ports 8545 and 8546, `~/.local/share/chibi`, `~/.config/chibi`, and the installed binary. Run your own with its own port and data directory.
- Push to main directly. Never force-push.
