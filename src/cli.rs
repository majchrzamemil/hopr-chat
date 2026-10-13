//! Terminal frontend: rustyline prompt with completion, conversation focus and inline events.

use std::{
    borrow::Cow,
    sync::{Arc, Mutex},
};

use anyhow::{bail, Context, Result};
use hopr_chat::{
    app::{SafeChoice, Status, Ui},
    chat::{compact, Chat, Event},
    db,
};
use rustyline::{
    completion::{Completer, Pair},
    error::ReadlineError,
    highlight::Highlighter,
    hint::{Hinter, HistoryHinter},
    CompletionType, Config, Editor, ExternalPrinter, Helper, Validator,
};
use tokio::sync::mpsc::UnboundedReceiver;

use crate::term::{self, ago, paint, DIM, GREEN, YELLOW};

const RED: u8 = 31;
const CYAN: u8 = 36;

const COMMANDS: &[(&str, &str, &str)] = &[
    ("/open", "<peer>", "focus a conversation; plain text then goes to it"),
    ("/close", "", "leave the focused conversation"),
    ("/chats", "", "conversations with unread counts"),
    ("/history", "<peer> [n]", "conversation with a peer (marks it read)"),
    ("/msg", "<peer> <text>", "queue and send a message"),
    ("/reply", "<conn> <text>", "answer an incoming connection over SURBs"),
    ("/add", "<nick> <0xaddress>", "save a peer by its hoprd node address"),
    ("/peers", "", "known peers and reachability"),
    ("/outbox", "", "messages not delivered yet"),
    ("/balance", "", "node and Safe balances"),
    ("/whoami", "", "the /add line to give to others"),
    ("/help", "", "this list"),
    ("/quit", "", "close sessions and exit"),
];

fn badge(verified: Option<&str>) -> String {
    match verified {
        Some("verified") => paint(GREEN, "✓"),
        Some("invalid") => paint(RED, "✗ FORGED?"),
        _ => paint(DIM, "?"),
    }
}

fn fmt_msgs(msgs: &[db::Msg], me: &str) -> String {
    if msgs.is_empty() {
        return paint(DIM, "(none)");
    }
    let rows: Vec<String> = msgs
        .iter()
        .map(|m| {
            let meta = paint(DIM, &format!("{:>4} #{}", ago(m.created), m.id));
            if m.dir == "out" {
                let status = paint(DIM, &format!("[{}]", m.status));
                format!("{meta} {} -> {}: {} {status}", paint(GREEN, me), m.peer, m.text)
            } else {
                let nick = m.nick.as_deref().unwrap_or(&m.peer);
                format!("{meta} {} {}: {}", paint(CYAN, nick), badge(m.verified.as_deref()), m.text)
            }
        })
        .collect();
    rows.join("\n")
}

type Printer = Arc<Mutex<Box<dyn ExternalPrinter + Send>>>;

fn say(out: &Printer, s: String) {
    let _ = out.lock().unwrap().print(s);
}

#[derive(Default)]
pub struct CliUi {
    funding_shown: bool,
}

impl Ui for CliUi {
    async fn choose_safe(&mut self, node_address: &str) -> Result<SafeChoice> {
        term::choose_safe(node_address).await
    }

    fn status(&mut self, status: Status) {
        term::print_status(status, &mut self.funding_shown);
    }

    async fn run(&mut self, chat: Arc<Chat>, mut events: UnboundedReceiver<Event>) -> Result<()> {
        let anon = if chat.hops() == 0 { paint(RED, " (direct, NOT anonymous)") } else { String::new() };
        let signing = if chat.signing() { paint(GREEN, "signing ✓") } else { paint(YELLOW, "unsigned") };
        println!("{} {} {signing}", paint(GREEN, chat.nick()), chat.me());
        println!("{}{anon}", paint(DIM, &format!("{} hop(s), listening on {}, /help for commands", chat.hops(), chat.listen())));
        if let Ok(w) = chat.wallet().await {
            println!("{}", paint(DIM, &format!("gas {} · funds {}", compact(&w.native), compact(&w.safe_hopr))));
            for (critical, text) in w.warnings() {
                println!("{}", paint(if critical { RED } else { YELLOW }, &format!("! {text}")));
            }
        }

        let focus: Arc<Mutex<Option<String>>> = Default::default();
        let (out, mut lines) = start_prompt(chat.clone(), focus.clone())?;

        let (render_chat, render_out, render_focus) = (chat.clone(), out.clone(), focus.clone());
        tokio::spawn(async move {
            while let Some(ev) = events.recv().await {
                render(&render_chat, &render_out, &render_focus, ev);
            }
        });

        while let Some(l) = lines.recv().await {
            if l == "/quit" {
                break;
            }
            if let Err(e) = command(&chat, &out, &l).await {
                say(&out, paint(RED, &format!("! {e:#}")));
            }
        }
        Ok(())
    }
}

