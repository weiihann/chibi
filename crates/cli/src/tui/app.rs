//! The TUI's screens as a state machine: keys, start-up progress, peer counts and node
//! results in; what the event loop should do out. No terminal and no network here.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use alloy_primitives::{Address, TxHash, U256};
use chibi_node::{Network, Startup, Token};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use zeroize::{Zeroize, Zeroizing};

use crate::config::{AddressError, parse_address};
use crate::format_units;
use crate::tui::transfer::{Asset, Draft, Fee, Quote, Quoted};
use crate::tui::wallet::{self, SECRET_BYTES, Secret};

/// Heads a sent transfer may stay pending before the screen says it is still pending.
pub const STILL_PENDING_HEADS: u64 = 10;

/// Shown on Review instead of signing on any network but Sepolia.
pub const MAINNET_REFUSED: &str = "sending on mainnet is not enabled yet";

/// Why nothing is sent while no state peer is connected.
pub const PAUSED: &str = "waiting for a state peer";

/// The longest password accepted, in bytes: its buffer's fixed capacity.
pub const PASSWORD_BYTES: usize = 256;

/// The shortest time the Excited screen stays up, so a fast anchor still shows it.
pub const EXCITED_FOR: Duration = Duration::from_millis(1500);

/// The fewest characters a new keystore's password may have.
pub const PASSWORD_MIN_CHARS: usize = 8;

/// Shown with the new recovery phrase.
pub const BACKUP_WARNING: &str = "write these words down. anyone with them controls this wallet. chibi will not show them again.";

/// Which screen is up.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Screen {
    /// No keystore is named: create, import, or point at one.
    Menu,
    /// The new recovery phrase, shown once.
    Backup,
    /// Two words of the new phrase asked back.
    Confirm,
    /// A private key or recovery phrase to import.
    Import,
    /// The path of an existing keystore file.
    Existing,
    /// The new keystore's password, typed twice.
    Password,
    /// The keystore password prompt, before the node starts.
    Unlock,
    /// Waiting for the first state peer.
    Waiting,
    /// A state peer connected; the node is anchoring.
    Excited,
    /// The transfer form.
    Ready,
    /// No state peer connected: nothing can be proven, so nothing is sent.
    Paused,
    /// The transfer as it will be signed.
    Review,
    /// A signed transfer, tracked until its receipt is verified.
    Sending,
}

impl Screen {
    /// Whether this is a step of setting up a wallet.
    pub const fn is_setup(self) -> bool {
        matches!(
            self,
            Self::Menu
                | Self::Backup
                | Self::Confirm
                | Self::Import
                | Self::Existing
                | Self::Password
        )
    }
}

/// A form field.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Field {
    /// ETH or a held registry token.
    Token,
    /// A decimal amount, or `max`.
    Amount,
    /// The recipient address.
    To,
}

/// What the event loop should do after a key.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Action {
    /// Decrypt the keystore with [`App::take_password`].
    Unlock,
    /// Encrypt [`App::take_secret`] with [`App::take_password`] and save it.
    Save,
    /// Unlock the keystore at this path from now on.
    Open(PathBuf),
    /// Sign and submit this transfer.
    Send(Draft, Fee),
    /// Shut the node down and leave.
    Quit,
}

/// Peer counts while waiting for the first state peer.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct WaitCounts {
    /// Connected devp2p sessions.
    pub peers: usize,
    /// Connected state peers.
    pub state_peers: usize,
    /// Time waited so far.
    pub elapsed: Duration,
}

/// The sender's balances at the latest display snapshot.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Balances {
    /// ETH in wei, `None` until proven.
    pub eth: Option<U256>,
    /// Registry tokens with a nonzero balance, in registry order.
    pub tokens: Vec<(Token, U256)>,
}

/// The transfer form's input and the messages shown beside each field.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Form {
    /// The focused field.
    pub field: Field,
    /// What is sent.
    pub asset: Asset,
    /// The amount as typed.
    pub amount: String,
    /// The recipient as typed.
    pub to: String,
    /// Why the amount cannot be sent.
    pub amount_error: Option<String>,
    /// Why the recipient cannot be sent to.
    pub to_error: Option<String>,
    /// Why the fee is not known.
    pub fee_error: Option<String>,
}

impl Default for Form {
    fn default() -> Self {
        Self {
            field: Field::Amount,
            asset: Asset::Eth,
            amount: String::new(),
            to: String::new(),
            amount_error: None,
            to_error: None,
            fee_error: None,
        }
    }
}

/// The fee quote for the form at one head.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Estimate {
    /// What was asked.
    pub quote: Quote,
    /// The head it was asked at.
    pub head: u64,
    /// The latest answer for this quote, `None` while the first is outstanding.
    pub quoted: Option<Result<Quoted, String>>,
}

/// A transfer ready to sign, or refused on this network.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Review {
    /// The transfer.
    pub draft: Draft,
    /// Its gas and fee caps.
    pub fee: Fee,
    /// Why it cannot be sent here, if it cannot.
    pub refused: Option<&'static str>,
}

/// Where a sent transfer is.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SendState {
    /// Being signed and submitted.
    Signing,
    /// Submitted, not yet in a verified block.
    Pending,
    /// In a verified block; its receipt is not verified yet.
    Included,
    /// The verified receipt has status 1.
    Succeeded,
    /// The verified receipt has status 0.
    Failed,
    /// The node refused it; nothing was sent.
    Rejected(String),
    /// Replaced or dropped after it was submitted: its nonce went to another transaction.
    Dropped,
}

/// What the node reports for a sent transfer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tracked {
    /// Not yet in a verified block.
    Pending,
    /// In a verified block, receipt not verified.
    Included,
    /// The receipt is verified with this status.
    Receipt {
        /// Status 1.
        success: bool,
    },
    /// The node no longer knows it: a transaction with its nonce replaced it or was mined.
    Dropped,
}

/// The latest transfer sent.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Sending {
    /// What was sent.
    pub draft: Draft,
    /// The transaction hash, once submitted.
    pub hash: Option<TxHash>,
    /// Where it is.
    pub state: SendState,
    /// Heads seen while it was pending.
    pub pending_heads: u64,
}

impl Sending {
    /// Whether the transfer may still be waiting in the outbox.
    pub const fn in_flight(&self) -> bool {
        matches!(
            self.state,
            SendState::Signing | SendState::Pending | SendState::Included
        )
    }

    /// Whether it has stayed pending for [`STILL_PENDING_HEADS`].
    pub const fn still_pending(&self) -> bool {
        matches!(self.state, SendState::Pending) && self.pending_heads >= STILL_PENDING_HEADS
    }
}

/// A way to get the wallet, on the setup menu.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Choice {
    /// A new recovery phrase.
    Create,
    /// A private key or recovery phrase the user has.
    Import,
    /// A keystore file the user has.
    Existing,
}

impl Choice {
    /// The menu, top to bottom.
    pub const ALL: [Self; 3] = [Self::Create, Self::Import, Self::Existing];

    /// The menu line.
    pub const fn label(self) -> &'static str {
        match self {
            Self::Create => "create a new wallet",
            Self::Import => "import a private key or recovery phrase",
            Self::Existing => "use an existing keystore file",
        }
    }
}

/// Which of the new password's two fields takes the keys.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Entry {
    /// The password, typed into [`App::password`].
    First,
    /// The same again, typed into [`Setup::again`].
    Again,
}

/// The wallet being set up when no keystore is named.
///
/// Not `Debug`: it holds the phrase, the key and the password being typed. Every buffer of
/// them is zeroizing, and those typed into are allocated once at a fixed capacity.
pub struct Setup {
    /// The highlighted menu line.
    pub choice: Choice,
    /// The home directory `~/` stands for in a keystore path.
    pub home: Option<PathBuf>,
    /// The new recovery phrase, from Backup until it is confirmed or discarded.
    pub phrase: Option<Zeroizing<String>>,
    /// The two 0-based word positions Confirm asks for.
    pub asked: [usize; 2],
    /// Which of them is being asked.
    pub asking: usize,
    /// A confirmation word or the secret to import, in a buffer of [`SECRET_BYTES`].
    pub input: Zeroizing<String>,
    /// The existing keystore's path as typed.
    pub path: String,
    /// The new password typed again, in a buffer of [`PASSWORD_BYTES`].
    pub again: Zeroizing<String>,
    /// The password field taking the keys.
    pub entry: Entry,
    /// Why the last step was refused.
    pub error: Option<String>,
    /// Whether the keystore is being encrypted and saved.
    pub saving: bool,
    secret: Option<Secret>,
}

impl Setup {
    fn new() -> Self {
        Self {
            choice: Choice::Create,
            home: None,
            phrase: None,
            asked: [0, 1],
            asking: 0,
            input: Zeroizing::new(String::with_capacity(SECRET_BYTES)),
            path: String::new(),
            again: Zeroizing::new(String::with_capacity(PASSWORD_BYTES)),
            entry: Entry::First,
            error: None,
            saving: false,
            secret: None,
        }
    }

