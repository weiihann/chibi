# Rust best practices for chibi

These rules hold for every crate in this workspace. Where a lint enforces a rule, the lint wins.

## Lints

The lints live in `[workspace.lints]` in `Cargo.toml`, with test exemptions in `clippy.toml`. Clippy runs with `-D warnings`, so a warning fails the check.

- `unsafe_code` is forbidden. chibi verifies untrusted bytes; the `unsafe` it needs stays inside its dependencies.
- `unwrap_used` is denied outside tests. Return an error instead.
- `expect_used` warns outside tests. Use `expect` only for an invariant the code guarantees, with a message that states it, and an `#[allow(clippy::expect_used, reason = "...")]`. Production code has none today.
- `print_stdout` and `print_stderr` warn. Only the CLI prints, and it allows them once in `crates/cli/src/main.rs` with a reason. Libraries log through `tracing` and print nothing.
- `wildcard_enum_match_arm` warns. Match every variant, so a new variant is a compile error wherever it matters (see the exhaustive inbound request match in `crates/node/src/devp2p.rs`).
- `dbg_macro` and `unused_must_use` are denied. `redundant_clone`, `implicit_clone`, `cloned_instead_of_copied`, `needless_pass_by_value` and `uninlined_format_args` warn.

## Errors

- Libraries return typed errors built with `thiserror`, one enum per failure domain: `VerifyError` and `StateError` in core, `NodeError`, `NetError` and `Devp2pError` in node.
- The binary uses `eyre` only at its command edge (`main.rs`, `init.rs`, `run.rs`, `tui/mod.rs`), adding context with `wrap_err`. Modules under it return typed errors. `cli::rpc` is the one place errors become JSON-RPC codes.
- Keep variants that mean different things apart. `VerifyError` separates a lying peer from a peer that lacks the data (`*Unavailable`), because only the first is penalized.
- Word messages for the user: lowercase, no trailing period, name the value or path at fault, and say the fix when there is one. See `ConfigError::NoConfigDir` in `crates/cli/src/config.rs`: "cannot locate the config directory: neither XDG_CONFIG_HOME nor HOME is an absolute path; pass --config".

## Types

- Parse at the boundary into types that cannot hold invalid values. `Config` is checked at load (`#[serde(try_from = "ConfigFile")]`), so everything downstream trusts it.
- Use newtypes whose only constructor validates. `EnsName` is built only by `EnsName::parse` (`crates/cli/src/tui/app.rs`), and `TrustedHash` marks a hash the light client attested (`crates/core/src/verify.rs`).
- Make empty or invalid states unrepresentable. `Misses` (`crates/core/src/exec.rs`) is a first miss plus the rest, so it is never empty, and a slot lives inside its account, so a slot without a proven account cannot exist.
- Use enums over flags and options. `Session::Wallet` or `Session::Watch` (`tui/mod.rs`), `Snap::V1` or `Snap::V2`, `Stepped`, and `Entry::Absent` or `Entry::Present` each replace a boolean or a pair of options.
- Use `NonZero*` and other std types when they state the rule, as `DEFAULT_RPC_PORT` does.

## Async and concurrency

- Hold a `parking_lot` lock only inside a synchronous closure. The node's `State` is reached through `Shared::read` and `Shared::update` (`crates/node/src/node.rs`), and a closure cannot await, so no guard crosses an await.
- Run disk and CPU work on `spawn_blocking`: the state database writes (`node.rs`), keystore decryption and encryption (`tui/mod.rs`), and the status snapshot.
- Use `tokio::sync::watch` for latest-only values, so a slow reader never blocks the writer. The light client's heads (`heads.rs`), startup progress, peer sessions and the TUI's `View` all go through one.
- Make every await point cancellation safe. A future dropped in a `select!` must leave no half-applied state: `Follower::warm` (`follow.rs`) merges what completed before it stops, and `snap1.rs` drops a request whose caller left before it was sent.

## Performance

- Do not clone or allocate in hot paths without need. Pin an `Arc<Store>` instead of copying the store, and copy on write with `Arc::make_mut` only when a request adds values (`crates/core/src/state.rs`).
- Size buffers once when the size is known (`Vec::with_capacity` in `Follower::warm`).
- Pass borrowed data (`&str`, `&[T]`, `&Path`) unless the callee needs ownership. `needless_pass_by_value` catches most cases.
- Batch network round trips. A request proves all its misses in one parallel batch, so round trips grow with dependency depth, not key count.

## Tests

- Write the failing test first, then the change.
- Test behavior, with literal expected values: `assert_eq!(balance(&node, ALICE).await, Some(eth(4)))`, not a value recomputed by the code under test.
- Name a test as the sentence it proves, such as `a_snap1_session_that_closes_within_a_second_never_gets_a_request`.
- Use `#[tokio::test(start_paused = true)]` and `tokio::time::advance` for anything timed. No real sleeps (see the snap/1 settle tests in `devp2p.rs`).
- Drive the node through the offline fixture: `Fixture` and `Fault` in `crates/node/src/fixture.rs`, over the real tries of `chibi_core::testing::World`. `crates/cli/src/rpc/node_tests.rs` runs the RPC over the same fixture.
- Do not test wiring (that a function calls another) or wording (the exact text of a log line or message) unless the text is a contract, such as a log line the verify-chibi skill reads.
- Tests may use `unwrap`, `expect` and printing; `clippy.toml` allows them there.

## Dependencies

- reth and helios are pinned to git revs in `[workspace.dependencies]`. Move a rev on purpose, in its own commit, and rerun the live checks.
- Prefer declaring a dependency in `[workspace.dependencies]` and using `workspace = true` in the crates, so each version is set in one place.
- Adding a dependency needs a reason in the commit message: what it does that the existing set cannot, at what cost in build time and trust.
- reth-network and helios compile only into `chibi-node`, and JSON-RPC types stay in `cli::rpc`. `chibi-core` takes only reth's chain spec and trie code.

## Docs and comments

- Every public item has a doc comment that says what it is and why it exists. Module docs (`//!`) say what the module owns, as `verify.rs` does: "The trust boundary. Peer bytes become domain values here and nowhere else."
- Comments explain why the code is as it is: an invariant, a peer quirk, a reason a simpler way fails. Never narrate what the next line does.
- Write in plain short sentences.