fn render(chat: &Chat, out: &Printer, focus: &Mutex<Option<String>>, ev: Event) {
    let line = match ev {
        Event::Message(m) => {
            let focused = focus.lock().unwrap().as_deref() == Some(m.peer.as_str());
            if focused {
                let _ = chat.mark_read(&m.peer);
            }
            let nick = m.nick.as_deref().unwrap_or(&m.peer);
            let conn = m.conn.map_or(String::new(), |c| format!("[#{c}] "));
            let from = if focused { String::new() } else { paint(DIM, &format!(" ({})", m.peer)) };
            format!("{}{} {}{from}: {}", paint(DIM, &conn), paint(CYAN, nick), badge(m.verified.as_deref()), m.text)
        }
        Event::Delivered { peer, id } => paint(DIM, &format!("✓ {peer} got #{id}")),
        Event::SessionClosed { peer } => paint(DIM, &format!("~ session to {peer} closed")),
        Event::Unreachable { peer, error } => paint(RED, &format!("! {peer} unreachable, queued for retry: {error}")),
        Event::Warning(w) => paint(RED, &format!("! {w}")),
    };
    say(out, line);
}

async fn command(chat: &Arc<Chat>, out: &Printer, l: &str) -> Result<()> {
    let (cmd, rest) = l.split_once(' ').unwrap_or((l, ""));
    match cmd {
        "" => {}
        "/whoami" => say(out, format!("/add {} {}", chat.nick(), chat.me())),
        "/add" => {
            let [nick, address] = rest.split_whitespace().collect::<Vec<_>>()[..] else {
                bail!("usage: /add <nick> <0xaddress>");
            };
            chat.add_peer(nick, address).await?;
            say(out, format!("added {nick}"));
        }
        "/msg" => {
            let (peer, text) = rest.split_once(' ').context("usage: /msg <peer> <text>")?;
            chat.send(peer, text).await?;
        }
        "/reply" => {
            let (conn, text) = rest.split_once(' ').context("usage: /reply <conn> <text>")?;
            chat.reply(conn.trim_start_matches('#').parse()?, text).await?;
        }
        "/history" => {
            let mut args = rest.split_whitespace();
            let peer = args.next().context("usage: /history <peer> [n]")?;
            let n = args.next().map(str::parse).transpose()?.unwrap_or(20);
            say(out, fmt_msgs(&chat.history(peer, n)?, chat.nick()));
        }
        "/chats" => {
            let convs = chat.conversations()?;
            if convs.is_empty() {
                say(out, paint(DIM, "(no conversations yet)"));
            }
            for c in convs {
                let unread = if c.unread > 0 { paint(YELLOW, &format!("({}) ", c.unread)) } else { String::new() };
                say(out, format!("{unread}{} {} {}", paint(CYAN, &c.peer), paint(DIM, &ago(c.last_at)), c.last_text));
            }
        }
        "/peers" => {
            let peers = chat.peers().await?;
            if peers.is_empty() {
                say(out, paint(DIM, "(none) - /add <nick> <0xaddress>"));
            }
            for p in peers {
                let state = if p.online { paint(GREEN, "session") } else { paint(DIM, "idle") };
                let seen = p.last_seen.map_or("never".into(), ago);
                let tried = p.last_attempt.map_or("never".into(), ago);
                let mut row = format!("{} {} {state} {}", p.nick, p.address, paint(DIM, &format!("seen {seen}, tried {tried}")));
                if let Some(e) = p.last_err {
                    row += &format!("\n    {}", paint(RED, &e));
                }
                say(out, row);
            }
        }
        "/outbox" => say(out, fmt_msgs(&chat.outbox()?, chat.nick())),
        "/balance" => {
            let w = chat.wallet().await?;
            let mut text = format!(
                "node wallet  gas {} · wxHOPR {} {}\nsafe         funds {} · allowance {} · xDai {}",
                compact(&w.native),
                compact(&w.hopr),
                paint(DIM, "(normally 0, it lives in the Safe)"),
                compact(&w.safe_hopr),
                compact(&w.safe_allowance),
                compact(&w.safe_native),
            );
            for (critical, warning) in w.warnings() {
                text += &format!("\n{}", paint(if critical { RED } else { YELLOW }, &format!("! {warning}")));
            }
            say(out, text);
        }
        _ => {
            let rows: Vec<String> =
                COMMANDS.iter().map(|(c, args, help)| format!("{:<28} {}", format!("{c} {args}"), paint(DIM, help))).collect();
            say(out, rows.join("\n"));
        }
    }
    Ok(())
}

