//! Turning the [`App`] into a [`View`] and drawing it with ratatui. Reads the state; changes
//! nothing.

use std::fmt::Write;
use std::time::Duration;

use alloy_primitives::U256;
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};

use super::app::{App, BACKUP_WARNING, Choice, Entry, Field, PAUSED, Screen, SendState, show};
use super::transfer::{Asset, Fee};
use crate::format_units;

/// Log lines the log pane shows at most.
pub const LOG_PANE_LINES: u16 = 10;

/// The token row's hint while some pinned key is not held yet.
const TOKENS_UNPROVEN: &str = "more tokens appear once their balances are proven";

/// A backup row's bytes: three columns of a 2-digit number and a word padded to 10.
const BACKUP_ROW_BYTES: usize = 3 * 16;

/// The dozing cat while waiting for a state peer: it breathes, then peeks.
const DOZING: [[&str; 4]; 3] = [
    ["  /\\_/\\", " ( -.- )  z", "  > ^ <", " (\")_(\")"],
    ["  /\\_/\\", " ( -.- )  zZ", "  > ^ <", " (\")_(\")"],
    ["  /\\_/\\", " ( o.- )", "  > ^ <", " (\")_(\")"],
];

/// The cat perking up at its first state peer, sparkles twinkling.
const EXCITED: [[&str; 4]; 2] = [
    ["*  /\\_/\\   *", "  ( ^o^ )", "*  > ^ <   *", "  (\")_(\")"],
    [
        "+  /\\_/\\   +",
        "  ( ^O^ )  *",
        "+  > ^ <   +",
        "  (\")_(\")",
    ],
];

/// The cat asleep while no state peer is connected.
const SLEEPY: [[&str; 4]; 2] = [
    ["  /\\_/\\", " ( u.u )  zzz", "  > ^ <", " (\")_(\")"],
    ["  /\\_/\\", " ( u.u )  ZZZ", "  > ^ <", " (\")_(\")"],
];

/// The cat at a verified successful receipt.
const HAPPY: [&str; 4] = ["  /\\_/\\", " ( ^.^ )  ~", "  > ^ <", " (\")_(\")"];

/// The cat at a failed receipt.
const SAD: [&str; 4] = ["  /\\_/\\", " ( ;.; )", "  > ^ <", " (\")_(\")"];

/// What the screen shows, built from the [`App`] on the control loop and drawn on the render
/// thread. Passwords and typed secrets are already masked; the one secret it can hold is the
/// new recovery phrase on Backup, gone with the next view sent.
#[derive(Clone, Debug, Default)]
pub struct View {
    header: Line<'static>,
    body: Vec<Line<'static>>,
    footer: Vec<Line<'static>>,
    /// The tail of the log buffer, oldest first, while the log pane is open.
    logs: Option<Vec<String>>,
}

impl View {
    /// The screen for `app`, with `logs` in the log pane when it is open.
    pub fn new(app: &App, logs: Vec<String>) -> Self {
        Self {
            header: header_line(app),
            body: with_notice(app, body_lines(app)),
            footer: footer_lines(app),
            logs: app.logs.then_some(logs),
        }
    }
}

/// Draws the whole screen.
pub fn draw(frame: &mut Frame<'_>, view: View) {
    let pane = if view.logs.is_some() {
        LOG_PANE_LINES + 2
    } else {
        0
    };
    let [header, body, footer, log_area] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(1),
        Constraint::Length(1),
        Constraint::Length(pane),
    ])
    .areas(frame.area());

    frame.render_widget(Paragraph::new(view.header), header);
    frame.render_widget(Paragraph::new(view.body).wrap(Wrap { trim: false }), body);
    frame.render_widget(Paragraph::new(view.footer), footer);
    if let Some(logs) = view.logs {
        let shown = usize::from(log_area.height.saturating_sub(2));
        let tail = &logs[logs.len().saturating_sub(shown)..];
        frame.render_widget(
            Paragraph::new(
                tail.iter()
                    .map(|line| Line::raw(line.as_str()))
                    .collect::<Vec<_>>(),
            )
            .block(Block::default().borders(Borders::TOP).title(" log ")),
            log_area,
        );
    }
}

