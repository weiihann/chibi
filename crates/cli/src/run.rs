//! `chibi run` in the foreground or background, and `chibi stop`.

use std::collections::BTreeSet;
use std::convert::Infallible;
use std::ffi::OsString;
use std::fs::{self, OpenOptions};
use std::io;
use std::net::TcpListener;
use std::num::NonZeroU16;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use chibi_core::state::Key;
use chibi_node::{Disk, Health, Network, Node, NodeConfig, Startup};
use clap::Args;
use eyre::{WrapErr, bail};
use nix::errno::Errno;
use nix::sys::signal::{Signal, kill};
use nix::unistd::Pid;
use tokio::signal::unix::{SignalKind, signal};
use tokio::sync::watch;
use tokio::time::MissedTickBehavior;
use tracing::{info, warn};
use tracing_subscriber::EnvFilter;

use crate::config::{Config, ConfigArgs, RpcHost, process_env};
use crate::rpc::{self, RpcConfig};
use crate::tui::{self, LogBuffer, Wallet, transfer};

/// The background chibi's log, in its data directory.
pub const LOG_FILE: &str = "chibi.log";

/// How long `run -b` watches the child for an early exit before reporting it started.
const START_GRACE: Duration = Duration::from_secs(2);
/// How long `stop` waits for the data directory's lock to be released.
const STOP_TIMEOUT: Duration = Duration::from_secs(30);
/// How long blocking tasks get to finish once the node has stopped.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(5);
const POLL_INTERVAL: Duration = Duration::from_millis(100);
/// How long shutdown waits for in-flight JSON-RPC requests, so the keys they learn are saved.
const RPC_DRAIN: Duration = Duration::from_secs(5);
/// Log lines shown when a background start fails.
const LOG_TAIL_LINES: usize = 20;
/// How often a serving run logs its status.
const STATUS_EVERY: Duration = Duration::from_secs(60);
/// Most unheld pinned keys one status warning names.
const UNHELD_PINNED_NAMED: usize = 5;
/// Printed to stdout when the first state peer connects.
const FOUND_STATE_PEER: &str = concat!(
    "  /\\_/\\\n",
    " ( o.o )  found a state peer\n",
    "  > ^ <\n",
    " (\")_(\")\n",
);

/// What `Node::start` reports before its first wakeup.
pub const STARTING: Startup = Startup::Waiting {
    peers: 0,
    state_peers: 0,
    elapsed: Duration::ZERO,
};

/// `chibi run` flags.
#[derive(Debug, Default, Args)]
pub struct RunArgs {
    /// Network to follow instead of the configured one.
    #[arg(long)]
    pub network: Option<Network>,
    /// IP address or domain to serve JSON-RPC on instead of the configured one.
    #[arg(long)]
    pub rpc_host: Option<RpcHost>,
    /// JSON-RPC port instead of the configured one.
    #[arg(long)]
    pub rpc_port: Option<NonZeroU16>,
    #[command(flatten)]
    pub keys: ConfigArgs,
    /// Run in the background, logging to chibi.log in the data directory.
    #[arg(short = 'b', long)]
    pub background: bool,
    /// Set on the child that `--background` starts.
    #[arg(long, hide = true, conflicts_with = "background")]
    pub detached: bool,
    /// Open the terminal UI: send ETH or a registry token from your keystore while JSON-RPC
    /// keeps serving the wallet.
    #[arg(long, conflicts_with_all = ["background", "detached"])]
    pub tui: bool,
    /// Start the terminal UI with the log pane open.
    #[arg(long, requires = "tui")]
    pub logs: bool,
}

/// The config at `path`. Without a file there, a config the user `named` with `--config` or
/// `CHIBI_CONFIG` is an error, and the default path yields the defaults.
pub fn config(path: &Path, named: bool) -> eyre::Result<Config> {
    match Config::load(path)? {
        Some(config) => Ok(config),
        None if named => bail!("no config at {}", path.display()),
        None => {
            info!(
                "no config at {}; using the defaults. `chibi init` writes one there",
                path.display()
            );
            Ok(Config::default())
        }
    }
}

/// [`config`] with the command-line overrides applied.
pub fn load(path: &Path, named: bool, args: &RunArgs) -> eyre::Result<Config> {
    let mut config = config(path, named)?;
    if let Some(network) = args.network {
        config.network = network;
    }
    if let Some(host) = &args.rpc_host {
        config.rpc_host = host.clone();
    }
    if let Some(port) = args.rpc_port {
        config.rpc_port = port;
    }
    args.keys.apply(&mut config);
    config.validate()?;
    Ok(config)
}