    /// Drops the phrase and the secret and wipes the buffers, keeping the menu line.
    fn discard(&mut self) {
        self.phrase = None;
        self.secret = None;
        self.input.zeroize();
        self.again.zeroize();
        self.path.clear();
        self.asking = 0;
        self.entry = Entry::First;
        self.error = None;
        self.saving = false;
    }
}

/// The whole TUI state.
///
/// Not `Debug`: it holds the password being typed.
pub struct App {
    /// The screen up.
    pub screen: Screen,
    /// The followed network.
    pub network: Network,
    /// Its chain id.
    pub chain_id: u64,
    /// The network's registry tokens.
    pub registry: &'static [Token],
    /// Ticks so far, for animation.
    pub frame: u64,
    /// Whether the log pane is open.
    pub logs: bool,
    /// The password typed so far, in a buffer of [`PASSWORD_BYTES`] that is never
    /// reallocated and is wiped on unlock, on quit and on drop.
    pub password: Zeroizing<String>,
    /// Whether the keystore is being decrypted.
    pub unlocking: bool,
    /// Why the last unlock failed.
    pub unlock_error: Option<String>,
    /// Peer counts while waiting for a state peer.
    pub waiting: WaitCounts,
    /// The unlocked address.
    pub sender: Option<Address>,
    /// The sender's balances.
    pub balances: Balances,
    /// Whether some pinned key is not held yet, so held tokens may be missing from the
    /// balances. True until a health read says every pinned key is held.
    pub tokens_unproven: bool,
    /// The latest head number seen.
    pub head: u64,
    /// State peers connected at the latest health tick or start-up report.
    pub state_peers: usize,
    /// The transfer form.
    pub form: Form,
    /// The fee estimate for the form's draft.
    pub estimate: Option<Estimate>,
    /// Whether an estimate is outstanding.
    pub estimating: bool,
    /// The transfer on the Review screen.
    pub review: Option<Review>,
    /// The latest transfer sent.
    pub sending: Option<Sending>,
    /// A one-line message under the screen.
    pub notice: Option<String>,
    /// The wallet being set up.
    pub setup: Setup,
    quit_armed: bool,
    balances_asked: Option<(u64, usize)>,
    refreshing: bool,
    excited_since: Option<Instant>,
    serving: bool,
}

impl App {
    /// The Unlock screen for `network`.
    pub fn new(network: Network, chain_id: u64, registry: &'static [Token], logs: bool) -> Self {
        Self {
            screen: Screen::Unlock,
            network,
            chain_id,
            registry,
            frame: 0,
            logs,
            password: Zeroizing::new(String::with_capacity(PASSWORD_BYTES)),
            unlocking: false,
            unlock_error: None,
            waiting: WaitCounts::default(),
            sender: None,
            balances: Balances::default(),
            tokens_unproven: true,
            head: 0,
            state_peers: 0,
            form: Form::default(),
            estimate: None,
            estimating: false,
            review: None,
            sending: None,
            notice: None,
            quit_armed: false,
            setup: Setup::new(),
            balances_asked: None,
            refreshing: false,
            excited_since: None,
            serving: false,
        }
    }

    /// Advances the animation, and leaves Excited once it has been up for [`EXCITED_FOR`]
    /// and the node is serving.
    pub fn tick(&mut self, now: Instant) {
        self.frame += 1;
        self.leave_excited(now);
    }

    /// The keystore decrypted to `address`, or did not.
    pub fn unlocked(&mut self, result: Result<Address, String>) {
        self.unlocking = false;
        match result {
            Ok(address) => {
                self.sender = Some(address);
                self.unlock_error = None;
                self.notice = None;
                self.screen = Screen::Waiting;
            }
            Err(reason) => self.unlock_error = Some(format!("{reason}, try again")),
        }
    }

    /// Opens the setup menu, for a run that names no keystore. `home` is what `~/` stands
    /// for in a keystore path.
    pub fn setup(&mut self, home: Option<PathBuf>) {
        self.setup.home = home;
        self.screen = Screen::Menu;
    }

    /// The wallet was encrypted to a new keystore at `path` and is unlocked as `address`.
    pub fn saved(&mut self, address: Address, path: &Path) {
        self.setup.discard();
        self.unlocked(Ok(address));
        self.notice = Some(format!(
            "saved the keystore for {} to {}",
            address.to_checksum(None),
            path.display()
        ));
    }

    /// A keystore for `address` was already at `path`: it is kept, and unlocked as any other.
    pub fn kept(&mut self, address: Address, path: &Path) {
        self.setup.discard();
        self.screen = Screen::Unlock;
        self.notice = Some(format!(
            "a keystore for {} is already at {} and was not overwritten; enter its password",
            address.to_checksum(None),
            path.display()
        ));
    }

    /// The wallet could not be saved; setup starts over from the menu.
    pub fn save_failed(&mut self, reason: &str) {
        self.setup.discard();
        self.setup.error = Some(format!("cannot save the keystore: {reason}"));
        self.screen = Screen::Menu;
    }

    /// The confirmed phrase or imported key, to save while [`Setup::saving`].
    pub fn take_secret(&mut self) -> Option<Secret> {
        self.setup.secret.take()
    }

    /// The node's start-up progress, observed at `now`.
    pub fn startup(&mut self, startup: Startup, now: Instant) {
        match startup {
            Startup::Waiting {
                peers,
                state_peers,
                elapsed,
            } => {
                self.state_peers = state_peers;
                self.waiting = WaitCounts {
                    peers,
                    state_peers,
                    elapsed,
                };
            }
            Startup::FoundStatePeer => {
                self.state_peers = self.state_peers.max(1);
                self.excite(now);
            }
            Startup::Serving => {
                self.state_peers = self.state_peers.max(1);
                self.serving = true;
                self.excite(now);
                self.leave_excited(now);
            }
        }
    }

    /// Shows Excited from Waiting, from `now`.
    fn excite(&mut self, now: Instant) {
        if self.screen == Screen::Waiting {
            self.screen = Screen::Excited;
            self.excited_since = Some(now);
        }
    }

    /// Moves from Excited to the form once serving and Excited was up for [`EXCITED_FOR`].
    fn leave_excited(&mut self, now: Instant) {
        if self.screen == Screen::Excited
            && self.serving
            && self
                .excited_since
                .is_some_and(|since| now.duration_since(since) >= EXCITED_FOR)
        {
            self.screen = self.form_screen();
        }
    }

    /// The node's head and state peer count, read each tick once serving.
    pub fn health(&mut self, head: u64, state_peers: usize) {
        if head > self.head {
            if let Some(sending) = self
                .sending
                .as_mut()
                .filter(|s| s.state == SendState::Pending)
            {
                sending.pending_heads += head - self.head;
            }
            self.head = head;
        }
        self.state_peers = state_peers;
        match self.screen {
            Screen::Review if state_peers == 0 => {
                self.review = None;
                self.screen = Screen::Paused;
            }
            Screen::Ready | Screen::Paused => self.screen = self.form_screen(),
            Screen::Menu
            | Screen::Backup
            | Screen::Confirm
            | Screen::Import
            | Screen::Existing
            | Screen::Password
            | Screen::Unlock
            | Screen::Waiting
            | Screen::Excited
            | Screen::Review
            | Screen::Sending => {}
        }
    }

    /// The form when a state peer is connected, else Paused.
    const fn form_screen(&self) -> Screen {
        if self.state_peers == 0 {
            Screen::Paused
        } else {
            Screen::Ready
        }
    }

    /// The sender's balances at a new snapshot.
    pub fn balances(&mut self, balances: Balances) {
        self.balances = balances;
    }

    /// The node holds `held` of its `total` pinned keys.
    pub const fn pinned(&mut self, held: usize, total: usize) {
        self.tokens_unproven = held < total;
    }

    /// Whether to refresh the balances at `head` with `pinned_held` pinned keys held,
    /// marking it asked: when either changed since the last refresh, or while ETH is
    /// unproven. Never while a refresh is outstanding.
    pub fn balances_request(&mut self, head: u64, pinned_held: usize) -> bool {
        let at = (head, pinned_held);
        if self.refreshing || self.balances_asked == Some(at) && self.balances.eth.is_some() {
            return false;
        }
        self.balances_asked = Some(at);
        self.refreshing = true;
        true
    }

    /// A refresh finished, with the sender's balances if the snapshot holds them.
    pub fn refreshed(&mut self, balances: Option<Balances>) {
        self.refreshing = false;
        if let Some(balances) = balances {
            self.balances(balances);
        }
    }

    /// The quote to ask when the form's quote or the head changed since the last one,
    /// marking it asked. Only while the form is up.
    pub fn estimate_request(&mut self) -> Option<Quote> {
        if self.screen != Screen::Ready {
            return None;
        }
        let quote = self.quote()?;
        match &mut self.estimate {
            Some(estimate) if estimate.quote == quote => {
                if estimate.head == self.head || self.estimating {
                    return None;
                }
                estimate.head = self.head;
            }
            slot => {
                *slot = Some(Estimate {
                    quote,
                    head: self.head,
                    quoted: None,
                });
            }
        }
        self.estimating = true;
        Some(quote)
    }