fn header_line(app: &App) -> Line<'static> {
    let mut spans = vec![
        Span::styled("chibi", Style::default().add_modifier(Modifier::BOLD)),
        Span::raw(format!(" · {}", app.network)),
    ];
    if let Some(sender) = app.sender {
        spans.push(Span::raw(format!(" · {}", sender.to_checksum(None))));
    }
    Line::from(spans)
}

fn cat(lines: [&str; 4]) -> impl Iterator<Item = Line<'static>> {
    lines.into_iter().map(|line| Line::raw(line.to_owned()))
}

/// Frame `n` of an animation, one step per tick.
fn pick<T: Copy, const N: usize>(frames: &[T; N], tick: u64) -> T {
    frames[usize::try_from(tick % N as u64).unwrap_or_default()]
}

fn body_lines(app: &App) -> Vec<Line<'static>> {
    let mut lines = vec![Line::default()];
    match app.screen {
        Screen::Menu => menu(app, &mut lines),
        Screen::Backup => backup(app, &mut lines),
        Screen::Confirm => confirm(app, &mut lines),
        Screen::Import => import(app, &mut lines),
        Screen::Existing => existing(app, &mut lines),
        Screen::Password => new_password(app, &mut lines),
        Screen::Unlock => unlock(app, &mut lines),
        Screen::Waiting => {
            lines.extend(cat(pick(&DOZING, app.frame)));
            lines.push(Line::default());
            lines.push(Line::raw(format!(
                "looking for a state peer · peers {} · state peers {} · {}",
                app.waiting.peers,
                app.waiting.state_peers,
                elapsed(app.waiting.elapsed)
            )));
        }
        Screen::Excited => {
            lines.extend(cat(pick(&EXCITED, app.frame)));
            lines.push(Line::default());
            lines.push(Line::raw("found a state peer!"));
            lines.push(Line::raw("anchoring at a verified head..."));
        }
        Screen::Paused => {
            lines.extend(cat(pick(&SLEEPY, app.frame)));
            lines.push(Line::default());
            lines.push(Line::raw(PAUSED));
            lines.push(Line::raw(
                "nothing can be proven or sent until one connects; your form is kept",
            ));
        }
        Screen::Ready => form(app, &mut lines),
        Screen::Review => review(app, &mut lines),
        Screen::Sending => sending(app, &mut lines),
    }
    lines
}

fn menu(app: &App, lines: &mut Vec<Line<'static>>) {
    lines.push(Line::raw("no keystore yet: set up the wallet to send from"));
    lines.push(Line::default());
    for choice in Choice::ALL {
        let marker = if app.setup.choice == choice { ">" } else { " " };
        lines.push(Line::raw(format!("{marker} {}", choice.label())));
    }
    setup_error(app, lines);
}

/// The new phrase in three numbered columns, read down.
fn backup(app: &App, lines: &mut Vec<Line<'static>>) {
    lines.push(Line::raw("your new recovery phrase:"));
    lines.push(Line::default());
    let words: Vec<&str> = app
        .setup
        .phrase
        .as_ref()
        .map(|phrase| phrase.split(' ').collect())
        .unwrap_or_default();
    let rows = words.len().div_ceil(3);
    for row in 0..rows {
        let mut line = String::with_capacity(BACKUP_ROW_BYTES);
        for at in (row..words.len()).step_by(rows) {
            let _ = write!(line, "  {:>2}. {:<10}", at + 1, words[at]);
        }
        lines.push(Line::raw(line));
    }
    lines.push(Line::default());
    lines.push(Line::styled(
        BACKUP_WARNING,
        Style::default().fg(Color::Yellow),
    ));
}

fn confirm(app: &App, lines: &mut Vec<Line<'static>>) {
    let [first, second] = app.setup.asked;
    lines.push(Line::raw(format!(
        "confirm your recovery phrase: enter words {} and {}",
        first + 1,
        second + 1
    )));
    lines.push(Line::default());
    lines.push(Line::raw(format!(
        "word {}  {}",
        app.setup.asked[app.setup.asking] + 1,
        mask(&app.setup.input)
    )));
    setup_error(app, lines);
}

