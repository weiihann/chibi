//! Drawing the [`App`] with ratatui. Reads the state; changes nothing.

use std::time::Duration;

use alloy_primitives::U256;
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};

use super::app::{App, Field, Screen, SendState, show};
use super::transfer::{Asset, Fee};
use crate::format_units;

/// Log lines the log pane shows at most.
const LOG_PANE_LINES: u16 = 10;

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

/// Draws the whole screen; `logs` is the tail of the log buffer, oldest first.
pub fn draw(frame: &mut Frame<'_>, app: &App, logs: &[String]) {
    let pane = if app.logs { LOG_PANE_LINES + 2 } else { 0 };
    let [header, body, footer, log_area] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(1),
        Constraint::Length(2),
        Constraint::Length(pane),
    ])
    .areas(frame.area());

    frame.render_widget(Paragraph::new(header_line(app)), header);
    frame.render_widget(
        Paragraph::new(body_lines(app)).wrap(Wrap { trim: false }),
        body,
    );
    frame.render_widget(Paragraph::new(footer_lines(app)), footer);
    if app.logs {
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
            lines.push(Line::raw("waiting for a state peer"));
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

fn unlock(app: &App, lines: &mut Vec<Line<'static>>) {
    lines.push(Line::raw("unlock your keystore to send from it"));
    lines.push(Line::default());
    lines.push(Line::raw(format!(
        "password  {}",
        "*".repeat(app.password.chars().count())
    )));
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
    lines.push(field(
        Field::Token,
        "token",
        format!(
            "< {} >   {}",
            asset.symbol(),
            balance(app.balance(asset), asset)
        ),
    ));
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
        SendState::Signing | SendState::Pending | SendState::Included | SendState::Rejected(_) => {}
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
    };
    lines.push(Line::raw(status));
}

fn footer_lines(app: &App) -> Vec<Line<'static>> {
    let keys = match app.screen {
        Screen::Unlock => "Enter unlock · Esc quit",
        Screen::Waiting | Screen::Excited | Screen::Paused => "l logs · q quit",
        Screen::Ready => {
            "up/down field · left/right token · type amount (or max) and to · Enter review · l logs · q quit"
        }
        Screen::Review => "y send · n back · l logs · q quit",
        Screen::Sending => "Enter back to the form · l logs · q quit",
    };
    vec![
        app.notice.as_deref().map_or_else(Line::default, |notice| {
            Line::styled(notice.to_owned(), Style::default().fg(Color::Yellow))
        }),
        Line::styled(keys, Style::default().add_modifier(Modifier::DIM)),
    ]
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
    use alloy_primitives::{Address, address};
    use chibi_node::{Network, Startup, Token};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    use super::*;
    use crate::tui::app::Balances;

    const SENDER: Address = address!("0x17c5185167401ed00cf5f5b2fc97d9bbfdb7d025");
    const TKN: Token = Token {
        symbol: "TKN",
        name: "Test token",
        address: address!("0x00000000000000000000000000000000000070c3"),
        decimals: 6,
    };
    const REGISTRY: &[Token] = &[TKN];

    fn screen(app: &App, logs: &[String]) -> String {
        let mut terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();
        terminal.draw(|frame| draw(frame, app, logs)).unwrap();
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
        app.startup(Startup::Waiting {
            peers: 12,
            state_peers: 0,
            elapsed: Duration::from_secs(80),
        });
        app
    }

    #[test]
    fn unlock_masks_the_password() {
        let mut app = App::new(Network::Sepolia, 11_155_111, REGISTRY, false);
        app.password = "hunter2".to_owned();
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
        app.tick();
        app.tick();
        assert!(screen(&app, &[]).contains(" ( o.- )"));
    }

    #[test]
    fn excited_shows_the_found_peer() {
        let mut app = waiting();
        app.startup(Startup::FoundStatePeer);
        let shown = screen(&app, &[]);
        assert!(shown.contains("found a state peer!"), "{shown}");
        assert!(shown.contains("( ^o^ )"), "{shown}");
    }

    fn ready() -> App {
        let mut app = waiting();
        app.startup(Startup::Serving);
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
    fn elapsed_time_reads_like_1m20s() {
        assert_eq!(elapsed(Duration::from_secs(45)), "45s");
        assert_eq!(elapsed(Duration::from_secs(80)), "1m20s");
        assert_eq!(elapsed(Duration::from_secs(3_725)), "1h02m05s");
    }
}