    /// The quote asked at `head` answered.
    pub fn estimated(&mut self, quote: Quote, head: u64, result: Result<Quoted, String>) {
        self.estimating = false;
        if let Some(estimate) = self
            .estimate
            .as_mut()
            .filter(|e| e.quote == quote && e.head == head)
        {
            estimate.quoted = Some(result);
            self.form.fee_error = None;
        }
    }

    /// The node answered the submission.
    pub fn sent(&mut self, result: Result<TxHash, String>) {
        let Some(sending) = self.sending.as_mut() else {
            return;
        };
        match result {
            Ok(hash) => {
                sending.hash = Some(hash);
                sending.state = SendState::Pending;
            }
            Err(reason) => sending.state = SendState::Rejected(reason),
        }
    }

    /// The node's view of the sent transfer.
    pub fn tracked(&mut self, tracked: Tracked) {
        let Some(sending) = self.sending.as_mut().filter(|s| s.hash.is_some()) else {
            return;
        };
        sending.state = match tracked {
            Tracked::Pending => SendState::Pending,
            Tracked::Included => SendState::Included,
            Tracked::Receipt { success: true } => SendState::Succeeded,
            Tracked::Receipt { success: false } => SendState::Failed,
            Tracked::Dropped => SendState::Dropped,
        };
    }

    /// The hash to track, while the transfer has no verified receipt.
    pub fn tracking(&self) -> Option<TxHash> {
        self.sending
            .as_ref()
            .filter(|s| matches!(s.state, SendState::Pending | SendState::Included))
            .and_then(|s| s.hash)
    }

    /// ETH, then each held registry token: the choices for the token field.
    pub fn assets(&self) -> Vec<Asset> {
        std::iter::once(Asset::Eth)
            .chain(
                self.balances
                    .tokens
                    .iter()
                    .map(|(token, _)| Asset::Token(*token)),
            )
            .collect()
    }

    /// The sender's balance of `asset`; zero for a token it no longer holds.
    pub fn balance(&self, asset: Asset) -> Option<U256> {
        match asset {
            Asset::Eth => self.balances.eth,
            Asset::Token(token) => Some(
                self.balances
                    .tokens
                    .iter()
                    .find(|(held, _)| *held == token)
                    .map_or(U256::ZERO, |(_, amount)| *amount),
            ),
        }
    }

    /// The answer for the form's current quote.
    pub fn quoted(&self) -> Option<&Result<Quoted, String>> {
        let quote = self.quote()?;
        self.estimate
            .as_ref()
            .filter(|estimate| estimate.quote == quote)
            .and_then(|estimate| estimate.quoted.as_ref())
    }

    /// The fee answered for the form's current quote.
    pub fn fee(&self) -> Option<Result<&Fee, &String>> {
        self.quoted()
            .map(|quoted| quoted.as_ref().map(|quoted| &quoted.fee))
    }

    /// A warning about the recipient that does not stop the transfer.
    pub fn to_warning(&self) -> Option<String> {
        let Asset::Token(token) = self.form.asset else {
            return None;
        };
        let to = parse_address(&self.form.to).ok()?;
        (to == token.address).then(|| {
            format!(
                "this is the {} contract itself; tokens sent to it are usually lost",
                token.symbol
            )
        })
    }

    /// Handles one key press.
    pub fn key(&mut self, key: KeyEvent) -> Option<Action> {
        let ctrl_c =
            key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c');
        let key = Key::of(key.code);
        if self.screen == Screen::Unlock {
            return self.unlock_key(key, ctrl_c);
        }
        if self.screen.is_setup() {
            return self.setup_key(key, ctrl_c);
        }
        if ctrl_c || key == Some(Key::Char('q')) {
            return self.quit();
        }
        self.quit_armed = false;
        let key = key?;
        if key == Key::Char('l') {
            self.logs = !self.logs;
            return None;
        }
        match self.screen {
            Screen::Ready => {
                self.form_key(key);
                None
            }
            Screen::Review => self.review_key(key),
            Screen::Sending => {
                self.sending_key(key);
                None
            }
            Screen::Menu
            | Screen::Backup
            | Screen::Confirm
            | Screen::Import
            | Screen::Existing
            | Screen::Password
            | Screen::Unlock
            | Screen::Waiting
            | Screen::Excited
            | Screen::Paused => None,
        }
    }

    fn unlock_key(&mut self, key: Option<Key>, ctrl_c: bool) -> Option<Action> {
        if ctrl_c || key == Some(Key::Esc) {
            self.password.zeroize();
            return Some(Action::Quit);
        }
        if self.unlocking {
            return None;
        }
        match key? {
            Key::Char(c) if self.password.len() + c.len_utf8() <= PASSWORD_BYTES => {
                self.password.push(c);
            }
            Key::Backspace => erase_last(&mut self.password),
            Key::Enter if !self.password.is_empty() => {
                self.unlocking = true;
                return Some(Action::Unlock);
            }
            Key::Char(_) | Key::Enter | Key::Esc | Key::Up | Key::Down | Key::Left | Key::Right => {
            }
        }
        None
    }

    fn setup_key(&mut self, key: Option<Key>, ctrl_c: bool) -> Option<Action> {
        if ctrl_c || self.screen == Screen::Menu && key == Some(Key::Esc) {
            self.setup.discard();
            self.password.zeroize();
            return Some(Action::Quit);
        }
        if self.setup.saving {
            return None;
        }
        let key = key?;
        if key == Key::Esc {
            self.setup.discard();
            self.password.zeroize();
            self.screen = Screen::Menu;
            return None;
        }
        match self.screen {
            Screen::Menu => self.menu_key(key),
            Screen::Backup if key == Key::Enter => {
                self.setup.asked = wallet::positions();
                self.setup.asking = 0;
                self.screen = Screen::Confirm;
            }
            Screen::Confirm => self.confirm_key(key),
            Screen::Import => self.import_key(key),
            Screen::Existing => return self.existing_key(key),
            Screen::Password => return self.password_key(key),
            Screen::Backup
            | Screen::Unlock
            | Screen::Waiting
            | Screen::Excited
            | Screen::Ready
            | Screen::Paused
            | Screen::Review
            | Screen::Sending => {}
        }
        None
    }

    fn menu_key(&mut self, key: Key) {
        let choices = Choice::ALL;
        let at = choices
            .iter()
            .position(|choice| *choice == self.setup.choice)
            .unwrap_or_default();
        match key {
            Key::Down => self.setup.choice = choices[(at + 1) % choices.len()],
            Key::Up => self.setup.choice = choices[(at + choices.len() - 1) % choices.len()],
            Key::Enter => {
                self.setup.error = None;
                self.screen = match self.setup.choice {
                    Choice::Create => {
                        self.setup.phrase = Some(wallet::generate());
                        Screen::Backup
                    }
                    Choice::Import => Screen::Import,
                    Choice::Existing => Screen::Existing,
                };
            }
            Key::Char(_) | Key::Backspace | Key::Esc | Key::Left | Key::Right => {}
        }
    }

    fn confirm_key(&mut self, key: Key) {
        if key != Key::Enter {
            type_into(&mut self.setup.input, key, SECRET_BYTES);
            return;
        }
        let at = self.setup.asked[self.setup.asking];
        let right = self
            .setup
            .phrase
            .as_ref()
            .and_then(|phrase| phrase.split(' ').nth(at))
            .is_some_and(|word| word.eq_ignore_ascii_case(self.setup.input.trim()));
        self.setup.input.zeroize();
        if !right {
            self.setup.error = Some(format!(
                "that is not word {}; check what you wrote down",
                at + 1
            ));
            return;
        }
        self.setup.error = None;
        if self.setup.asking == 0 {
            self.setup.asking = 1;
            return;
        }
        self.setup.secret = self.setup.phrase.take().map(Secret::Phrase);
        self.password_screen();
    }

    fn import_key(&mut self, key: Key) {
        if key != Key::Enter {
            type_into(&mut self.setup.input, key, SECRET_BYTES);
            return;
        }
        let parsed = wallet::parse(&self.setup.input);
        self.setup.input.zeroize();
        match parsed {
            Ok(secret) => {
                self.setup.secret = Some(secret);
                self.password_screen();
            }
            Err(err) => self.setup.error = Some(err.to_string()),
        }
    }

    fn existing_key(&mut self, key: Key) -> Option<Action> {
        match key {
            Key::Char(c) => self.setup.path.push(c),
            Key::Backspace => {
                self.setup.path.pop();
            }
            Key::Enter => match self.keystore_path() {
                Ok(path) => {
                    self.setup.discard();
                    self.screen = Screen::Unlock;
                    return Some(Action::Open(path));
                }
                Err(reason) => self.setup.error = Some(reason),
            },
            Key::Esc | Key::Up | Key::Down | Key::Left | Key::Right => {}
        }
        None
    }