fn import(app: &App, lines: &mut Vec<Line<'static>>) {
    lines.push(Line::raw(
        "type or paste a private key (64 hex digits) or a recovery phrase (12 or 24 words)",
    ));
    lines.push(Line::default());
    lines.push(Line::raw(format!("secret  {}", mask(&app.setup.input))));
    setup_error(app, lines);
}

fn existing(app: &App, lines: &mut Vec<Line<'static>>) {
    lines.push(Line::raw(
        "the path of an encrypted JSON keystore, absolute or starting with ~/",
    ));
    lines.push(Line::default());
    lines.push(Line::raw(format!("path  {}", app.setup.path)));
    setup_error(app, lines);
}

fn new_password(app: &App, lines: &mut Vec<Line<'static>>) {
    lines.push(Line::raw(
        "choose a password to encrypt the keystore with, at least 8 characters",
    ));
    lines.push(Line::default());
    let field = |entry: Entry, label: &str, text: &str| {
        let marker = if app.setup.entry == entry { ">" } else { " " };
        Line::raw(format!("{marker} {label:<9} {}", mask(text)))
    };
    lines.push(field(Entry::First, "password", &app.password));
    lines.push(field(Entry::Again, "again", &app.setup.again));
    if app.setup.saving {
        lines.push(Line::raw("encrypting and saving the keystore..."));
    }
    setup_error(app, lines);
}

fn setup_error(app: &App, lines: &mut Vec<Line<'static>>) {
    if let Some(error) = &app.setup.error {
        lines.push(error_line(error));
    }
}

/// One `*` per character typed.
fn mask(text: &str) -> String {
    "*".repeat(text.chars().count())
}

fn unlock(app: &App, lines: &mut Vec<Line<'static>>) {
    lines.push(Line::raw("unlock your keystore to send from it"));
    lines.push(Line::default());
    lines.push(Line::raw(format!("password  {}", mask(&app.password))));
    if app.unlocking {
        lines.push(Line::raw("unlocking..."));
    }
    if let Some(error) = &app.unlock_error {
        lines.push(error_line(error));
    }
}

fn form(app: &App, lines: &mut Vec<Line<'static>>) {
    let from = app
        .sender
        .map(|sender| sender.to_checksum(None))
        .unwrap_or_default();
    lines.push(Line::raw(format!(
        "  from    {from}   {}",
        balance(app.balance(Asset::Eth), Asset::Eth)
    )));
    let asset = app.form.asset;
    let field = |name: Field, label: &str, value: String| {
        let marker = if app.form.field == name { ">" } else { " " };
        Line::raw(format!("{marker} {label:<7} {value}"))
    };
    let mut token = field(
        Field::Token,
        "token",
        format!(
            "< {} >   {}",
            asset.symbol(),
            balance(app.balance(asset), asset)
        ),
    );
    if app.tokens_unproven {
        token.push_span(Span::styled(
            format!("   {TOKENS_UNPROVEN}"),
            Style::default().add_modifier(Modifier::DIM),
        ));
    }
    lines.push(token);
    lines.push(field(Field::Amount, "amount", app.form.amount.clone()));
    if let Some(error) = &app.form.amount_error {
        lines.push(error_line(error));
    }
    lines.push(field(Field::To, "to", app.form.to.clone()));
    if let Some(error) = &app.form.to_error {
        lines.push(error_line(error));
    } else if let Some(warning) = app.to_warning() {
        lines.push(Line::styled(
            format!("          {warning}"),
            Style::default().fg(Color::Yellow),
        ));
    }
    let fee = match app.fee() {
        Some(Ok(fee)) => fee_text(fee),
        Some(Err(reason)) => reason.clone(),
        None if app.form.to.is_empty() || app.form.amount.is_empty() => String::new(),
        None => "estimating...".to_owned(),
    };
    lines.push(Line::raw(format!("  fee     {fee}")));
    if let Some(error) = &app.form.fee_error {
        lines.push(error_line(error));
    }
}

