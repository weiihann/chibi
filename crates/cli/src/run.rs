//! `chibi run` in the foreground or background, and `chibi stop`.

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

use chibi_node::{Disk, Network, Node, NodeConfig};
use clap::Args;
use eyre::{WrapErr, bail};
use nix::errno::Errno;
use nix::sys::signal::{Signal, kill};
use nix::unistd::Pid;
use tokio::signal::unix::{SignalKind, signal};
use tracing::{info, warn};
use tracing_subscriber::EnvFilter;

use crate::config::{Config, RpcHost, process_env};
use crate::rpc::{self, RpcConfig};

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
    /// Run in the background, logging to chibi.log in the data directory.
    #[arg(short = 'b', long)]
    pub background: bool,
    /// Set on the child that `--background` starts.
    #[arg(long, hide = true, conflicts_with = "background")]
    pub detached: bool,
}

/// Loads the config at `path` with the command-line overrides applied. A network override
/// tracks the same token symbols at that network's addresses.
pub fn load(
    path: &Path,
    network: Option<Network>,
    rpc_host: Option<RpcHost>,
    rpc_port: Option<NonZeroU16>,
) -> eyre::Result<Config> {
    let mut config = Config::load(path)?;
    if let Some(network) = network {
        config.set_network(network)?;
    }
    if let Some(host) = rpc_host {
        config.rpc_host = host;
    }
    if let Some(port) = rpc_port {
        config.rpc_port = port;
    }
    Ok(config)
}

/// Runs chibi until Ctrl-C or SIGTERM, or starts it in the background with `--background`.
/// `path` is absolute; `config_flag` is the config the user named, repeated in hints.
pub fn run(args: &RunArgs, path: &Path, config_flag: Option<&Path>) -> eyre::Result<()> {
    let config = load(path, args.network, args.rpc_host.clone(), args.rpc_port)?;
    let data_dir = config.network_dir(config.network, process_env)?;
    if args.background {
        return start_background(args, &config, &data_dir, path, config_flag);
    }
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .with_ansi(!args.detached)
        .init();
    let rpc_config = config.rpc()?;
    let listener = rpc::bind(rpc_config.addr)?;
    let served = Served {
        rpc: rpc_config,
        listener,
        url: config.rpc_url(),
    };
    let runtime = tokio::runtime::Runtime::new()?;
    let result = runtime.block_on(serve(config.node_config(data_dir), served, args.detached));
    runtime.shutdown_timeout(SHUTDOWN_GRACE);
    result
}

/// The JSON-RPC side of a run: its settings, its already bound socket, and the URL wallets use.
struct Served {
    rpc: RpcConfig,
    listener: TcpListener,
    url: String,
}

