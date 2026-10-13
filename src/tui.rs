//! Default frontend, following common TUI conventions (lazygit, k9s, chat clients):
//! chats list | open conversation | input, `?` help overlay, `/` command palette with completion,
//! a one-line hint bar with short-lived toasts, and popups drawn over a cleared area.

use std::{
    sync::Arc,
    time::{Duration, Instant},
};

use anyhow::{bail, Result};
use crossterm::event::{Event as TermEvent, EventStream, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use futures::StreamExt;
use hopr_chat::{
    app::{SafeChoice, Status, Ui},
    chat::{compact, Chat, Event, Wallet},
    db,
};
use ratatui::{
    layout::{Constraint, Flex, Layout, Rect},
    style::{Color, Modifier, Style, Stylize},
    text::{Line, Span},
    widgets::{Block, BorderType, Clear, List, ListItem, ListState, Padding, Paragraph, Row, Table, Wrap},
    DefaultTerminal, Frame,
};
use tokio::sync::mpsc::UnboundedReceiver;

use crate::term::{self, ago};

const COMMANDS: &[(&str, &str, &str)] = &[
    ("/add", "<nick> <0xaddress>", "save a peer by its hoprd node address"),
    ("/peers", "", "known peers and their reachability"),
    ("/balance", "", "node and Safe balances"),
    ("/whoami", "", "your /add line to share with others"),
    ("/help", "", "keys and commands"),
    ("/quit", "", "close sessions and exit"),
];

const KEYS: &[(&str, &str)] = &[
    ("Enter", "send to the open chat / run command"),
    ("↑ ↓", "switch chat (or pick a completion)"),
    ("Tab", "complete command"),
    ("PgUp PgDn", "scroll conversation"),
    ("← → Home End", "move cursor"),
    ("Ctrl-W  Ctrl-U", "delete word / line"),
    ("Esc", "close popup, clear input"),
    ("?", "this help (when input is empty)"),
    ("Ctrl-C  Ctrl-Q", "quit"),
];

const TOAST_TTL: Duration = Duration::from_secs(6);

#[derive(Default)]
pub struct TuiUi {
    funding_shown: bool,
}

impl Ui for TuiUi {
    async fn choose_safe(&mut self, node_address: &str) -> Result<SafeChoice> {
        term::choose_safe(node_address).await
    }

    fn status(&mut self, status: Status) {
        term::print_status(status, &mut self.funding_shown);
    }

    async fn run(&mut self, chat: Arc<Chat>, events: UnboundedReceiver<Event>) -> Result<()> {
        let mut app = App::new(chat);
        app.refresh_wallet().await;
        app.reload();
        let mut terminal = ratatui::init();
        let res = app.event_loop(&mut terminal, events).await;
        ratatui::restore();
        res
    }
}

enum Popup {
    Help,
    Info { title: String, lines: Vec<Line<'static>> },
}

struct App {
    chat: Arc<Chat>,
    /// (label, unread, last text): conversations first, then peers without messages.
    chats: Vec<(String, i64, String)>,
    list: ListState,
    msgs: Vec<db::Msg>,
    /// Lines scrolled up from the bottom of the conversation.
    scroll: u16,
    input: Vec<char>,
    cursor: usize,
    completion: usize,
    popup: Option<Popup>,
    toast: Option<(String, Color, Instant)>,
    wallet: Option<Wallet>,
}

impl App {
    fn new(chat: Arc<Chat>) -> Self {
        Self {
            chat,
            chats: vec![],
            list: ListState::default(),
            msgs: vec![],
            scroll: 0,
            input: vec![],
            cursor: 0,
            completion: 0,
            popup: None,
            toast: None,
            wallet: None,
        }
    }

    async fn event_loop(&mut self, terminal: &mut DefaultTerminal, mut events: UnboundedReceiver<Event>) -> Result<()> {
        let mut keys = EventStream::new();
        let mut wallet_tick = tokio::time::interval(Duration::from_secs(30));
        let mut redraw_tick = tokio::time::interval(Duration::from_secs(1));
        loop {
            terminal.draw(|f| self.draw(f))?;
            tokio::select! {
                Some(ev) = events.recv() => self.on_event(ev),
                Some(Ok(ev)) = keys.next() => {
                    if let TermEvent::Key(key) = ev {
                        if key.kind == KeyEventKind::Press && self.on_key(key).await {
                            return Ok(());
                        }
                    }
                }
                _ = wallet_tick.tick() => self.refresh_wallet().await,
                _ = redraw_tick.tick() => {}
            }
        }
    }

    fn toast(&mut self, text: impl Into<String>, color: Color) {
        self.toast = Some((text.into(), color, Instant::now()));
    }

    fn selected(&self) -> Option<&str> {
        self.list.selected().and_then(|i| self.chats.get(i)).map(|(p, ..)| p.as_str())
    }

    /// Re-reads conversations and the open one; viewing a conversation marks it read.
    fn reload(&mut self) {
        let selected = self.selected().map(str::to_string);
        let mut chats: Vec<_> =
            self.chat.conversations().unwrap_or_default().into_iter().map(|c| (c.peer, c.unread, c.last_text)).collect();
        for name in self.chat.peer_names() {
            if !chats.iter().any(|(p, ..)| *p == name) {
                chats.push((name, 0, String::new()));
            }
        }
        self.chats = chats;
        let index = selected.and_then(|s| self.chats.iter().position(|(p, ..)| *p == s));
        self.list.select(index.or((!self.chats.is_empty()).then_some(0)));
        self.msgs = match self.selected().map(str::to_string) {
            Some(peer) => self.chat.history(&peer, 500).unwrap_or_default(),
            None => vec![],
        };
        if let Some(open) = self.list.selected().and_then(|i| self.chats.get_mut(i)) {
            open.1 = 0;
        }
    }

    fn switch_chat(&mut self, delta: isize) {
        if self.chats.is_empty() {
            return;
        }
        let len = self.chats.len() as isize;
        let next = (self.list.selected().unwrap_or(0) as isize + delta).rem_euclid(len);
        self.list.select(Some(next as usize));
        self.scroll = 0;
        self.reload();
    }

    fn on_event(&mut self, ev: Event) {
        match ev {
            Event::Message(m) if Some(m.peer.as_str()) != self.selected() => {
                let nick = m.nick.unwrap_or(m.peer);
                self.toast(format!("new message from {nick}"), Color::Cyan);
            }
            Event::Unreachable { peer, error } => self.toast(format!("{peer} unreachable, will retry: {error}"), Color::Red),
            Event::Warning(w) => self.toast(w, Color::Red),
            Event::Message(_) | Event::Delivered { .. } | Event::SessionClosed { .. } => {}
        }
        self.reload();
    }

    /// Slash commands matching the typed prefix, while the command word is still being typed.
    fn completions(&self) -> Vec<&'static (&'static str, &'static str, &'static str)> {
        let text: String = self.input.iter().collect();
        if !text.starts_with('/') || text.contains(' ') {
            return vec![];
        }
        COMMANDS.iter().filter(|(c, ..)| c.starts_with(text.as_str())).collect()
    }

    fn set_input(&mut self, text: &str) {
        self.input = text.chars().collect();
        self.cursor = self.input.len();
        self.completion = 0;
    }

    /// Returns true when the user wants to quit.
    async fn on_key(&mut self, key: KeyEvent) -> bool {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        if self.popup.is_some() {
            if matches!(key.code, KeyCode::Esc | KeyCode::Enter | KeyCode::Char('?') | KeyCode::Char('q')) {
                self.popup = None;
            }
            return false;
        }
        let completions = self.completions();
        match key.code {
            KeyCode::Char('c' | 'q') if ctrl => return true,
            KeyCode::Char('u') if ctrl => self.set_input(""),
            KeyCode::Char('w') if ctrl => {
                let start = self.input[..self.cursor].iter().rposition(|c| *c != ' ').map_or(0, |end| {
                    self.input[..end].iter().rposition(|c| *c == ' ').map_or(0, |i| i + 1)
                });
                self.input.drain(start..self.cursor);
                self.cursor = start;
            }
            KeyCode::Char('?') if self.input.is_empty() => self.popup = Some(Popup::Help),
            KeyCode::Char(c) => {
                self.input.insert(self.cursor, c);
                self.cursor += 1;
                self.completion = 0;
            }
            KeyCode::Backspace if self.cursor > 0 => {
                self.cursor -= 1;
                self.input.remove(self.cursor);
                self.completion = 0;
            }
            KeyCode::Delete if self.cursor < self.input.len() => {
                self.input.remove(self.cursor);
            }
            KeyCode::Left => self.cursor = self.cursor.saturating_sub(1),
            KeyCode::Right => self.cursor = (self.cursor + 1).min(self.input.len()),
            KeyCode::Home => self.cursor = 0,
            KeyCode::End => self.cursor = self.input.len(),
            KeyCode::Up if !completions.is_empty() => {
                self.completion = (self.completion + completions.len() - 1) % completions.len()
            }
            KeyCode::Down if !completions.is_empty() => self.completion = (self.completion + 1) % completions.len(),
            KeyCode::Up => self.switch_chat(-1),
            KeyCode::Down => self.switch_chat(1),
            KeyCode::PageUp => self.scroll = self.scroll.saturating_add(10),
            KeyCode::PageDown => self.scroll = self.scroll.saturating_sub(10),
            KeyCode::Tab if !completions.is_empty() => {
                let (cmd, args, _) = completions[self.completion % completions.len()];
                self.set_input(&if args.is_empty() { cmd.to_string() } else { format!("{cmd} ") });
            }
            KeyCode::Esc => self.set_input(""),
            KeyCode::Enter => {
                // Enter on a half-typed command accepts the highlighted completion first.
                if let Some((cmd, args, _)) = completions.get(self.completion % completions.len().max(1)).copied() {
                    let typed: String = self.input.iter().collect();
                    if typed != *cmd || !args.is_empty() {
                        self.set_input(&if args.is_empty() { cmd.to_string() } else { format!("{cmd} ") });
                        if !args.is_empty() {
                            return false;
                        }
                    }
                }
                let line: String = std::mem::take(&mut self.input).into_iter().collect();
                self.cursor = 0;
                match self.submit(line.trim()).await {
                    Ok(quit) => return quit,
                    Err(e) => self.toast(format!("{e:#}"), Color::Red),
                }
                self.reload();
            }
            _ => {}
        }
        false
    }

    /// Returns true on `/quit`.
    async fn submit(&mut self, line: &str) -> Result<bool> {
        let mut words = line.split_whitespace();
        match words.next() {
            None => {}
            Some("/quit") => return Ok(true),
            Some("/help") => self.popup = Some(Popup::Help),
            Some("/add") => {
                let (Some(nick), Some(address)) = (words.next(), words.next()) else {
                    bail!("usage: /add <nick> <0xaddress>");
                };
                self.chat.add_peer(nick, address).await?;
                self.reload();
                if let Some(i) = self.chats.iter().position(|(p, ..)| p == nick) {
                    self.list.select(Some(i));
                }
                self.toast(format!("added {nick}"), Color::Green);
            }
            Some("/peers") => {
                let lines = self
                    .chat
                    .peers()
                    .await?
                    .into_iter()
                    .map(|p| {
                        let state = if p.online { "session".green() } else { "idle".dim() };
                        let seen = p.last_seen.map_or("never".into(), ago);
                        Line::from(vec![format!("{:<12} ", p.nick).cyan(), format!("{} ", p.address).into(), state, format!("  seen {seen}").dim()])
                    })
                    .collect();
                self.popup = Some(Popup::Info { title: " peers ".into(), lines });
            }
            Some("/balance") => {
                self.refresh_wallet().await;
                let w = self.wallet.as_ref().ok_or_else(|| anyhow::anyhow!("balance unavailable"))?;
                let mut lines = vec![
                    Line::from(vec!["Node wallet".bold(), "  pays gas for channel and ticket transactions".dim()]),
                    Line::from(vec!["  gas       ".dim(), compact(&w.native).into()]),
                    Line::from(vec!["  wxHOPR    ".dim(), compact(&w.hopr).into(), "  (normally 0, it lives in the Safe)".dim()]),
                    Line::from(""),
                    Line::from(vec!["Safe".bold(), "  holds the wxHOPR that funds channels to relays".dim()]),
                    Line::from(vec!["  funds     ".dim(), compact(&w.safe_hopr).into()]),
                    Line::from(vec!["  allowance ".dim(), compact(&w.safe_allowance).into(), "  (max the channels contract may spend)".dim()]),
                    Line::from(vec!["  xDai      ".dim(), compact(&w.safe_native).into()]),
                ];
                let warnings = w.warnings();
                if !warnings.is_empty() {
                    lines.push(Line::from(""));
                }
                for (critical, text) in warnings {
                    lines.push(Line::from(if critical { format!("✗ {text}").red().bold() } else { format!("! {text}").yellow() }));
                }
                if lines.len() == 8 {
                    lines.push(Line::from(""));
                    lines.push(Line::from("✓ ready to pay relays".green()));
                }
                self.popup = Some(Popup::Info { title: " balance ".into(), lines });
            }
            Some("/whoami") => {
                let lines = vec![
                    Line::from("Give this to people who want to message you:".dim()),
                    Line::from(""),
                    Line::from(format!("/add {} {}", self.chat.nick(), self.chat.me()).green()),
                ];
                self.popup = Some(Popup::Info { title: " whoami ".into(), lines });
            }
            Some(cmd) if cmd.starts_with('/') => bail!("unknown command {cmd}, press ? for help"),
            Some(_) => {
                let Some(peer) = self.selected().map(str::to_string) else {
                    bail!("no chat open: /add <nick> <0xaddress> first");
                };
                self.chat.send(&peer, line).await?;
                self.scroll = 0;
            }
        }
        Ok(false)
    }

    /// Announces a critical funding problem once, when it first appears.
    async fn refresh_wallet(&mut self) {
        let was_critical = self.wallet.as_ref().is_some_and(|w| w.warnings().iter().any(|(c, _)| *c));
        self.wallet = self.chat.wallet().await.ok();
        let critical = self.wallet.as_ref().and_then(|w| w.warnings().into_iter().find(|(c, _)| *c));
        if let (Some((_, text)), false) = (critical, was_critical) {
            self.toast(text, Color::Red);
        }
    }

    fn draw(&mut self, f: &mut Frame) {
        let [header, body, input, footer] =
            Layout::vertical([Constraint::Length(1), Constraint::Min(5), Constraint::Length(3), Constraint::Length(1)]).areas(f.area());
        let [left, right] = Layout::horizontal([Constraint::Length(28), Constraint::Min(20)]).areas(body);

        self.draw_header(f, header);
        self.draw_chats(f, left);
        self.draw_conversation(f, right);
        self.draw_input(f, input);
        self.draw_footer(f, footer);
        self.draw_completions(f, input);
        match &self.popup {
            Some(Popup::Help) => draw_help(f),
            Some(Popup::Info { title, lines }) => draw_popup(f, title, lines.clone(), 72, lines.len() as u16 + 4),
            None => {}
        }
    }

    fn draw_header(&self, f: &mut Frame, area: Rect) {
        let signing = if self.chat.signing() { " signed ".black().on_green() } else { " UNSIGNED ".black().on_yellow() };
        let hops = match self.chat.hops() {
            0 => " 0 hops · NOT anonymous ".black().on_red(),
            n => format!(" {n} hop{} ", if n == 1 { "" } else { "s" }).black().on_blue(),
        };
        let mut spans = vec![
            format!(" {} ", self.chat.nick()).black().on_cyan().bold(),
            format!(" {} ", self.chat.me()).dim(),
            signing,
            " ".into(),
            hops,
        ];
        match &self.wallet {
            Some(w) => {
                let warnings = w.warnings();
                let level = |needle: &str| match warnings.iter().find(|(_, t)| t.contains(needle)) {
                    Some((true, _)) => Color::Red,
                    Some((false, _)) => Color::Yellow,
                    None => Color::Green,
                };
                spans.extend([
                    "  gas ".dim(),
                    compact(&w.native).fg(level("gas")),
                    " · funds ".dim(),
                    compact(&w.safe_hopr).fg(level("Safe")),
                ]);
                if !warnings.is_empty() {
                    spans.push("  ⚠ /balance".fg(level("")).bold());
                }
            }
            None => spans.push("  balance unavailable".dim()),
        }
        let line = Line::from(spans);
        f.render_widget(line, area);
    }

    fn draw_chats(&mut self, f: &mut Frame, area: Rect) {
        let items: Vec<ListItem> = self
            .chats
            .iter()
            .map(|(peer, unread, last)| {
                let mut name = vec![Span::raw(short(peer))];
                if *unread > 0 {
                    name[0] = name[0].clone().bold();
                    name.push(format!(" {unread}").black().on_yellow());
                }
                ListItem::new(vec![Line::from(name), Line::from(format!(" {last}").dim())])
            })
            .collect();
        let block = Block::bordered().border_type(BorderType::Rounded).title(" chats ");
        if items.is_empty() {
            let hint = Paragraph::new(vec![Line::from("no chats yet".dim()), Line::from(""), Line::from("/add <nick> <0xaddr>".cyan())])
                .block(block)
                .wrap(Wrap { trim: true });
            f.render_widget(hint, area);
            return;
        }
        let list = List::new(items)
            .block(block)
            .highlight_style(Style::new().bg(Color::DarkGray).add_modifier(Modifier::BOLD))
            .highlight_symbol("▌");
        f.render_stateful_widget(list, area, &mut self.list);
    }

    fn draw_conversation(&mut self, f: &mut Frame, area: Rect) {
        let title = self.selected().map_or(" ".to_string(), |p| format!(" {p} "));
        let block = Block::bordered().border_type(BorderType::Rounded).title(title);
        let lines: Vec<Line> = self.msgs.iter().map(|m| msg_line(m, self.chat.nick())).collect();
        let empty = lines.is_empty();
        let para = Paragraph::new(lines).wrap(Wrap { trim: false });
        let inner = block.inner(area);
        let total = para.line_count(inner.width) as u16;
        self.scroll = self.scroll.min(total.saturating_sub(inner.height));
        let top = total.saturating_sub(inner.height + self.scroll);
        f.render_widget(block, area);
        if empty {
            let hint = match self.selected() {
                Some(_) => vec![Line::from("no messages yet, say hi".dim())],
                None => vec![
                    Line::from("Welcome to hopr-chat".bold()),
                    Line::from(""),
                    Line::from(vec!["/add <nick> <0xaddress>".cyan(), " to start a chat".dim()]),
                    Line::from(vec!["/whoami".cyan(), " to get the line others need to reach you".dim()]),
                    Line::from(vec!["?".cyan(), " for all keys".dim()]),
                ],
            };
            let [middle] = Layout::vertical([Constraint::Length(hint.len() as u16)]).flex(Flex::Center).areas(inner);
            f.render_widget(Paragraph::new(hint).centered(), middle);
        } else {
            f.render_widget(para.scroll((top, 0)), inner);
        }
    }

    fn draw_input(&self, f: &mut Frame, area: Rect) {
        let title = match self.selected() {
            Some(p) => format!(" message {} ", short(p)),
            None => " message ".into(),
        };
        let block = Block::bordered().border_type(BorderType::Rounded).border_style(Style::new().cyan()).title(title);
        let inner = block.inner(area);
        let width = inner.width.max(1) as usize;
        let start = (self.cursor + 1).saturating_sub(width);
        let shown: String = self.input[start..].iter().take(width).collect();
        let text = if self.input.is_empty() {
            Line::from("type a message, / for commands, ? for help".dim())
        } else {
            Line::from(shown)
        };
        f.render_widget(Paragraph::new(text).block(block), area);
        if self.popup.is_none() {
            f.set_cursor_position((inner.x + (self.cursor - start) as u16, inner.y));
        }
    }

    fn draw_footer(&self, f: &mut Frame, area: Rect) {
        let line = match &self.toast {
            Some((text, color, at)) if at.elapsed() < TOAST_TTL => Line::from(format!(" {text}").fg(*color)),
            _ => Line::from(vec![
                " ? ".black().on_gray(),
                " help  ".dim(),
                " / ".black().on_gray(),
                " commands  ".dim(),
                " ↑↓ ".black().on_gray(),
                " switch chat  ".dim(),
                " PgUp/PgDn ".black().on_gray(),
                " scroll  ".dim(),
                " ^C ".black().on_gray(),
                " quit".dim(),
            ]),
        };
        f.render_widget(line, area);
    }

    /// Palette opens just above the input while a command word is being typed.
    fn draw_completions(&self, f: &mut Frame, input: Rect) {
        let completions = self.completions();
        if completions.is_empty() || self.popup.is_some() {
            return;
        }
        let height = completions.len() as u16 + 2;
        let area = Rect { x: input.x + 1, y: input.y.saturating_sub(height), width: input.width.saturating_sub(2).min(72), height };
        let selected = self.completion % completions.len();
        let rows = completions.iter().enumerate().map(|(i, (cmd, args, help))| {
            let row = Row::new(vec![cmd.to_string(), args.to_string(), help.to_string()]);
            if i == selected { row.style(Style::new().bg(Color::DarkGray).bold()) } else { row }
        });
        let table = Table::new(rows, [Constraint::Length(9), Constraint::Length(20), Constraint::Min(10)])
            .block(Block::bordered().border_type(BorderType::Rounded).title(" commands · Tab complete ").border_style(Style::new().cyan()));
        f.render_widget(Clear, area);
        f.render_widget(table, area);
    }
}

fn draw_help(f: &mut Frame) {
    let mut lines = vec![Line::from("Keys".bold().cyan())];
    lines.extend(KEYS.iter().map(|(k, d)| Line::from(vec![format!("  {k:<16}").yellow(), (*d).into()])));
    lines.push(Line::from(""));
    lines.push(Line::from("Commands".bold().cyan()));
    lines.extend(COMMANDS.iter().map(|(c, a, d)| Line::from(vec![format!("  {c:<9}").yellow(), format!("{a:<20} ").dim(), (*d).into()])));
    lines.push(Line::from(""));
    lines.push(Line::from("Badges: ✓ signed by sender's node · ? unsigned · ✗ forged".dim()));
    lines.push(Line::from("Sent: … pending · ✓ sent · ✓✓ delivered".dim()));
    let height = lines.len() as u16 + 4;
    draw_popup(f, " help · Esc to close ", lines, 76, height);
}

fn draw_popup(f: &mut Frame, title: &str, lines: Vec<Line<'static>>, width: u16, height: u16) {
    let area = centered(f.area(), width, height);
    f.render_widget(Clear, area);
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::new().cyan())
        .title(title.to_string())
        .padding(Padding::uniform(1));
    f.render_widget(Paragraph::new(lines).block(block).wrap(Wrap { trim: false }), area);
}