/// Where `--tui` gets its key: the keystore the flag or the config names, which must be
/// readable, else a wallet set up in the UI and saved beside the config file at `path`.
pub fn tui_wallet(config: &Config, path: &Path) -> eyre::Result<Wallet> {
    match &config.keystore {
        Some(keystore) => {
            transfer::check_readable(keystore)?;
            Ok(Wallet::Keystore(keystore.clone()))
        }
        None => Ok(Wallet::Setup(path.to_owned())),
    }
}

/// Runs chibi until Ctrl-C or SIGTERM, or starts it in the background with `--background`,
/// or runs it under the terminal UI with `--tui`. `path` is absolute; `config_flag` is the
/// config the user named, which must exist, repeated in hints and passed to the background
/// child.
pub fn run(args: &RunArgs, path: &Path, config_flag: Option<&Path>) -> eyre::Result<()> {
    let logs = args.tui.then(LogBuffer::default);
    if !args.background {
        let subscriber = tracing_subscriber::fmt().with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        );
        match &logs {
            Some(buffer) => subscriber
                .with_ansi(false)
                .with_writer(buffer.clone())
                .init(),
            None => subscriber.with_ansi(!args.detached).init(),
        }
    }
    let config = load(path, config_flag.is_some(), args)?;
    let data_dir = config.network_dir(config.network, process_env)?;
    if args.background {
        return start_background(args, &config, &data_dir, config_flag);
    }
    let wallet = match &logs {
        Some(_) => Some(tui_wallet(&config, path)?),
        None => None,
    };
    let rpc_config = config.rpc()?;
    let listener = rpc::bind(rpc_config.addr)?;
    let served = Served {
        rpc: rpc_config,
        listener,
        url: config.rpc_url(),
    };
    let node_config = config.node_config(data_dir);
    let runtime = tokio::runtime::Runtime::new()?;
    let result = match (wallet, logs) {
        (Some(wallet), Some(logs)) => {
            tui::run(&runtime, node_config, served, wallet, &logs, args.logs)
        }
        _ => runtime.block_on(async {
            let progress = watch::Sender::new(STARTING);
            tokio::spawn(greet_state_peer(progress.subscribe()));
            serve(
                node_config,
                served,
                args.detached,
                progress,
                shutdown_signal(),
                |_| {},
            )
            .await
        }),
    };
    runtime.shutdown_timeout(SHUTDOWN_GRACE);
    result
}

/// The JSON-RPC side of a run: its settings, its already bound socket, and the URL wallets use.
pub struct Served {
    rpc: RpcConfig,
    listener: TcpListener,
    url: String,
}

/// Starts the node, reporting on `progress`, and serves JSON-RPC until `shutdown` resolves
/// with its cause. `started` gets the node once it is anchored.
pub async fn serve(
    node_config: NodeConfig,
    served: Served,
    detached: bool,
    progress: watch::Sender<Startup>,
    shutdown: impl Future<Output = io::Result<&'static str>>,
    started: impl FnOnce(&Node),
) -> eyre::Result<()> {
    if detached {
        let mut hangup = signal(SignalKind::hangup())?;
        tokio::spawn(async move {
            while hangup.recv().await.is_some() {
                info!("ignoring SIGHUP");
            }
        });
    }
    tokio::pin!(shutdown);
    info!(
        network = %node_config.network,
        watched = node_config.watch.addresses.len(),
        tokens = node_config.network.spec().tokens.len(),
        contracts = node_config.watch.contracts.len(),
        data_dir = %node_config.data_dir.display(),
        "starting"
    );
    let data_dir = node_config.data_dir.clone();
    let (node, follower) = tokio::select! {
        result = Node::start(node_config, progress) => result?,
        signal = &mut shutdown => {
            info!("shutting down on {}", signal?);
            return Ok(());
        }
    };
    started(&node);
    let server = rpc::start(node.clone(), served.rpc, served.listener)?;
    if let Err(err) = Disk::announce(&data_dir, &served.url) {
        warn!(%err, "cannot record the JSON-RPC URL for `chibi status`");
    }
    tokio::select! {
        result = follower.run() => result.wrap_err("head follower stopped"),
        never = report(node.clone()) => match never {},
        () = server.clone().stopped() => bail!("JSON-RPC server stopped"),
        signal = &mut shutdown => {
            info!("shutting down on {}", signal?);
            if server.stop().is_ok()
                && tokio::time::timeout(RPC_DRAIN, server.stopped()).await.is_err()
            {
                warn!("JSON-RPC requests still running at shutdown; keys they learn are not saved");
            }
            if let Err(err) = node.save_keys().await {
                warn!(%err, "cannot save the learned keys");
            }
            Ok(())
        }
    }
}

