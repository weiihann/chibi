//! `chibi run --tui`: the node runs in-process, JSON-RPC keeps serving the wallet, and a
//! terminal UI sends ETH or a registry token from the user's keystore while a state peer is
//! connected.

pub mod app;
pub mod transfer;
pub mod view;

use std::collections::VecDeque;
use std::io;
use std::path::Path;
use std::sync::{Arc, Mutex, PoisonError};
use std::thread;
use std::time::Duration;

use alloy_primitives::TxHash;
use alloy_signer_local::PrivateKeySigner;
use chibi_node::{Holding, Node, NodeConfig, Token};
use crossterm::event::{self, Event, KeyEventKind};
use eyre::WrapErr;
use ratatui::DefaultTerminal;
use tokio::runtime::Runtime;
use tokio::sync::{mpsc, oneshot, watch};
use tokio::time::MissedTickBehavior;
use tracing_subscriber::fmt::MakeWriter;

use self::app::{Action, App, Balances};
use self::transfer::{Quote, Quoted, UnlockError};
use crate::run::{self, STARTING, Served};

/// Log lines kept for the log pane.
const LOG_LINES: usize = 1_000;

/// How often the TUI animates, reads the node's health, and tracks a sent transfer.
const TICK: Duration = Duration::from_secs(1);

/// How long the key reader waits for input before checking whether the TUI has gone.
const KEY_POLL: Duration = Duration::from_millis(100);

/// The last [`LOG_LINES`] log lines. Under `--tui` the tracing subscriber writes here
/// instead of stdout, which the UI owns.
#[derive(Clone, Debug, Default)]
pub struct LogBuffer(Arc<Mutex<VecDeque<String>>>);

impl LogBuffer {
    /// The newest `count` lines, oldest first.
    pub fn tail(&self, count: usize) -> Vec<String> {
        let lines = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        lines
            .iter()
            .skip(lines.len().saturating_sub(count))
            .cloned()
            .collect()
    }

    fn push(&self, text: &str) {
        let mut lines = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        for line in text.lines() {
            if lines.len() == LOG_LINES {
                lines.pop_front();
            }
            lines.push_back(line.to_owned());
        }
    }
}

impl<'a> MakeWriter<'a> for LogBuffer {
    type Writer = LogWriter;

    fn make_writer(&'a self) -> LogWriter {
        LogWriter {
            buffer: self.clone(),
            bytes: Vec::new(),
        }
    }
}

/// One log event being written; its lines reach the buffer when it is dropped.
#[derive(Debug)]
pub struct LogWriter {
    buffer: LogBuffer,
    bytes: Vec<u8>,
}

impl io::Write for LogWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.bytes.extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Drop for LogWriter {
    fn drop(&mut self) {
        self.buffer.push(&String::from_utf8_lossy(&self.bytes));
    }
}

/// The terminal in raw mode on the alternate screen, restored when dropped. ratatui's panic
/// hook restores it on a panic too, and [`run`] restores it if initialization fails partway.
struct Terminal(DefaultTerminal);

impl Drop for Terminal {
    fn drop(&mut self) {
        ratatui::restore();
    }
}

/// What a background task finished.
enum Done {
    Balances(Option<Balances>),
    Estimated(Quote, u64, Result<Quoted, String>),
    Sent(Result<TxHash, String>),
}

/// Unlocks the keystore, then runs the node under the UI until the user quits, a signal
/// arrives, or the node stops.
pub fn run(
    runtime: &Runtime,
    mut node_config: NodeConfig,
    served: Served,
    keystore: &Path,
    logs: &LogBuffer,
    logs_open: bool,
) -> eyre::Result<()> {
    let spec = node_config.network.spec();
    let mut app = App::new(node_config.network, spec.chain_id(), spec.tokens, logs_open);
    let terminal = ratatui::try_init().map_err(|err| {
        ratatui::restore();
        err
    });
    let mut terminal = Terminal(terminal.wrap_err("cannot open the terminal UI")?);
    let (input_tx, mut input) = mpsc::unbounded_channel();
    let reader = thread::spawn(move || read_input(&input_tx));
    let mut ui = Ui {
        terminal: &mut terminal,
        app: &mut app,
        input: &mut input,
        logs,
    };
    let result = runtime.block_on(async {
        let Some(signer) = ui.unlock(keystore).await? else {
            return Ok(());
        };
        let address = signer.address();
        if !node_config.watch.addresses.contains(&address) {
            node_config.watch.addresses.push(address);
        }
        ui.drive(node_config, served, Arc::new(signer)).await
    });
    drop(input);
    drop(terminal);
    if reader.join().is_err() {
        tracing::warn!("the key reader panicked");
    }
    result
}

