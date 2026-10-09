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
use std::time::{Duration, Instant};

use alloy_primitives::TxHash;
use alloy_signer_local::PrivateKeySigner;
use chibi_node::{Holding, Node, NodeConfig, Peers, Startup, Token};
use crossterm::event::{self, Event, KeyEventKind};
use eyre::WrapErr;
use ratatui::Terminal;
use ratatui::backend::Backend;
use tokio::runtime::{Handle, Runtime};
use tokio::sync::{mpsc, oneshot, watch};
use tokio::task::{JoinHandle, JoinSet};
use tokio::time::MissedTickBehavior;
use tracing_subscriber::fmt::MakeWriter;

use self::app::{Action, App, Balances};
use self::transfer::{Quote, Quoted, UnlockError};
use self::view::View;
use crate::run::{self, STARTING, Served};

/// Log lines kept for the log pane.
const LOG_LINES: usize = 1_000;

/// How often the TUI animates, reads the node's health, and tracks a sent transfer.
const TICK: Duration = Duration::from_secs(1);

/// How long the key reader waits for input before checking whether the TUI has gone.
const KEY_POLL: Duration = Duration::from_millis(100);

/// How long the TUI waits at exit for the render thread to restore the terminal. A render
/// thread still blocked writing is left behind, so it cannot keep the process alive.
const RESTORE_WAIT: Duration = Duration::from_millis(500);

/// How often [`join_within`] checks whether its thread has ended.
const JOIN_POLL: Duration = Duration::from_millis(10);

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

/// A terminal that `restore` puts back when dropped. ratatui's panic hook restores it on a
/// panic too.
struct Restoring<B: Backend> {
    terminal: Terminal<B>,
    restore: fn(),
}

impl<B: Backend> Drop for Restoring<B> {
    fn drop(&mut self) {
        (self.restore)();
    }
}

/// Opens the terminal with `init`, restoring it with `restore` if `init` fails partway.
fn open<B: Backend, E>(
    init: impl FnOnce() -> Result<Terminal<B>, E>,
    restore: fn(),
) -> Result<Restoring<B>, E> {
    init()
        .map(|terminal| Restoring { terminal, restore })
        .inspect_err(|_| restore())
}

/// Draws the latest view until every sender is dropped, then restores the terminal. Runs on
/// its own thread, so a terminal whose writes block stalls only this thread.
fn render<B: Backend>(
    mut terminal: Restoring<B>,
    mut views: watch::Receiver<View>,
    runtime: &Handle,
) -> Result<(), B::Error> {
    loop {
        let latest = views.borrow_and_update().clone();
        terminal.terminal.draw(|frame| view::draw(frame, latest))?;
        if runtime.block_on(views.changed()).is_err() {
            return Ok(());
        }
    }
}

/// Joins `thread` if it ends within `wait`, else hands it back still running.
fn join_within<T>(
    thread: thread::JoinHandle<T>,
    wait: Duration,
) -> Result<thread::Result<T>, thread::JoinHandle<T>> {
    let deadline = Instant::now() + wait;
    while !thread.is_finished() {
        if Instant::now() >= deadline {
            return Err(thread);
        }
        thread::sleep(JOIN_POLL);
    }
    Ok(thread.join())
}