/// Logs the node's health every [`STATUS_EVERY`], starting at once.
async fn report(node: Node) -> Infallible {
    let mut ticks = tokio::time::interval(STATUS_EVERY);
    ticks.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let mut previous = BTreeSet::new();
    loop {
        ticks.tick().await;
        let health = node.health();
        log_status(&health, &previous);
        previous = health.unheld_pinned.into_iter().collect();
    }
}

/// Prints the cat that greets the first state peer, off the startup path.
async fn greet_state_peer(mut progress: watch::Receiver<Startup>) {
    let found = progress
        .wait_for(|startup| !matches!(startup, Startup::Waiting { .. }))
        .await;
    if found.is_ok() {
        tokio::task::spawn_blocking(|| {
            if let Err(err) = found_state_peer(&mut io::stdout().lock()) {
                warn!(%err, "cannot print to stdout");
            }
        });
    }
}

/// Writes the cat that greets the first state peer.
fn found_state_peer(out: &mut impl io::Write) -> io::Result<()> {
    out.write_all(FOUND_STATE_PEER.as_bytes())?;
    out.flush()
}

/// Logs one status line, and warnings for no state peer and for pinned keys `previous`, the
/// last line's unheld pinned keys, also lacked.
fn log_status(health: &Health, previous: &BTreeSet<Key>) {
    info!(
        head = health.head.number,
        peers = health.peers,
        state_peers = health.state_peers,
        pinned = %health.pinned,
        learned = %health.learned,
        "status",
    );
    if health.state_peers == 0 {
        warn!(
            "no state peer (snap/2) connected; the head and state requests stall until one returns"
        );
    }
    if let Some((count, named)) = persisting_unheld(previous, &health.unheld_pinned) {
        warn!(count, keys = %named, "pinned keys not held at the head for two status lines");
    }
}

/// The pinned keys unheld at this status line and at the one before, counted, with the first
/// [`UNHELD_PINNED_NAMED`] named. A key still warming after start is unheld at one line only.
fn persisting_unheld(previous: &BTreeSet<Key>, current: &[Key]) -> Option<(usize, String)> {
    let persisting: Vec<Key> = current
        .iter()
        .filter(|key| previous.contains(key))
        .copied()
        .collect();
    if persisting.is_empty() {
        return None;
    }
    let named: Vec<String> = persisting
        .iter()
        .take(UNHELD_PINNED_NAMED)
        .map(|key| match key {
            Key::Account(address) => format!("{address}"),
            Key::Slot(address, slot) => format!("{address} slot {slot:#x}"),
        })
        .collect();
    Some((persisting.len(), named.join(", ")))
}

/// Resolves with the name of the first Ctrl-C or SIGTERM.
pub async fn shutdown_signal() -> io::Result<&'static str> {
    let mut terminate = signal(SignalKind::terminate())?;
    tokio::select! {
        result = tokio::signal::ctrl_c() => result.map(|()| "Ctrl-C"),
        _ = terminate.recv() => Ok("SIGTERM"),
    }
}

/// The arguments for the detached child: the config the user named, as an absolute path so the
/// child reads the same file whatever its working directory, and the same `run` flags plus
/// `--detached`. Without a named config the child resolves the default path from the
/// environment it inherits.
pub fn detached_args(args: &RunArgs, config_flag: Option<&Path>) -> Vec<OsString> {
    let mut argv: Vec<OsString> = Vec::new();
    if let Some(path) = config_flag {
        argv.extend(["--config".into(), path.as_os_str().to_owned()]);
    }
    argv.extend(["run".into(), "--detached".into()]);
    if let Some(network) = args.network {
        argv.extend(["--network".into(), network.name().into()]);
    }
    if let Some(host) = &args.rpc_host {
        argv.extend(["--rpc-host".into(), host.to_string().into()]);
    }
    if let Some(port) = args.rpc_port {
        argv.extend(["--rpc-port".into(), port.to_string().into()]);
    }
    argv.extend(args.keys.to_args());
    argv
}