fn review(app: &App, lines: &mut Vec<Line<'static>>) {
    let Some(review) = app.review else {
        return;
    };
    let draft = review.draft;
    lines.push(Line::raw("review the transfer"));
    lines.push(Line::default());
    lines.push(Line::raw(format!(
        "  network  {} (chain id {})",
        app.network, app.chain_id
    )));
    lines.push(Line::raw(format!(
        "  to       {}",
        draft.to.to_checksum(None)
    )));
    lines.push(Line::raw(format!(
        "  amount   {}",
        show(draft.amount, draft.asset)
    )));
    lines.push(Line::raw(format!("  max fee  {}", fee_text(&review.fee))));
    lines.push(Line::default());
    match review.refused {
        Some(reason) => lines.push(error_line(reason)),
        None => lines.push(Line::raw("y signs and sends · n goes back")),
    }
}

fn sending(app: &App, lines: &mut Vec<Line<'static>>) {
    let Some(sending) = &app.sending else {
        return;
    };
    match sending.state {
        SendState::Succeeded => lines.extend(cat(HAPPY)),
        SendState::Failed => lines.extend(cat(SAD)),
        SendState::Signing
        | SendState::Pending
        | SendState::Included
        | SendState::Rejected(_)
        | SendState::Dropped => {}
    }
    lines.push(Line::raw(format!(
        "sending {} to {}",
        show(sending.draft.amount, sending.draft.asset),
        sending.draft.to.to_checksum(None)
    )));
    if let Some(hash) = sending.hash {
        lines.push(Line::raw(format!("hash {hash}")));
    }
    let status = match &sending.state {
        SendState::Signing => "signing and sending...".to_owned(),
        SendState::Pending if sending.still_pending() => format!(
            "still pending after {} blocks; chibi keeps rebroadcasting it and bumps no fee",
            sending.pending_heads
        ),
        SendState::Pending => "pending: waiting to be included in a block".to_owned(),
        SendState::Included => "included in a block; verifying the receipt...".to_owned(),
        SendState::Succeeded => "sent! the receipt is verified with status 1".to_owned(),
        SendState::Failed => "the transfer failed: the verified receipt has status 0".to_owned(),
        SendState::Rejected(reason) => format!("not sent: {reason}"),
        SendState::Dropped => {
            "this transfer was replaced or dropped: its nonce was used by another transaction"
                .to_owned()
        }
    };
    lines.push(Line::raw(status));
}

fn footer_lines(app: &App) -> Vec<Line<'static>> {
    let keys = match app.screen {
        Screen::Menu => "up/down choose · Enter select · Esc quit",
        Screen::Backup => "Enter I wrote them down · Esc back to the menu, discarding them",
        Screen::Confirm => "Enter check · Esc back to the menu, discarding the phrase",
        Screen::Import | Screen::Existing | Screen::Password => {
            "Enter continue · Esc back to the menu"
        }
        Screen::Unlock => "Enter unlock · Esc quit",
        Screen::Waiting | Screen::Excited | Screen::Paused => "l logs · q quit",
        Screen::Ready => {
            "up/down field · left/right token · type amount (or max) and to · Enter review · l logs · q quit"
        }
        Screen::Review => "y send · n back · l logs · q quit",
        Screen::Sending => "Enter back to the form · l logs · q quit",
    };
    vec![Line::styled(
        keys,
        Style::default().add_modifier(Modifier::DIM),
    )]
}

/// Ends the body with the notice, if any, so it wraps like the rest of the body.
fn with_notice(app: &App, mut lines: Vec<Line<'static>>) -> Vec<Line<'static>> {
    if let Some(notice) = app.notice.as_deref() {
        lines.push(Line::default());
        lines.push(Line::styled(
            notice.to_owned(),
            Style::default().fg(Color::Yellow),
        ));
    }
    lines
}

fn error_line(text: &str) -> Line<'static> {
    Line::styled(format!("          {text}"), Style::default().fg(Color::Red))
}

fn balance(amount: Option<U256>, asset: Asset) -> String {
    amount.map_or_else(
        || format!("? {}", asset.symbol()),
        |amount| show(amount, asset),
    )
}

/// The fee caps: the most the transfer can cost, then its gas and fee per gas.
fn fee_text(fee: &Fee) -> String {
    format!(
        "{} ({} gas at up to {} gwei)",
        show(fee.max_cost(), Asset::Eth),
        fee.gas_limit,
        format_units(U256::from(fee.max_fee_per_gas), 9)
    )
}