/// Forwards terminal input until the UI drops its receiver.
fn read_input(input: &mpsc::UnboundedSender<Event>) {
    while !input.is_closed() {
        match event::poll(KEY_POLL) {
            Ok(false) => {}
            Ok(true) => match event::read() {
                Ok(Event::Key(key)) if key.kind != KeyEventKind::Press => {}
                Ok(event) => {
                    if input.send(event).is_err() {
                        return;
                    }
                }
                Err(_) => return,
            },
            Err(_) => return,
        }
    }
}

/// The UI's terminal, state, input and log.
struct Ui<'a> {
    terminal: &'a mut Terminal,
    app: &'a mut App,
    input: &'a mut mpsc::UnboundedReceiver<Event>,
    logs: &'a LogBuffer,
}

impl Ui<'_> {
    fn draw(&mut self) -> eyre::Result<()> {
        let logs = if self.app.logs {
            self.logs.tail(usize::from(view::LOG_PANE_LINES))
        } else {
            Vec::new()
        };
        self.terminal
            .0
            .draw(|frame| view::draw(frame, self.app, &logs))?;
        Ok(())
    }

    /// Asks for the password until the keystore decrypts. `None` when the user quits first.
    async fn unlock(&mut self, keystore: &Path) -> eyre::Result<Option<PrivateKeySigner>> {
        let shutdown = run::shutdown_signal();
        tokio::pin!(shutdown);
        loop {
            self.draw()?;
            let event = tokio::select! {
                event = self.input.recv() => event,
                _ = &mut shutdown => return Ok(None),
            };
            let Some(Event::Key(key)) = event else {
                if event.is_none() {
                    return Ok(None);
                }
                continue;
            };
            match self.app.key(key) {
                Some(Action::Unlock) => {
                    self.draw()?;
                    let path = keystore.to_owned();
                    let password = self.app.take_password();
                    let (unlocked, password) = tokio::task::spawn_blocking(move || {
                        (transfer::unlock(&path, &password), password)
                    })
                    .await?;
                    self.app.return_password(password);
                    match unlocked {
                        Ok(signer) => {
                            self.app.unlocked(Ok(signer.address()));
                            return Ok(Some(signer));
                        }
                        Err(UnlockError::WrongPassword) => {
                            self.app
                                .unlocked(Err(UnlockError::WrongPassword.to_string()));
                        }
                        Err(err) => return Err(err.into()),
                    }
                }
                Some(Action::Quit) => return Ok(None),
                Some(Action::Send(..)) | None => {}
            }
        }
    }

    /// Runs the node and the UI together until the node stops.
    async fn drive(
        &mut self,
        node_config: NodeConfig,
        served: Served,
        signer: Arc<PrivateKeySigner>,
    ) -> eyre::Result<()> {
        let progress = watch::Sender::new(STARTING);
        let mut startup = progress.subscribe();
        let mut startup_open = true;
        let (quit_tx, quit_rx) = oneshot::channel::<()>();
        let mut quit_tx = Some(quit_tx);
        let (node_tx, mut node_rx) = oneshot::channel::<Node>();
        let mut node: Option<Node> = None;
        let mut node_pending = true;
        let shutdown = async move {
            tokio::select! {
                signal = run::shutdown_signal() => signal,
                _ = quit_rx => Ok("the TUI's quit key"),
            }
        };
        let serving = run::serve(
            node_config,
            served,
            false,
            progress,
            shutdown,
            move |node: &Node| {
                let _ = node_tx.send(node.clone());
            },
        );
        tokio::pin!(serving);
        let (done_tx, mut done) = mpsc::unbounded_channel::<Done>();
        let mut ticks = tokio::time::interval(TICK);
        ticks.set_missed_tick_behavior(MissedTickBehavior::Delay);
        loop {
            self.draw()?;
            tokio::select! {
                result = &mut serving => return result,
                changed = startup.changed(), if startup_open => match changed {
                    Ok(()) => self.app.startup(*startup.borrow_and_update()),
                    Err(_) => startup_open = false,
                },
                started = &mut node_rx, if node_pending => {
                    node_pending = false;
                    node = started.ok();
                }
                Some(event) = self.input.recv() => {
                    let Event::Key(key) = event else { continue };
                    match self.app.key(key) {
                        Some(Action::Quit) => {
                            if let Some(quit) = quit_tx.take() {
                                let _ = quit.send(());
                                self.app.notice = Some("shutting down...".to_owned());
                            }
                        }
                        Some(Action::Send(draft, fee)) => {
                            if let Some(node) = node.clone() {
                                let signer = Arc::clone(&signer);
                                let done_tx = done_tx.clone();
                                tokio::spawn(async move {
                                    let sent = transfer::send(&node, &signer, &draft, &fee)
                                        .await
                                        .map_err(|err| err.to_string());
                                    let _ = done_tx.send(Done::Sent(sent));
                                });
                            }
                        }
                        Some(Action::Unlock) | None => {}
                    }
                }
                Some(finished) = done.recv() => match finished {
                    Done::Balances(balances) => self.app.refreshed(balances),
                    Done::Estimated(quote, head, quoted) => {
                        self.app.estimated(quote, head, quoted);
                    }
                    Done::Sent(sent) => self.app.sent(sent),
                },
                _ = ticks.tick() => {
                    self.app.tick();
                    if let Some(node) = &node {
                        let health = node.health();
                        self.app.health(health.head.number, health.state_peers);
                        if self.app.balances_request(health.head.number, health.pinned.held) {
                            refresh_balances(node, &signer, self.app.registry, &done_tx);
                        }
                        if let Some(hash) = self.app.tracking()
                            && let Some(tracked) = transfer::track(node, hash)
                        {
                            self.app.tracked(tracked);
                        }
                    }
                }
            }
            if let (Some(node), Some(quote)) = (&node, self.app.estimate_request()) {
                let node = node.clone();
                let from = signer.address();
                let head = self.app.head;
                let done_tx = done_tx.clone();
                tokio::spawn(async move {
                    let quoted = transfer::quote(&node, from, &quote)
                        .await
                        .map_err(|err| err.to_string());
                    let _ = done_tx.send(Done::Estimated(quote, head, quoted));
                });
            }
        }
    }
}

/// Computes the display snapshot off the event loop and sends the sender's balances.
fn refresh_balances(
    node: &Node,
    signer: &PrivateKeySigner,
    registry: &'static [Token],
    done: &mpsc::UnboundedSender<Done>,
) {
    let node = node.clone();
    let address = signer.address();
    let done = done.clone();
    tokio::task::spawn_blocking(move || {
        let snapshot = node.status().snapshot;
        let balances = snapshot
            .holdings
            .iter()
            .find(|h| h.address == address)
            .map(|holding| balances(holding, registry));
        let _ = done.send(Done::Balances(balances));
    });
}

/// The ETH balance and every registry token held with a nonzero balance.
fn balances(holding: &Holding, registry: &[Token]) -> Balances {
    Balances {
        eth: holding.balance,
        tokens: registry
            .iter()
            .filter_map(|token| {
                holding
                    .tokens
                    .iter()
                    .find(|(symbol, _)| symbol == token.symbol)
                    .and_then(|(_, amount)| *amount)
                    .filter(|amount| !amount.is_zero())
                    .map(|amount| (*token, amount))
            })
            .collect(),
    }
}

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