    /// The typed keystore path, absolute or under `~/`, if a file is there.
    fn keystore_path(&self) -> Result<PathBuf, String> {
        let text = self.setup.path.trim();
        if text.is_empty() {
            return Err("enter the keystore's path".to_owned());
        }
        let path = match (text.strip_prefix("~/"), &self.setup.home) {
            (Some(rest), Some(home)) => home.join(rest),
            _ => PathBuf::from(text),
        };
        if !path.is_absolute() {
            return Err("enter an absolute path, or one starting with ~/".to_owned());
        }
        if !path.is_file() {
            return Err(format!("no file at {}", path.display()));
        }
        Ok(path)
    }

    fn password_screen(&mut self) {
        self.setup.error = None;
        self.setup.entry = Entry::First;
        self.screen = Screen::Password;
    }

    fn password_key(&mut self, key: Key) -> Option<Action> {
        if key != Key::Enter {
            let field = match self.setup.entry {
                Entry::First => &mut self.password,
                Entry::Again => &mut self.setup.again,
            };
            type_into(field, key, PASSWORD_BYTES);
            return None;
        }
        match self.setup.entry {
            Entry::First if self.password.chars().count() < PASSWORD_MIN_CHARS => {
                self.password.zeroize();
                self.setup.error = Some(format!(
                    "a password needs at least {PASSWORD_MIN_CHARS} characters"
                ));
            }
            Entry::First => {
                self.setup.error = None;
                self.setup.entry = Entry::Again;
            }
            Entry::Again => {
                let same = self.password.as_str() == self.setup.again.as_str();
                self.setup.again.zeroize();
                self.setup.entry = Entry::First;
                if !same {
                    self.password.zeroize();
                    self.setup.error =
                        Some("the passwords do not match; enter them again".to_owned());
                    return None;
                }
                self.setup.error = None;
                self.setup.saving = true;
                return Some(Action::Save);
            }
        }
        None
    }

    /// The typed password, to decrypt the keystore with while [`App::unlocking`]. It comes
    /// back through [`App::return_password`].
    pub fn take_password(&mut self) -> Zeroizing<String> {
        std::mem::take(&mut self.password)
    }

    /// Wipes the password buffer after decrypting and keeps it for the next attempt.
    pub fn return_password(&mut self, mut password: Zeroizing<String>) {
        password.zeroize();
        self.password = password;
    }

    fn quit(&mut self) -> Option<Action> {
        let in_flight = self.sending.as_ref().is_some_and(Sending::in_flight);
        if in_flight && !self.quit_armed {
            self.quit_armed = true;
            self.notice = Some(
                "a transfer is still pending; chibi's outbox rebroadcasts it on the next run. press q again to quit"
                    .to_owned(),
            );
            return None;
        }
        Some(Action::Quit)
    }

    fn form_key(&mut self, key: Key) {
        const FIELDS: [Field; 3] = [Field::Token, Field::Amount, Field::To];
        let at = FIELDS
            .iter()
            .position(|field| *field == self.form.field)
            .unwrap_or_default();
        match key {
            Key::Down => self.form.field = FIELDS[(at + 1) % FIELDS.len()],
            Key::Up => self.form.field = FIELDS[(at + FIELDS.len() - 1) % FIELDS.len()],
            Key::Left | Key::Right if self.form.field == Field::Token => {
                let assets = self.assets();
                let at = assets
                    .iter()
                    .position(|asset| *asset == self.form.asset)
                    .unwrap_or_default();
                let step = if key == Key::Right {
                    1
                } else {
                    assets.len() - 1
                };
                self.form.asset = assets[(at + step) % assets.len()];
                self.form.amount_error = None;
            }
            Key::Char(c) => match self.form.field {
                Field::Amount if c.is_ascii_digit() || matches!(c, '.' | 'm' | 'a' | 'x') => {
                    self.form.amount.push(c);
                    self.form.amount_error = None;
                }
                Field::To if c.is_ascii_hexdigit() || c == 'x' => {
                    self.form.to.push(c);
                    self.form.to_error = None;
                }
                Field::Token | Field::Amount | Field::To => {}
            },
            Key::Backspace => match self.form.field {
                Field::Amount => {
                    self.form.amount.pop();
                    self.form.amount_error = None;
                }
                Field::To => {
                    self.form.to.pop();
                    self.form.to_error = None;
                }
                Field::Token => {}
            },
            Key::Enter => self.submit(),
            Key::Left | Key::Right | Key::Esc => {}
        }
    }

    fn review_key(&mut self, key: Key) -> Option<Action> {
        match key {
            Key::Char('y') => {
                let review = self.review.filter(|review| review.refused.is_none())?;
                self.review = None;
                self.sending = Some(Sending {
                    draft: review.draft,
                    hash: None,
                    state: SendState::Signing,
                    pending_heads: 0,
                });
                self.notice = None;
                self.screen = Screen::Sending;
                Some(Action::Send(review.draft, review.fee))
            }
            Key::Char('n') | Key::Esc => {
                self.review = None;
                self.screen = self.form_screen();
                None
            }
            Key::Char(_)
            | Key::Backspace
            | Key::Enter
            | Key::Up
            | Key::Down
            | Key::Left
            | Key::Right => None,
        }
    }

    fn sending_key(&mut self, key: Key) {
        let Some(sending) = &self.sending else {
            return;
        };
        if !matches!(key, Key::Enter | Key::Esc) || sending.state == SendState::Signing {
            return;
        }
        if sending.state == SendState::Succeeded {
            self.form.amount.clear();
            self.form.to.clear();
        }
        self.screen = self.form_screen();
    }

    /// Checks every field; on success shows Review with the quoted draft and fee. The fee is
    /// only judged once the other fields make a quote to ask.
    fn submit(&mut self) {
        let to = parse_recipient(&self.form.to);
        let quoted = self.quote().map(|_| match self.quoted() {
            Some(Ok(quoted)) => Ok(*quoted),
            Some(Err(reason)) => Err(reason.clone()),
            None => Err("waiting for the fee estimate".to_owned()),
        });
        let known_fee = quoted
            .as_ref()
            .and_then(|quoted| quoted.as_ref().ok())
            .map(|quoted| &quoted.fee);
        let amount = match self.amount() {
            Ok(Amount::Exact(amount)) => self.check_amount(amount, known_fee),
            Ok(Amount::Max) => self.check_max(),
            Err(reason) => Err(reason),
        };
        self.form.to_error = to.as_ref().err().cloned();
        self.form.fee_error = quoted
            .as_ref()
            .and_then(|quoted| quoted.as_ref().err().cloned());
        self.form.amount_error = amount.as_ref().err().cloned();
        let (Ok(_), Ok(()), Some(Ok(quoted))) = (to, amount, quoted) else {
            return;
        };
        self.review = Some(Review {
            draft: quoted.draft,
            fee: quoted.fee,
            refused: (self.network != Network::Sepolia).then_some(MAINNET_REFUSED),
        });
        self.screen = Screen::Review;
    }

    /// The amount field parsed for the chosen asset.
    fn amount(&self) -> Result<Amount, String> {
        let text = self.form.amount.trim();
        if text == "max" {
            return Ok(Amount::Max);
        }
        let asset = self.form.asset;
        parse_units(text, asset.decimals())
            .map(Amount::Exact)
            .map_err(|err| match err {
                UnitsError::Empty => "enter an amount".to_owned(),
                UnitsError::Malformed => "not a decimal amount".to_owned(),
                UnitsError::Decimals => format!(
                    "{} has at most {} decimals",
                    asset.symbol(),
                    asset.decimals()
                ),
            })
    }

    /// An exact amount checked against the balance, and for ETH against the balance less the
    /// max fee.
    fn check_amount(&self, amount: U256, fee: Option<&Fee>) -> Result<(), String> {
        let asset = self.form.asset;
        if amount.is_zero() {
            return Err("the amount must be more than 0".to_owned());
        }
        let balance = self
            .balance(asset)
            .ok_or_else(|| "the balance is not proven yet".to_owned())?;
        if amount > balance {
            return Err(format!(
                "more than your balance of {}",
                show(balance, asset)
            ));
        }
        if let (Asset::Eth, Some(fee)) = (asset, fee)
            && amount + fee.max_cost() > balance
        {
            return Err(format!(
                "the amount plus the max fee {} is more than your balance of {}",
                show(fee.max_cost(), Asset::Eth),
                show(balance, Asset::Eth)
            ));
        }
        Ok(())
    }

    /// `max` needs a proven balance, and a token one held. Whether the ETH balance covers its
    /// fee is the quote's to answer.
    fn check_max(&self) -> Result<(), String> {
        let asset = self.form.asset;
        let balance = self
            .balance(asset)
            .ok_or_else(|| "the balance is not proven yet".to_owned())?;
        match asset {
            Asset::Token(_) if balance.is_zero() => Err(format!("you hold no {}", asset.symbol())),
            Asset::Eth | Asset::Token(_) => Ok(()),
        }
    }

