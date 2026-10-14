//! UI-agnostic chat core: sessions over hoprd, acks, retries, signing. UIs drive it through the
//! public methods and render the `Event`s it emits.

use std::{
    collections::{HashMap, HashSet},
    path::PathBuf,
    sync::{
        atomic::{AtomicU32, Ordering},
        Arc, Mutex, MutexGuard,
    },
    time::Duration,
};

use anyhow::{Context, Result};
use edgli::{
    blokli::ChainKeypair,
    hopr_lib::{api::types::crypto::prelude::Signature, builder::Keypair},
};
use futures::StreamExt;
use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use tokio::{
    io::AsyncWriteExt,
    net::{
        tcp::{OwnedReadHalf, OwnedWriteHalf},
        TcpListener, TcpStream,
    },
    sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender},
};
use tokio_util::codec::{FramedRead, LinesCodec};

use crate::{db, hoprd};

/// Any announced node can open a session to us, so everything it sends is bounded.
const MAX_LINE: usize = 16 * 1024;
pub const MAX_TEXT: usize = 4000;
const MAX_NICK: usize = 32;
/// Unread messages kept per unknown sender; more are dropped until the conversation is read.
const MAX_UNREAD_FROM_STRANGER: i64 = 100;

type Writer = Arc<tokio::sync::Mutex<OwnedWriteHalf>>;

pub struct Config {
    pub nick: String,
    pub data_dir: PathBuf,
    pub hops: u8,
    pub retry_secs: u64,
    pub ack_timeout: i64,
    /// The node's chain key; messages go out unsigned without it.
    pub key: Option<ChainKeypair>,
}

/// Adjacently tagged (`{kind, data}`): internal tagging cannot carry `Warning(String)`.
#[derive(Serialize, Clone)]
#[serde(tag = "kind", content = "data", rename_all = "snake_case")]
pub enum Event {
    Message(db::Msg),
    Delivered { peer: String, id: i64 },
    SessionClosed { peer: String },
    Unreachable { peer: String, error: String },
    Warning(String),
}

#[derive(Serialize, Clone)]
pub struct Wallet {
    pub native: String,
    pub hopr: String,
    pub safe_native: String,
    pub safe_hopr: String,
    pub safe_allowance: String,
}

/// Gas lives in the node wallet (xDai), channel money in the Safe (wxHOPR), spendable up to the allowance.
impl Wallet {
    /// Things that stop the node from paying relays, as `(critical, message)`.
    pub fn warnings(&self) -> Vec<(bool, String)> {
        let mut out = vec![];
        if let Some(gas) = amount(&self.native) {
            if gas < 0.001 {
                out.push((true, format!("node out of gas ({}): send xDai to the node address", compact(&self.native))));
            } else if gas < 0.01 {
                out.push((false, format!("node gas low ({})", compact(&self.native))));
            }
        }
        if let Some(allowance) = amount(&self.safe_allowance) {
            if allowance <= 0.0 {
                out.push((true, "Safe allowance is 0: channels cannot be funded".into()));
            } else if allowance < 10.0 {
                out.push((false, format!("Safe allowance low ({})", compact(&self.safe_allowance))));
            }
        }
        if amount(&self.safe_hopr).is_some_and(|funds| funds < 1.0) {
            out.push((false, format!("Safe funds low ({})", compact(&self.safe_hopr))));
        }
        out
    }
}

fn amount(balance: &str) -> Option<f64> {
    balance.split_whitespace().next()?.parse().ok()
}

/// `0.853888310333046313 xDai` -> `0.854 xDai`; an unlimited allowance shows as `∞`.
pub fn compact(balance: &str) -> String {
    let unit = balance.split_whitespace().nth(1).unwrap_or("");
    match amount(balance) {
        Some(v) if v >= 1e12 => format!("∞ {unit}"),
        Some(v) if v >= 100.0 => format!("{v:.0} {unit}"),
        Some(v) => format!("{} {unit}", format!("{v:.3}").trim_end_matches('0').trim_end_matches('.')),
        None => balance.to_string(),
    }
}