/// The `--config`, `--network` and `--data-dir` a `run` was given, for the `status` and `stop`
/// it suggests.
fn hint_flags(args: &RunArgs, config_flag: Option<&Path>) -> String {
    let mut flags = String::new();
    if let Some(path) = config_flag {
        flags.push_str(&format!(" --config {}", path.display()));
    }
    if let Some(network) = args.network {
        flags.push_str(&format!(" --network {network}"));
    }
    if let Some(dir) = &args.keys.data_dir {
        flags.push_str(&format!(" --data-dir {}", dir.display()));
    }
    flags
}

fn start_background(
    args: &RunArgs,
    config: &Config,
    data_dir: &Path,
    config_flag: Option<&Path>,
) -> eyre::Result<()> {
    let flags = hint_flags(args, config_flag);
    if let Some(holder) = Disk::running(data_dir)? {
        bail!(
            "chibi is already running on {} (pid {}); stop it with `chibi stop{flags}`",
            config.network,
            holder.pid
        );
    }
    fs::create_dir_all(data_dir)
        .wrap_err_with(|| format!("failed to create {}", data_dir.display()))?;
    let log_path = data_dir.join(LOG_FILE);
    let log = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .wrap_err_with(|| format!("failed to open {}", log_path.display()))?;
    let start = log.metadata()?.len();
    let exe = std::env::current_exe().wrap_err("cannot locate the chibi executable")?;
    let mut child = Command::new(exe)
        .args(detached_args(args, config_flag))
        .stdin(Stdio::null())
        .stdout(log.try_clone()?)
        .stderr(log)
        .process_group(0)
        .spawn()
        .wrap_err("failed to start chibi in the background")?;
    let deadline = Instant::now() + START_GRACE;
    while Instant::now() < deadline {
        if let Some(status) = child.try_wait()? {
            let written = fs::read(&log_path).unwrap_or_default();
            let since_start = written
                .get(usize::try_from(start).unwrap_or(usize::MAX)..)
                .unwrap_or_default();
            bail!(
                "chibi exited right after starting ({status}). Last log lines from {}:\n{}",
                log_path.display(),
                tail(&String::from_utf8_lossy(since_start), LOG_TAIL_LINES)
            );
        }
        thread::sleep(POLL_INTERVAL);
    }
    println!("chibi is running in the background (pid {})", child.id());
    println!("  log:      {}", log_path.display());
    println!(
        "  JSON-RPC: {} (chain id {}), served once the first head is verified",
        config.rpc_url(),
        config.network.spec().chain_id()
    );
    println!("  status:   chibi status{flags}");
    println!("  stop:     chibi stop{flags}");
    Ok(())
}

/// The last `lines` lines of `text`.
fn tail(text: &str, lines: usize) -> &str {
    let text = text.trim_end();
    let start = text
        .rmatch_indices('\n')
        .nth(lines.saturating_sub(1))
        .map_or(0, |(index, _)| index + 1);
    &text[start..]
}