    /// What the fee is quoted for: the form's draft, `max` of a token as its balance, and
    /// `max` ETH as the balance less the fee of sending it.
    fn quote(&self) -> Option<Quote> {
        let to = parse_recipient(&self.form.to).ok()?;
        let asset = self.form.asset;
        Some(match (self.amount().ok()?, asset) {
            (Amount::Exact(amount), _) => Quote::Exact(Draft { asset, to, amount }),
            (Amount::Max, Asset::Eth) => Quote::MaxEth {
                to,
                balance: self.balance(asset)?,
            },
            (Amount::Max, Asset::Token(_)) => Quote::Exact(Draft {
                asset,
                to,
                amount: self.balance(asset)?,
            }),
        })
    }
}

/// The keys the screens act on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Key {
    Char(char),
    Backspace,
    Enter,
    Esc,
    Up,
    Down,
    Left,
    Right,
}

impl Key {
    /// The key a terminal key code means here; Tab moves like Down. `None` for keys the TUI
    /// ignores.
    #[expect(
        clippy::wildcard_enum_match_arm,
        reason = "crossterm has many keys the TUI ignores"
    )]
    const fn of(code: KeyCode) -> Option<Self> {
        Some(match code {
            KeyCode::Char(c) => Self::Char(c),
            KeyCode::Backspace => Self::Backspace,
            KeyCode::Enter => Self::Enter,
            KeyCode::Esc => Self::Esc,
            KeyCode::Up | KeyCode::BackTab => Self::Up,
            KeyCode::Down | KeyCode::Tab => Self::Down,
            KeyCode::Left => Self::Left,
            KeyCode::Right => Self::Right,
            _ => return None,
        })
    }
}

/// The amount field: a number, or everything that can be sent.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Amount {
    Exact(U256),
    Max,
}

/// `amount` of `asset` for display, with its symbol.
pub fn show(amount: U256, asset: Asset) -> String {
    format!(
        "{} {}",
        format_units(amount, asset.decimals()),
        asset.symbol()
    )
}

/// Types `key` into `buffer`, refusing characters that would grow it past `capacity`.
fn type_into(buffer: &mut String, key: Key, capacity: usize) {
    match key {
        Key::Char(c) if buffer.len() + c.len_utf8() <= capacity => buffer.push(c),
        Key::Backspace => erase_last(buffer),
        Key::Char(_) | Key::Enter | Key::Esc | Key::Up | Key::Down | Key::Left | Key::Right => {}
    }
}

/// Removes the last character and overwrites its bytes, which would otherwise stay in the
/// buffer's spare capacity. Writes within the capacity, so it never reallocates.
fn erase_last(password: &mut String) {
    if let Some(c) = password.pop() {
        let len = password.len();
        password.extend(std::iter::repeat_n('\0', c.len_utf8()));
        password.truncate(len);
    }
}

/// The recipient field as an address, with the reason it cannot be sent to.
fn parse_recipient(text: &str) -> Result<Address, String> {
    if text.trim().is_empty() {
        return Err("enter the recipient's address".to_owned());
    }
    match parse_address(text) {
        Ok(Address::ZERO) => Err("refusing to send to the zero address".to_owned()),
        Ok(address) => Ok(address),
        Err(AddressError::Malformed(_)) => Err("expected 0x followed by 40 hex digits".to_owned()),
        Err(AddressError::Checksum(_)) => Err(
            "the checksum does not match; check the address, or type it all lowercase".to_owned(),
        ),
    }
}

/// Why a decimal amount did not parse.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum UnitsError {
    Empty,
    Malformed,
    Decimals,
}

/// A decimal amount in base units of a token with `decimals` decimals.
fn parse_units(text: &str, decimals: u8) -> Result<U256, UnitsError> {
    if text.is_empty() {
        return Err(UnitsError::Empty);
    }
    let (whole, fraction) = text.split_once('.').unwrap_or((text, ""));
    let digits = |part: &str| part.bytes().all(|b| b.is_ascii_digit());
    if whole.is_empty() && fraction.is_empty() || !digits(whole) || !digits(fraction) {
        return Err(UnitsError::Malformed);
    }
    let fraction = fraction.trim_end_matches('0');
    if fraction.len() > usize::from(decimals) {
        return Err(UnitsError::Decimals);
    }
    let padded = format!(
        "{whole}{fraction}{}",
        "0".repeat(usize::from(decimals) - fraction.len())
    );
    U256::from_str_radix(&padded, 10).map_err(|_| UnitsError::Malformed)
}

#[cfg(test)]
mod tests {
    use alloy_primitives::{address, b256};

    use super::*;

    const SENDER: Address = address!("0x17c5185167401ed00cf5f5b2fc97d9bbfdb7d025");
    const BOB: &str = "0x0000000000000000000000000000000000000b0b";
    const TOKEN: Address = address!("0x00000000000000000000000000000000000070c3");
    const TKN: Token = Token {
        symbol: "TKN",
        name: "Test token",
        address: TOKEN,
        decimals: 6,
    };
    const REGISTRY: &[Token] = &[TKN];
    const SEPOLIA_ID: u64 = 11_155_111;
    const FEE: Fee = Fee {
        gas_limit: 21_000,
        max_fee_per_gas: 2_750_000_000,
        max_priority_fee_per_gas: 1_000_000_000,
    };

    fn press(app: &mut App, code: KeyCode) -> Option<Action> {
        app.key(KeyEvent::new(code, KeyModifiers::NONE))
    }

    fn typed(app: &mut App, text: &str) {
        for c in text.chars() {
            press(app, KeyCode::Char(c));
        }
    }

    fn eth(milli: u64) -> U256 {
        U256::from(milli) * U256::from(1_000_000_000_000_000u64)
    }

    /// Unlocked, serving with a state peer, holding 3 ETH and 1.5 TKN.
    fn ready(network: Network) -> App {
        let mut app = App::new(network, SEPOLIA_ID, REGISTRY, false);
        app.unlocked(Ok(SENDER));
        let now = Instant::now();
        app.startup(Startup::Serving, now);
        app.tick(now + EXCITED_FOR);
        app.health(100, 1);
        app.balances(Balances {
            eth: Some(eth(3_000)),
            tokens: vec![(TKN, U256::from(1_500_000))],
        });
        app
    }

    /// Fills the form, answers its quote with `fee` as a node whose fee does not depend on the
    /// amount, and presses Enter.
    fn submit(app: &mut App, amount: &str, to: &str, fee: Result<Fee, String>) {
        app.form.field = Field::Amount;
        typed(app, amount);
        app.form.field = Field::To;
        typed(app, to);
        if let Some(quote) = app.estimate_request() {
            let quoted = fee.map(|fee| Quoted {
                draft: match quote {
                    Quote::Exact(draft) => draft,
                    Quote::MaxEth { to, balance } => Draft {
                        asset: Asset::Eth,
                        to,
                        amount: balance.saturating_sub(fee.max_cost()),
                    },
                },
                fee,
            });
            app.estimated(quote, app.head, quoted);
        }
        press(app, KeyCode::Enter);
    }

    #[test]
    fn the_password_never_outgrows_its_buffer_and_is_wiped_on_quit() {
        let mut app = App::new(Network::Sepolia, SEPOLIA_ID, REGISTRY, false);
        let capacity = app.password.capacity();
        typed(&mut app, &"é".repeat(200));
        assert_eq!(
            (app.password.len(), app.password.capacity()),
            (256, capacity)
        );
        press(&mut app, KeyCode::Backspace);
        typed(&mut app, "abc");
        assert_eq!(app.password.len(), 256);
        assert!(app.password.ends_with("éab"));
        assert_eq!(press(&mut app, KeyCode::Esc), Some(Action::Quit));
        assert!(app.password.is_empty());
    }

    #[test]
    fn unlock_then_wait_then_perk_up_then_serve() {
        let mut app = App::new(Network::Sepolia, SEPOLIA_ID, REGISTRY, false);
        assert_eq!(app.screen, Screen::Unlock);
        assert_eq!(press(&mut app, KeyCode::Enter), None);
        typed(&mut app, "qlx");
        assert_eq!(press(&mut app, KeyCode::Enter), Some(Action::Unlock));
        assert!(app.unlocking);
        let capacity = app.password.capacity();
        let password = app.take_password();
        assert_eq!(password.as_str(), "qlx");
        app.return_password(password);
        assert_eq!(
            (app.password.as_str(), app.password.capacity()),
            ("", capacity)
        );
        app.unlocked(Err("wrong password".to_owned()));
        assert_eq!(
            (app.screen, app.unlock_error.as_deref()),
            (Screen::Unlock, Some("wrong password, try again"))
        );

        typed(&mut app, "hunter2");
        press(&mut app, KeyCode::Enter);
        app.unlocked(Ok(SENDER));
        assert_eq!((app.screen, app.sender), (Screen::Waiting, Some(SENDER)));

        let now = Instant::now();
        app.startup(
            Startup::Waiting {
                peers: 7,
                state_peers: 0,
                elapsed: Duration::from_secs(80),
            },
            now,
        );
        assert_eq!(
            (app.screen, app.waiting),
            (
                Screen::Waiting,
                WaitCounts {
                    peers: 7,
                    state_peers: 0,
                    elapsed: Duration::from_secs(80),
                }
            )
        );
        app.startup(Startup::FoundStatePeer, now);
        assert_eq!(app.screen, Screen::Excited);
        app.startup(Startup::Serving, now + Duration::from_millis(1499));
        assert_eq!(app.screen, Screen::Excited);
        app.tick(now + Duration::from_millis(1500));
        assert_eq!(app.screen, Screen::Ready);
    }