#[derive(Serialize, Deserialize, Debug, PartialEq)]
#[serde(tag = "t", rename_all = "lowercase")]
enum Wire {
    Msg {
        id: i64,
        from: String,
        nick: String,
        text: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        sig: Option<String>,
    },
    Ack {
        id: i64,
    },
}

fn line(w: &Wire) -> Vec<u8> {
    let mut bytes = serde_json::to_vec(w).expect("wire is always serializable");
    bytes.push(b'\n');
    bytes
}

pub fn is_address(s: &str) -> bool {
    s.len() == 42 && s.starts_with("0x") && s[2..].bytes().all(|b| b.is_ascii_hexdigit())
}

/// Remote text ends up in terminals: control characters could drive them.
fn clean(s: &str, keep_newlines: bool) -> String {
    s.chars().filter(|c| !c.is_control() || (keep_newlines && *c == '\n')).collect()
}

fn new_id() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos() as i64
}

/// Every chat listens on a port derived from its node address, so the address alone is enough to reach it.
pub fn chat_target(address: &str) -> String {
    let hash = address.to_lowercase().bytes().fold(0xcbf29ce484222325u64, |h, b| (h ^ b as u64).wrapping_mul(0x100000001b3));
    format!("127.0.0.1:{}", 20000 + hash % 40000)
}

/// Binding the recipient and id stops replays to other peers or as new messages. The nick is length
/// prefixed so it cannot be shifted into the text; the tag keeps these apart from ticket signatures.
fn signed_bytes(to: &str, id: i64, nick: &str, text: &str) -> Vec<u8> {
    format!("hopr-chat/v2|{}|{id}|{}:{nick}|{text}", to.to_lowercase(), nick.len()).into_bytes()
}

fn sign(key: &ChainKeypair, to: &str, id: i64, nick: &str, text: &str) -> String {
    hex::encode(Signature::sign_message(&signed_bytes(to, id, nick, text), key).as_ref())
}

fn verify(me: &str, from: &str, id: i64, nick: &str, text: &str, sig: Option<&str>) -> &'static str {
    let Some(sig) = sig else { return "unsigned" };
    let signer = hex::decode(sig)
        .ok()
        .and_then(|bytes| Signature::try_from(bytes.as_slice()).ok())
        .and_then(|s| s.recover_from_msg(&signed_bytes(me, id, nick, text)).ok())
        .map(|pk| pk.to_address().to_string());
    match signer {
        Some(addr) if addr.eq_ignore_ascii_case(from) => "verified",
        _ => "invalid",
    }
}

/// Conversation an incoming message is filed under, `None` to drop it. Only a verified message may
/// land in a contact's conversation; unsigned strangers are filed by their address.
fn conversation_for(contact: Option<String>, from: &str, verified: &str) -> Option<String> {
    match (contact, verified) {
        (_, "invalid") => None,
        (Some(nick), "verified") => Some(nick),
        (Some(_), _) => None,
        (None, _) => Some(from.to_lowercase()),
    }
}

pub struct Chat {
    db: Mutex<Connection>,
    api: hoprd::Api,
    me: String,
    nick: String,
    listen: String,
    hops: u8,
    ack_timeout: i64,
    key: Option<ChainKeypair>,
    events: UnboundedSender<Event>,
    /// peer nick -> (session writer, hoprd session socket)
    outgoing: Mutex<HashMap<String, (Writer, String)>>,
    /// Peers whose session is being opened; the opener flushes their queue once it is up.
    dialing: Mutex<HashSet<String>>,
    /// conn id -> (writer back over SURBs, peer label, sender address)
    incoming: Mutex<HashMap<u32, (Writer, String, String)>>,
    next_conn: AtomicU32,
}

