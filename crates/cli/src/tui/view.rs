//! Turning the [`App`] into a [`View`] and drawing it with ratatui. Reads the state; changes
//! nothing.

use std::fmt::Write;
use std::time::{Duration, Instant};

use alloy_primitives::U256;
use ratatui::Frame;
use ratatui::layout::{Alignment, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Clear, Padding, Paragraph, Wrap};

use super::app::{
    App, BACKUP_WARNING, Choice, Entry, Field, HistoryMode, PAUSED, Resolution, SIDE_CAT_COLUMNS,
    Screen, SendState, show,
};
use super::cat::{self, Pose};
use super::transfer::{Asset, Fee};
use crate::activity::{self, AMOUNT_COLUMNS, Cells, DetailLine, format_units};

/// Log lines the log pane shows at most.
pub const LOG_PANE_LINES: u16 = 10;

/// The narrowest terminal the card is drawn on; narrower ones get the borderless layout.
const CARD_MIN_WIDTH: u16 = 44;

/// The shortest terminal the card is drawn on; shorter ones get the borderless layout.
const CARD_MIN_HEIGHT: u16 = 14;

/// The card's widest.
const CARD_MAX_WIDTH: u16 = 120;

/// The card's tallest without the log pane.
const CARD_MAX_HEIGHT: u16 = 36;

/// The largest pixel cat scale.
const MAX_CAT_SCALE: usize = 3;

/// The widest ASCII cat frame, so a centered ASCII cat holds still as it animates.
const ASCII_CAT_COLUMNS: u16 = 13;

/// The token row's hint while some pinned key is not held yet.
const TOKENS_UNPROVEN: &str = "more tokens appear once their balances are proven";

/// The token list's title.
const PICKER_TITLE: &str = "choose a token";

/// The token list's keys, on its bottom border.
const PICKER_KEYS: &str = "type to filter · Enter pick · Esc cancel";

/// The columns between a form row's values on one line.
const VALUE_GAP: &str = "   ";

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
#[derive(Clone, Debug)]
pub struct View {
    header: Line<'static>,
    body: Body,
    /// The keys the footer names, packed into rows when drawn.
    keys: &'static [&'static str],
    /// Whether the footer names `h history` before `l logs`.
    history_key: bool,
    notice: Option<String>,
    /// The tail of the log buffer, oldest first, while the log pane is open.
    logs: Option<Vec<String>>,
}

/// What the body shows.
#[derive(Clone, Debug)]
enum Body {
    /// A cat with a few lines under it, centered in the body: Waiting, Excited, Paused, and
    /// Sending once the receipt is verified.
    Scene { cat: Cat, lines: Vec<Line<'static>> },
    /// Rows from the top left: setup, unlock, the form, review, and Sending until the
    /// receipt is verified.
    Page {
        rows: Vec<Row>,
        /// The idle cat's frame, drawn right of the form where it fits.
        side: Option<u32>,
        /// The token list, drawn over the form.
        picker: Option<Popup>,
        /// The account's newest activities, under the rows where they fit.
        recent: Option<Recent>,
    },
    /// The watched addresses, scrolled by address, drawn as a page: Watching.
    Watch {
        /// One row per watched address.
        addresses: Vec<Row>,
        /// The index in `addresses` of the first shown.
        first: usize,
        /// The rows under the addresses.
        after: Vec<Row>,
        /// The idle cat's frame, drawn right of the list where it fits.
        side: Option<u32>,
        /// The account's newest activities, under the list where they fit.
        recent: Option<Recent>,
    },
    /// The history pane: `head` from the top left, then the table, scrolled so its selected
    /// row shows.
    History { head: Vec<Row>, table: Table },
    /// One entry's details under a blank row: the display rows of `rows` from display row
    /// `first` on, scrolled no further than the last row showing at the bottom.
    Detail { rows: Vec<Row>, first: usize },
}

/// The account's newest entries shown under a page, in the rows it leaves.
#[derive(Clone, Debug)]
struct Recent {
    heading: Line<'static>,
    /// The entries, newest first; `None` when none is recorded.
    table: Option<Table>,
}

impl Recent {
    /// A blank line, the heading and as many whole table rows as fit in `room` rows of a
    /// body `width` columns wide; none unless the heading, the table's heading and one row
    /// fit.
    fn fitted(self, width: u16, room: usize) -> Vec<Line<'static>> {
        let heading = Paragraph::new(vec![self.heading.clone()])
            .wrap(Wrap { trim: false })
            .line_count(width);
        let body = match &self.table {
            Some(table) => table.lines(usize::from(width)),
            None => vec![Line::styled(
                "  no activity recorded yet",
                Style::default().add_modifier(Modifier::DIM),
            )],
        };
        let fits = room.saturating_sub(1 + heading);
        if fits < body.len().min(2) {
            return Vec::new();
        }
        let mut lines = vec![Line::default(), self.heading];
        lines.extend(body.into_iter().take(fits));
        lines
    }
}

/// A column of the history table, in the order drawn.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Column {
    Tx,
    Block,
    Age,
    From,
    To,
    Amount,
    Fee,
}

impl Column {
    const ALL: [Self; 7] = [
        Self::Tx,
        Self::Block,
        Self::Age,
        Self::From,
        Self::To,
        Self::Amount,
        Self::Fee,
    ];

    /// The columns a narrow body drops, first to last. Age, To and Amount always stay.
    const DROPPED: [Self; 4] = [Self::Block, Self::Fee, Self::From, Self::Tx];

    const fn heading(self) -> &'static str {
        match self {
            Self::Tx => "Txn Hash",
            Self::Block => "Block",
            Self::Age => "Age",
            Self::From => "From",
            Self::To => "To",
            Self::Amount => "Amount",
            Self::Fee => "Fee",
        }
    }

    fn cell(self, cells: &Cells) -> &str {
        match self {
            Self::Tx => &cells.tx,
            Self::Block => &cells.block,
            Self::Age => &cells.age,
            Self::From => &cells.from,
            Self::To => &cells.to,
            Self::Amount => &cells.amount,
            Self::Fee => &cells.fee,
        }
    }
}

/// The columns between two table columns.
const COLUMN_GAP: &str = "  ";

/// History entries as a table: one row per entry, each column as wide as its widest cell.
#[derive(Clone, Debug)]
struct Table {
    /// Each entry's cells, and whether it holds a zero transfer it did not send.
    rows: Vec<(Cells, bool)>,
    /// The row marked `>` in the gutter; `None` in a table nothing is chosen from.
    selected: Option<usize>,
}

impl Table {
    fn new(entries: &[activity::Entry], selected: Option<usize>) -> Self {
        Self {
            rows: entries
                .iter()
                .map(|entry| (entry.cells.clone(), entry.warning))
                .collect(),
            selected,
        }
    }

    /// The dim heading line, then one line per row, for a body `width` columns wide. Whole
    /// columns drop in [`Column::DROPPED`] order until the rest fit; then Amount is cut.
    fn lines(&self, width: usize) -> Vec<Line<'static>> {
        let mut columns: Vec<(Column, usize)> = Column::ALL
            .into_iter()
            .map(|column| {
                let widest = self
                    .rows
                    .iter()
                    .map(|(cells, _)| column.cell(cells).chars().count())
                    .fold(column.heading().len(), usize::max);
                let cap = if column == Column::Amount {
                    AMOUNT_COLUMNS
                } else {
                    usize::MAX
                };
                (column, widest.min(cap))
            })
            .collect();
        let total = |columns: &[(Column, usize)]| {
            2 + columns.iter().map(|(_, width)| width).sum::<usize>()
                + COLUMN_GAP.len() * columns.len().saturating_sub(1)
        };
        for dropped in Column::DROPPED {
            if total(&columns) <= width {
                break;
            }
            columns.retain(|(column, _)| *column != dropped);
        }
        let excess = total(&columns).saturating_sub(width);
        for (column, room) in &mut columns {
            if *column == Column::Amount {
                *room = room.saturating_sub(excess).max(1);
            }
        }
        let dim = Style::default().add_modifier(Modifier::DIM);
        let warning = Style::default().fg(Color::Red);
        let line = |gutter: &'static str, cells: Option<&Cells>, flagged: bool| {
            let mut spans = vec![Span::raw(gutter)];
            for (at, &(column, room)) in columns.iter().enumerate() {
                if at > 0 {
                    spans.push(Span::raw(COLUMN_GAP));
                }
                let text = cells.map_or(column.heading(), |cells| column.cell(cells));
                let shown = fit(text, room, at + 1 < columns.len());
                spans.push(if flagged && column == Column::Amount {
                    Span::styled(shown, warning)
                } else {
                    Span::raw(shown)
                });
            }
            cut_line(spans, width)
        };
        let mut lines = Vec::with_capacity(self.rows.len() + 1);
        lines.push(line("  ", None, false).style(dim));
        for (at, (cells, flagged)) in self.rows.iter().enumerate() {
            let gutter = if self.selected == Some(at) {
                "> "
            } else {
                "  "
            };
            lines.push(line(gutter, Some(cells), *flagged));
        }
        lines
    }
}