    #[test]
    fn the_excited_cat_shows_for_a_second_and_a_half_even_when_only_serving_is_seen() {
        let mut app = App::new(Network::Sepolia, SEPOLIA_ID, REGISTRY, false);
        app.unlocked(Ok(SENDER));
        let now = Instant::now();
        app.startup(Startup::Serving, now);
        assert_eq!(app.screen, Screen::Excited);
        app.tick(now + Duration::from_millis(1000));
        assert_eq!(app.screen, Screen::Excited);
        app.tick(now + Duration::from_millis(1500));
        assert_eq!(app.screen, Screen::Ready);
    }

    #[test]
    fn the_excited_cat_waits_for_serving_then_pauses_without_a_state_peer() {
        let mut app = App::new(Network::Sepolia, SEPOLIA_ID, REGISTRY, false);
        app.unlocked(Ok(SENDER));
        let now = Instant::now();
        app.startup(Startup::FoundStatePeer, now);
        app.tick(now + Duration::from_secs(5));
        assert_eq!(app.screen, Screen::Excited);
        app.startup(Startup::Serving, now + Duration::from_secs(6));
        assert_eq!(app.screen, Screen::Ready);

        let mut app = App::new(Network::Sepolia, SEPOLIA_ID, REGISTRY, false);
        app.unlocked(Ok(SENDER));
        app.startup(Startup::FoundStatePeer, now);
        app.startup(Startup::Serving, now + Duration::from_millis(100));
        app.health(100, 0);
        assert_eq!(app.screen, Screen::Excited);
        app.tick(now + Duration::from_millis(1500));
        assert_eq!(app.screen, Screen::Paused);
    }

    #[test]
    fn losing_every_state_peer_pauses_the_form_and_keeps_its_input() {
        let mut app = ready(Network::Sepolia);
        typed(&mut app, "1.5");
        app.health(101, 0);
        assert_eq!(app.screen, Screen::Paused);
        assert_eq!(press(&mut app, KeyCode::Enter), None);
        assert_eq!(app.screen, Screen::Paused);
        assert_eq!(app.estimate_request(), None);
        app.health(102, 2);
        assert_eq!(
            (app.screen, app.form.amount.as_str()),
            (Screen::Ready, "1.5")
        );

        submit(&mut app, "", BOB, Ok(FEE));
        assert_eq!(app.screen, Screen::Review);
        app.health(103, 0);
        assert_eq!((app.screen, app.review), (Screen::Paused, None));
        assert_eq!(press(&mut app, KeyCode::Char('y')), None);
        app.health(104, 1);
        assert_eq!(app.screen, Screen::Ready);
    }

    #[test]
    fn review_shows_the_transfer_and_y_sends_it() {
        let mut app = ready(Network::Sepolia);
        submit(&mut app, "1.25", BOB, Ok(FEE));
        let draft = Draft {
            asset: Asset::Eth,
            to: parse_address(BOB).unwrap(),
            amount: eth(1_250),
        };
        assert_eq!(app.screen, Screen::Review);
        assert_eq!(
            app.review,
            Some(Review {
                draft,
                fee: FEE,
                refused: None,
            })
        );
        assert_eq!(press(&mut app, KeyCode::Char('n')), None);
        assert_eq!(app.screen, Screen::Ready);
        press(&mut app, KeyCode::Enter);
        assert_eq!(
            press(&mut app, KeyCode::Char('y')),
            Some(Action::Send(draft, FEE))
        );
        assert_eq!(app.screen, Screen::Sending);
    }

    #[test]
    fn mainnet_shows_balances_but_review_refuses_to_send() {
        let mut app = ready(Network::Mainnet);
        submit(&mut app, "1", BOB, Ok(FEE));
        assert_eq!(app.screen, Screen::Review);
        assert_eq!(
            app.review.and_then(|review| review.refused),
            Some("sending on mainnet is not enabled yet")
        );
        assert_eq!(press(&mut app, KeyCode::Char('y')), None);
        assert_eq!(app.screen, Screen::Review);
    }

    fn errors(app: &App) -> [Option<&str>; 3] {
        [
            app.form.amount_error.as_deref(),
            app.form.to_error.as_deref(),
            app.form.fee_error.as_deref(),
        ]
    }

    #[test]
    fn losing_every_state_peer_while_sending_pauses_the_form_after_it() {
        let mut app = ready(Network::Sepolia);
        submit(&mut app, "1", BOB, Ok(FEE));
        press(&mut app, KeyCode::Char('y'));
        app.health(101, 0);
        assert_eq!(app.screen, Screen::Sending);
        app.sent(Ok(TxHash::ZERO));
        app.tracked(Tracked::Receipt { success: false });
        press(&mut app, KeyCode::Enter);
        assert_eq!(app.screen, Screen::Paused);
        press(&mut app, KeyCode::Enter);
        assert_eq!(press(&mut app, KeyCode::Char('y')), None);
        assert_eq!(
            (app.screen, app.form.amount.as_str()),
            (Screen::Paused, "1")
        );
        app.health(102, 1);
        assert_eq!(app.screen, Screen::Ready);
    }

    #[test]
    fn each_field_says_why_it_cannot_be_sent_and_keeps_its_input() {
        let checksum = "0x7f02C3E3c98b133055B8B348B2Ac625669Ed295d";
        for (asset, amount, to, expected) in [
            (
                Asset::Eth,
                "1",
                checksum,
                [
                    None,
                    Some(
                        "the checksum does not match; check the address, or type it all lowercase",
                    ),
                    None,
                ],
            ),
            (
                Asset::Eth,
                "1",
                "0x0000000000000000000000000000000000000000",
                [None, Some("refusing to send to the zero address"), None],
            ),
            (
                Asset::Eth,
                "1",
                "0x123",
                [None, Some("expected 0x followed by 40 hex digits"), None],
            ),
            (
                Asset::Eth,
                "",
                "",
                [
                    Some("enter an amount"),
                    Some("enter the recipient's address"),
                    None,
                ],
            ),
            (
                Asset::Token(TKN),
                "0.0000001",
                BOB,
                [Some("TKN has at most 6 decimals"), None, None],
            ),
            (
                Asset::Token(TKN),
                "0",
                BOB,
                [Some("the amount must be more than 0"), None, None],
            ),
            (
                Asset::Token(TKN),
                "1.6",
                BOB,
                [Some("more than your balance of 1.50 TKN"), None, None],
            ),
            (
                Asset::Eth,
                "3.1",
                BOB,
                [Some("more than your balance of 3.00 ETH"), None, None],
            ),
            (
                Asset::Eth,
                "3",
                BOB,
                [
                    Some(
                        "the amount plus the max fee 0.00005775 ETH is more than your balance of 3.00 ETH",
                    ),
                    None,
                    None,
                ],
            ),
            (
                Asset::Eth,
                "1.2.3",
                BOB,
                [Some("not a decimal amount"), None, None],
            ),
        ] {
            let mut app = ready(Network::Sepolia);
            app.form.asset = asset;
            submit(&mut app, amount, to, Ok(FEE));
            assert_eq!(
                (app.screen, errors(&app)),
                (Screen::Ready, expected),
                "{amount} to {to}"
            );
            assert_eq!(
                (app.form.amount.as_str(), app.form.to.as_str()),
                (amount, to)
            );
        }
    }

    #[test]
    fn a_reverting_or_unknown_fee_blocks_review() {
        let mut app = ready(Network::Sepolia);
        app.form.asset = Asset::Token(TKN);
        submit(
            &mut app,
            "1",
            BOB,
            Err("the transfer reverts: paused".to_owned()),
        );
        assert_eq!(
            (app.screen, errors(&app)),
            (
                Screen::Ready,
                [None, None, Some("the transfer reverts: paused")]
            )
        );

        let mut app = ready(Network::Sepolia);
        typed(&mut app, "1");
        app.form.field = Field::To;
        typed(&mut app, BOB);
        press(&mut app, KeyCode::Enter);
        assert_eq!(
            errors(&app),
            [None, None, Some("waiting for the fee estimate")]
        );
    }