pub async fn start(cfg: Config, api: hoprd::Api) -> Result<(Arc<Chat>, UnboundedReceiver<Event>)> {
    std::fs::create_dir_all(&cfg.data_dir)?;
    let db = db::open(&cfg.data_dir.join("chat.db"))?;
    let me = api.my_address().await?;
    let listen = chat_target(&me);
    let listener = TcpListener::bind(&listen).await.with_context(|| format!("binding {listen}"))?;
    let (events, rx) = unbounded_channel();

    let key = cfg.key.filter(|k| {
        let ok = k.public().to_address().to_string().eq_ignore_ascii_case(&me);
        if !ok {
            let _ = events.send(Event::Warning("identity key does not match hoprd's address, sending unsigned".into()));
        }
        ok
    });
    let chat = Arc::new(Chat {
        db: Mutex::new(db),
        api,
        me,
        nick: cfg.nick,
        listen,
        hops: cfg.hops,
        ack_timeout: cfg.ack_timeout,
        key,
        events,
        outgoing: Default::default(),
        dialing: Default::default(),
        incoming: Default::default(),
        next_conn: AtomicU32::new(1),
    });
    tokio::spawn(chat.clone().accept_loop(listener));
    tokio::spawn(chat.clone().retry_loop(Duration::from_secs(cfg.retry_secs)));
    Ok((chat, rx))
}

impl Chat {
    pub fn me(&self) -> &str {
        &self.me
    }

    pub fn nick(&self) -> &str {
        &self.nick
    }

    pub fn listen(&self) -> &str {
        &self.listen
    }

    pub fn hops(&self) -> u8 {
        self.hops
    }

    pub fn signing(&self) -> bool {
        self.key.is_some()
    }

    pub async fn add_peer(&self, nick: &str, address: &str) -> Result<()> {
        anyhow::ensure!(is_address(address), "{address} is not a node address (0x + 40 hex characters)");
        anyhow::ensure!(!address.eq_ignore_ascii_case(&self.me), "{address} is this node's own address");
        anyhow::ensure!(!nick.is_empty() && !is_address(nick), "pick a nickname for {address}");
        self.api.ensure_announced(address).await?;
        db::add_peer(&self.db(), nick, address)
    }

    /// Queues for a contact. A stranger is answered on its live connection while there is one,
    /// otherwise its conversation label is a node address and gets queued and dialed like a contact.
    pub async fn send(self: &Arc<Self>, peer: &str, text: &str) -> Result<()> {
        anyhow::ensure!(text.chars().count() <= MAX_TEXT, "message too long (max {MAX_TEXT} characters)");
        let contact = db::peer_address(&self.db(), peer)?.is_some();
        if !contact {
            let conn = self.incoming.lock().unwrap().iter().find(|(_, (_, label, _))| label.eq_ignore_ascii_case(peer)).map(|(c, _)| *c);
            if let Some(conn) = conn {
                return self.reply(conn, text).await;
            }
        }
        let address = self.address_of(peer)?.with_context(|| format!("unknown peer {peer}: add it as a contact first"))?;
        anyhow::ensure!(!address.eq_ignore_ascii_case(&self.me), "{peer} is this node's own address");
        db::insert_out(&self.db(), peer, new_id(), text, "pending")?;
        self.wake(peer);
        Ok(())
    }

    /// Node address behind a conversation: the contact's, or the label itself for a stranger.
    fn address_of(&self, peer: &str) -> Result<Option<String>> {
        let contact = db::peer_address(&self.db(), peer)?;
        Ok(contact.or_else(|| is_address(peer).then(|| peer.to_string())))
    }

    /// Answers a sender that is not a contact over its live connection (SURBs).
    async fn reply(&self, conn: u32, text: &str) -> Result<()> {
        let id = new_id();
        let (w, label, to) =
            self.incoming.lock().unwrap().get(&conn).cloned().context("their connection closed: add them as a contact to reach them")?;
        let sig = self.key.as_ref().map(|k| sign(k, &to, id, &self.nick, text));
        let msg = Wire::Msg { id, from: self.me.clone(), nick: self.nick.clone(), text: text.into(), sig };
        w.lock().await.write_all(&line(&msg)).await?;
        db::insert_out(&self.db(), &label, id, text, "sent")
    }

    pub fn peers(&self) -> Result<Vec<db::Peer>> {
        let open: HashSet<String> = self.outgoing.lock().unwrap().keys().cloned().collect();
        let mut peers = db::peers(&self.db())?;
        for p in &mut peers {
            p.online = open.contains(&p.nick);
        }
        Ok(peers)
    }

    /// Known peer nicks plus labels of conversations with senders that were never added.
    pub fn peer_names(&self) -> Vec<String> {
        let db = self.db();
        let mut names: Vec<String> = db::peers(&db).unwrap_or_default().into_iter().map(|p| p.nick).collect();
        for c in db::conversations(&db).unwrap_or_default() {
            if !names.contains(&c.peer) {
                names.push(c.peer);
            }
        }
        names
    }