async fn serve(node_config: NodeConfig, served: Served, detached: bool) -> eyre::Result<()> {
    if detached {
        let mut hangup = signal(SignalKind::hangup())?;
        tokio::spawn(async move {
            while hangup.recv().await.is_some() {
                info!("ignoring SIGHUP");
            }
        });
    }
    let shutdown = shutdown_signal();
    tokio::pin!(shutdown);
    info!(
        network = %node_config.network,
        watched = node_config.watch.addresses.len(),
        tokens = node_config.watch.tokens.len(),
        contracts = node_config.watch.contracts.len(),
        data_dir = %node_config.data_dir.display(),
        "starting"
    );
    let data_dir = node_config.data_dir.clone();
    let (node, follower) = tokio::select! {
        started = Node::start(node_config) => started?,
        signal = &mut shutdown => {
            info!("shutting down on {}", signal?);
            return Ok(());
        }
    };
    let server = rpc::start(node.clone(), served.rpc, served.listener)?;
    if let Err(err) = Disk::announce(&data_dir, &served.url) {
        warn!(%err, "cannot record the JSON-RPC URL for `chibi status`");
    }
    tokio::select! {
        result = follower.run() => result.wrap_err("head follower stopped"),
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

/// Resolves with the name of the first Ctrl-C or SIGTERM.
async fn shutdown_signal() -> io::Result<&'static str> {
    let mut terminate = signal(SignalKind::terminate())?;
    tokio::select! {
        result = tokio::signal::ctrl_c() => result.map(|()| "Ctrl-C"),
        _ = terminate.recv() => Ok("SIGTERM"),
    }
}

/// The arguments for the detached child: the config at its absolute `path`, so the child reads
/// the same file whatever its working directory, and the same `run` flags plus `--detached`.
pub fn detached_args(args: &RunArgs, path: &Path) -> Vec<OsString> {
    let mut argv: Vec<OsString> = vec![
        "--config".into(),
        path.as_os_str().to_owned(),
        "run".into(),
        "--detached".into(),
    ];
    if let Some(network) = args.network {
        argv.extend(["--network".into(), network.name().into()]);
    }
    if let Some(host) = &args.rpc_host {
        argv.extend(["--rpc-host".into(), host.to_string().into()]);
    }
    if let Some(port) = args.rpc_port {
        argv.extend(["--rpc-port".into(), port.to_string().into()]);
    }
    argv
}

/// The `--config` and `--network` a `run` was given, for the `status` and `stop` it suggests.
fn hint_flags(args: &RunArgs, config_flag: Option<&Path>) -> String {
    let mut flags = String::new();
    if let Some(path) = config_flag {
        flags.push_str(&format!(" --config {}", path.display()));
    }
    if let Some(network) = args.network {
        flags.push_str(&format!(" --network {network}"));
    }
    flags
}

fn start_background(
    args: &RunArgs,
    config: &Config,
    data_dir: &Path,
    path: &Path,
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
        .args(detached_args(args, path))
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

    use super::*;

    fn strings(argv: Vec<OsString>) -> Vec<String> {
        argv.into_iter()
            .map(|arg| arg.into_string().unwrap())
            .collect()
    }

    #[test]
    fn detached_child_gets_the_absolute_config_and_repeats_the_run_flags() {
        let path = Path::new("/cfg/chibi.toml");
        assert_eq!(
            strings(detached_args(
                &RunArgs {
                    background: true,
                    ..RunArgs::default()
                },
                path
            )),
            ["--config", "/cfg/chibi.toml", "run", "--detached"]
        );
        let args = RunArgs {
            network: Some(Network::Mainnet),
            rpc_host: Some("chibi.local".parse().unwrap()),
            rpc_port: NonZeroU16::new(8745),
            background: true,
            detached: false,
        };
        assert_eq!(
            strings(detached_args(&args, path)),
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
                "8745"
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
            background: true,
            detached: false,
        };
        let path = Path::new("/cfg/chibi.toml");
        let argv = std::iter::once(OsString::from("chibi")).chain(detached_args(&args, path));
        let cli = crate::Cli::try_parse_from(argv).unwrap();
        assert_eq!(cli.config.as_deref(), Some(path));
        let crate::Command::Run(parsed) = cli.command else {
            panic!("detached args parse to a command other than run");
        };
        assert!(parsed.detached && !parsed.background);
        assert_eq!(parsed.network, args.network);
        assert_eq!(parsed.rpc_host, args.rpc_host);
        assert_eq!(parsed.rpc_port, args.rpc_port);
    }

    #[test]
    fn tail_keeps_the_last_lines() {
        assert_eq!(tail("a\nb\nc\n", 2), "b\nc");
        assert_eq!(tail("a\nb", 5), "a\nb");
        assert_eq!(tail("", 3), "");
    }

    #[test]
    fn network_override_tracks_the_tokens_of_the_network_run() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        fs::write(&path, "tokens = [\"USDC\"]\n").unwrap();
        let config = load(&path, Some(Network::Mainnet), None, NonZeroU16::new(8745)).unwrap();
        assert_eq!(config.network, Network::Mainnet);
        assert_eq!(config.rpc_url(), "http://127.0.0.1:8745");
        let mainnet_usdc = Network::Mainnet.spec().token("USDC").unwrap();
        assert_eq!(
            config.node_config(PathBuf::from("/data")).watch.tokens,
            vec![mainnet_usdc]
        );

        fs::write(&path, "network = \"mainnet\"\ntokens = [\"PEPE\"]\n").unwrap();
        let error = load(&path, Some(Network::Sepolia), None, None)
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("unknown token \"PEPE\" on sepolia"),
            "{error}"
        );
    }

    #[test]
    fn run_without_a_config_file_uses_the_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let config = load(&path, None, None, None).unwrap();
        assert_eq!(config, Config::new(Network::Sepolia, Vec::new()));
        assert_eq!(config.rpc_url(), "http://127.0.0.1:8545");
        assert!(config.tokens.is_empty() && config.contracts.is_empty());
        assert_eq!(config.data_dir, None);
        assert!(!path.exists());

        let config = load(&path, Some(Network::Mainnet), None, NonZeroU16::new(8745)).unwrap();
        assert_eq!(config.network, Network::Mainnet);
        assert_eq!(config.rpc_url(), "http://127.0.0.1:8745");

        fs::write(&path, "network = \"goerli\"\n").unwrap();
        let error = load(&path, None, None, None).unwrap_err().to_string();
        assert!(error.starts_with("invalid config "), "{error}");
    }

    #[test]
    fn hints_repeat_the_config_and_network_flags() {
        assert_eq!(hint_flags(&RunArgs::default(), None), "");
        let args = RunArgs {
            network: Some(Network::Mainnet),
            background: true,
            ..RunArgs::default()
        };
        assert_eq!(
            hint_flags(&args, Some(Path::new("/cfg/chibi.toml"))),
            " --config /cfg/chibi.toml --network mainnet"
        );
    }
}