    #[test]
    fn max_sends_the_token_balance_or_the_eth_balance_less_the_max_fee() {
        let mut app = ready(Network::Sepolia);
        app.form.asset = Asset::Token(TKN);
        submit(&mut app, "max", BOB, Ok(FEE));
        assert_eq!(
            app.review.map(|review| review.draft.amount),
            Some(U256::from(1_500_000))
        );

        let mut app = ready(Network::Sepolia);
        submit(&mut app, "max", BOB, Ok(FEE));
        assert_eq!(
            app.review.map(|review| review.draft.amount),
            Some(U256::from(2_999_942_250_000_000_000u64))
        );

        let mut app = ready(Network::Sepolia);
        app.balances(Balances {
            eth: Some(U256::from(1_000)),
            tokens: Vec::new(),
        });
        let short =
            "your balance of 0.000000000000001 ETH does not cover the max fee 0.00005775 ETH";
        submit(&mut app, "max", BOB, Err(short.to_owned()));
        assert_eq!(
            (app.screen, errors(&app)),
            (Screen::Ready, [None, None, Some(short)])
        );

        let mut app = ready(Network::Sepolia);
        app.form.asset = Asset::Token(TKN);
        app.balances(Balances {
            eth: Some(eth(3_000)),
            tokens: Vec::new(),
        });
        submit(&mut app, "max", BOB, Ok(FEE));
        assert_eq!(errors(&app), [Some("you hold no TKN"), None, None]);
    }

    #[test]
    fn a_token_sent_to_its_own_contract_warns_without_blocking() {
        let mut app = ready(Network::Sepolia);
        press(&mut app, KeyCode::Up);
        assert_eq!(app.form.field, Field::Token);
        press(&mut app, KeyCode::Right);
        assert_eq!(app.form.asset, Asset::Token(TKN));
        press(&mut app, KeyCode::Right);
        assert_eq!(app.form.asset, Asset::Eth);
        press(&mut app, KeyCode::Left);
        assert_eq!(app.form.asset, Asset::Token(TKN));
        submit(
            &mut app,
            "1",
            "0x00000000000000000000000000000000000070c3",
            Ok(FEE),
        );
        assert_eq!(
            app.to_warning().as_deref(),
            Some("this is the TKN contract itself; tokens sent to it are usually lost")
        );
        assert_eq!(app.screen, Screen::Review);
    }

    #[test]
    fn a_sent_transfer_is_tracked_to_its_verified_receipt() {
        let hash = b256!("0x1111111111111111111111111111111111111111111111111111111111111111");
        let mut app = ready(Network::Sepolia);
        submit(&mut app, "1", BOB, Ok(FEE));
        press(&mut app, KeyCode::Char('y'));
        assert_eq!(app.tracking(), None);
        app.sent(Ok(hash));
        assert_eq!(app.tracking(), Some(hash));
        app.tracked(Tracked::Pending);
        app.health(109, 1);
        assert!(!app.sending.as_ref().unwrap().still_pending());
        app.health(110, 1);
        assert!(app.sending.as_ref().unwrap().still_pending());

        assert_eq!(press(&mut app, KeyCode::Char('q')), None);
        assert_eq!(
            app.notice.as_deref(),
            Some(
                "a transfer is still pending; chibi's outbox rebroadcasts it on the next run. press q again to quit"
            )
        );
        app.tracked(Tracked::Included);
        app.tracked(Tracked::Receipt { success: true });
        assert_eq!(
            app.sending.as_ref().map(|s| &s.state),
            Some(&SendState::Succeeded)
        );
        assert_eq!(app.tracking(), None);
        press(&mut app, KeyCode::Enter);
        assert_eq!((app.screen, app.form.amount.as_str()), (Screen::Ready, ""));
        assert_eq!(press(&mut app, KeyCode::Char('q')), Some(Action::Quit));
    }

    #[test]
    fn balances_refresh_on_a_new_head_on_new_held_pinned_keys_and_while_eth_is_unproven() {
        let mut app = App::new(Network::Sepolia, SEPOLIA_ID, REGISTRY, false);
        app.unlocked(Ok(SENDER));
        let now = Instant::now();
        app.startup(Startup::Serving, now);
        app.tick(now + EXCITED_FOR);
        assert!(app.balances_request(100, 3));
        assert!(!app.balances_request(100, 3));
        app.refreshed(Some(Balances::default()));
        assert!(app.balances_request(100, 3));
        app.refreshed(Some(Balances {
            eth: Some(eth(3_000)),
            tokens: Vec::new(),
        }));
        assert!(!app.balances_request(100, 3));
        assert!(app.balances_request(100, 5));
        app.refreshed(Some(Balances {
            eth: Some(eth(3_000)),
            tokens: vec![(TKN, U256::from(1_500_000))],
        }));
        assert_eq!(app.assets(), [Asset::Eth, Asset::Token(TKN)]);
        assert!(!app.balances_request(100, 5));
        assert!(app.balances_request(101, 5));
        app.refreshed(None);
        assert_eq!(app.balance(Asset::Token(TKN)), Some(U256::from(1_500_000)));
        assert!(!app.balances_request(101, 5));
    }

    #[test]
    fn tokens_are_unproven_until_every_pinned_key_is_held() {
        let mut app = ready(Network::Sepolia);
        assert!(app.tokens_unproven);
        app.pinned(3, 5);
        assert!(app.tokens_unproven);
        app.pinned(5, 5);
        assert!(!app.tokens_unproven);
    }

    #[test]
    fn a_replaced_or_dropped_transfer_stops_tracking_and_quits_without_a_warning() {
        let hash = b256!("0x2222222222222222222222222222222222222222222222222222222222222222");
        let mut app = ready(Network::Sepolia);
        submit(&mut app, "1", BOB, Ok(FEE));
        press(&mut app, KeyCode::Char('y'));
        app.sent(Ok(hash));
        app.tracked(Tracked::Pending);
        app.tracked(Tracked::Dropped);
        assert_eq!(
            (app.sending.as_ref().map(|s| &s.state), app.tracking()),
            (Some(&SendState::Dropped), None)
        );
        assert_eq!(press(&mut app, KeyCode::Char('q')), Some(Action::Quit));
        assert_eq!(app.notice, None);
    }

    #[test]
    fn a_failed_receipt_or_a_refused_submission_keeps_the_form() {
        let mut app = ready(Network::Sepolia);
        submit(&mut app, "1", BOB, Ok(FEE));
        press(&mut app, KeyCode::Char('y'));
        app.sent(Ok(TxHash::ZERO));
        app.tracked(Tracked::Receipt { success: false });
        assert_eq!(
            app.sending.as_ref().map(|s| &s.state),
            Some(&SendState::Failed)
        );
        press(&mut app, KeyCode::Esc);
        assert_eq!((app.screen, app.form.amount.as_str()), (Screen::Ready, "1"));

        press(&mut app, KeyCode::Enter);
        press(&mut app, KeyCode::Char('y'));
        app.sent(Err("invalid transaction: nonce too low".to_owned()));
        assert_eq!(
            app.sending.as_ref().map(|s| &s.state),
            Some(&SendState::Rejected(
                "invalid transaction: nonce too low".to_owned()
            ))
        );
        assert_eq!(press(&mut app, KeyCode::Char('q')), Some(Action::Quit));
    }