    pub fn conversations(&self) -> Result<Vec<db::Conversation>> {
        db::conversations(&self.db())
    }

    /// Also marks the conversation read.
    pub fn history(&self, peer: &str, n: i64) -> Result<Vec<db::Msg>> {
        let db = self.db();
        let address = db::peer_address(&db, peer)?;
        db::mark_read(&db, peer, address.as_deref())?;
        db::history(&db, peer, address.as_deref(), n)
    }

    /// Deletes a conversation's messages (queued ones are never sent), or every message with `None`.
    pub fn purge(&self, peer: Option<&str>) -> Result<usize> {
        let db = self.db();
        match peer {
            Some(peer) => {
                let address = db::peer_address(&db, peer)?;
                db::purge(&db, Some((peer, address.as_deref())))
            }
            None => db::purge(&db, None),
        }
    }

    /// Forgets a contact: its messages are purged like `purge` and its open session is closed.
    /// Returns the number of deleted messages.
    /// A stranger's conversation (no contact behind it) is simply deleted.
    pub async fn remove_peer(&self, nick: &str) -> Result<usize> {
        let deleted = {
            let db = self.db();
            let address = db::peer_address(&db, nick)?;
            let deleted = db::purge(&db, Some((nick, address.as_deref())))?;
            let removed = db::remove_peer(&db, nick)?;
            anyhow::ensure!(removed || deleted > 0, "no contact or conversation named {nick}");
            deleted
        };
        let session = self.outgoing.lock().unwrap().remove(nick);
        if let Some((_, sock)) = session {
            self.api.close_session(&sock).await;
        }
        Ok(deleted)
    }

    pub fn outbox(&self) -> Result<Vec<db::Msg>> {
        db::outbox(&self.db())
    }

    pub async fn wallet(&self) -> Result<Wallet> {
        let v = self.api.balances().await?;
        let field = |k: &str| v[k].as_str().unwrap_or("?").to_string();
        Ok(Wallet {
            native: field("native"),
            hopr: field("hopr"),
            safe_native: field("safeNative"),
            safe_hopr: field("safeHopr"),
            safe_allowance: field("safeHoprAllowance"),
        })
    }

    pub async fn shutdown(&self) {
        let socks: Vec<String> = self.outgoing.lock().unwrap().drain().map(|(_, (_, s))| s).collect();
        for s in socks {
            self.api.close_session(&s).await;
        }
    }