fn centered(area: Rect, width: u16, height: u16) -> Rect {
    let [v] = Layout::vertical([Constraint::Length(height.min(area.height))]).flex(Flex::Center).areas(area);
    let [h] = Layout::horizontal([Constraint::Length(width.min(area.width))]).flex(Flex::Center).areas(v);
    h
}

fn msg_line<'a>(m: &'a db::Msg, me: &'a str) -> Line<'a> {
    let time = format!("{:>4} ", ago(m.created)).dim();
    if m.dir == "out" {
        let status = match m.status.as_str() {
            "delivered" => " ✓✓".green(),
            "sent" => " ✓".dim(),
            _ => " …".yellow(),
        };
        return Line::from(vec![time, me.green().bold(), " ".into(), m.text.as_str().into(), status]);
    }
    let badge = match m.verified.as_deref() {
        Some("verified") => " ✓".green(),
        Some("invalid") => " ✗ forged?".red().bold(),
        _ => " ?".dim(),
    };
    let nick = m.nick.as_deref().unwrap_or(&m.peer);
    Line::from(vec![time, nick.cyan().bold(), badge, " ".into(), m.text.as_str().into()])
}

/// Unnamed senders show as their address; keep the list column readable.
fn short(peer: &str) -> String {
    if peer.starts_with("0x") && peer.len() > 14 {
        format!("{}…{}", &peer[..8], &peer[peer.len() - 4..])
    } else {
        peer.to_string()
    }
}