/// What a background task finished.
enum Done {
    Balances(Option<Balances>),
    Estimated(Box<(Quote, u64, Result<Quoted, String>)>),
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
    let app = App::new(node_config.network, spec.chain_id(), spec.tokens, logs_open);
    let terminal =
        open(ratatui::try_init, ratatui::restore).wrap_err("cannot open the terminal UI")?;
    let (views, latest) = watch::channel(View::new(&app, Vec::new()));
    let handle = runtime.handle().clone();
    let renderer = thread::spawn(move || render(terminal, latest, &handle));
    let (input_tx, input) = mpsc::unbounded_channel();
    let reader = thread::spawn(move || read_input(&input_tx));
    let mut ui = Ui {
        views,
        app,
        input,
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
        let progress = watch::Sender::new(STARTING);
        let startup = progress.subscribe();
        let (quit, quit_rx) = oneshot::channel::<()>();
        let (node_tx, node) = oneshot::channel::<Node>();
        let shutdown = async move {
            tokio::select! {
                signal = run::shutdown_signal() => signal,
                _ = quit_rx => Ok("the TUI's quit key"),
            }
        };
        let task = tokio::spawn(run::serve(
            node_config,
            served,
            false,
            progress,
            shutdown,
            move |node: &Node| {
                let _ = node_tx.send(node.clone());
            },
        ));
        let node_task = NodeTask {
            task,
            startup,
            node,
            quit,
        };
        ui.drive(node_task, Arc::new(signer)).await
    });
    drop(ui);
    let rendered = match join_within(renderer, RESTORE_WAIT) {
        Ok(Ok(rendered)) => rendered.wrap_err("cannot draw the terminal UI"),
        Ok(Err(_)) => Err(eyre::eyre!("the terminal UI panicked")),
        Err(_blocked) => {
            tracing::warn!("the terminal still blocks its writes; it is left as last drawn");
            Ok(())
        }
    };
    if reader.join().is_err() {
        tracing::warn!("the key reader panicked");
    }
    rendered.and(result)
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

/// The node's task as the UI sees it.
struct NodeTask<P: Peers> {
    /// `serve`, ending once the node has shut down.
    task: JoinHandle<eyre::Result<()>>,
    /// Its start-up progress.
    startup: watch::Receiver<Startup>,
    /// The node, once anchored.
    node: oneshot::Receiver<Node<P>>,
    /// Shuts the node down as a signal does.
    quit: oneshot::Sender<()>,
}

/// The UI's state, its input and log, and the channel to the render thread.
struct Ui<'a> {
    views: watch::Sender<View>,
    app: App,
    input: mpsc::UnboundedReceiver<Event>,
    logs: &'a LogBuffer,
}