/// Sends SIGTERM to the chibi running on `network` from `dir` and waits for it to release it.
pub fn stop(network: Network, dir: &Path) -> eyre::Result<()> {
    let Some(holder) = Disk::running(dir)? else {
        println!("chibi is not running on {network}");
        return Ok(());
    };
    let pid = holder.pid;
    let raw = i32::try_from(pid).wrap_err_with(|| format!("pid {pid} is out of range"))?;
    match kill(Pid::from_raw(raw), Signal::SIGTERM) {
        Ok(()) | Err(Errno::ESRCH) => {}
        Err(errno) => bail!("cannot signal chibi (pid {pid}): {errno}"),
    }
    println!("stopping chibi (pid {pid})...");
    let deadline = Instant::now() + STOP_TIMEOUT;
    while Disk::running(dir)?.is_some() {
        if Instant::now() >= deadline {
            bail!(
                "chibi (pid {pid}) is still running after {} s; see {}",
                STOP_TIMEOUT.as_secs(),
                dir.join(LOG_FILE).display()
            );
        }
        thread::sleep(POLL_INTERVAL);
    }
    println!("stopped");
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use url::Url;

    use super::*;

    fn strings(argv: Vec<OsString>) -> Vec<String> {
        argv.into_iter()
            .map(|arg| arg.into_string().unwrap())
            .collect()
    }

    fn keys(beacon: &str, data_dir: &str) -> ConfigArgs {
        ConfigArgs {
            beacon_url: Some(beacon.parse().unwrap()),
            data_dir: Some(PathBuf::from(data_dir)),
            ..ConfigArgs::default()
        }
    }

    #[test]
    fn detached_child_gets_the_named_config_and_repeats_the_run_flags() {
        let path = Path::new("/cfg/chibi.toml");
        let background = RunArgs {
            background: true,
            ..RunArgs::default()
        };
        assert_eq!(
            strings(detached_args(&background, None)),
            ["run", "--detached"]
        );
        assert_eq!(
            strings(detached_args(&background, Some(path))),
            ["--config", "/cfg/chibi.toml", "run", "--detached"]
        );
        let args = RunArgs {
            network: Some(Network::Mainnet),
            rpc_host: Some("chibi.local".parse().unwrap()),
            rpc_port: NonZeroU16::new(8745),
            keys: keys("http://127.0.0.1:5052/", "/srv/chibi"),
            background: true,
            ..RunArgs::default()
        };
        assert_eq!(
            strings(detached_args(&args, Some(path))),
            [
                "--config",
                "/cfg/chibi.toml",
                "run",
                "--detached",
                "--network",
                "mainnet",
                "--rpc-host",
                "chibi.local",
                "--rpc-port",
                "8745",
                "--beacon-url",
                "http://127.0.0.1:5052/",
                "--data-dir",
                "/srv/chibi"
            ]
        );
    }

    #[test]
    fn detached_args_parse_back_to_the_same_run() {
        use clap::Parser;

        let args = RunArgs {
            network: Some(Network::Sepolia),
            rpc_host: Some("::1".parse().unwrap()),
            rpc_port: NonZeroU16::new(9545),
            keys: keys("https://beacon.example/", "/srv/chibi"),
            background: true,
            ..RunArgs::default()
        };
        let path = Path::new("/cfg/chibi.toml");
        for named in [Some(path), None] {
            let argv = std::iter::once(OsString::from("chibi")).chain(detached_args(&args, named));
            let cli = crate::Cli::try_parse_from(argv).unwrap();
            assert_eq!(cli.config.as_deref(), named);
            let crate::Command::Run(parsed) = cli.command else {
                panic!("detached args parse to a command other than run");
            };
            assert!(parsed.detached && !parsed.background);
            assert_eq!(parsed.network, args.network);
            assert_eq!(parsed.rpc_host, args.rpc_host);
            assert_eq!(parsed.rpc_port, args.rpc_port);
            assert_eq!(parsed.keys, args.keys);
        }
    }

    #[test]
    fn tail_keeps_the_last_lines() {
        assert_eq!(tail("a\nb\nc\n", 2), "b\nc");
        assert_eq!(tail("a\nb", 5), "a\nb");
        assert_eq!(tail("", 3), "");
    }

    #[test]
    fn run_flags_override_the_config_for_that_run_only() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let text = "rpc_port = 9545\nbeacon_url = \"https://a.example/\"\ndata_dir = \"/srv/a\"\n";
        fs::write(&path, text).unwrap();
        let args = RunArgs {
            network: Some(Network::Mainnet),
            rpc_port: NonZeroU16::new(8745),
            keys: keys("http://127.0.0.1:5052/", "/srv/b"),
            ..RunArgs::default()
        };
        let config = load(&path, true, &args).unwrap();
        assert_eq!(config.network, Network::Mainnet);
        assert_eq!(config.rpc_url(), "http://127.0.0.1:8745");
        assert_eq!(
            config.beacon_url.as_ref().map(Url::as_str),
            Some("http://127.0.0.1:5052/")
        );
        assert_eq!(
            config.network_dir(config.network, |_| None).unwrap(),
            PathBuf::from("/srv/b/mainnet")
        );
        assert_eq!(fs::read_to_string(&path).unwrap(), text);

        let config = load(&path, true, &RunArgs::default()).unwrap();
        assert_eq!(
            config.beacon_url.as_ref().map(Url::as_str),
            Some("https://a.example/")
        );
        assert_eq!(config.data_dir, Some(PathBuf::from("/srv/a")));

        fs::write(&path, "tokens = [\"USDC\"]\n").unwrap();
        let error = format!("{:#}", load(&path, true, &args).unwrap_err());
        assert!(error.contains("`tokens` was removed"), "{error}");
    }

    fn enode(byte: u8) -> String {
        format!(
            "enode://{}@10.0.0.{byte}:30303",
            alloy_primitives::hex::encode([byte; 64])
        )
    }

    fn run_args(argv: &[&str]) -> RunArgs {
        use clap::Parser;

        let argv = ["chibi", "run"].iter().chain(argv);
        let crate::Command::Run(args) = crate::Cli::try_parse_from(argv).unwrap().command else {
            panic!("run parsed to another command");
        };
        args
    }

    #[test]
    fn peer_flags_override_the_config_and_reach_the_detached_child() {
        use std::num::NonZeroUsize;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let text = format!(
            "trusted_peers = [\"{}\"]\nmax_inbound_peers = 9\n",
            enode(1)
        );
        fs::write(&path, text).unwrap();
        let record = |byte| enode(byte).parse().unwrap();
        let args = run_args(&[
            "--trusted-peer",
            &enode(2),
            "--trusted-peer",
            &enode(3),
            "--trusted-only",
            "--boot-node",
            &enode(4),
            "--max-outbound-peers",
            "5",
            "--max-inbound-peers",
            "0",
            "--discovery-port",
            "30304",
            "--disable-discovery",
        ]);
        let config = load(&path, true, &args).unwrap();
        assert_eq!(config.trusted_peers, vec![record(2), record(3)]);
        assert!(config.trusted_only && config.disable_discovery);
        assert_eq!(config.boot_nodes, Some(vec![record(4)]));
        assert_eq!(config.max_outbound_peers, NonZeroUsize::new(5));
        assert_eq!(config.max_inbound_peers, Some(0));
        assert_eq!(config.discovery_port, Some(30304));

        let child = detached_args(&args, Some(&path))
            .into_iter()
            .map(|arg| arg.into_string().unwrap())
            .collect::<Vec<_>>();
        let child: Vec<&str> = child.iter().skip(4).map(String::as_str).collect();
        assert_eq!(run_args(&child).keys, args.keys);

        let plain = load(&path, true, &RunArgs::default()).unwrap();
        assert_eq!(plain.trusted_peers, vec![record(1)]);
        assert_eq!(plain.max_inbound_peers, Some(9));
        assert!(!plain.trusted_only && !plain.disable_discovery);

        fs::write(&path, "").unwrap();
        let error = format!(
            "{:#}",
            load(&path, true, &run_args(&["--trusted-only"])).unwrap_err()
        );
        assert!(error.contains("trusted_only = true needs"), "{error}");
        let error = format!(
            "{:#}",
            load(
                &path,
                true,
                &run_args(&["--network", "mainnet", "--disable-discovery"])
            )
            .unwrap_err()
        );
        assert!(error.contains("leaves nothing to dial"), "{error}");
    }

    #[test]
    fn the_keystore_comes_from_the_flag_or_the_config_and_without_one_the_tui_sets_one_up() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let keystore = dir.path().join("a.json");
        fs::write(&keystore, "{}").unwrap();
        fs::write(&path, format!("keystore = {keystore:?}\n")).unwrap();
        let config = load(&path, true, &run_args(&["--tui"])).unwrap();
        assert_eq!(config.keystore, Some(keystore.clone()));
        assert_eq!(
            tui_wallet(&config, &path).unwrap(),
            Wallet::Keystore(keystore)
        );

        let args = run_args(&["--tui", "--logs", "--keystore", "b.json"]);
        assert!(args.tui && args.logs);
        let config = load(&path, true, &args).unwrap();
        let flagged = config.keystore.clone().unwrap();
        assert!(flagged.is_absolute() && flagged.ends_with("b.json"));
        let error = tui_wallet(&config, &path).unwrap_err().to_string();
        assert!(error.starts_with("cannot read the keystore"), "{error}");

        fs::write(&path, "").unwrap();
        let config = load(&path, true, &run_args(&["--tui"])).unwrap();
        assert_eq!(
            tui_wallet(&config, &path).unwrap(),
            Wallet::Setup(path.clone())
        );

        fs::write(&path, "keystore = \"a.json\"\n").unwrap();
        let error = format!("{:#}", load(&path, true, &RunArgs::default()).unwrap_err());
        assert!(
            error.contains("keystore a.json is not an absolute path"),
            "{error}"
        );

        use clap::Parser;
        for argv in [
            &["chibi", "run", "--tui", "-b"][..],
            &["chibi", "run", "--tui", "--background"],
            &["chibi", "run", "--logs"],
        ] {
            assert!(crate::Cli::try_parse_from(argv).is_err(), "{argv:?}");
        }
    }

    #[test]
    fn run_without_a_config_at_the_default_path_uses_the_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let config = load(&path, false, &RunArgs::default()).unwrap();
        assert_eq!(config, Config::new(Network::Sepolia, Vec::new()));
        assert_eq!(config.rpc_url(), "http://127.0.0.1:8545");
        assert!(config.contracts.is_empty());
        assert_eq!(config.data_dir, None);
        assert!(!path.exists());

        let args = RunArgs {
            network: Some(Network::Mainnet),
            rpc_port: NonZeroU16::new(8745),
            ..RunArgs::default()
        };
        let config = load(&path, false, &args).unwrap();
        assert_eq!(config.network, Network::Mainnet);
        assert_eq!(config.rpc_url(), "http://127.0.0.1:8745");

        fs::write(&path, "network = \"goerli\"\n").unwrap();
        let error = load(&path, false, &RunArgs::default())
            .unwrap_err()
            .to_string();
        assert!(error.starts_with("invalid config "), "{error}");
    }

    #[test]
    fn a_named_config_that_does_not_exist_is_an_error_naming_it() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("typo.toml");
        let error = load(&path, true, &RunArgs::default())
            .unwrap_err()
            .to_string();
        assert!(error.contains(&path.display().to_string()), "{error}");
        let error = config(&path, true).unwrap_err().to_string();
        assert!(error.contains(&path.display().to_string()), "{error}");
        assert_eq!(config(&path, false).unwrap(), Config::default());
        assert!(!path.exists());
    }

    #[test]
    fn a_status_warning_names_at_most_five_pinned_keys_unheld_at_two_lines() {
        use alloy_primitives::{Address, U256};

        let accounts: Vec<Key> = (1..=6u8)
            .map(|byte| Key::Account(Address::repeat_byte(byte)))
            .collect();
        let slot = Key::Slot(Address::repeat_byte(0xcc), U256::from(2));
        let warming = Key::Account(Address::repeat_byte(0xdd));
        let mut current = accounts.clone();
        current.extend([slot, warming]);
        let previous: BTreeSet<Key> = accounts.iter().copied().chain([slot]).collect();

        assert_eq!(persisting_unheld(&BTreeSet::new(), &current), None);
        assert_eq!(persisting_unheld(&previous, &[]), None);
        assert_eq!(persisting_unheld(&previous, &[warming]), None);
        assert_eq!(
            persisting_unheld(&previous, &[slot]),
            Some((
                1,
                "0xCcCCccccCCCCcCCCCCCcCcCccCcCCCcCcccccccC slot 0x2".to_owned()
            ))
        );
        let (count, named) = persisting_unheld(&previous, &current).unwrap();
        assert_eq!(count, 7);
        assert_eq!(
            named,
            [1u8, 2, 3, 4, 5]
                .map(|byte| Address::repeat_byte(byte).to_string())
                .join(", ")
        );
    }

    #[test]
    fn the_first_state_peer_prints_a_plain_ascii_cat() {
        let mut out = Vec::new();
        found_state_peer(&mut out).unwrap();
        let printed = String::from_utf8(out).unwrap();
        assert_eq!(
            printed,
            concat!(
                "  /\\_/\\\n",
                " ( o.o )  found a state peer\n",
                "  > ^ <\n",
                " (\")_(\")\n",
            )
        );
        assert!(printed.is_ascii());
        assert!(printed.lines().all(|line| line == line.trim_end()));
    }

    #[test]
    fn hints_repeat_the_config_network_and_data_dir_flags() {
        assert_eq!(hint_flags(&RunArgs::default(), None), "");
        let args = RunArgs {
            network: Some(Network::Mainnet),
            keys: keys("http://127.0.0.1:5052/", "/srv/chibi"),
            background: true,
            ..RunArgs::default()
        };
        assert_eq!(
            hint_flags(&args, Some(Path::new("/cfg/chibi.toml"))),
            " --config /cfg/chibi.toml --network mainnet --data-dir /srv/chibi"
        );
    }
}