/// The `spans` cut to `width` columns, ending in `…` when cut.
fn cut_line(spans: Vec<Span<'static>>, width: usize) -> Line<'static> {
    let columns: usize = spans.iter().map(|span| span.content.chars().count()).sum();
    if columns <= width {
        return Line::from(spans);
    }
    let mut room = width.saturating_sub(1);
    let mut cut = Vec::with_capacity(spans.len());
    for span in spans {
        let count = span.content.chars().count();
        if count < room {
            room -= count;
            cut.push(span);
            continue;
        }
        let text: String = span.content.chars().take(room).chain(['…']).collect();
        cut.push(Span::styled(text, span.style));
        break;
    }
    Line::from(cut)
}

/// `text` in `width` columns: cut with `…` when wider, padded with spaces when `pad`.
fn fit(text: &str, width: usize, pad: bool) -> String {
    let columns = text.chars().count();
    if columns > width {
        let mut cut: String = text.chars().take(width.saturating_sub(1)).collect();
        cut.push('…');
        return cut;
    }
    if pad {
        format!("{text}{}", " ".repeat(width - columns))
    } else {
        text.to_owned()
    }
}

/// A watch list laid out for a body of a given width.
struct WatchList {
    /// Each address's lines and the body rows they take.
    addresses: Vec<(Vec<Line<'static>>, usize)>,
    /// The index in `addresses` of the first shown.
    first: usize,
    /// The lines under the addresses.
    after: Vec<Line<'static>>,
    /// The body rows `after` takes.
    after_rows: usize,
}

impl WatchList {
    /// `addresses` from `first` and `after` laid out for a body `width` columns wide.
    fn new(addresses: Vec<Row>, first: usize, after: Vec<Row>, width: u16) -> Self {
        let lay_out = |rows: Vec<Row>| {
            let mut lines = Vec::new();
            for row in rows {
                row.lay_out(usize::from(width), &mut lines);
            }
            let height = Paragraph::new(lines.clone())
                .wrap(Wrap { trim: false })
                .line_count(width);
            (lines, height)
        };
        let first = first.min(addresses.len());
        let addresses = addresses
            .into_iter()
            .map(|row| lay_out(vec![row]))
            .collect();
        let (after, after_rows) = lay_out(after);
        Self {
            addresses,
            first,
            after,
            after_rows,
        }
    }

    /// What a body `height` rows tall shows: the end of the addresses shown, then whether
    /// it shows `↓ N more`, the top row and `after`. Normally the addresses that fit between
    /// the top row and `after`, less a row for `↓ N more` when some below do not. When the
    /// first shown does not fit so, it is shown anyway, clipped at the bottom, and the rest
    /// fill what room is left: addresses that fit whole, `↓ N more`, the top row, `after`.
    fn shown(&self, height: u16) -> (usize, bool, bool, bool) {
        let len = self.addresses.len();
        let room = usize::from(height).saturating_sub(1 + self.after_rows);
        let rest = &self.addresses[self.first..];
        if rest.iter().map(|(_, rows)| rows).sum::<usize>() <= room {
            return (len, false, true, true);
        }
        let room = room.saturating_sub(1);
        let mut used = 0;
        let mut end = self.first;
        for (_, rows) in rest {
            if used + rows > room {
                break;
            }
            used += rows;
            end += 1;
        }
        if end > self.first {
            return (end, end < len, true, true);
        }
        let mut room = usize::from(height);
        for (_, rows) in rest {
            if end > self.first && *rows > room {
                break;
            }
            room = room.saturating_sub(*rows);
            end += 1;
        }
        let down = end < len && room > 0;
        room -= usize::from(down);
        let top = room > 0;
        room -= usize::from(top);
        (end, down, top, self.after_rows <= room)
    }

    /// Whether a body `height` rows tall hides some address.
    fn scrolls(&self, height: u16) -> bool {
        self.first > 0 || self.shown(height).0 < self.addresses.len()
    }

    /// The page rows for a body `height` rows tall: a dim `↑ N more` (else a blank row), the
    /// addresses shown, a dim `↓ N more` when some below are hidden, then `after`.
    fn rows(mut self, height: u16) -> Vec<Row> {
        let dim = Style::default().add_modifier(Modifier::DIM);
        let (first, len) = (self.first, self.addresses.len());
        let (end, down, top, after) = self.shown(height);
        let mut rows = Vec::new();
        if top {
            rows.push(Row::Text(if first > 0 {
                Line::styled(format!("  ↑ {first} more"), dim)
            } else {
                Line::default()
            }));
        }
        for (lines, _) in self.addresses.drain(first..end) {
            rows.extend(lines.into_iter().map(Row::Text));
        }
        if down {
            rows.push(Row::Text(Line::styled(
                format!("  ↓ {} more", len - end),
                dim,
            )));
        }
        if after {
            rows.extend(self.after.into_iter().map(Row::Text));
        }
        rows
    }
}

/// A page row.
#[derive(Clone, Debug)]
enum Row {
    /// A line word-wrapped to the body's width.
    Text(Line<'static>),
    /// A line in a style, wrapped at spaces, each continuation [`HANGING_INDENT`] in.
    Hanging(String, Style),
    /// A form row: its label, then its values [`VALUE_GAP`] apart, each kept whole. A value
    /// that does not fit on the line starts the next one, under the first value.
    Form {
        label: Span<'static>,
        values: Vec<Span<'static>>,
    },
}

impl Row {
    /// Pushes its lines for a body `width` columns wide.
    fn lay_out(self, width: usize, lines: &mut Vec<Line<'static>>) {
        let (label, values) = match self {
            Self::Text(line) => return lines.push(line),
            Self::Hanging(text, style) => {
                return lines.extend(
                    hanging(&text, width)
                        .into_iter()
                        .map(|line| line.style(style)),
                );
            }
            Self::Form { label, values } => (label, values),
        };
        let mut line = Line::from(label);
        let indent = line.width();
        for (at, value) in values.into_iter().enumerate() {
            if at > 0 {
                if line.width() + VALUE_GAP.len() + value.width() <= width {
                    line.push_span(VALUE_GAP);
                } else {
                    lines.push(std::mem::replace(&mut line, Line::raw(" ".repeat(indent))));
                }
            }
            line.push_span(value);
        }
        lines.push(line);
    }
}

/// A scene's cat: frame `n` of a pixel cat pose, or an ASCII cat.
#[derive(Clone, Copy, Debug)]
enum Cat {
    Pixels { pose: Pose, n: u32 },
    Ascii([&'static str; 4]),
}

/// The token list drawn in a bordered box over the body, below one of its lines.
#[derive(Clone, Debug)]
struct Popup {
    /// The page row it opens below.
    below: usize,
    filter: Line<'static>,
    /// One line per match, or why there is none.
    rows: Vec<Line<'static>>,
    /// The highlighted entry of `rows`.
    highlighted: usize,
    hint: Option<Line<'static>>,
}

impl View {
    /// The screen for `app` at `now`, with `logs` in the log pane when it is open.
    pub fn new(app: &App, logs: Vec<String>, now: Instant) -> Self {
        Self {
            header: header_line(app),
            body: body(app, now),
            keys: keys(app),
            history_key: app.history_key() && !app.history_shown(),
            notice: app.notice.clone(),
            logs: app.logs.then_some(logs),
        }
    }
}

/// The card in `area` with a log pane `pane` rows tall, `None` when `area` is too small for
/// one.
fn card(area: Rect, pane: u16) -> Option<Rect> {
    if area.width < CARD_MIN_WIDTH || area.height < CARD_MIN_HEIGHT {
        return None;
    }
    let width = (area.width - 4).min(CARD_MAX_WIDTH);
    let height = (area.height - 2).min(CARD_MAX_HEIGHT.saturating_add(pane));
    Some(Rect::new(
        area.x + (area.width - width) / 2,
        area.y + (area.height - height) / 2,
        width,
        height,
    ))
}

/// Draws the whole screen: in a card titled with the header when the terminal has room for
/// one, else borderless under a header row.
pub fn draw(frame: &mut Frame<'_>, view: View) {
    let pane = log_pane(view.logs.is_some());
    let screen = frame.area();
    if let Some(card) = card(screen, pane) {
        frame.render_widget(Clear, card);
        frame.render_widget(card_block(view.header), card);
    } else {
        frame.render_widget(
            Paragraph::new(view.header),
            Rect {
                height: screen.height.min(1),
                ..screen
            },
        );
    }
    let area = inner(screen, pane);
    let notice = view.notice.map(notice_paragraph);
    let keys = with_history(view.keys, view.history_key);
    let (mut areas, mut footer) = split(area, &keys, notice.as_ref(), pane);
    match view.body {
        Body::Scene { cat, lines } => scene(frame, cat, lines, areas[0]),
        Body::Page {
            rows,
            side,
            picker,
            recent,
        } => page(frame, rows, side, picker, recent, areas[0]),
        Body::Watch {
            addresses,
            first,
            after,
            side,
            recent,
        } => {
            let list = WatchList::new(addresses, first, after, area.width);
            if list.scrolls(areas[0].height) {
                let keys = with_history(WATCH_SCROLL_KEYS, view.history_key);
                (areas, footer) = split(area, &keys, notice.as_ref(), pane);
            }
            let rows = list.rows(areas[0].height);
            page(frame, rows, side, None, recent, areas[0]);
        }
        Body::History { head, table } => history_page(frame, head, &table, areas[0]),
        Body::Detail { rows, first } => detail_page(frame, rows, first, areas[0]),
    }
    let [_, notice_area, footer_area, log_area] = areas;
    if let Some(paragraph) = notice {
        frame.render_widget(paragraph, notice_area);
    }
    frame.render_widget(footer, footer_area);
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

/// The rows the log pane takes, borders included, when `open`.
const fn log_pane(open: bool) -> u16 {
    if open { LOG_PANE_LINES + 2 } else { 0 }
}

/// The card's border, titled with `header`.
fn card_block(header: Line<'static>) -> Block<'static> {
    Block::bordered()
        .border_type(BorderType::Rounded)
        .title(header)
        .padding(Padding::horizontal(1))
}

/// Where the screen draws on `screen` with a log pane `pane` rows tall: inside the card when
/// there is room for one, else under the header row.
fn inner(screen: Rect, pane: u16) -> Rect {
    if let Some(card) = card(screen, pane) {
        return card_block(Line::default()).inner(card);
    }
    let header = screen.height.min(1);
    Rect {
        y: screen.y + header,
        height: screen.height - header,
        ..screen
    }
}

/// The notice `text`, in yellow and wrapped.
fn notice_paragraph(text: String) -> Paragraph<'static> {
    Paragraph::new(Line::styled(text, Style::default().fg(Color::Yellow)))
        .wrap(Wrap { trim: false })
}

/// The body, notice, footer, and log pane areas of `area`, top to bottom, for `notice`, a
/// log pane `pane` rows tall, and a footer naming `keys`, which it returns too.
fn split(
    area: Rect,
    keys: &[&'static str],
    notice: Option<&Paragraph<'_>>,
    pane: u16,
) -> ([Rect; 4], Paragraph<'static>) {
    let footer = Paragraph::new(footer_lines(keys, area.width)).wrap(Wrap { trim: false });
    let footer_rows = u16::try_from(footer.line_count(area.width))
        .unwrap_or(u16::MAX)
        .max(1);
    let free = area.height.saturating_sub(1);
    let rows = notice.map_or(0, |paragraph| {
        u16::try_from(paragraph.line_count(area.width))
            .unwrap_or(u16::MAX)
            .min(free)
    });
    let rest = free - rows;
    let more = (footer_rows - 1).min(rest.saturating_sub(1));
    let rest = rest - more;
    let pane = if rest >= 2 { pane.min(rest - 1) } else { 0 };
    let mut next = area.y;
    let mut take = |height: u16| {
        let rect = Rect::new(area.x, next, area.width, height.min(area.bottom() - next));
        next += rect.height;
        rect
    };
    let body = take(rest - pane);
    let notice_area = take(rows);
    let footer_area = take(1 + more);
    let log_area = take(pane);
    ([body, notice_area, footer_area, log_area], footer)
}

/// Draws the cat, a blank line and `lines` under it, each centered, the whole centered in
/// `body`.
fn scene(frame: &mut Frame<'_>, cat: Cat, lines: Vec<Line<'static>>, body: Rect) {
    let (art, width) = match cat {
        Cat::Pixels { pose, n } => {
            let scale = cat_scale(body);
            let width = u16::try_from(cat::WIDTH * scale).unwrap_or(u16::MAX);
            (cat::lines(&cat::frame(pose, n), scale), width)
        }
        Cat::Ascii(rows) => (rows.map(Line::raw).to_vec(), ASCII_CAT_COLUMNS),
    };
    let art_rows = u16::try_from(art.len()).unwrap_or(u16::MAX);
    let text = Paragraph::new(lines)
        .alignment(Alignment::Center)
        .wrap(Wrap { trim: false });
    let text_rows = u16::try_from(text.line_count(body.width)).unwrap_or(u16::MAX);
    let height = art_rows.saturating_add(1).saturating_add(text_rows);
    let top = body.y + body.height.saturating_sub(height) / 2;
    let left = body.x + body.width.saturating_sub(width) / 2;
    frame.render_widget(
        Paragraph::new(art),
        Rect::new(left, top, width, art_rows).intersection(body),
    );
    frame.render_widget(
        text,
        Rect::new(
            body.x,
            top.saturating_add(art_rows).saturating_add(1),
            body.width,
            text_rows,
        )
        .intersection(body),
    );
}

/// The largest pixel cat scale up to [`MAX_CAT_SCALE`] at which the cat takes at most half
/// of `body` each way, at least 1.
fn cat_scale(body: Rect) -> usize {
    let (width, height) = (usize::from(body.width), usize::from(body.height));
    (1..=MAX_CAT_SCALE)
        .rev()
        .find(|scale| cat::WIDTH * scale <= width / 2 && cat::HEIGHT / 2 * scale <= height / 2)
        .unwrap_or(1)
}

/// Draws `rows` from the top left of `body`, the side cat right of them where it fits,
/// `recent` in the rows left under both, and the token list over them.
fn page(
    frame: &mut Frame<'_>,
    rows: Vec<Row>,
    side: Option<u32>,
    picker: Option<Popup>,
    recent: Option<Recent>,
    body: Rect,
) {
    let mut lines = Vec::with_capacity(rows.len());
    let mut above = 0;
    for (at, row) in rows.into_iter().enumerate() {
        row.lay_out(usize::from(body.width), &mut lines);
        if picker.as_ref().is_some_and(|popup| popup.below == at) {
            above = lines.len();
        }
    }
    let side = side.and_then(|n| side_cat(&lines, &cat::frame(Pose::Idle, n), body));
    let picker = picker.map(|popup| {
        let above = Paragraph::new(lines[..above].to_vec())
            .wrap(Wrap { trim: false })
            .line_count(body.width);
        let top = body
            .y
            .saturating_add(u16::try_from(above).unwrap_or(u16::MAX));
        let room = body.bottom().saturating_sub(top).saturating_sub(2);
        let lines = popup.lines(usize::from(room));
        (popup_area(popup.widest(), lines.len(), top, body), lines)
    });
    let page = Paragraph::new(lines).wrap(Wrap { trim: false });
    let recent = recent.map(|recent| {
        let cat_rows = side.as_ref().map_or(0, |(rect, _)| rect.bottom() - body.y);
        let top = u16::try_from(page.line_count(body.width))
            .unwrap_or(u16::MAX)
            .max(cat_rows);
        let room = body.height.saturating_sub(top);
        let lines = recent.fitted(body.width, usize::from(room));
        let rect = Rect::new(body.x, body.y.saturating_add(top), body.width, room);
        (rect.intersection(body), lines)
    });
    frame.render_widget(page, body);
    if let Some((rect, lines)) = recent {
        frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), rect);
    }
    if let Some((rect, cat)) = side {
        frame.render_widget(Paragraph::new(cat), rect);
    }
    if let Some((rect, lines)) = picker {
        frame.render_widget(Clear, rect);
        frame.render_widget(
            Paragraph::new(lines).block(
                Block::bordered()
                    .title(PICKER_TITLE)
                    .title_bottom(PICKER_KEYS),
            ),
            rect,
        );
    }
}

/// Draws `head` from the top left of `body`, then the table's heading and as many of its rows
/// as fit, starting low enough that the selected row shows.
fn history_page(frame: &mut Frame<'_>, head: Vec<Row>, table: &Table, body: Rect) {
    let mut lines = Vec::new();
    for row in head {
        row.lay_out(usize::from(body.width), &mut lines);
    }
    let used = Paragraph::new(lines.clone())
        .wrap(Wrap { trim: false })
        .line_count(body.width);
    let mut table_lines = table.lines(usize::from(body.width)).into_iter();
    lines.extend(table_lines.next());
    let room = usize::from(body.height).saturating_sub(used + 1).max(1);
    let first = table
        .selected
        .map_or(0, |selected| (selected + 1).saturating_sub(room));
    lines.extend(table_lines.skip(first).take(room));
    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), body);
}

/// Draws a blank row, then the display rows of `rows` from display row `first` on in the
/// rest of `body`, starting no later than [`detail_last`].
fn detail_page(frame: &mut Frame<'_>, rows: Vec<Row>, first: usize, body: Rect) {
    let laid = laid_out(rows, body.width);
    let first = first.min(detail_last(laid.len(), body));
    let mut lines = vec![Line::default()];
    lines.extend(laid.into_iter().skip(first));
    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), body);
}

/// The display rows of `rows` in a body `width` columns wide.
fn laid_out(rows: Vec<Row>, width: u16) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    for row in rows {
        row.lay_out(usize::from(width), &mut lines);
    }
    lines
}

/// The last of `lines` display rows a detail can start from in `body`: the one that leaves
/// the rows under its blank row full, 0 when all of them fit.
fn detail_last(lines: usize, body: Rect) -> usize {
    lines.saturating_sub(usize::from(body.height).saturating_sub(1))
}

/// [`detail_last`] for the details open in `app` on its `width` x `height` terminal, 0 when
/// none show.
pub fn detail_end(app: &App) -> usize {
    let HistoryMode::Detail { first } = app.history_mode() else {
        return 0;
    };
    let Body::Detail { rows, .. } = detail(app, first) else {
        return 0;
    };
    let pane = log_pane(app.logs);
    let notice = app.notice.clone().map(notice_paragraph);
    let keys = with_history(keys(app), app.history_key() && !app.history_shown());
    let area = inner(Rect::new(0, 0, app.width, app.height), pane);
    let ([body, ..], _) = split(area, &keys, notice.as_ref(), pane);
    detail_last(laid_out(rows, body.width).len(), body)
}

/// The side cat cropped to its pixels and where it goes in `body`: two columns right of the
/// widest of `lines` and level with the first, when `body` is at least [`SIDE_CAT_COLUMNS`]
/// wide, at the scale [`side_cat_size`] picks.
fn side_cat(
    lines: &[Line<'_>],
    grid: &cat::Grid,
    body: Rect,
) -> Option<(Rect, Vec<Line<'static>>)> {
    if body.width < SIDE_CAT_COLUMNS {
        return None;
    }
    let x = u16::try_from(lines.iter().map(Line::width).max().unwrap_or(0) + 2).ok()?;
    let room = body.height.saturating_sub(1);
    let bounds = cat::Bounds::of(grid)?;
    let (scale, width, height) = side_cat_size(&bounds, body.width.checked_sub(x)?, room)?;
    Some((
        Rect::new(body.x + x, body.y + 1, width, height.min(room)),
        cat::cropped(grid, &bounds, scale),
    ))
}

/// The scale and the cells across and down of a cat cropped to `bounds`: twice its size
/// where that fits whole in `columns` by `rows`, else its own size where it fits across.
fn side_cat_size(bounds: &cat::Bounds, columns: u16, rows: u16) -> Option<(usize, u16, u16)> {
    [2, 1].into_iter().find_map(|scale| {
        let (across, down) = bounds.cells(scale);
        let width = u16::try_from(across.len()).ok()?;
        let height = u16::try_from(down.len()).unwrap_or(u16::MAX);
        (width <= columns && (scale == 1 || height <= rows)).then_some((scale, width, height))
    })
}

/// Where a box `height` lines tall goes: from row `top`, two columns into `body`, as wide as
/// `widest`, title and keys need up to two columns less than `body`, clipped to `body`.
fn popup_area(widest: usize, height: usize, top: u16, body: Rect) -> Rect {
    let widest = [PICKER_TITLE.chars().count(), PICKER_KEYS.chars().count()]
        .into_iter()
        .fold(widest, usize::max);
    let width = u16::try_from(widest + 2)
        .unwrap_or(u16::MAX)
        .min(body.width.saturating_sub(2));
    let height = u16::try_from(height + 2).unwrap_or(u16::MAX);
    Rect::new(body.x.saturating_add(2), top, width, height).intersection(body)
}

fn header_line(app: &App) -> Line<'static> {
    let mut spans = vec![
        Span::styled("chibi", Style::default().add_modifier(Modifier::BOLD)),
        Span::raw(format!(" · {}", app.network)),
    ];
    if app.watch_only() {
        spans.push(Span::raw(" · watching"));
    }
    if let Some(name) = &app.balances.name {
        spans.push(Span::raw(format!(" · {name}")));
    }
    if let Some(sender) = app.sender {
        spans.push(Span::raw(format!(" · {}", sender.to_checksum(None))));
    }
    Line::from(spans)
}