/// `1m20s`-style elapsed time.
fn elapsed(elapsed: Duration) -> String {
    let seconds = elapsed.as_secs();
    match seconds {
        0..60 => format!("{seconds}s"),
        60..3_600 => format!("{}m{:02}s", seconds / 60, seconds % 60),
        _ => format!(
            "{}h{:02}m{:02}s",
            seconds / 3_600,
            seconds % 3_600 / 60,
            seconds % 60
        ),
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::time::Instant;

    use alloy_primitives::{Address, address};
    use chibi_node::{Network, Startup, Token};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    use super::*;
    use crate::tui::app::{Balances, EXCITED_FOR, Sending};
    use crate::tui::transfer::Draft;

    const SENDER: Address = address!("0x17c5185167401ed00cf5f5b2fc97d9bbfdb7d025");
    const TKN: Token = Token {
        symbol: "TKN",
        name: "Test token",
        address: address!("0x00000000000000000000000000000000000070c3"),
        decimals: 6,
    };
    const REGISTRY: &[Token] = &[TKN];

    fn screen(app: &App, logs: &[String]) -> String {
        sized(app, logs, 100, 30)
    }

    fn sized(app: &App, logs: &[String], width: u16, height: u16) -> String {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal
            .draw(|frame| draw(frame, View::new(app, logs.to_vec())))
            .unwrap();
        let buffer = terminal.backend().buffer();
        let width = usize::from(buffer.area().width);
        buffer
            .content()
            .chunks(width)
            .map(|row| row.iter().map(|cell| cell.symbol()).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn waiting() -> App {
        let mut app = App::new(Network::Sepolia, 11_155_111, REGISTRY, false);
        app.unlocked(Ok(SENDER));
        app.startup(
            Startup::Waiting {
                peers: 12,
                state_peers: 0,
                elapsed: Duration::from_secs(80),
            },
            Instant::now(),
        );
        app
    }

    #[test]
    fn unlock_masks_the_password() {
        let mut app = App::new(Network::Sepolia, 11_155_111, REGISTRY, false);
        app.password.push_str("hunter2");
        let shown = screen(&app, &[]);
        assert!(shown.contains("password  *******"), "{shown}");
        assert!(!shown.contains("hunter2"), "{shown}");
    }

    #[test]
    fn waiting_shows_the_dozing_cat_and_the_peer_counts() {
        let mut app = waiting();
        let shown = screen(&app, &[]);
        assert!(shown.contains(" ( -.- )  z"), "{shown}");
        assert!(
            shown.contains("looking for a state peer · peers 12 · state peers 0 · 1m20s"),
            "{shown}"
        );
        let now = Instant::now();
        app.tick(now);
        app.tick(now);
        assert!(screen(&app, &[]).contains(" ( o.- )"));
    }

    #[test]
    fn excited_shows_the_found_peer() {
        let mut app = waiting();
        app.startup(Startup::FoundStatePeer, Instant::now());
        let shown = screen(&app, &[]);
        assert!(shown.contains("found a state peer!"), "{shown}");
        assert!(shown.contains("( ^o^ )"), "{shown}");
    }

    fn ready() -> App {
        let mut app = waiting();
        let now = Instant::now();
        app.startup(Startup::Serving, now);
        app.tick(now + EXCITED_FOR);
        app.health(100, 1);
        app.balances(Balances {
            eth: Some(U256::from(3_000_000_000_000_000_000u64)),
            tokens: vec![(TKN, U256::from(1_500_000))],
        });
        app
    }

    #[test]
    fn ready_shows_the_form() {
        let shown = screen(&ready(), &[]);
        for expected in [
            "from    0x17c5185167401eD00cF5F5b2fc97D9BBfDb7D025   3.00 ETH",
            "  token   < ETH >   3.00 ETH",
            "> amount",
            "  to",
            "  fee",
            "Enter review",
        ] {
            assert!(shown.contains(expected), "{expected}\n{shown}");
        }
    }

    #[test]
    fn the_token_row_says_more_tokens_appear_until_every_pinned_key_is_held() {
        const HINT: &str = "more tokens appear once their balances are proven";
        let mut app = ready();
        app.pinned(4, 6);
        assert!(screen(&app, &[]).contains(HINT));
        let mut terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();
        terminal
            .draw(|frame| draw(frame, View::new(&app, Vec::new())))
            .unwrap();
        let buffer = terminal.backend().buffer();
        let row = (0..30)
            .find(|&y| {
                (0..100)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
                    .contains("  token   < ETH >   3.00 ETH   more tokens appear")
            })
            .expect("the token row carries the hint");
        let hint_x = (0..100)
            .find(|&x| buffer[(x, row)].symbol() == "m")
            .unwrap();
        assert!(buffer[(hint_x, row)].modifier.contains(Modifier::DIM));
        assert!(!buffer[(2, row)].modifier.contains(Modifier::DIM));

        app.pinned(6, 6);
        assert!(!screen(&app, &[]).contains(HINT));
    }

    #[test]
    fn paused_shows_the_sleepy_cat_and_why() {
        let mut app = ready();
        app.health(101, 0);
        let shown = screen(&app, &[]);
        assert!(shown.contains("( u.u )"), "{shown}");
        assert!(shown.contains("waiting for a state peer"), "{shown}");
    }

    #[test]
    fn the_log_pane_shows_the_tail_of_the_log() {
        let mut app = ready();
        let logs: Vec<String> = (0..30).map(|n| format!("log line {n}")).collect();
        assert!(!screen(&app, &logs).contains("log line 29"));
        app.logs = true;
        let shown = screen(&app, &logs);
        assert!(shown.contains("log line 29"), "{shown}");
        assert!(shown.contains("log line 20"), "{shown}");
        assert!(!shown.contains("log line 19"), "{shown}");
    }

    #[test]
    fn a_replaced_or_dropped_transfer_says_its_nonce_was_used() {
        let mut app = ready();
        app.screen = Screen::Sending;
        app.sending = Some(Sending {
            draft: Draft {
                asset: Asset::Eth,
                to: SENDER,
                amount: U256::from(1_000_000_000u64),
            },
            hash: Some(alloy_primitives::TxHash::ZERO),
            state: SendState::Dropped,
            pending_heads: 0,
        });
        let shown = screen(&app, &[]);
        assert!(
            shown.contains(
                "this transfer was replaced or dropped: its nonce was used by another transaction"
            ),
            "{shown}"
        );
    }

    #[test]
    fn elapsed_time_reads_like_1m20s() {
        assert_eq!(elapsed(Duration::from_secs(45)), "45s");
        assert_eq!(elapsed(Duration::from_secs(80)), "1m20s");
        assert_eq!(elapsed(Duration::from_secs(3_725)), "1h02m05s");
    }

    const ANVIL_PHRASE: &str = "test test test test test test test test test test test junk";

    fn setting_up() -> App {
        let mut app = App::new(Network::Sepolia, 11_155_111, REGISTRY, false);
        app.setup(None);
        app
    }

    #[test]
    fn setup_offers_three_ways_to_get_a_wallet() {
        let mut app = setting_up();
        let shown = screen(&app, &[]);
        for expected in [
            "no keystore yet: set up the wallet to send from",
            "> create a new wallet",
            "  import a private key or recovery phrase",
            "  use an existing keystore file",
            "up/down choose · Enter select · Esc quit",
        ] {
            assert!(shown.contains(expected), "{expected}\n{shown}");
        }
        app.setup.choice = Choice::Existing;
        assert!(screen(&app, &[]).contains("> use an existing keystore file"));
    }

    #[test]
    fn the_backup_shows_the_twelve_words_numbered_once() {
        let mut app = setting_up();
        app.screen = Screen::Backup;
        app.setup.phrase = Some(zeroize::Zeroizing::new(
            ANVIL_PHRASE.replace("test junk", "test chibi"),
        ));
        let shown = screen(&app, &[]);
        for expected in [
            " 1. test",
            " 9. test",
            "11. test",
            "12. chibi",
            "write these words down. anyone with them controls this wallet. chibi will not show them again.",
        ] {
            assert!(shown.contains(expected), "{expected}\n{shown}");
        }
        assert_eq!(shown.matches("chibi").count(), 3, "{shown}");
    }

    #[test]
    fn the_confirmation_names_the_positions_and_masks_the_word() {
        let mut app = setting_up();
        app.screen = Screen::Confirm;
        app.setup.phrase = Some(zeroize::Zeroizing::new(ANVIL_PHRASE.to_owned()));
        app.setup.asked = [2, 8];
        app.setup.input.push_str("tset");
        let shown = screen(&app, &[]);
        assert!(
            shown.contains("confirm your recovery phrase: enter words 3 and 9"),
            "{shown}"
        );
        assert!(shown.contains("word 3  ****"), "{shown}");
        assert!(!shown.contains("tset"), "{shown}");
        app.setup.asking = 1;
        app.setup.input.clear();
        assert!(screen(&app, &[]).contains("word 9  "));
    }

    #[test]
    fn secret_and_password_fields_never_render_their_text() {
        let mut app = setting_up();
        app.screen = Screen::Import;
        app.setup.input.push_str("deadbeef");
        let shown = screen(&app, &[]);
        assert!(shown.contains("secret  ********"), "{shown}");
        assert!(!shown.contains("deadbeef"), "{shown}");

        app.setup.input.clear();
        app.screen = Screen::Password;
        app.password.push_str("hunter22");
        app.setup.again.push_str("hunter2");
        app.setup.entry = Entry::Again;
        let shown = screen(&app, &[]);
        assert!(shown.contains("  password  ********"), "{shown}");
        assert!(shown.contains("> again     *******"), "{shown}");
        assert!(!shown.contains("hunter"), "{shown}");
    }

    #[test]
    fn a_wallet_saved_without_its_config_shows_the_whole_way_to_start_again() {
        let path = Path::new(
            "/home/someone/.config/chibi/keystores/0x17c5185167401eD00cF5F5b2fc97D9BBfDb7D025.json",
        );
        let config = Path::new("/home/someone/.config/chibi/config.toml");
        let mut app = App::new(Network::Sepolia, 11_155_111, REGISTRY, false);
        app.unrecorded(SENDER, path, config, "the file is read-only");
        let shown: String = screen(&app, &[])
            .chars()
            .filter(|c| !c.is_whitespace())
            .collect();
        let notice: String = format!(
            "but could not record it in {}: the file is read-only. start with --keystore {} next time.",
            config.display(),
            path.display()
        )
        .chars()
        .filter(|c| !c.is_whitespace())
        .collect();
        assert!(shown.contains(&notice), "{shown}");
    }

    fn squeezed(text: &str) -> String {
        text.chars().filter(|c| !c.is_whitespace()).collect()
    }

    #[test]
    fn the_quit_warning_shows_whole_over_a_pending_transfer_and_the_log_pane() {
        let mut app = ready();
        app.screen = Screen::Sending;
        app.sending = Some(Sending {
            draft: Draft {
                asset: Asset::Eth,
                to: SENDER,
                amount: U256::from(1_000_000_000u64),
            },
            hash: Some(alloy_primitives::TxHash::ZERO),
            state: SendState::Pending,
            pending_heads: 0,
        });
        let warning = "a transfer is still pending; chibi's outbox rebroadcasts it on the next run. press q again to quit";
        app.notice = Some(warning.to_owned());
        app.logs = true;
        let logs: Vec<String> = (0..30).map(|i| format!("log line {i}")).collect();
        let shown = squeezed(&sized(&app, &logs, 100, 18));
        assert!(shown.contains(&squeezed(warning)), "{shown}");
    }

    #[test]
    fn a_long_notice_shows_whole_on_ready_with_the_log_pane() {
        let mut app = ready();
        let notice = "saved your wallet 0x17c5185167401eD00cF5F5b2fc97D9BBfDb7D025 to /home/someone/.config/chibi/keystores/0x17c5185167401eD00cF5F5b2fc97D9BBfDb7D025.json, but could not record it in /home/someone/.config/chibi/config.toml: the file is read-only. start with --keystore /home/someone/.config/chibi/keystores/0x17c5185167401eD00cF5F5b2fc97D9BBfDb7D025.json next time.";
        app.notice = Some(notice.to_owned());
        app.logs = true;
        let logs: Vec<String> = (0..30).map(|i| format!("log line {i}")).collect();
        let shown = squeezed(&sized(&app, &logs, 80, 24));
        assert!(shown.contains(&squeezed(notice)), "{shown}");
    }
}