impl Ui<'_> {
    /// Hands the render thread the screen for the current state.
    fn draw(&self) -> eyre::Result<()> {
        let logs = if self.app.logs {
            self.logs.tail(usize::from(view::LOG_PANE_LINES))
        } else {
            Vec::new()
        };
        self.views
            .send(View::new(&self.app, logs))
            .map_err(|_| eyre::eyre!("the terminal UI stopped drawing"))
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

    /// Runs the UI until the node task ends. The render thread draws, so a terminal that
    /// blocks its writes stalls the screen but not this loop: quit keys and signals still
    /// shut the node down, and every [`Node`] handle the UI holds is dropped before it
    /// returns.
    async fn drive<P: Peers>(
        &mut self,
        node_task: NodeTask<P>,
        signer: Arc<PrivateKeySigner>,
    ) -> eyre::Result<()> {
        let NodeTask {
            task: mut serving,
            mut startup,
            node: mut node_rx,
            quit,
        } = node_task;
        let mut quit = Some(quit);
        let mut startup_open = true;
        let mut node: Option<Node<P>> = None;
        let mut node_pending = true;
        let mut tasks = JoinSet::new();
        let mut ticks = tokio::time::interval(TICK);
        ticks.set_missed_tick_behavior(MissedTickBehavior::Delay);
        let ui: eyre::Result<eyre::Result<()>> = async {
            loop {
                self.draw()?;
                tokio::select! {
                    joined = &mut serving => {
                        return Ok(match joined {
                            Ok(served) => served,
                            Err(err) => Err(err).wrap_err("the node task failed"),
                        });
                    }
                    changed = startup.changed(), if startup_open => match changed {
                        Ok(()) => self.app.startup(*startup.borrow_and_update()),
                        Err(_) => startup_open = false,
                    },
                    started = &mut node_rx, if node_pending => {
                        node_pending = false;
                        if quit.is_some() {
                            node = started.ok();
                        }
                    }
                    Some(event) = self.input.recv() => {
                        let Event::Key(key) = event else { continue };
                        match self.app.key(key) {
                            Some(Action::Quit) => {
                                if let Some(quit) = quit.take() {
                                    let _ = quit.send(());
                                    self.app.notice = Some("shutting down...".to_owned());
                                    node = None;
                                    tasks.abort_all();
                                }
                            }
                            Some(Action::Send(draft, fee)) => {
                                if let Some(node) = node.clone() {
                                    let signer = Arc::clone(&signer);
                                    tasks.spawn(async move {
                                        let sent = transfer::send(&node, &signer, &draft, &fee)
                                            .await
                                            .map_err(|err| err.to_string());
                                        Done::Sent(sent)
                                    });
                                }
                            }
                            Some(Action::Unlock) | None => {}
                        }
                    }
                    Some(finished) = tasks.join_next() => match finished {
                        Ok(Done::Balances(balances)) => self.app.refreshed(balances),
                        Ok(Done::Estimated(estimated)) => {
                            let (quote, head, quoted) = *estimated;
                            self.app.estimated(quote, head, quoted);
                        }
                        Ok(Done::Sent(sent)) => self.app.sent(sent),
                        Err(err) if err.is_cancelled() => {}
                        Err(err) => return Err(err).wrap_err("a TUI task failed"),
                    },
                    _ = ticks.tick() => {
                        self.app.tick();
                        if let Some(node) = &node {
                            let health = node.health();
                            self.app.health(health.head.number, health.state_peers);
                            self.app.pinned(health.pinned.held, health.pinned.total);
                            if self.app.balances_request(health.head.number, health.pinned.held) {
                                refresh_balances(node, &signer, self.app.registry, &mut tasks);
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
                    tasks.spawn(async move {
                        let quoted = transfer::quote(&node, from, &quote)
                            .await
                            .map_err(|err| err.to_string());
                        Done::Estimated(Box::new((quote, head, quoted)))
                    });
                }
            }
        }
        .await;
        drop(node);
        tasks.shutdown().await;
        match ui {
            Ok(served) => served,
            Err(err) => {
                if let Some(quit) = quit.take() {
                    let _ = quit.send(());
                }
                if let Ok(Err(served)) = serving.await {
                    tracing::warn!(%served, "the node stopped with an error after the UI failed");
                }
                Err(err)
            }
        }
    }
}

/// Computes the display snapshot off the event loop and answers the sender's balances.
fn refresh_balances<P: Peers>(
    node: &Node<P>,
    signer: &PrivateKeySigner,
    registry: &'static [Token],
    tasks: &mut JoinSet<Done>,
) {
    let node = node.clone();
    let address = signer.address();
    tasks.spawn_blocking(move || {
        let snapshot = node.status().snapshot;
        let balances = snapshot
            .holdings
            .iter()
            .find(|h| h.address == address)
            .map(|holding| balances(holding, registry));
        Done::Balances(balances)
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
    use std::convert::Infallible;
    use std::io::Write as _;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::mpsc as std_mpsc;

    use alloy_primitives::{U256, address};
    use chibi_core::testing::{WorldAccount, eth};
    use chibi_node::fixture::Fixture;
    use chibi_node::{Disk, Network, Watchlist};
    use crossterm::event::KeyCode;
    use ratatui::backend::{ClearType, TestBackend, WindowSize};
    use ratatui::buffer::Cell;
    use ratatui::layout::{Position, Size};

    use super::*;

    /// A test terminal whose flush tells `entered`, then blocks until `release` is sent to or
    /// dropped.
    struct Stalled {
        screen: TestBackend,
        entered: std_mpsc::Sender<()>,
        release: std_mpsc::Receiver<()>,
    }

    impl Backend for Stalled {
        type Error = Infallible;

        fn draw<'a, I>(&mut self, content: I) -> Result<(), Infallible>
        where
            I: Iterator<Item = (u16, u16, &'a Cell)>,
        {
            self.screen.draw(content)
        }

        fn hide_cursor(&mut self) -> Result<(), Infallible> {
            self.screen.hide_cursor()
        }

        fn show_cursor(&mut self) -> Result<(), Infallible> {
            self.screen.show_cursor()
        }

        fn get_cursor_position(&mut self) -> Result<Position, Infallible> {
            self.screen.get_cursor_position()
        }

        fn set_cursor_position<P: Into<Position>>(
            &mut self,
            position: P,
        ) -> Result<(), Infallible> {
            self.screen.set_cursor_position(position)
        }

        fn clear(&mut self) -> Result<(), Infallible> {
            self.screen.clear()
        }

        fn clear_region(&mut self, clear_type: ClearType) -> Result<(), Infallible> {
            self.screen.clear_region(clear_type)
        }

        fn size(&self) -> Result<Size, Infallible> {
            self.screen.size()
        }

        fn window_size(&mut self) -> Result<WindowSize, Infallible> {
            self.screen.window_size()
        }

        fn flush(&mut self) -> Result<(), Infallible> {
            let _ = self.entered.send(());
            let _ = self.release.recv();
            Ok(())
        }
    }

    #[test]
    fn a_terminal_that_fails_to_open_is_restored() {
        static RESTORED: AtomicUsize = AtomicUsize::new(0);
        fn restored() {
            RESTORED.fetch_add(1, Ordering::SeqCst);
        }
        let failed = open::<TestBackend, _>(|| Err(io::Error::other("no tty")), restored);
        assert_eq!(
            failed.err().map(|err| err.to_string()).as_deref(),
            Some("no tty")
        );
        assert_eq!(RESTORED.load(Ordering::SeqCst), 1);

        let opened = open(|| Terminal::new(TestBackend::new(10, 2)), restored).unwrap();
        assert_eq!(RESTORED.load(Ordering::SeqCst), 1);
        drop(opened);
        assert_eq!(RESTORED.load(Ordering::SeqCst), 2);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn quitting_releases_the_node_while_the_terminal_blocks() {
        static RESTORED: AtomicUsize = AtomicUsize::new(0);
        fn restored() {
            RESTORED.fetch_add(1, Ordering::SeqCst);
        }
        let signer = PrivateKeySigner::from_slice(&[0x42; 32]).unwrap();
        let sender = signer.address();
        let fixture = Fixture::genesis([(sender, WorldAccount::eoa(eth(3)))]);
        let dir = tempfile::tempdir().unwrap();
        let (node, follower) = Node::with_parts(
            Fixture::spec(&[]),
            fixture.peers(),
            fixture.heads(),
            Disk::open(dir.path()).unwrap(),
            &Watchlist {
                addresses: vec![sender],
                contracts: Vec::new(),
            },
        )
        .await
        .unwrap();

        let (entered_tx, entered) = std_mpsc::channel();
        let (release, release_rx) = std_mpsc::channel();
        let stalled = Stalled {
            screen: TestBackend::new(100, 30),
            entered: entered_tx,
            release: release_rx,
        };
        let terminal = open(|| Terminal::new(stalled), restored).unwrap();
        let mut app = App::new(Network::Sepolia, 11_155_111, &[], false);
        app.unlocked(Ok(sender));
        let (views, latest) = watch::channel(View::new(&app, Vec::new()));
        let handle = Handle::current();
        let renderer = thread::spawn(move || render(terminal, latest, &handle));
        entered.recv_timeout(Duration::from_secs(5)).unwrap();

        let (input_tx, input) = mpsc::unbounded_channel();
        let logs = LogBuffer::default();
        let mut ui = Ui {
            views,
            app,
            input,
            logs: &logs,
        };
        let progress = watch::Sender::new(STARTING);
        let startup = progress.subscribe();
        let (quit, quit_rx) = oneshot::channel();
        let (node_tx, node_rx) = oneshot::channel();
        let task = tokio::spawn(async move {
            let _ = node_tx.send(node.clone());
            progress.send_replace(Startup::Serving);
            let _ = quit_rx.await;
            drop((node, follower));
            Ok(())
        });
        input_tx
            .send(Event::Key(KeyCode::Char('q').into()))
            .unwrap();
        let node_task = NodeTask {
            task,
            startup,
            node: node_rx,
            quit,
        };
        tokio::time::timeout(
            Duration::from_secs(5),
            ui.drive(node_task, Arc::new(signer)),
        )
        .await
        .expect("the UI ends while the terminal blocks")
        .unwrap();
        assert_eq!(ui.app.notice.as_deref(), Some("shutting down..."));
        assert!(Disk::running(dir.path()).unwrap().is_none());

        drop(ui);
        let renderer = join_within(renderer, Duration::from_millis(100))
            .expect_err("the render thread is still blocked");
        assert_eq!(RESTORED.load(Ordering::SeqCst), 0);
        drop(release);
        assert!(matches!(
            join_within(renderer, Duration::from_secs(5)),
            Ok(Ok(Ok(())))
        ));
        assert_eq!(RESTORED.load(Ordering::SeqCst), 1);
    }

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