#[derive(Helper, Validator)]
struct Prompt {
    chat: Arc<Chat>,
    history: HistoryHinter,
}

impl Completer for Prompt {
    type Candidate = Pair;

    fn complete(&self, line: &str, pos: usize, _: &rustyline::Context<'_>) -> rustyline::Result<(usize, Vec<Pair>)> {
        let head = &line[..pos];
        let start = head.rfind(' ').map_or(0, |i| i + 1);
        let options: Vec<String> = match head[..start].split_whitespace().collect::<Vec<_>>()[..] {
            [] => COMMANDS.iter().map(|(c, ..)| c.to_string()).collect(),
            ["/msg" | "/history" | "/open"] => self.chat.peer_names(),
            ["/reply"] => self.chat.open_connections().iter().map(u32::to_string).collect(),
            _ => vec![],
        };
        let matches = options
            .into_iter()
            .filter(|o| o.starts_with(&head[start..]))
            .map(|o| Pair { display: o.clone(), replacement: o + " " })
            .collect();
        Ok((start, matches))
    }
}

impl Hinter for Prompt {
    type Hint = String;

    fn hint(&self, line: &str, pos: usize, ctx: &rustyline::Context<'_>) -> Option<String> {
        let usage = COMMANDS
            .iter()
            .find(|(c, args, _)| !args.is_empty() && line.trim_end() == *c)
            .map(|(_, args, _)| if line.ends_with(' ') { args.to_string() } else { format!(" {args}") });
        usage.or_else(|| self.history.hint(line, pos, ctx))
    }
}

impl Highlighter for Prompt {
    fn highlight_prompt<'b, 's: 'b, 'p: 'b>(&'s self, prompt: &'p str, _default: bool) -> Cow<'b, str> {
        Cow::Owned(format!("\x1b[1;32m{prompt}\x1b[0m"))
    }

    fn highlight_hint<'h>(&self, hint: &'h str) -> Cow<'h, str> {
        Cow::Owned(paint(DIM, hint))
    }
}

/// rustyline blocks, so it runs on its own thread. Focus is pure UI state and is handled there,
/// so the prompt never lags behind `/open`; everything else is handed over as command lines.
fn start_prompt(chat: Arc<Chat>, focus: Arc<Mutex<Option<String>>>) -> Result<(Printer, UnboundedReceiver<String>)> {
    let history = chat_history_file();
    let config = Config::builder().completion_type(CompletionType::List).build();
    let mut rl: Editor<Prompt, _> = Editor::with_config(config)?;
    rl.set_helper(Some(Prompt { chat: chat.clone(), history: HistoryHinter::new() }));
    if let Some(h) = &history {
        let _ = rl.load_history(h);
    }
    let out: Printer = Arc::new(Mutex::new(Box::new(rl.create_external_printer()?)));

    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    let thread_out = out.clone();
    std::thread::spawn(move || loop {
        let prompt = match focus.lock().unwrap().as_deref() {
            Some(peer) => format!("{}→{peer}> ", chat.nick()),
            None => format!("{}> ", chat.nick()),
        };
        let line = match rl.readline(&prompt) {
            Ok(l) => l.trim().to_string(),
            Err(ReadlineError::Interrupted) => continue,
            Err(_) => "/quit".into(),
        };
        if !line.is_empty() {
            let _ = rl.add_history_entry(&line);
            if let Some(h) = &history {
                let _ = rl.save_history(h);
            }
        }
        let forward = match line.split_once(' ').unwrap_or((&line, "")) {
            ("/open", peer) if !peer.trim().is_empty() => {
                *focus.lock().unwrap() = Some(peer.trim().to_string());
                format!("/history {}", peer.trim())
            }
            ("/close", _) => {
                *focus.lock().unwrap() = None;
                continue;
            }
            _ if !line.is_empty() && !line.starts_with('/') => match focus.lock().unwrap().clone() {
                Some(peer) => format!("/msg {peer} {line}"),
                None => {
                    say(&thread_out, paint(DIM, "no conversation open: /open <peer>, or /msg <peer> <text>"));
                    continue;
                }
            },
            _ => line,
        };
        let quit = forward == "/quit";
        if tx.send(forward).is_err() || quit {
            break;
        }
    });
    Ok((out, rx))
}

fn chat_history_file() -> Option<std::path::PathBuf> {
    let dir = hopr_chat::settings::home().ok()?;
    std::fs::create_dir_all(&dir).ok()?;
    Some(dir.join("prompt-history.txt"))
}