    fn db(&self) -> MutexGuard<'_, Connection> {
        self.db.lock().unwrap()
    }

    fn emit(&self, event: Event) {
        let _ = self.events.send(event);
    }

    fn wake(self: &Arc<Self>, peer: &str) {
        let (chat, peer) = (self.clone(), peer.to_string());
        tokio::spawn(async move {
            // A peer removed while its dial was in flight is not worth a warning.
            if let Err(e) = chat.flush(&peer).await {
                if chat.address_of(&peer).is_ok_and(|a| a.is_some()) {
                    chat.emit(Event::Unreachable { peer, error: format!("{e:#}") });
                }
            }
        });
    }

    async fn flush(self: &Arc<Self>, peer: &str) -> Result<()> {
        if db::pending(&self.db(), peer)?.is_empty() {
            return Ok(());
        }
        let Some(address) = self.address_of(peer)? else { return Ok(()) };
        let writer = match self.writer(peer) {
            Some(w) => w,
            None => match self.connect(peer, &address).await? {
                Some(w) => w,
                None => return Ok(()),
            },
        };
        let pending = db::pending(&self.db(), peer)?;
        let mut w = writer.lock().await;
        for (id, text) in pending {
            let sig = self.key.as_ref().map(|k| sign(k, &address, id, &self.nick, &text));
            let msg = Wire::Msg { id, from: self.me.clone(), nick: self.nick.clone(), text, sig };
            if let Err(e) = w.write_all(&line(&msg)).await {
                drop(w);
                self.close_outgoing(peer, None).await;
                return Err(e.into());
            }
            db::mark_sent(&self.db(), peer, id)?;
        }
        Ok(())
    }

    fn writer(&self, peer: &str) -> Option<Writer> {
        self.outgoing.lock().unwrap().get(peer).map(|(w, _)| w.clone())
    }

    /// Opens the peer's session without holding the session map, so one slow peer stalls no one else.
    /// `None` when another task is already dialing it: that task flushes the queue once connected.
    async fn connect(self: &Arc<Self>, peer: &str, address: &str) -> Result<Option<Writer>> {
        if !self.dialing.lock().unwrap().insert(peer.to_string()) {
            return Ok(None);
        }
        if let Some(w) = self.writer(peer) {
            self.dialing.lock().unwrap().remove(peer);
            return Ok(Some(w));
        }
        let dialed = match self.dial(address).await {
            Ok((_, sock)) if self.address_of(peer).is_ok_and(|a| a.is_none()) => {
                self.api.close_session(&sock).await;
                self.dialing.lock().unwrap().remove(peer);
                return Ok(None);
            }
            dialed => dialed,
        };
        let attempt = db::peer_attempt(&self.db(), peer, dialed.as_ref().err().map(|e| format!("{e:#}")));
        let connected = dialed.map(|(stream, sock)| {
            let (r, w) = stream.into_split();
            let w: Writer = Arc::new(tokio::sync::Mutex::new(w));
            tokio::spawn(self.clone().read_outgoing(peer.to_string(), r, sock.clone()));
            self.outgoing.lock().unwrap().insert(peer.to_string(), (w.clone(), sock));
            Some(w)
        });
        self.dialing.lock().unwrap().remove(peer);
        attempt?;
        connected
    }

    async fn dial(&self, address: &str) -> Result<(TcpStream, String)> {
        let sock = self.api.open_session(address, &chat_target(address), self.hops).await?;
        match TcpStream::connect(&sock).await {
            Ok(stream) => Ok((stream, sock)),
            Err(e) => {
                self.api.close_session(&sock).await;
                Err(e.into())
            }
        }
    }

    /// `sock = None` force-closes whatever session the peer has (ack timeout).
    async fn close_outgoing(&self, peer: &str, sock: Option<&str>) {
        let removed = {
            let mut out = self.outgoing.lock().unwrap();
            if sock.is_some_and(|s| out.get(peer).is_none_or(|(_, cur)| cur != s)) {
                return;
            }
            out.remove(peer)
        };
        if let Some((_, s)) = removed {
            self.api.close_session(&s).await;
        }
        if let Err(e) = db::requeue(&self.db(), peer) {
            self.emit(Event::Warning(format!("db: {e:#}")));
        }
    }

    async fn read_outgoing(self: Arc<Self>, peer: String, r: OwnedReadHalf, sock: String) {
        let mut lines = FramedRead::new(r, LinesCodec::new_with_max_length(MAX_LINE));
        while let Some(next) = lines.next().await {
            let l = match next {
                Ok(l) => l,
                Err(e) => {
                    self.emit(Event::Warning(format!("closing session to {peer}: {e}")));
                    break;
                }
            };
            match serde_json::from_str::<Wire>(&l) {
                Ok(Wire::Ack { id }) => self.on_ack(&peer, id),
                Ok(Wire::Msg { id, from, nick, text, sig }) => {
                    self.on_msg(None, id, &from, &nick, &text, sig.as_deref());
                    if let Some(w) = self.writer(&peer) {
                        let _ = w.lock().await.write_all(&line(&Wire::Ack { id })).await;
                    }
                }
                Err(e) => self.emit(Event::Warning(format!("bad line from {peer}: {e}"))),
            }
        }
        self.close_outgoing(&peer, Some(&sock)).await;
        self.emit(Event::SessionClosed { peer });
    }

    async fn accept_loop(self: Arc<Self>, listener: TcpListener) {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                tokio::time::sleep(Duration::from_secs(1)).await;
                continue;
            };
            let conn = self.next_conn.fetch_add(1, Ordering::Relaxed);
            let (r, w) = stream.into_split();
            self.incoming.lock().unwrap().insert(conn, (Arc::new(tokio::sync::Mutex::new(w)), String::new(), String::new()));
            tokio::spawn(self.clone().read_incoming(conn, r));
        }
    }

    /// Rejected messages are acked too, so their sender stops resending them.
    async fn read_incoming(self: Arc<Self>, conn: u32, r: OwnedReadHalf) {
        let mut lines = FramedRead::new(r, LinesCodec::new_with_max_length(MAX_LINE));
        while let Some(next) = lines.next().await {
            let l = match next {
                Ok(l) => l,
                Err(e) => {
                    self.emit(Event::Warning(format!("closing connection #{conn}: {e}")));
                    break;
                }
            };
            match serde_json::from_str::<Wire>(&l) {
                Ok(Wire::Msg { id, from, nick, text, sig }) => {
                    let filed = self.on_msg(Some(conn), id, &from, &nick, &text, sig.as_deref());
                    let writer = {
                        let mut incoming = self.incoming.lock().unwrap();
                        let Some((w, label, sender)) = incoming.get_mut(&conn) else { break };
                        if let Some(filed) = filed {
                            *label = filed;
                            *sender = from.to_lowercase();
                        }
                        w.clone()
                    };
                    let _ = writer.lock().await.write_all(&line(&Wire::Ack { id })).await;
                }
                Ok(Wire::Ack { id }) => {
                    let label = self.incoming.lock().unwrap().get(&conn).map(|(_, label, _)| label.clone());
                    if let Some(label) = label.filter(|l| !l.is_empty()) {
                        self.on_ack(&label, id);
                    }
                }
                Err(e) => self.emit(Event::Warning(format!("bad line on #{conn}: {e}"))),
            }
        }
        self.incoming.lock().unwrap().remove(&conn);
    }

    /// Stores and announces an incoming message; returns the conversation it was filed under,
    /// `None` when it was dropped.
    fn on_msg(&self, conn: Option<u32>, id: i64, from: &str, nick: &str, text: &str, sig: Option<&str>) -> Option<String> {
        if !is_address(from) {
            self.emit(Event::Warning("dropped a message with a malformed sender address".into()));
            return None;
        }
        if text.chars().count() > MAX_TEXT {
            self.emit(Event::Warning(format!("dropped an oversized message from {from}")));
            return None;
        }
        let verified = verify(&self.me, from, id, nick, text, sig);
        let db = self.db();
        let contact = db::nick_by_address(&db, from).ok().flatten();
        let Some(peer) = conversation_for(contact.clone(), from, verified) else {
            drop(db);
            let who = contact.unwrap_or_else(|| from.to_string());
            self.emit(Event::Warning(format!("dropped a message claiming to be {who}: signature {verified}")));
            return None;
        };
        if contact.is_none() && db::unread(&db, &peer).unwrap_or(0) >= MAX_UNREAD_FROM_STRANGER {
            return None;
        }
        let msg = db::Msg {
            id,
            peer: peer.clone(),
            dir: "in".into(),
            text: clean(text, true),
            status: "received".into(),
            created: db::now(),
            nick: Some(clean(nick, false).chars().take(MAX_NICK).collect()),
            from_addr: Some(from.to_lowercase()),
            verified: Some(verified.into()),
            conn,
        };
        let stored = db::insert_in(&db, &msg);
        drop(db);
        match stored {
            Ok(true) => self.emit(Event::Message(msg)),
            Ok(false) => {}
            Err(e) => self.emit(Event::Warning(format!("db: {e:#}"))),
        }
        Some(peer)
    }

    fn on_ack(&self, peer: &str, id: i64) {
        let db = self.db();
        let _ = db::peer_seen(&db, peer);
        let first = db::mark_delivered(&db, peer, id).unwrap_or(false);
        drop(db);
        if first {
            self.emit(Event::Delivered { peer: peer.to_string(), id });
        }
    }

    async fn retry_loop(self: Arc<Self>, every: Duration) {
        loop {
            let stale = db::stale_sent(&self.db(), self.ack_timeout).unwrap_or_default();
            for peer in stale {
                self.close_outgoing(&peer, None).await;
            }
            for peer in db::peers_with_pending(&self.db()).unwrap_or_default() {
                self.wake(&peer);
            }
            tokio::time::sleep(every).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wire_roundtrip() {
        let msg = Wire::Msg { id: 1, from: "0xA".into(), nick: "a".into(), text: "hi".into(), sig: None };
        let bytes = line(&msg);
        assert_eq!(bytes.last(), Some(&b'\n'));
        assert_eq!(serde_json::from_slice::<Wire>(&bytes).unwrap(), msg);
        assert_eq!(serde_json::from_str::<Wire>(r#"{"t":"ack","id":5}"#).unwrap(), Wire::Ack { id: 5 });
    }

    #[test]
    fn only_verified_messages_reach_a_contact() {
        let addr = "0xAbCdEf0000000000000000000000000000000001";
        let bob = || Some("bob".to_string());
        assert_eq!(conversation_for(bob(), addr, "verified"), Some("bob".into()));
        assert_eq!(conversation_for(bob(), addr, "unsigned"), None);
        assert_eq!(conversation_for(bob(), addr, "invalid"), None);
        assert_eq!(conversation_for(None, addr, "invalid"), None);
        assert_eq!(conversation_for(None, addr, "unsigned"), Some(addr.to_lowercase()));
    }

    #[test]
    fn remote_input_is_validated() {
        assert!(is_address("0x00000000000000000000000000000000000000aB"));
        assert!(!is_address("bob"));
        assert!(!is_address("0xaéééééééééééééééééééééééééééééééééééé"));
        assert!(!is_address("0x00000000000000000000000000000000000000a"));
        assert_eq!(clean("hi\x1b[2J\nthere\r", true), "hi[2J\nthere");
        assert_eq!(clean("al\nice", false), "alice");
    }

    #[test]
    fn chat_target_ignores_address_case() {
        assert_eq!(chat_target("0xAbC"), chat_target("0xabc"));
        assert_ne!(chat_target("0xabc"), chat_target("0xabd"));
    }

    #[test]
    fn wallet_warnings_and_compact_amounts() {
        let w = |native: &str, safe: &str, allowance: &str| Wallet {
            native: native.into(),
            hopr: "0 wxHOPR".into(),
            safe_native: "0 xDai".into(),
            safe_hopr: safe.into(),
            safe_allowance: allowance.into(),
        };
        assert!(w("0.85 xDai", "997 wxHOPR", "1000 wxHOPR").warnings().is_empty());
        assert!(w("0.0001 xDai", "997 wxHOPR", "1000 wxHOPR").warnings()[0].0);
        assert!(!w("0.005 xDai", "997 wxHOPR", "1000 wxHOPR").warnings()[0].0);
        assert!(w("1 xDai", "997 wxHOPR", "0 wxHOPR").warnings()[0].0);
        assert_eq!(w("1 xDai", "0.5 wxHOPR", "1000 wxHOPR").warnings().len(), 1);
        assert_eq!(compact("0.853888310333046313 xDai"), "0.854 xDai");
        assert_eq!(compact("997.2 wxHOPR"), "997 wxHOPR");
        assert_eq!(compact("1 xDai"), "1 xDai");
        assert_eq!(compact("115792089237316195423570985008687907853269984665640564039457.584 wxHOPR"), "∞ wxHOPR");
    }

    #[test]
    fn signatures_bind_sender_recipient_and_text() {
        let alice = ChainKeypair::random();
        let a = alice.public().to_address().to_string();
        let b = ChainKeypair::random().public().to_address().to_string();
        let sig = sign(&alice, &b, 7, "alice", "hi");
        assert_eq!(verify(&b, &a, 7, "alice", "hi", Some(&sig)), "verified");
        assert_eq!(verify(&b, &a, 7, "alice", "hi!", Some(&sig)), "invalid");
        assert_eq!(verify(&b, &b, 7, "alice", "hi", Some(&sig)), "invalid");
        assert_eq!(verify(&a, &a, 7, "alice", "hi", Some(&sig)), "invalid");
        assert_eq!(verify(&b, &a, 7, "alice", "hi", Some("zz")), "invalid");
        assert_eq!(verify(&b, &a, 7, "alice", "hi", None), "unsigned");
        let shifted = sign(&alice, &b, 7, "al", "ice|hi");
        assert_eq!(verify(&b, &a, 7, "al|ice", "hi", Some(&shifted)), "invalid");
    }
}