/// The pixel cat up at `now` if there is one, else `ascii`.
fn cat(app: &App, now: Instant, ascii: [&'static str; 4]) -> Cat {
    app.cat().map_or(Cat::Ascii(ascii), |pose| Cat::Pixels {
        pose,
        n: app.cat_frame(now),
    })
}

/// Frame `n` of an animation, one step per tick.
fn pick<T: Copy, const N: usize>(frames: &[T; N], tick: u64) -> T {
    frames[usize::try_from(tick % N as u64).unwrap_or_default()]
}

/// A page of `lines` under a blank one, with no side cat or token list.
fn plain(write: impl FnOnce(&mut Vec<Line<'static>>)) -> Body {
    let mut lines = Vec::new();
    write(&mut lines);
    rows(lines.into_iter().map(Row::Text).collect())
}

/// A page of `rows` under a blank line, with no side cat or token list.
fn rows(rows: Vec<Row>) -> Body {
    Body::Page {
        rows: std::iter::once(Row::Text(Line::default()))
            .chain(rows)
            .collect(),
        side: None,
        picker: None,
        recent: None,
    }
}

/// The newest entries of the account in view, once a read for it has finished, headed by the
/// account itself when several addresses are watched.
fn recent(app: &App) -> Option<Recent> {
    let account = app.history_account()?;
    let (read, history) = app.history.as_ref()?.read.as_ref()?;
    if *read != account {
        return None;
    }
    let heading = if app.screen == Screen::Watching && app.watched.len() > 1 {
        format!("  recent activity of {}", account.to_checksum(None))
    } else {
        "  recent activity".to_owned()
    };
    Some(Recent {
        heading: Line::styled(heading, Style::default().add_modifier(Modifier::DIM)),
        table: (!history.entries.is_empty()).then(|| Table::new(&history.entries, None)),
    })
}

fn body(app: &App, now: Instant) -> Body {
    match app.history_mode() {
        HistoryMode::Closed => {}
        HistoryMode::List => return history(app),
        HistoryMode::Detail { first } => return detail(app, first),
    }
    match app.screen {
        Screen::Menu => plain(|lines| menu(app, lines)),
        Screen::Backup => plain(|lines| backup(app, lines)),
        Screen::Confirm => plain(|lines| confirm(app, lines)),
        Screen::Import => plain(|lines| import(app, lines)),
        Screen::Existing => plain(|lines| existing(app, lines)),
        Screen::Password => plain(|lines| new_password(app, lines)),
        Screen::WatchAddress => plain(|lines| watch_address(app, lines)),
        Screen::Unlock => plain(|lines| unlock(app, lines)),
        Screen::Waiting => Body::Scene {
            cat: cat(app, now, pick(&DOZING, app.frame)),
            lines: vec![Line::raw(format!(
                "looking for a state peer · peers {} · state peers {} · {}",
                app.waiting.peers,
                app.waiting.state_peers,
                elapsed(app.waiting.elapsed)
            ))],
        },
        Screen::Excited => Body::Scene {
            cat: cat(app, now, pick(&EXCITED, app.frame)),
            lines: vec![
                Line::raw("found a state peer!"),
                Line::raw("anchoring at a verified head..."),
            ],
        },
        Screen::Paused => Body::Scene {
            cat: cat(app, now, pick(&SLEEPY, app.frame)),
            lines: vec![
                Line::raw(PAUSED),
                Line::raw(if app.watch_only() {
                    "nothing can be proven until one connects"
                } else {
                    "nothing can be proven or sent until one connects; your form is kept"
                }),
            ],
        },
        Screen::Ready => {
            let mut rows = vec![Row::Text(Line::default())];
            let token_row = form(app, &mut rows);
            let picker = picker_popup(app, token_row);
            let side =
                (app.cat() == Some(Pose::Idle) && picker.is_none()).then(|| app.cat_frame(now));
            Body::Page {
                rows,
                side,
                picker,
                recent: recent(app),
            }
        }
        Screen::Watching => {
            let mut after = Vec::new();
            if app.tokens_unproven {
                let dim = Style::default().add_modifier(Modifier::DIM);
                after.push(Row::Text(Line::default()));
                after.push(Row::Text(Line::styled(format!("  {TOKENS_UNPROVEN}"), dim)));
            }
            Body::Watch {
                addresses: watched(app),
                first: app.watch_first,
                after,
                side: (app.cat() == Some(Pose::Idle)).then(|| app.cat_frame(now)),
                recent: recent(app),
            }
        }
        Screen::Review => plain(|lines| review(app, lines)),
        Screen::Sending => sending(app, now),
    }
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

fn watch_address(app: &App, lines: &mut Vec<Line<'static>>) {
    lines.push(Line::raw(
        "the address to watch: 0x and 40 hex digits, checksummed or all lowercase",
    ));
    lines.push(Line::default());
    lines.push(Line::raw(format!("address  {}", app.setup.address)));
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

/// Pushes the form, returning the index of its token row.
fn form(app: &App, rows: &mut Vec<Row>) -> usize {
    let from = app
        .sender
        .map(|sender| sender.to_checksum(None))
        .unwrap_or_default();
    rows.push(Row::Form {
        label: Span::raw("  from    "),
        values: vec![
            Span::raw(from),
            Span::raw(balance(app.balance(Asset::Eth), Asset::Eth)),
        ],
    });
    let asset = app.form.asset;
    let field = |name: Field, label: &str, values: Vec<Span<'static>>| {
        let marker = if app.form.field == name { ">" } else { " " };
        Row::Form {
            label: Span::raw(format!("{marker} {label:<7} ")),
            values,
        }
    };
    let mut token = vec![
        Span::raw(format!("< {} >", asset.symbol())),
        Span::raw(balance(app.balance(asset), asset)),
    ];
    let dim = Style::default().add_modifier(Modifier::DIM);
    if app.form.field == Field::Token {
        token.push(Span::styled("Enter to choose", dim));
    }
    if app.tokens_unproven {
        token.push(Span::styled(TOKENS_UNPROVEN, dim));
    }
    let token_row = rows.len();
    rows.push(field(Field::Token, "token", token));
    rows.push(field(
        Field::Amount,
        "amount",
        vec![Span::raw(app.form.amount.clone())],
    ));
    if let Some(error) = &app.form.amount_error {
        rows.push(Row::Text(error_line(error)));
    }
    rows.push(field(Field::To, "to", vec![Span::raw(app.form.to.clone())]));
    if let Some(error) = &app.form.to_error {
        rows.push(Row::Text(error_line(error)));
    } else {
        match app.resolution() {
            Some(Resolution::Pending) => {
                rows.push(Row::Text(Line::styled("          resolving...", dim)));
            }
            Some(Resolution::Resolved(address)) => rows.push(Row::Text(Line::styled(
                format!("          → {}", address.to_checksum(None)),
                dim,
            ))),
            Some(Resolution::Failed(reason)) => rows.push(Row::Text(error_line(reason))),
            None => {}
        }
        if let Some(warning) = app.to_warning() {
            rows.push(Row::Text(Line::styled(
                format!("          {warning}"),
                Style::default().fg(Color::Yellow),
            )));
        }
    }
    let fee = match app.fee() {
        Some(Ok(fee)) => fee_text(fee),
        Some(Err(reason)) => reason.clone(),
        None if app.form.to.is_empty() || app.form.amount.is_empty() => String::new(),
        None => "estimating...".to_owned(),
    };
    rows.push(Row::Form {
        label: Span::raw("  fee     "),
        values: vec![Span::raw(fee)],
    });
    if let Some(error) = &app.form.fee_error {
        rows.push(Row::Text(error_line(error)));
    }
    token_row
}

/// One row per watched address: the address, its name, ETH and each held token.
fn watched(app: &App) -> Vec<Row> {
    let mut rows = Vec::with_capacity(app.watched.len());
    for (address, balances) in &app.watched {
        let mut values = vec![Span::raw(address.to_checksum(None))];
        if let Some(name) = &balances.name {
            values.push(Span::raw(name.clone()));
        }
        values.push(Span::raw(balance(balances.eth, Asset::Eth)));
        values.extend(
            balances
                .tokens
                .iter()
                .map(|(token, amount)| Span::raw(show(*amount, Asset::Token(*token)))),
        );
        rows.push(Row::Form {
            label: Span::raw("  "),
            values,
        });
    }
    rows
}

/// The open token list below body line `below`: the filter, then each matching asset with
/// its balance, the highlighted one marked.
fn picker_popup(app: &App, below: usize) -> Option<Popup> {
    let picker = app.form.picker.as_ref()?;
    let assets = app.assets();
    let width = |text: fn(Asset) -> &'static str| {
        assets
            .iter()
            .map(|asset| text(*asset).chars().count())
            .max()
            .unwrap_or(0)
    };
    let (symbols, names) = (width(Asset::symbol), width(Asset::name));
    let matches = picker.matches(&assets);
    let highlighted = picker.position(&matches);
    let mut rows: Vec<Line<'static>> = matches
        .into_iter()
        .enumerate()
        .map(|(at, asset)| {
            let marker = if at == highlighted { ">" } else { " " };
            Line::raw(format!(
                "{marker} {:<symbols$}  {:<names$}  {}",
                asset.symbol(),
                asset.name(),
                balance(app.balance(asset), asset)
            ))
        })
        .collect();
    if rows.is_empty() {
        rows.push(Line::raw("no token matches"));
    }
    Some(Popup {
        below,
        filter: Line::raw(format!("filter  {}", picker.filter)),
        rows,
        highlighted,
        hint: app.tokens_unproven.then(|| {
            Line::styled(
                TOKENS_UNPROVEN,
                Style::default().add_modifier(Modifier::DIM),
            )
        }),
    })
}

impl Popup {
    /// The widest line it can show.
    fn widest(&self) -> usize {
        std::iter::once(&self.filter)
            .chain(&self.rows)
            .chain(&self.hint)
            .map(Line::width)
            .max()
            .unwrap_or(0)
    }

    /// Its lines in at most `room` rows where it can: when the rows do not fit, a window
    /// keeping the highlighted one, with a dim count above and below for those left out.
    /// When even that is too tall, the rows come first, then the counts, the filter and
    /// the hint.
    fn lines(&self, room: usize) -> Vec<Line<'static>> {
        let fit = room
            .saturating_sub(1 + usize::from(self.hint.is_some()))
            .max(1);
        let (count, at) = (self.rows.len(), self.highlighted);
        let (start, end) = if count <= fit {
            (0, count)
        } else {
            let shown = fit.saturating_sub(1).max(1);
            if at < shown {
                (0, shown)
            } else if at >= count - shown {
                (count - shown, count)
            } else {
                let shown = fit.saturating_sub(2).max(1);
                (at + 1 - shown, at + 1)
            }
        };
        let more = |arrow: &str, hidden: usize| {
            Line::styled(
                format!("{arrow} {hidden} more"),
                Style::default().add_modifier(Modifier::DIM),
            )
        };
        let mut lines = vec![self.filter.clone()];
        if start > 0 {
            lines.push(more("↑", start));
        }
        lines.extend_from_slice(&self.rows[start..end]);
        if end < count {
            lines.push(more("↓", count - end));
        }
        lines.extend(self.hint.clone());
        if lines.len() <= room.max(1) {
            return lines;
        }
        let shown = room.clamp(1, count);
        let start = (at + 1).saturating_sub(shown).min(count - shown);
        let end = start + shown;
        let mut spare = room.max(1) - shown;
        let mut take = |wanted: bool| {
            let taken = wanted && spare > 0;
            spare -= usize::from(taken);
            taken
        };
        let up = take(start > 0);
        let down = take(end < count);
        let filter = take(true);
        let hint = take(self.hint.is_some());
        let mut lines = Vec::with_capacity(room);
        if filter {
            lines.push(self.filter.clone());
        }
        if up {
            lines.push(more("↑", start));
        }
        lines.extend_from_slice(&self.rows[start..end]);
        if down {
            lines.push(more("↓", count - end));
        }
        if hint {
            lines.extend(self.hint.clone());
        }
        lines
    }
}

fn review(app: &App, lines: &mut Vec<Line<'static>>) {
    let Some(review) = &app.review else {
        return;
    };
    let draft = review.draft;
    lines.push(Line::raw("review the transfer"));
    lines.push(Line::default());
    lines.push(Line::raw(format!(
        "  network  {} (chain id {})",
        app.network, app.chain_id
    )));
    match &review.name {
        Some(name) => {
            lines.push(Line::raw(format!("  to       {name}")));
            lines.push(Line::raw(format!(
                "           {}",
                draft.to.to_checksum(None)
            )));
        }
        None => lines.push(Line::raw(format!(
            "  to       {}",
            draft.to.to_checksum(None)
        ))),
    }
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

/// The transfer under way: a scene with the cat once its receipt is verified, else a page.
fn sending(app: &App, now: Instant) -> Body {
    let Some(sending) = &app.sending else {
        return plain(|_| {});
    };
    let ascii = match sending.state {
        SendState::Succeeded => Some(HAPPY),
        SendState::Failed => Some(SAD),
        SendState::Signing
        | SendState::Pending
        | SendState::Included
        | SendState::Rejected(_)
        | SendState::Dropped => None,
    };
    let mut lines = vec![Line::raw(format!(
        "sending {} to {}",
        show(sending.draft.amount, sending.draft.asset),
        sending.draft.to.to_checksum(None)
    ))];
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
    if let (SendState::Succeeded | SendState::Failed, Some(hash)) = (&sending.state, sending.hash) {
        lines.push(Line::raw(app.explorer.tx_url(hash)));
    }
    match ascii {
        Some(ascii) => Body::Scene {
            cat: cat(app, now, ascii),
            lines,
        },
        None => plain(|page| page.append(&mut lines)),
    }
}

/// How far an activity's continuation rows sit in from the body's edge.
const HANGING_INDENT: &str = "    ";

/// `text` in lines at most `width` columns wide, broken at spaces with each continuation
/// [`HANGING_INDENT`] in, and a word wider than a continuation's room split at the width.
fn hanging(text: &str, width: usize) -> Vec<Line<'static>> {
    let columns = |text: &str| text.chars().count();
    if columns(text) <= width {
        return vec![Line::raw(text.to_owned())];
    }
    let words = text.trim_start_matches(' ');
    let mut line = text[..text.len() - words.len()].to_owned();
    let mut lines = Vec::new();
    let mut fresh = true;
    let mut gap = 1;
    for word in words.split(' ') {
        if word.is_empty() {
            gap += 1;
            continue;
        }
        if !fresh {
            if columns(&line) + gap + columns(word) > width {
                lines.push(Line::raw(std::mem::replace(
                    &mut line,
                    HANGING_INDENT.to_owned(),
                )));
            } else {
                line.extend(std::iter::repeat_n(' ', gap));
            }
        }
        let mut word = word;
        loop {
            let room = width.saturating_sub(columns(&line)).max(1);
            let Some((split, _)) = word.char_indices().nth(room) else {
                line.push_str(word);
                break;
            };
            line.push_str(&word[..split]);
            lines.push(Line::raw(std::mem::replace(
                &mut line,
                HANGING_INDENT.to_owned(),
            )));
            word = &word[split..];
        }
        fresh = false;
        gap = 1;
    }
    lines.push(Line::raw(line));
    lines
}

/// The history pane: the account, its dim coverage line, then a table of its entries with
/// the selected one marked.
fn history(app: &App) -> Body {
    let Some(account) = app.history_account() else {
        return rows(Vec::new());
    };
    let text = |text: String| Row::Text(Line::raw(text));
    let mut head = vec![text(format!("  {}", account.to_checksum(None)))];
    let read = app
        .history
        .as_ref()
        .and_then(|pane| Some((pane.read.as_ref()?, pane.selected)));
    let Some(((_, history), selected)) = read.filter(|((read, _), _)| *read == account) else {
        head.push(text("  reading history...".to_owned()));
        return rows(head);
    };
    let dim = Style::default().add_modifier(Modifier::DIM);
    head.push(Row::Hanging(format!("  {}", history.coverage), dim));
    if history.entries.is_empty() {
        return rows(head);
    }
    head.insert(0, Row::Text(Line::default()));
    head.push(Row::Text(Line::default()));
    Body::History {
        head,
        table: Table::new(&history.entries, Some(selected)),
    }
}

/// The selected entry's details from display row `first` on, each line whole and wrapped
/// under itself; the table when nothing is selected or the entries read are about another
/// account than the one in view.
fn detail(app: &App, first: usize) -> Body {
    let account = app.history_account();
    let selected = app.history.as_ref().and_then(|pane| {
        let (_, history) = pane
            .read
            .as_ref()
            .filter(|(read, _)| Some(*read) == account)?;
        history.entries.get(pane.selected)
    });
    let Some(entry) = selected else {
        return history(app);
    };
    let rows = entry
        .detail
        .iter()
        .map(|line| match line {
            DetailLine::Blank => Row::Text(Line::default()),
            DetailLine::Field { .. } | DetailLine::Text(_) => {
                Row::Hanging(format!("  {line}"), Style::default())
            }
        })
        .collect();
    Body::Detail { rows, first }
}

/// The keys the footer names on Watching when some address is hidden.
const WATCH_SCROLL_KEYS: &[&str] = &["up/down scroll", "l logs", "q quit"];

/// The keys the footer names over the history table.
const HISTORY_LIST_KEYS: &[&str] = &[
    "up/down choose",
    "Enter details",
    "h/Esc close",
    "l logs",
    "q quit",
];

/// The keys the footer names over an entry's details.
const HISTORY_DETAIL_KEYS: &[&str] = &["Esc back", "h close", "l logs", "q quit"];

/// `keys` with `h history` before `l logs` when `history`.
fn with_history(keys: &[&'static str], history: bool) -> Vec<&'static str> {
    let mut all = Vec::with_capacity(keys.len() + 1);
    for &key in keys {
        if history && key == "l logs" {
            all.push("h history");
        }
        all.push(key);
    }
    all
}

/// The keys the footer names on `app`'s screen.
fn keys(app: &App) -> &'static [&'static str] {
    match app.history_mode() {
        HistoryMode::Closed => {}
        HistoryMode::List => return HISTORY_LIST_KEYS,
        HistoryMode::Detail { .. } => return HISTORY_DETAIL_KEYS,
    }
    match app.screen {
        Screen::Menu => &["up/down choose", "Enter select", "Esc quit"],
        Screen::Backup => &[
            "Enter I wrote them down",
            "Esc back to the menu, discarding them",
        ],
        Screen::Confirm => &[
            "Enter check",
            "ctrl+u clear",
            "Esc back to the menu, discarding the phrase",
        ],
        Screen::Import | Screen::Existing => {
            &["Enter continue", "ctrl+u clear", "Esc back to the menu"]
        }
        Screen::Password => &[
            "up/down field",
            "Enter continue",
            "ctrl+u clear",
            "Esc back to the menu",
        ],
        Screen::WatchAddress => &["Enter watch", "ctrl+u clear", "Esc back"],
        Screen::Unlock => &["Enter unlock", "ctrl+u clear", "Tab watch only", "Esc quit"],
        Screen::Waiting | Screen::Excited | Screen::Paused | Screen::Watching => {
            &["l logs", "q quit"]
        }
        Screen::Ready if app.typing_recipient() => &[
            "up/down field",
            "Enter or left/right token",
            "type amount (or max) and to",
            "ctrl+u clear",
            "Enter review",
            "Ctrl-C quit",
        ],
        Screen::Ready if app.form.field == Field::Amount => &[
            "up/down field",
            "Enter or left/right token",
            "type amount (or max) and to",
            "ctrl+u clear",
            "Enter review",
            "l logs",
            "q quit",
        ],
        Screen::Ready => &[
            "up/down field",
            "Enter or left/right token",
            "type amount (or max) and to",
            "Enter review",
            "l logs",
            "q quit",
        ],
        Screen::Review => &["y send", "n back", "l logs", "q quit"],
        Screen::Sending => &["Enter back to the form", "l logs", "q quit"],
    }
}

/// `keys` joined by ` · `, dim, as many to a row as fit in `width` columns, none split
/// across rows.
fn footer_lines(keys: &[&'static str], width: u16) -> Vec<Line<'static>> {
    let dim = Style::default().add_modifier(Modifier::DIM);
    let separator = Span::raw(" · ");
    let mut lines = Vec::new();
    let mut line = Line::default().style(dim);
    for &key in keys {
        let key = Span::raw(key);
        if !line.spans.is_empty() {
            if line.width() + separator.width() + key.width() <= usize::from(width) {
                line.push_span(separator.clone());
            } else {
                lines.push(std::mem::replace(&mut line, Line::default().style(dim)));
            }
        }
        line.push_span(key);
    }
    lines.push(line);
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

    use alloy_primitives::{Address, address};
    use chibi_core::history::At;
    use chibi_node::{Network, Startup, Token};
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::buffer::{Buffer, Cell};

    use super::*;
    use crate::tui::app::{
        Balances, EXCITED_FOR, EnsName, HistoryLines, HistoryPane, Picker, Review, Sending,
    };
    use crate::tui::transfer::Draft;

    const SENDER: Address = address!("0x17c5185167401ed00cf5f5b2fc97d9bbfdb7d025");
    const TKN: Token = Token {
        symbol: "TKN",
        name: "Test token",
        address: address!("0x00000000000000000000000000000000000070c3"),
        decimals: 6,
    };
    const REGISTRY: &[Token] = &[TKN];
    const WEIHAN: Address = address!("0x3499ddf9E66b53Ca484554d5309Bf32521AAE5a7");

    fn screen(app: &App, logs: &[String]) -> String {
        sized(app, logs, 120, 30)
    }

    fn sized(app: &App, logs: &[String], width: u16, height: u16) -> String {
        let buffer = drawn(app, logs, width, height);
        let width = usize::from(buffer.area().width);
        buffer
            .content()
            .chunks(width)
            .map(|row| row.iter().map(|cell| cell.symbol()).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn drawn(app: &App, logs: &[String], width: u16, height: u16) -> Buffer {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal
            .draw(|frame| draw(frame, View::new(app, logs.to_vec(), Instant::now())))
            .unwrap();
        terminal.backend().buffer().clone()
    }

    /// The text inside the card on a `width` x `height` terminal, one row per line.
    fn inside(app: &App, logs: &[String], width: u16, height: u16) -> String {
        let pane = if app.logs { LOG_PANE_LINES + 2 } else { 0 };
        let card = card(Rect::new(0, 0, width, height), pane).unwrap();
        let buffer = drawn(app, logs, width, height);
        (card.y + 1..card.bottom() - 1)
            .map(|y| {
                (card.x + 2..card.right() - 2)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Asserts that `cat`'s cells are drawn with their top left corner at (`x`, `y`).
    fn assert_cat_at(buffer: &Buffer, cat: &[Line<'_>], x: u16, y: u16) {
        for (row, line) in (y..).zip(cat) {
            for (column, span) in (x..).zip(&line.spans) {
                let cell = &buffer[(column, row)];
                assert_eq!(
                    (
                        cell.symbol(),
                        Some(cell.fg).filter(|&fg| fg != Color::Reset),
                        Some(cell.bg).filter(|&bg| bg != Color::Reset)
                    ),
                    (span.content.as_ref(), span.style.fg, span.style.bg),
                    "({column}, {row})"
                );
            }
        }
    }

    /// Row `y` of `buffer` from column `x` on.
    fn row_from(buffer: &Buffer, x: u16, y: u16) -> String {
        (x..buffer.area().width)
            .map(|x| buffer[(x, y)].symbol())
            .collect()
    }

    fn half_block(symbol: &str) -> bool {
        matches!(symbol, "▀" | "▄")
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
        assert!(!shown.contains('▀') && !shown.contains('▄'), "{shown}");
        let buffer = drawn(&waiting(), &[], 120, 40);
        assert!(row_from(&buffer, 53, 16).starts_with("  /\\_/\\ "));
        assert!(row_from(&buffer, 53, 17).starts_with(" ( -.- )  z"));
        assert_eq!(buffer[(52, 17)].symbol(), " ");
    }

    #[test]
    fn waiting_with_pixel_cats_draws_the_dozing_cat_in_half_blocks() {
        const OUTLINE: Color = Color::Rgb(59, 42, 32);
        let mut app = waiting();
        app.pixel_cats = true;
        app.width = 100;
        let buffer = drawn(&app, &[], 100, 30);
        let cell = |x: u16, y: u16| {
            let cell = &buffer[(x, y)];
            (cell.symbol(), cell.fg, cell.bg)
        };
        assert_eq!(cell(39, 10), (" ", Color::Reset, Color::Reset));
        assert_eq!(cell(42, 10), ("▄", OUTLINE, Color::Reset));
        assert_eq!(cell(41, 11), ("▀", OUTLINE, OUTLINE));
        assert_eq!(
            cell(42, 11),
            ("▀", Color::Rgb(242, 139, 155), Color::Rgb(242, 139, 155))
        );
        let shown = screen(&app, &[]);
        assert!(
            shown.contains("looking for a state peer · peers 12 · state peers 0 · 1m20s"),
            "{shown}"
        );
        assert!(!shown.contains("( -.- )"), "{shown}");
    }

    #[test]
    fn the_card_is_centered_capped_and_gives_way_on_small_terminals() {
        let card = |width, height, pane| card(Rect::new(0, 0, width, height), pane);
        assert_eq!(card(200, 60, 0), Some(Rect::new(40, 12, 120, 36)));
        assert_eq!(card(80, 24, 0), Some(Rect::new(2, 1, 76, 22)));
        assert_eq!(
            card(120, 40, LOG_PANE_LINES + 2),
            Some(Rect::new(2, 1, 116, 38))
        );
        assert_eq!(card(43, 30, 0), None);
        assert_eq!(card(80, 13, 0), None);
    }

    #[test]
    fn waiting_sits_in_a_rounded_card_with_the_cat_twice_as_big_and_centered() {
        let mut app = waiting();
        app.pixel_cats = true;
        app.width = 120;
        let buffer = drawn(&app, &[], 120, 40);
        let card = Rect::new(2, 2, 116, 36);
        for (at, corner) in [
            ((2, 2), "╭"),
            ((117, 2), "╮"),
            ((2, 37), "╰"),
            ((117, 37), "╯"),
        ] {
            assert_eq!(buffer[at].symbol(), corner, "{at:?}");
        }
        let header = header_line(&app).to_string();
        assert!(row_from(&buffer, 3, 2).starts_with(&header), "{header}");
        for y in 0..40 {
            for x in 0..120 {
                if !card.contains((x, y).into()) {
                    assert_eq!(buffer[(x, y)], Cell::default(), "({x}, {y})");
                }
            }
        }
        let cat = cat::lines(&cat::frame(Pose::Dozing, 0), 2);
        assert_eq!(cat[0].width(), 44);
        assert_cat_at(&buffer, &cat, 38, 10);
        let status = "looking for a state peer · peers 12 · state peers 0 · 1m20s";
        assert_eq!(status.chars().count(), 59);
        assert!(row_from(&buffer, 31, 27).starts_with(status));
        assert_eq!(buffer[(30, 27)].symbol(), " ");
    }

    #[test]
    fn the_cat_is_its_own_size_on_an_80_by_24_terminal() {
        let mut app = waiting();
        app.pixel_cats = true;
        app.width = 80;
        let buffer = drawn(&app, &[], 80, 24);
        assert_eq!(buffer[(2, 1)].symbol(), "╭");
        assert_eq!(buffer[(77, 22)].symbol(), "╯");
        assert_cat_at(&buffer, &cat::lines(&cat::frame(Pose::Dozing, 0), 1), 29, 6);
        let status = "looking for a state peer · peers 12 · state peers 0 · 1m20s";
        assert!(row_from(&buffer, 11, 15).starts_with(status));
        assert_eq!(buffer[(10, 15)].symbol(), " ");
    }

    #[test]
    fn the_ascii_cats_fit_the_columns_they_are_centered_in() {
        let widest = DOZING
            .iter()
            .chain(&EXCITED)
            .chain(&SLEEPY)
            .chain([&HAPPY, &SAD])
            .flatten()
            .map(|line| line.chars().count())
            .max();
        assert_eq!(widest, Some(usize::from(ASCII_CAT_COLUMNS)));
    }

    #[test]
    fn a_small_terminal_keeps_the_borderless_layout() {
        let app = waiting();
        let shown = sized(&app, &[], 40, 12);
        assert!(shown.starts_with("chibi · "), "{shown}");
        assert!(!shown.contains('╭') && !shown.contains('│'), "{shown}");
        assert!(
            shown.lines().last().unwrap().starts_with("l logs · q quit"),
            "{shown}"
        );
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
            name: None,
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
    fn the_header_names_the_primary_name_before_the_address() {
        let mut app = ready();
        assert_eq!(
            header_line(&app).to_string(),
            "chibi · sepolia · 0x17c5185167401eD00cF5F5b2fc97D9BBfDb7D025"
        );
        app.balances(Balances {
            name: Some("weihan.eth".to_owned()),
            ..app.balances.clone()
        });
        assert_eq!(
            header_line(&app).to_string(),
            "chibi · sepolia · weihan.eth · 0x17c5185167401eD00cF5F5b2fc97D9BBfDb7D025"
        );
    }

    #[test]
    fn the_to_row_shows_a_name_resolving_then_its_address_or_why_not() {
        let mut app = ready();
        app.form.field = Field::To;
        app.form.to = "weihan.eth".to_owned();
        let weihan = EnsName::parse("weihan.eth").unwrap();
        let (asked, id) = app.resolve_request().unwrap();
        assert_eq!(asked, weihan);
        let rows = inside_rows(&app, 120, 40);
        let to = rows
            .iter()
            .position(|row| row == "> to      weihan.eth")
            .unwrap();
        assert_eq!(rows[to + 1], "          resolving...");

        app.resolved(id, Ok(WEIHAN));
        let rows = inside_rows(&app, 120, 40);
        assert_eq!(
            rows[to + 1],
            "          → 0x3499ddf9E66b53Ca484554d5309Bf32521AAE5a7"
        );

        let offchain = "offchain.eth is served offchain, which chibi does not follow";
        app.form.to = "offchain.eth".to_owned();
        let (_, id) = app.resolve_request().unwrap();
        app.resolved(id, Err(offchain.to_owned()));
        let buffer = drawn(&app, &[], 120, 40);
        let rows = inside_rows(&app, 120, 40);
        assert_eq!(rows[to + 1], format!("          {offchain}"));
        let card = card(Rect::new(0, 0, 120, 40), 0).unwrap();
        let row = card.y + 1 + u16::try_from(to + 1).unwrap();
        assert_eq!(buffer[(card.x + 12, row)].fg, Color::Red);
    }

    #[test]
    fn on_the_to_field_the_footer_clears_with_ctrl_u_quits_with_ctrl_c_and_drops_the_log_key() {
        let mut app = ready();
        app.form.field = Field::To;
        let rows = inside_rows(&app, 120, 40);
        assert_eq!(
            rows[rows.len() - 2..],
            [
                "up/down field · Enter or left/right token · type amount (or max) and to · ctrl+u clear · Enter review",
                "Ctrl-C quit",
            ]
        );
    }

    #[test]
    fn text_fields_and_screens_name_ctrl_u_in_the_footer() {
        let clears = |app: &App| keys(app).contains(&"ctrl+u clear");
        let mut app = ready();
        for (field, expected) in [
            (Field::Token, false),
            (Field::Amount, true),
            (Field::To, true),
        ] {
            app.form.field = field;
            assert_eq!(clears(&app), expected, "{field:?}");
        }
        for (screen, expected) in [
            (Screen::Unlock, true),
            (Screen::Menu, false),
            (Screen::Backup, false),
            (Screen::Confirm, true),
            (Screen::Import, true),
            (Screen::Existing, true),
            (Screen::Password, true),
            (Screen::WatchAddress, true),
            (Screen::Waiting, false),
            (Screen::Watching, false),
            (Screen::Review, false),
            (Screen::Sending, false),
        ] {
            app.screen = screen;
            assert_eq!(clears(&app), expected, "{screen:?}");
        }
    }

    #[test]
    fn review_shows_the_name_and_the_address_it_resolved_to() {
        let mut app = ready();
        app.review = Some(Review {
            draft: Draft {
                asset: Asset::Eth,
                to: WEIHAN,
                amount: U256::from(1_000_000_000_000_000u64),
            },
            name: Some(EnsName::parse("weihan.eth").unwrap()),
            fee: Fee {
                gas_limit: 21_000,
                max_fee_per_gas: 2_750_000_000,
                max_priority_fee_per_gas: 1_000_000_000,
            },
            refused: None,
        });
        app.screen = Screen::Review;
        let rows = inside_rows(&app, 120, 40);
        let to = rows
            .iter()
            .position(|row| row.trim_start() == "to       weihan.eth")
            .unwrap();
        assert_eq!(
            rows[to + 1].trim_start(),
            "0x3499ddf9E66b53Ca484554d5309Bf32521AAE5a7"
        );
        assert_eq!(rows[to + 1].find("0x"), rows[to].find("weihan"));
    }

    #[test]
    fn the_idle_cat_sits_right_of_the_form_only_on_wide_terminals() {
        let mut app = ready();
        app.width = 120;
        let plain = drawn(&app, &[], 120, 30);
        app.pixel_cats = true;
        let with_cat = drawn(&app, &[], 120, 30);
        let form_right = (2..14)
            .flat_map(|y| (4..116).map(move |x| (x, y)))
            .filter(|&at| plain[at].symbol() != " ")
            .map(|(x, _)| x)
            .max()
            .unwrap();
        let mut cat_cells = 0;
        for y in 0..30 {
            for x in 0..120 {
                if plain[(x, y)].symbol() != " " {
                    assert_eq!(plain[(x, y)], with_cat[(x, y)], "({x}, {y})");
                }
                if half_block(with_cat[(x, y)].symbol()) {
                    assert!(x > form_right, "({x}, {y}) right of {form_right}");
                    cat_cells += 1;
                }
            }
        }
        assert!(cat_cells > 50, "{cat_cells}");
        let cat_columns = |buffer: &Buffer| {
            let columns: Vec<u16> = buffer
                .content()
                .iter()
                .enumerate()
                .filter(|(_, cell)| half_block(cell.symbol()))
                .map(|(at, _)| buffer.pos_of(at).0)
                .collect();
            columns.iter().max().unwrap() - columns.iter().min().unwrap() + 1
        };
        assert_eq!(cat_columns(&with_cat), 16);
        app.pinned(6, 6, true);
        assert_eq!(cat_columns(&drawn(&app, &[], 120, 40)), 32);

        app.pixel_cats = false;
        app.width = 60;
        let narrow = drawn(&app, &[], 60, 30);
        app.pixel_cats = true;
        assert_eq!(drawn(&app, &[], 60, 30), narrow);
        app.width = 100;
        assert_eq!(drawn(&app, &[], 60, 30), narrow);
    }

    #[test]
    fn the_token_row_says_more_tokens_appear_until_every_pinned_key_is_held() {
        const HINT: &str = "more tokens appear once their balances are proven";
        let mut app = ready();
        app.pinned(4, 6, true);
        assert!(screen(&app, &[]).contains(HINT));
        let mut terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();
        terminal
            .draw(|frame| draw(frame, View::new(&app, Vec::new(), Instant::now())))
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
        assert!(!buffer[(6, row)].modifier.contains(Modifier::DIM));

        app.pinned(6, 6, true);
        assert!(!screen(&app, &[]).contains(HINT));
    }

    /// The form on the token row holding 3 ETH, 25 USDC and 0.5 WETH, sending WETH.
    fn on_the_token_row() -> App {
        const USDC: Token = Token {
            symbol: "USDC",
            name: "USD Coin (Circle)",
            address: address!("0x1c7D4B196Cb0C7B01d743Fbc6116a902379C7238"),
            decimals: 6,
        };
        const WETH: Token = Token {
            symbol: "WETH",
            name: "Wrapped Ether",
            address: address!("0xfFf9976782d46CC05630D1f6eBAb18b2324d6B14"),
            decimals: 18,
        };
        let mut app = ready();
        app.balances(Balances {
            eth: Some(U256::from(3_000_000_000_000_000_000u64)),
            tokens: vec![
                (USDC, U256::from(25_000_000)),
                (WETH, U256::from(500_000_000_000_000_000u64)),
            ],
            name: None,
        });
        app.form.asset = Asset::Token(WETH);
        app.form.field = Field::Token;
        app
    }

    #[test]
    fn the_token_list_opens_below_the_token_row_and_leaves_the_form_in_place() {
        let mut app = on_the_token_row();
        let closed = drawn(&app, &[], 120, 40);
        let shown = sized(&app, &[], 120, 40);
        assert!(
            shown.contains("> token   < WETH >   0.50 WETH   Enter to choose   more tokens"),
            "{shown}"
        );
        assert!(
            row_from(&closed, 4, 4).starts_with("  from    0x17c5"),
            "{shown}"
        );
        let row = row_from(&closed, 4, 5);
        let hint_x = 4 + u16::try_from(row.find("Enter").unwrap()).unwrap();
        assert!(closed[(hint_x, 5)].modifier.contains(Modifier::DIM));
        assert!(!closed[(hint_x - 4, 5)].modifier.contains(Modifier::DIM));
        assert!(!shown.contains("choose a token"), "{shown}");

        app.form.picker = Some(Picker {
            filter: String::new(),
            highlighted: app.assets()[2],
        });
        let open = drawn(&app, &[], 120, 40);
        let shown = sized(&app, &[], 120, 40);
        for expected in [
            "┌choose a token",
            "│  ETH   Ether              3.00 ETH",
            "│  USDC  USD Coin (Circle)  25.00 USDC",
            "│> WETH  Wrapped Ether      0.50 WETH",
            "│more tokens appear once their balances are proven",
            "└type to filter · Enter pick · Esc cancel",
        ] {
            assert!(shown.contains(expected), "{expected}\n{shown}");
        }
        let corner = |symbol: &str| {
            (0..40)
                .flat_map(|y| (0..120).map(move |x| (x, y)))
                .find(|&at| open[at].symbol() == symbol)
                .unwrap()
        };
        let (left, top) = corner("┌");
        let (right, bottom) = corner("┘");
        assert_eq!((left, top), (6, 6), "{shown}");
        for y in 0..40 {
            for x in 0..120 {
                if !(left..=right).contains(&x) || !(top..=bottom).contains(&y) {
                    assert_eq!(open[(x, y)], closed[(x, y)], "({x}, {y})\n{shown}");
                }
            }
        }

        app.form.picker = Some(Picker {
            filter: "dai".to_owned(),
            highlighted: Asset::Eth,
        });
        let shown = sized(&app, &[], 120, 40);
        assert!(shown.contains("│no token matches"), "{shown}");
        assert!(shown.contains("│filter  dai"), "{shown}");
    }

    #[test]
    fn the_token_list_fits_a_tiny_terminal() {
        let mut app = on_the_token_row();
        app.form.picker = Some(Picker {
            filter: String::new(),
            highlighted: Asset::Eth,
        });
        for (width, height) in [(20, 8), (3, 2), (1, 1), (120, 5)] {
            sized(&app, &[], width, height);
        }
    }

    /// [`ready`] holding 4.019083742425574983 ETH, on the token row.
    fn rich() -> App {
        let mut app = ready();
        app.balances(Balances {
            eth: Some(U256::from(4_019_083_742_425_574_983u64)),
            tokens: vec![(TKN, U256::from(1_500_000))],
            name: None,
        });
        app.form.field = Field::Token;
        app
    }

    /// The rows inside the card on a `width` x `height` terminal, trailing blanks dropped.
    fn inside_rows(app: &App, width: u16, height: u16) -> Vec<String> {
        inside(app, &[], width, height)
            .lines()
            .map(|row| row.trim_end().to_owned())
            .collect()
    }

    #[test]
    fn a_form_row_too_wide_for_the_card_continues_under_its_values() {
        let app = rich();
        let rows = inside_rows(&app, 80, 24);
        assert_eq!(
            rows[1..7],
            [
                "  from    0x17c5185167401eD00cF5F5b2fc97D9BBfDb7D025",
                "          4.019083742425574983 ETH",
                "> token   < ETH >   4.019083742425574983 ETH   Enter to choose",
                "          more tokens appear once their balances are proven",
                "  amount",
                "  to",
            ],
            "{rows:#?}"
        );
        let buffer = drawn(&app, &[], 80, 24);
        assert_eq!(buffer[(14, 6)].symbol(), "m");
        assert!(buffer[(14, 6)].modifier.contains(Modifier::DIM));
    }

    #[test]
    fn a_wide_card_keeps_each_form_row_on_one_line() {
        let mut app = rich();
        let rows = inside_rows(&app, 200, 60);
        assert_eq!(
            rows[1..4],
            [
                "  from    0x17c5185167401eD00cF5F5b2fc97D9BBfDb7D025   4.019083742425574983 ETH",
                "> token   < ETH >   4.019083742425574983 ETH   Enter to choose   more tokens appear once their balances are proven",
                "  amount",
            ],
            "{rows:#?}"
        );
        app.pinned(6, 6, true);
        let rows = inside_rows(&app, 120, 40);
        assert_eq!(
            rows[1..4],
            [
                "  from    0x17c5185167401eD00cF5F5b2fc97D9BBfDb7D025   4.019083742425574983 ETH",
                "> token   < ETH >   4.019083742425574983 ETH   Enter to choose",
                "  amount",
            ],
            "{rows:#?}"
        );
    }

    #[test]
    fn the_footer_packs_its_keys_into_as_few_rows_as_hold_them() {
        let app = ready();
        let last = |rows: Vec<String>, count: usize| rows[rows.len() - count..].to_vec();
        assert_eq!(
            last(inside_rows(&app, 80, 24), 3),
            [
                "",
                "up/down field · Enter or left/right token · type amount (or max) and to",
                "ctrl+u clear · Enter review · l logs · q quit",
            ]
        );
        assert_eq!(
            last(inside_rows(&app, 104, 24), 3),
            [
                "",
                "up/down field · Enter or left/right token · type amount (or max) and to · ctrl+u clear",
                "Enter review · l logs · q quit",
            ]
        );
        assert_eq!(
            last(inside_rows(&app, 120, 40), 3),
            [
                "",
                "up/down field · Enter or left/right token · type amount (or max) and to · ctrl+u clear · Enter review · l logs",
                "q quit",
            ]
        );
        let borderless = sized(&app, &[], 43, 30)
            .lines()
            .map(|row| row.trim_end().to_owned())
            .collect();
        assert_eq!(
            last(borderless, 4),
            [
                "",
                "up/down field · Enter or left/right token",
                "type amount (or max) and to · ctrl+u clear",
                "Enter review · l logs · q quit",
            ]
        );
    }

    #[test]
    fn the_token_list_opens_below_the_token_rows_last_line_on_an_80_by_24_terminal() {
        let mut app = on_the_token_row();
        let closed = drawn(&app, &[], 80, 24);
        let rows = inside_rows(&app, 80, 24);
        assert_eq!(
            rows[1..4],
            [
                "  from    0x17c5185167401eD00cF5F5b2fc97D9BBfDb7D025   3.00 ETH",
                "> token   < WETH >   0.50 WETH   Enter to choose",
                "          more tokens appear once their balances are proven",
            ],
            "{rows:#?}"
        );
        app.form.picker = Some(Picker {
            filter: String::new(),
            highlighted: app.assets()[2],
        });
        let open = drawn(&app, &[], 80, 24);
        let shown = sized(&app, &[], 80, 24);
        let corner = |symbol: &str| {
            (0..24)
                .flat_map(|y| (0..80).map(move |x| (x, y)))
                .find(|&at| open[at].symbol() == symbol)
                .unwrap()
        };
        let (left, top) = corner("┌");
        let (right, bottom) = corner("┘");
        assert_eq!((left, top), (6, 6), "{shown}");
        for y in 0..24 {
            for x in 0..80 {
                if !(left..=right).contains(&x) || !(top..=bottom).contains(&y) {
                    assert_eq!(open[(x, y)], closed[(x, y)], "({x}, {y})\n{shown}");
                }
            }
        }
    }

    #[test]
    fn the_side_cat_doubles_only_where_its_bounds_fit_whole() {
        let bounds = cat::Bounds {
            columns: 3..19,
            rows: 2..16,
        };
        assert_eq!(side_cat_size(&bounds, 40, 20), Some((2, 32, 14)));
        assert_eq!(side_cat_size(&bounds, 32, 14), Some((2, 32, 14)));
        assert_eq!(side_cat_size(&bounds, 40, 13), Some((1, 16, 7)));
        assert_eq!(side_cat_size(&bounds, 31, 20), Some((1, 16, 7)));
        assert_eq!(side_cat_size(&bounds, 16, 3), Some((1, 16, 7)));
        assert_eq!(side_cat_size(&bounds, 15, 20), None);
    }

    #[test]
    fn the_token_list_stays_above_the_notice_and_footer() {
        let mut app = on_the_token_row();
        app.notice = Some("a notice".to_owned());
        let closed = drawn(&app, &[], 120, 10);
        app.form.picker = Some(Picker {
            filter: String::new(),
            highlighted: app.assets()[2],
        });
        let open = drawn(&app, &[], 120, 10);
        let shown = sized(&app, &[], 120, 10);
        assert!(shown.contains("┌choose a token"), "{shown}");
        assert!(shown.contains("│> WETH"), "{shown}");
        for y in [8, 9] {
            for x in 0..120 {
                assert_eq!(open[(x, y)], closed[(x, y)], "({x}, {y})\n{shown}");
            }
        }
    }

    #[test]
    fn a_short_token_list_keeps_the_highlighted_row_first() {
        let popup = Popup {
            below: 0,
            filter: Line::raw("filter"),
            rows: ["  ETH", "  USDC", "> WETH"].map(Line::raw).to_vec(),
            highlighted: 2,
            hint: Some(Line::raw("hint")),
        };
        let text = |room| {
            popup
                .lines(room)
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
        };
        assert_eq!(text(1), ["> WETH"]);
        assert_eq!(text(2), ["  USDC", "> WETH"]);
        assert_eq!(text(3), ["  ETH", "  USDC", "> WETH"]);
        assert_eq!(text(4), ["filter", "↑ 2 more", "> WETH", "hint"]);
        assert_eq!(text(5), ["filter", "  ETH", "  USDC", "> WETH", "hint"]);
    }

    #[test]
    fn a_long_token_list_scrolls_to_keep_the_highlighted_row_visible() {
        const fn token(symbol: &'static str, byte: u8) -> Token {
            let mut address = [0; 20];
            address[19] = byte;
            Token {
                symbol,
                name: "Test token",
                address: Address::new(address),
                decimals: 6,
            }
        }
        let mut app = ready();
        app.balances(Balances {
            eth: Some(U256::from(3_000_000_000_000_000_000u64)),
            tokens: ["TA", "TB", "TC", "TD", "TE", "TF", "TG"]
                .into_iter()
                .zip(1..)
                .map(|(symbol, byte)| (token(symbol, byte), U256::from(1_000_000)))
                .collect(),
            name: None,
        });
        app.form.field = Field::Token;
        let closed = drawn(&app, &[], 120, 13);
        app.form.picker = Some(Picker {
            filter: String::new(),
            highlighted: app.assets()[7],
        });
        let open = drawn(&app, &[], 120, 13);
        let shown = sized(&app, &[], 120, 13);
        for expected in ["│↑ 5 more", "│  TE ", "│  TF ", "│> TG "] {
            assert!(shown.contains(expected), "{expected}\n{shown}");
        }
        assert!(!shown.contains("│  TD "), "{shown}");
        for x in 0..120 {
            assert_eq!(open[(x, 12)], closed[(x, 12)], "({x}, 12)\n{shown}");
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

    const EXPLORER_TX: &str = "https://sepolia.etherscan.io/tx/0xb7f9b94255de07145c869c658b4a1b7f6fcfe3e155c56b031e7a2254c5f286e5";

    fn sent(state: SendState) -> String {
        let mut app = ready();
        app.screen = Screen::Sending;
        app.sending = Some(Sending {
            draft: Draft {
                asset: Asset::Eth,
                to: SENDER,
                amount: U256::from(1_000_000_000u64),
            },
            hash: Some(alloy_primitives::b256!(
                "0xb7f9b94255de07145c869c658b4a1b7f6fcfe3e155c56b031e7a2254c5f286e5"
            )),
            state,
            pending_heads: 0,
        });
        sized(&app, &[], 120, 40)
    }

    #[test]
    fn a_verified_send_links_its_explorer_page_on_one_line() {
        for state in [SendState::Succeeded, SendState::Failed] {
            let shown = sent(state);
            assert!(
                shown.lines().any(|row| row.contains(EXPLORER_TX)),
                "{shown}"
            );
        }
    }

    #[test]
    fn an_unverified_or_unsent_transfer_has_no_explorer_link() {
        for state in [
            SendState::Included,
            SendState::Signing,
            SendState::Rejected("nonce too low".to_owned()),
            SendState::Dropped,
        ] {
            let shown = sent(state);
            assert!(!shown.contains("etherscan"), "{shown}");
        }
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
    fn setup_offers_three_ways_to_get_a_wallet_or_just_watching() {
        let mut app = setting_up();
        let shown = screen(&app, &[]);
        for expected in [
            "no keystore yet: set up the wallet to send from",
            "> create a new wallet",
            "  import a private key or recovery phrase",
            "  use an existing keystore file",
            "  just watch balances, without a wallet",
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
        let shown: String = inside(&app, &[], 120, 30)
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
        let shown = squeezed(&inside(&app, &logs, 100, 18));
        assert!(shown.contains(&squeezed(warning)), "{shown}");
    }

    #[test]
    fn a_long_notice_shows_whole_on_ready_with_the_log_pane() {
        let mut app = ready();
        let notice = "saved your wallet 0x17c5185167401eD00cF5F5b2fc97D9BBfDb7D025 to /home/someone/.config/chibi/keystores/0x17c5185167401eD00cF5F5b2fc97D9BBfDb7D025.json, but could not record it in /home/someone/.config/chibi/config.toml: the file is read-only. start with --keystore /home/someone/.config/chibi/keystores/0x17c5185167401eD00cF5F5b2fc97D9BBfDb7D025.json next time.";
        app.notice = Some(notice.to_owned());
        app.logs = true;
        let logs: Vec<String> = (0..30).map(|i| format!("log line {i}")).collect();
        let shown = squeezed(&inside(&app, &logs, 80, 24));
        assert!(shown.contains(&squeezed(notice)), "{shown}");
    }

    #[test]
    fn a_notice_with_runs_of_spaces_and_wide_characters_shows_whole() {
        let mut app = ready();
        let notice = "could not write  /tmp/日本日本日本日本日本/a.json:  denied";
        app.notice = Some(notice.to_owned());
        let shown = squeezed(&sized(&app, &[], 20, 30));
        assert!(shown.contains(&squeezed(notice)), "{shown}");
    }

    #[test]
    fn a_notice_fits_exactly_between_the_header_and_the_footer() {
        let mut app = ready();
        let notice = "press q again to quit; a transfer is pending";
        app.notice = Some(notice.to_owned());
        let tall = sized(&app, &[], 30, 30);
        let tall: Vec<&str> = tall.lines().collect();
        app.logs = true;
        let logs: Vec<String> = (0..30).map(|i| format!("log line {i}")).collect();
        let shown = sized(&app, &logs, 30, 4);
        let rows: Vec<&str> = shown.lines().collect();
        assert_eq!(rows[0], tall[0], "{shown}");
        assert_eq!(squeezed(&rows[1..3].concat()), squeezed(notice), "{shown}");
        assert_eq!(rows[3], tall[25], "{shown}");
    }

    /// Watching WEIHAN, named and holding 3 ETH and 1.5 TKN, and SENDER holding 0.25 ETH.
    fn watching() -> App {
        let mut app = watching_only(&[WEIHAN, SENDER]);
        app.refreshed(vec![
            (
                WEIHAN,
                Balances {
                    eth: Some(U256::from(3_000_000_000_000_000_000u64)),
                    tokens: vec![(TKN, U256::from(1_500_000))],
                    name: Some("weihan.eth".to_owned()),
                },
            ),
            (
                SENDER,
                Balances {
                    eth: Some(U256::from(250_000_000_000_000_000u64)),
                    tokens: Vec::new(),
                    name: None,
                },
            ),
        ]);
        app
    }

    /// Watching `addresses` with no balances yet, serving with a state peer.
    fn watching_only(addresses: &[Address]) -> App {
        let mut app = App::new(Network::Sepolia, 11_155_111, REGISTRY, false);
        app.watch_list(addresses);
        app.setup(None);
        app.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        let now = Instant::now();
        app.startup(Startup::Serving, now);
        app.tick(now + EXCITED_FOR);
        app.health(100, 1);
        app
    }

    #[test]
    fn watching_lists_each_address_with_its_name_and_balances() {
        const HINT: &str = "more tokens appear once their balances are proven";
        let mut app = watching();
        assert_eq!(app.screen, Screen::Watching);
        assert_eq!(header_line(&app).to_string(), "chibi · sepolia · watching");
        let shown = inside(&app, &[], 120, 40);
        let weihan = shown
            .find("0x3499ddf9E66b53Ca484554d5309Bf32521AAE5a7   weihan.eth   3.00 ETH   1.50 TKN")
            .expect(&shown);
        let sender = shown
            .find("0x17c5185167401eD00cF5F5b2fc97D9BBfDb7D025   0.25 ETH")
            .expect(&shown);
        assert!(weihan < sender, "{shown}");
        assert!(shown.contains(HINT), "{shown}");
        assert!(shown.contains("l logs · q quit"), "{shown}");
        for absent in ["from", "amount", "fee", "Enter review"] {
            assert!(!shown.contains(absent), "{absent}\n{shown}");
        }
        app.pinned(6, 6, true);
        assert!(!inside(&app, &[], 120, 40).contains(HINT));
    }

    #[test]
    fn watching_clips_on_tiny_terminals_without_panicking() {
        let mut app = watching();
        app.pixel_cats = true;
        app.width = 120;
        let logs: Vec<String> = (0..30).map(|i| format!("log line {i}")).collect();
        for logs_open in [false, true] {
            app.logs = logs_open;
            for (width, height) in [(1, 1), (10, 3), (30, 5), (43, 13), (44, 14), (70, 16)] {
                let shown = sized(&app, &logs, width, height);
                assert_eq!(shown.lines().count(), usize::from(height));
            }
        }
    }

    #[test]
    fn watching_scrolls_a_list_taller_than_the_body_by_address() {
        let addresses: Vec<Address> = (1..=12).map(Address::with_last_byte).collect();
        let rows = |at: std::ops::Range<usize>| {
            addresses[at]
                .iter()
                .map(|address| format!("  {}   ? ETH", address.to_checksum(None)))
                .collect::<Vec<_>>()
        };
        let shown = |app: &App| {
            inside(app, &[], 120, 16)
                .lines()
                .map(|line| line.trim_end().to_owned())
                .collect::<Vec<_>>()
        };
        let hint = format!("  {TOKENS_UNPROVEN}");
        let hint = hint.as_str();
        let footer = "up/down scroll · l logs · q quit";
        let mut app = watching_only(&addresses);
        let mut expected = vec![String::new()];
        expected.extend(rows(0..7));
        expected.extend(["  ↓ 5 more", "", hint, footer].map(str::to_owned));
        assert_eq!(shown(&app), expected);

        for _ in 0..3 {
            app.key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        }
        let mut expected = vec!["  ↑ 3 more".to_owned()];
        expected.extend(rows(3..10));
        expected.extend(["  ↓ 2 more", "", hint, footer].map(str::to_owned));
        assert_eq!(shown(&app), expected);

        for _ in 0..20 {
            app.key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        }
        let mut expected = vec!["  ↑ 11 more".to_owned()];
        expected.extend(rows(11..12));
        expected.extend(["", hint].map(str::to_owned));
        expected.extend(vec![String::new(); 7]);
        expected.push(footer.to_owned());
        assert_eq!(shown(&app), expected);

        app.key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE));
        assert_eq!(shown(&app)[0], "  ↑ 10 more");
        for (width, height) in [(1, 1), (10, 3), (30, 5), (43, 13), (44, 14)] {
            assert_eq!(
                sized(&app, &[], width, height).lines().count(),
                usize::from(height)
            );
        }
    }

    #[test]
    fn a_watched_address_taller_than_the_body_shows_its_first_rows() {
        let address = |name: &'static str| Row::Form {
            label: Span::raw(name),
            values: (0..9).map(|i| Span::raw(format!("{i} TOKEN"))).collect(),
        };
        let hint = || vec![Row::Text(Line::raw("hint"))];
        let text = |list: WatchList, height: u16| {
            list.rows(height)
                .into_iter()
                .take(usize::from(height))
                .map(|row| match row {
                    Row::Text(line) => line.to_string().trim_end().to_owned(),
                    Row::Form { .. } | Row::Hanging(..) => unreachable!(),
                })
                .collect::<Vec<_>>()
        };
        let one = WatchList::new(vec![address("0xA")], 0, hint(), 10);
        assert_eq!(text(one, 3), ["0xA0 TOKEN", "   1 TOKEN", "   2 TOKEN"]);
        let two = || WatchList::new(vec![address("0xA"), address("0xB")], 1, hint(), 10);
        assert_eq!(text(two(), 3), ["0xB0 TOKEN", "   1 TOKEN", "   2 TOKEN"]);
        let mut expected = vec!["  ↑ 1 more".to_owned(), "0xB0 TOKEN".to_owned()];
        expected.extend((1..9).map(|i| format!("   {i} TOKEN")));
        assert_eq!(text(two(), 10), expected);
        assert!(text(two(), 0).is_empty());
    }

    #[test]
    fn an_oversized_watched_address_shows_its_address_with_the_log_pane_open() {
        let mut app = watching_only(&[WEIHAN]);
        app.refreshed(vec![(
            WEIHAN,
            Balances {
                eth: Some(U256::from(1)),
                tokens: vec![(TKN, U256::from(1_500_000)); 200],
                name: None,
            },
        )]);
        app.logs = true;
        let shown = inside(&app, &[], 80, 24);
        assert!(shown.contains(&WEIHAN.to_checksum(None)), "{shown}");
    }

    #[test]
    fn a_watch_list_that_fits_shows_no_scroll_marks_or_key() {
        let twelve: Vec<Address> = (1..=12).map(Address::with_last_byte).collect();
        for app in [watching(), watching_only(&twelve)] {
            let shown = inside(&app, &[], 120, 24);
            for address in &app.watched {
                assert!(shown.contains(&address.0.to_checksum(None)), "{shown}");
            }
            for absent in ["↑", "↓", "up/down scroll"] {
                assert!(!shown.contains(absent), "{absent}\n{shown}");
            }
            assert!(shown.contains("l logs · q quit"), "{shown}");
        }
    }

    #[test]
    fn the_address_prompt_and_unlock_name_their_keys() {
        let mut app = App::new(Network::Sepolia, 11_155_111, REGISTRY, false);
        assert!(
            screen(&app, &[]).contains("Enter unlock · ctrl+u clear · Tab watch only · Esc quit"),
            "{}",
            screen(&app, &[])
        );
        app.key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        for c in "0x12".chars() {
            app.key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE));
        }
        app.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        let shown = screen(&app, &[]);
        for expected in [
            "the address to watch",
            "address  0x12",
            "expected 0x followed by 40 hex digits",
            "Enter watch · ctrl+u clear · Esc back",
        ] {
            assert!(shown.contains(expected), "{expected}\n{shown}");
        }
    }

    /// Presses `code` the way the control loop does: storing where the details drawn end
    /// first.
    fn press(app: &mut App, code: KeyCode) {
        app.set_detail_end(detail_end(app));
        app.key(KeyEvent::new(code, KeyModifiers::NONE));
    }

    fn with_history(mut app: App) -> App {
        app.history = Some(HistoryPane::default());
        app
    }

    const COVERAGE: &str = "history since block 11,856,337 · gaps: none · no sends found";

    /// An entry of block `number` with `cells` in column order.
    fn entry(number: u64, cells: [&str; 7], warning: bool) -> activity::Entry {
        let [tx, block, age, from, to, amount, fee] = cells.map(str::to_owned);
        activity::Entry {
            key: activity::EntryKey {
                number,
                at: At::Tx(0),
                nth: 0,
            },
            cells: Cells {
                tx,
                block,
                age,
                from,
                to,
                amount,
                fee,
            },
            warning,
            detail: Vec::new(),
        }
    }

    /// Five entries, newest first, of the kinds a real account shows; the first has details.
    fn five_entries() -> Vec<activity::Entry> {
        let mut entries = vec![
            entry(
                5,
                [
                    "0x671e…4543",
                    "11,969,090",
                    "7m",
                    "you",
                    "0xd72A…8146",
                    "-1.00 USDC",
                    "<0.000001",
                ],
                false,
            ),
            entry(
                4,
                [
                    "0x9a3c…01fe",
                    "11,966,550",
                    "2h",
                    "you",
                    "you",
                    "0.000000001 ETH",
                    "0.000016",
                ],
                false,
            ),
            entry(
                3,
                [
                    "withdrawal",
                    "11,962,690",
                    "13h",
                    "validator 42",
                    "you",
                    "+0.50 ETH",
                    "",
                ],
                false,
            ),
            entry(
                2,
                [
                    "0x5b20…77aa",
                    "11,962,527",
                    "13h",
                    "0x1111…2222",
                    "you",
                    "⚠ 0.00 USDC",
                    "",
                ],
                true,
            ),
            entry(
                1,
                [
                    "0x0c4d…9e31",
                    "11,856,400",
                    "2d",
                    "you",
                    "0xAbFC…D06f",
                    "-0.000000000001669629 ETH",
                    "0.000512",
                ],
                false,
            ),
        ];
        let field = |label, value: &str| DetailLine::Field {
            label,
            value: value.to_owned(),
        };
        entries[0].detail = vec![
            field("transaction", HASH),
            field("block", "11,969,090 · 7 min ago"),
            field("status", "success"),
            field("from", "0x17c5185167401eD00cF5F5b2fc97D9BBfDb7D025 (you)"),
            field("to", "0x1c7D4B196Cb0C7B01d743Fbc6116a902379C7238"),
            field("nonce", "46"),
            field("fee", "0.000000000001669629 ETH"),
            DetailLine::Blank,
            DetailLine::Text("-1.00 USDC to 0xd72A04f52788544702EA111E54B4B226Fd328146".to_owned()),
            DetailLine::Blank,
            DetailLine::Text(format!("https://sepolia.etherscan.io/tx/{HASH}")),
        ];
        entries
    }

    const HASH: &str = "0x671eb606000de4305cab87a0f6b334fef8ac4be6e994536e05586d7752e24543";

    /// [`five_entries`] as a table in a body 112 columns wide, nothing selected.
    const AT_112: [&str; 6] = [
        "  Txn Hash     Block       Age  From          To           Amount                    Fee",
        "  0x671e…4543  11,969,090  7m   you           0xd72A…8146  -1.00 USDC                <0.000001",
        "  0x9a3c…01fe  11,966,550  2h   you           you          0.000000001 ETH           0.000016",
        "  withdrawal   11,962,690  13h  validator 42  you          +0.50 ETH",
        "  0x5b20…77aa  11,962,527  13h  0x1111…2222   you          ⚠ 0.00 USDC",
        "  0x0c4d…9e31  11,856,400  2d   you           0xAbFC…D06f  -0.000000000001669629 E…  0.000512",
    ];

    /// [`five_entries`] in a body 88 columns wide: Block dropped, Fee kept.
    const AT_88: [&str; 6] = [
        "  Txn Hash     Age  From          To           Amount                    Fee",
        "  0x671e…4543  7m   you           0xd72A…8146  -1.00 USDC                <0.000001",
        "  0x9a3c…01fe  2h   you           you          0.000000001 ETH           0.000016",
        "  withdrawal   13h  validator 42  you          +0.50 ETH",
        "  0x5b20…77aa  13h  0x1111…2222   you          ⚠ 0.00 USDC",
        "  0x0c4d…9e31  2d   you           0xAbFC…D06f  -0.000000000001669629 E…  0.000512",
    ];

    /// [`five_entries`] in a body 72 columns wide: Block and Fee dropped.
    const AT_72: [&str; 6] = [
        "  Txn Hash     Age  From          To           Amount",
        "  0x671e…4543  7m   you           0xd72A…8146  -1.00 USDC",
        "  0x9a3c…01fe  2h   you           you          0.000000001 ETH",
        "  withdrawal   13h  validator 42  you          +0.50 ETH",
        "  0x5b20…77aa  13h  0x1111…2222   you          ⚠ 0.00 USDC",
        "  0x0c4d…9e31  2d   you           0xAbFC…D06f  -0.000000000001669629 E…",
    ];

    /// [`five_entries`] in a body 60 columns wide: From dropped too, Txn Hash kept.
    const AT_60: [&str; 6] = [
        "  Txn Hash     Age  To           Amount",
        "  0x671e…4543  7m   0xd72A…8146  -1.00 USDC",
        "  0x9a3c…01fe  2h   you          0.000000001 ETH",
        "  withdrawal   13h  you          +0.50 ETH",
        "  0x5b20…77aa  13h  you          ⚠ 0.00 USDC",
        "  0x0c4d…9e31  2d   0xAbFC…D06f  -0.000000000001669629 E…",
    ];

    /// [`five_entries`] in a body 42 columns wide: From and Txn Hash dropped too, Amount cut.
    const AT_42: [&str; 6] = [
        "  Age  To           Amount",
        "  7m   0xd72A…8146  -1.00 USDC",
        "  2h   you          0.000000001 ETH",
        "  13h  you          +0.50 ETH",
        "  13h  you          ⚠ 0.00 USDC",
        "  2d   0xAbFC…D06f  -0.000000000001669629…",
    ];

    /// `table` with row `at` (0 for the first under the heading) marked selected.
    fn marked(table: &[&str], at: usize) -> Vec<String> {
        table
            .iter()
            .enumerate()
            .map(|(n, row)| {
                if n == at + 1 {
                    format!("> {}", &row[2..])
                } else {
                    (*row).to_owned()
                }
            })
            .collect()
    }

    /// `app` with history on and `entries` read for `account`.
    fn read(app: App, account: Address, entries: Vec<activity::Entry>) -> App {
        let mut app = with_history(app);
        assert_eq!(app.history_request(false), Some(account));
        app.history_read(
            account,
            Some(HistoryLines {
                coverage: COVERAGE.to_owned(),
                entries,
            }),
        );
        app
    }

    #[test]
    fn the_open_history_pane_shows_the_coverage_then_one_row_per_entry_with_one_selected() {
        let mut app = with_history(ready());
        press(&mut app, KeyCode::Char('h'));
        let account = "  0x17c5185167401eD00cF5F5b2fc97D9BBfDb7D025";
        let rows = inside_rows(&app, 120, 30);
        assert_eq!(rows[..3], ["", account, "  reading history..."]);

        assert_eq!(app.history_request(false), Some(SENDER));
        app.history_read(
            SENDER,
            Some(HistoryLines {
                coverage: COVERAGE.to_owned(),
                entries: five_entries(),
            }),
        );
        let rows = inside_rows(&app, 120, 30);
        assert_eq!(rows[..4], ["", account, &format!("  {COVERAGE}"), ""]);
        assert_eq!(rows[4..10], marked(&AT_112, 0), "{rows:#?}");
        assert_eq!(
            rows.last().unwrap(),
            "up/down choose · Enter details · h/Esc close · l logs · q quit"
        );

        press(&mut app, KeyCode::Down);
        let rows = inside_rows(&app, 120, 30);
        assert_eq!(rows[4..10], marked(&AT_112, 1), "{rows:#?}");
        press(&mut app, KeyCode::Esc);
        assert!(inside(&app, &[], 120, 30).contains("> amount"));
    }

    #[test]
    fn narrow_bodies_drop_block_fee_from_then_txn_hash_and_cut_the_amount() {
        let mut app = read(ready(), SENDER, five_entries());
        press(&mut app, KeyCode::Char('h'));
        for (width, table) in [
            (120, AT_112),
            (96, AT_88),
            (80, AT_72),
            (68, AT_60),
            (50, AT_42),
        ] {
            let rows = inside_rows(&app, width, 24);
            let heading = rows
                .iter()
                .position(|row| row == table[0])
                .unwrap_or_else(|| panic!("{width}: {rows:#?}"));
            assert_eq!(
                rows[heading..heading + 6],
                marked(&table, 0),
                "{width}: {rows:#?}"
            );
        }
    }

    #[test]
    fn enter_shows_the_selected_entrys_details_in_full_and_esc_returns_to_the_same_row() {
        let mut app = read(ready(), SENDER, five_entries());
        press(&mut app, KeyCode::Char('h'));
        press(&mut app, KeyCode::Enter);
        let rows = inside_rows(&app, 120, 30);
        assert_eq!(
            rows[..12],
            [
                "",
                "  transaction  0x671eb606000de4305cab87a0f6b334fef8ac4be6e994536e05586d7752e24543",
                "  block        11,969,090 · 7 min ago",
                "  status       success",
                "  from         0x17c5185167401eD00cF5F5b2fc97D9BBfDb7D025 (you)",
                "  to           0x1c7D4B196Cb0C7B01d743Fbc6116a902379C7238",
                "  nonce        46",
                "  fee          0.000000000001669629 ETH",
                "",
                "  -1.00 USDC to 0xd72A04f52788544702EA111E54B4B226Fd328146",
                "",
                "  https://sepolia.etherscan.io/tx/0x671eb606000de4305cab87a0f6b334fef8ac4be6e994536e05586d7752e24543",
            ],
            "{rows:#?}"
        );
        assert_eq!(rows.last().unwrap(), "Esc back · h close · l logs · q quit");

        press(&mut app, KeyCode::Down);
        assert_eq!(inside_rows(&app, 120, 30), rows);
        press(&mut app, KeyCode::Esc);
        assert_eq!(inside_rows(&app, 120, 30)[4..10], marked(&AT_112, 0));

        press(&mut app, KeyCode::Down);
        press(&mut app, KeyCode::Enter);
        press(&mut app, KeyCode::Backspace);
        assert_eq!(inside_rows(&app, 120, 30)[4..10], marked(&AT_112, 1));
    }

    #[test]
    fn details_read_for_another_account_are_never_drawn() {
        let mut app = read(watching(), WEIHAN, five_entries());
        press(&mut app, KeyCode::Down);
        press(&mut app, KeyCode::Char('h'));
        if let Some(pane) = &mut app.history {
            pane.mode = HistoryMode::Detail { first: 0 };
        }
        let rows = inside_rows(&app, 120, 30);
        assert_eq!(
            rows[..3],
            [
                "",
                "  0x17c5185167401eD00cF5F5b2fc97D9BBfDb7D025",
                "  reading history..."
            ],
            "{rows:#?}"
        );
    }

    #[test]
    fn details_taller_than_the_body_scroll_by_row_until_their_last_row_shows() {
        let mut app = read(ready(), SENDER, five_entries());
        (app.width, app.height) = (80, 16);
        press(&mut app, KeyCode::Char('h'));
        press(&mut app, KeyCode::Enter);
        let body = |app: &App| {
            let rows = inside_rows(app, 80, 16);
            rows[..rows.len() - 1].to_vec()
        };
        assert_eq!(
            body(&app)[..3],
            [
                "",
                "  transaction",
                "    0x671eb606000de4305cab87a0f6b334fef8ac4be6e994536e05586d7752e24543"
            ]
        );
        press(&mut app, KeyCode::Down);
        assert_eq!(
            body(&app)[1],
            "    0x671eb606000de4305cab87a0f6b334fef8ac4be6e994536e05586d7752e24543"
        );
        press(&mut app, KeyCode::Down);
        assert_eq!(body(&app)[1], "  block        11,969,090 · 7 min ago");
        for _ in 0..10 {
            press(&mut app, KeyCode::Down);
        }
        let rows = body(&app);
        assert_eq!(
            rows.last().unwrap(),
            "    4be6e994536e05586d7752e24543",
            "{rows:#?}"
        );
        press(&mut app, KeyCode::Up);
        assert_eq!(body(&app)[2..], rows[1..rows.len() - 1]);
    }

    #[test]
    fn a_detail_row_wrapped_past_a_short_body_scrolls_to_its_end_and_straight_back() {
        let mut app = read(ready(), SENDER, five_entries());
        (app.width, app.height) = (20, 8);
        press(&mut app, KeyCode::Char('h'));
        press(&mut app, KeyCode::Enter);
        let body = |app: &App| {
            let buffer = drawn(app, &[], 20, 8);
            (1..8)
                .map(|y| row_from(&buffer, 0, y).trim_end().to_owned())
                .take_while(|row| !row.starts_with("Esc back"))
                .collect::<Vec<_>>()
        };
        let url = "  https://sepolia.etherscan.io/tx/0x671eb606000de4305cab87a0f6b334fef8ac4be6e994536e05586d7752e24543";
        let tail = hanging(url, 20).last().unwrap().to_string();
        assert!(
            body(&app).len() < hanging(url, 20).len(),
            "{:#?}",
            body(&app)
        );
        for _ in 0..100 {
            press(&mut app, KeyCode::Down);
        }
        let end = body(&app);
        assert_eq!(end.last(), Some(&tail), "{end:#?}");
        press(&mut app, KeyCode::Up);
        let up = body(&app);
        assert_eq!(up[2..], end[1..end.len() - 1], "{up:#?}");
    }

    #[test]
    fn up_and_down_scroll_the_details_by_a_row_between_their_first_and_last_start() {
        let mut app = read(watching(), WEIHAN, five_entries());
        (app.width, app.height) = (80, 8);
        press(&mut app, KeyCode::Char('h'));
        press(&mut app, KeyCode::Enter);
        let first = |app: &App| match app.history_mode() {
            HistoryMode::Detail { first } => Some(first),
            HistoryMode::Closed | HistoryMode::List => None,
        };
        press(&mut app, KeyCode::Up);
        assert_eq!(first(&app), Some(0));
        let end = detail_end(&app);
        assert!(end >= 2, "{end}");
        for _ in 0..end + 5 {
            press(&mut app, KeyCode::Down);
        }
        assert_eq!(first(&app), Some(end));
        press(&mut app, KeyCode::Up);
        assert_eq!(first(&app), Some(end - 1));
        assert_eq!(app.history.as_ref().map(|pane| pane.selected), Some(0));
    }

    #[test]
    fn the_selected_row_stays_in_view_as_it_moves_down_a_long_history() {
        let entries = (0..40)
            .map(|n| {
                let tx = format!("tx {n}");
                let block = format!("11,900,{n:03}");
                entry(
                    100 - n,
                    [&tx, &block, "1m", "0x1111…2222", "you", "+0.001 ETH", ""],
                    false,
                )
            })
            .collect();
        let mut app = read(ready(), SENDER, entries);
        press(&mut app, KeyCode::Char('h'));
        let table = |app: &App| inside_rows(app, 120, 16)[5..11].to_vec();
        let rows = table(&app);
        assert!(rows[0].starts_with("> tx 0 ") && rows[5].starts_with("  tx 5 "));
        for _ in 0..10 {
            press(&mut app, KeyCode::Down);
        }
        let rows = table(&app);
        assert!(rows[0].starts_with("  tx 5 "), "{rows:#?}");
        assert!(rows[5].starts_with("> tx 10 "), "{rows:#?}");
    }

    #[test]
    fn below_the_mandatory_columns_width_each_table_row_takes_one_screen_row() {
        let rows_of = |app: &App, width: u16, height: u16| -> Vec<String> {
            sized(app, &[], width, height)
                .lines()
                .map(|row| row.trim_end().to_owned())
                .collect()
        };
        let entries = (0..40)
            .map(|n| {
                let tx = format!("tx {n}");
                let block = format!("11,900,{n:03}");
                entry(
                    100 - n,
                    [&tx, &block, "1m", "0x1111…2222", "you", "+0.001 ETH", ""],
                    false,
                )
            })
            .collect();
        let mut app = read(ready(), SENDER, entries);
        press(&mut app, KeyCode::Char('h'));
        for _ in 0..10 {
            press(&mut app, KeyCode::Down);
            let rows = rows_of(&app, 20, 16);
            assert!(rows.iter().any(|row| row.starts_with("> ")), "{rows:#?}");
        }

        let app = read(watching_only(&[WEIHAN]), WEIHAN, five_entries());
        let rows = rows_of(&app, 20, 60);
        let recent = recent_rows(&rows).expect("a section");
        assert_eq!(recent.len(), 7, "{rows:#?}");
        assert!(recent.iter().all(|row| row.chars().count() <= 20));
    }

    #[test]
    fn a_flagged_row_shows_its_amount_in_the_warning_colour() {
        let app = read(ready(), SENDER, five_entries());
        let buffer = drawn(&app, &[], 120, 40);
        let colour_of = |text: &str| {
            let (x, y) = (0..40)
                .flat_map(|y| (0..120).map(move |x| (x, y)))
                .find(|&(x, y)| row_from(&buffer, x, y).starts_with(text))
                .unwrap_or_else(|| panic!("{text}"));
            buffer[(x, y)].fg
        };
        assert_eq!(colour_of("⚠ 0.00 USDC"), Color::Red);
        assert_eq!(colour_of("+0.50 ETH"), Color::Reset);
    }

    #[test]
    fn a_wrapped_coverage_line_continues_four_spaces_in_without_splitting_a_dot() {
        let mut app = with_history(ready());
        press(&mut app, KeyCode::Char('h'));
        assert_eq!(app.history_request(false), Some(SENDER));
        let coverage = "history since block 11,962,247 · gaps: 11,966,534-11,966,544, 11,966,588-11,966,605 · nonce 42 unexplained between blocks 11,962,690 and 11,966,550 · earlier sends may predate the history";
        app.history_read(
            SENDER,
            Some(HistoryLines {
                coverage: coverage.to_owned(),
                entries: Vec::new(),
            }),
        );
        let rows = inside_rows(&app, 80, 24);
        let start = rows
            .iter()
            .position(|row| row.starts_with("  history since"))
            .expect("coverage row");
        let end = start
            + rows[start..]
                .iter()
                .take_while(|row| !row.is_empty())
                .count();
        let wrapped = &rows[start..end];
        assert!(wrapped.len() > 1, "{rows:#?}");
        for row in &wrapped[1..] {
            assert!(
                row.starts_with("    ") && !row.starts_with("     "),
                "{rows:#?}"
            );
        }
        let joined: Vec<&str> = wrapped.iter().map(|row| row.trim()).collect();
        assert_eq!(joined.join(" "), coverage, "{rows:#?}");
    }

    #[test]
    fn the_history_pane_says_when_nothing_is_recorded_yet() {
        let mut app = with_history(watching());
        press(&mut app, KeyCode::Char('h'));
        assert_eq!(app.history_request(false), Some(WEIHAN));
        app.history_read(
            WEIHAN,
            Some(HistoryLines {
                coverage: crate::activity::NO_HISTORY.to_owned(),
                entries: Vec::new(),
            }),
        );
        let rows = inside_rows(&app, 120, 30);
        assert_eq!(
            rows[..4],
            [
                "",
                "  0x3499ddf9E66b53Ca484554d5309Bf32521AAE5a7",
                "  no history yet",
                ""
            ]
        );
    }

    #[test]
    fn the_footer_names_the_history_key_only_with_history_on() {
        let twelve: Vec<Address> = (1..=12).map(Address::with_last_byte).collect();
        let cases = [
            (
                ready(),
                "Enter review · l logs",
                "h history · l logs · q quit",
            ),
            (watching(), "l logs · q quit", "h history · l logs · q quit"),
            (
                watching_only(&twelve),
                "up/down scroll · l logs · q quit",
                "up/down scroll · h history · l logs · q quit",
            ),
        ];
        for (app, off, on) in cases {
            let shown = inside(&app, &[], 120, 16);
            assert!(
                shown.contains(off) && !shown.contains("h history"),
                "{shown}"
            );
            let app = with_history(app);
            let shown = inside(&app, &[], 120, 16);
            assert!(shown.contains(on), "{shown}");
        }
        let mut app = with_history(ready());
        app.form.field = Field::To;
        assert!(!inside(&app, &[], 120, 30).contains("h history"));
    }

    /// The rows from the `recent activity` heading's blank row on, `None` without one.
    fn recent_rows(rows: &[String]) -> Option<Vec<String>> {
        let heading = rows
            .iter()
            .position(|row| row.starts_with("  recent activity"))?;
        assert_eq!(rows[heading - 1], "", "{rows:#?}");
        Some(
            rows[heading..]
                .iter()
                .take_while(|row| !row.is_empty())
                .cloned()
                .collect(),
        )
    }

    fn cat_cells(buffer: &Buffer) -> Vec<(u16, u16, Cell)> {
        let width = buffer.area().width;
        (0..buffer.area().height)
            .flat_map(|y| (0..width).map(move |x| (x, y)))
            .filter(|&at| half_block(buffer[at].symbol()))
            .map(|(x, y)| (x, y, buffer[(x, y)].clone()))
            .collect()
    }

    fn to_strings(rows: &[&str]) -> Vec<String> {
        rows.iter().map(|&row| row.to_owned()).collect()
    }

    #[test]
    fn the_send_form_shows_the_newest_entries_as_a_table_under_it_with_the_cat_in_place() {
        let mut app = ready();
        app.pixel_cats = true;
        app.width = 120;
        let alone = drawn(&app, &[], 120, 40);
        let app = read(app, SENDER, five_entries());
        let rows = inside_rows(&app, 120, 40);
        let mut expected = vec!["  recent activity"];
        expected.extend(AT_112);
        assert_eq!(recent_rows(&rows), Some(to_strings(&expected)), "{rows:#?}");
        let heading = rows.iter().position(|row| row == "  recent activity");
        let fee = rows.iter().position(|row| row.contains("  fee"));
        assert!(fee < heading, "{rows:#?}");
        let with = drawn(&app, &[], 120, 40);
        assert!(!cat_cells(&alone).is_empty());
        assert_eq!(cat_cells(&with), cat_cells(&alone));
        let cat_bottom = cat_cells(&with).iter().map(|&(_, y, _)| y).max().unwrap();
        let heading_y = (0..40)
            .find(|&y| row_from(&with, 0, y).contains("recent activity"))
            .unwrap();
        assert!(heading_y > cat_bottom + 1, "{heading_y} {cat_bottom}");
    }

    #[test]
    fn a_short_send_form_shows_only_the_rows_that_fit_under_the_table_heading() {
        let app = read(ready(), SENDER, five_entries());
        let expected = |shown: usize| {
            let mut rows = vec!["  recent activity"];
            rows.extend(&AT_72[..=shown]);
            rows.push("up/down field · Enter or left/right token · type amount (or max) and to");
            rows.push("ctrl+u clear · Enter review · h history · l logs · q quit");
            Some(to_strings(&rows))
        };
        let rows = inside_rows(&app, 80, 21);
        assert_eq!(recent_rows(&rows), expected(5), "{rows:#?}");
        let rows = inside_rows(&app, 80, 20);
        assert_eq!(recent_rows(&rows), expected(4), "{rows:#?}");
        let rows = inside_rows(&app, 80, 17);
        assert_eq!(recent_rows(&rows), expected(1), "{rows:#?}");

        let rows = inside_rows(&app, 80, 16);
        assert_eq!(recent_rows(&rows), None, "{rows:#?}");
        assert!(rows.iter().any(|row| row.contains("  fee")), "{rows:#?}");
    }

    #[test]
    fn a_word_wider_than_a_continuation_row_is_split_at_the_width() {
        let lines = hanging("  1  to 0xAbFC25F2A93829e9A500A5641222F1f2530BD06f", 20);
        let rows: Vec<String> = lines.iter().map(ToString::to_string).collect();
        assert_eq!(
            rows,
            [
                "  1  to",
                "    0xAbFC25F2A93829",
                "    e9A500A5641222F1",
                "    f2530BD06f",
            ]
        );
    }

    #[test]
    fn the_watch_screen_names_the_account_of_its_entries_only_when_watching_several() {
        let one = read(watching_only(&[WEIHAN]), WEIHAN, five_entries());
        let rows = inside_rows(&one, 120, 30);
        let recent = recent_rows(&rows).expect("a section");
        assert_eq!(recent[..3], ["  recent activity", AT_112[0], AT_112[1]]);

        let several = read(watching(), WEIHAN, five_entries());
        let rows = inside_rows(&several, 120, 30);
        let recent = recent_rows(&rows).expect("a section");
        assert_eq!(
            recent[..2],
            [
                "  recent activity of 0x3499ddf9E66b53Ca484554d5309Bf32521AAE5a7",
                AT_112[0]
            ]
        );
        let list = rows
            .iter()
            .position(|row| row.contains("0x17c5185167401eD00cF5F5b2fc97D9BBfDb7D025"));
        let heading = rows
            .iter()
            .position(|row| row.starts_with("  recent activity"));
        assert!(list < heading, "{rows:#?}");

        let twelve: Vec<Address> = (1..=12).map(Address::with_last_byte).collect();
        let off = inside_rows(&watching_only(&twelve), 120, 16);
        let on = read(watching_only(&twelve), twelve[0], five_entries());
        let rows = inside_rows(&on, 120, 16);
        assert_eq!(recent_rows(&rows), None, "{rows:#?}");
        assert_eq!(rows[..rows.len() - 1], off[..off.len() - 1]);
    }

    #[test]
    fn the_section_says_when_no_activity_is_recorded_and_is_absent_before_the_first_read() {
        let mut app = with_history(watching_only(&[WEIHAN]));
        assert_eq!(recent_rows(&inside_rows(&app, 120, 30)), None);
        assert_eq!(app.history_request(false), Some(WEIHAN));
        assert_eq!(recent_rows(&inside_rows(&app, 120, 30)), None);
        app.history_read(
            WEIHAN,
            Some(HistoryLines {
                coverage: crate::activity::NO_HISTORY.to_owned(),
                entries: Vec::new(),
            }),
        );
        assert_eq!(
            recent_rows(&inside_rows(&app, 120, 30)),
            Some(vec![
                "  recent activity".to_owned(),
                "  no activity recorded yet".to_owned()
            ])
        );
    }

    #[test]
    fn with_history_off_the_form_and_watch_screen_draw_as_before() {
        for mut app in [ready(), watching()] {
            let before = drawn(&app, &[], 120, 40);
            let account = app.history_account().unwrap();
            assert_eq!(app.history_request(true), None);
            app.history_read(
                account,
                Some(HistoryLines {
                    coverage: String::new(),
                    entries: five_entries(),
                }),
            );
            assert_eq!(drawn(&app, &[], 120, 40), before);
        }
    }
}