    #[test]
    fn l_toggles_the_log_pane_and_ctrl_c_quits() {
        let mut app = ready(Network::Sepolia);
        press(&mut app, KeyCode::Char('l'));
        assert!(app.logs);
        press(&mut app, KeyCode::Char('l'));
        assert!(!app.logs);
        assert_eq!(
            app.key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)),
            Some(Action::Quit)
        );
    }

    const ANVIL: Address = address!("0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266");
    const ANVIL_PHRASE: &str = "test test test test test test test test test test test junk";
    const ANVIL_KEY: &str = "0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80";

    fn setting_up(home: Option<std::path::PathBuf>) -> App {
        let mut app = App::new(Network::Sepolia, SEPOLIA_ID, REGISTRY, false);
        app.setup(home);
        app
    }

    fn phrase_words(app: &App) -> Vec<String> {
        app.setup
            .phrase
            .as_ref()
            .expect("a generated phrase")
            .split(' ')
            .map(str::to_owned)
            .collect()
    }

    #[test]
    fn the_setup_menu_moves_with_the_arrows_and_esc_quits() {
        let mut app = setting_up(None);
        assert_eq!(app.screen, Screen::Menu);
        assert_eq!(app.setup.choice, Choice::Create);
        press(&mut app, KeyCode::Down);
        assert_eq!(app.setup.choice, Choice::Import);
        press(&mut app, KeyCode::Down);
        assert_eq!(app.setup.choice, Choice::Existing);
        press(&mut app, KeyCode::Down);
        assert_eq!(app.setup.choice, Choice::Create);
        press(&mut app, KeyCode::Up);
        assert_eq!(app.setup.choice, Choice::Existing);
        assert_eq!(press(&mut app, KeyCode::Char('q')), None);
        assert_eq!(app.screen, Screen::Menu);
        assert_eq!(press(&mut app, KeyCode::Esc), Some(Action::Quit));
    }

    #[test]
    fn creating_backs_up_and_confirms_the_phrase_then_asks_the_password_twice() {
        let mut app = setting_up(None);
        assert_eq!(press(&mut app, KeyCode::Enter), None);
        assert_eq!(app.screen, Screen::Backup);
        let words = phrase_words(&app);
        assert_eq!(words.len(), 12);
        assert!(matches!(
            wallet::parse(&words.join(" ")),
            Ok(Secret::Phrase(_))
        ));

        press(&mut app, KeyCode::Enter);
        assert_eq!(app.screen, Screen::Confirm);
        let [first, second] = app.setup.asked;
        assert_ne!(first, second);
        assert!(first < 12 && second < 12);
        assert_eq!(app.setup.asking, 0);

        let wrong = if words[first] == "zoo" {
            "abandon"
        } else {
            "zoo"
        };
        typed(&mut app, wrong);
        press(&mut app, KeyCode::Enter);
        assert_eq!(app.screen, Screen::Confirm);
        assert_eq!(app.setup.asking, 0);
        assert_eq!(
            app.setup.error,
            Some(format!(
                "that is not word {}; check what you wrote down",
                first + 1
            ))
        );
        assert!(app.setup.input.is_empty());

        typed(&mut app, &words[first]);
        press(&mut app, KeyCode::Enter);
        assert_eq!(app.screen, Screen::Confirm);
        assert_eq!(app.setup.asking, 1);
        assert_eq!(app.setup.error, None);
        typed(&mut app, &words[second]);
        press(&mut app, KeyCode::Enter);
        assert_eq!(app.screen, Screen::Password);
        assert!(app.setup.phrase.is_none());
        assert_eq!(app.setup.entry, Entry::First);

        typed(&mut app, "password1");
        press(&mut app, KeyCode::Enter);
        assert_eq!(app.setup.entry, Entry::Again);
        typed(&mut app, "password2");
        assert_eq!(press(&mut app, KeyCode::Enter), None);
        assert_eq!(
            app.setup.error.as_deref(),
            Some("the passwords do not match; enter them again")
        );
        assert_eq!(app.setup.entry, Entry::First);
        assert!(app.password.is_empty() && app.setup.again.is_empty());

        typed(&mut app, "short");
        press(&mut app, KeyCode::Enter);
        assert_eq!(
            app.setup.error.as_deref(),
            Some("a password needs at least 8 characters")
        );
        assert_eq!(app.setup.entry, Entry::First);
        assert!(app.password.is_empty());

        typed(&mut app, "password1");
        press(&mut app, KeyCode::Enter);
        typed(&mut app, "password1");
        assert_eq!(press(&mut app, KeyCode::Enter), Some(Action::Save));
        assert!(app.setup.saving);
        assert_eq!(app.setup.error, None);
        assert!(app.setup.again.is_empty());
        let Some(Secret::Phrase(phrase)) = app.take_secret() else {
            panic!("the confirmed phrase is the secret");
        };
        assert_eq!(phrase.as_str(), words.join(" "));
        assert_eq!(app.take_password().as_str(), "password1");
        assert_eq!(press(&mut app, KeyCode::Enter), None);

        let path = std::path::PathBuf::from(
            "/cfg/keystores/0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266.json",
        );
        app.saved(ANVIL, &path);
        assert_eq!(app.screen, Screen::Waiting);
        assert_eq!(app.sender, Some(ANVIL));
        assert_eq!(
            app.notice.as_deref(),
            Some(
                "saved the keystore for 0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266 to /cfg/keystores/0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266.json"
            )
        );
    }

    #[test]
    fn a_wallet_saved_to_an_unwritable_config_still_goes_on_and_says_how_to_start() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("config.toml");
        std::fs::write(&config, "network = \"mainnet\"\n").unwrap();
        std::fs::set_permissions(&config, std::fs::Permissions::from_mode(0o400)).unwrap();
        let secret =
            wallet::parse("test test test test test test test test test test test junk").unwrap();
        let Ok(wallet::Saved::Unrecorded(signer, path, reason)) =
            wallet::save(&secret, "password1", &config)
        else {
            panic!("the keystore is written but the config is read-only");
        };
        let mut app = setting_up(None);
        app.unrecorded(signer.address(), &path, &config, &reason);
        assert_eq!(app.screen, Screen::Waiting);
        assert_eq!(app.sender, Some(ANVIL));
        assert_eq!(
            app.notice,
            Some(format!(
                "saved your wallet {ANVIL} to {}, but could not record it in {}: {reason}. start with --keystore {} next time.",
                path.display(),
                config.display(),
                path.display()
            ))
        );
    }

    #[test]
    fn esc_from_the_backup_or_the_confirmation_discards_the_phrase() {
        let mut app = setting_up(None);
        press(&mut app, KeyCode::Enter);
        assert_eq!(app.screen, Screen::Backup);
        assert_eq!(press(&mut app, KeyCode::Esc), None);
        assert_eq!(app.screen, Screen::Menu);
        assert!(app.setup.phrase.is_none());

        press(&mut app, KeyCode::Enter);
        press(&mut app, KeyCode::Enter);
        assert_eq!(app.screen, Screen::Confirm);
        typed(&mut app, "abc");
        press(&mut app, KeyCode::Esc);
        assert_eq!(app.screen, Screen::Menu);
        assert!(app.setup.phrase.is_none());
        assert!(app.setup.input.is_empty());
    }

    #[test]
    fn import_tells_a_key_from_a_phrase_and_says_what_it_expected() {
        for (text, key) in [
            (ANVIL_KEY, true),
            (&ANVIL_KEY[2..], true),
            (ANVIL_PHRASE, false),
            (
                "  Test test TEST test test test test test test test test junk ",
                false,
            ),
        ] {
            let mut app = setting_up(None);
            press(&mut app, KeyCode::Down);
            press(&mut app, KeyCode::Enter);
            assert_eq!(app.screen, Screen::Import);
            typed(&mut app, text);
            press(&mut app, KeyCode::Enter);
            assert_eq!(app.screen, Screen::Password, "{text}");
            assert!(app.setup.input.is_empty());
            match app.take_secret() {
                Some(Secret::Key(_)) => assert!(key, "{text}"),
                Some(Secret::Phrase(phrase)) => {
                    assert!(!key, "{text}");
                    assert_eq!(phrase.as_str(), ANVIL_PHRASE);
                }
                None => panic!("no secret for {text}"),
            }
        }

        for (text, expected) in [
            (
                "test test test test test test test test test test test test",
                "the recovery phrase's checksum does not match; check the words and their order",
            ),
            (
                "test test test",
                "a recovery phrase has 12 or 24 words; this has 3",
            ),
            (
                "test test test test test test test test test test test chibi",
                "word 12 is not in the English BIP-39 word list",
            ),
            (
                "0xac0974bec39a17e36ba4a6b4d238ff944bacb478",
                "expected a private key (64 hex digits) or a recovery phrase (12 or 24 words)",
            ),
            (
                "0x0000000000000000000000000000000000000000000000000000000000000000",
                "this is not a valid private key",
            ),
        ] {
            let mut app = setting_up(None);
            press(&mut app, KeyCode::Down);
            press(&mut app, KeyCode::Enter);
            typed(&mut app, text);
            press(&mut app, KeyCode::Enter);
            assert_eq!(app.screen, Screen::Import, "{text}");
            assert_eq!(app.setup.error.as_deref(), Some(expected), "{text}");
            assert!(app.setup.input.is_empty());
            assert!(app.take_secret().is_none());
            assert_eq!(press(&mut app, KeyCode::Esc), None);
            assert_eq!(app.screen, Screen::Menu);
        }
    }

    #[test]
    fn an_existing_keystore_must_be_a_file_and_goes_to_unlock() {
        let dir = tempfile::tempdir().unwrap();
        let key = dir.path().join("key.json");
        std::fs::write(&key, "{}").unwrap();
        let missing = dir.path().join("missing.json");
        let mut app = setting_up(Some(dir.path().to_owned()));
        press(&mut app, KeyCode::Up);
        press(&mut app, KeyCode::Enter);
        assert_eq!(app.screen, Screen::Existing);

        typed(&mut app, &missing.display().to_string());
        assert_eq!(press(&mut app, KeyCode::Enter), None);
        assert_eq!(app.screen, Screen::Existing);
        assert_eq!(
            app.setup.error,
            Some(format!("no file at {}", missing.display()))
        );

        app.setup.path.clear();
        typed(&mut app, "key.json");
        assert_eq!(press(&mut app, KeyCode::Enter), None);
        assert_eq!(
            app.setup.error.as_deref(),
            Some("enter an absolute path, or one starting with ~/")
        );

        app.setup.path.clear();
        typed(&mut app, "~/key.json");
        assert_eq!(press(&mut app, KeyCode::Enter), Some(Action::Open(key)));
        assert_eq!(app.screen, Screen::Unlock);
        assert_eq!(app.setup.error, None);
    }

    #[test]
    fn a_kept_keystore_asks_its_password_and_a_failed_save_says_why() {
        let path = std::path::PathBuf::from("/cfg/keystores/a.json");
        let mut app = setting_up(None);
        app.kept(ANVIL, &path);
        assert_eq!(app.screen, Screen::Unlock);
        assert_eq!(
            app.notice.as_deref(),
            Some(
                "a keystore for 0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266 is already at /cfg/keystores/a.json and was not overwritten; enter its password"
            )
        );

        let mut app = setting_up(None);
        app.save_failed("disk full");
        assert_eq!(app.screen, Screen::Menu);
        assert_eq!(
            app.setup.error.as_deref(),
            Some("cannot save the keystore: disk full")
        );
    }
}
